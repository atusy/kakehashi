//! `kakehashi/virtualDocument/prepare`: let a peer rewrite a virtual document
//! before downstream servers see it.
//!
//! kakehashi splits the virtual document (V) into ordered segments — injected
//! `content` and host-owned `gap`s — and asks the configured peer how to
//! present them. The answer may delete leading whitespace from content lines
//! (dedent) and replace any gap with arbitrary text (placeholders). Applying
//! it yields the prepared document (P) that downstream servers receive, plus
//! a [`PreparedMap`] translating coordinates between P and V. V keeps its
//! existing host translation ([`super::RegionOffset`]), so host ↔ P is the
//! composition of the two.
//!
//! Content may only lose leading whitespace so that every edit a downstream
//! server makes in P can be mapped back unambiguously; gaps are opaque, so an
//! edit touching one is refused rather than guessed.

use std::ops::Range;
use std::time::{Duration, Instant};

use line_index::{LineIndex, WideEncoding, WideLineCol};
use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{Position, Range as LspRange, TextEdit};

/// The request kakehashi sends to the configured prepare peer.
pub(crate) const PREPARE_METHOD: &str = "kakehashi/virtualDocument/prepare";

/// How long a prepare answer may take. The document is not sent downstream
/// until it arrives, so this bounds how stale a downstream view can get.
pub(crate) const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on the diff that maps a downstream edit back through P → V.
/// A diff that cannot finish in time degrades to coarser hunks, which the gap
/// check then refuses — slower than this is not worth an edit.
const EDIT_DIFF_BUDGET: Duration = Duration::from_millis(200);

/// Kind of a virtual-document segment, as named on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SegmentKind {
    /// Injected text the downstream language owns.
    Content,
    /// Host-owned text between (or inside) injected content.
    Gap,
}

/// One segment of a virtual document.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Segment {
    pub(crate) kind: SegmentKind,
    /// Byte range in the virtual document text.
    pub(crate) virtual_range: Range<usize>,
    /// What the peer sees: the virtual text for content, the original host
    /// text for a gap (whose virtual text is only coordinate-preserving
    /// whitespace).
    pub(crate) text: String,
}

/// The ordered segments covering a virtual document end to end.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct VirtualLayout {
    segments: Vec<Segment>,
}

impl VirtualLayout {
    /// A document that is all content (an isolated injection).
    #[cfg(test)]
    pub(crate) fn single(virtual_text: &str) -> Self {
        Self {
            segments: vec![Segment {
                kind: SegmentKind::Content,
                virtual_range: 0..virtual_text.len(),
                text: virtual_text.to_string(),
            }],
        }
    }

    /// Build a layout from adjacent pieces in virtual-text order.
    ///
    /// Adjacent pieces of one kind merge. A gap whose virtual text is empty
    /// (a stripped line prefix such as a blockquote `> `) is not presented:
    /// it never reaches downstream servers, so there is nothing to replace,
    /// and the content around it merges into one segment.
    pub(crate) fn from_pieces(
        virtual_text: &str,
        pieces: impl IntoIterator<Item = (SegmentKind, Range<usize>, String)>,
    ) -> Self {
        let mut segments: Vec<Segment> = Vec::new();
        let mut pending_gap_text = String::new();
        for (kind, range, host_text) in pieces {
            if kind == SegmentKind::Gap && range.is_empty() {
                pending_gap_text.push_str(&host_text);
                continue;
            }
            match segments.last_mut() {
                Some(last) if last.kind == kind && last.virtual_range.end == range.start => {
                    last.virtual_range.end = range.end;
                    match kind {
                        SegmentKind::Content => {
                            last.text.push_str(&virtual_text[range.clone()]);
                        }
                        SegmentKind::Gap => {
                            last.text.push_str(&pending_gap_text);
                            last.text.push_str(&host_text);
                        }
                    }
                }
                _ => {
                    let text = match kind {
                        SegmentKind::Content => virtual_text[range.clone()].to_string(),
                        SegmentKind::Gap => {
                            // A stripped prefix right before a visible gap is
                            // host text that gap spans.
                            let mut text = std::mem::take(&mut pending_gap_text);
                            text.push_str(&host_text);
                            text
                        }
                    };
                    segments.push(Segment {
                        kind,
                        virtual_range: range,
                        text,
                    });
                }
            }
            pending_gap_text.clear();
        }
        Self { segments }
    }

    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[Segment] {
        &self.segments
    }
}

// =============================================================================
// Wire types
// =============================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareTextDocument<'a> {
    pub(crate) uri: &'a str,
    pub(crate) language_id: &'a str,
    pub(crate) version: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareHostTextDocument<'a> {
    pub(crate) uri: &'a str,
    pub(crate) language_id: &'a str,
}

#[derive(Debug, Serialize)]
struct WireSegment<'a> {
    #[serde(rename = "type")]
    kind: SegmentKind,
    content: &'a str,
}

