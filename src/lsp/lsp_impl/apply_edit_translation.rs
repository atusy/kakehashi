//! Virtual→host translation for inbound `workspace/applyEdit` requests (#568).
//!
//! A bridged downstream server only knows the *virtual* document it was handed,
//! so a `workspace/applyEdit` it issues may carry edits keyed by a virtual URI
//! in virtual coordinates. Before the bridge forwards the request to the
//! editor, [`ApplyEditTranslator`] rewrites those edits back to the host
//! document via the shared
//! [`transform_workspace_edit_to_host`] (the same transform rename responses
//! use), so the editor applies the edit to the real file at the right spot.
//!
//! The host/virt distinction is made from the edit itself, not the connection:
//! an edit that touches **no** virtual URIs (every edit from a host-layer
//! connection, and real-file-only edits from virt connections) is forwarded
//! verbatim. An edit that touches exactly one virtual document is translated
//! against that region's live offset (rebuilt exactly as goto/showDocument do,
//! via [`resolve_region_offset`](super::region_offset::resolve_region_offset)).
//!
//! Unlike `window/showDocument` — which degrades by dropping the selection —
//! an applyEdit whose coordinates can't be trusted must **not** be forwarded:
//! a mistranslated edit corrupts the user's buffer. Untranslatable edits
//! (unknown/stale virtual URI, region invalidated by edits, file operations on
//! a virtual document, or an edit spanning multiple virtual documents) are
//! rejected with `Err(failure_reason)`; the caller answers the downstream
//! `applied: false` locally without contacting the editor.
//!
//! `label` and `changeAnnotations` pass through untouched (the transform only
//! rewrites URIs/ranges/versions).
//!
//! [`transform_workspace_edit_to_host`]: crate::lsp::bridge::transform_workspace_edit_to_host

use std::sync::Arc;

use tower_lsp_server::ls_types::{
    ApplyWorkspaceEditParams, DocumentChangeOperation, DocumentChanges, Position, ResourceOp, Uri,
    WorkspaceEdit,
};

use crate::document::DocumentStore;
use crate::language::LanguageCoordinator;
use crate::lsp::bridge::{
    BridgeCoordinator, RegionOffset, VirtualDocumentUri, transform_workspace_edit_to_host,
    workspace_edit_within_region,
};

use super::region_offset::resolve_region_offset;

/// Translates `workspace/applyEdit` params whose edit targets a virtual
/// document back to the host document + host coordinates. Holds shared
/// (cheaply cloneable) handles to the document store, language coordinator,
/// and bridge so it can run off the forwarding loop without the `Kakehashi`
/// receiver — same shape as
/// [`ShowDocumentTranslator`](super::show_document_translation::ShowDocumentTranslator).
pub(super) struct ApplyEditTranslator {
    documents: Arc<DocumentStore>,
    language: Arc<LanguageCoordinator>,
    bridge: Arc<BridgeCoordinator>,
}

impl ApplyEditTranslator {
    pub(super) fn new(
        documents: Arc<DocumentStore>,
        language: Arc<LanguageCoordinator>,
        bridge: Arc<BridgeCoordinator>,
    ) -> Self {
        Self {
            documents,
            language,
            bridge,
        }
    }

