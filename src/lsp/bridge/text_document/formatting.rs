//! `textDocument/formatting` bridge handler. Translates each returned
//! `TextEdit` range from virtual coordinates back to the host document, applying
//! the injection's [`RegionOffset`] (including per-line column for the first line).
//!
//! [`transform_formatting_response_to_host`] is `pub(super)` so
//! [`super::range_formatting`], which shares the same virtual→host hazards,
//! can reuse it; [`super::on_type_formatting`] uses
//! [`transform_on_type_formatting_response_to_host`].
//!
//! # Known limitation: formatting in prefixed regions can drop WHOLE responses
//!
//! [`RegionOffset::column_for_line`] translates positions correctly for both
//! `new()` (single-column) and `with_per_line_offsets()` (blockquote `> `) shapes.
//! But `new_text` of a multi-line edit starts at column 0 of the embedded language,
//! so replacement lines would insert at host column 0 instead of re-applying the
//! `> ` prefix. A response containing any such edit is dropped WHOLE by the
//! shared prefix guard (`text_edit_safe_in_region`) — a formatter
//! answer is one atomic diff, so applying only its safe edits could
//! duplicate or lose content. All-safe responses (single-line edits and
//! zero-width inserts — the common `trimTrailingWhitespace` cases) pass
//! through (a newline-bearing `new_text` in a prefixed region still rejects).
//! Unprefixed regions are exempt from the prefix rules but still subject to
//! region containment and the fence-boundary EOL rule. Re-applying prefixes to `new_text` (which would let these
//! edits APPLY instead of dropping) means rewriting it per embedded newline;
//! deferred because it interacts with `trim_final_newlines` semantics —
//! the lsp_impl concatenated pipeline's `reapply_host_line_prefixes` is the
//! model if demand appears.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use crate::text::PositionMapper;
use crate::text::edge_layout::{EdgeRestore, restore_edges};
use crate::text::position::positions_to_bytes_clamped;
use tower_lsp_server::ls_types::{
    DocumentFormattingParams, FormattingOptions, NumberOrString, Position, Range,
    TextDocumentIdentifier, TextEdit, WorkDoneProgressParams,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    EditMode, apply_text_edits_clamped, restore_joined_layout, translate_virtual_range_to_host,
    translate_virtual_text_edits_to_host,
};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, region_host_end,
    response_has_jsonrpc_error, text_edit_safe_in_region,
};

impl LanguageServerPool {
    /// Send a formatting request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle)
    /// for the full lifecycle, providing formatting-specific request building and
    /// response transformation.
    ///
    /// Returns `Ok(None)` when the downstream server does not advertise
    /// `documentFormattingProvider`.
    ///
    /// `downstream_id_probe`, when provided, receives the allocated downstream
    /// request id as soon as it is known so a caller that drops this future
    /// (the pipeline's per-step timeout) can still cancel the in-flight
    /// request precisely.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_formatting_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        options: FormattingOptions,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
        downstream_id_probe: Option<&std::sync::OnceLock<RequestId>>,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/formatting") {
            return Ok(None);
        }
        let region_end = region_host_end(virtual_content, &offset);
        self.execute_bridge_request_observed(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            None,
            |virtual_uri, request_id| {
                build_formatting_request(virtual_uri, options, request_id, client_progress_token)
            },
            // The transform promotes error responses, missing results, and
            // malformed payloads to `Err` (request failure) — only the
            // no-capability early return above yields `Ok(None)`.
            |response, ctx| {
                transform_formatting_response_to_host(
                    response,
                    ctx.offset,
                    virtual_content,
                    region_end,
                )
                .map(Some)
            },
            downstream_id_probe,
        )
        .await?
    }
}

/// Count the number of lines in `text`, with the LSP convention that a
/// trailing line terminator introduces an extra (empty) line. Like
/// `PositionMapper`, a lone CR ends a line too, and CRLF ends one.
///
/// Returns 1 for the empty string (a single empty line, index 0).
fn count_lines(text: &str) -> u32 {
    // Every LF ends a line; a CR ends one unless an LF follows it.
    let bytes = text.as_bytes();
    let terminators = bytes
        .iter()
        .enumerate()
        .filter(|&(index, &byte)| {
            byte == b'\n' || (byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        })
        .count();
    u32::try_from(terminators)
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1)
}

/// If `pos` is the "synthetic next-line anchor" (column 0 of the line
/// immediately after `last_real_line`), rewrite it to (last_real_line,
/// u32::MAX) — a sentinel the transform later snaps to the region's
/// content-precise host end (see `transform_formatting_response_to_host`).
/// Used to let the canonical insertFinalNewline shape past the past-EOF
/// drop; whether a line break is then added is the region's edge layout's to
/// decide (see `keep_edge_layout`).
///
/// `virtual_line_count` is the synthetic-line index — i.e., the value that
/// would be `last_real_line + 1` for non-empty docs. Passed in to avoid
/// re-deriving it at each callsite and to keep the arithmetic safe under
/// overflow (last_real_line + 1 would wrap at u32::MAX).
fn clamp_synthetic_eof_anchor(
    pos: &mut tower_lsp_server::ls_types::Position,
    last_real_line: u32,
    virtual_line_count: u32,
) {
    if pos.line == virtual_line_count && pos.character == 0 {
        pos.line = last_real_line;
        pos.character = u32::MAX;
    }
}

/// If the range end `pos` lies on any line past `last_real_line`, rewrite it
/// to the same (last_real_line, u32::MAX) end-of-document sentinel. An end is
/// a bound, not a location: like an editor clamping a past-EOF position, read
/// it as the document end. Covers the "end of document" sentinels servers
/// send for a whole-document replacement, e.g. bash-language-server's
/// (2^31-1, 2^31-1).
fn clamp_end_past_eof(
    pos: &mut tower_lsp_server::ls_types::Position,
    last_real_line: u32,
    virtual_line_count: u32,
) {
    if pos.line >= virtual_line_count {
        pos.line = last_real_line;
        pos.character = u32::MAX;
    }
}

/// Keep the region's edge layout through a formatting answer (see
/// [`restore_edges`]). When the answer changes the edges, the formatter's
/// edits away from them pass as sent, and those reaching into an edge become
/// one edit with that edge's restore (see [`AppliedEdits::with_edges`]); an
/// answer that changed only the edges becomes none.
///
/// Edits that do not apply in order (reversed or overlapping) leave the
/// formatted text undefined, so they pass as received, edges and all.
fn keep_edge_layout(
    edits: Vec<TextEdit>,
    virtual_content: &str,
    offset: &RegionOffset,
) -> Vec<TextEdit> {
    // An empty answer, the usual one from on-type formatting, changes nothing.
    if edits.is_empty() || !edits_apply_in_order(&edits) {
        return edits;
    }
    // Edits inside the content cannot reach the edges: skip applying them,
    // which costs a scan per position on long lines.
    if edits_stay_inside_content(virtual_content, &edits) {
        return edits;
    }
    let Some(applied) = AppliedEdits::new(virtual_content, &edits) else {
        return edits;
    };
    let edges = restore_edges(virtual_content, &applied.formatted, offset.opens_mid_line());
    let restored = edges.apply(&applied.formatted);
    if restored == applied.formatted {
        return edits;
    }
    if restored == virtual_content {
        return Vec::new();
    }
    let mapper = PositionMapper::new(virtual_content);
    if let Some(kept) = applied.with_edges(virtual_content, &mapper, &edits, &edges, &restored) {
        return kept;
    }
    // Forward-looking: every offset `replacement_between` maps is a char
    // boundary within `virtual_content`, so a position always exists today.
    // Should one not, the answer passes as received rather than being lost.
    replacement_between(virtual_content, &restored).map_or(edits, |edit| vec![edit])
}