/// Params of [`PREPARE_METHOD`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PrepareParams<'a> {
    text_document: PrepareTextDocument<'a>,
    host_text_document: PrepareHostTextDocument<'a>,
    segments: Vec<WireSegment<'a>>,
}

impl<'a> PrepareParams<'a> {
    pub(crate) fn new(
        text_document: PrepareTextDocument<'a>,
        host_text_document: PrepareHostTextDocument<'a>,
        layout: &'a VirtualLayout,
    ) -> Self {
        Self {
            text_document,
            host_text_document,
            segments: layout
                .segments
                .iter()
                .map(|segment| WireSegment {
                    kind: segment.kind,
                    content: &segment.text,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireSegmentResult {
    Content {
        #[serde(default)]
        changes: Option<Vec<TextEdit>>,
    },
    Gap {
        #[serde(default)]
        content: Option<String>,
    },
}

/// Result of [`PREPARE_METHOD`]; `null` on the wire means "unchanged".
#[derive(Debug, Deserialize)]
pub(crate) struct PrepareResult {
    segments: Vec<WireSegmentResult>,
}

/// Parse the response to [`PREPARE_METHOD`]. An error response or a
/// malformed result is an `InvalidData` error — a failed prepare, never
/// "unchanged".
pub(crate) fn parse_prepare_response(
    response: &serde_json::Value,
) -> std::io::Result<Option<PrepareResult>> {
    // Every unusable answer is `InvalidData`, which callers tell apart from
    // a failure to get an answer at all.
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bridge: prepare peer answered with an error: {error}"),
        ));
    }
    let result = response.get("result").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bridge: prepare response missing result",
        )
    })?;
    serde_json::from_value(result.clone())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Why a prepare answer was refused. A refused answer is a failed prepare:
/// the document is not sent at all, never sent unprepared.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PrepareError {
    #[error("expected {expected} segments, got {actual}")]
    SegmentCount { expected: usize, actual: usize },
    #[error("segment {index}: expected type {expected:?}")]
    SegmentKind { index: usize, expected: SegmentKind },
    #[error("segment {index}: {reason}")]
    InvalidChange { index: usize, reason: &'static str },
}

// =============================================================================
// Applying an answer
// =============================================================================

/// A virtual document as downstream servers receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedDocument {
    /// The text sent downstream.
    pub(crate) text: String,
    /// `None` when the answer changed nothing: P is V and needs no mapping.
    pub(crate) map: Option<std::sync::Arc<PreparedMap>>,
}

impl PreparedDocument {
    /// The document unchanged (a `null` answer, or no prepare configured).
    pub(crate) fn unchanged(virtual_text: &str) -> Self {
        Self {
            text: virtual_text.to_string(),
            map: None,
        }
    }
}

/// Apply a peer answer to the virtual document it was asked about.
///
/// `result` of `None` is the wire `null`: every gap keeps its
/// coordinate-preserving whitespace and no content changes.
pub(crate) fn apply_prepare_result(
    virtual_text: &str,
    layout: &VirtualLayout,
    result: Option<PrepareResult>,
) -> Result<PreparedDocument, PrepareError> {
    // `null` keeps every segment. It still yields a map when the layout has
    // gaps: the map is what keeps downstream edits off host-owned text.
    let result = result.unwrap_or_else(|| PrepareResult {
        segments: layout
            .segments
            .iter()
            .map(|segment| match segment.kind {
                SegmentKind::Content => WireSegmentResult::Content { changes: None },
                SegmentKind::Gap => WireSegmentResult::Gap { content: None },
            })
            .collect(),
    });
    if result.segments.len() != layout.segments.len() {
        return Err(PrepareError::SegmentCount {
            expected: layout.segments.len(),
            actual: result.segments.len(),
        });
    }

    let mut text = String::with_capacity(virtual_text.len());
    let mut runs: Vec<Run> = Vec::new();
    let mut indents: Vec<SegmentIndent> = Vec::new();
    for (index, (segment, answer)) in layout.segments.iter().zip(result.segments).enumerate() {
        let virtual_segment = &virtual_text[segment.virtual_range.clone()];
        match (segment.kind, answer) {
            (SegmentKind::Content, WireSegmentResult::Content { changes }) => {
                let deletions = content_deletions(
                    virtual_text,
                    segment.virtual_range.start,
                    virtual_segment,
                    changes.unwrap_or_default(),
                )
                .map_err(|reason| PrepareError::InvalidChange { index, reason })?;
                let mut cursor = segment.virtual_range.start;
                let mut deleted: Vec<&str> = Vec::new();
                for deletion in deletions {
                    push_run(
                        &mut runs,
                        &mut text,
                        RunKind::Identity,
                        cursor..deletion.start,
                        virtual_text,
                        "",
                    );
                    push_run(
                        &mut runs,
                        &mut text,
                        RunKind::Deleted,
                        deletion.clone(),
                        virtual_text,
                        "",
                    );
                    deleted.push(&virtual_text[deletion.clone()]);
                    cursor = deletion.end;
                }
                push_run(
                    &mut runs,
                    &mut text,
                    RunKind::Identity,
                    cursor..segment.virtual_range.end,
                    virtual_text,
                    "",
                );
                indents.push(SegmentIndent::from_deletions(
                    segment.virtual_range.clone(),
                    &deleted,
                ));
            }
            (SegmentKind::Gap, WireSegmentResult::Gap { content }) => {
                let replacement = content.as_deref().unwrap_or(virtual_segment);
                push_run(
                    &mut runs,
                    &mut text,
                    RunKind::Gap,
                    segment.virtual_range.clone(),
                    virtual_text,
                    replacement,
                );
            }
            (expected, _) => return Err(PrepareError::SegmentKind { index, expected }),
        }
    }

    // Without deletions or gaps P is V and nothing needs mapping or
    // protecting.
    if runs.iter().all(|run| run.kind == RunKind::Identity) {
        return Ok(PreparedDocument::unchanged(virtual_text));
    }
    let fingerprint = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        virtual_text.hash(&mut hasher);
        text.hash(&mut hasher);
        runs.hash(&mut hasher);
        hasher.finish()
    };
    let map = PreparedMap {
        virtual_lines: LineMap::new(virtual_text.to_string()),
        prepared_lines: LineMap::new(text.clone()),
        runs,
        indents,
        fingerprint,
    };
    Ok(PreparedDocument {
        text,
        map: Some(std::sync::Arc::new(map)),
    })
}

