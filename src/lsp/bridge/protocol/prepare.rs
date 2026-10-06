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

/// How long one prepare request may wait for its answer (after the peer is
/// up). A request that times out counts as no answer and is retried with
/// backoff; the document is held back meanwhile.
pub(crate) const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on the diff that maps a formatting result back through P → V.
/// A diff that cannot finish in time degrades to coarser hunks: still
/// correct, but more likely to span a gap and be refused.
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
    /// (stripped host text, such as a blockquote `> `) is not presented on its
    /// own: between content it never reaches downstream servers, so there is
    /// nothing to replace, and the content around it merges into one segment.
    /// Next to a visible gap it is part of that gap's host text.
    pub(crate) fn from_pieces(
        virtual_text: &str,
        pieces: impl IntoIterator<Item = (SegmentKind, Range<usize>, String)>,
    ) -> Self {
        let mut segments: Vec<Segment> = Vec::new();
        let mut pending_gap_text = String::new();
        for (kind, range, host_text) in pieces {
            if kind == SegmentKind::Gap && range.is_empty() {
                match segments.last_mut() {
                    // Stripped text right after a visible gap is part of it.
                    Some(last)
                        if last.kind == SegmentKind::Gap
                            && last.virtual_range.end == range.start =>
                    {
                        last.text.push_str(&host_text);
                    }
                    _ => pending_gap_text.push_str(&host_text),
                }
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
    // a failure to get an answer at all. The cancellation codes LSP lets a
    // client retry (RequestCancelled, ContentModified, ServerCancelled) are
    // not answers.
    if let Some(error) = response.get("error").filter(|error| !error.is_null()) {
        let retryable = matches!(
            error.get("code").and_then(serde_json::Value::as_i64),
            Some(-32802..=-32800)
        );
        return Err(std::io::Error::new(
            if retryable {
                std::io::ErrorKind::Interrupted
            } else {
                std::io::ErrorKind::InvalidData
            },
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
    /// `Some` for every document that went through a peer, even when its
    /// answer changed nothing.
    pub(crate) map: Option<std::sync::Arc<PreparedMap>>,
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
    // `null` keeps every segment, and still yields a map (see below).
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

    // Even an answer that changed nothing yields a map: the map is how every
    // later path knows the document went through the peer (and must wait
    // for the prepared text to reach a server, keep off its gaps, …).
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

    /// The prepared text this map was built for.
    pub(crate) fn prepared_text(&self) -> &str {
        self.prepared_lines.text()
    }

    /// Whether any of these P edits, as sent, reaches into a gap. Mapping a
    /// set through the diff ([`Self::edits_to_virtual`]) checks only what
    /// changed: an edit that rewrites a gap's replacement with the same text
    /// passes there, which suits a formatter's whole-document result but not
    /// an edit whose extent states what it replaces (a rename, a code
    /// action).
    pub(crate) fn edits_touch_gap(&self, edits: &[TextEdit]) -> bool {
        edits.iter().any(|edit| {
            let start = self.prepared_lines.offset_clamped(edit.range.start);
            let end = self.prepared_lines.offset_clamped(edit.range.end);
            self.hunk_touches_gap(&(start.min(end)..start.max(end)))
        })
    }

    /// Translate one edit a downstream server made to P into an edit to V,
    /// keeping its extent (a completion's replace range, say) rather than
    /// minimizing it — except that a change at a dedented line's start also
    /// covers the indent the peer removed there, which it restores. `None` — refuse the edit — under the same rules as
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
        // A change starting at a P line start that removes whole lines (its
        // old text spans a line break), clears a line, or opens with a line
        // break covers the whole line in V, its removed indent included:
        // deleting or clearing a dedented line must not leave that indent
        // behind as trailing whitespace. Its end then keeps the next line's
        // indent the same way. Anywhere else (a change within one line's
        // content, an insertion, a change joining onto the previous line) a
        // line start maps after its removed indent, as positions do — so a
        // completion replacing the line's first word keeps the indent.
        let prepared = self.prepared_lines.text();
        let clears_line = new_text.is_empty() && is_line_end(prepared, old.end);
        let whole_lines = is_line_start(prepared, old.start)
            && (prepared[old.clone()].contains(['\n', '\r'])
                || clears_line && !old.is_empty()
                || new_text.starts_with(['\n', '\r']));
        let plain_start = map_offset(&self.runs, old.start, Bias::Start, Side::Prepared);
        // The removed indent right before where the start plainly maps — not
        // one beyond a gap the peer emptied at the same P offset.
        let start_indent = whole_lines
            .then(|| empty_run(&self.runs, old.start, RunKind::Deleted))
            .flatten()
            .filter(|run| run.virtual_.end == plain_start);
        let virtual_start = start_indent.map_or(plain_start, |run| run.virtual_.start);
        // The end maps plainly: after a removed indent at a line start, which
        // the change then takes along and `reindent` restores once.
        let virtual_end = if old.is_empty() {
            plain_start
        } else {
            map_offset(&self.runs, old.end, Bias::End, Side::Prepared)
        };
        // Backstop: host text a gap stands for is never inside a mapped change.
        if self.virtual_range_touches_gap(virtual_start, virtual_end) {
            return None;
        }
        // What follows the change in V: a gap the peer emptied right at its
        // end is not the text after it in P.
        let following = match empty_run(&self.runs, old.end, RunKind::Gap) {
            Some(gap) if gap.virtual_.start == virtual_end => None,
            _ => following,
        };
        let virtual_text = self.virtual_lines.text();
        // The existing line after a change ending at a P line start keeps its
        // own removed indent (none, for a line the peer did not dedent), not
        // the segment's uniform one.
        let tail = (following.is_some() && is_line_start(self.prepared_lines.text(), old.end))
            .then(|| {
                empty_run(&self.runs, old.end, RunKind::Deleted)
                    .map_or("", |run| &virtual_text[run.virtual_.clone()])
            });
        let first = match start_indent {
            // Whole lines replaced by text without a line break, before a
            // non-blank line: what remains is that existing line, which keeps
            // its own indent (deleting a line must not re-indent the next).
            Some(_)
                if !new_text.contains(['\n', '\r'])
                    && tail.is_some()
                    && following.is_some_and(|next| next != '\n' && next != '\r') =>
            {
                FirstIndent::Exact(tail.unwrap_or_default())
            }
            // The replaced line's own removed indent comes back.
            Some(run) => FirstIndent::Exact(&virtual_text[run.virtual_.clone()]),
            // Text landing at a V line start that kept no indent before it
            // starts a line of its own — at a blank line or the document end,
            // or when it breaks the line — and needs the indent too. Text that
            // just joins a non-blank line the peer did not dedent belongs to
            // that line, which has no indent to restore.
            None if is_line_start(virtual_text, virtual_start)
                && is_line_start(self.prepared_lines.text(), old.start)
                && (is_blank_from(virtual_text, virtual_start)
                    || new_text.contains(['\n', '\r'])) =>
            {
                FirstIndent::Uniform
            }
            None => FirstIndent::None,
        };
        let new_text = self.reindent(virtual_start, first, new_text, following, tail)?;
        Some(TextEdit {
            range: LspRange::new(
                self.virtual_lines.position(virtual_start),
                self.virtual_lines.position(virtual_end),
            ),
            new_text,
        })
    }

    /// The V ranges of the gaps: host-owned text in the virtual document.
    pub(crate) fn virtual_gap_ranges(&self) -> impl Iterator<Item = LspRange> + '_ {
        self.runs
            .iter()
            .filter(|run| run.kind == RunKind::Gap)
            .map(|gap| {
                LspRange::new(
                    self.virtual_lines.position(gap.virtual_.start),
                    self.virtual_lines.position(gap.virtual_.end),
                )
            })
    }

    /// Whether a V position sits strictly inside a gap: host-owned text no
    /// downstream position stands for.
    pub(crate) fn virtual_position_in_gap(&self, position: Position) -> bool {
        let offset = self.virtual_lines.offset_clamped(position);
        self.virtual_range_touches_gap(offset, offset)
    }

    /// Whether the V range `start..end` reaches inside a gap's V text (an
    /// empty range: whether it sits strictly inside one).
    fn virtual_range_touches_gap(&self, start: usize, end: usize) -> bool {
        let first = self.runs.partition_point(|run| run.virtual_.end <= start);
        self.runs[first..]
            .iter()
            .take_while(|run| run.virtual_.start <= end)
            .filter(|run| run.kind == RunKind::Gap)
            .any(|gap| {
                if start == end {
                    gap.virtual_.start < start && start < gap.virtual_.end
                } else {
                    gap.virtual_.start < end && start < gap.virtual_.end
                }
            })
    }

    fn hunk_touches_gap(&self, old: &Range<usize>) -> bool {
        // Runs tile P in order: only those from the first ending at or after
        // the change's start can touch it.
        let first = self
            .runs
            .partition_point(|run| run.prepared.end < old.start);
        self.runs[first..]
            .iter()
            .take_while(|run| run.prepared.start <= old.end)
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
    /// non-empty line the text begins: after each line break (the segment's
    /// uniform indent, or `tail` before the existing line that follows), and
    /// at the very start as `first` says.
    fn reindent(
        &self,
        virtual_start: usize,
        first: FirstIndent<'_>,
        text: &str,
        following: Option<char>,
        tail: Option<&str>,
    ) -> Option<String> {
        let begins_line =
            |next: Option<char>| next.is_some_and(|next| next != '\n' && next != '\r');
        let breaks_line = text.contains(['\n', '\r']);
        let first = if begins_line(text.chars().next().or(following)) {
            first
        } else {
            FirstIndent::None
        };
        if matches!(first, FirstIndent::None) && !breaks_line {
            return Some(text.to_string());
        }
        let uniform = || {
            let candidate = self
                .indents
                .partition_point(|indent| indent.virtual_range.end < virtual_start);
            self.indents
                .get(candidate)
                .filter(|indent| indent.virtual_range.start <= virtual_start)
                .map_or(Some(""), |indent| indent.uniform.as_deref())
        };
        let first_indent = match first {
            FirstIndent::None => "",
            FirstIndent::Exact(indent) => indent,
            FirstIndent::Uniform => uniform()?,
        };
        let line_indent = if breaks_line { uniform()? } else { "" };
        let mut output = String::with_capacity(text.len() + first_indent.len() + line_indent.len());
        output.push_str(first_indent);
        let mut chars = text.chars().peekable();
        while let Some(character) = chars.next() {
            output.push(character);
            let line_break =
                character == '\n' || (character == '\r' && chars.peek() != Some(&'\n'));
            if line_break && begins_line(chars.peek().copied().or(following)) {
                match (chars.peek(), tail) {
                    // The break before existing text: that line's own indent.
                    (None, Some(tail)) => output.push_str(tail),
                    _ => output.push_str(line_indent),
                }
            }
        }
        Some(output)
    }
}

/// The indentation a mapped edit's first line regains.
#[derive(Clone, Copy)]
enum FirstIndent<'a> {
    /// None: the edit starts after an indent V still has.
    None,
    /// The segment's uniform indent: the edit starts a line V never indented
    /// (the document end, or a line the peer did not dedent).
    Uniform,
    /// Exactly this: the edit replaces a whole line including the indent the
    /// peer removed from it.
    Exact(&'a str),
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

/// Map a byte offset across the runs, in O(log runs).
///
/// Inside an identity run the offset moves by the run's shift. Inside an
/// opaque run (a deleted indent or a gap) it lands on the run's start, or —
/// strictly inside, with [`Bias::End`] — its end. On a boundary the run that
/// *starts* there wins, so a P line start maps after the indent V deleted
/// there: the host keeps its indentation and the edit lands on the content.
fn map_offset(runs: &[Run], offset: usize, bias: Bias, from: Side) -> usize {
    // The end of a P range that reaches a gap the peer replaced with nothing
    // stops before that gap: the range covers none of its host text.
    if bias == Bias::End
        && matches!(from, Side::Prepared)
        && let Some(gap) = empty_run(runs, offset, RunKind::Gap)
    {
        return gap.virtual_.start;
    }
    // Runs tile both texts in order, so the run holding `offset` is the
    // first that ends after it (runs empty on this side never hold one).
    let index = runs.partition_point(|run| from.ranges(run).0.end <= offset);
    if let Some(run) = runs.get(index) {
        let (source, target) = from.ranges(run);
        if source.start <= offset {
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
    if bias == Bias::Start {
        let first = runs.partition_point(|run| from.ranges(run).0.start < offset);
        if let Some(run) = runs[first..]
            .iter()
            .take_while(|run| from.ranges(run).0.start == offset)
            .find(|run| from.ranges(run).0.is_empty())
        {
            let target = from.ranges(run).1;
            // A removed indent at the very end is passed, as at any line
            // start; a gap emptied there stays after the position.
            return match run.kind {
                RunKind::Deleted => target.end,
                _ => target.start,
            };
        }
    }
    runs.last().map_or(0, |run| from.ranges(run).1.end)
}

/// The `kind` run that is empty in P and sits at P `offset`.
fn empty_run(runs: &[Run], offset: usize, kind: RunKind) -> Option<&Run> {
    let first = runs.partition_point(|run| run.prepared.start < offset);
    runs[first..]
        .iter()
        .take_while(|run| run.prepared.start == offset)
        .find(|run| run.kind == kind && run.prepared.is_empty())
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

fn is_line_end(text: &str, offset: usize) -> bool {
    offset == text.len() || matches!(text.as_bytes().get(offset), Some(b'\n' | b'\r'))
}

/// Whether `text` holds only whitespace from `offset` to its line's end.
fn is_blank_from(text: &str, offset: usize) -> bool {
    text[offset..]
        .split(['\n', '\r'])
        .next()
        .is_none_or(|rest| rest.trim().is_empty())
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
        let mut old_bytes =
            byte_at(&old_chars, old, old_range.start)..byte_at(&old_chars, old, old_range.end);
        let mut new_bytes =
            byte_at(&new_chars, new, new_range.start)..byte_at(&new_chars, new, new_range.end);
        // A hunk must not split a CRLF: a position between its CR and LF is
        // past the line's content, where LSP clamps it, so a deletion of
        // just the CR would apply as nothing. Widen into the equal text on
        // either side (the same in both texts).
        let splits_crlf = |text: &str, offset: usize| {
            offset > 0
                && text.as_bytes()[offset - 1] == b'\r'
                && text.as_bytes().get(offset) == Some(&b'\n')
        };
        if splits_crlf(old, old_bytes.start) || splits_crlf(new, new_bytes.start) {
            old_bytes.start -= 1;
            new_bytes.start -= 1;
        }
        if splits_crlf(old, old_bytes.end) || splits_crlf(new, new_bytes.end) {
            old_bytes.end += 1;
            new_bytes.end += 1;
        }
        match hunks.last_mut() {
            Some(last) if last.old.end >= old_bytes.start && last.new.end >= new_bytes.start => {
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
                (SegmentKind::Gap, 4..6, "${}"),
                (SegmentKind::Content, 6..7, "c"),
            ]
        );
    }

    #[test]
    fn error_responses_are_final_unless_retryable() {
        let kind = |code: i64| {
            parse_prepare_response(&json!({"error": {"code": code, "message": "x"}}))
                .unwrap_err()
                .kind()
        };
        assert_eq!(kind(-32603), std::io::ErrorKind::InvalidData);
        for retryable in [-32800, -32801, -32802] {
            assert_eq!(kind(retryable), std::io::ErrorKind::Interrupted);
        }
        assert_eq!(
            parse_prepare_response(&json!({"result": {"segments": "x"}}))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(
            parse_prepare_response(&json!({"result": null}))
                .unwrap()
                .is_none()
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
    fn null_answer_without_gaps_still_maps_identically() {
        let layout = VirtualLayout::single("a\n");
        let prepared = apply_prepare_result("a\n", &layout, None).unwrap();
        assert_eq!(prepared.text, "a\n");
        let map = prepared
            .map
            .expect("a prepared document always carries a map");
        assert_eq!(map.to_virtual(pos(0, 1), Bias::Start), pos(0, 1));
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

    /// `ab${x}cd` with the interpolation (a gap) replaced by nothing.
    fn emptied_gap() -> (String, PreparedMap) {
        let virtual_text = "ab    cd".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..6, "${x}".to_string()),
                (SegmentKind::Content, 6..8, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "abcd");
        (
            virtual_text,
            std::sync::Arc::try_unwrap(prepared.map.unwrap()).unwrap(),
        )
    }

    #[test]
    fn a_change_before_an_emptied_gap_keeps_the_gap() {
        let (virtual_text, map) = emptied_gap();
        // Deleting `b` must not reach over the gap's host text.
        let deleted = map.edit_to_virtual(&edit((0, 1), (0, 2), "")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[deleted]), "a    cd");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (0, 4), "aBcd")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &formatted), "aB    cd");
        // A range ending there stops before the gap too…
        assert_eq!(map.to_virtual(pos(0, 2), Bias::End), pos(0, 2));
        // …while an insertion there stays an insertion.
        let inserted = map.edit_to_virtual(&edit((0, 2), (0, 2), "!")).unwrap();
        assert_eq!(inserted.range.start, inserted.range.end);
    }

    fn dedent_by_two(virtual_text: &str) -> PreparedMap {
        let layout = VirtualLayout::single(virtual_text);
        let changes: Vec<_> = virtual_text
            .split('\n')
            .enumerate()
            .filter(|(_, line)| line.starts_with("  "))
            .map(|(line, _)| json!({
                "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 2}},
                "newText": ""
            }))
            .collect();
        let prepared = apply_prepare_result(
            virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": changes}]})),
        )
        .unwrap();
        std::sync::Arc::try_unwrap(prepared.map.unwrap()).unwrap()
    }

    #[test]
    fn an_undedented_line_stays_unindented() {
        let virtual_text = "  a\nb\n";
        let layout = VirtualLayout::single(virtual_text);
        let prepared = apply_prepare_result(
            virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        let replaced = map.edit_to_virtual(&edit((1, 0), (1, 1), "B")).unwrap();
        assert_eq!(apply_to(virtual_text, &[replaced]), "  a\nB\n");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "a\nB\n")])
            .unwrap();
        assert_eq!(apply_to(virtual_text, &formatted), "  a\nB\n");
    }

    #[test]
    fn replacing_a_dedented_lines_first_word_keeps_its_indent() {
        let (virtual_text, prepared) = dedented();
        let map = prepared.map.unwrap();
        // A completion's replace range over `if` (insert range empty there).
        let replaced = map.edit_to_virtual(&edit((0, 0), (0, 2), "while")).unwrap();
        let inserted = map.edit_to_virtual(&edit((0, 0), (0, 0), "while")).unwrap();
        assert_eq!(replaced.new_text, inserted.new_text);
        assert_eq!(replaced.range.start, inserted.range.start);
        assert_eq!(apply_to(&virtual_text, &[replaced]), "  while x:\n    y\n");
    }

    #[test]
    fn inserting_on_an_indented_last_line_keeps_one_indent() {
        let virtual_text = "  a\n  ";
        let layout = VirtualLayout::single(virtual_text);
        let prepared = apply_prepare_result(
            virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\n");
        let map = prepared.map.unwrap();
        let inserted = map.edit_to_virtual(&edit((1, 0), (1, 0), "b")).unwrap();
        assert_eq!(apply_to(virtual_text, &[inserted]), "  a\n  b");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (1, 0), "a\nb")])
            .unwrap();
        assert_eq!(apply_to(virtual_text, &formatted), "  a\n  b");
    }

    #[test]
    fn crlf_to_lf_formatting_changes_the_terminators() {
        let virtual_text = "  a\r\n  b\r\n";
        let map = dedent_all(virtual_text, "  ");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "a\nb\n")])
            .unwrap();
        assert_eq!(apply_to(virtual_text, &formatted), "  a\n  b\n");
    }

    #[test]
    fn deleting_a_line_keeps_the_next_lines_own_indent() {
        // Lines indented differently, each fully dedented by the peer.
        let virtual_text = "  a\n    b\n";
        let layout = VirtualLayout::single(virtual_text);
        let prepared = apply_prepare_result(
            virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        let removed = map.edit_to_virtual(&edit((0, 0), (1, 0), "")).unwrap();
        assert_eq!(apply_to(virtual_text, &[removed]), "    b\n");
    }

    #[test]
    fn inserted_blank_lines_carry_no_indent() {
        let imports = "  import os\n  x = 1\n";
        let map = dedent_by_two(imports);
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "import os\n\n\nx = 1\n")])
            .unwrap();
        assert_eq!(apply_to(imports, &formatted), "  import os\n\n\n  x = 1\n");
        let inserted = map.edit_to_virtual(&edit((1, 0), (1, 0), "\n")).unwrap();
        assert_eq!(apply_to(imports, &[inserted]), "  import os\n\n  x = 1\n");
    }

    #[test]
    fn a_line_the_peer_left_alone_gains_no_indent() {
        // Only the first line was dedented.
        let virtual_text = "  a\nb\n";
        let layout = VirtualLayout::single(virtual_text);
        let prepared = apply_prepare_result(
            virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        let inserted = map.edit_to_virtual(&edit((1, 0), (1, 0), "x\n")).unwrap();
        assert_eq!(apply_to(virtual_text, &[inserted]), "  a\n  x\nb\n");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "a\n\nb\n")])
            .unwrap();
        assert_eq!(apply_to(virtual_text, &formatted), "  a\n\nb\n");
    }

    /// A small deterministic generator for the property test below.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    /// Every non-empty line of `prepared` indented by `indent`.
    fn indent_lines(prepared: &str, indent: &str) -> String {
        let mut output = String::new();
        let mut at_line_start = true;
        let mut chars = prepared.chars().peekable();
        while let Some(character) = chars.next() {
            if at_line_start && character != '\n' && character != '\r' {
                output.push_str(indent);
            }
            output.push(character);
            at_line_start = character == '\n' || (character == '\r' && chars.peek() != Some(&'\n'));
        }
        output
    }

    /// A map dedenting every line of `virtual_text` that starts with `indent`.
    fn dedent_all(virtual_text: &str, indent: &str) -> PreparedMap {
        let lines = LineMap::new(virtual_text.to_string());
        let changes: Vec<_> = (0..=virtual_text.len())
            .filter(|&offset| {
                virtual_text.is_char_boundary(offset)
                    && is_line_start(virtual_text, offset)
                    // Not between the CR and LF of one terminator.
                    && !(offset > 0
                        && virtual_text.as_bytes()[offset - 1] == b'\r'
                        && virtual_text.as_bytes().get(offset) == Some(&b'\n'))
                    && virtual_text[offset..].starts_with(indent)
            })
            .map(|offset| {
                json!({"range": {
                    "start": lines.position(offset),
                    "end": lines.position(offset + indent.len())
                }, "newText": ""})
            })
            .collect();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            result(json!({"segments": [{"type": "content", "changes": changes}]})),
        )
        .unwrap();
        std::sync::Arc::try_unwrap(prepared.map.unwrap()).unwrap()
    }

    #[test]
    fn formatting_a_uniformly_dedented_document_reindents_exactly() {
        // Property: for any document indented uniformly and dedented by the
        // peer, any formatter result maps back to that result re-indented.
        let mut random = Lcg(12345);
        let words = ["a", "bb", "c d", "é", "x(y)", "  deep", "\tt"];
        for _ in 0..3000 {
            let eol = ["\n", "\r\n", "\r"][random.below(3)];
            let indent = ["  ", "\t", "    "][random.below(3)];
            let mut lines: Vec<String> = (0..1 + random.below(6))
                .map(|_| {
                    if random.below(4) == 0 {
                        String::new()
                    } else {
                        words[random.below(words.len())].to_string()
                    }
                })
                .collect();
            if lines.iter().all(String::is_empty) {
                continue;
            }
            let trailing = random.below(3) != 0;
            let join = |lines: &[String], trailing: bool| {
                let mut text = lines.join(eol);
                if trailing {
                    text.push_str(eol);
                }
                text
            };
            let prepared = join(&lines, trailing);
            let virtual_text = indent_lines(&prepared, indent);
            let map = dedent_all(&virtual_text, indent);
            assert_eq!(map.prepared_lines.text(), prepared);
            for _ in 0..1 + random.below(3) {
                let len = lines.len();
                match random.below(5) {
                    0 if len > 1 => {
                        lines.remove(random.below(len));
                    }
                    1 => lines.insert(
                        random.below(len + 1),
                        ["z", "", "q r"][random.below(3)].to_string(),
                    ),
                    2 if len > 1 => {
                        let at = random.below(len - 1);
                        let next = lines.remove(at + 1);
                        lines[at].push_str(&next);
                    }
                    3 => lines[random.below(len)].insert(0, 'w'),
                    _ => lines[random.below(len)].clear(),
                }
            }
            let formatted = join(
                &lines,
                if random.below(5) == 0 {
                    !trailing
                } else {
                    trailing
                },
            );
            let whole = TextEdit {
                range: LspRange::new(pos(0, 0), map.prepared_lines.position(prepared.len())),
                new_text: formatted.clone(),
            };
            let edits = map
                .edits_to_virtual(&[whole])
                .unwrap_or_else(|| panic!("refused: {virtual_text:?} → {formatted:?}"));
            assert_eq!(
                apply_to(&virtual_text, &edits),
                indent_lines(&formatted, indent),
                "{virtual_text:?} → {formatted:?}"
            );
        }
    }

    #[test]
    fn removing_a_line_keeps_the_next_lines_indent_once() {
        let virtual_text = "  a\n  b\n";
        let map = dedent_by_two(virtual_text);
        let removed = map.edit_to_virtual(&edit((0, 0), (1, 0), "")).unwrap();
        assert_eq!(apply_to(virtual_text, &[removed]), "  b\n");
        let replaced = map.edit_to_virtual(&edit((0, 0), (1, 0), "x\n")).unwrap();
        assert_eq!(apply_to(virtual_text, &[replaced]), "  x\n  b\n");

        let imports = "  import os\n  import sys\n\n\n\n  x = 1\n";
        let map = dedent_by_two(imports);
        let fixed = map.edit_to_virtual(&edit((0, 0), (1, 0), "")).unwrap();
        assert_eq!(apply_to(imports, &[fixed]), "  import sys\n\n\n\n  x = 1\n");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (6, 0), "import os\nimport sys\n\nx = 1\n")])
            .unwrap();
        assert_eq!(
            apply_to(imports, &formatted),
            "  import os\n  import sys\n\n  x = 1\n"
        );

        let blank = "  a\n\n  b\n";
        let map = dedent_by_two(blank);
        let removed = map.edit_to_virtual(&edit((1, 0), (2, 0), "")).unwrap();
        assert_eq!(apply_to(blank, &[removed]), "  a\n  b\n");
    }

    #[test]
    fn an_emptied_gap_beside_a_removed_indent_survives_line_edits() {
        // Gap then indent: `  a\n` + gap (host fences) + `  b\n`, all
        // emptied or dedented.
        let virtual_text = "  a\n\n\n  b\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..6, "```\n```py\n".to_string()),
                (SegmentKind::Content, 6..10, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [
                {"type": "content", "changes": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}]},
                {"type": "gap", "content": ""},
                {"type": "content", "changes": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}]}
            ]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\nb\n");
        let map = prepared.map.unwrap();
        let removed = map.edit_to_virtual(&edit((0, 0), (1, 0), "")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[removed]), "\n\n  b\n");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "b\n")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &formatted), "\n\n  b\n");

        // Indent then gap: `\n  foo\n  ${x}b\n` with `${x}` emptied.
        let virtual_text = "\n  foo\n      b\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..9, String::new()),
                (SegmentKind::Gap, 9..13, "${x}".to_string()),
                (SegmentKind::Content, 13..15, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [
                {"type": "content", "changes": [
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
                ]},
                {"type": "gap", "content": ""},
                {"type": "content"}
            ]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "\nfoo\nb\n");
        let map = prepared.map.unwrap();
        let replaced = map.edit_to_virtual(&edit((2, 0), (2, 1), "z")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[replaced]), "\n  foo\n      z\n");
    }

    #[test]
    fn the_document_end_before_an_emptied_trailing_gap() {
        let virtual_text = "ab   ".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..5, "${x}".to_string()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": ""}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        assert_eq!(map.to_virtual(pos(0, 2), Bias::Start), pos(0, 2));
        assert_eq!(map.to_virtual(pos(0, 1), Bias::Start), pos(0, 1));
        assert_eq!(map.to_prepared(pos(0, 4), Bias::Start), pos(0, 2));
    }

    #[test]
    fn deleting_a_dedented_line_takes_its_indent() {
        let (virtual_text, prepared) = dedented();
        let map = prepared.map.unwrap();
        let deleted = map.edit_to_virtual(&edit((1, 0), (2, 0), "")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[deleted]), "  if x:\n");
        let cleared = map.edit_to_virtual(&edit((1, 0), (1, 3), "")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[cleared]), "  if x:\n\n");
        let replaced = map.edit_to_virtual(&edit((1, 0), (1, 3), "z")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[replaced]), "  if x:\n  z\n");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "if x:\n")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &formatted), "  if x:\n");
    }

    #[test]
    fn crlf_dedent_maps_and_reindents_with_crlf() {
        let virtual_text = "  a\r\n  b\r\n".to_string();
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
        assert_eq!(prepared.text, "a\r\nb\r\n");
        let map = prepared.map.unwrap();
        assert_eq!(map.to_virtual(pos(1, 1), Bias::Start), pos(1, 3));
        let inserted = map.edit_to_virtual(&edit((1, 1), (1, 1), "\r\nc")).unwrap();
        assert_eq!(
            apply_to(&virtual_text, &[inserted]),
            "  a\r\n  b\r\n  c\r\n"
        );
    }

    #[test]
    fn lone_cr_lines_are_lines() {
        let virtual_text = "  a\r  b".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "  a\rb");
        assert_eq!(
            prepared.map.unwrap().to_virtual(pos(1, 0), Bias::Start),
            pos(1, 2)
        );
    }

    #[test]
    fn columns_after_a_placeholder_count_utf16_units() {
        // `f(${a}, 𝄞é)`: the placeholder shrinks the gap, and the columns
        // after it on that line count the surrogate pair as two units.
        let virtual_text = "f(    , 𝄞é)".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..6, "${a}".to_string()),
                (SegmentKind::Content, 6..virtual_text.len(), String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": "1"}, {"type": "content"}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "f(1, 𝄞é)");
        let map = prepared.map.unwrap();
        // `é` is P column 7 (f ( 1 , space 𝄞=2) and V column 10.
        assert_eq!(map.to_virtual(pos(0, 7), Bias::Start), pos(0, 10));
        assert_eq!(map.to_prepared(pos(0, 10), Bias::Start), pos(0, 7));
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
