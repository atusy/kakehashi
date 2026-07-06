//! WorkspaceEdit coordinate transformation from virtual to host documents.
//!
//! Shared by response paths that carry a `WorkspaceEdit` produced against a
//! virtual document — rename today; codeAction and workspace/applyEdit will
//! reuse it (#568). Per LSP spec a WorkspaceEdit may carry edits via `changes`
//! (URI→TextEdit map) or `documentChanges`; both are handled.

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp, TextDocumentEdit, TextEdit, Uri,
    WorkspaceEdit,
};

use super::translation::{RegionOffset, translate_virtual_range_to_host};
use super::virtual_uri::VirtualDocumentUri;

/// Transform a WorkspaceEdit in place from virtual to host document coordinates.
///
/// For text edits: real-file URIs pass through, the request's own virtual URI
/// is re-keyed to the host URI (ranges translated, stale virtual-doc versions
/// dropped), and other (cross-region) virtual URIs are filtered out.
///
/// Returns `false` when the edit cannot be represented in host coordinates:
/// a file operation (create/rename/delete) references a virtual URI. The
/// spec applies `documentChanges` in order, so dropping just the op (e.g. a
/// virtual→real rename) would misdirect later edits at an unrelated existing
/// file — the caller must discard the whole edit instead.
pub(crate) fn transform_workspace_edit_to_host(
    edit: &mut WorkspaceEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> bool {
    if let Some(DocumentChanges::Operations(ops)) = &edit.document_changes {
        let has_virtual_file_op = ops.iter().any(|op| match op {
            DocumentChangeOperation::Op(op) => !resource_op_targets_real_files_only(op),
            DocumentChangeOperation::Edit(_) => false,
        });
        if has_virtual_file_op {
            return false;
        }
    }

    // Transform changes map: { [uri: string]: TextEdit[] }
    if let Some(changes) = &mut edit.changes {
        transform_changes_map(changes, request_virtual_uri, host_uri, offset);
    }

    // Transform documentChanges array
    if let Some(doc_changes) = &mut edit.document_changes {
        transform_document_changes(doc_changes, request_virtual_uri, host_uri, offset);
    }

    true
}

/// Transform the `changes` map in a WorkspaceEdit.
///
/// Re-keys virtual URIs to host URI and transforms TextEdit ranges.
/// Cross-region virtual URIs are removed entirely.
fn transform_changes_map(
    changes: &mut HashMap<Uri, Vec<TextEdit>>,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
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
                    translate_virtual_range_to_host(&mut edit.range, offset);
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
/// File operations (CreateFile, RenameFile, DeleteFile) are preserved as-is;
/// the caller has already rejected edits whose file ops reference virtual
/// URIs.
fn transform_document_changes(
    doc_changes: &mut DocumentChanges,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    match doc_changes {
        DocumentChanges::Edits(edits) => {
            edits.retain_mut(|edit| {
                transform_text_document_edit(edit, request_virtual_uri, host_uri, offset)
            });
        }
        DocumentChanges::Operations(ops) => {
            ops.retain_mut(|op| match op {
                DocumentChangeOperation::Edit(edit) => {
                    transform_text_document_edit(edit, request_virtual_uri, host_uri, offset)
                }
                DocumentChangeOperation::Op(_) => true, // Pre-validated real-file ops
            });
        }
    }
}

/// Whether a file operation references only real (non-virtual) URIs.
fn resource_op_targets_real_files_only(op: &ResourceOp) -> bool {
    match op {
        ResourceOp::Create(create) => !VirtualDocumentUri::is_virtual_uri(create.uri.as_str()),
        ResourceOp::Rename(rename) => {
            !VirtualDocumentUri::is_virtual_uri(rename.old_uri.as_str())
                && !VirtualDocumentUri::is_virtual_uri(rename.new_uri.as_str())
        }
        ResourceOp::Delete(delete) => !VirtualDocumentUri::is_virtual_uri(delete.uri.as_str()),
    }
}

/// Transform a single TextDocumentEdit's URI and edit ranges.
///
/// Returns `true` if the edit should be kept, `false` if it should be filtered out.
fn transform_text_document_edit(
    edit: &mut TextDocumentEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> bool {
    let uri_str = edit.text_document.uri.as_str();

    // Case 1: Real file URI → keep as-is
    if !VirtualDocumentUri::is_virtual_uri(uri_str) {
        return true;
    }

    // Case 2: Same virtual URI → transform
    if uri_str == request_virtual_uri {
        edit.text_document.uri = host_uri.clone();
        // The version counted the virtual document; against the host URI it
        // would make clients reject the edit as stale.
        edit.text_document.version = None;
        for one_of in &mut edit.edits {
            let text_edit = match one_of {
                OneOf::Left(text_edit) => text_edit,
                OneOf::Right(annotated_edit) => &mut annotated_edit.text_edit,
            };
            translate_virtual_range_to_host(&mut text_edit.range, offset);
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

    fn make_host_uri() -> Uri {
        crate::lsp::lsp_impl::url_to_uri(&url::Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        VirtualDocumentUri::new(&make_host_uri(), "lua", "region-0").to_uri_string()
    }

    fn parse_workspace_edit(value: serde_json::Value) -> WorkspaceEdit {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn document_changes_operations_preserves_file_ops_and_transforms_edits() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let mut edit = parse_workspace_edit(json!({
            "documentChanges": [
                { "kind": "create", "uri": "file:///new.lua" },
                {
                    "textDocument": { "uri": virtual_uri, "version": 3 },
                    "edits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "newName"
                    }]
                }
            ]
        }));

        transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        match edit.document_changes.unwrap() {
            DocumentChanges::Operations(ops) => {
                assert_eq!(ops.len(), 2, "file op and edit must both survive");
                assert!(
                    matches!(&ops[0], DocumentChangeOperation::Op(_)),
                    "file operation must pass through untouched"
                );
                match &ops[1] {
                    DocumentChangeOperation::Edit(edit) => {
                        assert_eq!(edit.text_document.uri, host_uri);
                        assert_eq!(edit.text_document.version, None);
                    }
                    DocumentChangeOperation::Op(_) => panic!("Expected Edit operation"),
                }
            }
            DocumentChanges::Edits(_) => panic!("Expected Operations variant"),
        }
    }

    #[test]
    fn document_changes_rejects_whole_edit_on_virtual_uri_file_ops() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // A file op against a virtual URI would leak the synthetic resource
        // to the client, and documentChanges apply IN ORDER — dropping just
        // the op (e.g. a virtual→real rename) would misdirect later edits at
        // an unrelated existing file. The whole edit must be rejected.
        let mut edit = parse_workspace_edit(json!({
            "documentChanges": [
                { "kind": "rename", "oldUri": virtual_uri, "newUri": "file:///renamed.lua" },
                {
                    "textDocument": { "uri": "file:///renamed.lua", "version": null },
                    "edits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "dependsOnRename"
                    }]
                }
            ]
        }));

        let representable = transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        assert!(
            !representable,
            "a virtual-URI file op must reject the whole WorkspaceEdit"
        );
    }

    #[rstest::rstest]
    #[case::create(json!({ "kind": "create", "uri": "kakehashi://virtual" }))]
    #[case::rename_new(
        json!({ "kind": "rename", "oldUri": "file:///a.lua", "newUri": "kakehashi://virtual" })
    )]
    #[case::delete(json!({ "kind": "delete", "uri": "kakehashi://virtual" }))]
    fn document_changes_rejects_each_virtual_file_op_kind(#[case] mut op: serde_json::Value) {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // Substitute a well-formed virtual URI for the placeholder
        for key in ["uri", "oldUri", "newUri"] {
            if op.get(key).is_some_and(|v| v == "kakehashi://virtual") {
                op[key] = json!(virtual_uri.clone());
            }
        }
        let mut edit = parse_workspace_edit(json!({ "documentChanges": [op] }));

        let representable = transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        assert!(!representable);
    }

    #[test]
    fn document_changes_translates_annotated_edits() {
        use tower_lsp_server::ls_types::{
            AnnotatedTextEdit, OptionalVersionedTextDocumentIdentifier, Position, Range,
        };

        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // Constructed in code: serde's untagged OneOf parses annotated edits as
        // plain TextEdits (unknown fields are ignored), so responses
        // deserialized from JSON never reach Right and silently lose their
        // annotationId (an upstream ls-types limitation, tracked in #568
        // checklist item 7). This pins the arm for typed construction.
        let mut edit = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: virtual_uri.parse().unwrap(),
                    version: Some(1),
                },
                edits: vec![OneOf::Right(AnnotatedTextEdit {
                    text_edit: TextEdit {
                        range: Range {
                            start: Position {
                                line: 2,
                                character: 0,
                            },
                            end: Position {
                                line: 2,
                                character: 5,
                            },
                        },
                        new_text: "newName".to_string(),
                    },
                    annotation_id: "refactor".to_string(),
                })],
            }])),
            ..Default::default()
        };

        transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => match &edits[0].edits[0] {
                OneOf::Right(annotated) => {
                    assert_eq!(annotated.text_edit.range.start.line, 12); // 2 + 10
                    assert_eq!(annotated.annotation_id, "refactor");
                }
                OneOf::Left(_) => panic!("Expected Right(AnnotatedTextEdit)"),
            },
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn document_changes_real_file_edit_preserved_untouched() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let real_uri = "file:///usr/local/lib/types.lua";
        let mut edit = parse_workspace_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": real_uri, "version": 5 },
                "edits": [{
                    "range": {
                        "start": { "line": 50, "character": 0 },
                        "end": { "line": 50, "character": 5 }
                    },
                    "newText": "newName"
                }]
            }]
        }));

        transform_workspace_edit_to_host(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
        );

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits[0].text_document.uri.as_str(), real_uri);
                assert_eq!(
                    edits[0].text_document.version,
                    Some(5),
                    "real-file version must survive untouched"
                );
                match &edits[0].edits[0] {
                    OneOf::Left(text_edit) => {
                        assert_eq!(text_edit.range.start.line, 50, "real-file range untouched");
                    }
                    OneOf::Right(_) => panic!("Expected Left(TextEdit)"),
                }
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }
}
