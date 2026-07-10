//! Completion request handling for bridge connections.
//!
//! Handles the coordinate transformation between host and virtual documents,
//! and drops items whose primary insertion (text edit, or the insertText/
//! label fallback at the request position) is unsafe for the injection region —
//! escape it, break per-line `> ` prefixes, or merge content into the closing
//! fence — if applied verbatim (see `transform_completion_item`).
//! Messages are queued via the channel-based writer task (ls-bridge-message-ordering) for FIFO
//! ordering with other messages.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{CompletionItem, CompletionList, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, build_position_based_request,
    region_host_end, response_has_jsonrpc_error, text_edit_safe_in_region,
};
use tower_lsp_server::ls_types::TextDocumentPositionParams;

impl LanguageServerPool {
    /// Send a completion request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing completion-specific request building and response
    /// transformation.
    ///
    /// Content synchronization (didChange) is handled by the notification pipeline
    /// in `forward_didchange_to_bridges`, which runs during `did_change()` processing
    /// before any subsequent request is handled.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_completion_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<CompletionList>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/completion") {
            return Ok(None);
        }
        self.execute_position_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            host_position,
            "textDocument/completion",
            |virtual_uri, request_id| {
                build_completion_request(virtual_uri, host_position, &offset, request_id)
            },
            |response, ctx| {
                let region_end = region_host_end(virtual_content, ctx.offset);
                transform_completion_response_to_host(
                    response,
                    ctx.offset,
                    region_end,
                    Some(host_position.line),
                    Some(EnvelopeContext {
                        server_name,
                        host_uri: host_uri.as_str(),
                        offset: ctx.offset,
                        region_end: Some(region_end),
                    }),
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC completion request for a downstream language server.
fn build_completion_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: tower_lsp_server::ls_types::Position,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<TextDocumentPositionParams> {
    build_position_based_request(
        virtual_uri,
        host_position,
        offset,
        request_id,
        "textDocument/completion",
    )
}

/// Parse a JSON-RPC completion response and transform coordinates to host document space.
///
/// Normalizes all responses to `CompletionList` (an array result is wrapped as
/// `CompletionList { isIncomplete: false, items }`). Returns `None` for null
/// results, missing results, and deserialization failures. When `envelope_ctx`
/// is `Some`, each item's `data` is wrapped in a routing envelope.
fn transform_completion_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    region_end: Position,
    request_host_line: Option<u32>,
    envelope_ctx: Option<EnvelopeContext<'_>>,
) -> Option<CompletionList> {
    if response_has_jsonrpc_error(&response, "textDocument/completion") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    // Determine format and deserialize into a unified CompletionList
    let mut list = if result.is_array() {
        // Legacy format: array of CompletionItem. Normalize to CompletionList.
        let Ok(items) = serde_json::from_value::<Vec<CompletionItem>>(result) else {
            return None;
        };
        CompletionList {
            is_incomplete: false,
            items,
        }
    } else {
        // Preferred format: CompletionList object
        let Ok(list) = serde_json::from_value::<CompletionList>(result) else {
            return None;
        };
        list
    };

    // Transform all items in the list (dropping any whose primary edit is
    // unsafe for the injection region), then optionally envelope for resolve
    // routing
    let before = list.items.len();
    list.items.retain_mut(|item| {
        if !transform_completion_item(item, offset, region_end, request_host_line) {
            return false;
        }
        if let Some(ref ctx) = envelope_ctx {
            envelope_item_data(item, ctx);
        }
        true
    });
    let dropped = before - list.items.len();
    if dropped > 0 {
        log::warn!(
            target: "kakehashi::bridge",
            "completion: dropped {dropped} item(s) whose primary insertion/edit is unsafe for the injection region (escapes it, breaks line prefixes, splits the host line, or merges content into the closing fence)"
        );
    }

    Some(list)
}

/// Transform textEdit range in a single completion item to host coordinates.
///
/// Handles both TextEdit format and InsertReplaceEdit format. Also transforms
/// additionalTextEdits if present.
///
/// Returns `false` when the item's primary insertion — its text edit, or the
/// insertText/label fallback applied at the request position — is unsafe for the
/// injection region — escapes it, corrupts per-line `> ` prefixes, or merges
/// content into the closing fence — if applied verbatim; the caller must
/// drop the whole item (same fail-closed rule as WorkspaceEdit bridging).
/// An unsafe `additionalTextEdits` member drops the whole array instead:
/// the primary insertion still applies, though possibly semantically
/// incomplete (e.g. without its auto-import) — availability over fidelity.
pub(super) fn transform_completion_item(
    item: &mut CompletionItem,
    offset: &RegionOffset,
    region_end: Position,
    request_host_line: Option<u32>,
) -> bool {
    // A multi-line insertion is unsafe only where its INSERTION POINT's
    // neighborhood is prefixed — a mixed region (single-element `[n]` offsets:
    // line 0 starts mid-host-line, later lines are whole and unprefixed)
    // safely takes it on a later line. Checking the following line too covers
    // the inserted-lines-land-below shape, mirroring the shared predicate.
    // `None` (the resolve path, which has no position snapshot) falls back to
    // region-wide any-prefix: fail-closed.
    let prefixed_at_insertion = |host_line: Option<u32>| match host_line {
        Some(line) => {
            insertion_point_prefixed(offset, region_end, line)
                || insertion_point_prefixed(offset, region_end, line.saturating_add(1))
        }
        None => offset.columns().iter().any(|&column| column != 0),
    };
    // The literal newline scans below can't see what a SNIPPET expands to at
    // the client: runtime variables (`${CLIPBOARD}`, `$TM_SELECTED_TEXT`) may
    // be multiline. At a prefixed insertion point, fail closed on any snippet
    // variable in the primary insert text (tabstops/placeholders expand from
    // literal text the scans already cover; additionalTextEdits are never
    // snippets).
    let snippet =
        item.insert_text_format == Some(tower_lsp_server::ls_types::InsertTextFormat::SNIPPET);
    let snippet_unsafe = |text: &str, host_line: Option<u32>| {
        snippet && prefixed_at_insertion(host_line) && snippet_contains_variable(text)
    };

    // Transform text_edit if present
    if let Some(ref mut text_edit) = item.text_edit {
        match text_edit {
            tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit) => {
                translate_virtual_range_to_host(&mut edit.range, offset);
                if !text_edit_safe_in_region(edit, offset, region_end)
                    || snippet_unsafe(&edit.new_text, Some(edit.range.start.line))
                {
                    return false;
                }
            }
            tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(edit) => {
                translate_virtual_range_to_host(&mut edit.insert, offset);
                translate_virtual_range_to_host(&mut edit.replace, offset);
                // Check both ranges through one probe edit, moving new_text in
                // and out instead of cloning it per range.
                let mut probe = tower_lsp_server::ls_types::TextEdit {
                    range: edit.insert,
                    new_text: std::mem::take(&mut edit.new_text),
                };
                let insert_ok = text_edit_safe_in_region(&probe, offset, region_end);
                probe.range = edit.replace;
                let replace_ok = text_edit_safe_in_region(&probe, offset, region_end);
                edit.new_text = probe.new_text;
                // Check BOTH client-selectable ranges: with unequal starts,
                // the minimum line could be unprefixed while the other range
                // starts on a prefixed line.
                if !insert_ok
                    || !replace_ok
                    || snippet_unsafe(&edit.new_text, Some(edit.insert.start.line))
                    || snippet_unsafe(&edit.new_text, Some(edit.replace.start.line))
                {
                    return false;
                }
            }
        }
    }

    // No textEdit: the client inserts insertText (or the label) at the
    // completion position instead. In a prefixed region (per-line `> ` or an
    // inline region's host-line tail) a multi-line insertion creates
    // unprefixed/splitting lines — the same class the textEdit guard rejects —
    // and the transform has no range to check, so the newline itself is the
    // reject signal. All-zero (plain fenced) regions take multi-line
    // insertions safely and stay exempt.
    if item.text_edit.is_none() {
        let effective = item.insert_text.as_deref().unwrap_or(&item.label);
        if (effective.contains('\n') || effective.contains('\r'))
            && prefixed_at_insertion(request_host_line)
        {
            return false;
        }
        if snippet_unsafe(effective, request_host_line) {
            return false;
        }
    }

    // Transform additional_text_edits if present. ALL-OR-NOTHING: the array
    // can carry paired halves of one operation (a deletion plus a
    // reinsertion), so stripping only the unsafe member could apply half and
    // lose content — if any member is unsafe, drop the whole array. The item
    // is kept: its primary insertion still applies, though possibly
    // semantically incomplete (e.g. without its auto-import) — availability
    // over fidelity, never corruption.
    if let Some(ref mut additional_edits) = item.additional_text_edits {
        for edit in additional_edits.iter_mut() {
            translate_virtual_range_to_host(&mut edit.range, offset);
        }
        if !additional_edits
            .iter()
            .all(|edit| text_edit_safe_in_region(edit, offset, region_end))
        {
            log::warn!(
                target: "kakehashi::bridge",
                "completion: dropped an item's additionalTextEdits ({}): a member is unsafe for the injection region (escapes it, breaks line prefixes, or merges content into the closing fence)",
                additional_edits.len()
            );
            item.additional_text_edits = None;
        }
    }
    true
}