/// Validate a content segment's changes and return the byte ranges (in V)
/// they delete, sorted.
fn content_deletions(
    virtual_text: &str,
    segment_start: usize,
    segment_text: &str,
    changes: Vec<TextEdit>,
) -> Result<Vec<Range<usize>>, &'static str> {
    let segment_lines = LineMap::new(segment_text.to_string());
    let mut deletions = Vec::with_capacity(changes.len());
    for change in changes {
        if !change.new_text.is_empty() {
            return Err("content changes may only delete text");
        }
        let start = segment_lines
            .offset_strict(change.range.start)
            .ok_or("change starts outside the segment")?;
        let end = segment_lines
            .offset_strict(change.range.end)
            .ok_or("change ends outside the segment")?;
        if start > end {
            return Err("change range is reversed");
        }
        if start == end {
            continue;
        }
        let deleted = &segment_text[start..end];
        if !deleted.bytes().all(|byte| byte == b' ' || byte == b'\t') {
            return Err("content changes may only delete spaces and tabs");
        }
        let absolute = segment_start + start;
        let at_line_start =
            absolute == 0 || matches!(virtual_text.as_bytes()[absolute - 1], b'\n' | b'\r');
        if !at_line_start {
            return Err("content changes may only delete leading whitespace");
        }
        deletions.push(absolute..segment_start + end);
    }
    deletions.sort_by_key(|range| range.start);
    if deletions.windows(2).any(|pair| pair[0].end > pair[1].start) {
        return Err("content changes overlap");
    }
    Ok(deletions)
}

fn push_run(
    runs: &mut Vec<Run>,
    text: &mut String,
    kind: RunKind,
    virtual_range: Range<usize>,
    virtual_text: &str,
    replacement: &str,
) {
    let prepared_text = match kind {
        RunKind::Identity => &virtual_text[virtual_range.clone()],
        RunKind::Deleted => "",
        RunKind::Gap => replacement,
    };
    if kind == RunKind::Identity && virtual_range.is_empty() {
        return;
    }
    let start = text.len();
    text.push_str(prepared_text);
    runs.push(Run {
        kind,
        prepared: start..text.len(),
        virtual_: virtual_range,
    });
}

// =============================================================================
// The prepared ↔ virtual map
// =============================================================================

/// Which side of an opaque run a position strictly inside it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bias {
    /// The run's start — for positions and range starts.
    Start,
    /// The run's end — for range ends, so a range over a replaced gap covers
    /// the original gap.
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RunKind {
    /// Text present unchanged in both documents.
    Identity,
    /// Leading whitespace of a content line, absent from P.
    Deleted,
    /// A host-owned gap, with any text in P.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Run {
    kind: RunKind,
    prepared: Range<usize>,
    virtual_: Range<usize>,
}

/// The indentation a content segment lost, used to re-indent lines that an
/// edit in P creates.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentIndent {
    virtual_range: Range<usize>,
    /// `Some(indent)` when every deletion in the segment removed the same
    /// text (a dedent); `None` when they differ, so a new line has no single
    /// right indentation.
    uniform: Option<String>,
}

impl SegmentIndent {
    fn from_deletions(virtual_range: Range<usize>, deleted: &[&str]) -> Self {
        let uniform = match deleted.split_first() {
            None => Some(String::new()),
            Some((first, rest)) => rest
                .iter()
                .all(|other| other == first)
                .then(|| (*first).to_string()),
        };
        Self {
            virtual_range,
            uniform,
        }
    }
}

