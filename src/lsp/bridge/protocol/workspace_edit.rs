//! WorkspaceEdit coordinate transformation from virtual to host documents.
//!
//! Shared by response paths that carry a `WorkspaceEdit` produced against a
//! virtual document — rename today; codeAction and workspace/applyEdit will
//! reuse it (#568). Per LSP spec a WorkspaceEdit may carry edits via `changes`
//! (URI→TextEdit map) or `documentChanges`; both are handled.

use std::collections::HashMap;

use tower_lsp_server::ls_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use super::translation::{RegionOffset, translate_virtual_range_to_host};
use super::virtual_uri::VirtualDocumentUri;

/// Transform a WorkspaceEdit in place from virtual to host document coordinates.
///
/// For each URI: real files pass through, the request's own virtual URI is
/// re-keyed to the host URI (ranges translated, stale virtual-doc versions
/// dropped), and other (cross-region) virtual URIs are filtered out.
pub(crate) fn transform_workspace_edit_to_host(
    edit: &mut WorkspaceEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) {
    // Transform changes map: { [uri: string]: TextEdit[] }
    if let Some(changes) = &mut edit.changes {
        transform_changes_map(changes, request_virtual_uri, host_uri, offset);
    }

    // Transform documentChanges array
    if let Some(doc_changes) = &mut edit.document_changes {
        transform_document_changes(doc_changes, request_virtual_uri, host_uri, offset);
    }
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
/// File operations (CreateFile, RenameFile, DeleteFile) are preserved as-is.
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
