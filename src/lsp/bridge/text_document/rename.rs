//! Rename request handling for bridge connections.
//!
//! This module provides rename request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;
use std::collections::HashMap;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, Position, TextDocumentEdit, TextEdit, Uri,
    WorkspaceEdit,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_position_based_request};

impl LanguageServerPool {
    /// Send a rename request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing rename-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_rename_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        new_name: &str,
        upstream_request_id: UpstreamId,
    ) -> io::Result<Option<WorkspaceEdit>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/rename") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            region_start_line,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_rename_request(
                    virtual_uri,
                    host_position,
                    region_start_line,
                    new_name,
                    request_id,
                )
            },
            |response, ctx| {
                transform_workspace_edit_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.region_start_line,
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC rename request for a downstream language server.
///
/// Rename extends the position-based request pattern with an additional
/// `newName` parameter that specifies the new name for the symbol.
///
/// # Coordinate Translation
///
/// Uses `build_position_based_request` to handle host→virtual position
/// translation, then adds the `newName` field to params.
fn build_rename_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: tower_lsp_server::ls_types::Position,
    region_start_line: u32,
    new_name: &str,
    request_id: RequestId,
) -> serde_json::Value {
    let mut request = build_position_based_request(
        virtual_uri,
        host_position,
        region_start_line,
        request_id,
        "textDocument/rename",
    );

    // Add the newName parameter required by rename request
    if let Some(params) = request.get_mut("params") {
        params["newName"] = serde_json::json!(new_name);
    }

    request
}

/// Transform a WorkspaceEdit response from virtual to host document coordinates.
///
/// WorkspaceEdit can have two formats per LSP spec:
/// 1. `changes: { [uri: string]: TextEdit[] }` - A map from URI to text edits
/// 2. `documentChanges: (TextDocumentEdit | CreateFile | RenameFile | DeleteFile)[]`
///
/// This function handles three cases for each URI in the response:
/// 1. **Real file URI** (not a virtual URI): Preserved as-is with original coordinates
/// 2. **Same virtual URI as request**: Transformed using request's context
/// 3. **Different virtual URI** (cross-region): Filtered out from results
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `request_virtual_uri` - The virtual URI from the request
/// * `host_uri` - The pre-parsed host URI to use in transformed responses
/// * `region_start_line` - Line offset to add when transforming to host coordinates
fn transform_workspace_edit_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) -> Option<WorkspaceEdit> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/rename: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse into typed WorkspaceEdit
    let mut edit: WorkspaceEdit = serde_json::from_value(result).ok()?;

    // Transform changes map: { [uri: string]: TextEdit[] }
    if let Some(changes) = &mut edit.changes {
        transform_changes_map(changes, request_virtual_uri, host_uri, region_start_line);
    }

    // Transform documentChanges array
    if let Some(doc_changes) = &mut edit.document_changes {
        transform_document_changes(
            doc_changes,
            request_virtual_uri,
            host_uri,
            region_start_line,
        );
    }

    Some(edit)
}

/// Transform the `changes` map in a WorkspaceEdit.
///
/// Re-keys virtual URIs to host URI and transforms TextEdit ranges.
/// Cross-region virtual URIs are removed entirely.
fn transform_changes_map(
    changes: &mut HashMap<Uri, Vec<TextEdit>>,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) {
    // Collect keys to process (can't modify HashMap keys in-place)
    let keys: Vec<Uri> = changes.keys().cloned().collect();

    for key in keys {
        let uri_str = key.as_str();

        // Case 1: Real file URI → keep as-is
        if !VirtualDocumentUri::is_virtual_uri(uri_str) {
            continue;
        }

        // Case 2: Same virtual URI → transform ranges, re-key to host URI
        if uri_str == request_virtual_uri {
            if let Some(mut edits) = changes.remove(&key) {
                for edit in &mut edits {
                    edit.range.start.line = edit.range.start.line.saturating_add(region_start_line);
                    edit.range.end.line = edit.range.end.line.saturating_add(region_start_line);
                }
                changes.entry(host_uri.clone()).or_default().extend(edits);
            }
            continue;
        }

        // Case 3: Different virtual URI (cross-region) → filter out
        changes.remove(&key);
    }
}