/// Coordinates between a prepared document (P) and its virtual document (V).
///
/// Equality compares a fingerprint of both texts and the runs between them:
/// region offsets carrying a map are compared on every resolve gate, which
/// must not cost a full text comparison.
#[derive(Debug)]
pub(crate) struct PreparedMap {
    virtual_lines: LineMap,
    prepared_lines: LineMap,
    runs: Vec<Run>,
    indents: Vec<SegmentIndent>,
    fingerprint: u64,
}

impl PartialEq for PreparedMap {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
    }
}

impl Eq for PreparedMap {}

impl PreparedMap {
    /// Translate a position in P to V.
    pub(crate) fn to_virtual(&self, position: Position, bias: Bias) -> Position {
        let offset = self.prepared_lines.offset_clamped(position);
        let mapped = map_offset(&self.runs, offset, bias, Side::Prepared);
        self.virtual_lines.position(mapped)
    }

    /// Translate a position in V to P.
    pub(crate) fn to_prepared(&self, position: Position, bias: Bias) -> Position {
        let offset = self.virtual_lines.offset_clamped(position);
        let mapped = map_offset(&self.runs, offset, bias, Side::Virtual);
        self.prepared_lines.position(mapped)
    }

    /// Translate edits a downstream server made to P into edits to V.
    ///
    /// The edits are applied to P and the result diffed against P, so a
    /// whole-document replacement (what most formatters send) maps exactly as
    /// small edits do. Returns `None` — refuse the edit — when any change
    /// touches a gap, or when it creates a line in a content segment whose
    /// indentation is not one uniform string.
    pub(crate) fn edits_to_virtual(&self, edits: &[TextEdit]) -> Option<Vec<TextEdit>> {
        let prepared = self.prepared_lines.text();
        let edited = apply_edits(&self.prepared_lines, edits)?;
        diff_hunks(prepared, &edited)
            .into_iter()
            .map(|hunk| {
                let following = edited[hunk.new.end..].chars().next();
                self.hunk_to_virtual(hunk.old, &edited[hunk.new], following)
            })
            .collect()
    }

    /// Translate one edit a downstream server made to P into an edit to V,
    /// keeping its extent (a completion's replace range, say) rather than
    /// minimizing it. `None` — refuse the edit — under the same rules as
    /// [`Self::edits_to_virtual`].
    pub(crate) fn edit_to_virtual(&self, edit: &TextEdit) -> Option<TextEdit> {
        let start = self.prepared_lines.offset_clamped(edit.range.start);
        let end = self.prepared_lines.offset_clamped(edit.range.end);
        if start > end {
            return None;
        }
        let following = self.prepared_lines.text()[end..].chars().next();
        self.hunk_to_virtual(start..end, &edit.new_text, following)
    }

    /// Map the replacement of P's `old` bytes by `new_text` onto V.
    /// `following` is the character after the replacement in the edited P.
    fn hunk_to_virtual(
        &self,
        old: Range<usize>,
        new_text: &str,
        following: Option<char>,
    ) -> Option<TextEdit> {
        if self.hunk_touches_gap(&old) {
            return None;
        }
        let virtual_start = map_offset(&self.runs, old.start, Bias::Start, Side::Prepared);
        let virtual_end = map_offset(&self.runs, old.end, Bias::End, Side::Prepared);
        // Text landing at a V line start that kept no indent before it (the
        // document end, or a line the peer did not dedent) starts a line of
        // its own and needs the indent too.
        let starts_bare_line = is_line_start(self.virtual_lines.text(), virtual_start)
            && is_line_start(self.prepared_lines.text(), old.start);
        let new_text = self.reindent(virtual_start, starts_bare_line, new_text, following)?;
        Some(TextEdit {
            range: LspRange::new(
                self.virtual_lines.position(virtual_start),
                self.virtual_lines.position(virtual_end),
            ),
            new_text,
        })
    }

    fn hunk_touches_gap(&self, old: &Range<usize>) -> bool {
        self.runs
            .iter()
            .filter(|run| run.kind == RunKind::Gap)
            .any(|gap| {
                if gap.prepared.is_empty() {
                    // A gap replaced by nothing: only a change spanning its
                    // position removes the host text around it.
                    old.start < gap.prepared.start && gap.prepared.start < old.end
                } else {
                    old.start < gap.prepared.end && gap.prepared.start < old.end
                }
            })
    }

    /// Re-insert the segment's lost indentation at the start of every
    /// non-empty line the text begins: after each line break, and at the
    /// very start when `starts_bare_line`.
    fn reindent(
        &self,
        virtual_start: usize,
        starts_bare_line: bool,
        text: &str,
        following: Option<char>,
    ) -> Option<String> {
        let begins_line =
            |next: Option<char>| next.is_some_and(|next| next != '\n' && next != '\r');
        let indents_first = starts_bare_line && begins_line(text.chars().next().or(following));
        if !indents_first && !text.contains(['\n', '\r']) {
            return Some(text.to_string());
        }
        let indent = self
            .indents
            .iter()
            .find(|indent| {
                indent.virtual_range.start <= virtual_start
                    && virtual_start <= indent.virtual_range.end
            })
            .map_or(Some(""), |indent| indent.uniform.as_deref())?;
        if indent.is_empty() {
            return Some(text.to_string());
        }
        let mut output = String::with_capacity(text.len() + indent.len());
        if indents_first {
            output.push_str(indent);
        }
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            output.push(character);
            let line_break =
                character == '\n' || (character == '\r' && chars.peek() != Some(&'\n'));
            if line_break && begins_line(chars.peek().copied().or(following)) {
                output.push_str(indent);
            }
        }
        Some(output)
    }
}