/// [`keep_edge_layout`] for a prepared document: the edge layout of each
/// string `prepared_text` joins (see [`restore_joined_layout`]) is restored,
/// and the answer becomes one replacement of the prepared text, which the
/// prepared map diffs back anyway. Edits that do not apply in order pass as
/// received, for the map to refuse.
fn keep_prepared_edge_layout(
    edits: Vec<TextEdit>,
    prepared_text: &str,
    map: &super::super::protocol::PreparedMap,
    offset: &RegionOffset,
) -> Vec<TextEdit> {
    if edits.is_empty() {
        return edits;
    }
    let Some(formatted) = apply_text_edits_clamped(prepared_text, &edits) else {
        return edits;
    };
    let restored = restore_joined_layout(
        prepared_text,
        &formatted,
        map.emptied_gap_offsets(),
        offset.opens_mid_line(),
    );
    if restored == formatted {
        return edits;
    }
    let end = PositionMapper::new(prepared_text)
        .byte_to_position(prepared_text.len())
        .unwrap_or_default();
    vec![TextEdit {
        range: Range::new(Position::new(0, 0), end),
        new_text: restored,
    }]
}

/// Whether every edit starts after the first character of `text`'s content
/// and ends before its last, so the whitespace around the content, its edge
/// layout, stays as it is. False for a whitespace-only `text`.
fn edits_stay_inside_content(text: &str, edits: &[TextEdit]) -> bool {
    let is_layout = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let Some(first) = text.find(|c| !is_layout(c)) else {
        return false;
    };
    let last_end = text.trim_end_matches(is_layout).len();
    let mapper = PositionMapper::new(text);
    let (Some(first), Some(last_end)) = (
        mapper.byte_to_position(first),
        mapper.byte_to_position(last_end),
    ) else {
        return false;
    };
    let key = |position: Position| (position.line, position.character);
    edits
        .iter()
        .all(|edit| key(edit.range.start) > key(first) && key(edit.range.end) < key(last_end))
}

/// A formatting answer applied to the text it was made for, keeping where
/// each edit landed: its bytes in that text and in the formatted text.
struct AppliedEdits {
    formatted: String,
    /// In text order.
    spans: Vec<AppliedSpan>,
}

struct AppliedSpan {
    /// The edit's index in the answer.
    index: usize,
    start: usize,
    end: usize,
    formatted_start: usize,
    formatted_end: usize,
}

impl AppliedEdits {
    /// Apply `edits` as [`crate::text::edit::apply_text_edits`] does, mapping
    /// their positions in one pass. `None` when two of them overlap once their
    /// positions are clamped to the text.
    fn new(text: &str, edits: &[TextEdit]) -> Option<Self> {
        let positions: Vec<Position> = edits
            .iter()
            .flat_map(|edit| [edit.range.start, edit.range.end])
            .collect();
        let bytes = positions_to_bytes_clamped(text, &positions);
        let mut spans: Vec<AppliedSpan> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
            .map(|(index, &[start, end])| AppliedSpan {
                index,
                start,
                end,
                formatted_start: 0,
                formatted_end: 0,
            })
            .collect();
        spans.sort_by_key(|span| (span.start, span.end));
        let mut formatted = String::with_capacity(text.len());
        let mut cursor = 0;
        for span in &mut spans {
            if span.start < cursor || span.end < span.start {
                return None;
            }
            formatted.push_str(&text[cursor..span.start]);
            span.formatted_start = formatted.len();
            formatted.push_str(&edits[span.index].new_text);
            span.formatted_end = formatted.len();
            cursor = span.end;
        }
        formatted.push_str(&text[cursor..]);
        Some(Self { formatted, spans })
    }

    /// The answer with `edges` restored into `restored`: the edits away from
    /// the edges as sent, and for each changed edge one edit from the text's
    /// start (or to its end) through every edit reaching into that edge.
    /// `None` when one edit reaches into both edges, or should the edits miss
    /// `restored`; one edit then covers the changes.
    fn with_edges(
        &self,
        text: &str,
        mapper: &PositionMapper,
        edits: &[TextEdit],
        edges: &EdgeRestore,
        restored: &str,
    ) -> Option<Vec<TextEdit>> {
        let formatted = self.formatted.as_str();
        // How far the edits before `count` moved the text after them.
        let shift = |count: usize| {
            self.spans[..count]
                .iter()
                .map(|span| {
                    (span.formatted_end - span.formatted_start) as isize
                        - (span.end - span.start) as isize
                })
                .sum::<isize>()
        };

        let mut head = None;
        let mut first_kept = 0;
        if edges.head_end > 0 || !edges.head.is_empty() {
            first_kept = self
                .spans
                .iter()
                .take_while(|span| span.formatted_start < edges.head_end)
                .count();
            let (end, formatted_end) = match first_kept.checked_sub(1).map(|i| &self.spans[i]) {
                Some(span) if span.formatted_end > edges.head_end => (span.end, span.formatted_end),
                _ => (
                    edges.head_end.checked_add_signed(-shift(first_kept))?,
                    edges.head_end,
                ),
            };
            let new_text = [
                edges.head.as_str(),
                &formatted[edges.head_end..formatted_end],
            ]
            .concat();
            head = Some((0, end, new_text));
        }

        let mut tail = None;
        let mut kept_end = self.spans.len();
        if edges.tail_start < formatted.len() || !edges.tail.is_empty() {
            kept_end = self
                .spans
                .iter()
                .position(|span| {
                    span.formatted_end > edges.tail_start
                        || span.formatted_start >= edges.tail_start
                })
                .unwrap_or(self.spans.len());
            let (start, formatted_start) = match self.spans.get(kept_end) {
                Some(span) if span.formatted_start < edges.tail_start => {
                    (span.start, span.formatted_start)
                }
                _ => (
                    edges.tail_start.checked_add_signed(-shift(kept_end))?,
                    edges.tail_start,
                ),
            };
            let new_text = [
                &formatted[formatted_start..edges.tail_start],
                edges.tail.as_str(),
            ]
            .concat();
            tail = Some((start, text.len(), new_text));
        }

        if first_kept > kept_end
            || matches!((&head, &tail), (Some((_, head_end, _)), Some((tail_start, _, _))) if head_end > tail_start)
        {
            return None;
        }
        let middle = &self.spans[first_kept..kept_end];

        // Check the composition in bytes, as positions would take a scan each.
        let mut composed = String::with_capacity(restored.len());
        let mut cursor = 0;
        if let Some((_, end, new_text)) = &head {
            composed.push_str(new_text);
            cursor = *end;
        }
        for span in middle {
            composed.push_str(text.get(cursor..span.start)?);
            composed.push_str(&edits[span.index].new_text);
            cursor = span.end;
        }
        if let Some((start, _, new_text)) = &tail {
            composed.push_str(text.get(cursor..*start)?);
            composed.push_str(new_text);
            cursor = text.len();
        }
        composed.push_str(text.get(cursor..)?);
        if composed != restored {
            return None;
        }

        let mut kept = Vec::with_capacity(kept_end - first_kept + 2);
        if let Some((start, end, new_text)) = head {
            kept.extend(trimmed_edit(text, mapper, start, end, &new_text)?);
        }
        kept.extend(middle.iter().map(|span| edits[span.index].clone()));
        if let Some((start, end, new_text)) = tail {
            kept.extend(trimmed_edit(text, mapper, start, end, &new_text)?);
        }
        Some(kept)
    }
}

/// Whether every range is ordered and none overlaps another.
///
/// Positions compare as sent, after the past-EOF clamps: an edit ending at the
/// (last line, u32::MAX) sentinel reads as overlapping one starting at that
/// line's real end, although they only touch. Such an answer passes as
/// received, as before the edge layout was kept.
fn edits_apply_in_order(edits: &[TextEdit]) -> bool {
    let key = |position: Position| (position.line, position.character);
    let mut ranges: Vec<_> = edits.iter().map(|edit| edit.range).collect();
    // In the order `apply_text_edits` applies them: an insert before a
    // replacement starting where it inserts.
    ranges.sort_by_key(|range| (key(range.start), key(range.end)));
    ranges
        .iter()
        .all(|range| key(range.start) <= key(range.end))
        && ranges
            .windows(2)
            .all(|pair| key(pair[0].end) <= key(pair[1].start))
}

/// One edit turning `original` into `replacement` (which differ), spanning
/// only what differs. `None` when a position cannot be expressed.
fn replacement_between(original: &str, replacement: &str) -> Option<TextEdit> {
    let mapper = PositionMapper::new(original);
    trimmed_edit(original, &mapper, 0, original.len(), replacement)?
}