/// Transform the `documentChanges` array in a WorkspaceEdit.
///
/// Handles both `Edits(Vec<TextDocumentEdit>)` and
/// `Operations(Vec<DocumentChangeOperation>)` variants.
/// File operations (CreateFile, RenameFile, DeleteFile) are preserved as-is.
fn transform_document_changes(
    doc_changes: &mut DocumentChanges,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) {
    match doc_changes {
        DocumentChanges::Edits(edits) => {
            edits.retain_mut(|edit| {
                transform_text_document_edit(edit, request_virtual_uri, host_uri, region_start_line)
            });
        }
        DocumentChanges::Operations(ops) => {
            ops.retain_mut(|op| match op {
                DocumentChangeOperation::Edit(edit) => transform_text_document_edit(
                    edit,
                    request_virtual_uri,
                    host_uri,
                    region_start_line,
                ),
                DocumentChangeOperation::Op(_) => true, // File operations preserved
            });
        }
    }
}

/// Transform a single TextDocumentEdit's URI and edit ranges.
///
/// Returns `true` if the edit should be kept, `false` if it should be filtered out.
fn transform_text_document_edit(
    edit: &mut TextDocumentEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) -> bool {
    let uri_str = edit.text_document.uri.as_str();

    // Case 1: Real file URI → keep as-is
    if !VirtualDocumentUri::is_virtual_uri(uri_str) {
        return true;
    }

    // Case 2: Same virtual URI → transform
    if uri_str == request_virtual_uri {
        edit.text_document.uri = host_uri.clone();
        for one_of in &mut edit.edits {
            let text_edit = match one_of {
                OneOf::Left(text_edit) => text_edit,
                OneOf::Right(annotated_edit) => &mut annotated_edit.text_edit,
            };
            text_edit.range.start.line =
                text_edit.range.start.line.saturating_add(region_start_line);
            text_edit.range.end.line = text_edit.range.end.line.saturating_add(region_start_line);
        }
        return true;
    }

    // Case 3: Cross-region → filter out
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ==========================================================================
    // Rename request builder tests
    // ==========================================================================

    #[test]
    fn rename_request_uses_virtual_uri() {
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let host_position = Position {
            line: 5,
            character: 10,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request =
            build_rename_request(&virtual_uri, host_position, 3, "newName", RequestId::new(1));

        let uri_str = request["params"]["textDocument"]["uri"].as_str().unwrap();
        assert!(
            VirtualDocumentUri::is_virtual_uri(uri_str),
            "Request should use a virtual URI: {}",
            uri_str
        );
        assert!(
            uri_str.ends_with(".lua"),
            "Virtual URI should have .lua extension: {}",
            uri_str
        );
    }

    #[test]
    fn rename_request_translates_position_and_includes_new_name() {
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let host_position = Position {
            line: 5,
            character: 10,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_rename_request(
            &virtual_uri,
            host_position,
            3,
            "renamedVariable",
            RequestId::new(42),
        );

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 42);
        assert_eq!(request["method"], "textDocument/rename");
        // Position translated: line 5 - 3 = 2
        assert_eq!(request["params"]["position"]["line"], 2);
        assert_eq!(request["params"]["position"]["character"], 10);
        // newName included
        assert_eq!(request["params"]["newName"], "renamedVariable");
    }

    // ==========================================================================
    // Rename response transformation tests
    // ==========================================================================

    fn make_host_uri() -> Uri {
        use url::Url;
        crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        let host_uri = make_host_uri();
        VirtualDocumentUri::new(&host_uri, "lua", "region-0").to_uri_string()
    }

    #[test]
    fn workspace_edit_with_null_result_returns_none() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let result = transform_workspace_edit_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            5,
        );

        assert!(result.is_none());
    }

    #[test]
    fn workspace_edit_without_result_returns_none() {
        let response = json!({ "jsonrpc": "2.0", "id": 42 });

        let result = transform_workspace_edit_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            5,
        );

        assert!(result.is_none());
    }

    #[test]
    fn workspace_edit_changes_transforms_ranges_and_rekeys_uri() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "changes": {
                    virtual_uri.clone(): [
                        {
                            "range": {
                                "start": { "line": 0, "character": 5 },
                                "end": { "line": 0, "character": 10 }
                            },
                            "newText": "newName"
                        },
                        {
                            "range": {
                                "start": { "line": 3, "character": 0 },
                                "end": { "line": 3, "character": 7 }
                            },
                            "newText": "newName"
                        }
                    ]
                }
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 10)
            .unwrap();

        let changes = edit.changes.unwrap();
        // Virtual URI key should be replaced with host URI
        assert!(!changes.contains_key(&virtual_uri.as_str().parse::<Uri>().unwrap()));
        let edits = changes.get(&host_uri).expect("Should have host URI key");
        assert_eq!(edits.len(), 2);
        // Ranges transformed: line 0 + 10 = 10, line 3 + 10 = 13
        assert_eq!(edits[0].range.start.line, 10);
        assert_eq!(edits[0].range.end.line, 10);
        assert_eq!(edits[1].range.start.line, 13);
        assert_eq!(edits[1].range.end.line, 13);
    }

    #[test]
    fn workspace_edit_changes_cross_region_filtered_out() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let cross_region_uri =
            VirtualDocumentUri::new(&host_uri, "lua", "region-1").to_uri_string();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "changes": {
                    virtual_uri.clone(): [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "kept"
                    }],
                    cross_region_uri: [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "filtered"
                    }]
                }
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 5)
            .unwrap();

        let changes = edit.changes.unwrap();
        assert_eq!(
            changes.len(),
            1,
            "Cross-region entry should be filtered out"
        );
        assert!(changes.contains_key(&host_uri));
    }

    #[test]
    fn workspace_edit_changes_real_file_uri_preserved() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let real_file_uri = "file:///usr/local/lib/types.lua";

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "changes": {
                    real_file_uri: [{
                        "range": {
                            "start": { "line": 50, "character": 0 },
                            "end": { "line": 50, "character": 5 }
                        },
                        "newText": "newName"
                    }]
                }
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 10)
            .unwrap();

        let changes = edit.changes.unwrap();
        let real_uri: Uri = real_file_uri.parse().unwrap();
        let edits = changes
            .get(&real_uri)
            .expect("Real file URI should be preserved");
        // Range NOT transformed for real file
        assert_eq!(edits[0].range.start.line, 50);
    }

    #[test]
    fn workspace_edit_changes_merges_virtual_and_real_uri_edits() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        // Downstream server returns edits under both the host URI (real file)
        // and the virtual URI. The virtual-URI edits get re-keyed to host URI,
        // so they must be merged with—not overwrite—the existing host-URI edits.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "changes": {
                    host_uri.to_string(): [{
                        "range": {
                            "start": { "line": 100, "character": 0 },
                            "end": { "line": 100, "character": 5 }
                        },
                        "newText": "fromReal"
                    }],
                    virtual_uri.clone(): [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "fromVirtual"
                    }]
                }
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 10)
            .unwrap();

        let changes = edit.changes.unwrap();
        let edits = changes.get(&host_uri).expect("Should have host URI key");
        // Both edits should be present: the real-file edit and the transformed virtual edit
        assert_eq!(edits.len(), 2, "Real and virtual edits should be merged");
        // Real-file edit: range untouched
        assert_eq!(edits[0].range.start.line, 100);
        assert_eq!(edits[0].new_text, "fromReal");
        // Virtual edit: range transformed (0 + 10 = 10)
        assert_eq!(edits[1].range.start.line, 10);
        assert_eq!(edits[1].new_text, "fromVirtual");
    }

    #[test]
    fn workspace_edit_document_changes_transforms_edits_variant() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "documentChanges": [
                    {
                        "textDocument": { "uri": virtual_uri, "version": 1 },
                        "edits": [
                            {
                                "range": {
                                    "start": { "line": 2, "character": 0 },
                                    "end": { "line": 2, "character": 5 }
                                },
                                "newText": "newName"
                            }
                        ]
                    }
                ]
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 10)
            .unwrap();

        let doc_changes = edit.document_changes.unwrap();
        match doc_changes {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits.len(), 1);
                assert_eq!(edits[0].text_document.uri, host_uri);
                match &edits[0].edits[0] {
                    OneOf::Left(text_edit) => {
                        assert_eq!(text_edit.range.start.line, 12); // 2 + 10
                    }
                    OneOf::Right(_) => panic!("Expected Left(TextEdit)"),
                }
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn workspace_edit_document_changes_cross_region_filtered() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let cross_region_uri =
            VirtualDocumentUri::new(&host_uri, "lua", "region-1").to_uri_string();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "documentChanges": [
                    {
                        "textDocument": { "uri": virtual_uri, "version": 1 },
                        "edits": [{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 5 }
                            },
                            "newText": "kept"
                        }]
                    },
                    {
                        "textDocument": { "uri": cross_region_uri, "version": 1 },
                        "edits": [{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 5 }
                            },
                            "newText": "filtered"
                        }]
                    }
                ]
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 5)
            .unwrap();

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits.len(), 1, "Cross-region edit should be filtered out");
                assert_eq!(edits[0].text_document.uri, host_uri);
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn workspace_edit_changes_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "changes": {
                    virtual_uri.clone(): [
                        {
                            "range": {
                                "start": { "line": u32::MAX, "character": 0 },
                                "end": { "line": u32::MAX, "character": 5 }
                            },
                            "newText": "newName"
                        }
                    ]
                }
            }
        });
        let region_start_line = 10;

        let edit = transform_workspace_edit_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            region_start_line,
        )
        .unwrap();

        let changes = edit.changes.unwrap();
        let edits = changes.get(&host_uri).expect("Should have host URI key");
        assert_eq!(edits.len(), 1);
        assert_eq!(
            edits[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
        assert_eq!(
            edits[0].range.end.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }

    #[test]
    fn workspace_edit_document_changes_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "documentChanges": [
                    {
                        "textDocument": { "uri": virtual_uri, "version": 1 },
                        "edits": [
                            {
                                "range": {
                                    "start": { "line": u32::MAX, "character": 0 },
                                    "end": { "line": u32::MAX, "character": 5 }
                                },
                                "newText": "newName"
                            }
                        ]
                    }
                ]
            }
        });
        let region_start_line = 10;

        let edit = transform_workspace_edit_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            region_start_line,
        )
        .unwrap();

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits.len(), 1);
                match &edits[0].edits[0] {
                    OneOf::Left(text_edit) => {
                        assert_eq!(
                            text_edit.range.start.line,
                            u32::MAX,
                            "Overflow should saturate at u32::MAX, not panic"
                        );
                        assert_eq!(
                            text_edit.range.end.line,
                            u32::MAX,
                            "Overflow should saturate at u32::MAX, not panic"
                        );
                    }
                    OneOf::Right(_) => panic!("Expected Left(TextEdit)"),
                }
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn workspace_edit_empty_changes_returns_empty() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "changes": {}
            }
        });

        let edit = transform_workspace_edit_response_to_host(response, &virtual_uri, &host_uri, 5)
            .unwrap();

        assert!(edit.changes.unwrap().is_empty());
    }
}