/// Whether the host line carries region-adjacent host content an unprefixed
/// insertion would break. Lines before the region are fail-closed `true`.
/// Lines past
/// the recorded per-line array are the BOUNDARY in per-line (blockquote)
/// regions — fail-closed `true` (their real prefix is unrecorded) — but in
/// the single-element fallback shape (`[start_column]`: only line 0 starts
/// mid-host-line) later lines are whole and unprefixed: `false`.
fn insertion_point_prefixed(offset: &RegionOffset, region_end: Position, host_line: u32) -> bool {
    let columns = offset.columns();
    // Boundary rows of a per-line-prefixed region — the recorded trailing
    // zeros and everything past the array — carry an unrecorded real prefix
    // (the closing fence's `> `): fail-closed, mirroring the shared guard's
    // boundary rule. All-zero multi-line regions have no boundary semantics.
    let per_line_prefixed = columns.len() > 1 && columns.iter().any(|&column| column != 0);
    if per_line_prefixed && region_end.character == 0 && host_line >= region_end.line {
        return true;
    }
    match host_line.checked_sub(offset.line()) {
        None => true,
        Some(virtual_line) => match columns.get(virtual_line as usize) {
            // Single-element shape on line 0: in a MULTI-LINE region the
            // captured content runs to end-of-line, so an inserted newline
            // splits only injected content (mirror the shared guard's
            // `newline_splits_nothing`); in a SAME-LINE region the host line
            // continues past the region, so a non-zero start column (inline
            // content always sits behind its delimiter) is prefixed. A
            // column-0 same-line region is a one-line fenced block — safe.
            // Known limitation: a CUSTOM injection query (`#offset!`) could
            // capture a column-0 same-line range with a host suffix after it;
            // the offsets can't represent that, and treating column 0 as
            // unsafe breaks insertFinalNewline for one-line fences — the
            // built-in queries never produce the shape.
            Some(&column) => column != 0 && (columns.len() > 1 || region_end.line == offset.line()),
            None => per_line_prefixed,
        },
    }
}

