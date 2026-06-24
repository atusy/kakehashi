//! Code action request handling for bridge connections.
//!
//! First increment (#352): a range-based request (like inlay hint). The request
//! range is translated host->virtual; in the response, each `CodeAction`'s edit
//! ranges and diagnostic ranges are translated virtual->host.
//!
//! Scope / deferred follow-ups:
//! - Request `context.diagnostics` is empty (the handler does not forward the
//!   editor's host-coordinate diagnostics yet).
//! - `WorkspaceEdit` translation handles only the `changes` map. If an action
//!   carries `documentChanges`, the whole `edit` is dropped (set to `None`) and
//!   logged at debug — half-translating `documentChanges` would emit virtual
//!   coordinates to the editor.
//! - `Command` items pass through unchanged; `codeAction/resolve` is not wired,
//!   so `data` passes through but a follow-up resolve request is unhandled.
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::collections::HashMap;
use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionContext, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionResponse, CodeActionTriggerKind, Diagnostic, Range, TextDocumentIdentifier, TextEdit,
    Uri,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};

use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, response_has_jsonrpc_error,
    translate_host_range_to_virtual, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};

impl LanguageServerPool {
    /// Send a code action request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle)
    /// for the full lifecycle, providing code-action-specific request building
    /// and response transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_code_action_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_range: Range,
        only: Option<Vec<CodeActionKind>>,
        trigger_kind: Option<CodeActionTriggerKind>,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<CodeActionResponse>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/codeAction") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_code_action_request(
                    virtual_uri,
                    host_range,
                    only.clone(),
                    trigger_kind,
                    &offset,
                    request_id,
                )
            },
            |response, ctx| {
                transform_code_action_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.offset,
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC code action request for a downstream language server.
///
/// The visible request range is translated host->virtual. `context.diagnostics`
/// is empty for this increment; `only` / `trigger_kind` pass through.
fn build_code_action_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    only: Option<Vec<CodeActionKind>>,
    trigger_kind: Option<CodeActionTriggerKind>,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<CodeActionParams> {
    let mut virtual_range = host_range;
    translate_host_range_to_virtual(&mut virtual_range, offset);

    let params = CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        range: virtual_range,
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only,
            trigger_kind,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/codeAction", params)
}

/// Translate a code-action response from virtual to host coordinates.
///
/// `Command` items pass through unchanged. For each `CodeAction`, edit ranges
/// (`changes` only) and diagnostic ranges are translated; see
/// [`transform_code_action`].
fn transform_code_action_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> Option<CodeActionResponse> {
    if response_has_jsonrpc_error(&response, "textDocument/codeAction") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    let mut actions: CodeActionResponse = serde_json::from_value(result).ok()?;

    for item in &mut actions {
        if let CodeActionOrCommand::CodeAction(action) = item {
            transform_code_action(action, request_virtual_uri, host_uri, offset);
        }
        // CodeActionOrCommand::Command passes through unchanged.
    }

    Some(actions)
}

/// Translate a single `CodeAction`'s edit and diagnostics from virtual to host.
///
/// - `edit`: if it carries `documentChanges`, the whole edit is dropped (set to
///   `None`) and logged — documentChanges translation is a follow-up. Otherwise
///   the `changes` map is translated in place (re-key own virtual URI to host,
///   drop cross-region virtual URIs, keep real files).
/// - `diagnostics`: each diagnostic's main range is translated, and any
///   `relatedInformation` on virtual URIs is dropped (clients cannot resolve
///   virtual URIs), mirroring `diagnostic_cache::transform_region_diagnostic`.
/// - `title`, `kind`, `command`, `is_preferred`, `disabled`, `data` pass through.
fn transform_code_action(
    action: &mut CodeAction,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    if let Some(edit) = &mut action.edit {
        if edit.document_changes.is_some() {
            // documentChanges translation is a follow-up; do not half-translate.
            log::debug!(
                target: "kakehashi::bridge",
                "code action edit carries documentChanges; dropping edit (translation is a follow-up)"
            );
            action.edit = None;
        } else if let Some(changes) = &mut edit.changes {
            transform_changes_map(changes, request_virtual_uri, host_uri, offset);
        }
    }

    if let Some(diagnostics) = &mut action.diagnostics {
        for diagnostic in diagnostics.iter_mut() {
            transform_diagnostic(diagnostic, request_virtual_uri, host_uri, offset);
        }
    }
}

/// Translate the `changes` map: re-key the request's own virtual URI to the host
/// URI (translating ranges), drop cross-region virtual URIs, keep real files.
fn transform_changes_map(
    changes: &mut HashMap<Uri, Vec<TextEdit>>,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    let keys: Vec<Uri> = changes.keys().cloned().collect();

    for key in keys {
        let uri_str = key.as_str();

        // Case 1: Real file URI → keep as-is.
        if !VirtualDocumentUri::is_virtual_uri(uri_str) {
            continue;
        }

        // Case 2: Same virtual URI → translate ranges, re-key to host URI.
        if uri_str == request_virtual_uri {
            if let Some(mut edits) = changes.remove(&key) {
                for edit in &mut edits {
                    translate_virtual_range_to_host(&mut edit.range, offset);
                }
                changes.entry(host_uri.clone()).or_default().extend(edits);
            }
            continue;
        }

        // Case 3: Different virtual URI (cross-region) → drop.
        changes.remove(&key);
    }
}

/// Translate a diagnostic's main range to host coordinates and drop
/// `relatedInformation` entries on virtual URIs (mirrors the diagnostic cache).
fn transform_diagnostic(
    diag: &mut Diagnostic,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    translate_virtual_range_to_host(&mut diag.range, offset);
    if let Some(related) = &mut diag.related_information {
        related.retain_mut(|info| {
            let uri_str = info.location.uri.as_str();
            if !VirtualDocumentUri::is_virtual_uri(uri_str) {
                // Real file → keep as-is.
                return true;
            }
            if uri_str == request_virtual_uri {
                info.location.uri = host_uri.clone();
                translate_virtual_range_to_host(&mut info.location.range, offset);
                return true;
            }
            // Cross-region virtual URI → drop.
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    fn make_host_uri() -> Uri {
        crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        let host_uri = make_host_uri();
        VirtualDocumentUri::new(&host_uri, "lua", "region-0").to_uri_string()
    }

    // ==========================================================================
    // Request builder tests
    // ==========================================================================

    #[test]
    fn code_action_request_uses_virtual_uri() {
        let host_uri = test_host_uri();
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 10,
                character: 0,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 20,
                character: 0,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_code_action_request(
            &virtual_uri,
            host_range,
            None,
            None,
            &RegionOffset::new(5, 0),
            RequestId::new(1),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn code_action_request_translates_range_and_sends_empty_diagnostics() {
        let host_uri = test_host_uri();
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 10,
                character: 5,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 20,
                character: 30,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_code_action_request(
            &virtual_uri,
            host_range,
            None,
            None,
            &RegionOffset::new(8, 0),
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "textDocument/codeAction");
        // Range translated: line 10 - 8 = 2, line 20 - 8 = 12.
        assert_eq!(json["params"]["range"]["start"]["line"], 2);
        assert_eq!(json["params"]["range"]["start"]["character"], 5);
        assert_eq!(json["params"]["range"]["end"]["line"], 12);
        assert_eq!(json["params"]["range"]["end"]["character"], 30);
        // Context diagnostics always empty in this increment.
        assert_eq!(json["params"]["context"]["diagnostics"], json!([]));
    }

    #[test]
    fn code_action_request_passes_through_only_filter() {
        let host_uri = test_host_uri();
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 1,
                character: 0,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 2,
                character: 0,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_code_action_request(
            &virtual_uri,
            host_range,
            Some(vec![CodeActionKind::QUICKFIX]),
            Some(CodeActionTriggerKind::INVOKED),
            &RegionOffset::new(0, 0),
            RequestId::new(1),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["context"]["only"][0], "quickfix");
        assert_eq!(json["params"]["context"]["triggerKind"], 1);
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::without_result(json!({"jsonrpc": "2.0", "id": 42}))]
    fn code_action_returns_none_for_invalid_response(#[case] response: serde_json::Value) {
        let result = transform_code_action_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(5, 0),
        );
        assert!(result.is_none());
    }

    #[test]
    fn code_action_empty_array_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });
        let actions = transform_code_action_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(5, 0),
        );
        assert!(actions.is_some());
        assert!(actions.unwrap().is_empty());
    }

    #[test]
    fn code_action_command_passes_through_unchanged() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "title": "Run it", "command": "do.something", "arguments": [1, 2] }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(10, 0),
        )
        .unwrap();

        assert_eq!(actions.len(), 1);
        match &actions[0] {
            CodeActionOrCommand::Command(cmd) => {
                assert_eq!(cmd.title, "Run it");
                assert_eq!(cmd.command, "do.something");
            }
            CodeActionOrCommand::CodeAction(_) => panic!("Expected Command"),
        }
    }

    #[test]
    fn code_action_changes_rekeys_virtual_drops_cross_region_keeps_real() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let cross_region_uri =
            VirtualDocumentUri::new(&host_uri, "lua", "region-1").to_uri_string();
        let cross: Uri = cross_region_uri.parse().unwrap();
        let real_file_uri = "file:///usr/local/lib/types.lua";

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "title": "Fix it",
                    "kind": "quickfix",
                    "edit": {
                        "changes": {
                            virtual_uri.clone(): [{
                                "range": {
                                    "start": { "line": 0, "character": 5 },
                                    "end": { "line": 0, "character": 10 }
                                },
                                "newText": "fixed"
                            }],
                            cross_region_uri: [{
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 3 }
                                },
                                "newText": "dropped"
                            }],
                            real_file_uri: [{
                                "range": {
                                    "start": { "line": 50, "character": 0 },
                                    "end": { "line": 50, "character": 4 }
                                },
                                "newText": "kept"
                            }]
                        }
                    }
                }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        )
        .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a,
            CodeActionOrCommand::Command(_) => panic!("Expected CodeAction"),
        };
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();

        // Cross-region dropped; host (re-keyed from virtual) + real file remain.
        assert_eq!(changes.len(), 2);
        assert!(
            !changes.contains_key(&cross),
            "cross-region must be dropped"
        );

        // Virtual key re-keyed to host URI, range translated: line 0 + 10 = 10.
        let host_edits = changes.get(&host_uri).expect("host URI key");
        assert_eq!(host_edits[0].range.start.line, 10);
        assert_eq!(host_edits[0].range.end.line, 10);
        assert_eq!(host_edits[0].new_text, "fixed");

        // Real file preserved, range untouched.
        let real: Uri = real_file_uri.parse().unwrap();
        let real_edits = changes.get(&real).expect("real file URI preserved");
        assert_eq!(real_edits[0].range.start.line, 50);
        assert_eq!(real_edits[0].new_text, "kept");
    }

    #[test]
    fn code_action_document_changes_drops_whole_edit() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "title": "Refactor",
                    "edit": {
                        "documentChanges": [
                            {
                                "textDocument": { "uri": virtual_uri, "version": 1 },
                                "edits": [{
                                    "range": {
                                        "start": { "line": 0, "character": 0 },
                                        "end": { "line": 0, "character": 5 }
                                    },
                                    "newText": "x"
                                }]
                            }
                        ]
                    }
                }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        )
        .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a,
            CodeActionOrCommand::Command(_) => panic!("Expected CodeAction"),
        };
        assert!(
            action.edit.is_none(),
            "edit carrying documentChanges must be dropped entirely"
        );
        // Other fields preserved.
        assert_eq!(action.title, "Refactor");
    }

    #[test]
    fn code_action_document_changes_drops_edit_even_with_changes_present() {
        // If BOTH changes and documentChanges are present, the whole edit drops.
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "title": "Mixed",
                    "edit": {
                        "changes": {
                            virtual_uri.clone(): [{
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 1 }
                                },
                                "newText": "y"
                            }]
                        },
                        "documentChanges": [
                            {
                                "textDocument": { "uri": virtual_uri, "version": 1 },
                                "edits": [{
                                    "range": {
                                        "start": { "line": 0, "character": 0 },
                                        "end": { "line": 0, "character": 5 }
                                    },
                                    "newText": "z"
                                }]
                            }
                        ]
                    }
                }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        )
        .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a,
            CodeActionOrCommand::Command(_) => panic!("Expected CodeAction"),
        };
        assert!(
            action.edit.is_none(),
            "documentChanges present drops the edit"
        );
    }

    #[test]
    fn code_action_translates_diagnostic_ranges() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "title": "Fix",
                    "diagnostics": [
                        {
                            "range": {
                                "start": { "line": 2, "character": 0 },
                                "end": { "line": 2, "character": 8 }
                            },
                            "message": "oops"
                        }
                    ]
                }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        )
        .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a,
            CodeActionOrCommand::Command(_) => panic!("Expected CodeAction"),
        };
        let diags = action.diagnostics.as_ref().unwrap();
        // Range translated: line 2 + 10 = 12.
        assert_eq!(diags[0].range.start.line, 12);
        assert_eq!(diags[0].range.end.line, 12);
    }

    #[test]
    fn code_action_diagnostic_drops_virtual_related_information() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let cross_region_uri =
            VirtualDocumentUri::new(&host_uri, "lua", "region-1").to_uri_string();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "title": "Fix",
                    "diagnostics": [
                        {
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 1 }
                            },
                            "message": "oops",
                            "relatedInformation": [
                                {
                                    "location": {
                                        "uri": cross_region_uri,
                                        "range": {
                                            "start": { "line": 0, "character": 0 },
                                            "end": { "line": 0, "character": 1 }
                                        }
                                    },
                                    "message": "elsewhere"
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(5, 0),
        )
        .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a,
            CodeActionOrCommand::Command(_) => panic!("Expected CodeAction"),
        };
        let diags = action.diagnostics.as_ref().unwrap();
        let related = diags[0].related_information.as_ref().unwrap();
        assert!(
            related.is_empty(),
            "cross-region virtual relatedInformation must be dropped"
        );
    }

    #[test]
    fn code_action_preserves_data_and_is_preferred() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "title": "Fix",
                    "isPreferred": true,
                    "data": { "resolveMe": 123 }
                }
            ]
        });

        let actions = transform_code_action_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(5, 0),
        )
        .unwrap();

        let action = match &actions[0] {
            CodeActionOrCommand::CodeAction(a) => a,
            CodeActionOrCommand::Command(_) => panic!("Expected CodeAction"),
        };
        assert_eq!(action.is_preferred, Some(true));
        assert_eq!(action.data, Some(json!({ "resolveMe": 123 })));
    }
}