#[derive(Clone, Copy)]
enum Side {
    Prepared,
    Virtual,
}

impl Side {
    /// The run's (source, target) ranges when mapping from this side.
    fn ranges(self, run: &Run) -> (&Range<usize>, &Range<usize>) {
        match self {
            Side::Prepared => (&run.prepared, &run.virtual_),
            Side::Virtual => (&run.virtual_, &run.prepared),
        }
    }
}

/// Map a byte offset across the runs.
///
/// Inside an identity run the offset moves by the run's shift. Inside an
/// opaque run (a deleted indent or a gap) it lands on the run's start, or —
/// strictly inside, with [`Bias::End`] — its end. On a boundary the run that
/// *starts* there wins, so a P line start maps after the indent V deleted
/// there: the host keeps its indentation and the edit lands on the content.
fn map_offset(runs: &[Run], offset: usize, bias: Bias, from: Side) -> usize {
    for run in runs {
        let (source, target) = from.ranges(run);
        if source.start <= offset && offset < source.end {
            return match run.kind {
                RunKind::Identity => target.start + (offset - source.start),
                _ if offset == source.start => target.start,
                _ => match bias {
                    Bias::Start => target.start,
                    Bias::End => target.end,
                },
            };
        }
    }
    // Past every run non-empty on this side: the document end, unless a run
    // empty on this side sits exactly here and the caller wants its start.
    if bias == Bias::Start
        && let Some(run) = runs.iter().find(|run| {
            let (source, _) = from.ranges(run);
            source.is_empty() && source.start == offset
        })
    {
        return from.ranges(run).1.start;
    }
    runs.last().map_or(0, |run| from.ranges(run).1.end)
}

// =============================================================================
// Text helpers
// =============================================================================

/// An owned text with an LSP line index (UTF-16 columns, LF / CRLF / lone CR
/// line terminators).
#[derive(Debug, PartialEq, Eq)]
struct LineMap {
    text: String,
    index: LineIndexEq,
}

impl LineMap {
    fn new(text: String) -> Self {
        let index = LineIndex::new(&normalize_lone_carriage_returns(&text));
        Self {
            text,
            index: LineIndexEq(index),
        }
    }

    fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of `position`, or `None` unless it names a real location.
    fn offset_strict(&self, position: Position) -> Option<usize> {
        let line_col = self.index.0.to_utf8(
            WideEncoding::Utf16,
            WideLineCol {
                line: position.line,
                col: position.character,
            },
        )?;
        let offset: usize = self.index.0.offset(line_col)?.into();
        (offset <= self.text.len()
            && self.text.is_char_boundary(offset)
            && self.position(offset) == position)
            .then_some(offset)
    }

    /// Byte offset of `position`, clamped to its line's content (a column
    /// past the end) or to the document end (a line past the end).
    fn offset_clamped(&self, position: Position) -> usize {
        let Some(line_range) = self.index.0.line(position.line) else {
            return self.text.len();
        };
        let line_start: usize = line_range.start().into();
        let line_end: usize = line_range.end().into();
        let line = &self.text[line_start..line_end];
        let content = line
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or_else(|| line.strip_suffix('\r').unwrap_or(line));
        let mut units = 0u32;
        for (byte, character) in content.char_indices() {
            if units >= position.character {
                return line_start + byte;
            }
            units += character.len_utf16() as u32;
        }
        line_start + content.len()
    }

    fn position(&self, offset: usize) -> Position {
        let offset = self.text.floor_char_boundary(offset.min(self.text.len()));
        let Some(line_col) = u32::try_from(offset)
            .ok()
            .and_then(|offset| self.index.0.try_line_col(offset.into()))
        else {
            return Position::new(0, 0);
        };
        self.index
            .0
            .to_wide(WideEncoding::Utf16, line_col)
            .map_or(Position::new(line_col.line, line_col.col), |wide| {
                Position::new(wide.line, wide.col)
            })
    }
}

/// `LineIndex` is a pure function of the text, so two indexes over equal text
/// are equal; this lets `LineMap` derive equality from its text alone.
#[derive(Debug)]
struct LineIndexEq(LineIndex);