/// Whether snippet-syntax text references a snippet VARIABLE — `$name` or
/// `${name...}` with a leading letter/underscore — whose expansion the client
/// resolves at insert time and may be multiline (`${CLIPBOARD}`,
/// `$TM_SELECTED_TEXT`). Numeric tabstops/placeholders (`$1`, `${1:text}`)
/// expand from literal text the caller's newline scans already cover, so they
/// don't count. An escaped `\\$` is literal per the snippet grammar; a
/// preceding backslash therefore skips the candidate (an over-escape here
/// would only reject, never admit).
fn snippet_contains_variable(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if !escaped => escaped = true,
            b'$' if !escaped => {
                let next = bytes.get(i + 1).copied();
                let starts_name = |c: u8| c.is_ascii_alphabetic() || c == b'_';
                match next {
                    Some(b'{') if bytes.get(i + 2).copied().is_some_and(starts_name) => {
                        return true;
                    }
                    Some(c) if starts_name(c) => return true,
                    _ => {}
                }
                escaped = false;
            }
            _ => escaped = false,
        }
        i += 1;
    }
    false
}

// =============================================================================
// Envelope types for completionItem/resolve routing
// =============================================================================

/// Wrapper key inside `CompletionItem.data` that identifies the origin server.
const ENVELOPE_KEY: &str = "kakehashi";

/// Envelope stored in `CompletionItem.data` for routing `completionItem/resolve`.
///
/// When Kakehashi fans out completion to multiple downstream servers, each item's
/// `data` field is wrapped in this envelope so that a later `completionItem/resolve`
/// can be routed back to the correct server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct KakehashiEnvelope {
    /// Server name identifying which downstream produced the item.
    pub origin: String,
    /// Host document URI the completion was requested on. Used to re-resolve the
    /// same `(server, root)` connection for `completionItem/resolve` so the
    /// resolve reaches the very process that produced the item (#382) — without
    /// it, resolve would land on the server's client-root fallback connection,
    /// a *different* process in a multi-root monorepo. Stored as a `String`
    /// (not `url::Url`) because this crate does not enable the `url/serde`
    /// feature; the resolve path parses it once. Empty (via `serde(default)`)
    /// for envelopes from before this field existed → falls back to root-less
    /// routing, matching the old behavior.
    #[serde(default)]
    pub host_uri: String,
    /// The downstream server's original `data` value (preserved verbatim).
    pub inner: Option<Value>,
    /// Region offset snapshot for coordinate transformation of the resolved response.
    pub offset: EnvelopeOffset,
    /// Region end (host `(line, character)`) snapshot for the resolve-path
    /// safety guard (containment, prefixes, fence boundary) — the resolve has
    /// no region-content access, so it can't recompute this. `None` (via `serde(default)`) for envelopes
    /// minted before this field existed → the resolve path falls back to a
    /// fully fail-closed guard in prefixed regions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_end: Option<(u32, u32)>,
}

/// Offset snapshot stored in the envelope for coordinate back-translation.
///
/// Serialized into `CompletionItem.data` and roundtripped through the client.
/// The optional `line_column_offsets` field preserves per-line column offsets
/// for blockquoted injections. Old envelopes without this field deserialize
/// with `None` thanks to `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct EnvelopeOffset {
    pub line: u32,
    pub column: u32,
    /// Per-line column offsets for blockquoted injections.
    /// `None` for non-blockquote injections (backwards-compatible default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_column_offsets: Option<Vec<u32>>,
}

