//! `textDocument/formatting` bridge handler. Translates each returned
//! `TextEdit` range from virtual coordinates back to the host document, applying
//! the injection's [`RegionOffset`] (including per-line column for the first line).
//!
//! [`transform_formatting_response_to_host`] and [`count_lines`] are `pub(super)`
//! so [`super::range_formatting`], which shares the same virtual→host hazards,
//! can reuse them.
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
use tower_lsp_server::ls_types::{
    DocumentFormattingParams, FormattingOptions, NumberOrString, Position, TextDocumentIdentifier,
    TextEdit, WorkDoneProgressParams,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
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
        let virtual_line_count = count_lines(virtual_content);
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
                    virtual_line_count,
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
pub(super) fn count_lines(text: &str) -> u32 {
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
/// Used to accept the canonical insertFinalNewline shape without dropping it
/// as past-EOF.
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
/// `virtual_line_count` is the LSP line count (1 for empty), from [`count_lines`].
pub(super) fn transform_formatting_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    virtual_line_count: u32,
    region_end: Position,
) -> io::Result<Vec<TextEdit>> {
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
    // inserted at end-of-content, not over any existing range. A start at
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

    for edit in &mut edits {
        translate_virtual_range_to_host(&mut edit.range, offset);
    }

    // Clamp positions past the end of the last content line into the region:
    // a character past the line length defaults back to the line length, so
    // such a position — the (last line, u32::MAX) sentinel from the EOF
    // clamps, or a server's own large column — means "end of the region's
    // last content line", which IS the region end. Without this, the host
    // line may run on past the region (an inline region's closing quote) and
    // the exact containment check below would drop every canonical
    // insertFinalNewline response. Only positions on the region end's own
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

        let edits =
            transform_formatting_response_to_host(response, &offset, 2, region_end).unwrap();

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

        let edits =
            transform_formatting_response_to_host(response, &offset, 2, region_end).unwrap();

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

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            UNBOUNDED,
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
            UNBOUNDED,
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
            UNBOUNDED,
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
            UNBOUNDED,
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
            UNBOUNDED,
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
            2,
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

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            3,
            TEST_REGION_END,
        )
        .unwrap();

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

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            3,
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "in-bounds zero-width EOF insert is kept");
        assert_eq!(edits[0].new_text, "\n");
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
            2,
            TEST_REGION_END,
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "any out-of-bounds edit must drop the whole response: {edits:?}"
        );
    }

    #[test]
    fn insert_final_newline_survives_a_realistic_region_end() {
        // Content "local x = 1" (no trailing newline) at host line 3 in a
        // plain fence: region end is the exact content end (3, 11), NOT a
        // permissive sentinel. The canonical insertFinalNewline shape — a
        // zero-width insert at the synthetic next-line anchor — must snap to
        // the region end and survive containment.
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
                  "newText": "\n" }
            ]
        });

        let edits =
            transform_formatting_response_to_host(response, &offset, 1, region_end).unwrap();

        assert_eq!(edits.len(), 1, "insertFinalNewline must survive: {edits:?}");
        assert_eq!(edits[0].range.start, region_end);
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, "\n");
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

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            count_lines(virtual_content),
            region_end,
        )
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
    // sentinel survives).

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

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            1,
            TEST_REGION_END,
        )
        .unwrap();

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

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(0, 0),
            2,
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
            2,
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
                    "newText": "b\n"
                }
            ]
        });
        let region_end = Position::new(11, 3);

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            2,
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(11, 0));
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, "b\n");
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
            2,
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
            1,
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
            4,
            region_end,
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(10, 0));
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, "if true; then\n  echo\nfi\n");
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
            2,
            TEST_REGION_END,
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "edits starting more than one line past EOF must still be dropped"
        );
    }
}