/// The edit replacing `text[start..end]` with `new_text`, narrowed to what
/// differs: `Some(None)` when nothing does, and `None` when a position cannot
/// be expressed (one between a CR and its LF names no column).
fn trimmed_edit(
    text: &str,
    mapper: &PositionMapper,
    mut start: usize,
    mut end: usize,
    new_text: &str,
) -> Option<Option<TextEdit>> {
    let common_len = |a: &mut dyn Iterator<Item = (char, char)>| {
        a.take_while(|(x, y)| x == y)
            .map(|(x, _)| x.len_utf8())
            .sum::<usize>()
    };
    let prefix = common_len(&mut text[start..end].chars().zip(new_text.chars()));
    let suffix = common_len(
        &mut text[start + prefix..end]
            .chars()
            .rev()
            .zip(new_text[prefix..].chars().rev()),
    );
    if prefix == end - start && prefix == new_text.len() {
        return Some(None);
    }
    let (mut new_start, mut new_end) = (prefix, new_text.len() - suffix);
    start += prefix;
    end -= suffix;
    // Never split a CRLF: a position between its CR and LF names no column.
    // The CR or LF taken back in is unchanged (it is in the common part), so
    // the new text takes it too.
    if new_start > 0 && text[..start].ends_with('\r') && text[start..].starts_with('\n') {
        start -= 1;
        new_start -= 1;
    }
    if new_end < new_text.len() && text[..end].ends_with('\r') && text[end..].starts_with('\n') {
        end += 1;
        new_end += 1;
    }
    // A CRLF split outside the common part cannot be widened over: no edit.
    let splits_crlf = |at: usize| text[..at].ends_with('\r') && text[at..].starts_with('\n');
    if splits_crlf(start) || splits_crlf(end) {
        return None;
    }
    Some(Some(TextEdit {
        range: Range {
            start: mapper.byte_to_position(start)?,
            end: mapper.byte_to_position(end)?,
        },
        new_text: new_text[new_start..new_end].to_string(),
    }))
}

/// Build a JSON-RPC formatting request for a downstream language server.
///
/// Like `documentLink`/`documentSymbol`, formatting carries no position — only
/// the document identifier plus the editor-supplied [`FormattingOptions`]
/// (tab size, insert-spaces, trim trailing whitespace, etc.). The options are
/// forwarded unchanged so each downstream server can honor user preferences.
fn build_formatting_request(
    virtual_uri: &VirtualDocumentUri,
    options: FormattingOptions,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<DocumentFormattingParams> {
    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri.to_lsp_uri(),
        },
        options,
        // Forward the bridge-minted token so the downstream reports `$/progress`
        // against this request's shared aggregator (ls-bridge-client-progress).
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: client_progress_token,
        },
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/formatting", params)
}

/// Translate each `TextEdit` from virtual to host coordinates via `offset`.
///
/// LSP returns `TextEdit[] | null`. A `null` result from a server that handled
/// the request is the authoritative "no changes / already formatted" signal and
/// maps to `Ok(vec![])`. A JSON-RPC error response, a success response with no
/// `result` member (protocol violation), or a `result` that fails to
/// deserialize as `TextEdit[]` is a request **failure** (`Err`): collapsing it
/// into the same value as "no capability" would let a broken formatter pass as
/// "nothing to format" — the fan-in counts `Err`s, which CLI mode maps onto
/// its error exit code and the editor path logs at WARNING.
///
/// Downstream formatters often emit edits past the virtual EOF (e.g. enforce a
/// trailing newline) as if it were a real file; the host bytes immediately past
/// that EOF are the surrounding markdown/string-literal container, so applying
/// such edits would corrupt the closing fence or quotes. An end past EOF is
/// therefore clamped to the end of the last real line, as is a start at the
/// canonical insert-final-newline anchor (column 0 of the synthetic line just
/// past EOF); any edit whose start is still on a line `>= virtual_line_count`
/// is then dropped before translation.
/// `virtual_content` is the text the server formatted: the edits apply to it.
///
/// The region's edge layout is kept (see `keep_edge_layout`).
pub(super) fn transform_formatting_response_to_host(
    response: serde_json::Value,
    offset: &RegionOffset,
    virtual_content: &str,
    region_end: Position,
) -> io::Result<Vec<TextEdit>> {
    transform_formatting_style_response(response, offset, virtual_content, region_end, true)
}

/// [`transform_formatting_response_to_host`] for on-type formatting, whose
/// answer is about the line being typed, leaving the edges as answered: at
/// the end of an unclosed region (a fence at EOF), that line is the region's
/// last, and its indentation is content, not a closing delimiter's.
pub(super) fn transform_on_type_formatting_response_to_host(
    response: serde_json::Value,
    offset: &RegionOffset,
    virtual_content: &str,
    region_end: Position,
) -> io::Result<Vec<TextEdit>> {
    transform_formatting_style_response(response, offset, virtual_content, region_end, false)
}