impl From<&RegionOffset> for EnvelopeOffset {
    fn from(o: &RegionOffset) -> Self {
        Self {
            line: o.line(),
            column: o.column_for_line(0),
            line_column_offsets: Some(o.columns().to_vec()),
        }
    }
}

impl From<&EnvelopeOffset> for RegionOffset {
    fn from(o: &EnvelopeOffset) -> Self {
        match &o.line_column_offsets {
            Some(columns) => Self::with_per_line_offsets(o.line, columns.clone()),
            None => Self::new(o.line, o.column),
        }
    }
}

/// Context needed to create envelopes during completion response processing.
pub(crate) struct EnvelopeContext<'a> {
    pub server_name: &'a str,
    /// Host document URI the completion ran on, stored in the envelope so
    /// `completionItem/resolve` can route back to the originating connection.
    pub host_uri: &'a str,
    pub offset: &'a RegionOffset,
    /// Region end (host coords) snapshot for the resolve-path safety guard;
    /// `None` only when re-enveloping a legacy envelope that never carried it.
    pub region_end: Option<Position>,
}

/// Wrap `item.data` in a Kakehashi envelope for origin tracking.
///
/// The original `data` value is moved into `inner`, and the envelope is
/// serialized as `{"kakehashi": {...}}`.
pub(crate) fn envelope_item_data(item: &mut CompletionItem, ctx: &EnvelopeContext) {
    let inner = item.data.take();
    let envelope = KakehashiEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        inner,
        offset: EnvelopeOffset::from(ctx.offset),
        region_end: ctx.region_end.map(|end| (end.line, end.character)),
    };
    item.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