    /// Translate a virtual-document applyEdit to host coordinates; forward
    /// `params` unchanged when the edit touches no virtual URIs. `Err` carries
    /// the `failureReason` for a local `applied: false` answer — the edit must
    /// not reach the editor. See the module docs for the exact behavior.
    pub(super) async fn translate(
        &self,
        mut params: ApplyWorkspaceEditParams,
    ) -> Result<ApplyWorkspaceEditParams, String> {
        let virtual_uris = collect_virtual_uris(&params.edit);
        let virtual_uri = match virtual_uris.as_slice() {
            // No virtual URIs: a host-layer connection's edit (real URIs
            // throughout), or a virt connection editing only real files.
            [] => return Ok(params),
            [uri] => uri,
            // The shared transform is scoped to one region (it filters other
            // virtual URIs as cross-region); translating each region against
            // its own offset would still interleave documentChanges from
            // different transforms. A partially-applied edit is worse than
            // none, so reject the whole edit.
            _ => {
                return Err("kakehashi: the edit spans multiple injected regions; \
                     it cannot be applied to the host document"
                    .to_string());
            }
        };

        let Some((host_url, region_id)) = self.bridge.resolve_virtual_uri(virtual_uri).await else {
            return Err(format!(
                "kakehashi: the edit targets an unknown virtual document: {virtual_uri}"
            ));
        };
        let Ok(host_uri) = super::url_to_uri(&host_url) else {
            return Err(format!(
                "kakehashi: the virtual document's host URI is unmappable: {host_url}"
            ));
        };
        let Some((offset, region_end)) = resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            &host_url,
            &region_id,
        ) else {
            // The region moved or was removed since the downstream produced
            // the edit; translating against a stale offset would edit the
            // wrong host text.
            return Err(
                "kakehashi: the injected region changed before the edit could be applied"
                    .to_string(),
            );
        };

        transform_params_to_host(&mut params, virtual_uri, &host_uri, &offset, region_end)?;
        Ok(params)
    }
}

/// Rewrite `params.edit` to host coordinates via the shared WorkspaceEdit
/// transform. Pure mutation, split out so it can be unit-tested without a live
/// parse. `Err` when the edit performs file operations on a virtual document
/// (the transform's whole-edit reject) or when a translated range escapes the
/// region end (`region_end`) — a stale/malformed downstream edit whose range
/// runs past the region would otherwise land in unrelated host text after the
/// fence and corrupt the buffer.
fn transform_params_to_host(
    params: &mut ApplyWorkspaceEditParams,
    virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
) -> Result<(), String> {
    if !transform_workspace_edit_to_host(&mut params.edit, virtual_uri, host_uri, offset) {
        return Err(
            "kakehashi: the edit performs file operations on a virtual document".to_string(),
        );
    }
    if !workspace_edit_within_region(&params.edit, host_uri, region_end) {
        return Err(
            "kakehashi: the edit extends past the injected region; it cannot be applied to the \
             host document without corrupting surrounding text"
                .to_string(),
        );
    }
    Ok(())
}