impl PartialEq for LineIndexEq {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for LineIndexEq {}

fn normalize_lone_carriage_returns(text: &str) -> std::borrow::Cow<'_, str> {
    if !text
        .as_bytes()
        .iter()
        .enumerate()
        .any(|(index, byte)| *byte == b'\r' && text.as_bytes().get(index + 1) != Some(&b'\n'))
    {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let normalized: String = text
        .char_indices()
        .map(|(index, character)| {
            if character == '\r' && bytes.get(index + 1) != Some(&b'\n') {
                '\n'
            } else {
                character
            }
        })
        .collect();
    std::borrow::Cow::Owned(normalized)
}

fn is_line_start(text: &str, offset: usize) -> bool {
    offset == 0 || matches!(text.as_bytes().get(offset - 1), Some(b'\n' | b'\r'))
}

/// Apply LSP edits to `lines`' text; `None` when an edit is reversed or
/// edits overlap. A column past its line's end means the line's end, as LSP
/// prescribes.
fn apply_edits(lines: &LineMap, edits: &[TextEdit]) -> Option<String> {
    let mut ranges = edits
        .iter()
        .map(|edit| {
            let start = lines.offset_clamped(edit.range.start);
            let end = lines.offset_clamped(edit.range.end);
            (start <= end).then_some((start..end, edit.new_text.as_str()))
        })
        .collect::<Option<Vec<_>>>()?;
    // Stable sort keeps same-position inserts in the order the server sent.
    ranges.sort_by_key(|(range, _)| range.start);
    if ranges
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start)
    {
        return None;
    }
    let text = lines.text();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for (range, new_text) in ranges {
        output.push_str(&text[cursor..range.start]);
        output.push_str(new_text);
        cursor = range.end;
    }
    output.push_str(&text[cursor..]);
    Some(output)
}

struct Hunk {
    old: Range<usize>,
    new: Range<usize>,
}

