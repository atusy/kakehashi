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
use super::super::protocol::translate_virtual_text_edits_to_host;
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
                    virtual_content,
                )
                .map(Some)
            },
            downstream_id_probe,
        )
        .await?
    }
}

/// Count the number of lines in `text`, with the LSP convention that a
/// trailing newline introduces an extra (empty) line.
///
/// Returns 1 for the empty string (a single empty line, index 0).
pub(super) fn count_lines(text: &str) -> u32 {
    // Line breaks + 1 gives the number of line "buckets" in the split —
    // exactly what the LSP position model expects, whose line breaks are
    // `\n`, `\r\n` and a lone `\r` (which a prepare peer's gap placeholder,
    // say, may contain).
    let breaks =
        text.matches('\n').count() + text.matches('\r').count() - text.matches("\r\n").count();
    u32::try_from(breaks)
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

/// `edits` to `server_text`, unless they change its boundary layout (see
/// [`restore_boundary_layout`]), or that of a string joined into it at one
/// of `joints` ([`restore_joined_layout`]): then the edits from
/// `server_text` to the formatted text with that layout restored. Edits that
/// do not apply (reversed or overlapping) are left as they are, for the
/// checks downstream to judge rather than be laundered.
///
/// [`restore_boundary_layout`]: crate::text::layout::restore_boundary_layout
/// [`restore_joined_layout`]: super::super::protocol::restore_joined_layout
fn keep_boundary_layout(
    server_text: &str,
    edits: Vec<TextEdit>,
    joints: impl IntoIterator<Item = usize>,
) -> Vec<TextEdit> {
    if edits.is_empty() {
        return edits;
    }
    let Some(formatted) = super::super::protocol::apply_text_edits_clamped(server_text, &edits)
    else {
        return edits;
    };
    let restored = super::super::protocol::restore_joined_layout(server_text, &formatted, joints);
    if restored == formatted {
        return edits;
    }
    super::super::protocol::text_edits_between(server_text, &restored)
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
/// such edits would corrupt the closing fence or quotes. The canonical
/// insert-final-newline anchor (column 0 of the synthetic line just past EOF) is
/// first clamped back onto the last real line; any edit with *either* endpoint
/// still on a line `>= virtual_line_count` is then dropped before translation.
/// `virtual_line_count` is the LSP line count (1 for empty), from [`count_lines`].
/// `server_text` is the text the server formatted (a prepared document's
/// prepared text), whose boundary layout the result keeps
/// ([`keep_boundary_layout`]).
pub(super) fn transform_formatting_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    virtual_line_count: u32,
    region_end: Position,
    server_text: &str,
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
    // (end.line == virtual_line_count && end.character == 0). That is one
    // past EOF in line space but cannot corrupt host bytes because the
    // payload is inserted at end-of-content, not over any existing range.
    // Clamp those anchors down to (last_real_line, u32::MAX) so the editor's
    // standard past-end-of-line clamping snaps them to the line's actual
    // length. Skipped for empty virtual docs (virtual_line_count == 0 is
    // never produced by count_lines, but guard against it just in case).
    //
    // A whole-document replacement — from the document start to an end past
    // EOF, often an "end of document" sentinel such as (2^31-1, 2^31-1), as
    // bash-language-server's shfmt answer uses — is clamped the same way:
    // its new text is the whole document, so ending it at the document end
    // touches nothing beyond the region. A partial edit ending past EOF stays
    // malformed and is dropped below.
    if virtual_line_count > 0 {
        let last_real_line = virtual_line_count - 1;
        for edit in &mut edits {
            clamp_synthetic_eof_anchor(&mut edit.range.start, last_real_line, virtual_line_count);
            clamp_synthetic_eof_anchor(&mut edit.range.end, last_real_line, virtual_line_count);
            let document_start = tower_lsp_server::ls_types::Position::new(0, 0);
            if edit.range.start == document_start && edit.range.end.line >= virtual_line_count {
                edit.range.end.line = last_real_line;
                edit.range.end.character = u32::MAX;
            }
        }
    }

    // Drop edits whose start OR end position is still past the virtual
    // document's last line after clamping. Such edits would corrupt host
    // content beyond the injection region after offset translation (see
    // function-level docs). Checking both endpoints handles both the common
    // "formatter overshoots EOF" case and the malformed `start > virtual_eof`
    // shape that would otherwise sneak through with an in-bounds `end`.
    // ALL-OR-NOTHING (like the prefix gate below): a formatter answer is one
    // atomic diff, so applying only its in-bounds edits could pair-break a
    // deletion/insertion and duplicate or lose content.
    if !edits.iter().all(|edit| {
        edit.range.start.line < virtual_line_count && edit.range.end.line < virtual_line_count
    }) {
        warn!(
            target: "kakehashi::bridge",
            "Dropped a formatting response ({} edit(s)): an edit extends past virtual EOF (line {}) and would corrupt host content beyond the injection region",
            edits.len(),
            virtual_line_count
        );
        return Ok(Vec::new());
    }

    // The document's edges are the host's layout (the line break after a Nix
    // `''`, the final line break and indentation before the closing `''`),
    // which a formatter treating the document as a file strips; keep them,
    // and those of each string a prepared document joins.
    let joints = offset
        .prepared()
        .into_iter()
        .flat_map(super::super::protocol::PreparedMap::emptied_gap_offsets);
    let edits = keep_boundary_layout(server_text, edits, joints);

    // A prepared document's edits are re-diffed back into the virtual
    // document; one touching host-owned text (a gap) fails the request
    // rather than reading as "already formatted" (the CLI exits non-zero,
    // an editor's log shows the failure).
    let Some(mut edits) = translate_virtual_text_edits_to_host(edits, offset) else {
        return Err(io::Error::other(
            "formatting result edits host-owned text of a prepared virtual document",
        ));
    };

    // Clamp synthetic-EOF sentinels into the region: the (last line, u32::MAX)
    // sentinel from `clamp_synthetic_eof_anchor` saturates through translation
    // and would otherwise fail the exact containment check below, dropping
    // every canonical insertFinalNewline response. Clamping to `region_end`
    // (the content-precise host end) is exact — the sentinel means "end of
    // the region's last content line", which IS the region end. Restricted to
    // the last content line: a stray u32::MAX character anywhere else stays
    // put and fails containment (fail-closed).
    let last_real_host_line = offset
        .line()
        .saturating_add(virtual_line_count.saturating_sub(1));
    for edit in &mut edits {
        for pos in [&mut edit.range.start, &mut edit.range.end] {
            if pos.character == u32::MAX
                && pos.line == last_real_host_line
                && (pos.line, pos.character) > (region_end.line, region_end.character)
            {
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
            2,
            region_end,
            "abcd
efghijkl",
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
            2,
            region_end,
            "abcd
efghijkl",
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
            "abcd
x
y
z
",
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
            "abcdef
ghijklmn",
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
            "a",
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
            "a",
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
            "a",
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
            "a
b",
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
                    "newText": "!"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            3,
            TEST_REGION_END,
            "a
b
xxxxxx",
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "synthetic-EOF-anchored edit is kept");
        assert_eq!(edits[0].new_text, "!");
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
                    "newText": "!"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            3,
            TEST_REGION_END,
            "a
b
xxxxxx",
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "in-bounds zero-width EOF insert is kept");
        assert_eq!(edits[0].new_text, "!");
    }

    #[test]
    fn formatting_response_with_an_out_of_bounds_edit_drops_whole() {
        // Mixed batch: a valid edit on line 0 and a malformed edit whose end
        // extends past EOF. A formatter answer is one atomic diff, so the
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
                        "start": { "line": 1, "character": 0 },
                        "end": { "line": 5, "character": 0 }
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
            "abcd
efgh",
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "any out-of-bounds edit must drop the whole response: {edits:?}"
        );
    }

    #[test]
    fn insert_final_newline_leaves_a_region_without_a_final_line_break_unchanged() {
        // Content "local x = 1" (no trailing newline) at host line 3: its end
        // is the host's layout (a Nix `''local x = 1''`, say), so the
        // canonical insertFinalNewline shape — a zero-width insert at the
        // synthetic next-line anchor — must not move the closing delimiter
        // onto the next line.
        let region = "local x = 1";
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                { "range": { "start": { "line": 1, "character": 0 },
                             "end": { "line": 1, "character": 0 } },
                  "newText": "\n" }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            count_lines(region),
            Position::new(10, 11),
            region,
        )
        .unwrap();

        assert_eq!(apply_to_region(region, &edits), region, "{edits:?}");
    }

    #[rstest]
    #[case::at_the_synthetic_next_line("local x = 1\n", 2, 0)]
    #[case::after_a_closing_indentation("local x = 1\n    ", 1, 4)]
    fn insert_final_newline_is_not_doubled_for_a_region_ending_with_a_line_break(
        #[case] region: &str,
        #[case] line: u32,
        #[case] character: u32,
    ) {
        // The region already ends with its line break (and, before a Nix
        // closing `''`, its indentation): a further line break at the end
        // would add a blank line to the host's layout.
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                { "range": { "start": { "line": line, "character": character },
                             "end": { "line": line, "character": character } },
                  "newText": "\n" }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            count_lines(region),
            Position::new(11, character),
            region,
        )
        .unwrap();

        assert_eq!(apply_to_region(region, &edits), region, "{edits:?}");
    }

    #[rstest]
    #[case::empty("", 1)]
    #[case::single_line("abc", 1)]
    #[case::two_lines("abc\ndef", 2)]
    #[case::trailing_newline("abc\n", 2)]
    #[case::two_trailing_newlines("abc\n\n", 3)]
    #[case::only_newline("\n", 2)]
    #[case::crlf("abc\r\ndef", 2)]
    #[case::lone_cr("abc\rdef\r", 3)]
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
                    "newText": "!"
                }
            ]
        });

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            1,
            TEST_REGION_END,
            "local x",
        )
        .unwrap();

        assert_eq!(
            edits.len(),
            1,
            "insert at the synthetic next-line anchor kept"
        );
        assert_eq!(edits[0].new_text, "!");
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
            "ab
