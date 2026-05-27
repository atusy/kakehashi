//! Formatting request handling for bridge connections.
//!
//! This module provides `textDocument/formatting` functionality for downstream
//! language servers, translating each returned `TextEdit` range from virtual
//! document coordinates back to the host document.
//!
//! Like `documentLink` and `documentSymbol`, formatting operates on an entire
//! document — there is no cursor position. The injection region's
//! [`RegionOffset`] is applied to every edit range, including the per-line
//! column adjustment for the first line of the region.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.
//!
//! # Multi-line edit limitation (host indentation)
//!
//! [`translate_virtual_range_to_host`] adds the per-line column offset only
//! to virtual line 0. For lines >0 of a returned [`TextEdit`], host coords
//! land at column 0. This is correct for *positions* (the virtual line model
//! starts at column 0 for every line after the first), but it is **not**
//! sufficient for the `new_text` payload of a multi-line edit inside an
//! indented injection (e.g., an indented markdown code fence). The text
//! itself still starts at column 0 of the embedded language, so when the
//! formatter rewraps a function body the new lines insert at the host's
//! column 0 instead of the host's indentation column.
//!
//! Single-line edits and zero-width inserts (the common case for
//! `trimTrailingWhitespace` / `insertFinalNewline`) are unaffected. A full
//! fix requires rewriting `new_text` to prepend the host indentation after
//! every embedded newline; that is left as future work and tracked
//! separately because it interacts with `trim_final_newlines` semantics.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    DocumentFormattingParams, FormattingOptions, TextDocumentIdentifier, TextEdit,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri};

impl LanguageServerPool {
    /// Send a formatting request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle)
    /// for the full lifecycle, providing formatting-specific request building and
    /// response transformation.
    ///
    /// Returns `Ok(None)` when the downstream server does not advertise
    /// `documentFormattingProvider`.
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
    ) -> io::Result<Option<Vec<TextEdit>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/formatting") {
            return Ok(None);
        }
        let virtual_line_count = count_lines(virtual_content);
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| build_formatting_request(virtual_uri, options, request_id),
            |response, ctx| {
                transform_formatting_response_to_host(response, ctx.offset, virtual_line_count)
            },
        )
        .await
    }
}

/// Count the number of lines in `text`, with the LSP convention that a
/// trailing newline introduces an extra (empty) line.
///
/// Returns 1 for the empty string (a single empty line, index 0).
fn count_lines(text: &str) -> u32 {
    // `matches('\n').count() + 1` gives the number of line "buckets" in the
    // split — exactly what the LSP position model expects.
    u32::try_from(text.matches('\n').count())
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1)
}

/// If `pos` is the "synthetic next-line anchor" (column 0 of the line
/// immediately after `last_real_line`), rewrite it to (last_real_line,
/// u32::MAX) so the editor's standard end-of-line clamping snaps it to the
/// real last column. Used to accept the canonical insertFinalNewline shape
/// without dropping it as past-EOF.
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
) -> JsonRpcRequest<DocumentFormattingParams> {
    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri.to_lsp_uri(),
        },
        options,
        work_done_progress_params: Default::default(),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/formatting", params)
}

