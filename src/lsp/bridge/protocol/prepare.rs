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
//! Content may only lose leading whitespace and blank lines at the
//! document's edges, so the map is fixed by the answer itself: every P
//! position has one V position, and every V position outside those deleted
//! runs and the gaps one P position. Gaps are opaque: a position inside one
//! stands for no P position of its own.

use std::ops::Range;
use std::time::Duration;

use line_index::{LineIndex, TextRange, WideEncoding, WideLineCol};

use crate::language::injection::VirtualGap;
use crate::text::position::normalize_lone_carriage_returns;
use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{Position, TextEdit};

mod edits;

pub(crate) use edits::{EditMode, FormattingRefusal};

/// The request kakehashi sends to the configured prepare peer.
pub(crate) const PREPARE_METHOD: &str = "kakehashi/virtualDocument/prepare";

/// How long one prepare request may wait for its answer (after the peer is
/// up). A request that times out counts as no answer and is retried with
/// backoff; the document is held back meanwhile.
pub(crate) const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);

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

    /// Present a virtual document as content and gap segments: the gaps the
    /// resolver recorded (see [`VirtualGap`]), and content everywhere
    /// between them. An isolated document is one content segment, an empty
    /// one included.
    pub(crate) fn from_gaps(virtual_text: &str, gaps: &[VirtualGap]) -> Self {
        let mut pieces = Vec::with_capacity(gaps.len() * 2 + 1);
        let mut cursor = 0;
        for gap in gaps {
            if cursor < gap.virtual_range.start {
                pieces.push((
                    SegmentKind::Content,
                    cursor..gap.virtual_range.start,
                    String::new(),
                ));
            }
            pieces.push((
                SegmentKind::Gap,
                gap.virtual_range.clone(),
                gap.host_text.clone(),
            ));
            cursor = cursor.max(gap.virtual_range.end);
        }
        if cursor < virtual_text.len() || pieces.is_empty() {
            pieces.push((
                SegmentKind::Content,
                cursor..virtual_text.len(),
                String::new(),
            ));
        }
        Self::from_pieces(virtual_text, pieces)
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
/// "unchanged" — but for the cancellation codes LSP lets a client retry,
/// which are `Interrupted`: no answer at all.
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
    #[error("the answer joins a CR and a LF the document keeps apart into one line break")]
    FusedLineBreak,
}

// =============================================================================
// Applying an answer
// =============================================================================

/// A virtual document as downstream servers receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedDocument {
    /// The text sent downstream.
    pub(crate) text: String,
    /// The map back to the virtual document, even for an answer that changed
    /// nothing: it is what records that the document went through the peer.
    pub(crate) map: std::sync::Arc<PreparedMap>,
}

/// [`apply_prepare_result_at`] taking its place in the presentation order
/// now, as a test applying an answer directly does.
#[cfg(test)]
pub(crate) fn apply_prepare_result(
    virtual_text: &str,
    layout: &VirtualLayout,
    result: Option<PrepareResult>,
) -> Result<PreparedDocument, PrepareError> {
    apply_prepare_result_at(virtual_text, layout, result, next_presentation())
}

/// Apply a peer answer to the virtual document it was asked about, for an
/// input taken at `taken` in the presentation order (see
/// [`next_presentation`]): what orders it against other texts is when its
/// virtual text was read, not when the answer came — two attempts in flight
/// can answer in either order.
///
/// `result` of `None` is the wire `null`: every gap keeps its
/// coordinate-preserving whitespace and no content changes.
pub(crate) fn apply_prepare_result_at(
    virtual_text: &str,
    layout: &VirtualLayout,
    result: Option<PrepareResult>,
    taken: u64,
) -> Result<PreparedDocument, PrepareError> {
    build_prepared(virtual_text, layout, result, taken, true)
}