/// Extract the envelope from a completion item's `data` without modifying the item.
///
/// Returns `None` if `data` is absent or not an envelope.
pub(crate) fn extract_envelope(item: &CompletionItem) -> Option<KakehashiEnvelope> {
    let data = item.data.as_ref()?;
    let wrapper = data.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

/// Extract the envelope and restore the original `data` value.
///
/// On success, `item.data` is set back to the downstream's original value (`inner`).
/// Returns the extracted envelope. Returns `None` if not an envelope (item unchanged).
pub(crate) fn strip_envelope(item: &mut CompletionItem) -> Option<KakehashiEnvelope> {
    let mut envelope = extract_envelope(item)?;
    item.data = envelope.inner.take();
    Some(envelope)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    // ==========================================================================
    // Completion request tests
    // ==========================================================================

    #[test]
    fn completion_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_completion_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn completion_request_translates_position_to_virtual_coordinates() {
        // Host line 5, region starts at line 3 -> virtual line 2
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_completion_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_position_request(&request, "textDocument/completion", 2);
    }

    // ==========================================================================
    // Completion response transformation tests
    // ==========================================================================

    #[test]
    fn completion_response_transforms_textedit_ranges() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [
                    {
                        "label": "print",
                        "kind": 3,
                        "textEdit": {
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 3 }
                            },
                            "newText": "print"
                        }
                    },
                    { "label": "pairs", "kind": 3 }
                ]
            }
        });
        let region_start_line = 3;

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        assert_eq!(list.items.len(), 2);

        // First item should have transformed range
        let item = &list.items[0];
        assert_eq!(item.label, "print");
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) = item.text_edit
        {
            assert_eq!(edit.range.start.line, 4); // 1 + 3 = 4
            assert_eq!(edit.range.end.line, 4);
        } else {
            panic!("Expected TextEdit");
        }

        // Second item has no textEdit
        assert_eq!(list.items[1].label, "pairs");
        assert!(list.items[1].text_edit.is_none());
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::without_result(json!({"jsonrpc": "2.0", "id": 42}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_a_completion_response"}))]
    #[case::error_response(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    fn completion_response_returns_none_for_invalid_response(#[case] response: serde_json::Value) {
        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(3, 0),
            TEST_REGION_END,
            None,
            None,
        );
        assert!(transformed.is_none());
    }

    #[test]
    fn completion_response_handles_array_format() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "print",
                "textEdit": {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 2 }
                    },
                    "newText": "print"
                }
            }]
        });
        let region_start_line = 5;

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        // Array format is normalized to CompletionList with isIncomplete=false
        assert!(!list.is_incomplete);
        assert_eq!(list.items.len(), 1);

        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 5); // 0 + 5 = 5
            assert_eq!(edit.range.end.line, 5);
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_transforms_insert_replace_edit() {
        // InsertReplaceEdit format used by rust-analyzer, tsserver, etc.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "println!",
                    "textEdit": {
                        "insert": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 3 }
                        },
                        "replace": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 8 }
                        },
                        "newText": "println!"
                    }
                }]
            }
        });
        let region_start_line = 10;

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();

        let item = &list.items[0];
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(ref edit)) =
            item.text_edit
        {
            // Insert range transformed: line 2 + 10 = 12
            assert_eq!(edit.insert.start.line, 12);
            assert_eq!(edit.insert.end.line, 12);
            // Replace range transformed: line 2 + 10 = 12
            assert_eq!(edit.replace.start.line, 12);
            assert_eq!(edit.replace.end.line, 12);
        } else {
            panic!("Expected InsertReplaceEdit");
        }
    }

    #[test]
    fn completion_response_transforms_additional_text_edits() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "import",
                    "additionalTextEdits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "import module\n"
                    }]
                }]
            }
        });
        let region_start_line = 5;

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();

        let item = &list.items[0];
        assert!(item.additional_text_edits.is_some());
        let edits = item.additional_text_edits.as_ref().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range.start.line, 5); // 0 + 5 = 5
    }

    // ==========================================================================
    // Column offset tests for completion response transformation
    // ==========================================================================

    #[test]
    fn completion_response_applies_column_offset_on_first_virtual_line() {
        // Item on virtual line 0 → column offset added to character positions
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "func",
                    "textEdit": {
                        "range": {
                            "start": { "line": 0, "character": 2 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "function"
                    }
                }]
            }
        });

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(10, 4),
            TEST_REGION_END,
            None,
            None,
        );

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 10); // 0 + 10
            assert_eq!(edit.range.start.character, 6); // 2 + 4 (first line)
            assert_eq!(edit.range.end.line, 10);
            assert_eq!(edit.range.end.character, 9); // 5 + 4 (first line)
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_ignores_column_offset_on_non_first_virtual_line() {
        // Item on virtual line 1 → only line offset, character unchanged
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "func",
                    "textEdit": {
                        "range": {
                            "start": { "line": 1, "character": 2 },
                            "end": { "line": 1, "character": 5 }
                        },
                        "newText": "function"
                    }
                }]
            }
        });

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(10, 4),
            TEST_REGION_END,
            None,
            None,
        );

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 11); // 1 + 10
            assert_eq!(edit.range.start.character, 2); // unchanged
            assert_eq!(edit.range.end.line, 11);
            assert_eq!(edit.range.end.character, 5); // unchanged
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_column_offset_with_insert_replace_edit() {
        // InsertReplaceEdit on virtual line 0 → column offset on both ranges
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "println!",
                    "textEdit": {
                        "insert": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 6 }
                        },
                        "replace": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "newText": "println!"
                    }
                }]
            }
        });

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(5, 7),
            TEST_REGION_END,
            None,
            None,
        );

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(ref edit)) =
            list.items[0].text_edit
        {
            // Insert range: line 0→5, char +7
            assert_eq!(edit.insert.start.line, 5);
            assert_eq!(edit.insert.start.character, 10); // 3 + 7
            assert_eq!(edit.insert.end.line, 5);
            assert_eq!(edit.insert.end.character, 13); // 6 + 7
            // Replace range: line 0→5, char +7
            assert_eq!(edit.replace.start.line, 5);
            assert_eq!(edit.replace.start.character, 10); // 3 + 7
            assert_eq!(edit.replace.end.line, 5);
            assert_eq!(edit.replace.end.character, 17); // 10 + 7
        } else {
            panic!("Expected InsertReplaceEdit");
        }
    }

    #[test]
    fn completion_response_column_offset_with_additional_text_edits_on_first_line() {
        // additionalTextEdits on virtual line 0 → column offset applied
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "import",
                    "additionalTextEdits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "use std::io;\n"
                    }]
                }]
            }
        });

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(5, 3),
            TEST_REGION_END,
            None,
            None,
        );

        let list = transformed.unwrap();
        let edits = list.items[0].additional_text_edits.as_ref().unwrap();
        assert_eq!(edits[0].range.start.line, 5);
        assert_eq!(edits[0].range.start.character, 3); // 0 + 3
        assert_eq!(edits[0].range.end.line, 5);
        assert_eq!(edits[0].range.end.character, 3); // 0 + 3
    }

    #[test]
    fn completion_request_applies_column_offset_on_first_line() {
        // Host position on first line of region → column subtracted in request
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 5,
            character: 14,
        };
        let request = build_completion_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            test_request_id(),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 0); // 5 - 5
        assert_eq!(json["params"]["position"]["character"], 10); // 14 - 4
    }

    #[test]
    fn completion_request_ignores_column_offset_on_non_first_line() {
        // Host position on non-first line → column NOT subtracted
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 7,
            character: 14,
        };
        let request = build_completion_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            test_request_id(),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 2); // 7 - 5
        assert_eq!(json["params"]["position"]["character"], 14); // unchanged
    }

    #[test]
    fn completion_range_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "test",
                "textEdit": {
                    "range": {
                        "start": { "line": 4294967295u32, "character": 0 },  // u32::MAX
                        "end": { "line": 4294967295u32, "character": 5 }
                    },
                    "newText": "test"
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_completion_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            None,
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        // Array format is normalized to CompletionList with isIncomplete=false
        assert!(!list.is_incomplete);

        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, u32::MAX);
            assert_eq!(edit.range.end.line, u32::MAX);
        } else {
            panic!("Expected TextEdit");
        }
    }

    // ==========================================================================
    // Envelope round-trip tests
    // ==========================================================================

    #[test]
    fn envelope_round_trip_with_data() {
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"resolve_id": 42})),
            ..Default::default()
        };
        let offset = RegionOffset::new(3, 4);
        let ctx = EnvelopeContext {
            server_name: "lua-ls",
            host_uri: "file:///test/doc.md",
            offset: &offset,
            region_end: Some(TEST_REGION_END),
        };
        envelope_item_data(&mut item, &ctx);

        // data should now be wrapped
        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(
            envelope.host_uri, "file:///test/doc.md",
            "host URI must survive the envelope round-trip for resolve routing (#382)"
        );
        assert_eq!(envelope.inner, Some(json!({"resolve_id": 42})));
        assert_eq!(
            envelope.offset,
            EnvelopeOffset {
                line: 3,
                column: 4,
                line_column_offsets: Some(vec![4])
            }
        );

        // strip restores original data
        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.origin, "lua-ls");
        assert_eq!(item.data, Some(json!({"resolve_id": 42})));
    }

    /// A pre-#382 envelope serialized without a `host_uri` field still
    /// deserializes (to an empty host URI), so resolve degrades to root-less
    /// routing rather than failing — `serde(default)` backward compatibility.
    #[test]
    fn envelope_without_host_uri_deserializes_to_empty() {
        let legacy = json!({
            "origin": "lua-ls",
            "inner": null,
            "offset": { "line": 1, "column": 0 }
        });
        let envelope: KakehashiEnvelope =
            serde_json::from_value(legacy).expect("legacy envelope must still deserialize");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(
            envelope.host_uri, "",
            "missing host_uri must default to empty, not fail"
        );
    }

    #[test]
    fn envelope_round_trip_with_none_data() {
        let mut item = CompletionItem {
            label: "pairs".to_string(),
            data: None,
            ..Default::default()
        };
        let offset = RegionOffset::new(3, 4);
        let ctx = EnvelopeContext {
            server_name: "lua-ls",
            host_uri: "file:///test/doc.md",
            offset: &offset,
            region_end: Some(TEST_REGION_END),
        };
        envelope_item_data(&mut item, &ctx);

        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert_eq!(envelope.inner, None);

        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.inner, None);
        assert_eq!(item.data, None);
    }

    #[test]
    fn extract_envelope_returns_none_for_non_envelope_data() {
        let item = CompletionItem {
            label: "test".to_string(),
            data: Some(json!({"some_other_key": "value"})),
            ..Default::default()
        };
        assert!(extract_envelope(&item).is_none());
    }

    #[test]
    fn extract_envelope_returns_none_for_no_data() {
        let item = CompletionItem {
            label: "test".to_string(),
            data: None,
            ..Default::default()
        };
        assert!(extract_envelope(&item).is_none());
    }

    #[test]
    fn strip_envelope_leaves_non_envelope_data_unchanged() {
        let original_data = json!({"custom": true});
        let mut item = CompletionItem {
            label: "test".to_string(),
            data: Some(original_data.clone()),
            ..Default::default()
        };
        assert!(strip_envelope(&mut item).is_none());
        assert_eq!(item.data, Some(original_data));
    }

    #[test]
    fn envelope_offset_deserializes_without_line_column_offsets() {
        // Old envelopes serialized before the line_column_offsets field was added
        // should still deserialize correctly with None for the new field.
        let json = json!({"line": 5, "column": 3});
        let offset: EnvelopeOffset =
            serde_json::from_value(json).expect("should deserialize old format");
        assert_eq!(offset.line, 5);
        assert_eq!(offset.column, 3);
        assert_eq!(offset.line_column_offsets, None);
    }

    #[test]
    fn envelope_offset_round_trips_with_line_column_offsets() {
        let offset = EnvelopeOffset {
            line: 5,
            column: 2,
            line_column_offsets: Some(vec![2, 2, 2]),
        };
        let json = serde_json::to_value(&offset).expect("should serialize");
        let deserialized: EnvelopeOffset =
            serde_json::from_value(json).expect("should deserialize");
        assert_eq!(deserialized, offset);
    }

    #[test]
    fn envelope_offset_without_line_column_offsets_omits_field() {
        // skip_serializing_if = "Option::is_none" should omit the field
        let offset = EnvelopeOffset {
            line: 5,
            column: 0,
            line_column_offsets: None,
        };
        let json = serde_json::to_value(&offset).expect("should serialize");
        assert!(json.get("line_column_offsets").is_none());
    }

    #[test]
    fn transform_drops_prefix_breaking_items_in_blockquote_regions() {
        // Blockquote region (per-line `> ` widths plus the trailing
        // boundary-row zero), content host lines 3-4, region end (5, 0).
        // An item whose primary textEdit inserts a newline behind the prefix
        // would produce an unprefixed continuation line — the whole item must
        // be dropped. A same-line item survives.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "unsafe",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "multi\nline" } },
                { "label": "safe",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "single" } }
            ]
        });

        let list = transform_completion_response_to_host(response, &offset, region_end, None, None)
            .unwrap();

        assert_eq!(list.items.len(), 1, "prefix-breaking item must be dropped");
        assert_eq!(list.items[0].label, "safe");
    }

    #[test]
    fn transform_drops_whole_additional_edit_array_but_keeps_item() {
        // An import-style additionalTextEdit that inserts a raw newline at a
        // prefixed line would break the `> ` prefix — the array drops WHOLE
        // (it can carry paired halves of one operation); the item's primary
        // insertion stays mechanically applicable, though possibly
        // semantically incomplete without its auxiliaries.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "item",
                  "textEdit": { "range": { "start": { "line": 1, "character": 0 },
                                           "end": { "line": 1, "character": 3 } },
                                "newText": "single" },
                  "additionalTextEdits": [
                      { "range": { "start": { "line": 0, "character": 0 },
                                   "end": { "line": 0, "character": 0 } },
                        "newText": "import x\n" },
                      { "range": { "start": { "line": 0, "character": 3 },
                                   "end": { "line": 0, "character": 5 } },
                        "newText": "ok" }
                  ] }
            ]
        });

        let list = transform_completion_response_to_host(response, &offset, region_end, None, None)
            .unwrap();

        assert_eq!(list.items.len(), 1, "item with safe primary edit is kept");
        assert!(
            list.items[0].additional_text_edits.is_none(),
            "an unsafe member drops the whole additionalTextEdits array \
             (halves of one operation must not apply separately)"
        );
    }

    #[test]
    fn transform_drops_prefix_breaking_insert_replace_items() {
        // InsertReplaceEdit variant: reject when EITHER range is unsafe.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "unsafe",
                  "textEdit": { "insert": { "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 3 } },
                                "replace": { "start": { "line": 0, "character": 0 },
                                             "end": { "line": 1, "character": 3 } },
                                "newText": "x" } }
            ]
        });

        let list = transform_completion_response_to_host(response, &offset, region_end, None, None)
            .unwrap();

        assert!(
            list.items.is_empty(),
            "item whose replace range spans a prefixed line must be dropped"
        );
    }

    #[test]
    fn transform_drops_items_whose_edit_escapes_the_region() {
        // Plain fenced region (all-zero offsets — the prefix rules fast-path),
        // content host lines 3-4, region end (5, 0). A stale/oversized range
        // translates past the closing fence; containment must drop the item.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "escapes",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 9, "character": 0 } },
                                "newText": "x" } },
                { "label": "contained",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "y" } }
            ]
        });

        let list = transform_completion_response_to_host(response, &offset, region_end, None, None)
            .unwrap();

        assert_eq!(list.items.len(), 1, "region-escaping item must drop");
        assert_eq!(list.items[0].label, "contained");
    }

    #[test]
    fn transform_drops_multiline_insert_text_fallback_in_prefixed_regions() {
        // No textEdit: the client inserts insertText (or the label) at the
        // completion position. Multi-line fallback text in a prefixed region
        // creates unprefixed lines — drop the item. Single-line fallbacks and
        // multi-line fallbacks in plain (all-zero) regions are kept.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "multiline", "insertText": "for x in y:\n    pass" },
                { "label": "single", "insertText": "print" }
            ]
        });

        let list = transform_completion_response_to_host(
            response.clone(),
            &offset,
            region_end,
            None,
            None,
        )
        .unwrap();
        assert_eq!(list.items.len(), 1, "multiline fallback must drop");
        assert_eq!(list.items[0].label, "single");

        // Same response in a plain fenced region: both survive.
        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let list = transform_completion_response_to_host(response, &plain, region_end, None, None)
            .unwrap();
        assert_eq!(list.items.len(), 2, "plain regions take multiline inserts");
    }

    #[test]
    fn transform_drops_snippet_variables_in_prefixed_regions() {
        // A snippet VARIABLE (`${CLIPBOARD}`) expands client-side and may be
        // multiline — the literal newline scan can't see it, so a prefixed
        // region must fail closed. Numeric tabstops/placeholders expand from
        // literal text and stay allowed; plain regions are exempt.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "clipboard", "insertTextFormat": 2,
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "${CLIPBOARD}" } },
                { "label": "tabstop", "insertTextFormat": 2,
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 3 } },
                                "newText": "print(${1:msg})" } },
                { "label": "fallback-variable", "insertTextFormat": 2,
                  "insertText": "$TM_SELECTED_TEXT" }
            ]
        });

        let list = transform_completion_response_to_host(
            response.clone(),
            &offset,
            region_end,
            None,
            None,
        )
        .unwrap();
        let labels: Vec<_> = list.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["tabstop"],
            "snippet variables must drop in prefixed regions (both textEdit and fallback)"
        );

        // Plain (all-zero) region: everything survives.
        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let list = transform_completion_response_to_host(response, &plain, region_end, None, None)
            .unwrap();
        assert_eq!(list.items.len(), 3, "plain regions are exempt");
    }

    #[test]
    fn snippet_variable_detection_handles_escapes_and_tabstops() {
        assert!(snippet_contains_variable("${CLIPBOARD}"));
        assert!(snippet_contains_variable("$TM_SELECTED_TEXT"));
        assert!(snippet_contains_variable("x${_private}y"));
        assert!(!snippet_contains_variable("$1"));
        assert!(!snippet_contains_variable("${1:placeholder}"));
        assert!(!snippet_contains_variable("plain text"));
        assert!(!snippet_contains_variable("price: \\$100"));
        assert!(
            !snippet_contains_variable("\\$CLIPBOARD"),
            "escaped dollar is literal per the snippet grammar"
        );
    }

    #[test]
    fn mixed_region_allows_multiline_fallback_on_unprefixed_later_lines() {
        // Single-element offsets `[7]`: line 0 starts mid-host-line but its
        // captured content runs to end-of-line in a MULTI-LINE region, and
        // later lines are whole and unprefixed — multi-line no-textEdit
        // insertions are safe on every content line; only a SAME-LINE inline
        // region rejects them. The region-wide any-prefix shortcut used to
        // drop all of these.
        let offset = RegionOffset::new(3, 7);
        let region_end = Position {
            line: 9,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [ { "label": "multiline", "insertText": "a\nb" } ]
        });

        // Requested on host line 5 (virtual line 2, unprefixed): kept.
        let list = transform_completion_response_to_host(
            response.clone(),
            &offset,
            region_end,
            Some(5),
            None,
        )
        .unwrap();
        assert_eq!(list.items.len(), 1, "later-line insertion is safe");

        // Requested on host line 3 (virtual line 0): in a MULTI-LINE region
        // line 0's captured content runs to end-of-line, so the inserted
        // newline splits only injected content — kept.
        let list = transform_completion_response_to_host(
            response.clone(),
            &offset,
            region_end,
            Some(3),
            None,
        )
        .unwrap();
        assert_eq!(list.items.len(), 1, "multi-line region line 0 is safe");

        // Same shape but a SAME-LINE inline region (region end on the start
        // line): the host line continues past the region, so a newline
        // splits it — dropped.
        let inline_end = Position {
            line: 3,
            character: 20,
        };
        let list =
            transform_completion_response_to_host(response, &offset, inline_end, Some(3), None)
                .unwrap();
        assert!(
            list.items.is_empty(),
            "inline-region line-0 insertion would split the host line"
        );
    }

    #[test]
    fn insert_replace_snippet_checks_both_start_lines() {
        // Malformed InsertReplaceEdit with unequal starts in a MIXED-offset
        // region ([0,0,2,0]): the insert range anchors on virtual line 0
        // (host 3 — unprefixed, and host 4 is unprefixed too), the replace
        // range on virtual line 2 (host 5 — prefixed). A minimum-start
        // implementation anchors at the unprefixed line and would ADMIT the
        // snippet variable; checking both starts rejects it.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 2, 0]);
        let region_end = Position {
            line: 7,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [
                { "label": "unequal", "insertTextFormat": 2,
                  "textEdit": { "insert": { "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 3 } },
                                "replace": { "start": { "line": 2, "character": 2 },
                                             "end": { "line": 2, "character": 5 } },
                                "newText": "${CLIPBOARD}" } }
            ]
        });

        let list = transform_completion_response_to_host(response, &offset, region_end, None, None)
            .unwrap();
        assert!(
            list.items.is_empty(),
            "either prefixed start line must reject the snippet variable"
        );
    }

    #[test]
    fn mixed_offset_boundary_rows_reject_multiline_fallbacks() {
        // Mixed per-line offsets [2,0,0] (a blockquote with a lazy
        // continuation line): host 4 is the lazy-continuation CONTENT row
        // (recorded 0); host 5 is the BOUNDARY row, whose real prefix is
        // unrecorded. A multi-line
        // fallback insertion on host 4 must reject — its following row is
        // the boundary; the same shape in an all-zero region has no boundary
        // semantics and passes.
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [ { "label": "multiline", "insertText": "a\nb" } ]
        });

        let mixed = RegionOffset::with_per_line_offsets(3, vec![2, 0, 0]);
        let list = transform_completion_response_to_host(
            response.clone(),
            &mixed,
            region_end,
            Some(4),
            None,
        )
        .unwrap();
        assert!(
            list.items.is_empty(),
            "boundary-adjacent insertion in a prefixed region must drop"
        );

        let plain = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let list =
            transform_completion_response_to_host(response, &plain, region_end, Some(4), None)
                .unwrap();
        assert_eq!(list.items.len(), 1, "all-zero regions keep no boundary");
    }
}