/// Transform a formatting response from virtual to host document coordinates.
///
/// Each returned `TextEdit` references a range inside the virtual document; we
/// apply the region offset so the edit lines up with the corresponding bytes
/// in the host. Per LSP, formatting returns `TextEdit[] | null`, so a `null`
/// or missing `result` collapses to `None`.
///
/// # Boundary enforcement
///
/// Downstream formatters typically operate as if the virtual document were a
/// real file and emit edits at its EOF (e.g., enforcing a trailing newline).
/// When the virtual content sits inside a larger host (a markdown code fence,
/// a string literal, …), the bytes immediately after the virtual EOF belong
/// to the host. An edit whose range extends past the virtual line count
/// would translate to a host position that overwrites those bytes — corrupting
/// the closing ` ``` `, surrounding markdown, or string-literal quotes.
///
/// To prevent that, edits whose `range.end.line` is past the last virtual
/// line are dropped before translation. `virtual_line_count` is the number of
/// LSP lines in the virtual document (1 for an empty document, computed via
/// [`count_lines`]).
fn transform_formatting_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    virtual_line_count: u32,
) -> Option<Vec<TextEdit>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/formatting: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    let mut edits: Vec<TextEdit> = serde_json::from_value(result).ok()?;

    // Some formatters emit "insert final newline" as a zero-width edit
    // anchored at column 0 of the synthetic line *after* the last real line
    // (end.line == virtual_line_count && end.character == 0). That is one
    // past EOF in line space but cannot corrupt host bytes because the
    // payload is inserted at end-of-content, not over any existing range.
    // Clamp those anchors down to (last_real_line, u32::MAX) so the editor's
    // standard past-end-of-line clamping snaps them to the line's actual
    // length. Skipped for empty virtual docs (virtual_line_count == 0 is
    // never produced by count_lines, but guard against it just in case).
    if virtual_line_count > 0 {
        let last_real_line = virtual_line_count - 1;
        for edit in &mut edits {
            clamp_synthetic_eof_anchor(&mut edit.range.start, last_real_line, virtual_line_count);
            clamp_synthetic_eof_anchor(&mut edit.range.end, last_real_line, virtual_line_count);
        }
    }

    // Drop edits whose start OR end position is still past the virtual
    // document's last line after clamping. Such edits would corrupt host
    // content beyond the injection region after offset translation (see
    // function-level docs). Checking both endpoints handles both the common
    // "formatter overshoots EOF" case and the malformed `start > virtual_eof`
    // shape that would otherwise sneak through with an in-bounds `end`.
    let before = edits.len();
    edits.retain(|edit| {
        edit.range.start.line < virtual_line_count && edit.range.end.line < virtual_line_count
    });
    // `retain` never grows the vec so this can't underflow today, but
    // saturating_sub keeps the count valid if the surrounding logic ever
    // changes (e.g., a new clamping step that re-inserts edits).
    let dropped = before.saturating_sub(edits.len());
    if dropped > 0 {
        warn!(
            target: "kakehashi::bridge",
            "Dropped {} formatting edit(s) extending past virtual EOF (line {}); would corrupt host content beyond injection region",
            dropped,
            virtual_line_count
        );
    }

    for edit in &mut edits {
        translate_virtual_range_to_host(&mut edit.range, offset);
    }

    Some(edits)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

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
        let request = build_formatting_request(&virtual_uri, default_options(), test_request_id());

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn formatting_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_formatting_request(&virtual_uri, default_options(), RequestId::new(7));

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

        let request = build_formatting_request(&virtual_uri, options, RequestId::new(1));

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

    /// Permissive line count used by tests that don't care about boundary
    /// behavior — chosen large enough that no test edit is filtered out.
    const UNBOUNDED: u32 = u32::MAX;

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

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(10, 0), UNBOUNDED)
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

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(5, 4), UNBOUNDED)
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
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn formatting_response_returns_none_for_invalid_response(#[case] response: serde_json::Value) {
        let transformed =
            transform_formatting_response_to_host(response, &RegionOffset::new(5, 0), UNBOUNDED);
        assert!(transformed.is_none());
    }

    #[test]
    fn formatting_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(5, 0), UNBOUNDED)
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

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(u32::MAX - 1, 0), 2)
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
        // down to (2, u32::MAX) and let the editor snap it. The edit is kept
        // (not dropped) — previous behavior was overly conservative.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 2, "character": 6 },
                        "end": { "line": 3, "character": 0 }
                    },
                    "newText": "\n"
                }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(10, 0), 3).unwrap();

        assert_eq!(edits.len(), 1, "synthetic-EOF-anchored edit is kept");
        assert_eq!(edits[0].new_text, "\n");
        // start unchanged (still on last real line); end clamped down by one.
        assert_eq!(edits[0].range.start.line, 12);
        assert_eq!(edits[0].range.start.character, 6);
        assert_eq!(edits[0].range.end.line, 12, "end clamped down by one line");
        assert_eq!(edits[0].range.end.character, u32::MAX);
    }

    #[test]
    fn formatting_response_keeps_zero_width_edit_at_virtual_eof() {
        // The common "insert trailing newline at EOF" pattern: zero-width edit
        // anchored at the last column of the last virtual line. Stays in
        // bounds (end.line == last valid line index) so it must be preserved.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 2, "character": 6 },
                        "end": { "line": 2, "character": 6 }
                    },
                    "newText": "\n"
                }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(10, 0), 3).unwrap();

        assert_eq!(edits.len(), 1, "in-bounds zero-width EOF insert is kept");
        assert_eq!(edits[0].new_text, "\n");
    }

    #[test]
    fn formatting_response_keeps_in_bounds_drops_out_of_bounds() {
        // Mixed batch: a valid edit on line 0 and a malformed edit whose end
        // extends past EOF. Only the valid one survives.
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
                        "start": { "line": 1, "character": 0 },
                        "end": { "line": 5, "character": 0 }
                    },
                    "newText": "wrong"
                }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(10, 0), 2).unwrap();

        assert_eq!(edits.len(), 1, "only the in-bounds edit survives");
        assert_eq!(edits[0].new_text, "    ");
    }

    #[rstest]
    #[case::empty("", 1)]
    #[case::single_line("abc", 1)]
    #[case::two_lines("abc\ndef", 2)]
    #[case::trailing_newline("abc\n", 2)]
    #[case::two_trailing_newlines("abc\n\n", 3)]
    #[case::only_newline("\n", 2)]
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
    // column of the last real line and let the editor's standard
    // past-end-of-line clamping snap them into place.

    #[test]
    fn formatting_response_clamps_zero_width_insert_on_synthetic_eof_line() {
        // virtual_line_count = 1 (e.g., "foo" with no trailing newline) →
        // formatter emits (1,0)..(1,0) → "\n". Must be clamped to a zero-width
        // insert on line 0 rather than dropped.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 1, "character": 0 },
                        "end":   { "line": 1, "character": 0 }
                    },
                    "newText": "\n"
                }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(10, 0), 1).unwrap();

        assert_eq!(edits.len(), 1, "canonical insertFinalNewline shape kept");
        assert_eq!(edits[0].new_text, "\n");
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

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(0, 0), 2).unwrap();

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

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(0, 0), 2).unwrap();

        assert!(
            edits.is_empty(),
            "edit whose start.line is past virtual EOF must be dropped, \
             even when end.line is in bounds"
        );
    }

    #[test]
    fn formatting_response_still_drops_edits_two_or_more_lines_past_eof() {
        // Regression guard: the new "synthetic-line-0" exception only relaxes
        // a single-line overshoot. An edit ending two lines past EOF is still
        // malformed and must be dropped to protect host content.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end":   { "line": 5, "character": 0 }
                    },
                    "newText": "wrong"
                }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &RegionOffset::new(0, 0), 2).unwrap();

        assert!(
            edits.is_empty(),
            "edits ending more than one line past EOF must still be dropped"
        );
    }
}