cdefg",
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
            "ab
cd",
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "edit whose start.line is past virtual EOF must be dropped, \
             even when end.line is in bounds"
        );
    }

    #[test]
    fn a_whole_document_replacement_ending_past_eof_ends_at_the_region_end() {
        // bash-language-server (shfmt) and others answer with one edit from
        // the document start to an "end of document" sentinel far past EOF.
        // Its new text is the whole document, so clamping its end to the
        // region end touches nothing beyond the region.
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
        let region_end = Position {
            line: 13,
            character: 0,
        };

        let edits = transform_formatting_response_to_host(
            response,
            &RegionOffset::new(10, 0),
            4,
            region_end,
            "if true;then
    echo
fi
",
        )
        .unwrap();

        assert_eq!(edits.len(), 1, "{edits:?}");
        assert_eq!(edits[0].range.start, Position::new(10, 0));
        assert_eq!(edits[0].range.end, region_end);
        assert_eq!(edits[0].new_text, "if true; then\n  echo\nfi\n");
    }

    #[test]
    fn a_whole_document_replacement_ending_past_eof_maps_through_a_prepared_document() {
        use super::super::super::protocol::{VirtualLayout, apply_prepare_result};
        // `  if true; then\n      echo\n  fi\n` at host line 10, dedented by
        // two: the formatter sees `if true; then\n    echo\nfi\n`.
        let virtual_text = "  if true; then\n      echo\n  fi\n";
        let changes = [0, 1, 2].map(|line| {
            json!({"range": {"start": {"line": line, "character": 0},
                             "end": {"line": line, "character": 2}}, "newText": ""})
        });
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content", "changes": changes}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(result),
        )
        .unwrap();
        let offset = RegionOffset::new(10, 0).with_prepared(prepared.map);
        let response = json!({"jsonrpc": "2.0", "id": 42, "result": [{
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 2147483647, "character": 2147483647}},
            "newText": "if true; then\n  echo\nfi\n"
        }]});

        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            count_lines(&prepared.text),
            Position::new(13, 0),
            &prepared.text,
        )
        .unwrap();

        // Applied to the host region, the echo line is re-indented to the
        // formatter's two spaces plus the two the peer removed.
        let mut host: Vec<String> = virtual_text.split('\n').map(str::to_string).collect();
        let mut sorted = edits.clone();
        sorted.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        for edit in sorted {
            let mut text = host.join("\n");
            let offset_of = |position: Position| {
                text.split_inclusive('\n')
                    .take((position.line - 10) as usize)
                    .map(str::len)
                    .sum::<usize>()
                    + position.character as usize
            };
            let (start, end) = (offset_of(edit.range.start), offset_of(edit.range.end));
            text.replace_range(start..end, &edit.new_text);
            host = text.split('\n').map(str::to_string).collect();
        }
        assert_eq!(host.join("\n"), "  if true; then\n    echo\n  fi\n");
    }

    /// `region` (at host line 10, column 0) with host `edits` applied.
    fn apply_to_region(region: &str, edits: &[TextEdit]) -> String {
        let mut text = region.to_string();
        let mut sorted = edits.to_vec();
        sorted.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
        for edit in sorted {
            let offset_of = |position: Position| {
                text.split_inclusive('\n')
                    .take((position.line - 10) as usize)
                    .map(str::len)
                    .sum::<usize>()
                    + position.character as usize
            };
            let (start, end) = (offset_of(edit.range.start), offset_of(edit.range.end));
            text.replace_range(start..end, &edit.new_text);
        }
        text
    }

    fn whole_document(new_text: &str) -> serde_json::Value {
        json!({"jsonrpc": "2.0", "id": 42, "result": [{
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": 2147483647, "character": 2147483647}},
            "newText": new_text
        }]})
    }

    #[test]
    fn formatting_keeps_the_boundary_layout_of_an_unprepared_document() {
        // `''\n{  }\n    ''` in Nix: the formatter strips the line break after
        // the opening `''` and the one (with the indentation) before the
        // closing `''`; only its change to the content is kept.
        let region = "\n{  }\n    ";
        let edits = transform_formatting_response_to_host(
            whole_document("{}"),
            &RegionOffset::new(10, 0),
            count_lines(region),
            Position::new(12, 4),
            region,
        )
        .unwrap();
        assert_eq!(apply_to_region(region, &edits), "\n{}\n    ");
    }

    #[test]
    fn formatting_keeps_the_boundary_layout_of_a_prepared_document() {
        use super::super::super::protocol::{VirtualLayout, apply_prepare_result};
        // The JSON in `''\n      {\n        "a":1\n      }\n    ''`, dedented
        // by six; the formatter fixes the spacing and strips the boundaries.
        let virtual_text = "\n      {\n        \"a\":1\n      }\n    ";
        let changes = [1, 2, 3].map(|line| {
            json!({"range": {"start": {"line": line, "character": 0},
                             "end": {"line": line, "character": 6}}, "newText": ""})
        });
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content", "changes": changes}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(result),
        )
        .unwrap();
        let offset = RegionOffset::new(10, 0).with_prepared(prepared.map);
        let edits = transform_formatting_response_to_host(
            whole_document("{\n  \"a\": 1\n}"),
            &offset,
            count_lines(&prepared.text),
            Position::new(14, 4),
            &prepared.text,
        )
        .unwrap();
        assert_eq!(
            apply_to_region(virtual_text, &edits),
            "\n      {\n        \"a\": 1\n      }\n    "
        );
    }

    /// Format `region` (at host line 10) through a document prepared with
    /// `changes`, the formatter answering `formatted` for the whole of it.
    fn format_prepared(region: &str, changes: serde_json::Value, formatted: &str) -> String {
        use super::super::super::protocol::{VirtualLayout, apply_prepare_result};
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content", "changes": changes}
        ]}))
        .unwrap();
        let prepared =
            apply_prepare_result(region, &VirtualLayout::single(region), Some(result)).unwrap();
        let offset = RegionOffset::new(10, 0).with_prepared(prepared.map);
        let region_end = region_host_end(&prepared.text, &offset);
        let edits = transform_formatting_response_to_host(
            whole_document(formatted),
            &offset,
            count_lines(&prepared.text),
            region_end,
            &prepared.text,
        )
        .unwrap();
        apply_to_region(region, &edits)
    }

    #[test]
    fn formatting_keeps_a_leading_blank_line_the_peer_deleted() {
        // `''\n      {"a":1}\n    ''` in Nix, prepared as the string's
        // value: the opening line break goes, and the content's indent. The
        // formatter breaks the object into lines, which regain that indent.
        let region = "\n      {\"a\":1}\n    ";
        let changes = json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 6}}, "newText": ""}
        ]);
        let formatted = "{\n  \"a\": 1\n}\n";
        let once = format_prepared(region, changes, formatted);
        assert_eq!(once, "\n      {\n        \"a\": 1\n      }\n    ");
        // Formatting again, prepared the same way, changes nothing.
        let mut changes = vec![json!(
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}
        )];
        changes.extend([1, 2, 3].map(|line| json!(
            {"range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 6}}, "newText": ""}
        )));
        assert_eq!(format_prepared(&once, json!(changes), formatted), once);
    }

    #[test]
    fn formatting_keeps_trailing_blank_lines_the_peer_deleted() {
        // A YAML `run: |` block whose blank lines the block clips. The
        // formatter splits the line; the new one regains the indent.
        let region = "  a; b\n\n\n";
        let changes = json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 3, "character": 0}}, "newText": ""}
        ]);
        let once = format_prepared(region, changes, "a\nb\n");
        assert_eq!(once, "  a\n  b\n\n\n");
        // Formatting again, prepared the same way, changes nothing.
        let changes = json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 2, "character": 0}, "end": {"line": 4, "character": 0}}, "newText": ""}
        ]);
        assert_eq!(format_prepared(&once, changes.clone(), "a\nb\n"), once);
        // A line the formatter appends at P's end goes on the content,
        // before the host's blank lines.
        assert_eq!(
            format_prepared(&once, changes, "a\nb\nc\n"),
            "  a\n  b\n  c\n\n\n"
        );
    }

    #[test]
    fn a_formatter_adding_a_final_line_break_does_not_double_it() {
        let region = "\n{}\n    ";
        let edits = transform_formatting_response_to_host(
            whole_document("{}\n"),
            &RegionOffset::new(10, 0),
            count_lines(region),
            Position::new(12, 4),
            region,
        )
        .unwrap();
        assert_eq!(apply_to_region(region, &edits), region);
    }

    #[test]
    fn a_formatter_still_trims_blank_lines_before_the_final_line_break() {
        // A markdown fence's content: only the last line break is layout.
        let region = "code\n\n\n";
        let edits = transform_formatting_response_to_host(
            whole_document("code\n"),
            &RegionOffset::new(10, 0),
            count_lines(region),
            Position::new(13, 0),
            region,
        )
        .unwrap();
        assert_eq!(apply_to_region(region, &edits), "code\n");
    }

    #[test]
    fn formatting_response_still_drops_a_partial_edit_two_or_more_lines_past_eof() {
        // Regression guard: only a whole-document replacement may end past
        // EOF. An edit from the middle of the document ending two lines past
        // EOF is still malformed and must be dropped to protect host content.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 1, "character": 0 },
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
            "ab
cd",
        )
        .unwrap();

        assert!(
            edits.is_empty(),
            "partial edits ending more than one line past EOF must still be dropped"
        );
    }

    /// Two Nix strings joined into one JSON document, as kakehashi combines
    /// `# json` strings: `head` indented by six, `tail` by eight, prepared
    /// by dedenting each string, emptying the Nix between them and replacing
    /// interpolations with `0`.
    fn joined_strings() -> (String, super::super::super::protocol::PreparedDocument) {
        use super::super::super::protocol::{SegmentKind, VirtualLayout, apply_prepare_result};
        let pad = |n: usize| " ".repeat(n);
        let pieces = [
            (
                SegmentKind::Content,
                format!("\n{}{{\n{}\"name\":    \"", pad(6), pad(14)),
                "",
            ),
            (SegmentKind::Gap, pad(11), "${cfg.name}"),
            (SegmentKind::Content, format!("\",\n{}", pad(4)), ""),
            (SegmentKind::Gap, format!("{}\n", pad(3)), "'';\n"),
            (SegmentKind::Gap, String::new(), "  nested = {"),
            (SegmentKind::Gap, "\n".to_string(), "\n"),
            (SegmentKind::Gap, String::new(), "      ''"),
            (SegmentKind::Content, format!("\n{}\"port\":", pad(16)), ""),
            (SegmentKind::Gap, pad(20), "${toString cfg.port}"),
            (
                SegmentKind::Content,
                format!("\n{}}}\n{}", pad(8), pad(6)),
                "",
            ),
        ];
        let virtual_text: String = pieces.iter().map(|(_, text, _)| text.as_str()).collect();
        let mut start = 0;
        let layout = VirtualLayout::from_pieces(
            &virtual_text,
            pieces.iter().map(|(kind, text, host)| {
                let range = start..start + text.len();
                start = range.end;
                (*kind, range, host.to_string())
            }),
        );
        let dedent = |line: u32, width: u32| {
            json!({"range": {"start": {"line": line, "character": 0},
                             "end": {"line": line, "character": width}}, "newText": ""})
        };
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content", "changes": [dedent(1, 6), dedent(2, 6)]},
            {"type": "gap", "content": "0"},
            {"type": "content"},
            {"type": "gap", "content": ""},
            {"type": "content", "changes": [dedent(1, 8)]},
            {"type": "gap", "content": "0"},
            {"type": "content", "changes": [dedent(1, 8)]}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(&virtual_text, &layout, Some(result)).unwrap();
        assert_eq!(
            prepared.text,
            format!(
                "\n{{\n{}\"name\":    \"0\",\n{}\n{}\"port\":0\n}}\n{}",
                pad(8),
                pad(4),
                pad(8),
                pad(6)
            )
        );
        (virtual_text, prepared)
    }

    #[test]
    fn formatting_keeps_the_layout_between_joined_strings() {
        // vscode-json-language-server's answer (tabSize 4): it reindents both
        // strings, and deletes the closing indentation of `head` and the line
        // break opening `tail`, which are the Nix layout around the gap.
        let (virtual_text, prepared) = joined_strings();
        let edit = |start: (u32, u32), end: (u32, u32), new_text: &str| {
            json!({"range": {"start": {"line": start.0, "character": start.1},
                             "end": {"line": end.0, "character": end.1}}, "newText": new_text})
        };
        let response = json!({"jsonrpc": "2.0", "id": 42, "result": [
            edit((0, 0), (1, 0), ""),
            edit((1, 1), (2, 8), "\n    "),
            edit((2, 15), (2, 19), " "),
            edit((2, 23), (4, 8), "\n    "),
            edit((4, 15), (4, 15), " "),
            edit((5, 1), (6, 6), ""),
        ]});
        let offset = RegionOffset::new(10, 0).with_prepared(prepared.map);
        let edits = transform_formatting_response_to_host(
            response,
            &offset,
            count_lines(&prepared.text),
            Position::new(17, 6),
            &prepared.text,
        )
        .unwrap();
        let pad = |n: usize| " ".repeat(n);
        // Each string keeps its own indentation: `head` six plus four,
        // `tail` eight plus four, and its closing brace eight.
        assert_eq!(
            apply_to_region(&virtual_text, &edits),
            format!(
                "\n{}{{\n{}\"name\": \"{}\",\n{}{}\n\n\n{}\"port\": {}\n{}}}\n{}",
                pad(6),
                pad(10),
                pad(11),
                pad(4),
                pad(3),
                pad(12),
                pad(20),
                pad(8),
                pad(6)
            )
        );
    }
}