fn transform_formatting_style_response(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    virtual_content: &str,
    region_end: Position,
    keep_edges: bool,
) -> io::Result<Vec<TextEdit>> {
    let virtual_line_count = count_lines(virtual_content);
    if response_has_jsonrpc_error(&response, "formatting-style request") {
        return Err(io::Error::other(
            "downstream server answered the formatting request with an error response",
        ));
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        return Err(io::Error::other(
            "formatting response carries neither result nor error (protocol violation)",
        ));
    };

    if result.is_null() {
        // Authoritative "no changes / already formatted" — an empty edit list,
        // not a missing result (see function docs).
        return Ok(Vec::new());
    }

    // A non-null `result` that fails to deserialize as `Vec<TextEdit>` means
    // the downstream server returned a malformed `textDocument/formatting`
    // payload (wrong shape, missing fields, etc.) — a request failure; the
    // log keeps the misbehaving downstream diagnosable.
    let mut edits: Vec<TextEdit> = match serde_json::from_value(result) {
        Ok(edits) => edits,
        Err(err) => {
            warn!(target: "kakehashi::bridge", "Failed to deserialize formatting-style result as TextEdit[]: {}", err);
            return Err(io::Error::other(format!(
                "malformed formatting result from downstream server: {err}"
            )));
        }
    };

    // Some formatters emit "insert final newline" as a zero-width edit
    // anchored at column 0 of the synthetic line *after* the last real line
    // (line == virtual_line_count && character == 0). That is one past EOF
    // in line space but cannot corrupt host bytes because the payload is
    // inserted at end-of-content, not over any existing range; the edge
    // layout below then decides whether its line break is kept. A start at
    // that anchor, and any end past EOF, is clamped to the
    // (last_real_line, u32::MAX) sentinel, which is snapped to the region end
    // after translation: such an edit replaces up to the region end and
    // nothing beyond it. Any other start past EOF stays malformed and is
    // dropped below. Skipped when virtual_line_count == 0 (never produced by
    // count_lines, which returns 1 for empty text, but guarded just in case):
    // the drop below then rejects every edit.
    if virtual_line_count > 0 {
        let last_real_line = virtual_line_count - 1;
        for edit in &mut edits {
            clamp_synthetic_eof_anchor(&mut edit.range.start, last_real_line, virtual_line_count);
            clamp_end_past_eof(&mut edit.range.end, last_real_line, virtual_line_count);
        }
    }

    // Drop edits whose start is still past the virtual document's last line
    // after clamping; every end is in bounds by now. Such edits would corrupt
    // host content beyond the injection region after offset translation (see
    // function-level docs).
    // ALL-OR-NOTHING (like the prefix gate below): a formatter answer is one
    // atomic diff, so applying only its in-bounds edits could pair-break a
    // deletion/insertion and duplicate or lose content.
    if !edits
        .iter()
        .all(|edit| edit.range.start.line < virtual_line_count)
    {
        warn!(
            target: "kakehashi::bridge",
            "Dropped a formatting response ({} edit(s)): an edit starts past virtual EOF (line {}) and would corrupt host content beyond the injection region",
            edits.len(),
            virtual_line_count
        );
        return Ok(Vec::new());
    }

    match offset.prepared() {
        // The server formatted the prepared text: its answer goes back
        // through the prepared map, which restores dedented indentation and
        // refuses an answer rewriting host-owned text (a gap). The deleted
        // edge lines stay in the host; the edges kept are the prepared
        // text's, per string it joins. A refused answer fails the request
        // (the CLI reports it), unlike the malformed answers dropped below:
        // the server did its job, and the region is left unformatted only
        // because its result would rewrite the host.
        Some(map) => {
            if keep_edges {
                edits = keep_prepared_edge_layout(edits, virtual_content, map, offset);
            }
            edits = translate_virtual_text_edits_to_host(edits, offset, EditMode::Formatting)
                .ok_or_else(|| {
                    warn!(
                        target: "kakehashi::bridge",
                        "Refused a formatting response: it edits host-owned text of a prepared virtual document"
                    );
                    io::Error::other(
                        "formatting response edits host-owned text of a prepared virtual document",
                    )
                })?;
        }
        None => {
            if keep_edges {
                edits = keep_edge_layout(edits, virtual_content, offset);
            }
            for edit in &mut edits {
                translate_virtual_range_to_host(&mut edit.range, offset);
            }
        }
    }

    // Clamp positions past the end of the last content line into the region:
    // a character past the line length defaults back to the line length, so
    // such a position — the (last line, u32::MAX) sentinel from the EOF
    // clamps, or a server's own large column — means "end of the region's
    // last content line", which IS the region end. Without this, the host
    // line may run on past the region (an inline region's closing quote) and
    // the exact containment check below would drop every edit ending there,
    // such as shfmt's whole-document replacement up to its end-of-document
    // sentinel. Only positions on the region end's own
    // line are snapped: one past the region on a later line could only come
    // from a line count disagreeing with `region_end`, and fails containment
    // (fail-closed).
    for edit in &mut edits {
        for pos in [&mut edit.range.start, &mut edit.range.end] {
            if pos.line == region_end.line && pos.character > region_end.character {
                *pos = region_end;
            }
        }
    }

    // Prefix safety (same guard codeAction/rename/applyEdit apply via the
    // WorkspaceEdit form): the transform translates RANGES but emits newText
    // verbatim, so a multi-line replacement or newline insertion in a
    // prefixed (blockquote) region would strip the `> ` prefixes it overlaps.
    // ALL-OR-NOTHING: a formatting response is one atomic diff — formatters
    // routinely pair a line-deletion with a same-line insertion of the merged
    // text, so dropping only the unsafe half would duplicate or lose content.
    // If any edit is unsafe, drop the whole response (an empty edit list —
    // the document is left untouched, like the null "already formatted"
    // answer). All-zero regions (plain fences) skip the prefix rules but are
    // still containment- and fence-boundary-checked.
    if !edits
        .iter()
        .all(|edit| text_edit_safe_in_region(edit, offset, region_end))
    {
        warn!(
            target: "kakehashi::bridge",
            "Dropped a formatting response ({} edit(s)): an edit is unsafe for the \
             injection region (escapes it, breaks per-line `> ` prefixes, or merges \
             content into the closing fence)",
            edits.len()
        );
        return Ok(Vec::new());
    }

    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    #[test]
    fn transform_drops_the_whole_response_when_any_edit_breaks_prefixes() {
        // Blockquote region (production offsets: per-line `> ` widths plus the
        // trailing boundary-row zero), content host lines 3-4, fence line 5,
        // region end (5, 0). The response pairs a multi-line prefix-breaking
        // replacement with a safe single-line edit — the diff halves of one
        // atomic formatter answer. Applying only the safe half would
        // duplicate/lose content, so the WHOLE response must drop.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end": { "line": 1, "character": 4 } },
                  "newText": "formatted\nlines" },
                { "range": { "start": { "line": 1, "character": 6 },
                             "end": { "line": 1, "character": 8 } },
                  "newText": "safe" }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            "aaaaaaaa\nbbbbbbbb",
            region_end,
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "a response with any prefix-breaking edit must drop whole: {edits:?}"
        );
    }

    #[test]
    fn transform_keeps_an_all_safe_response_in_blockquote_regions() {
        // Same region: single-line, newline-free edits behind the prefix are
        // safe and must pass through (translated), not be caught by the
        // all-or-nothing drop.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "range": { "start": { "line": 1, "character": 0 },
                             "end": { "line": 1, "character": 4 } },
                  "newText": "safe" }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            "aaaaaaaa\nbbbbbbbb",
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "all-safe response passes: {edits:?}");
        assert_eq!(edits[0].new_text, "safe");
        assert_eq!(edits[0].range.start.line, 4, "translated to host line");
    }

    fn default_options() -> FormattingOptions {
        FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..Default::default()
        }
    }

    // ==========================================================================
    // Formatting request tests
    // ==========================================================================

    #[test]
    fn formatting_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_formatting_request(&virtual_uri, default_options(), test_request_id(), None);

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn formatting_request_carries_work_done_token_only_when_present() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");

        let with = build_formatting_request(
            &virtual_uri,
            default_options(),
            test_request_id(),
            Some(NumberOrString::String("cprog-1".to_string())),
        );
        assert_eq!(
            serde_json::to_value(&with).unwrap()["params"]["workDoneToken"],
            "cprog-1"
        );

        let without =
            build_formatting_request(&virtual_uri, default_options(), test_request_id(), None);
        assert!(
            serde_json::to_value(&without).unwrap()["params"]
                .get("workDoneToken")
                .is_none(),
            "None omits the token"
        );
    }

    #[test]
    fn formatting_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_formatting_request(&virtual_uri, default_options(), RequestId::new(7), None);

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7);
        assert_eq!(json["method"], "textDocument/formatting");
        assert!(
            json["params"].get("position").is_none(),
            "Formatting request should not have position parameter"
        );
    }

    #[test]
    fn formatting_request_forwards_options() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let options = FormattingOptions {
            tab_size: 2,
            insert_spaces: false,
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(true),
            trim_final_newlines: Some(false),
            ..Default::default()
        };

        let request = build_formatting_request(&virtual_uri, options, RequestId::new(1), None);

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["options"]["tabSize"], 2);
        assert_eq!(json["params"]["options"]["insertSpaces"], false);
        assert_eq!(json["params"]["options"]["trimTrailingWhitespace"], true);
        assert_eq!(json["params"]["options"]["insertFinalNewline"], true);
        assert_eq!(json["params"]["options"]["trimFinalNewlines"], false);
    }

    // ==========================================================================
    // Formatting response transformation tests
    // ==========================================================================

    /// Virtual text for tests that don't care about boundary behavior: long
    /// enough that no test edit is filtered out, with no line break at
    /// either end.
    const PLAIN_TEXT: &str = "aaaaaaaa\nbbbbbbbb\ncccccccc\ndddddddd\neeeeeeee";

    #[test]
    fn formatting_response_transforms_text_edit_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 4 }
                    },
                    "newText": "    "
                },
                {
                    "range": {
                        "start": { "line": 2, "character": 0 },
                        "end": { "line": 3, "character": 0 }
                    },
                    "newText": ""
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            PLAIN_TEXT,
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].range.start.line, 10);
        assert_eq!(edits[0].range.end.line, 10);
        assert_eq!(edits[0].new_text, "    ");
        assert_eq!(edits[1].range.start.line, 12);
        assert_eq!(edits[1].range.end.line, 13);
        assert_eq!(edits[1].new_text, "");
    }

    #[test]
    fn formatting_response_applies_column_offset_only_to_first_line() {
        // First-line edits get the per-line column offset; later lines do not.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 1 },
                        "end": { "line": 0, "character": 3 }
                    },
                    "newText": "x"
                },
                {
                    "range": {
                        "start": { "line": 1, "character": 5 },
                        "end": { "line": 1, "character": 7 }
                    },
                    "newText": "y"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(5, 4),
            PLAIN_TEXT,
            TEST_REGION_END,
        )
        .unwrap();

        // Line 0 in virtual → line 5 in host, character shifted by column offset 4
        assert_eq!(edits[0].range.start.line, 5);
        assert_eq!(edits[0].range.start.character, 5);
        assert_eq!(edits[0].range.end.character, 7);
        // Line 1 in virtual → line 6 in host, character NOT shifted (column offset
        // only applies to virtual line 0)
        assert_eq!(edits[1].range.start.line, 6);
        assert_eq!(edits[1].range.start.character, 5);
        assert_eq!(edits[1].range.end.character, 7);
    }

    #[rstest]
    #[case::error_response(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::missing_result(json!({"jsonrpc": "2.0", "id": 42}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn formatting_response_is_a_request_failure_for_invalid_response(
        #[case] response: serde_json::Value,
    ) {
        // Error / missing / malformed must be `Err` (request failure) — not
        // the no-capability `None` — so the fan-in counts it and CLI mode
        // can exit non-zero for a broken formatter.
        let transformed = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(5, 0),
            PLAIN_TEXT,
            TEST_REGION_END,
        );
        assert!(transformed.is_err());
    }

    #[test]
    fn formatting_response_null_result_is_authoritative_empty_edit_list() {
        // Per LSP, `null` from a server that handled the request means
        // "no changes / already formatted" — an authoritative answer, not a
        // missing one. It must come back as Ok(vec![]) so callers can tell
        // it apart from a request failure (`Err`) and from the caller-level
        // "no capability" `None`, which the concatenated pipeline's
        // capability fallback depends on (ADR
        // concatenated-formatting-pipeline Decision point 3.2).
        let response = json!({"jsonrpc": "2.0", "id": 42, "result": null});

        let transformed = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(5, 0),
            PLAIN_TEXT,
            TEST_REGION_END,
        )
        .expect("null result is a handled response, not a failure");

        assert_eq!(
            transformed,
            Vec::new(),
            "null result must be an authoritative empty edit list"
        );
    }

    #[test]
    fn formatting_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(5, 0),
            PLAIN_TEXT,
            TEST_REGION_END,
        )
        .unwrap();
        assert!(edits.is_empty());
    }

    #[test]
    fn formatting_response_transformation_saturates_on_overflow() {
        // Use a high but in-bounds line and an `u32::MAX` character to keep
        // the boundary filter happy while still exercising overflow saturation
        // in the line/character translation path.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 1, "character": u32::MAX },
                    "end": { "line": 1, "character": u32::MAX }
                },
                "newText": "boom"
            }]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(u32::MAX - 1, 0),
            "aaaaaaaa\nbbbbbbbb",
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range.start.line,
            u32::MAX,
            "Line + offset overflow should saturate at u32::MAX, not panic"
        );
        assert_eq!(
            edits[0].range.start.character,
            u32::MAX,
            "Character at u32::MAX should remain saturated"
        );
    }

    // ==========================================================================
    // Boundary enforcement tests (regression coverage for #303 review)
    // ==========================================================================

    #[test]
    fn formatting_response_clamps_edits_at_synthetic_eof_anchor() {
        // virtual_line_count = 3 → valid lines are 0, 1, 2. An edit ending at
        // (3, 0) is the synthetic "next line column 0" anchor that formatters
        // emit for insertFinalNewline / preserveFinalNewline. Per LSP position
        // clamping it's equivalent to (2, eol-of-line-2), so clamp the end
        // down to the (2, u32::MAX) sentinel, which the transform snaps to the
        // region end (unbounded here, so the sentinel survives). The edit is
        // kept (not dropped) — previous behavior was overly conservative.
        // It inserts no line break, which the region's edge layout would
        // refuse (see `insert_final_newline_leaves_a_region_as_is`).
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 2, "character": 6 },
                        "end": { "line": 3, "character": 0 }
                    },
                    "newText": ";"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "aaaaaaaa\nbbbbbbbb\ncccccc",
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "synthetic-EOF-anchored edit is kept");
        assert_eq!(edits[0].new_text, ";");
        // start unchanged (still on last real line); end clamped down by one.
        assert_eq!(edits[0].range.start.line, 12);
        assert_eq!(edits[0].range.start.character, 6);
        assert_eq!(edits[0].range.end.line, 12, "end clamped down by one line");
        assert_eq!(edits[0].range.end.character, u32::MAX);
    }

    #[test]
    fn formatting_response_keeps_zero_width_edit_at_virtual_eof() {
        // A zero-width edit anchored at the last column of the last virtual
        // line, the shape of an "insert at EOF". Stays in bounds (end.line ==
        // last valid line index) so it must be preserved.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 2, "character": 6 },
                        "end": { "line": 2, "character": 6 }
                    },
                    "newText": ";"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "aaaaaaaa\nbbbbbbbb\ncccccc",
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "in-bounds zero-width EOF insert is kept");
        assert_eq!(edits[0].new_text, ";");
    }

    #[test]
    fn formatting_response_with_an_out_of_bounds_edit_drops_whole() {
        // Mixed batch: a valid edit on line 0 and a malformed edit whose start
        // lies past EOF. A formatter answer is one atomic diff, so the
        // WHOLE response drops — applying only the valid half could
        // duplicate or lose content.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 4 }
                    },
                    "newText": "    "
                },
                {
                    "range": {
                        "start": { "line": 5, "character": 0 },
                        "end": { "line": 6, "character": 0 }
                    },
                    "newText": "wrong"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "aaaaaaaa\nbbbbbbbb",
            TEST_REGION_END,
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "any out-of-bounds edit must drop the whole response: {edits:?}"
        );
    }

    #[test]
    fn an_insert_at_the_synthetic_next_line_survives_a_realistic_region_end() {
        // Content "local x = 1" (no trailing newline) at host line 3: region
        // end is the exact content end (3, 11), NOT a permissive sentinel. A
        // zero-width insert at the synthetic next-line anchor (the canonical
        // insertFinalNewline shape) must snap to the region end and survive
        // containment.
        let offset = RegionOffset::new(3, 0);
        let region_end = Position {
            line: 3,
            character: 11,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                { "range": { "start": { "line": 1, "character": 0 },
                             "end": { "line": 1, "character": 0 } },
                  "newText": ";" }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &offset, "local x = 1", region_end)
                .unwrap();

        assert_eq!(
            edits.len(),
            1,
            "the anchored insert must survive: {edits:?}"
        );
        assert_eq!(edits[0].range.start, region_end);
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, ";");
    }

    #[rstest]
    #[case::without_a_final_line_break("local x = 1", Position::new(1, 0))]
    #[case::with_a_final_line_break("local x = 1\n", Position::new(2, 0))]
    fn insert_final_newline_leaves_a_region_as_is(
        #[case] virtual_content: &str,
        #[case] anchor: Position,
    ) {
        // The final line break is the host's layout: insertFinalNewline adds
        // none to a region ending without one (its closing delimiter shares
        // the line), and doubles none.
        let offset = RegionOffset::new(3, 0);
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [{ "range": { "start": anchor, "end": anchor }, "newText": "\n" }]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            virtual_content,
            region_host_end(virtual_content, &offset),
        )
        .unwrap();

        assert_eq!(edits, Vec::new());
    }

    // ==========================================================================
    // Edge layout: a region's edges belong to the host
    // ==========================================================================

    /// Format the region of `host` that starts at `offset` and spans
    /// `virtual_content`, and return the host text after the answer applies.
    fn format_host(
        host: &str,
        offset: RegionOffset,
        virtual_content: &str,
        result: serde_json::Value,
    ) -> String {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": result });
        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            virtual_content,
            region_host_end(virtual_content, &offset),
        )
        .unwrap();
        crate::text::edit::apply_text_edits(host, &edits)
    }

    #[test]
    fn a_nix_string_keeps_its_delimiter_lines_when_its_edges_are_stripped() {
        // jsonls formats `''…''` content as a file of its own: it drops the
        // line break after `''` and the final line break with the closing
        // indentation, which would join both delimiters onto the JSON.
        let host = "{\n  a = /* json */ ''\n    {\n      \"a\":   1\n    }\n  '';\n}\n";
        let virtual_content = "\n    {\n      \"a\":   1\n    }\n  ";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 4, "character": 2 } },
              "newText": "{\n    \"a\": 1\n}" }
        ]);

        let formatted = format_host(host, RegionOffset::new(1, 19), virtual_content, result);

        assert_eq!(
            formatted,
            "{\n  a = /* json */ ''\n{\n    \"a\": 1\n}\n  '';\n}\n"
        );
    }

    #[test]
    fn a_nix_string_keeps_its_delimiter_lines_through_a_whole_document_sentinel() {
        // bash-language-server (shfmt) replaces the whole document up to the
        // end-of-document sentinel, ending its text with a single line break.
        let host = "{\n  b = /* bash */ ''\n    if true;then echo hi;fi\n  '';\n}\n";
        let virtual_content = "\n    if true;then echo hi;fi\n  ";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 2147483647, "character": 2147483647 } },
              "newText": "if true; then echo hi; fi\n" }
        ]);

        let formatted = format_host(host, RegionOffset::new(1, 19), virtual_content, result);

        assert_eq!(
            formatted,
            "{\n  b = /* bash */ ''\nif true; then echo hi; fi\n  '';\n}\n"
        );
    }

    #[test]
    fn a_markdown_fence_keeps_its_final_line_break() {
        // jsonls's answer, verbatim: its last edit deletes the final line
        // break, which would merge the closing fence into the JSON.
        let host = "```json\n{\"a\":   1}\n```\n";
        let virtual_content = "{\"a\":   1}\n";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 1 },
                         "end": { "line": 0, "character": 1 } }, "newText": "\n    " },
            { "range": { "start": { "line": 0, "character": 5 },
                         "end": { "line": 0, "character": 8 } }, "newText": " " },
            { "range": { "start": { "line": 0, "character": 9 },
                         "end": { "line": 0, "character": 9 } }, "newText": "\n" },
            { "range": { "start": { "line": 0, "character": 10 },
                         "end": { "line": 1, "character": 0 } }, "newText": "" }
        ]);

        let formatted = format_host(host, RegionOffset::new(1, 0), virtual_content, result);

        assert_eq!(formatted, "```json\n{\n    \"a\": 1\n}\n```\n");
    }

    #[test]
    fn a_markdown_fence_lets_the_formatter_remove_a_leading_blank_line() {
        // A fence's content opens at a line start, so its leading line break
        // is a blank line of content, not the host's layout.
        let host = "```sh\n\nif x; then y; fi\n```\n";
        let virtual_content = "\nif x; then y; fi\n";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 2147483647, "character": 2147483647 } },
              "newText": "if x; then y; fi\n" }
        ]);

        let formatted = format_host(host, RegionOffset::new(1, 0), virtual_content, result);

        assert_eq!(formatted, "```sh\nif x; then y; fi\n```\n");
    }

    #[test]
    fn a_blockquoted_region_keeps_its_formatter_edits_apart() {
        // Each trailing-space deletion stays on its line behind the `> `
        // prefix. Restoring the closing indentation the formatter trimmed
        // must not merge them into one edit spanning a prefix, which the
        // prefix guard would drop with the whole answer.
        let host = "> ```lua\n> a  \n> b  \n>   ";
        let virtual_content = "a  \nb  \n  ";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 1 },
                         "end": { "line": 0, "character": 3 } }, "newText": "" },
            { "range": { "start": { "line": 1, "character": 1 },
                         "end": { "line": 1, "character": 3 } }, "newText": "" },
            { "range": { "start": { "line": 2, "character": 0 },
                         "end": { "line": 2, "character": 2 } }, "newText": "" }
        ]);

        let formatted = format_host(
            host,
            RegionOffset::with_per_line_offsets(1, vec![2, 2, 2]),
            virtual_content,
            result,
        );

        assert_eq!(formatted, "> ```lua\n> a\n> b\n>   ");
    }

    #[test]
    fn a_blockquoted_region_keeps_edits_that_swap_lines() {
        // Two single-line edits swap the lines; a diff of the texts would
        // align across them and span a `> ` prefix. The formatter's own
        // edits away from the edges pass as sent.
        let host = "> ```lua\n> a\n> b\n>   ";
        let virtual_content = "a\nb\n  ";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 0, "character": 1 } }, "newText": "b" },
            { "range": { "start": { "line": 1, "character": 0 },
                         "end": { "line": 1, "character": 1 } }, "newText": "a" },
            { "range": { "start": { "line": 2, "character": 0 },
                         "end": { "line": 2, "character": 2 } }, "newText": "" }
        ]);

        let formatted = format_host(
            host,
            RegionOffset::with_per_line_offsets(1, vec![2, 2, 2]),
            virtual_content,
            result,
        );

        assert_eq!(formatted, "> ```lua\n> b\n> a\n>   ");
    }

    #[test]
    fn a_nix_string_keeps_the_edits_jsonls_made_away_from_its_edges() {
        // jsonls's answer, verbatim: its first and last edits take the
        // string's edges, the three between reformat the JSON.
        let virtual_content = "\n    {\n      \"name\":   \"x\"\n    }\n  ";
        let host = "{\n  a = /* json */ ''\n    {\n      \"name\":   \"x\"\n    }\n  '';\n}\n";
        let middle = json!([
            { "range": { "start": { "line": 1, "character": 5 },
                         "end": { "line": 2, "character": 6 } }, "newText": "\n    " },
            { "range": { "start": { "line": 2, "character": 13 },
                         "end": { "line": 2, "character": 16 } }, "newText": " " },
            { "range": { "start": { "line": 2, "character": 19 },
                         "end": { "line": 3, "character": 4 } }, "newText": "\n" }
        ]);
        let mut result = middle.as_array().unwrap().clone();
        result.insert(
            0,
            json!({ "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 1, "character": 4 } }, "newText": "" }),
        );
        result.push(json!({ "range": { "start": { "line": 3, "character": 5 },
                                      "end": { "line": 4, "character": 2 } }, "newText": "" }));
        let offset = RegionOffset::new(1, 19);
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": result });

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            virtual_content,
            region_host_end(virtual_content, &offset),
        )
        .unwrap();

        assert_eq!(
            crate::text::edit::apply_text_edits(host, &edits),
            "{\n  a = /* json */ ''\n{\n    \"name\": \"x\"\n}\n  '';\n}\n"
        );
        let new_texts: Vec<&str> = edits.iter().map(|edit| edit.new_text.as_str()).collect();
        assert!(
            new_texts.windows(3).any(|w| w == ["\n    ", " ", "\n"]),
            "the edits away from the edges pass as sent: {edits:?}"
        );
    }

    #[test]
    fn a_blockquoted_region_keeps_its_trimmed_blank_lines_apart() {
        // Each blank line's spaces go in an edit of its own; only the last,
        // the closing indentation, is the host's to keep.
        let host = "> ```lua\n> a\n>   \n>   \n>   ";
        let virtual_content = "a\n  \n  \n  ";
        let result = json!([
            { "range": { "start": { "line": 1, "character": 0 },
                         "end": { "line": 1, "character": 2 } }, "newText": "" },
            { "range": { "start": { "line": 2, "character": 0 },
                         "end": { "line": 2, "character": 2 } }, "newText": "" },
            { "range": { "start": { "line": 3, "character": 0 },
                         "end": { "line": 3, "character": 2 } }, "newText": "" }
        ]);

        let formatted = format_host(
            host,
            RegionOffset::with_per_line_offsets(1, vec![2, 2, 2, 2]),
            virtual_content,
            result,
        );

        assert_eq!(formatted, "> ```lua\n> a\n> \n> \n>   ");
    }

    #[test]
    fn a_blank_line_removed_next_to_a_crlf_keeps_the_final_line_break() {
        // The formatter removes the lone-CR blank line; the restored tail
        // shares only the CR of a CRLF with the formatted one, and an edit
        // must not end between them.
        let host = "x\r\n\r  ";
        let result = json!([
            { "range": { "start": { "line": 1, "character": 0 },
                         "end": { "line": 2, "character": 0 } }, "newText": "" }
        ]);

        let formatted = format_host(host, RegionOffset::new(0, 0), host, result);

        assert_eq!(formatted, "x\r  ");
    }

    #[test]
    fn on_type_formatting_reindents_the_last_line_of_an_unclosed_fence() {
        // An unclosed fence at EOF ends on the line being typed: its
        // indentation is content, not a closing delimiter's.
        let virtual_content = "if (true) {\n  ";
        let offset = RegionOffset::new(1, 0);
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [
            { "range": { "start": { "line": 1, "character": 0 },
                         "end": { "line": 1, "character": 2 } }, "newText": "    " }
        ]});

        let edits = transform_on_type_formatting_response_to_host(
            response,
            &offset,
            virtual_content,
            region_host_end(virtual_content, &offset),
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].new_text, "    ");
    }

    #[test]
    fn a_replacement_listed_before_an_insert_at_its_start_keeps_the_edges() {
        // The pair applies in order (the insert, then the replacement), so
        // the edges are kept: the line break after `''` survives.
        let host = "{\n  b = /* bash */ ''\n    x\n  '';\n}\n";
        let virtual_content = "\n    x\n  ";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 1, "character": 4 } }, "newText": "" },
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 0, "character": 0 } }, "newText": "y" }
        ]);

        let formatted = format_host(host, RegionOffset::new(1, 19), virtual_content, result);

        assert_eq!(formatted, "{\n  b = /* bash */ ''\nyx\n  '';\n}\n");
    }

    #[test]
    fn an_empty_nix_string_keeps_its_delimiter_lines() {
        // Its whole text is the host's layout: deleting it would join `''`
        // and `'';`.
        let host = "let x = ''\n  '';\n";
        let virtual_content = "\n  ";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 1, "character": 2 } }, "newText": "" }
        ]);

        let formatted = format_host(host, RegionOffset::new(0, 10), virtual_content, result);

        assert_eq!(formatted, host);
    }

    #[test]
    fn a_region_without_a_final_line_break_gains_none() {
        // This Nix string's value ends without a line break; adding one, as
        // shfmt does, would change the value and move the closing delimiter.
        let host = "{\n  c = /* bash */ ''\n    echo  hi'';\n}\n";
        let virtual_content = "\n    echo  hi";
        let result = json!([
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 2147483647, "character": 2147483647 } },
              "newText": "echo hi\n" }
        ]);

        let formatted = format_host(host, RegionOffset::new(1, 19), virtual_content, result);

        assert_eq!(formatted, "{\n  c = /* bash */ ''\necho hi'';\n}\n");
    }

    /// A range as ((start line, start character), (end line, end character)).
    type Span = ((u32, u32), (u32, u32));

    #[rstest]
    // The common suffix starts at the first line break's LF; ending the edit
    // after its CR would name a position inside a line break.
    #[case::suffix_never_splits_a_crlf("x\r\ny\r\n", "x\ny\r\n", ((0, 1), (1, 0)), "\n")]
    // The common prefix runs through a CR whose LF the replacement lacks.
    #[case::prefix_never_splits_a_crlf("x\r\ny", "x\ry", ((0, 1), (1, 0)), "\r")]
    #[case::utf16_columns("é\u{1F600}a", "é\u{1F600}b", ((0, 3), (0, 4)), "b")]
    fn replacement_between_spans_only_what_differs(
        #[case] original: &str,
        #[case] replacement: &str,
        #[case] range: Span,
        #[case] new_text: &str,
    ) {
        let ((start_line, start_character), (end_line, end_character)) = range;

        let edit = replacement_between(original, replacement).unwrap();

        assert_eq!(edit.range.start, Position::new(start_line, start_character));
        assert_eq!(edit.range.end, Position::new(end_line, end_character));
        assert_eq!(edit.new_text, new_text);
        assert_eq!(
            crate::text::edit::apply_text_edits(original, &[edit]),
            replacement
        );
    }

    #[rstest]
    #[case::inside(&[((1, 2), (1, 3))], true)]
    #[case::at_the_first_content_character(&[((1, 1), (1, 2))], false)]
    #[case::at_the_last_content_character(&[((1, 2), (1, 5))], false)]
    #[case::in_the_leading_layout(&[((0, 0), (0, 0))], false)]
    #[case::at_the_eof_sentinel(&[((1, 2), (1, u32::MAX))], false)]
    fn edits_inside_content_leave_the_edges_alone(#[case] ranges: &[Span], #[case] expected: bool) {
        let text = "\n abcd\n  ";
        let edits: Vec<TextEdit> = ranges
            .iter()
            .map(|&((sl, sc), (el, ec))| TextEdit {
                range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
                new_text: "\n\n".to_string(),
            })
            .collect();

        assert_eq!(edits_stay_inside_content(text, &edits), expected);
    }

    #[rstest]
    #[case::ordered(&[((0, 0), (0, 1)), ((0, 1), (0, 2))], true)]
    #[case::listed_out_of_order(&[((1, 0), (1, 1)), ((0, 0), (0, 1))], true)]
    #[case::inserts_at_one_position(&[((0, 1), (0, 1)), ((0, 1), (0, 1))], true)]
    // Applied as `apply_text_edits` does: the insert first, then the
    // replacement starting where it inserts.
    #[case::replacement_listed_before_an_insert_at_its_start(&[((0, 0), (0, 2)), ((0, 0), (0, 0))], true)]
    #[case::reversed(&[((0, 2), (0, 1))], false)]
    #[case::overlapping(&[((0, 0), (0, 2)), ((0, 1), (0, 3))], false)]
    fn edits_apply_in_order_rejects_reversed_and_overlapping_ranges(
        #[case] ranges: &[Span],
        #[case] expected: bool,
    ) {
        let edits: Vec<TextEdit> = ranges
            .iter()
            .map(|&((sl, sc), (el, ec))| TextEdit {
                range: Range::new(Position::new(sl, sc), Position::new(el, ec)),
                new_text: String::new(),
            })
            .collect();

        assert_eq!(edits_apply_in_order(&edits), expected);
    }

    #[test]
    fn edits_that_overlap_pass_through_unchanged() {
        // Overlapping edits apply in no defined order, so the formatted text,
        // and the edges with it, are unknown: the edits pass as received.
        let virtual_content = "\n    x\n  ";
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 1, "character": 5 } }, "newText": "x" },
            { "range": { "start": { "line": 1, "character": 0 },
                         "end": { "line": 2, "character": 2 } }, "newText": "y" }
        ]});
        let offset = RegionOffset::new(1, 19);

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            virtual_content,
            region_host_end(virtual_content, &offset),
        )
        .unwrap();

        assert_eq!(
            edits
                .iter()
                .map(|edit| edit.new_text.as_str())
                .collect::<Vec<_>>(),
            ["x", "y"]
        );
    }

    /// A Nix `''` string's JSON at host line 0, column 5, dedented by the
    /// prepare peer: its virtual and host text, and the offset with the map.
    fn prepared_nix_json() -> (&'static str, RegionOffset) {
        use super::super::super::protocol::{VirtualLayout, apply_prepare_result};
        let virtual_content = "\n    {\"a\":1}\n  ";
        let result = serde_json::from_value(json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}, "newText": ""}
        ]}]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_content,
            &VirtualLayout::single(virtual_content),
            Some(result),
        )
        .unwrap();
        assert_eq!(prepared.text, "\n{\"a\":1}\n  ");
        (
            virtual_content,
            RegionOffset::new(0, 5).with_prepared(prepared.map),
        )
    }

    /// `virtual_content` (a region at host line 0, column 5) with host
    /// `edits` applied.
    fn apply_host_edits(virtual_content: &str, edits: &[TextEdit]) -> String {
        let unprepared = RegionOffset::new(0, 5);
        let virtual_edits: Vec<TextEdit> = edits
            .iter()
            .map(|edit| {
                let mut range = edit.range;
                super::super::super::protocol::translate_host_range_to_virtual(
                    &mut range,
                    &unprepared,
                );
                TextEdit {
                    range,
                    new_text: edit.new_text.clone(),
                }
            })
            .collect();
        crate::text::edit::apply_text_edits(virtual_content, &virtual_edits)
    }

    #[test]
    fn a_prepared_region_keeps_its_edges_through_a_whole_document_answer() {
        // jsonls drops the line break after `''` and the closing indentation
        // of the prepared text it formatted.
        let (virtual_content, offset) = prepared_nix_json();
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [
            { "range": { "start": { "line": 0, "character": 0 },
                         "end": { "line": 2, "character": 2 } },
              "newText": "{\n  \"a\": 1\n}" }
        ]});

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            "\n{\"a\":1}\n  ",
            region_host_end(virtual_content, &offset.unprepared()),
        )
        .unwrap();

        assert_eq!(
            apply_host_edits(virtual_content, &edits),
            "\n    {\n      \"a\": 1\n    }\n  "
        );
    }

    #[test]
    fn an_end_past_eof_over_lone_cr_content_ends_at_the_region_end() {
        // A lone CR ends an LSP line, so "a\rb" has two lines. Counting only
        // LF would leave the clamped end on line 0, short of the region end,
        // and the whole-document replacement would keep the trailing "b".
        let virtual_content = "a\rb";
        let offset = RegionOffset::new(10, 0);
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end": { "line": 2147483647, "character": 2147483647 } },
                  "newText": "A\rB" }
            ]
        });
        let region_end = region_host_end(virtual_content, &offset);

        let edits =
            transform_formatting_response_to_host(response, &offset, virtual_content, region_end)
                .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(10, 0));
        assert_eq!(edits[0].range.end, Position::new(11, 1));
    }

    #[rstest]
    #[case::empty("", 1)]
    #[case::single_line("abc", 1)]
    #[case::two_lines("abc\ndef", 2)]
    #[case::trailing_newline("abc\n", 2)]
    #[case::two_trailing_newlines("abc\n\n", 3)]
    #[case::only_newline("\n", 2)]
    #[case::crlf("abc\r\ndef", 2)]
    #[case::lone_cr("abc\rdef", 2)]
    #[case::trailing_lone_cr("abc\r", 2)]
    #[case::mixed_terminators("a\rb\r\nc\nd", 4)]
    fn count_lines_matches_lsp_line_model(#[case] input: &str, #[case] expected: u32) {
        assert_eq!(count_lines(input), expected);
    }

    // ==========================================================================
    // "Insert final newline" canonical shape (review MINOR follow-up)
    // ==========================================================================
    //
    // Formatters commonly emit the trailing-newline insertion as a zero-width
    // edit anchored at column 0 of the synthetic line *after* the last real
    // line, i.e., end.line == virtual_line_count && end.character == 0. The
    // boundary guard would drop these as "past EOF", even though they are
    // structurally safe — they insert at the very end of the virtual content
    // without overwriting any host bytes. Treat them as inserts at the last
    // column of the last real line: the (last line, u32::MAX) sentinel, which
    // the transform snaps to the region end (unbounded in these tests, so the
    // sentinel survives). These tests insert no line break, which the edge
    // layout decides on (`insert_final_newline_leaves_a_region_as_is`).

    #[test]
    fn formatting_response_clamps_zero_width_insert_on_synthetic_eof_line() {
        // virtual_line_count = 1 (e.g., "foo" with no trailing newline) →
        // formatter emits (1,0)..(1,0). Must be clamped to a zero-width
        // insert on line 0 rather than dropped. The insert is no line break,
        // which the region's edge layout would refuse.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 1, "character": 0 },
                        "end":   { "line": 1, "character": 0 }
                    },
                    "newText": ";"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "foo",
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "the synthetic-EOF anchored insert is kept");
        assert_eq!(edits[0].new_text, ";");
        // After clamping virtual (1,0)..(1,0) → (0, u32::MAX)..(0, u32::MAX),
        // then translation adds the region's line offset (10).
        assert_eq!(edits[0].range.start.line, 10);
        assert_eq!(edits[0].range.end.line, 10);
        assert_eq!(
            edits[0].range.start.character,
            u32::MAX,
            "u32::MAX signals 'end of line' per LSP position clamping"
        );
        assert_eq!(edits[0].range.end.character, u32::MAX);
    }

    #[test]
    fn formatting_response_clamps_replacement_crossing_synthetic_eof_boundary() {
        // virtual_line_count = 2 → valid lines are 0 and 1. Formatter emits
        // (1, 3)..(2, 0) → "" — i.e., "replace the implicit empty trailing
        // line with nothing". end.line=2 is the synthetic next-line anchor;
        // only `end` needs clamping while `start` is already in-bounds.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 1, "character": 3 },
                        "end":   { "line": 2, "character": 0 }
                    },
                    "newText": ""
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(0, 0),
            "aaa\nbbb",
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "boundary-crossing replacement kept");
        assert_eq!(edits[0].range.start.line, 1);
        assert_eq!(edits[0].range.start.character, 3);
        assert_eq!(edits[0].range.end.line, 1, "end clamped down by one line");
        assert_eq!(edits[0].range.end.character, u32::MAX);
    }

    #[test]
    fn formatting_response_drops_edit_with_out_of_bounds_start_line() {
        // Malformed edit shape (e.g., from a buggy or hostile formatter):
        // `end.line` is in bounds but `start.line` overshoots EOF. The
        // previous filter only checked `end.line`, so this edit slipped
        // through and `translate_virtual_range_to_host` saturating-added
        // the host offset, landing on real host bytes outside the injection.
        // Guard against it explicitly.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 5, "character": 0 },
                        "end":   { "line": 1, "character": 0 }
                    },
                    "newText": "wrong"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(0, 0),
            "aaaaaaaa\nbbbbbbbb",
            TEST_REGION_END,
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "edit whose start.line is past virtual EOF must be dropped, \
             even when end.line is in bounds"
        );
    }

    #[test]
    fn formatting_response_ends_an_edit_ending_past_eof_at_the_region_end() {
        // An end past EOF is a bound, not a location: like an editor, read it
        // as the document end. A partial edit from the middle of the document
        // replaces up to the region end and nothing beyond it.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 1, "character": 0 },
                        "end":   { "line": 5, "character": 0 }
                    },
                    "newText": "b;"
                }
            ]
        });
        let region_end = Position::new(11, 3);

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "aaa\nbbb",
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(11, 0));
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, "b;");
    }

    #[test]
    fn formatting_response_ends_an_end_one_line_past_eof_mid_line_at_the_region_end() {
        // Only column 0 of the line just past EOF is the insertFinalNewline
        // anchor; a later column on that line is past EOF all the same.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 1, "character": 1 },
                        "end":   { "line": 2, "character": 5 }
                    },
                    "newText": ""
                }
            ]
        });
        let region_end = Position::new(11, 3);

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "aaa\nbbb",
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(11, 1));
        assert_eq!(edits[0].range.end, region_end);
    }

    #[test]
    fn formatting_response_ends_an_end_past_the_last_line_length_at_the_region_end() {
        // A character past the line length defaults back to the line length,
        // so a column past the last line's end is the document end too. In an
        // inline region the host line runs on past it (here the closing
        // quote), so the end must stop at the region end, not the line end.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end":   { "line": 0, "character": 2147483647 }
                    },
                    "newText": "x = 1"
                }
            ]
        });
        let region_end = Position::new(4, 12);

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(4, 7),
            "x  =1",
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(4, 7));
        assert_eq!(edits[0].range.end, region_end);
    }

    #[test]
    fn formatting_response_ends_an_end_of_document_sentinel_at_the_region_end() {
        // bash-language-server (shfmt) answers with one edit from the
        // document start to an "end of document" sentinel, (2^31-1, 2^31-1).
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end":   { "line": 2147483647, "character": 2147483647 }
                    },
                    "newText": "if true; then\n  echo\nfi\n"
                }
            ]
        });
        let region_end = Position::new(13, 0);

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            "if true;then\necho\nfi\n",
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(10, 0));
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, "if true; then\n  echo\nfi\n");
    }

    #[test]
    fn formatting_response_drops_edit_starting_one_line_past_eof_mid_line() {
        // Only column 0 of the line just past EOF is the insertFinalNewline
        // anchor; a start later on that line names no document location.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 2, "character": 3 },
                        "end":   { "line": 2, "character": 3 }
                    },
                    "newText": "wrong"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(0, 0),
            "aaaaaaaa\nbbbbbbbb",
            TEST_REGION_END,
        )
        .unwrap();

        assert!(edits.is_empty(), "{edits:?}");
    }

    #[test]
    fn formatting_response_drops_edit_starting_two_or_more_lines_past_eof() {
        // Only the end is a bound. A start two lines past EOF names no
        // location in the document and must be dropped to protect host
        // content (one line past, column 0, is the insertFinalNewline anchor).
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 3, "character": 0 },
                        "end":   { "line": 5, "character": 0 }
                    },
                    "newText": "wrong"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(0, 0),
            "aaaaaaaa\nbbbbbbbb",
            TEST_REGION_END,
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "edits starting more than one line past EOF must still be dropped"
        );
    }
}
