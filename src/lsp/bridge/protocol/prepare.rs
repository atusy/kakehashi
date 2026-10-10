//! `kakehashi/virtualDocument/prepare`: let a peer rewrite a virtual document
//! before downstream servers see it.
//!
//! kakehashi splits the virtual document (V) into ordered segments — injected
//! `content` and host-owned `gap`s — and asks the peer how to present them.
//! The answer may delete leading whitespace from content lines (dedent) and
//! whole blank lines at the document's edges, and replace any gap with
//! arbitrary text (placeholders). Applying it yields the prepared document
//! (P) that downstream servers receive, plus a [`PreparedMap`] translating
//! coordinates between P and V. V keeps its existing host translation
//! ([`super::RegionOffset`]), so host ↔ P is the composition of the two.
//!
//! Content may only lose leading whitespace and edge blank lines so that
//! every edit a downstream server makes in P can be mapped back
//! unambiguously; gaps are opaque, so an edit touching one is refused rather
//! than guessed.

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
    /// answer changed nothing; `None` when no candidate advertises the
    /// request, and the virtual text is sent as is.
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
    let last_index = layout.segments.len().saturating_sub(1);
    for (index, (segment, answer)) in layout.segments.iter().zip(result.segments).enumerate() {
        let virtual_segment = &virtual_text[segment.virtual_range.clone()];
        match (segment.kind, answer) {
            (SegmentKind::Content, WireSegmentResult::Content { changes }) => {
                let deletions = content_deletions(
                    virtual_text,
                    segment.virtual_range.start,
                    virtual_segment,
                    changes.unwrap_or_default(),
                    DocumentEdges {
                        start: index == 0,
                        end: index == last_index,
                    },
                )
                .map_err(|reason| PrepareError::InvalidChange { index, reason })?;
                let mut cursor = segment.virtual_range.start;
                let mut deleted: Vec<&str> = Vec::new();
                let deletes_closing_lines = deletions
                    .iter()
                    .any(|(_, kind)| *kind == RunKind::TrailingLines);
                for (deletion, kind) in deletions {
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
                        kind,
                        deletion.clone(),
                        virtual_text,
                        "",
                    );
                    // The closing indent deleted with the document's closing
                    // lines (the indent before a closing `''`, which must go
                    // with them) indents no content line, so it says nothing
                    // of the indent new lines regain. Without closing lines
                    // deleted, an indentation-only last line may be content
                    // of its own, and counts as any line does.
                    let closing_indent = deletes_closing_lines
                        && index == last_index
                        && deletion.end == segment.virtual_range.end;
                    if kind == RunKind::Deleted && !closing_indent {
                        deleted.push(&virtual_text[deletion.clone()]);
                    }
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

/// Which of the document's edges a content segment holds.
#[derive(Clone, Copy)]
struct DocumentEdges {
    start: bool,
    end: bool,
}

/// Validate a content segment's changes and return the byte ranges (in V)
/// they delete, sorted, each with the kind of run it becomes.
fn content_deletions(
    virtual_text: &str,
    segment_start: usize,
    segment_text: &str,
    changes: Vec<TextEdit>,
    edges: DocumentEdges,
) -> Result<Vec<(Range<usize>, RunKind)>, &'static str> {
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
        let absolute = segment_start + start;
        let at_line_start = is_line_start(virtual_text, absolute);
        if !deleted
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            return Err("content changes may only delete whitespace");
        }
        let kind = if !deleted.contains(['\n', '\r']) {
            if !at_line_start {
                return Err("content changes may only delete leading whitespace");
            }
            RunKind::Deleted
        } else if at_line_start
            && is_line_start(virtual_text, segment_start + end)
            && !splits_crlf(virtual_text, absolute)
            && !splits_crlf(virtual_text, segment_start + end)
        {
            // Classified as leading or trailing below.
            RunKind::LeadingLines
        } else {
            // Not whole lines in V: a line break deleted mid-line, such as
            // the one opening a segment that starts after a gap on the same
            // line, or a deletion ending mid-line or inside a CRLF.
            return Err("content changes may only delete whole blank lines");
        };
        deletions.push((absolute..segment_start + end, kind));
    }
    deletions.sort_by_key(|(range, _)| range.start);
    if deletions
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start)
    {
        return Err("content changes overlap");
    }
    // Whole lines go only from the document's edges, where the host's
    // string syntax drops them (the line break after a Nix `''`, the blank
    // lines a YAML `|` block clips): those running from its start, then
    // those running to its end — before a last line without a line break,
    // which only indentation fills (the indentation before a closing `''`).
    // Beside a gap they are inside the document, where a formatter may add
    // a blank line back at the join and the host would hold it twice.
    let mut leading_end = segment_start;
    for (range, kind) in &deletions {
        if edges.start && *kind == RunKind::LeadingLines && range.start == leading_end {
            leading_end = range.end;
        }
    }
    let segment_end = segment_start + segment_text.len();
    let mut trailing_start = segment_text
        .rfind(['\n', '\r'])
        .map(|last| segment_start + last + 1)
        .filter(|&last_line| {
            virtual_text[last_line..segment_end]
                .trim_matches([' ', '\t'])
                .is_empty()
        })
        .unwrap_or(segment_end);
    let closing_line = trailing_start;
    for (range, kind) in deletions.iter_mut().rev() {
        if *kind != RunKind::LeadingLines || range.end <= leading_end {
            continue;
        }
        if !edges.end || range.end != trailing_start {
            return Err("content changes may only delete blank lines at the document's edges");
        }
        *kind = RunKind::TrailingLines;
        trailing_start = range.start;
    }
    // The closing line's indentation is no part of the value either, and
    // with it gone P ends where the deleted lines were: no kept text starts
    // there in P that positions or edits at P's end could belong to.
    let has_trailing = deletions
        .iter()
        .any(|(_, kind)| *kind == RunKind::TrailingLines);
    if has_trailing
        && closing_line < segment_end
        && !deletions
            .iter()
            .any(|(range, kind)| *kind == RunKind::Deleted && *range == (closing_line..segment_end))
    {
        return Err(
            "content changes may only delete closing blank lines with all of the closing line's indentation",
        );
    }
    // A segment with no content left beside its deleted lines puts them
    // beside the gap that follows (or precedes) it — when that gap starts
    // (or ends) a line: within a line, the blank text left is that line's
    // indent or line break (`''\n  ${x}/bin/foo''`).
    let is_blank = |text: &str| text.trim_matches([' ', '\t', '\n', '\r']).is_empty();
    let beside_gap = (!edges.end
        && leading_end > segment_start
        && is_blank(&virtual_text[leading_end..segment_end])
        && is_line_start(virtual_text, segment_end))
        || (!edges.start
            && has_trailing
            && is_blank(&virtual_text[segment_start..trailing_start])
            && is_line_start(virtual_text, segment_start));
    if beside_gap {
        return Err("content changes may only delete blank lines at the document's edges");
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
        RunKind::Deleted | RunKind::LeadingLines | RunKind::TrailingLines => "",
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
    /// Whole blank lines opening the document (its first content segment),
    /// absent from P.
    LeadingLines,
    /// Whole blank lines ending the document (its last content segment),
    /// absent from P along with any closing indent after them. P's content
    /// ends before them, so P's end maps before them too.
    TrailingLines,
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

    /// Whether two P positions name the same offset once clamped onto P (a
    /// column past a line's end means its end).
    pub(crate) fn same_prepared_offset(&self, a: Position, b: Position) -> bool {
        self.prepared_lines.offset_clamped(a) == self.prepared_lines.offset_clamped(b)
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

    /// The P offsets of the gaps the peer emptied, in order: where host text
    /// sits that P does not show at all, such as the Nix between two joined
    /// strings.
    pub(crate) fn emptied_gap_offsets(&self) -> impl Iterator<Item = usize> + '_ {
        self.runs
            .iter()
            .filter(|run| run.kind == RunKind::Gap && run.prepared.is_empty())
            .map(|gap| gap.prepared.start)
    }

    /// The virtual text this map was built for.
    pub(crate) fn virtual_text(&self) -> &str {
        self.virtual_lines.text()
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
        let plain_start = match empty_run(&self.runs, old.start, RunKind::Gap) {
            // An insertion where the peer emptied a gap goes before the gap's
            // host text, like a change ending there and an insertion at the
            // document end: the start bias alone would carry it past.
            Some(gap) if old.is_empty() => gap.virtual_.start,
            _ => map_offset(&self.runs, old.start, Bias::Start, Side::Prepared),
        };
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
        // Host text a gap stands for that starts a line (a closing fence and
        // what follows it, between combined code blocks) must keep starting
        // one: a change ending right before it either ends with a line break
        // or deletes whole lines, as at the region's own end. So must the
        // document's deleted closing lines, kept in V after P's end.
        if (self.gap_starts_line_at(virtual_end) || self.closing_lines_start_at(virtual_end))
            && !(new_text.ends_with(['\n', '\r'])
                || new_text.is_empty() && is_line_start(virtual_text, virtual_start))
        {
            return None;
        }
        // A CR and an LF meeting across the change's edges would fuse two
        // line breaks into one CRLF. When the one beside the change is a
        // deleted edge line's, which P does not show, that drops a line P
        // keeps apart from it; one P has too is the edit's own doing.
        let prepared = self.prepared_lines.text();
        let cr_before = virtual_text[..virtual_start].ends_with('\r');
        let lf_after = virtual_text[virtual_end..].starts_with('\n');
        let cr_hidden = cr_before && !prepared[..old.start].ends_with('\r');
        let lf_hidden = lf_after && !prepared[old.end..].starts_with('\n');
        let fuses = if new_text.is_empty() {
            cr_before && lf_after && (cr_hidden || lf_hidden)
        } else {
            cr_hidden && new_text.starts_with('\n') || lf_hidden && new_text.ends_with('\r')
        };
        if fuses {
            return None;
        }
        Some(TextEdit {
            range: LspRange::new(
                self.virtual_lines.position(virtual_start),
                self.virtual_lines.position(virtual_end),
            ),
            new_text,
        })
    }

    /// The V positions where a gap starts a line.
    pub(crate) fn line_start_gap_positions(&self) -> impl Iterator<Item = Position> + '_ {
        let virtual_text = self.virtual_lines.text();
        self.runs
            .iter()
            .filter(move |run| {
                run.kind == RunKind::Gap && is_line_start(virtual_text, run.virtual_.start)
            })
            .map(|gap| self.virtual_lines.position(gap.virtual_.start))
    }

    /// Whether the document's deleted closing lines start at V `offset`.
    fn closing_lines_start_at(&self, offset: usize) -> bool {
        let first = self.runs.partition_point(|run| run.virtual_.start < offset);
        self.runs[first..]
            .iter()
            .take_while(|run| run.virtual_.start == offset)
            .any(|run| run.kind == RunKind::TrailingLines)
    }

    /// Whether a gap starts at V offset `offset`, at a line start.
    fn gap_starts_line_at(&self, offset: usize) -> bool {
        if !is_line_start(self.virtual_lines.text(), offset) {
            return false;
        }
        let first = self.runs.partition_point(|run| run.virtual_.start < offset);
        self.runs[first..]
            .iter()
            .take_while(|run| run.virtual_.start == offset)
            .any(|run| run.kind == RunKind::Gap)
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

    /// Whether a V position sits within indentation or blank lines the peer
    /// removed: P has no position there, and one at the line's content (or
    /// the content's end) stands in for it, starts included.
    pub(crate) fn virtual_position_in_removed_indent(&self, position: Position) -> bool {
        let offset = self.virtual_lines.offset_clamped(position);
        // From where the document's deleted closing lines start, V holds only
        // what P dropped (those lines and the closing indent after them), up
        // to its very end. P's end maps to that start, but what a client
        // inserts at a caret there (a completion without a text edit) joins
        // the first of those lines, which P does not have.
        // Those are the runs empty in P at its end; the closing lines may
        // be several runs, deleted one by one.
        if let Some(lines) = self
            .runs
            .iter()
            .rev()
            .take_while(|run| run.prepared.is_empty())
            .filter(|run| run.kind == RunKind::TrailingLines)
            .last()
            && offset >= lines.virtual_.start
        {
            return true;
        }
        let first = self.runs.partition_point(|run| run.virtual_.end <= offset);
        self.runs[first..]
            .iter()
            .take_while(|run| run.virtual_.start <= offset)
            .any(|run| {
                matches!(run.kind, RunKind::Deleted | RunKind::LeadingLines)
                    && run.virtual_.start <= offset
                    && offset < run.virtual_.end
            })
    }

    /// Whether the V range reaches inside a gap's host text (an empty range:
    /// whether it sits strictly inside one).
    pub(crate) fn virtual_range_in_gap(&self, range: LspRange) -> bool {
        self.virtual_range_touches_gap(
            self.virtual_lines.offset_clamped(range.start),
            self.virtual_lines.offset_clamped(range.end),
        )
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
/// opaque run (a deleted indent, deleted edge lines or a gap) it lands on
/// the run's start, or — strictly inside, with [`Bias::End`] — its end. On a
/// boundary the run that *starts* there wins, so a P line start maps after
/// the indent V deleted there: the host keeps its indentation and the edit
/// lands on the content. Likewise P's start maps after the lines deleted
/// from the document's start, and P's end, whatever the bias, before those
/// deleted from its end.
fn map_offset(runs: &[Run], offset: usize, bias: Bias, from: Side) -> usize {
    // P's end, where the document's deleted closing lines were: the
    // content ends there in V too, before them.
    if matches!(from, Side::Prepared)
        && let Some(lines) = empty_run(runs, offset, RunKind::TrailingLines)
    {
        return lines.virtual_.start;
    }
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
        let mut passed = None;
        for run in runs[first..]
            .iter()
            .take_while(|run| from.ranges(run).0.start == offset)
            .filter(|run| from.ranges(run).0.is_empty())
        {
            let target = from.ranges(run).1;
            // Lines deleted from the document's start and a removed indent
            // at the very end are passed, as at any line start; a gap
            // emptied there stays after the position.
            match run.kind {
                RunKind::LeadingLines => passed = Some(target.end),
                RunKind::Deleted => return target.end,
                _ => return target.start,
            }
        }
        if let Some(end) = passed {
            return end;
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

/// Whether `offset` sits between the CR and LF of a CRLF: past its line's
/// content, where LSP clamps a position, yet a distinct byte offset.
fn splits_crlf(text: &str, offset: usize) -> bool {
    offset > 0
        && text.as_bytes()[offset - 1] == b'\r'
        && text.as_bytes().get(offset) == Some(&b'\n')
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

/// `text` with LSP `edits` applied, a column past a line's end meaning the
/// line's end; `None` when an edit is reversed or edits overlap.
pub(crate) fn apply_text_edits_clamped(text: &str, edits: &[TextEdit]) -> Option<String> {
    apply_edits(&LineMap::new(text.to_string()), edits)
}

/// LSP edits turning `old` into `new`, one per character-level diff hunk —
/// minimal where a formatter's own answer may be one whole-document edit.
pub(crate) fn text_edits_between(old: &str, new: &str) -> Vec<TextEdit> {
    let lines = LineMap::new(old.to_string());
    diff_hunks(old, new)
        .into_iter()
        .map(|hunk| TextEdit {
            range: LspRange::new(lines.position(hunk.old.start), lines.position(hunk.old.end)),
            new_text: new[hunk.new].to_string(),
        })
        .collect()
}

/// `formatted` with the boundary layout (see [`restore_boundary_layout`])
/// of every piece of `prepared` between `joints` restored, a joint being an
/// emptied gap within blank text that spans a line break: the closing
/// indentation of one joined string and the line break opening the next are
/// host layout, as a document's edges are. A joint whose surrounding blank
/// text has no counterpart in `formatted` (the text around it changed) does
/// not split, and an edit crossing it is left for the gap checks to refuse.
///
/// [`restore_boundary_layout`]: crate::text::layout::restore_boundary_layout
pub(crate) fn restore_joined_layout(
    prepared: &str,
    formatted: &str,
    joints: impl IntoIterator<Item = usize>,
) -> String {
    use crate::text::layout::restore_boundary_layout;
    let is_blank = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r');
    let mut joints = joints.into_iter().peekable();
    if joints.peek().is_none() {
        return restore_boundary_layout(prepared, formatted);
    }
    let hunks = diff_hunks(prepared, formatted);
    // Where an unchanged character of `prepared` is in `formatted`.
    let unchanged = |offset: usize| -> Option<usize> {
        let mut shifted = offset;
        for hunk in &hunks {
            if hunk.old.end <= offset {
                shifted = shifted + hunk.new.len() - hunk.old.len();
            } else if hunk.old.start <= offset {
                return None;
            } else {
                break;
            }
        }
        Some(shifted)
    };
    let mut splits: Vec<(usize, usize)> = Vec::new();
    for joint in joints {
        let start = prepared[..joint].trim_end_matches(is_blank).len();
        let end = joint
            + (prepared[joint..].len() - prepared[joint..].trim_start_matches(is_blank).len());
        if start == 0 || end == prepared.len() || !prepared[start..end].contains(['\n', '\r']) {
            continue;
        }
        let Some(before) = prepared[..start].chars().next_back() else {
            continue;
        };
        let (Some(blank_start), Some(blank_end)) = (
            unchanged(start - before.len_utf8()).map(|offset| offset + before.len_utf8()),
            unchanged(end),
        ) else {
            continue;
        };
        if blank_start > blank_end || !formatted[blank_start..blank_end].chars().all(is_blank) {
            continue;
        }
        // The next string's line break, with the indentation a formatter
        // gave its first line, opens it; any blank text before that ends the
        // previous string.
        let blank = &formatted[blank_start..blank_end];
        let split = blank_start
            + blank.rfind(['\n', '\r']).map_or(0, |last| {
                if last > 0 && blank[last..].starts_with('\n') && blank[..last].ends_with('\r') {
                    last - 1
                } else {
                    last
                }
            });
        if splits
            .last()
            .is_some_and(|&(previous, previous_split)| joint <= previous || split < previous_split)
        {
            continue;
        }
        splits.push((joint, split));
    }
    let mut restored = String::with_capacity(formatted.len());
    let (mut piece, mut formatted_piece) = (0, 0);
    for (joint, split) in splits
        .into_iter()
        .chain([(prepared.len(), formatted.len())])
    {
        restored.push_str(&restore_boundary_layout(
            &prepared[piece..joint],
            &formatted[formatted_piece..split],
        ));
        (piece, formatted_piece) = (joint, split);
    }
    restored
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

    /// Two combined fenced blocks: the closing fence, the prose and the
    /// opening fence between them are a gap starting a line.
    fn fenced() -> (String, VirtualLayout) {
        let virtual_text = "foo()\n\n\n\nbar()\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..6, String::new()),
                (SegmentKind::Gap, 6..9, "```\ntext\n```lua\n".to_string()),
                (SegmentKind::Content, 9..15, String::new()),
            ],
        );
        (virtual_text, layout)
    }

    #[test]
    fn each_joined_string_keeps_its_boundary_layout() {
        // `{"a":1,` + `''; b = ''` + `"b":2}`: the closing indentation of
        // the first string and the line break opening the second are layout.
        let prepared = "\n{\n  \"a\":1,\n  \n    \"b\":2\n}\n  ";
        let joint = prepared.find("\n    \"b\"").unwrap();
        let formatted = "{\n    \"a\": 1,\n    \"b\": 2\n}";
        assert_eq!(
            restore_joined_layout(prepared, formatted, [joint]),
            "\n{\n    \"a\": 1,\n  \n    \"b\": 2\n}\n  "
        );
        // Without the joint, only the document's edges are kept.
        assert_eq!(
            restore_joined_layout(prepared, formatted, []),
            "\n{\n    \"a\": 1,\n    \"b\": 2\n}\n  "
        );
    }

    #[test]
    fn a_joint_within_a_line_is_left_to_the_formatter() {
        // An emptied interpolation between tokens on one line is no layout.
        assert_eq!(restore_joined_layout("\na b\n", "a\nb", [2]), "\na\nb\n");
    }

    #[test]
    fn a_joint_whose_surroundings_changed_does_not_split() {
        // The token before the joint was rewritten: there is no telling which
        // blank text of the formatted text stands where the joint was.
        let prepared = "\nx,\n  \ny\n";
        let joint = prepared.find("\ny").unwrap();
        assert_eq!(
            restore_joined_layout(prepared, "\nz;\ny\n", [joint]),
            "\nz;\ny\n"
        );
    }

    #[test]
    fn joined_strings_keep_crlf_layout() {
        let prepared = "\r\nx,\r\n  \r\n  y\r\n  ";
        let joint = prepared.find("\r\n  y").unwrap();
        assert_eq!(
            restore_joined_layout(prepared, "x,\r\ny", [joint]),
            "\r\nx,\r\n  \r\ny\r\n  "
        );
    }

    #[test]
    fn edits_between_two_texts_turn_one_into_the_other() {
        for (old, new) in [
            ("\n  {\n  }\n    ", "\n{\n}\n    "),
            ("a\r\nb\r\n", "a\r\nB\r\nc\r\n"),
            ("same", "same"),
            ("", "x"),
        ] {
            let edits = text_edits_between(old, new);
            assert_eq!(apply_to(old, &edits), new, "{old:?} -> {new:?}: {edits:?}");
        }
        assert!(text_edits_between("same", "same").is_empty());
    }

    #[test]
    fn a_change_keeps_the_line_break_before_a_gap_starting_a_line() {
        let (virtual_text, layout) = fenced();
        for gap in [
            json!({"type": "gap"}),
            json!({"type": "gap", "content": ""}),
        ] {
            let prepared = apply_prepare_result(
                &virtual_text,
                &layout,
                result(json!({"segments": [{"type": "content"}, gap, {"type": "content"}]})),
            )
            .unwrap();
            let map = prepared.map.unwrap();
            // Joining `foo()` onto the closing fence.
            assert_eq!(map.edit_to_virtual(&edit((0, 5), (1, 0), "")), None);
            assert_eq!(map.edits_to_virtual(&[edit((0, 5), (1, 0), "")]), None);
            assert_eq!(map.edit_to_virtual(&edit((0, 5), (1, 0), ";")), None);
            // Whole lines, or text ending with a line break, keep it.
            assert!(map.edit_to_virtual(&edit((0, 0), (1, 0), "")).is_some());
            assert!(
                map.edit_to_virtual(&edit((0, 0), (1, 0), "baz()\n"))
                    .is_some()
            );
            assert!(map.edit_to_virtual(&edit((0, 5), (0, 5), ";")).is_some());
        }
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

    /// A Nix `''` string's content: the line break right after `''`, then
    /// indented lines. The peer deletes the leading blank line and dedents.
    fn leading_blank_dedented() -> (String, PreparedDocument) {
        let virtual_text = "\n  a\n  b\n".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        (virtual_text, prepared)
    }

    #[test]
    fn leading_blank_lines_are_deleted() {
        let (_, prepared) = leading_blank_dedented();
        assert_eq!(prepared.text, "a\nb\n");
        let map = prepared.map.unwrap();
        // P's first line is V's second, after its removed indent.
        assert_eq!(map.to_virtual(pos(0, 0), Bias::Start), pos(1, 2));
        assert_eq!(map.to_virtual(pos(1, 1), Bias::Start), pos(2, 3));
        // A V position on the deleted line lands on P's start.
        assert_eq!(map.to_prepared(pos(0, 0), Bias::Start), pos(0, 0));
        assert_eq!(map.to_prepared(pos(2, 3), Bias::Start), pos(1, 1));
    }

    /// A YAML `|` block's content: the blank lines ending it are not part
    /// of the value (clip). The peer deletes them and dedents.
    fn trailing_blank_dedented() -> (String, PreparedDocument) {
        let virtual_text = "  a\n  \n\n".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        (virtual_text, prepared)
    }

    #[test]
    fn trailing_blank_lines_are_deleted() {
        let (_, prepared) = trailing_blank_dedented();
        assert_eq!(prepared.text, "a\n");
        let map = prepared.map.unwrap();
        assert_eq!(map.to_virtual(pos(0, 1), Bias::Start), pos(0, 3));
        // P's end is the content's end, before the deleted lines.
        assert_eq!(map.to_virtual(pos(1, 0), Bias::Start), pos(1, 0));
        assert_eq!(map.to_virtual(pos(1, 0), Bias::End), pos(1, 0));
        assert_eq!(map.to_prepared(pos(2, 0), Bias::Start), pos(1, 0));
        // V → P keeps to the content: P's end rule is for P offsets only.
        assert_eq!(map.to_prepared(pos(0, 2), Bias::Start), pos(0, 0));
        assert_eq!(map.to_prepared(pos(0, 3), Bias::Start), pos(0, 1));
    }

    #[test]
    fn edits_keep_deleted_trailing_blank_lines() {
        let (virtual_text, prepared) = trailing_blank_dedented();
        let map = prepared.map.unwrap();
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (1, 0), "if a:\n  b\n")])
            .unwrap();
        assert_eq!(
            apply_to(&virtual_text, &formatted),
            "  if a:\n    b\n  \n\n"
        );
        // A change reaching P's end stops before the deleted lines.
        let changed = map.edit_to_virtual(&edit((0, 0), (1, 0), "b\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[changed]), "  b\n  \n\n");
        // An append at P's end goes before them, indented.
        let appended = map.edit_to_virtual(&edit((1, 0), (1, 0), "c\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[appended]), "  a\n  c\n  \n\n");
    }

    #[test]
    fn trailing_blank_lines_go_before_a_closing_indent() {
        // A Nix `''` string ending in a blank line, then the indentation
        // before the closing `''`.
        let virtual_text = "  a\n\n  ".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\n");
        let map = prepared.map.unwrap();
        let changed = map.edit_to_virtual(&edit((0, 0), (1, 0), "b\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[changed]), "  b\n\n  ");
    }

    #[test]
    fn a_tab_indented_closing_line_goes_with_the_trailing_lines() {
        let virtual_text = "  a\n\n\t\t".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .map(|prepared| prepared.text);
        assert_eq!(prepared, Ok("  a\n".to_string()));
    }

    #[test]
    fn a_line_appended_above_a_deleted_whitespace_line_takes_no_tail() {
        // The deleted closing line holds whitespace, and the closing indent
        // after it is deleted too: a line appended at P's end goes above
        // that line, which keeps its own text.
        let virtual_text = "  a\n   \n  ".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\n");
        let map = prepared.map.unwrap();
        let appended = map.edit_to_virtual(&edit((1, 0), (1, 0), "b\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[appended]), "  a\n  b\n   \n  ");
        let appended = map
            .edits_to_virtual(&[edit((1, 0), (1, 0), "b\n")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &appended), "  a\n  b\n   \n  ");
    }

    #[test]
    fn a_deleted_closing_indent_is_no_dedent() {
        // The indent before a closing `''` is usually shallower than the
        // content's, and deleting it removes no indent from a content line:
        // new lines still regain the content's.
        let virtual_text = "\n    a\n\n  ".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\n");
        let map = prepared.map.unwrap();
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (1, 0), "a\nb\n")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &formatted), "\n    a\n    b\n\n  ");
        // Without a dedent, a new line gains none either.
        let virtual_text = "  a\n\n\t\t".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        let appended = map.edit_to_virtual(&edit((1, 0), (1, 0), "b\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[appended]), "  a\nb\n\n\t\t");
    }

    #[test]
    fn a_whitespace_only_document_keeps_its_dedent() {
        // With no content line, the only line's indent is all a new line
        // can regain.
        let virtual_text = "  ".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (0, 0), "if True:\n  pass\nprint(1)")])
            .unwrap();
        assert_eq!(
            apply_to(&virtual_text, &formatted),
            "  if True:\n    pass\n  print(1)"
        );
    }

    #[test]
    fn a_change_at_p_end_must_end_its_line() {
        // The deleted closing lines start a line in V, as P's end does: a
        // change ending there without a line break would join the host's
        // first blank line (its whitespace onto a heredoc terminator, say).
        let virtual_text = "  cat <<'EOF'\n  \n\n".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "cat <<'EOF'\n");
        let map = prepared.map.unwrap();
        for refused in [
            edit((1, 0), (1, 0), "hello\nEOF"),
            edit((0, 0), (1, 0), "x"),
        ] {
            assert_eq!(map.edit_to_virtual(&refused), None, "{refused:?}");
            assert_eq!(map.edits_to_virtual(&[refused]), None);
        }
        // Ending its line, or deleting whole lines, is fine.
        let appended = map.edit_to_virtual(&edit((1, 0), (1, 0), "EOF\n")).unwrap();
        assert_eq!(
            apply_to(&virtual_text, &[appended]),
            "  cat <<'EOF'\n  EOF\n  \n\n"
        );
        let deleted = map.edit_to_virtual(&edit((0, 0), (1, 0), "")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[deleted]), "  \n\n");
    }

    #[test]
    fn a_blank_last_segment_after_content_takes_no_closing_dedent() {
        // `''a${x}\n\n  ''`: the last segment is blank, but the document
        // has content, so its closing indent is no dedent.
        let virtual_text = "a    \n\n  ".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..1, String::new()),
                (SegmentKind::Gap, 1..5, "${x}".to_string()),
                (SegmentKind::Content, 5..9, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [
                {"type": "content"},
                {"type": "gap", "content": "x"},
                {"type": "content", "changes": [
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
                ]}
            ]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "ax\n");
        let map = prepared.map.unwrap();
        let appended = map.edit_to_virtual(&edit((1, 0), (1, 0), "b\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[appended]), "a    \nb\n\n  ");
    }

    #[test]
    fn a_change_fusing_a_cr_and_lf_across_deleted_lines_is_refused() {
        // A CR the change ends with, before a deleted line's LF, or an LF
        // it starts with, after a deleted line's CR, would make one CRLF of
        // two line breaks and drop a host line.
        let prepare = |virtual_text: &str, changes: serde_json::Value| {
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                result(json!({"segments": [{"type": "content", "changes": changes}]})),
            )
            .unwrap()
            .map
            .unwrap()
        };
        let map = prepare(
            "a\n\n",
            json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]),
        );
        assert_eq!(map.edit_to_virtual(&edit((1, 0), (1, 0), "b\r")), None);
        assert_eq!(map.edits_to_virtual(&[edit((1, 0), (1, 0), "b\r")]), None);
        let map = prepare(
            "\ra",
            json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]),
        );
        assert_eq!(map.edit_to_virtual(&edit((0, 0), (0, 0), "\nb")), None);
        assert_eq!(map.edits_to_virtual(&[edit((0, 0), (0, 0), "\nb")]), None);
        // Deleting what keeps a hidden CR from a visible LF fuses them too.
        let map = prepare(
            "\ra\n",
            json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]),
        );
        assert_eq!(map.edit_to_virtual(&edit((0, 0), (0, 1), "")), None);
        assert_eq!(map.edits_to_virtual(&[edit((0, 0), (0, 1), "")]), None);
        // A CRLF the edit makes from line breaks P has is the edit's own.
        let map = prepare("a\n", json!([]));
        let crlf = map.edit_to_virtual(&edit((0, 1), (0, 1), "\r")).unwrap();
        assert_eq!(apply_to("a\n", &[crlf]), "a\r\n");
    }

    #[test]
    fn a_blank_last_segment_of_one_line_keeps_its_dedent() {
        // A last segment holding only an indented line of its own (no line
        // break before it in the segment) is no closing indent after
        // content lines: its indent is what new lines there regain.
        let virtual_text = "a\n\n\n  ".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..4, "```\n```py\n".to_string()),
                (SegmentKind::Content, 4..6, String::new()),
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
        let map = prepared.map.unwrap();
        let edits = map
            .edits_to_virtual(&[edit((3, 0), (3, 0), "if True:\n  pass\nprint(1)")])
            .unwrap();
        assert_eq!(
            apply_to(&virtual_text, &edits),
            "a\n\n\n  if True:\n    pass\n  print(1)"
        );
        // Nor is one after a blank line in it, when no closing lines go.
        let virtual_text = "a\n\n\n\n  ".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..4, "```\n```py\n".to_string()),
                (SegmentKind::Content, 4..7, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [
                {"type": "content"},
                {"type": "gap"},
                {"type": "content", "changes": [
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
                ]}
            ]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        let edits = map
            .edits_to_virtual(&[edit((4, 0), (4, 0), "if True:\n  pass\nprint(1)")])
            .unwrap();
        assert_eq!(
            apply_to(&virtual_text, &edits),
            "a\n\n\n\n  if True:\n    pass\n  print(1)"
        );
    }

    #[test]
    fn trailing_lines_go_only_with_the_whole_closing_indent() {
        // The indentation before a closing `''` is no part of the string's
        // value either: a peer deleting the closing blank lines deletes it
        // too, so P ends where they were.
        let reason = "content changes may only delete closing blank lines with all of the closing line's indentation";
        for (virtual_text, changes) in [
            // The closing indent kept.
            (
                "  a\n\n\t\t",
                json!([
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}
                ]),
            ),
            // Only part of it deleted.
            (
                "    a\n\n    ",
                json!([
                    {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
                ]),
            ),
        ] {
            assert_eq!(
                apply_prepare_result(
                    virtual_text,
                    &VirtualLayout::single(virtual_text),
                    result(json!({"segments": [{"type": "content", "changes": changes}]})),
                ),
                Err(PrepareError::InvalidChange { index: 0, reason }),
                "{virtual_text:?}"
            );
        }
    }

    #[test]
    fn a_caret_on_the_first_deleted_closing_line_is_removed() {
        let (_, prepared) = trailing_blank_dedented();
        let map = prepared.map.unwrap();
        // V (1, 0) is where the deleted lines start, and P's end maps
        // there, but what the client inserts at a caret there joins the
        // first of those lines, which P does not have.
        assert!(!map.virtual_position_in_removed_indent(pos(0, 3)));
        assert!(map.virtual_position_in_removed_indent(pos(1, 0)));
        assert!(map.virtual_position_in_removed_indent(pos(1, 1)));
        assert!(map.virtual_position_in_removed_indent(pos(2, 0)));
    }

    #[test]
    fn a_caret_at_the_end_of_a_deleted_closing_indent_is_removed() {
        // `''\n  a\n\n  ''`'s value ends after `a`: a caret right before the
        // closing `''` stands for P's end, which maps above the blank line.
        let virtual_text = "  a\n\n  ".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        assert!(!map.virtual_position_in_removed_indent(pos(0, 3)));
        assert!(map.virtual_position_in_removed_indent(pos(1, 0)));
        assert!(map.virtual_position_in_removed_indent(pos(2, 0)));
        assert!(map.virtual_position_in_removed_indent(pos(2, 2)));
    }

    #[test]
    fn closing_lines_deleted_one_by_one_are_removed_from_the_first() {
        let virtual_text = "a\n\n\n".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        let map = prepared.map.unwrap();
        assert!(!map.virtual_position_in_removed_indent(pos(0, 1)));
        assert!(map.virtual_position_in_removed_indent(pos(1, 0)));
        assert!(map.virtual_position_in_removed_indent(pos(2, 0)));
        assert!(map.virtual_position_in_removed_indent(pos(3, 0)));
    }

    #[test]
    fn a_position_on_a_deleted_line_is_in_removed_indent() {
        let (_, prepared) = leading_blank_dedented();
        let map = prepared.map.unwrap();
        // P has no position on the deleted line either: a caret there
        // stands in for one at P's first content.
        assert!(map.virtual_position_in_removed_indent(pos(0, 0)));
        assert!(map.virtual_position_in_removed_indent(pos(1, 1)));
        assert!(!map.virtual_position_in_removed_indent(pos(1, 2)));
    }

    #[test]
    fn edits_keep_a_deleted_leading_blank_line() {
        let (virtual_text, prepared) = leading_blank_dedented();
        let map = prepared.map.unwrap();
        // A formatter's whole-document answer re-indents under the host's
        // line break.
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (2, 0), "if a:\n  b\n")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &formatted), "\n  if a:\n    b\n");
        // Replacing P's whole first line restores that line's indent only.
        let replaced = map
            .edits_to_virtual(&[edit((0, 0), (1, 0), "c\n")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &replaced), "\n  c\n  b\n");
        // A line inserted before P's first goes after the deleted line.
        let inserted = map.edit_to_virtual(&edit((0, 0), (0, 0), "z\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[inserted]), "\n  z\n  a\n  b\n");
    }

    #[test]
    fn only_blank_lines_at_the_document_edges_may_be_deleted() {
        let virtual_text = "\n\na\n\nb\n\n\n".to_string();
        let prepare = |virtual_text: &str, changes: serde_json::Value| {
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                result(json!({"segments": [{"type": "content", "changes": changes}]})),
            )
            .map(|prepared| prepared.text)
        };
        let prepared_text = |changes| prepare(&virtual_text, changes);
        // Both leading lines, in one edit or one each, go.
        let both = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]);
        assert_eq!(prepared_text(both), Ok("a\n\nb\n\n\n".to_string()));
        let each = json!([
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}
        ]);
        assert_eq!(prepared_text(each), Ok("a\n\nb\n\n\n".to_string()));
        // So do both trailing lines, in one edit or one each.
        let trailing = json!([{"range": {"start": {"line": 5, "character": 0}, "end": {"line": 7, "character": 0}}, "newText": ""}]);
        assert_eq!(prepared_text(trailing), Ok("\n\na\n\nb\n".to_string()));
        let each = json!([
            {"range": {"start": {"line": 5, "character": 0}, "end": {"line": 6, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 6, "character": 0}, "end": {"line": 7, "character": 0}}, "newText": ""}
        ]);
        assert_eq!(prepared_text(each), Ok("\n\na\n\nb\n".to_string()));
        let reason = "content changes may only delete blank lines at the document's edges";
        for changes in [
            // A blank line after one that stays.
            json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]),
            // A blank line between content lines.
            json!([{"range": {"start": {"line": 3, "character": 0}, "end": {"line": 4, "character": 0}}, "newText": ""}]),
            // A blank line before one that stays.
            json!([{"range": {"start": {"line": 5, "character": 0}, "end": {"line": 6, "character": 0}}, "newText": ""}]),
        ] {
            assert_eq!(
                prepared_text(changes),
                Err(PrepareError::InvalidChange { index: 0, reason })
            );
        }
        // Before a last line with content, a blank line is no closing one.
        let before_content = json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]);
        assert_eq!(
            prepare("a\n\nb", before_content),
            Err(PrepareError::InvalidChange { index: 0, reason })
        );
        let reason = "content changes may only delete whole blank lines";
        for (virtual_text, changes) in [
            // A line break alone, joining a line onto the previous one.
            (
                virtual_text.as_str(),
                json!([{"range": {"start": {"line": 2, "character": 1}, "end": {"line": 3, "character": 0}}, "newText": ""}]),
            ),
            // A blank line and part of the next line's indent.
            (
                "\n  a\n",
                json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 1}}, "newText": ""}]),
            ),
        ] {
            assert_eq!(
                prepare(virtual_text, changes),
                Err(PrepareError::InvalidChange { index: 0, reason })
            );
        }
    }

    #[test]
    fn edge_blank_lines_end_with_any_line_break() {
        for eol in ["\r\n", "\r"] {
            let virtual_text = format!("{eol}  a{eol}  b{eol}{eol}");
            let layout = VirtualLayout::single(&virtual_text);
            let prepared = apply_prepare_result(
                &virtual_text,
                &layout,
                result(json!({"segments": [{"type": "content", "changes": [
                    {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""},
                    {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 4, "character": 0}}, "newText": ""}
                ]}]})),
            )
            .unwrap();
            assert_eq!(prepared.text, format!("a{eol}b{eol}"));
            let map = prepared.map.unwrap();
            // A line added at P's end lands before the deleted line,
            // indented: both need the line break recognized as one.
            let formatted = map
                .edits_to_virtual(&[edit((0, 0), (2, 0), &format!("a{eol}b{eol}c{eol}"))])
                .unwrap();
            assert_eq!(
                apply_to(&virtual_text, &formatted),
                format!("{eol}  a{eol}  b{eol}  c{eol}{eol}")
            );
            // A closing indent after the deleted line ends it there too.
            let virtual_text = format!("{eol}  a{eol}{eol}  ");
            let prepared = apply_prepare_result(
                &virtual_text,
                &VirtualLayout::single(&virtual_text),
                result(json!({"segments": [{"type": "content", "changes": [
                    {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""},
                    {"range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 2}}, "newText": ""}
                ]}]})),
            )
            .unwrap();
            assert_eq!(prepared.text, format!("{eol}  a{eol}"));
        }
    }

    #[test]
    fn a_deletion_splitting_a_crlf_is_refused() {
        // LSP treats CRLF as one line break, but a position between its CR
        // and LF still names an offset: deleting up to or from there would
        // leave a lone CR or LF the map does not count.
        for (virtual_text, start, end) in [
            ("\r\nfoo", (0, 0), (0, 1)),
            ("  \r\nfoo", (0, 0), (0, 3)),
            ("foo\r\n\r\n", (0, 4), (2, 0)),
            ("a\r\n\r\n  ", (1, 1), (2, 0)),
        ] {
            let change = json!({"range": {
                "start": {"line": start.0, "character": start.1},
                "end": {"line": end.0, "character": end.1}
            }, "newText": ""});
            assert_eq!(
                apply_prepare_result(
                    virtual_text,
                    &VirtualLayout::single(virtual_text),
                    result(json!({"segments": [{"type": "content", "changes": [change]}]})),
                ),
                Err(PrepareError::InvalidChange {
                    index: 0,
                    reason: "content changes may only delete whole blank lines"
                }),
                "{virtual_text:?} {start:?}-{end:?}"
            );
        }
    }

    #[test]
    fn an_all_blank_document_starts_after_its_deleted_lines() {
        // An empty Nix `''\n''`: the peer deletes its one line, so P is
        // empty, and P's start is still after the host's line break.
        let virtual_text = "\n".to_string();
        let prepared = apply_prepare_result(
            &virtual_text,
            &VirtualLayout::single(&virtual_text),
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "");
        let map = prepared.map.unwrap();
        assert_eq!(map.to_virtual(pos(0, 0), Bias::Start), pos(1, 0));
        let inserted = map.edit_to_virtual(&edit((0, 0), (0, 0), "x\n")).unwrap();
        assert_eq!(apply_to(&virtual_text, &[inserted]), "\nx\n");
    }

    #[test]
    fn blank_lines_beside_a_gap_stay() {
        // Two combined fences: the blank lines around the host text between
        // them are inside the document, whatever segment they end or open.
        let virtual_text = "foo\n\n\n\n\nbar\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..5, String::new()),
                (SegmentKind::Gap, 5..7, "```\n```lua\n".to_string()),
                (SegmentKind::Content, 7..12, String::new()),
            ],
        );
        let line = json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]);
        let opening = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]);
        for (index, segments) in [
            (
                0,
                json!([{"type": "content", "changes": line}, {"type": "gap"}, {"type": "content"}]),
            ),
            (
                2,
                json!([{"type": "content"}, {"type": "gap"}, {"type": "content", "changes": opening}]),
            ),
        ] {
            assert_eq!(
                apply_prepare_result(
                    &virtual_text,
                    &layout,
                    result(json!({"segments": segments}))
                ),
                Err(PrepareError::InvalidChange {
                    index,
                    reason: "content changes may only delete blank lines at the document's edges"
                })
            );
        }
        // A first or last segment that is all blank lines is beside the gap
        // too, though it holds the document's edge.
        let virtual_text = "\n\n\n\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..1, String::new()),
                (SegmentKind::Gap, 1..3, "```\n```lua\n".to_string()),
                (SegmentKind::Content, 3..4, String::new()),
            ],
        );
        let whole = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]);
        for (index, segments) in [
            (
                0,
                json!([{"type": "content", "changes": whole}, {"type": "gap"}, {"type": "content"}]),
            ),
            (
                2,
                json!([{"type": "content"}, {"type": "gap"}, {"type": "content", "changes": whole}]),
            ),
        ] {
            assert_eq!(
                apply_prepare_result(
                    &virtual_text,
                    &layout,
                    result(json!({"segments": segments}))
                ),
                Err(PrepareError::InvalidChange {
                    index,
                    reason: "content changes may only delete blank lines at the document's edges"
                })
            );
        }
    }

    #[test]
    fn edge_lines_beside_a_gap_within_a_line_go() {
        // `''\n  ${x}/bin/foo\n''` and `''\n  a\n  ${x}\n\n''`: the gap
        // sits within a line, so the blank text left beside it is that
        // line's indent or line break, not blank lines beside the gap.
        let first = "\n      /bin/foo\n";
        let layout = VirtualLayout::from_pieces(
            first,
            [
                (SegmentKind::Content, 0..3, String::new()),
                (SegmentKind::Gap, 3..7, "${x}".to_string()),
                (SegmentKind::Content, 7..16, String::new()),
            ],
        );
        let opening = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}]);
        let prepared = apply_prepare_result(
            first,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": opening}, {"type": "gap"}, {"type": "content"}]})),
        )
        .map(|prepared| prepared.text);
        assert_eq!(prepared, Ok("      /bin/foo\n".to_string()));
        let last = "\n  a\n      \n\n";
        let layout = VirtualLayout::from_pieces(
            last,
            [
                (SegmentKind::Content, 0..7, String::new()),
                (SegmentKind::Gap, 7..11, "${x}".to_string()),
                (SegmentKind::Content, 11..13, String::new()),
            ],
        );
        let closing = json!([{"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}]);
        let prepared = apply_prepare_result(
            last,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap"}, {"type": "content", "changes": closing}]})),
        )
        .map(|prepared| prepared.text);
        assert_eq!(prepared, Ok("\n  a\n      \n".to_string()));
    }

    #[test]
    fn a_line_break_opening_a_segment_mid_line_stays() {
        // `''\n  a\n'' + ''\n  b\n''`: the second string opens after the
        // joining Nix on the same line, so its line break is no whole line.
        let virtual_text = "\n  a\n       \n  b\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..5, String::new()),
                (SegmentKind::Gap, 5..12, "'' + ''".to_string()),
                (SegmentKind::Content, 12..17, String::new()),
            ],
        );
        assert_eq!(
            apply_prepare_result(
                &virtual_text,
                &layout,
                result(json!({"segments": [
                    {"type": "content"},
                    {"type": "gap"},
                    {"type": "content", "changes": [
                        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}
                    ]}
                ]})),
            ),
            Err(PrepareError::InvalidChange {
                index: 2,
                reason: "content changes may only delete whole blank lines"
            })
        );
    }

    #[test]
    fn content_change_must_delete_leading_whitespace_or_whole_lines() {
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
                "content changes may only delete whitespace",
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
        // …while an insertion there stays an insertion, before the gap too.
        let inserted = map.edit_to_virtual(&edit((0, 2), (0, 2), "!")).unwrap();
        assert_eq!(inserted.range.start, inserted.range.end);
        assert_eq!(apply_to(&virtual_text, &[inserted]), "ab!    cd");
        let formatted = map
            .edits_to_virtual(&[edit((0, 0), (0, 4), "ab;cd")])
            .unwrap();
        assert_eq!(apply_to(&virtual_text, &formatted), "ab;    cd");
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