/// Character-level diff hunks between `old` and `new`, as byte ranges.
fn diff_hunks(old: &str, new: &str) -> Vec<Hunk> {
    let old_chars: Vec<(usize, char)> = old.char_indices().collect();
    let new_chars: Vec<(usize, char)> = new.char_indices().collect();
    let old_values: Vec<char> = old_chars.iter().map(|(_, c)| *c).collect();
    let new_values: Vec<char> = new_chars.iter().map(|(_, c)| *c).collect();
    let byte_at = |chars: &[(usize, char)], text: &str, index: usize| {
        chars.get(index).map_or(text.len(), |(byte, _)| *byte)
    };
    let ops = similar::capture_diff_slices_deadline(
        similar::Algorithm::Myers,
        &old_values,
        &new_values,
        Some(Instant::now() + EDIT_DIFF_BUDGET),
    );
    let mut hunks: Vec<Hunk> = Vec::new();
    for op in ops {
        let (old_range, new_range) = match op {
            similar::DiffOp::Equal { .. } => continue,
            similar::DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => (old_index..old_index + old_len, new_index..new_index),
            similar::DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (old_index..old_index, new_index..new_index + new_len),
            similar::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (
                old_index..old_index + old_len,
                new_index..new_index + new_len,
            ),
        };
        let old_bytes =
            byte_at(&old_chars, old, old_range.start)..byte_at(&old_chars, old, old_range.end);
        let new_bytes =
            byte_at(&new_chars, new, new_range.start)..byte_at(&new_chars, new, new_range.end);
        match hunks.last_mut() {
            Some(last) if last.old.end == old_bytes.start && last.new.end == new_bytes.start => {
                last.old.end = old_bytes.end;
                last.new.end = new_bytes.end;
            }
            _ => hunks.push(Hunk {
                old: old_bytes,
                new: new_bytes,
            }),
        }
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(value: serde_json::Value) -> Option<PrepareResult> {
        serde_json::from_value(value).unwrap()
    }

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    fn edit(start: (u32, u32), end: (u32, u32), new_text: &str) -> TextEdit {
        TextEdit {
            range: LspRange::new(pos(start.0, start.1), pos(end.0, end.1)),
            new_text: new_text.to_string(),
        }
    }

    fn apply_to(text: &str, edits: &[TextEdit]) -> String {
        apply_edits(&LineMap::new(text.to_string()), edits).unwrap()
    }

    /// `x = ${a}\nprint(x)\n` as a combined document: the interpolation is a
    /// gap, masked in V by coordinate-preserving whitespace.
    fn interpolated() -> (String, VirtualLayout) {
        let virtual_text = "x =     \nprint(x)\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..8, "${a}".to_string()),
                (SegmentKind::Content, 8..18, String::new()),
            ],
        );
        (virtual_text, layout)
    }

    /// Two lines indented by two spaces, dedented by the peer.
    fn dedented() -> (String, PreparedDocument) {
        let virtual_text = "  if x:\n    y\n".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        (virtual_text, prepared)
    }

    #[test]
    fn layout_presents_gap_host_text_and_merges_stripped_prefixes() {
        let virtual_text = "a\nb\n  c";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                // A stripped blockquote prefix: empty in V, not presented.
                (SegmentKind::Gap, 2..2, "> ".to_string()),
                (SegmentKind::Content, 2..4, String::new()),
                (SegmentKind::Gap, 4..6, "${".to_string()),
                (SegmentKind::Gap, 6..6, "}".to_string()),
                (SegmentKind::Content, 6..7, String::new()),
            ],
        );
        let kinds: Vec<_> = layout
            .segments()
            .iter()
            .map(|s| (s.kind, s.virtual_range.clone(), s.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (SegmentKind::Content, 0..4, "a\nb\n"),
                (SegmentKind::Gap, 4..6, "${"),
                (SegmentKind::Content, 6..7, "c"),
            ]
        );
    }

    #[test]
    fn params_serialize_segments_in_order() {
        let (_, layout) = interpolated();
        let params = PrepareParams::new(
            PrepareTextDocument {
                uri: "file:///virtual.py",
                language_id: "python",
                version: 3,
            },
            PrepareHostTextDocument {
                uri: "file:///host.nix",
                language_id: "nix",
            },
            &layout,
        );
        assert_eq!(
            serde_json::to_value(params).unwrap(),
            json!({
                "textDocument": {"uri": "file:///virtual.py", "languageId": "python", "version": 3},
                "hostTextDocument": {"uri": "file:///host.nix", "languageId": "nix"},
                "segments": [
                    {"type": "content", "content": "x = "},
                    {"type": "gap", "content": "${a}"},
                    {"type": "content", "content": "\nprint(x)\n"}
                ]
            })
        );
    }

    #[test]
    fn null_answer_keeps_the_text_but_still_protects_gaps() {
        let (virtual_text, layout) = interpolated();
        let prepared = apply_prepare_result(&virtual_text, &layout, None).unwrap();
        assert_eq!(prepared.text, virtual_text);
        let map = prepared.map.expect("gaps stay protected");
        assert_eq!(map.edits_to_virtual(&[edit((0, 5), (0, 6), "1")]), None);
    }

    #[test]
    fn null_answer_without_gaps_needs_no_map() {
        let layout = VirtualLayout::single("a\n");
        let prepared = apply_prepare_result("a\n", &layout, None).unwrap();
        assert_eq!(prepared, PreparedDocument::unchanged("a\n"));
    }

    #[test]
    fn omitted_fields_keep_their_segments() {
        let (virtual_text, layout) = interpolated();
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(
                json!({"segments": [{"type": "content"}, {"type": "gap"}, {"type": "content"}]}),
            ),
        )
        .unwrap();
        assert_eq!(prepared.text, virtual_text);
    }

    #[test]
    fn gap_replacement_may_change_length() {
        let (virtual_text, layout) = interpolated();
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "x = None\nprint(x)\n");
        let map = prepared.map.unwrap();
        // Content after the gap moves with the gap's new length…
        assert_eq!(map.to_virtual(pos(1, 6), Bias::Start), pos(1, 6));
        assert_eq!(map.to_prepared(pos(0, 2), Bias::Start), pos(0, 2));
        // …and a range over the placeholder covers the whole original gap.
        assert_eq!(map.to_virtual(pos(0, 4), Bias::Start), pos(0, 4));
        assert_eq!(map.to_virtual(pos(0, 6), Bias::End), pos(0, 8));
    }

    #[test]
    fn shrinking_gap_shifts_later_columns_on_its_line() {
        let virtual_text = "f(    , b)".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..6, "${a}".to_string()),
                (SegmentKind::Content, 6..10, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": "1"}, {"type": "content"}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "f(1, b)");
        let map = prepared.map.unwrap();
        // `b` sits at column 5 in P and column 8 in V.
        assert_eq!(map.to_virtual(pos(0, 5), Bias::Start), pos(0, 8));
        assert_eq!(map.to_prepared(pos(0, 8), Bias::Start), pos(0, 5));
    }

    #[test]
    fn multiline_gap_may_collapse_lines() {
        let virtual_text = "a\n\n\nb".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..1, String::new()),
                (SegmentKind::Gap, 1..4, "\n```\n```lua\n".to_string()),
                (SegmentKind::Content, 4..5, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": "\n"}, {"type": "content"}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\nb");
        let map = prepared.map.unwrap();
        assert_eq!(map.to_virtual(pos(1, 0), Bias::Start), pos(3, 0));
        assert_eq!(map.to_prepared(pos(3, 0), Bias::Start), pos(1, 0));
    }

    #[test]
    fn dedent_shifts_columns_by_the_deleted_indent() {
        let (_, prepared) = dedented();
        assert_eq!(prepared.text, "if x:\n  y\n");
        let map = prepared.map.unwrap();
        assert_eq!(map.to_virtual(pos(1, 2), Bias::Start), pos(1, 4));
        assert_eq!(map.to_prepared(pos(1, 4), Bias::Start), pos(1, 2));
        // A V position inside the deleted indent lands on the P line start.
        assert_eq!(map.to_prepared(pos(1, 1), Bias::Start), pos(1, 0));
        // A P line start maps after the deleted indent.
        assert_eq!(map.to_virtual(pos(1, 0), Bias::Start), pos(1, 2));
    }

    #[test]
    fn content_change_must_be_leading_whitespace_deletion() {
        let virtual_text = "  a b\n".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        for (change, reason) in [
            (
                json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": "x"}),
                "content changes may only delete text",
            ),
            (
                json!({"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}, "newText": ""}),
                "content changes may only delete leading whitespace",
            ),
            (
                json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": ""}),
                "content changes may only delete spaces and tabs",
            ),
            (
                json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 99}}, "newText": ""}),
                "change ends outside the segment",
            ),
        ] {
            assert_eq!(
                apply_prepare_result(
                    &virtual_text,
                    &layout,
                    result(json!({"segments": [{"type": "content", "changes": [change]}]})),
                ),
                Err(PrepareError::InvalidChange { index: 0, reason }),
            );
        }
    }

    #[test]
    fn content_change_positions_are_segment_relative() {
        let virtual_text = "x\n    \n  y\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..7, "${a}\n".to_string()),
                (SegmentKind::Content, 7..11, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [
                {"type": "content"},
                {"type": "gap"},
                {"type": "content", "changes": [
                    {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
                ]}
            ]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "x\n    \ny\n");
    }

    #[test]
    fn mismatched_answers_are_refused() {
        let (virtual_text, layout) = interpolated();
        assert_eq!(
            apply_prepare_result(
                &virtual_text,
                &layout,
                result(json!({"segments": [{"type": "content"}]}))
            ),
            Err(PrepareError::SegmentCount {
                expected: 3,
                actual: 1
            })
        );
        assert_eq!(
            apply_prepare_result(
                &virtual_text,
                &layout,
                result(
                    json!({"segments": [{"type": "gap"}, {"type": "gap"}, {"type": "content"}]})
                )
            ),
            Err(PrepareError::SegmentKind {
                index: 0,
                expected: SegmentKind::Content
            })
        );
    }

    #[test]
    fn whole_document_format_reindents_new_lines() {
        let (virtual_text, prepared) = dedented();
        let map = prepared.map.unwrap();
        // A formatter rewrites P wholesale, adding a statement.
        let edits = [edit((0, 0), (2, 0), "if x:\n  y\n  z\n")];
        let virtual_edits = map.edits_to_virtual(&edits).unwrap();
        assert_eq!(
            apply_to(&virtual_text, &virtual_edits),
            "  if x:\n    y\n    z\n"
        );
    }

    #[test]
    fn edit_joining_lines_drops_the_inner_indent() {
        let (virtual_text, prepared) = dedented();
        let map = prepared.map.unwrap();
        let edits = [edit((0, 5), (1, 0), " ")];
        let virtual_edits = map.edits_to_virtual(&edits).unwrap();
        assert_eq!(apply_to(&virtual_text, &virtual_edits), "  if x:   y\n");
    }

    #[test]
    fn edit_indenting_a_line_keeps_the_host_indent() {
        let (virtual_text, prepared) = dedented();
        let map = prepared.map.unwrap();
        let virtual_edits = map.edits_to_virtual(&[edit((1, 0), (1, 0), "  ")]).unwrap();
        assert_eq!(
            apply_to(&virtual_text, &virtual_edits),
            "  if x:\n      y\n"
        );
    }

    #[test]
    fn edit_around_a_gap_is_kept_and_into_it_refused() {
        let (virtual_text, layout) = interpolated();
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        // `x = None` → `x=None`: the change is in content before the gap.
        let virtual_edits = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "x=None\nprint(x)\n")])
            .unwrap();
        assert_eq!(
            apply_to(&virtual_text, &virtual_edits),
            "x=    \nprint(x)\n"
        );
        // Touching the placeholder would edit host-owned text.
        assert_eq!(map.edits_to_virtual(&[edit((0, 4), (0, 8), "1")]), None);
        assert_eq!(
            map.edits_to_virtual(&[edit((0, 0), (2, 0), "x = 1\nprint(x)\n")]),
            None
        );
    }

    #[test]
    fn exact_edit_keeps_its_extent_and_reindents() {
        let (virtual_text, prepared) = dedented();
        let map = prepared.map.unwrap();
        // A completion replacing `y` (P line 1, columns 2..3) keeps its range.
        let completed = map.edit_to_virtual(&edit((1, 2), (1, 3), "yes")).unwrap();
        assert_eq!(completed.range, LspRange::new(pos(1, 4), pos(1, 5)));
        // An inserted block gains the host indent on each new line.
        let inserted = map.edit_to_virtual(&edit((1, 3), (1, 3), "\n  z")).unwrap();
        assert_eq!(
            apply_to(&virtual_text, &[inserted]),
            "  if x:\n    y\n    z\n"
        );
    }

    #[test]
    fn reindent_refuses_non_uniform_indentation() {
        let virtual_text = "  a\n    b\n".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        // Same-line edits still map…
        assert!(map.edits_to_virtual(&[edit((0, 0), (0, 1), "A")]).is_some());
        // …but a new line has no single right indentation.
        assert_eq!(map.edits_to_virtual(&[edit((0, 1), (0, 1), "\nc")]), None);
    }
}