/// [`apply_prepare_result_at`], refusing an answer that fuses a line break
/// only when `check_line_breaks` (see [`fuses_a_line_break`]).
fn build_prepared(
    virtual_text: &str,
    layout: &VirtualLayout,
    result: Option<PrepareResult>,
    taken: u64,
    check_line_breaks: bool,
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
                // Indentation removed from whitespace-only lines: the
                // content's indent only in a segment with no content line to
                // say it (a blank line left indented says nothing of it).
                let mut blank: Vec<&str> = Vec::new();
                let mut closing: Option<&str> = None;
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
                    // The indentation-only last line the peer deleted (the
                    // indent before a closing `''`, with the closing lines or
                    // right after content, where nixfmt puts it shallower
                    // than the content) does not outvote the content's
                    // dedent: new lines regain the content's, and with content
                    // left alone, none. Only in a segment with no content at
                    // all, and no closing lines deleted with it, is it the
                    // indent new lines regain.
                    let closing_indent = kind == RunKind::Deleted
                        && index == last_index
                        && deletion.end == segment.virtual_range.end;
                    if closing_indent {
                        closing = Some(&virtual_text[deletion.clone()]);
                    } else if kind == RunKind::Deleted {
                        if rest_of_line_blank(virtual_text, deletion.end) {
                            blank.push(&virtual_text[deletion.clone()]);
                        } else {
                            deleted.push(&virtual_text[deletion.clone()]);
                        }
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
                let whitespace_only = virtual_segment
                    .chars()
                    .all(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
                // Content lines set the indent new lines regain; with none,
                // the blank lines' removed indentation, then the closing one.
                if deleted.is_empty() && whitespace_only {
                    deleted = blank;
                }
                if deleted.is_empty()
                    && !deletes_closing_lines
                    && whitespace_only
                    && let Some(closing) = closing
                {
                    deleted.push(closing);
                }
                indents.push(SegmentIndent::from_deletions(
                    virtual_text,
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

    if check_line_breaks && fuses_a_line_break(&text, &runs, virtual_text) {
        return Err(PrepareError::FusedLineBreak);
    }
    // A segment that says nothing of its indentation (no indent removed,
    // and no block of text of its own: the rest of a line after an
    // interpolation, blank lines) is indented as the content before it.
    for index in 1..indents.len() {
        if !indents[index].evidence {
            indents[index].uniform = indents[index - 1].uniform.clone();
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
        built: taken,
        #[cfg(test)]
        input: 0,
        revocation: None,
    };
    Ok(PreparedDocument {
        text,
        map: std::sync::Arc::new(map),
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
    let mut ranges = Vec::with_capacity(changes.len());
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
        ranges.push(start..end);
    }
    ranges.sort_by_key(|range| range.start);
    if ranges.windows(2).any(|pair| pair[0].end > pair[1].start) {
        return Err("content changes overlap");
    }
    // Deletions that touch inside a line are one: a peer may split a line's
    // indentation, or a blank line's indentation and its line break, across
    // edits, each but the first then starting past the line's start. Never
    // where they touch at a line start, which would join a deleted blank
    // line to the next line's indentation into no whole line.
    let touch_inside_a_line = |at: usize| !is_line_start(virtual_text, segment_start + at);
    let mut joined: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match joined.last_mut() {
            Some(last) if last.end == range.start && touch_inside_a_line(range.start) => {
                last.end = range.end;
            }
            _ => joined.push(range),
        }
    }
    let mut deletions = Vec::with_capacity(joined.len());
    for Range { start, end } in joined {
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
    // there in P that a position at P's end could belong to.
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

/// The indentation a content segment lost, which lines an edit to P creates
/// there regain.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentIndent {
    virtual_range: Range<usize>,
    /// `Some(indent)` when every deletion in the segment removed the same
    /// text (a dedent); `None` when they differ, so a new line of text has no
    /// single right indentation (a blank one needs none). Indentation removed from whitespace-only lines
    /// counts only in a segment of whitespace alone (a blank line says
    /// nothing of the content's indent), and the document's deleted
    /// indentation-only last line (the closing indent) only there with no
    /// other deletion and no closing lines deleted with it. A
    /// segment without `evidence` takes the previous segment's instead.
    uniform: Option<String>,
    /// Whether the segment says anything of its indentation: an indent
    /// removed, or text in a segment that opens a line of its own.
    evidence: bool,
}

impl SegmentIndent {
    fn from_deletions(virtual_text: &str, virtual_range: Range<usize>, deleted: &[&str]) -> Self {
        // A line of text the peer left alone says nothing of the indent new
        // lines regain, as within one segment; but a segment that opens a
        // line of its own (a block after a fence) and holds text is a block
        // the peer left unindented, not the rest of the line before it.
        let evidence = !deleted.is_empty()
            || (is_line_start(virtual_text, virtual_range.start)
                && !virtual_text[virtual_range.clone()]
                    .trim_matches([' ', '\t', '\n', '\r'])
                    .is_empty());
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
            evidence,
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
    /// One per content segment, in order; derived from the runs, so not
    /// part of the fingerprint.
    indents: Vec<SegmentIndent>,
    fingerprint: u64,
    /// Where the input its answer came from was taken in the presentation
    /// order (see [`apply_prepare_result_at`]); not part of its equality.
    built: u64,
    /// That input's identity apart from the epoch it was asked under (see
    /// [`AnswerOrigin`]), named by tests.
    #[cfg(test)]
    input: u64,
    /// What revokes it (see [`Revocation`]): set by the registry, shared with
    /// every map answered for the same input of the same region, not part
    /// of its equality. `None` for a map no registry gave out.
    revocation: Option<std::sync::Arc<Revocation>>,
}

/// When the peer last failed one input of one region — its text and gaps
/// and host language, whatever the epoch it was asked under: the answers
/// given for that input taken before then are ones it no longer gives.
/// Shared by every map answered for the input, and marked by the registry
/// as each such failure is recorded, before the failure is
/// published: whether a map is revoked is read off the map, for as long as
/// it lives, however the registry's answers are evicted meanwhile.
#[derive(Debug, Default)]
pub(crate) struct Revocation(std::sync::atomic::AtomicU64);

impl Revocation {
    /// Record a failure of the input taken at `taken` in the presentation
    /// order (see [`next_presentation`]).
    pub(crate) fn revoke(&self, taken: u64) {
        self.0.fetch_max(taken, std::sync::atomic::Ordering::AcqRel);
    }

    /// Whether a failure taken after `taken` was recorded.
    fn revokes(&self, taken: u64) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire) > taken
    }
}

/// Which answer a prepared text came from: when its input was taken, in the
/// presentation order (one per registry cell), and the input's identity
/// apart from the epoch and candidates it was asked under — the same text
/// asked again after an epoch move has the same `input`. What a
/// [`Revocation`] tracks per map, named by tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AnswerOrigin {
    pub(crate) taken: u64,
    pub(crate) input: u64,
}

/// A process-wide order of the decisions about what a virtual document's
/// servers are sent: a prepared answer applied, a text recorded as the
/// latest to send. Tells which of two such decisions is the newer.
pub(crate) fn next_presentation() -> u64 {
    static PRESENTATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    PRESENTATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

impl PartialEq for PreparedMap {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
    }
}

impl Eq for PreparedMap {}

impl PreparedMap {
    /// Where the input this map's answer came from was taken in the
    /// presentation order (see [`apply_prepare_result_at`]).
    pub(crate) fn built(&self) -> u64 {
        self.built
    }

    /// Which answer this map came from (see [`AnswerOrigin`]).
    #[cfg(test)]
    pub(crate) fn origin(&self) -> AnswerOrigin {
        AnswerOrigin {
            taken: self.built,
            input: self.input,
        }
    }

    /// Name the input this map's answer was given for (see
    /// [`AnswerOrigin::input`]).
    #[cfg(test)]
    pub(crate) fn set_input(&mut self, input: u64) {
        self.input = input;
    }

    /// Share `revocation` (see [`Revocation`]), as the registry applies the
    /// answer.
    pub(crate) fn set_revocation(&mut self, revocation: std::sync::Arc<Revocation>) {
        self.revocation = Some(revocation);
    }

    /// The indentation the peer removed right at P offset `prepared` (a
    /// deleted run P shows nothing of), if any.
    fn removed_indent_at(&self, prepared: usize) -> Option<&Run> {
        empty_run(&self.runs, prepared, |kind| kind == RunKind::Deleted)
    }

    /// Whether `a` and `b` translate alike: built from the same texts with
    /// the same runs (the maps' equality), whichever answers they came from.
    /// What decides whether a server holding one needs the other's text sent
    /// (it does not), or reads a reply computed on one through the other.
    pub(crate) fn same_coordinates(a: Option<&PreparedMap>, b: Option<&PreparedMap>) -> bool {
        a == b
    }

    /// Whether `a` and `b` are the same answer — one map, which one
    /// revocation speaks for — not merely alike in coordinates. What
    /// decides whether a record or a cache still holds the answer a
    /// request was built on, and whether a holder moved to another.
    pub(crate) fn same_answer(
        a: Option<&std::sync::Arc<PreparedMap>>,
        b: Option<&std::sync::Arc<PreparedMap>>,
    ) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Whether `incoming` is an answer for the same input as `held` (they
    /// share what revokes them), taken before it: holding it in its place
    /// would move that input's answer backwards, and once a failure taken
    /// between the two revoked it alone, hold a revoked text where a
    /// standing one was. Answers for other inputs (an undo back to a text
    /// answered before the edit) are never taken before: whichever is
    /// recorded last stands, however alike their coordinates.
    pub(crate) fn taken_before(
        incoming: Option<&std::sync::Arc<PreparedMap>>,
        held: Option<&std::sync::Arc<PreparedMap>>,
    ) -> bool {
        matches!(
            (incoming, held),
            (Some(incoming), Some(held))
                if incoming.built < held.built
                    && matches!(
                        (&incoming.revocation, &held.revocation),
                        (Some(incoming), Some(held)) if std::sync::Arc::ptr_eq(incoming, held)
                    )
        )
    }

    /// Whether the peer failed this map's input since it was taken: an
    /// answer it no longer gives, through which no coordinates are read and
    /// no request is sent.
    pub(crate) fn revoked(&self) -> bool {
        self.revocation
            .as_ref()
            .is_some_and(|revocation| revocation.revokes(self.built))
    }

    /// The length (in UTF-16 units, its line break excluded) of the virtual
    /// document's `line`; the last line's for one past the end.
    pub(crate) fn virtual_line_length(&self, line: u32) -> u32 {
        let end = self
            .virtual_lines
            .offset_clamped(Position::new(line, u32::MAX));
        self.virtual_lines.position(end).character
    }

    /// The virtual text this map's answer was given for.
    pub(crate) fn virtual_text(&self) -> &str {
        &self.virtual_lines.text
    }

    /// The prepared text this map was built for.
    #[cfg(test)]
    pub(crate) fn prepared_text(&self) -> &str {
        &self.prepared_lines.text
    }

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

    /// Whether a V position sits on the blank lines the peer deleted from
    /// the document's edges (starts included): those lines are not in P at
    /// all, so a request there would be answered for another line.
    pub(crate) fn virtual_position_in_removed_lines(&self, position: Position) -> bool {
        self.offset_in_removed_lines(self.virtual_lines.offset_clamped(position))
    }

    fn offset_in_removed_lines(&self, offset: usize) -> bool {
        // From where the document's deleted closing lines start, V holds only
        // what P dropped (those lines and the closing indent after them), up
        // to its very end. P's end maps to that start, yet a position there
        // is on the first of those lines, which P does not have. Those are
        // the runs empty in P at its end; the closing lines may be several
        // runs, deleted one by one.
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
        self.runs_at_virtual(offset).any(|run| {
            run.kind == RunKind::LeadingLines
                && run.virtual_.start <= offset
                && offset < run.virtual_.end
        })
    }

    /// The runs whose V range may hold V `offset`.
    fn runs_at_virtual(&self, offset: usize) -> impl Iterator<Item = &Run> {
        let first = self.runs.partition_point(|run| run.virtual_.end <= offset);
        self.runs[first..]
            .iter()
            .take_while(move |run| run.virtual_.start <= offset)
    }

    /// Whether a V position sits within indentation or blank lines the peer
    /// removed: P has no position there, and one at the line's content (or
    /// the content's end) stands in for it, starts included.
    pub(crate) fn virtual_position_in_removed_indent(&self, position: Position) -> bool {
        let offset = self.virtual_lines.offset_clamped(position);
        self.offset_in_removed_lines(offset)
            || self.runs_at_virtual(offset).any(|run| {
                run.kind == RunKind::Deleted
                    && run.virtual_.start <= offset
                    && offset < run.virtual_.end
            })
    }

    /// Whether the V range reaches inside a gap's host text (an empty range:
    /// whether it sits strictly inside one).
    pub(crate) fn virtual_range_in_gap(&self, range: tower_lsp_server::ls_types::Range) -> bool {
        self.virtual_range_reaches_into_gap(
            self.virtual_lines.offset_clamped(range.start),
            self.virtual_lines.offset_clamped(range.end),
        )
    }

    /// Whether a P range, mapped as ranges are (its start as positions, its
    /// end after any run it ends inside), covers in V exactly the text it
    /// covers in P: no indentation the peer removed, no hidden line nor gap
    /// text inside it. A range a client shows as the text an action works
    /// on (the symbol a rename will replace) or edits through in place
    /// (linked ranges typed into together) is right only so: what it
    /// stands for in the host is what the server saw.
    pub(crate) fn range_reads_alike(&self, range: tower_lsp_server::ls_types::Range) -> bool {
        if range.start > range.end {
            return false;
        }
        let start = self.prepared_lines.offset_clamped(range.start);
        let end = self.prepared_lines.offset_clamped(range.end);
        let virtual_start = map_offset(&self.runs, start, Bias::Start, Side::Prepared);
        let virtual_end = if start == end {
            virtual_start
        } else {
            map_offset(&self.runs, end, Bias::End, Side::Prepared)
        };
        virtual_start <= virtual_end
            && self.prepared_lines.text[start..end]
                == self.virtual_lines.text[virtual_start..virtual_end]
    }

    /// Whether a V position sits strictly inside a gap: host-owned text no
    /// downstream position stands for.
    pub(crate) fn virtual_position_in_gap(&self, position: Position) -> bool {
        let offset = self.virtual_lines.offset_clamped(position);
        self.virtual_range_reaches_into_gap(offset, offset)
    }

    /// Whether the V range `start..end` reaches inside a gap's V text (an
    /// empty range: whether it sits strictly inside one).
    fn virtual_range_reaches_into_gap(&self, start: usize, end: usize) -> bool {
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
/// the indent V deleted there: a position there names the line's content.
/// A range end there ([`Bias::End`]) stops before that indent instead: it
/// covers nothing of the line, as it does in P. Likewise P's start maps
/// after the lines deleted from the document's start, and P's end, whatever
/// the bias, before those deleted from its end.
fn map_offset(runs: &[Run], offset: usize, bias: Bias, from: Side) -> usize {
    map_offset_stopping(runs, offset, bias, from, |kind| {
        matches!(kind, RunKind::Gap | RunKind::Deleted)
    })
}

/// [`map_offset`], with a [`Bias::End`] offset from P stopping before the
/// empty runs of the kinds `stops_before` accepts that sit there (and before
/// the document's deleted closing lines, whatever it accepts).
fn map_offset_stopping(
    runs: &[Run],
    offset: usize,
    bias: Bias,
    from: Side,
    stops_before: impl Fn(RunKind) -> bool,
) -> usize {
    // P's end, where the document's deleted closing lines were: the
    // content ends there in V too, before them.
    if matches!(from, Side::Prepared)
        && let Some(lines) = empty_run(runs, offset, |kind| kind == RunKind::TrailingLines)
    {
        return lines.virtual_.start;
    }
    // The end of a P range that reaches a gap the peer replaced with nothing,
    // or a line start whose indent the peer deleted, stops before it: the
    // range covers none of that host text.
    if bias == Bias::End
        && matches!(from, Side::Prepared)
        && let Some(run) = empty_run(runs, offset, stops_before)
    {
        return run.virtual_.start;
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

/// The first run, in V order, that is empty in P, sits at P `offset` and is
/// of a kind `kind` accepts.
fn empty_run(runs: &[Run], offset: usize, kind: impl Fn(RunKind) -> bool) -> Option<&Run> {
    let first = runs.partition_point(|run| run.prepared.start < offset);
    runs[first..]
        .iter()
        .take_while(|run| run.prepared.start == offset)
        .find(|run| kind(run.kind) && run.prepared.is_empty())
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

    /// Byte offset of `position`, or `None` unless its line exists and its
    /// column falls between characters. A column past the line's content
    /// defaults back to the line's length, as LSP has it.
    fn offset_strict(&self, position: Position) -> Option<usize> {
        let line = self.index.0.line(position.line)?;
        // Also keeps the column from overflowing once the index adds it to
        // the line's start.
        let length = self.position(self.content_end(line)).character;
        let position = Position::new(position.line, position.character.min(length));
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
        #[cfg(test)]
        CONVERSIONS.with(|conversions| conversions.set(conversions.get() + 1));
        let Some(line_range) = self.index.0.line(position.line) else {
            return self.text.len();
        };
        let line_start: usize = line_range.start().into();
        let content_end = self.content_end(line_range);
        let content = &self.text[line_start..content_end];
        // Through the index's per-line table of non-ASCII characters rather
        // than a scan of the line. A line has no more UTF-16 units than
        // bytes, so capping the column at the byte length keeps every column
        // on the line while the byte it maps to cannot overflow; a column
        // past the content clamps to its end, one inside a surrogate pair to
        // the end of its character.
        let character = position
            .character
            .min(u32::try_from(content.len()).unwrap_or(u32::MAX));
        let byte = self
            .index
            .0
            .to_utf8(
                WideEncoding::Utf16,
                WideLineCol {
                    line: position.line,
                    col: character,
                },
            )
            .and_then(|line_col| self.index.0.offset(line_col))
            .map_or(content_end, usize::from)
            .min(content_end);
        self.text.ceil_char_boundary(byte)
    }

    /// Where `line`'s content ends: before its line break.
    fn content_end(&self, line: TextRange) -> usize {
        let line_start: usize = line.start().into();
        let line_end: usize = line.end().into();
        let line = &self.text[line_start..line_end];
        let content = line
            .strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .unwrap_or_else(|| line.strip_suffix('\r').unwrap_or(line));
        line_start + content.len()
    }

    /// The byte offsets of `positions`, each as [`Self::offset_clamped`]
    /// gives it, in one forward pass over each line they name: a line's
    /// first position scans it from its start, and each later one on it
    /// from the one before. Converting positions one by one rescans a
    /// line's earlier non-ASCII characters for each, quadratic in the
    /// positions on one long line.
    fn offsets_clamped(&self, positions: &[Position]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..positions.len()).collect();
        order.sort_by_key(|&index| (positions[index].line, positions[index].character));
        let mut offsets = vec![0; positions.len()];
        // The line scanned so far: its number, its content's end, and the
        // byte and UTF-16 column reached on it.
        let mut at: Option<(u32, usize, usize, u32)> = None;
        for index in order {
            let position = positions[index];
            let (byte, column, content_end) = match at {
                Some((line, content_end, byte, column)) if line == position.line => {
                    (byte, column, content_end)
                }
                _ => {
                    let Some(line_range) = self.index.0.line(position.line) else {
                        offsets[index] = self.text.len();
                        continue;
                    };
                    (line_range.start().into(), 0, self.content_end(line_range))
                }
            };
            #[cfg(test)]
            let from = byte;
            let (mut byte, mut column) = (byte, column);
            for character in self.text[byte..content_end].chars() {
                if column >= position.character {
                    break;
                }
                column += character.len_utf16() as u32;
                byte += character.len_utf8();
            }
            #[cfg(test)]
            SCANNED.with(|scanned| scanned.set(scanned.get() + byte - from));
            at = Some((position.line, content_end, byte, column));
            offsets[index] = byte;
        }
        offsets
    }

    /// The positions of `offsets`, each as [`Self::position`] gives it, in
    /// one forward pass over each line they fall on (see
    /// [`Self::offsets_clamped`]), each line found in a number of lookups
    /// logarithmic in how far it lies past the line before.
    fn positions(&self, offsets: &[usize]) -> Vec<Position> {
        let mut order: Vec<usize> = (0..offsets.len()).collect();
        order.sort_by_key(|&index| offsets[index]);
        let mut positions = vec![Position::new(0, 0); offsets.len()];
        // The line reached so far, and the byte and UTF-16 column reached on
        // it. An offset's line is found from the one reached by doubling the
        // stride while that line still starts at or before the offset, then
        // halving it back: a few lookups however far it lies (looking a line
        // up by offset in the index checks every non-ASCII character on it,
        // as converting an offset does).
        let start = |line: u32| -> Option<usize> {
            #[cfg(test)]
            LINE_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));
            self.index.0.line(line).map(|range| range.start().into())
        };
        let (mut line, mut byte, mut column) = (0u32, 0usize, 0u32);
        for index in order {
            let offset = self
                .text
                .floor_char_boundary(offsets[index].min(self.text.len()));
            let reaches = |line: u32| start(line).is_some_and(|start| start <= offset);
            if line.checked_add(1).is_some_and(reaches) {
                let mut stride = 1u32;
                while line
                    .checked_add(stride.saturating_mul(2))
                    .is_some_and(reaches)
                {
                    stride = stride.saturating_mul(2);
                }
                let (mut low, mut high) =
                    (line + stride, line.saturating_add(stride.saturating_mul(2)));
                while low + 1 < high {
                    let middle = low + (high - low) / 2;
                    if reaches(middle) {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                (line, byte, column) = (low, start(low).unwrap_or(offset), 0);
            }
            column += self.text[byte..offset]
                .chars()
                .map(|character| character.len_utf16() as u32)
                .sum::<u32>();
            #[cfg(test)]
            SCANNED.with(|scanned| scanned.set(scanned.get() + offset - byte));
            byte = offset;
            positions[index] = Position::new(line, column);
        }
        positions
    }

    fn position(&self, offset: usize) -> Position {
        #[cfg(test)]
        CONVERSIONS.with(|conversions| conversions.set(conversions.get() + 1));
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

#[cfg(test)]
thread_local! {
    /// Positions or offsets converted one by one (see `LineMap::position`).
    static CONVERSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Bytes the batch conversions scanned (see `LineMap::positions`).
    static SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Lines `LineMap::positions` looked up to find its offsets' lines.
    static LINE_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

/// Whether the prepared text holds a CRLF whose CR and LF are not one CRLF
/// of the virtual text: a lone CR meeting a LF once whitespace or a gap
/// between them is gone, or a placeholder completing one. P would have a
/// line fewer than V where they meet, and a position between them would map
/// to another line.
fn fuses_a_line_break(prepared: &str, runs: &[Run], virtual_text: &str) -> bool {
    // The run a prepared byte at `offset` comes from (runs tile P in order).
    let run_of = |offset: usize| {
        let index = runs.partition_point(|run| run.prepared.end <= offset);
        runs.get(index)
            .filter(|run| run.prepared.contains(&offset))
            .map(|_| index)
    };
    // Whether each run holds V's text verbatim: unchanged content, or a gap
    // kept as its original text (a CRLF's LF left out of combined captures
    // that took its CR, say). Once per run, not per CRLF: a gap's
    // comparison reads all of it.
    let verbatim: Vec<bool> = runs
        .iter()
        .map(|run| match run.kind {
            RunKind::Identity => true,
            RunKind::Gap => {
                prepared.get(run.prepared.clone()) == virtual_text.get(run.virtual_.clone())
            }
            _ => false,
        })
        .collect();
    // The V offset of the prepared byte at `offset` in run `index`, when V
    // holds it there: anywhere in a verbatim run; in a gap rewritten
    // otherwise, only at the `edge` it meets a neighbour at, where it is the
    // byte the gap's text has there (the LF of a CRLF whose CR the content
    // before took, say). One byte compared, not the gap.
    let virtual_offset = |index: usize, offset: usize, edge: Edge| {
        let run = &runs[index];
        if verbatim[index] {
            return Some(run.virtual_.start + (offset - run.prepared.start));
        }
        if !matches!(run.kind, RunKind::Gap) || run.virtual_.is_empty() {
            return None;
        }
        let virtual_at = match edge {
            Edge::Leading => run.virtual_.start,
            Edge::Trailing => run.virtual_.end - 1,
        };
        (virtual_text.as_bytes()[virtual_at] == prepared.as_bytes()[offset]).then_some(virtual_at)
    };
    prepared.match_indices("\r\n").any(|(cr, _)| {
        let (Some(left), Some(right)) = (run_of(cr), run_of(cr + 1)) else {
            return true;
        };
        if left == right {
            // Within one run: as V has it there (a verbatim run), or the
            // placeholder's (or the answer's) own line break.
            return false;
        }
        // Across a seam: the CR ends the left run, the LF opens the right.
        // Each must be half of a CRLF of V — the same one, or two whose
        // other halves the seam left out — so no position of V lies between
        // them that P would have none for.
        match (
            virtual_offset(left, cr, Edge::Trailing),
            virtual_offset(right, cr + 1, Edge::Leading),
        ) {
            (Some(virtual_cr), Some(virtual_lf)) => {
                !splits_crlf(virtual_text, virtual_cr + 1) || !splits_crlf(virtual_text, virtual_lf)
            }
            _ => true,
        }
    })
}

/// Which edge of a run a byte meets its neighbour at.
#[derive(Clone, Copy)]
enum Edge {
    Leading,
    Trailing,
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

/// Whether the rest of `text`'s line from `offset` holds nothing but spaces
/// and tabs (or nothing).
fn rest_of_line_blank(text: &str, offset: usize) -> bool {
    let rest = text[offset..]
        .split(['\n', '\r'])
        .next()
        .unwrap_or_default();
    #[cfg(test)]
    tests::BLANK_SCANNED.with(|scanned| scanned.set(scanned.get() + rest.len()));
    rest.chars().all(|c| matches!(c, ' ' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Bytes `rest_of_line_blank` examined on this thread.
        pub(super) static BLANK_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// An answer whose prepared text would join a lone CR and a following
    /// LF into one CRLF — one line fewer than the virtual document keeps,
    /// with a position between them that maps to the wrong line — is
    /// refused. A CRLF the virtual document has stays.
    #[test]
    fn an_answer_fusing_a_cr_and_a_lf_is_refused() {
        let dedent_line_1 = |virtual_text: &str, columns: u32| {
            let answer: PrepareResult = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": columns}}, "newText": ""}
            ]}]}))
            .unwrap();
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                Some(answer),
            )
        };
        // V: "a" ends at a lone CR; line 1 is two spaces and its LF.
        assert!(dedent_line_1("a\r  \nb", 2).is_err());
        // V keeps a CRLF: dedenting the next line joins nothing.
        let kept = dedent_line_1("a\r\n  b", 2).unwrap();
        assert_eq!(kept.text, "a\r\nb");
    }

    /// A CRLF split between content and a gap, as combined Rust line
    /// comments with their children included capture it (the CR in the
    /// comment, the LF between comments), is the virtual text's own: kept by
    /// a `null` answer, it fuses nothing.
    #[test]
    fn a_crlf_split_between_content_and_a_kept_gap_is_the_virtual_texts_own() {
        // `// a\r\n// b\r\n`: each comment holds its CR; the LF between
        // them is a gap.
        let virtual_text = "// a\r\n// b\r";
        let gaps = [VirtualGap {
            virtual_range: 5..6,
            host_text: "\n".to_string(),
        }];
        let layout = VirtualLayout::from_gaps(virtual_text, &gaps);
        let prepared = apply_prepare_result(virtual_text, &layout, None).unwrap();
        assert_eq!(prepared.text, virtual_text);
        // So is one the gap starts with its CR, the content going on with its LF.
        let virtual_text = "a\r\nb";
        let gaps = [VirtualGap {
            virtual_range: 1..2,
            host_text: "\r".to_string(),
        }];
        let layout = VirtualLayout::from_gaps(virtual_text, &gaps);
        assert!(apply_prepare_result(virtual_text, &layout, None).is_ok());
        // A gap whose placeholder brings the CR the content's LF lacked
        // still fuses one.
        let virtual_text = "ax\nb";
        let gaps = [VirtualGap {
            virtual_range: 1..2,
            host_text: "x".to_string(),
        }];
        let layout = VirtualLayout::from_gaps(virtual_text, &gaps);
        let answer: PrepareResult = serde_json::from_value(serde_json::json!({"segments": [
            {"type": "content"},
            {"type": "gap", "content": "\r"},
            {"type": "content"}
        ]}))
        .unwrap();
        assert!(apply_prepare_result(virtual_text, &layout, Some(answer)).is_err());
    }

    /// The same split, as discovery itself makes it: combined Rust line
    /// comments with their children included take each comment's CR, and
    /// leave the LF between them a gap. A `null` answer is accepted, and so
    /// is one keeping that LF at the head of the gap's placeholder: the
    /// CRLF it ends is the virtual text's own.
    #[test]
    fn crlf_combined_comments_keep_their_split_line_break() {
        use crate::language::injection::InjectionResolver;
        let text = "// a\r\n// b\r\n";
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(text, None).unwrap();
        let query = tree_sitter::Query::new(
            &language,
            r#"((line_comment) @injection.content
                (#set! injection.language "markdown")
                (#set! injection.include-children)
                (#set! injection.combined))"#,
        )
        .unwrap();
        let resolved = InjectionResolver::resolve_all(
            &crate::language::LanguageCoordinator::new(),
            &crate::language::NodeTracker::new(),
            &url::Url::parse("file:///test/crlf-comments.rs").unwrap(),
            &tree,
            text,
            &query,
            0,
        );
        assert_eq!(resolved.len(), 1);
        let injection = &resolved[0];
        let virtual_text = &injection.virtual_content;
        // Discovery still splits it so: the CR content, the LF a gap.
        assert!(
            injection.gaps.iter().any(|gap| {
                virtual_text[gap.virtual_range.clone()].starts_with('\n')
                    && virtual_text[..gap.virtual_range.start].ends_with('\r')
            }),
            "{virtual_text:?}"
        );
        let layout = VirtualLayout::from_gaps(virtual_text, &injection.gaps);
        let prepared = apply_prepare_result(virtual_text, &layout, None).unwrap();
        assert_eq!(&prepared.text, virtual_text);
        let mut placeholders = 0;
        let segments: Vec<_> = layout
            .segments()
            .iter()
            .map(|segment| match segment.kind {
                SegmentKind::Content => serde_json::json!({"type": "content"}),
                SegmentKind::Gap
                    if virtual_text[segment.virtual_range.clone()].starts_with('\n') =>
                {
                    placeholders += 1;
                    serde_json::json!({"type": "gap", "content": "\nplaceholder"})
                }
                SegmentKind::Gap => serde_json::json!({"type": "gap"}),
            })
            .collect();
        assert!(placeholders > 0);
        let answer = serde_json::from_value(serde_json::json!({ "segments": segments })).unwrap();
        let prepared = apply_prepare_result(virtual_text, &layout, Some(answer)).unwrap();
        assert!(
            prepared.text.contains("\r\nplaceholder"),
            "{:?}",
            prepared.text
        );
    }

    /// A rewritten gap's CRLFs: one wholly inside its placeholder is the
    /// placeholder's own line break, whatever the gap's text; across a seam,
    /// the CR is the left run's last byte and the LF the right run's first,
    /// so a one-byte placeholder is read at the edge that meets the seam.
    #[test]
    fn a_rewritten_gap_keeps_the_crlfs_it_should() {
        let answer = |placeholder: &str| -> PrepareResult {
            serde_json::from_value(serde_json::json!({"segments": [
                {"type": "content"},
                {"type": "gap", "content": placeholder},
                {"type": "content"}
            ]}))
            .unwrap()
        };
        let apply = |virtual_text: &str, gap: Range<usize>, placeholder: &str| {
            let gaps = [VirtualGap {
                virtual_range: gap.clone(),
                host_text: virtual_text[gap].to_string(),
            }];
            let layout = VirtualLayout::from_gaps(virtual_text, &gaps);
            apply_prepare_result(virtual_text, &layout, Some(answer(placeholder)))
        };
        // Two CRLFs replaced by one, wholly the placeholder's.
        assert!(apply("a\r\n\r\nb", 1..5, "\r\n").is_ok());
        // A CR ending the gap's text kept before content opening with the
        // LF: the virtual text's own CRLF.
        assert!(apply("a\r  \r\nb", 1..5, "\r").is_ok());
        // The same CR read at the gap's head instead would not be one.
        assert!(apply("a\r  x\nb", 1..5, "\r").is_err());
        // A LF opening the gap's text kept after content closing with the
        // CR, more placeholder text after it.
        assert!(apply("a\r\n  b", 2..5, "\nx").is_ok());
        // A placeholder bringing the CR or the LF its edge did not have.
        assert!(apply("a\r \nb", 2..3, "\n").is_err());
        assert!(apply("ax\nb", 1..2, "\r").is_err());
    }

    /// Every small layout and placeholder, against an oracle that knows
    /// nothing of how CRs and LFs are told apart at a gap's edges: an
    /// answer is refused exactly when, built anyway, the map sends an LSP
    /// position of one text's content between a CR and a LF of the other
    /// that are not split there in the first (two line breaks made one, or
    /// one made two: no position names that place).
    ///
    /// Bounded to a few thousand cases; [`line_break_refusals_match_exhaustively`]
    /// runs the wider bounds.
    #[test]
    fn line_break_refusals_match_what_positions_survive() {
        let cases = check_line_break_refusals(b"a\r\n", 4, 2);
        assert!(cases > 5_000, "{cases}");
    }

    /// [`line_break_refusals_match_what_positions_survive`] over a wider
    /// alphabet and longer texts: millions of cases, tens of seconds.
    #[test]
    #[ignore = "exhaustive: run with --ignored"]
    fn line_break_refusals_match_exhaustively() {
        check_line_break_refusals(b"a\r\n ", 5, 3);
    }

    /// Check every virtual text over `alphabet` up to `max_virtual` bytes,
    /// every split of it into content and gaps, and every answer replacing
    /// its gaps (up to `max_placeholder` bytes for a single gap, one for
    /// several): the cases checked.
    fn check_line_break_refusals(
        alphabet: &[u8],
        max_virtual: usize,
        max_placeholder: usize,
    ) -> usize {
        fn strings(alphabet: &[u8], max: usize) -> Vec<Vec<u8>> {
            let mut all = vec![Vec::new()];
            let mut level = vec![Vec::new()];
            for _ in 0..max {
                level = level
                    .iter()
                    .flat_map(|prefix: &Vec<u8>| {
                        alphabet.iter().map(move |byte| {
                            let mut next = prefix.clone();
                            next.push(*byte);
                            next
                        })
                    })
                    .collect();
                all.extend(level.iter().cloned());
            }
            all
        }

        let placeholders = |max| -> Vec<Option<String>> {
            std::iter::once(None)
                .chain(
                    strings(alphabet, max)
                        .into_iter()
                        .map(|bytes| Some(String::from_utf8(bytes).unwrap())),
                )
                .collect()
        };
        let long = placeholders(max_placeholder);
        let short = placeholders(1);
        let mut cases = 0usize;
        for virtual_bytes in strings(alphabet, max_virtual) {
            let virtual_text = String::from_utf8(virtual_bytes).unwrap();
            let len = virtual_text.len();
            // Each byte content or gap; runs of one kind form a segment.
            for labels in 0u32..(1 << len) {
                let mut gaps = Vec::new();
                let mut index = 0;
                while index < len {
                    if labels & (1 << index) != 0 {
                        let start = index;
                        while index < len && labels & (1 << index) != 0 {
                            index += 1;
                        }
                        gaps.push(VirtualGap {
                            virtual_range: start..index,
                            host_text: virtual_text[start..index].to_string(),
                        });
                    } else {
                        index += 1;
                    }
                }
                let layout = VirtualLayout::from_gaps(&virtual_text, &gaps);
                let gap_count = layout
                    .segments()
                    .iter()
                    .filter(|segment| segment.kind == SegmentKind::Gap)
                    .count();
                let choices = if gap_count > 1 { &short } else { &long };
                let mut picks = vec![0usize; gap_count];
                loop {
                    let mut gap = 0;
                    let segments: Vec<_> = layout
                        .segments()
                        .iter()
                        .map(|segment| match segment.kind {
                            SegmentKind::Content => serde_json::json!({"type": "content"}),
                            SegmentKind::Gap => {
                                let content = &choices[picks[gap]];
                                gap += 1;
                                serde_json::json!({"type": "gap", "content": content})
                            }
                        })
                        .collect();
                    let answer = || -> PrepareResult {
                        serde_json::from_value(serde_json::json!({ "segments": segments })).unwrap()
                    };
                    cases += 1;
                    let verdict = build_prepared(&virtual_text, &layout, Some(answer()), 0, true);
                    let built = build_prepared(&virtual_text, &layout, Some(answer()), 0, false)
                        .expect("an answer without changes always builds");
                    // Every LSP position of one text maps to one of the
                    // other: a content offset of V (or a P offset of
                    // unchanged content) never lands between a CR and a LF
                    // the other text does not have there.
                    let runs = &built.map.runs;
                    let prepared = &built.text;
                    let virtual_positions = layout
                        .segments()
                        .iter()
                        .filter(|segment| segment.kind == SegmentKind::Content)
                        .flat_map(|segment| segment.virtual_range.start..=segment.virtual_range.end)
                        .all(|offset| {
                            splits_crlf(&virtual_text, offset)
                                || !splits_crlf(
                                    prepared,
                                    map_offset(runs, offset, Bias::Start, Side::Virtual),
                                )
                        });
                    let prepared_positions = runs
                        .iter()
                        .filter(|run| run.kind == RunKind::Identity)
                        .flat_map(|run| run.prepared.start..=run.prepared.end)
                        .all(|offset| {
                            !splits_crlf(prepared, offset)
                                || splits_crlf(
                                    &virtual_text,
                                    map_offset(runs, offset, Bias::Start, Side::Prepared),
                                )
                        });
                    let survives = virtual_positions && prepared_positions;
                    assert_eq!(
                        verdict.is_ok(),
                        survives,
                        "{virtual_text:?} gaps {:?} answer {:?} prepared {:?}",
                        gaps.iter()
                            .map(|gap| gap.virtual_range.clone())
                            .collect::<Vec<_>>(),
                        picks.iter().map(|pick| &choices[*pick]).collect::<Vec<_>>(),
                        built.text,
                    );
                    // Next combination of placeholders.
                    let mut digit = 0;
                    while digit < gap_count {
                        picks[digit] += 1;
                        if picks[digit] < choices.len() {
                            break;
                        }
                        picks[digit] = 0;
                        digit += 1;
                    }
                    if digit == gap_count {
                        break;
                    }
                }
            }
        }
        cases
    }

    /// Checking a gap kept as is reads it once, not once per CRLF in it: a
    /// gap of many CRLFs is checked in linear time.
    #[test]
    fn a_kept_gap_of_many_crlfs_is_checked_in_linear_time() {
        let gap = "\r\n".repeat(200_000);
        let virtual_text = format!("a\r{gap}\nb");
        let gaps = [VirtualGap {
            virtual_range: 2..2 + gap.len(),
            host_text: gap.clone(),
        }];
        let layout = VirtualLayout::from_gaps(&virtual_text, &gaps);
        let started = std::time::Instant::now();
        assert!(apply_prepare_result(&virtual_text, &layout, None).is_ok());
        // Quadratic, this takes seconds (200k CRLFs, each comparing the
        // whole 400 kB gap); linear, milliseconds even unoptimized.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    /// A gap placeholder that would complete a CRLF with the content beside
    /// it is refused the same way.
    #[test]
    fn a_placeholder_fusing_a_cr_and_a_lf_is_refused() {
        let gaps = [VirtualGap {
            virtual_range: 2..3,
            host_text: "x".to_string(),
        }];
        let virtual_text = "a\r \nb";
        let layout = VirtualLayout::from_gaps(virtual_text, &gaps);
        let answer = |placeholder: &str| -> PrepareResult {
            serde_json::from_value(serde_json::json!({"segments": [
                {"type": "content"},
                {"type": "gap", "content": placeholder},
                {"type": "content"}
            ]}))
            .unwrap()
        };
        assert!(apply_prepare_result(virtual_text, &layout, Some(answer(""))).is_err());
        assert!(apply_prepare_result(virtual_text, &layout, Some(answer("\n"))).is_err());
        assert!(apply_prepare_result(virtual_text, &layout, Some(answer("-"))).is_ok());
    }

    #[test]
    fn a_column_past_a_line_defaults_back_to_its_length() {
        let lines = LineMap::new("\n \r\nb".to_string());
        assert_eq!(lines.offset_strict(Position::new(1, u32::MAX)), Some(2));
        assert_eq!(lines.offset_strict(Position::new(1, 2)), Some(2));
        assert_eq!(lines.offset_strict(Position::new(1, 1)), Some(2));
        assert_eq!(lines.offset_strict(Position::new(0, 5)), Some(0));
        assert_eq!(lines.offset_strict(Position::new(2, 9)), Some(5));
        assert_eq!(
            lines.offset_strict(Position::new(3, 0)),
            None,
            "no such line"
        );
        assert_eq!(lines.offset_clamped(Position::new(1, u32::MAX)), 2);
    }

    #[test]
    fn positions_far_into_a_text_look_their_lines_up() {
        // One offset near the end of 100,000 lines (an edit a completion
        // or a hint maps): its line is found in a few lookups, not by
        // stepping through every line before it.
        let text = "x\n".repeat(100_000);
        let lines = LineMap::new(text.clone());
        LINE_LOOKUPS.with(|lookups| lookups.set(0));
        let offset = text.len() - 3;
        assert_eq!(lines.positions(&[offset]), vec![lines.position(offset)]);
        let lookups = LINE_LOOKUPS.with(|lookups| lookups.get());
        assert!(lookups <= 64, "{lookups}");
    }

    #[test]
    fn conversions_in_one_pass_agree_with_one_at_a_time() {
        let pieces = [
            "a",
            " ",
            "\u{e9}",
            "\u{1F600}",
            "\u{3042}",
            "\n",
            "\r",
            "\r\n",
        ];
        let mut seed: u64 = 7;
        let mut next = |n: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as usize) % n
        };
        for _ in 0..300 {
            let text: String = (0..next(30)).map(|_| pieces[next(pieces.len())]).collect();
            let lines = LineMap::new(text.clone());
            let offsets: Vec<usize> = (0..next(20)).map(|_| next(text.len() + 3)).collect();
            let one_at_a_time: Vec<Position> = offsets
                .iter()
                .map(|&offset| lines.position(offset))
                .collect();
            assert_eq!(
                lines.positions(&offsets),
                one_at_a_time,
                "{text:?} {offsets:?}"
            );
            let positions: Vec<Position> = (0..next(20))
                .map(|_| {
                    let character = if next(8) == 0 {
                        u32::MAX
                    } else {
                        next(12) as u32
                    };
                    Position::new(next(8) as u32, character)
                })
                .collect();
            let one_at_a_time: Vec<usize> = positions
                .iter()
                .map(|&position| lines.offset_clamped(position))
                .collect();
            assert_eq!(
                lines.offsets_clamped(&positions),
                one_at_a_time,
                "{text:?} {positions:?}"
            );
        }
    }

    #[test]
    fn a_clamped_offset_is_where_a_scan_of_its_line_stops() {
        // The column-by-column scan the indexed conversion replaces.
        fn scanned(text: &str, position: Position) -> usize {
            let mut lines = Vec::new();
            let mut start = 0;
            let bytes = text.as_bytes();
            for (index, byte) in bytes.iter().enumerate() {
                let ends =
                    *byte == b'\n' || (*byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'));
                if ends {
                    lines.push(start..index + 1);
                    start = index + 1;
                }
            }
            lines.push(start..text.len());
            let Some(range) = lines.get(position.line as usize) else {
                return text.len();
            };
            let line = &text[range.clone()];
            let content = line
                .strip_suffix('\n')
                .map(|line| line.strip_suffix('\r').unwrap_or(line))
                .unwrap_or_else(|| line.strip_suffix('\r').unwrap_or(line));
            let mut units = 0u32;
            for (byte, character) in content.char_indices() {
                if units >= position.character {
                    return range.start + byte;
                }
                units += character.len_utf16() as u32;
            }
            range.start + content.len()
        }
        let text = "a\u{1F600}b\u{e9}\r\n\u{3042}\rx\n\n\u{1F600}";
        let lines = LineMap::new(text.to_string());
        for line in 0..7 {
            for character in (0..12).chain([u32::MAX]) {
                let position = Position::new(line, character);
                assert_eq!(
                    lines.offset_clamped(position),
                    scanned(text, position),
                    "{position:?}"
                );
            }
        }
    }

    use serde_json::json;

    fn result(value: serde_json::Value) -> Option<PrepareResult> {
        serde_json::from_value(value).unwrap()
    }

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
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
    fn null_answer_without_gaps_still_maps_identically() {
        let layout = VirtualLayout::single("a\n");
        let prepared = apply_prepare_result("a\n", &layout, None).unwrap();
        assert_eq!(prepared.text, "a\n");
        let map = prepared.map;
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
        let map = prepared.map;
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
        let map = prepared.map;
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
        let map = prepared.map;
        assert_eq!(map.to_virtual(pos(1, 0), Bias::Start), pos(3, 0));
        assert_eq!(map.to_prepared(pos(3, 0), Bias::Start), pos(1, 0));
    }

    #[test]
    fn dedent_shifts_columns_by_the_deleted_indent() {
        let (_, prepared) = dedented();
        assert_eq!(prepared.text, "if x:\n  y\n");
        let map = prepared.map;
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
        let map = prepared.map;
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
        let map = prepared.map;
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

    /// A line's indentation deleted in pieces, in any order, is deleted as
    /// one: the text is V with those bytes gone, and the map is the one the
    /// whole deletion gives, a blank line's indentation and line break
    /// included. Never joined where pieces touch at a line start, so a
    /// deleted blank line and the next line's indentation still go apart.
    #[test]
    fn indentation_deleted_in_pieces_is_deleted_as_one() {
        let change = |start: (u32, u32), end: (u32, u32)| {
            json!({"range": {
                "start": {"line": start.0, "character": start.1},
                "end": {"line": end.0, "character": end.1}
            }, "newText": ""})
        };
        let prepare = |virtual_text: &str, changes: Vec<serde_json::Value>| {
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                result(json!({"segments": [{"type": "content", "changes": changes}]})),
            )
        };
        // (V, the pieces, the whole deletions, the bytes of V they delete)
        let cases: [(&str, Vec<_>, Vec<_>, std::ops::Range<usize>); 8] = [
            (
                "  a",
                vec![change((0, 0), (0, 1)), change((0, 1), (0, 2))],
                vec![change((0, 0), (0, 2))],
                0..2,
            ),
            (
                "  a",
                vec![change((0, 1), (0, 2)), change((0, 0), (0, 1))],
                vec![change((0, 0), (0, 2))],
                0..2,
            ),
            (
                "  a\n\n\t\t",
                vec![
                    change((1, 0), (2, 0)),
                    change((2, 0), (2, 1)),
                    change((2, 1), (2, 2)),
                ],
                vec![change((1, 0), (2, 0)), change((2, 0), (2, 2))],
                4..7,
            ),
            (
                "\n  a",
                vec![change((0, 0), (1, 0)), change((1, 0), (1, 2))],
                vec![change((0, 0), (1, 0)), change((1, 0), (1, 2))],
                0..3,
            ),
            // A blank line's indentation and its line break, apart.
            (
                "  \n  a",
                vec![change((0, 0), (0, 2)), change((0, 2), (1, 0))],
                vec![change((0, 0), (1, 0))],
                0..3,
            ),
            (
                "  \n  a",
                vec![change((0, 2), (1, 0)), change((0, 0), (0, 2))],
                vec![change((0, 0), (1, 0))],
                0..3,
            ),
            (
                "  \r\n  a",
                vec![change((0, 0), (0, 1)), change((0, 1), (1, 0))],
                vec![change((0, 0), (1, 0))],
                0..4,
            ),
            (
                "a\n  \n",
                vec![change((1, 0), (1, 2)), change((1, 2), (2, 0))],
                vec![change((1, 0), (2, 0))],
                2..5,
            ),
        ];
        for (virtual_text, pieces, whole, deleted) in cases {
            let split = prepare(virtual_text, pieces).expect(virtual_text);
            let joined = prepare(virtual_text, whole).expect(virtual_text);
            let mut expected = virtual_text.to_string();
            expected.replace_range(deleted, "");
            assert_eq!(split.text, expected, "{virtual_text:?}");
            assert_eq!(split.text, joined.text, "{virtual_text:?}");
            for line in 0..4 {
                for character in 0..4 {
                    for bias in [Bias::Start, Bias::End] {
                        let at = pos(line, character);
                        assert_eq!(
                            split.map.to_virtual(at, bias),
                            joined.map.to_virtual(at, bias),
                            "{virtual_text:?} {at:?}"
                        );
                        assert_eq!(
                            split.map.to_prepared(at, bias),
                            joined.map.to_prepared(at, bias),
                            "{virtual_text:?} {at:?}"
                        );
                    }
                }
            }
        }
        // Overlapping pieces are still refused.
        assert_eq!(
            prepare("  a", vec![change((0, 0), (0, 2)), change((0, 1), (0, 2))]),
            Err(PrepareError::InvalidChange {
                index: 0,
                reason: "content changes overlap"
            })
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
    fn no_deletion_splits_a_crlf() {
        // LSP treats CRLF as one line break: a column between its CR and LF
        // is past the line's content, and defaults back to before the CR.
        // No change leaves a lone CR or LF the map does not count.
        for (virtual_text, start, end, clamped) in [
            ("\r\nfoo", (0, 0), (0, 1), ((0, 0), (0, 0))),
            ("  \r\nfoo", (0, 0), (0, 3), ((0, 0), (0, 2))),
            ("foo\r\n\r\n", (0, 4), (2, 0), ((0, 3), (2, 0))),
            ("a\r\n\r\n  ", (1, 1), (2, 0), ((1, 0), (2, 0))),
        ] {
            let apply = |start: (u32, u32), end: (u32, u32)| {
                let change = json!({"range": {
                    "start": {"line": start.0, "character": start.1},
                    "end": {"line": end.0, "character": end.1}
                }, "newText": ""});
                apply_prepare_result(
                    virtual_text,
                    &VirtualLayout::single(virtual_text),
                    result(json!({"segments": [{"type": "content", "changes": [change]}]})),
                )
            };
            assert_eq!(
                apply(start, end),
                apply(clamped.0, clamped.1),
                "{virtual_text:?} {start:?}-{end:?}"
            );
        }
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
    fn a_dedent_past_its_line_deletes_up_to_the_line_end() {
        // LSP: a character past the line defaults back to its length.
        let virtual_text = "a\n   \nb\n".to_string();
        let layout = VirtualLayout::single(&virtual_text);
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 99}}, "newText": ""}
            ]}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\n\nb\n");
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
                // Past the line: up to its end, then.
                json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 99}}, "newText": ""}),
                "content changes may only delete whitespace",
            ),
            (
                json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}}, "newText": ""}),
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
            std::sync::Arc::try_unwrap(prepared.map).unwrap(),
        )
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
        std::sync::Arc::try_unwrap(prepared.map).unwrap()
    }

    #[test]
    fn a_position_after_an_emptied_gap_lands_after_its_host_text() {
        let (virtual_text, layout) = fenced();
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [{"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "foo()\nbar()\n");
        let map = prepared.map;
        // `bar()` starts P line 1 and V line 4, past the gap's lines.
        assert_eq!(map.to_virtual(pos(1, 0), Bias::Start), pos(4, 0));
        assert_eq!(map.to_prepared(pos(4, 0), Bias::Start), pos(1, 0));
        // The gap's lines are inside it; the content around it is not.
        assert!(map.virtual_position_in_gap(pos(2, 0)));
        assert!(!map.virtual_position_in_gap(pos(0, 2)));
        assert!(!map.virtual_position_in_gap(pos(4, 0)));
    }

    #[test]
    fn a_range_end_at_an_emptied_gap_stops_before_it() {
        let (_, map) = emptied_gap();
        // P `ab|cd`: the end of `b` and the start of `c` share one offset,
        // on either side of the gap's host text.
        assert_eq!(map.to_virtual(pos(0, 2), Bias::End), pos(0, 2));
        assert_eq!(map.to_virtual(pos(0, 2), Bias::Start), pos(0, 6));
        assert_eq!(map.to_virtual(pos(0, 3), Bias::Start), pos(0, 7));
        assert!(map.virtual_position_in_gap(pos(0, 4)));
        // A V position inside the gap maps to where it was in P.
        assert_eq!(map.to_prepared(pos(0, 4), Bias::Start), pos(0, 2));
    }

    #[test]
    fn a_dedented_line_start_maps_past_its_removed_indent() {
        let map = dedent_by_two("  a\n  b");
        assert_eq!(map.to_virtual(pos(1, 0), Bias::Start), pos(1, 2));
        // P's end too.
        assert_eq!(map.to_virtual(pos(1, 1), Bias::End), pos(1, 3));
        // A range ending at that line start covers nothing of the line.
        assert_eq!(map.to_virtual(pos(1, 0), Bias::End), pos(1, 0));
        // A V position inside the removed indent maps to its line's start.
        assert_eq!(map.to_prepared(pos(1, 1), Bias::Start), pos(1, 0));
        assert!(!map.virtual_position_in_removed_lines(pos(1, 1)));
    }

    fn deleting(virtual_text: &str, changes: serde_json::Value) -> PreparedDocument {
        apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            result(json!({"segments": [{"type": "content", "changes": changes}]})),
        )
        .unwrap()
    }

    #[test]
    fn a_position_on_the_first_deleted_closing_line_is_removed() {
        let (_, prepared) = trailing_blank_dedented();
        let map = prepared.map;
        assert!(!map.virtual_position_in_removed_lines(pos(0, 3)));
        for removed in [pos(1, 0), pos(1, 1), pos(2, 0)] {
            assert!(
                map.virtual_position_in_removed_lines(removed),
                "{removed:?}"
            );
        }
    }

    #[test]
    fn closing_lines_deleted_one_by_one_are_removed_from_the_first() {
        let map = deleting(
            "a\n\n\n",
            json!([
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
            ]),
        )
        .map;
        assert!(!map.virtual_position_in_removed_lines(pos(0, 1)));
        assert!(map.virtual_position_in_removed_lines(pos(1, 0)));
        assert!(map.virtual_position_in_removed_lines(pos(3, 0)));
    }

    #[test]
    fn the_end_of_a_deleted_closing_indent_is_removed() {
        let map = deleting(
            "  a\n\n  ",
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 2}}, "newText": ""}
            ]),
        )
        .map;
        for removed in [pos(1, 0), pos(2, 0), pos(2, 2)] {
            assert!(
                map.virtual_position_in_removed_lines(removed),
                "{removed:?}"
            );
        }
    }

    #[test]
    fn an_all_blank_document_starts_after_its_deleted_lines() {
        let prepared = deleting(
            "\n",
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}
            ]),
        );
        assert_eq!(prepared.text, "");
        assert_eq!(prepared.map.to_virtual(pos(0, 0), Bias::Start), pos(1, 0));
    }

    #[test]
    fn an_indent_only_document_starts_after_its_removed_indent() {
        let prepared = deleting(
            "  ",
            json!([
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
            ]),
        );
        assert_eq!(prepared.text, "");
        assert_eq!(prepared.map.to_virtual(pos(0, 0), Bias::Start), pos(0, 2));
    }

    #[test]
    fn a_range_end_before_a_removed_indent_and_an_emptied_gap_stops_before_both() {
        // `a` then `  ${x}foo`: the second line's indent removed, the
        // interpolation emptied.
        let virtual_text = "a\n      foo\n".to_string();
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..8, "${x}".to_string()),
                (SegmentKind::Content, 8..12, String::new()),
            ],
        );
        let prepared = apply_prepare_result(
            &virtual_text,
            &layout,
            result(json!({"segments": [
                {"type": "content", "changes": [
                    {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
                ]},
                {"type": "gap", "content": ""},
                {"type": "content"}
            ]})),
        )
        .unwrap();
        assert_eq!(prepared.text, "a\nfoo\n");
        // The first line taken whole ends where the second starts in V.
        assert_eq!(prepared.map.to_virtual(pos(1, 0), Bias::End), pos(1, 0));
        assert_eq!(prepared.map.to_virtual(pos(1, 0), Bias::Start), pos(1, 6));
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
        let map = prepared.map;
        assert_eq!(map.to_virtual(pos(0, 2), Bias::Start), pos(0, 2));
        assert_eq!(map.to_virtual(pos(0, 1), Bias::Start), pos(0, 1));
        assert_eq!(map.to_prepared(pos(0, 4), Bias::Start), pos(0, 2));
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
        assert_eq!(prepared.map.to_virtual(pos(1, 0), Bias::Start), pos(1, 2));
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
        let map = prepared.map;
        // `é` is P column 7 (f ( 1 , space 𝄞=2) and V column 10.
        assert_eq!(map.to_virtual(pos(0, 7), Bias::Start), pos(0, 10));
        assert_eq!(map.to_prepared(pos(0, 10), Bias::Start), pos(0, 7));
    }
}