/// Collect the distinct virtual-document URIs an edit touches, across the
/// `changes` map, `documentChanges` text edits, and file-operation URIs.
/// Deduplicated (a URI appearing in both shapes counts once); order follows
/// discovery and is only meaningful for the single-element case.
///
/// A text-edit entry (`changes` value or a `TextDocumentEdit`) with an EMPTY
/// edit vector is a no-op and does NOT count as touching its URI: an edit that
/// is real-file-only but carries a stray empty virtual entry must forward
/// verbatim, not be routed down the virtual-translation path (and then fail
/// `applied: false`). File operations always count — a create/rename/delete is
/// a real change even with no accompanying text edits.
fn collect_virtual_uris(edit: &WorkspaceEdit) -> Vec<String> {
    let mut uris: Vec<String> = Vec::new();
    let mut push = |uri: &str| {
        if VirtualDocumentUri::is_virtual_uri(uri) && !uris.iter().any(|u| u == uri) {
            uris.push(uri.to_string());
        }
    };

    if let Some(changes) = &edit.changes {
        for (uri, edits) in changes {
            if !edits.is_empty() {
                push(uri.as_str());
            }
        }
    }
    match &edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => {
            for edit in edits {
                if !edit.edits.is_empty() {
                    push(edit.text_document.uri.as_str());
                }
            }
        }
        Some(DocumentChanges::Operations(ops)) => {
            for op in ops {
                match op {
                    DocumentChangeOperation::Edit(edit) if !edit.edits.is_empty() => {
                        push(edit.text_document.uri.as_str())
                    }
                    DocumentChangeOperation::Edit(_) => {}
                    DocumentChangeOperation::Op(ResourceOp::Create(create)) => {
                        push(create.uri.as_str())
                    }
                    DocumentChangeOperation::Op(ResourceOp::Rename(rename)) => {
                        push(rename.old_uri.as_str());
                        push(rename.new_uri.as_str());
                    }
                    DocumentChangeOperation::Op(ResourceOp::Delete(delete)) => {
                        push(delete.uri.as_str())
                    }
                }
            }
        }
        None => {}
    }
    uris
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    fn translator() -> ApplyEditTranslator {
        ApplyEditTranslator::new(
            Arc::new(DocumentStore::new()),
            Arc::new(LanguageCoordinator::new()),
            Arc::new(BridgeCoordinator::new()),
        )
    }

    fn host_uri() -> Uri {
        Uri::from_str("file:///project/doc.md").unwrap()
    }

    fn virtual_uri(region_id: &str) -> String {
        VirtualDocumentUri::new(&host_uri(), "lua", region_id).to_uri_string()
    }

    fn params_with_edit(edit: serde_json::Value) -> ApplyWorkspaceEditParams {
        serde_json::from_value(json!({ "edit": edit })).unwrap()
    }

    fn text_edit(line: u32) -> serde_json::Value {
        json!({
            "range": {
                "start": { "line": line, "character": 0 },
                "end": { "line": line, "character": 5 }
            },
            "newText": "newName"
        })
    }

    #[tokio::test]
    async fn passes_through_edit_touching_only_real_files() {
        // Host-layer connections (and virt connections editing real files)
        // produce edits with real URIs throughout: forwarded verbatim.
        let original: ApplyWorkspaceEditParams = serde_json::from_value(json!({
            "label": "quickfix",
            "edit": { "changes": { "file:///project/main.rs": [text_edit(3)] } }
        }))
        .unwrap();
        let out = translator()
            .translate(original.clone())
            .await
            .expect("real-file edit must pass through");
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn rejects_edit_for_unknown_virtual_document() {
        // A well-formed virtual URI no open document maps to: forwarding it
        // would hand the editor a URI it can't apply — answer locally.
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let original = params_with_edit(json!({ "changes": { uri.clone(): [text_edit(0)] } }));
        let reason = translator()
            .translate(original)
            .await
            .expect_err("unknown virtual document must be rejected");
        assert!(
            reason.contains("unknown virtual document"),
            "reason should name the failure: {reason}"
        );
    }

    #[tokio::test]
    async fn rejects_edit_spanning_multiple_virtual_documents() {
        let uri_a = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let uri_b = virtual_uri("01BX5ZZKBKACTAV9WEVGEMMVRZ");
        let original = params_with_edit(json!({
            "changes": {
                uri_a: [text_edit(0)],
                uri_b: [text_edit(1)]
            }
        }));
        let reason = translator()
            .translate(original)
            .await
            .expect_err("multi-region edit must be rejected");
        assert!(
            reason.contains("multiple injected regions"),
            "reason should name the failure: {reason}"
        );
    }

    #[tokio::test]
    async fn same_virtual_uri_in_both_shapes_counts_as_one_region() {
        // The same virtual URI in `changes` AND `documentChanges` must dedup to
        // the single-region path (here it then fails resolution — empty bridge —
        // which proves it got past the multi-region reject).
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let original = params_with_edit(json!({
            "changes": { uri.clone(): [text_edit(0)] },
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": 2 },
                "edits": [text_edit(1)]
            }]
        }));
        let reason = translator().translate(original).await.unwrap_err();
        assert!(
            reason.contains("unknown virtual document"),
            "dedup must reach the single-region path, not the multi-region reject: {reason}"
        );
    }

    #[test]
    fn transform_params_to_host_translates_and_preserves_annotations() {
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let mut params: ApplyWorkspaceEditParams = serde_json::from_value(json!({
            "label": "extract function",
            "edit": {
                "changes": { uri.clone(): [text_edit(0)] },
                "changeAnnotations": {
                    "refactor": { "label": "Extract", "needsConfirmation": true }
                }
            }
        }))
        .unwrap();
        let host = host_uri();

        // Generous region end: this test exercises translation + annotation
        // passthrough, not the region-bounds guard (which has its own test).
        transform_params_to_host(
            &mut params,
            &uri,
            &host,
            &RegionOffset::new(10, 2),
            Position::new(u32::MAX, u32::MAX),
        )
        .expect("text-only edit must transform");

        assert_eq!(params.label.as_deref(), Some("extract function"));
        let changes = params.edit.changes.expect("changes survive");
        let edits = changes.get(&host).expect("re-keyed to the host URI");
        assert_eq!(
            edits[0].range.start,
            tower_lsp_server::ls_types::Position::new(10, 2),
            "line-0 edit shifts by the region's (line, column) offset"
        );
        let annotations = params
            .edit
            .change_annotations
            .expect("changeAnnotations pass through untouched");
        assert!(annotations.contains_key("refactor"));
    }

    #[test]
    fn transform_params_to_host_rejects_virtual_file_ops() {
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let mut params = params_with_edit(json!({
            "documentChanges": [
                { "kind": "rename", "oldUri": uri.clone(), "newUri": "file:///renamed.lua" }
            ]
        }));

        let reason = transform_params_to_host(
            &mut params,
            &uri,
            &host_uri(),
            &RegionOffset::new(10, 0),
            Position::default(),
        )
        .expect_err("virtual-URI file ops must reject the whole edit");
        assert!(
            reason.contains("file operations on a virtual document"),
            "reason should name the failure: {reason}"
        );
    }

    #[test]
    fn transform_params_to_host_rejects_edit_past_region_end() {
        // The region occupies a single host line (offset line 10). An edit whose
        // virtual range runs onto virtual line 3 translates to host line 13 —
        // past the region end — and would corrupt unrelated host text. It must
        // be rejected, not forwarded.
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let mut params = params_with_edit(json!({
            "changes": { uri.clone(): [{
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 3, "character": 4 }
                },
                "newText": "oops"
            }] }
        }));
        // Region end is host line 10 (a one-line region at the offset line).
        let region_end = Position::new(10, 11);
        let reason = transform_params_to_host(
            &mut params,
            &uri,
            &host_uri(),
            &RegionOffset::new(10, 0),
            region_end,
        )
        .expect_err("an edit past the region end must reject the whole edit");
        assert!(
            reason.contains("past the injected region"),
            "reason should name the failure: {reason}"
        );
    }

    #[test]
    fn collect_virtual_uris_covers_file_operation_uris() {
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let edit: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [
                { "kind": "rename", "oldUri": "file:///a.lua", "newUri": uri }
            ]
        }))
        .unwrap();
        assert_eq!(collect_virtual_uris(&edit), vec![uri]);
    }

    #[test]
    fn collect_virtual_uris_ignores_empty_text_edit_entries() {
        // An edit with real-file changes plus a STRAY EMPTY virtual entry must
        // not be routed down the virtual-translation path: the empty virtual
        // entry is a no-op, so it doesn't count as touching a virtual document.
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let changes_edit: WorkspaceEdit = serde_json::from_value(json!({
            "changes": {
                uri.clone(): [],
                "file:///real.lua": [text_edit(0)]
            }
        }))
        .unwrap();
        assert!(
            collect_virtual_uris(&changes_edit).is_empty(),
            "an empty virtual `changes` entry must not count as touched"
        );

        // Same for a `documentChanges` TextDocumentEdit with no edits.
        let doc_changes_edit: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [
                { "textDocument": { "uri": uri.clone(), "version": null }, "edits": [] }
            ]
        }))
        .unwrap();
        assert!(
            collect_virtual_uris(&doc_changes_edit).is_empty(),
            "an empty virtual `documentChanges` edit must not count as touched"
        );

        // A NON-empty virtual entry still counts.
        let non_empty: WorkspaceEdit = serde_json::from_value(json!({
            "changes": { uri.clone(): [text_edit(0)] }
        }))
        .unwrap();
        assert_eq!(collect_virtual_uris(&non_empty), vec![uri]);
    }
}
