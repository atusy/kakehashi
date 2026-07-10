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
//! with every `TextDocumentEdit.version` nulled — downstream versions are
//! bridge-local and would read as stale to the editor — and no-op (empty)
//! virtual entries removed (a raw virtual URI must never reach the editor,
//! see [`remove_empty_virtual_entries`]); it is otherwise unchanged. An edit
//! that touches exactly one virtual document is translated
//! against that region's live offset (rebuilt exactly as goto/showDocument do,
//! via [`resolve_region_offset`](super::region_offset::resolve_region_offset)).
//!
//! **Version validation** (before the nulling): a downstream that versions a
//! `TextDocumentEdit` for a VIRTUAL document pins the edit to the content
//! revision it computed against, in the bridge-local version space of the
//! connection it arrived on. The translation compares that version against the
//! version the bridge currently tracks for `(connection, virtual doc)`: a
//! mismatch means the bridge has since replaced the content (stale) — or the
//! downstream claims a revision the bridge never announced — and the edit's
//! coordinates cannot be trusted, so it is rejected with `applied: false`
//! instead of silently un-versioning it. Versions on REAL-file edits are
//! nulled without validation, as before: they are equally bridge-local (a
//! host-layer didOpen starts its own counter), but a real-file edit may
//! target a file the bridge never synced at all (a downstream can edit any
//! file it knows from disk), and the host-document sync state lives in a
//! separate tracker (`LanguageServerPool::host_documents`) not exposed to
//! this translator — nulling ("apply without a version check") is the
//! spec-sanctioned degradation there, and the editor still applies host
//! edits against its own live buffer. Validating host-document versions
//! through that tracker is follow-up hardening.
//!
//! The validation is a point-in-time read, not a lock: a content-changing
//! `didChange` can still race in between the check and the editor applying
//! the edit (exactly as it always could for the unversioned majority shape).
//! The check narrows the stale window to that race instead of accepting
//! arbitrarily old revisions; CLOSING it needs a bridge→editor version
//! mapping so the editor can do the atomic check itself — also follow-up
//! hardening (see `strip_bridge_local_versions`).
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
//! No-op (empty) virtual entries are removed before the forward —
//! [`remove_empty_virtual_entries`] on every path (a raw virtual URI must
//! never reach the editor), and the transform's foreign-virtual drop on the
//! translated path — which shifts the `documentChanges` indices the editor's
//! `failedChange` answer refers to. The forwarding loop compares the entry
//! count before/after translation (via [`document_change_count`]) and drops
//! `failedChange` from the relayed response when they differ, rather than
//! relaying a misaligned index. A real (non-empty) foreign entry rejects the
//! whole edit instead, so responses for edits that actually apply anything
//! keep their index.
//!
//! [`transform_workspace_edit_to_host`]: crate::lsp::bridge::transform_workspace_edit_to_host

use std::sync::Arc;

use tower_lsp_server::ls_types::{
    ApplyWorkspaceEditParams, DocumentChangeOperation, DocumentChanges, Position, ResourceOp,
    TextDocumentEdit, Uri, WorkspaceEdit,
};

use crate::document::DocumentStore;
use crate::language::LanguageCoordinator;
use crate::lsp::bridge::{
    BridgeCoordinator, ConnectionKey, RegionOffset, VirtualDocumentUri,
    strip_bridge_local_versions, transform_workspace_edit_to_host,
    workspace_edit_preserves_line_prefixes, workspace_edit_within_region,
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

    /// Translate a virtual-document applyEdit to host coordinates; when the
    /// edit touches no virtual URIs, forward `params` with the bridge-local
    /// `TextDocumentEdit.version`s nulled and no-op virtual entries removed
    /// (otherwise unchanged). `connection` is the
    /// `(server, root)` key of the downstream connection the request arrived
    /// on — versioned virtual-document edits are validated against that
    /// connection's tracked versions before the versions are nulled. `Err`
    /// carries the `failureReason` for a local `applied: false` answer — the
    /// edit must not reach the editor. See the module docs for the exact
    /// behavior.
    pub(super) async fn translate(
        &self,
        mut params: ApplyWorkspaceEditParams,
        connection: &ConnectionKey,
    ) -> Result<ApplyWorkspaceEditParams, String> {
        // A versioned virtual-document edit pins the content revision the
        // downstream computed against; validate it against the version this
        // connection currently tracks BEFORE the versions are erased below —
        // a stale edit's coordinates target content the bridge has since
        // replaced and must not be applied.
        self.validate_virtual_document_versions(&params.edit, connection)
            .await?;
        // Downstream document versions live in the bridge's version space,
        // never the editor's — null them on every forward (see the helper).
        strip_bridge_local_versions(&mut params.edit);
        // No-op virtual entries skip the translation path below (they don't
        // count as touching a virtual document), so they must be dropped here
        // or a real-file-only forward would carry raw virtual URIs to the
        // editor. Runs after validation: a versioned no-op's precondition has
        // already been honored before its carrier is removed.
        remove_empty_virtual_entries(&mut params.edit);
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
        let Some((offset, region_end, contiguous)) = resolve_region_offset(
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
        ensure_editable_region(contiguous)?;

        transform_params_to_host(&mut params, virtual_uri, &host_uri, &offset, region_end)?;
        Ok(params)
    }

    /// Validate every versioned `TextDocumentEdit` targeting a VIRTUAL
    /// document against the version the bridge tracks for that document on
    /// `connection` (didOpen = 1, each content-changing didChange bumps it).
    ///
    /// `Err` (→ `applied: false`) when the supplied version is STALE (older
    /// than tracked: the downstream computed the edit against content the
    /// bridge has since replaced — its coordinates may be misplaced), AHEAD
    /// (newer than tracked: a revision the bridge never announced on this
    /// connection — bookkeeping is broken, fail closed), or the document is
    /// not tracked on this connection at all (closed or purged since the
    /// downstream saw it — its content basis is gone). Only a version equal
    /// to the tracked one proceeds.
    ///
    /// Unversioned edits (`version: null`) and the `changes` map (which
    /// carries no versions) are not validated — the spec defines a missing
    /// version as "apply without a version check", and the region-freshness +
    /// region-bounds guards downstream of this still apply. A VERSIONED entry
    /// is validated even with an EMPTY edit vector: per spec the version is a
    /// per-document precondition on the whole apply, so a downstream may send
    /// `edits: []` purely to assert "only apply the rest if this document is
    /// still at version N" — stripping such an entry's version unvalidated
    /// would erase that precondition (only [`collect_virtual_uris`] ignores
    /// empty entries, since a no-op needs no coordinate translation).
    /// Real-file versions are out of scope here (see the module docs) and are
    /// nulled by `strip_bridge_local_versions`.
    ///
    /// Entries are grouped per URI BEFORE any tracker read: two entries for
    /// the same document claiming different versions are self-contradictory
    /// (at most one can match) and are rejected up front — checking each
    /// entry with its own awaited read would let a didChange bump the
    /// tracked version between reads so that versions N and N+1 both pass.
    /// One read per distinct URI then keeps the equal-version case coherent.
    async fn validate_virtual_document_versions(
        &self,
        edit: &WorkspaceEdit,
        connection: &ConnectionKey,
    ) -> Result<(), String> {
        let mut versions_by_uri: std::collections::HashMap<&str, i32> =
            std::collections::HashMap::new();
        for (uri, version) in versioned_virtual_text_document_edits(edit) {
            match versions_by_uri.entry(uri) {
                std::collections::hash_map::Entry::Occupied(seen) => {
                    let seen_version = *seen.get();
                    if seen_version != version {
                        return Err(format!(
                            "kakehashi: the edit claims two different versions ({seen_version} \
                             and {version}) for the same virtual document; at most one can be \
                             current, so the edit cannot be applied",
                        ));
                    }
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(version);
                }
            }
        }
        for (uri, version) in versions_by_uri {
            let Some(tracked) = self.bridge.virtual_document_version(uri, connection).await else {
                return Err(format!(
                    "kakehashi: the edit is versioned against virtual document {uri}, \
                     which is no longer open on this connection; the content it was \
                     computed against is gone"
                ));
            };
            if version != tracked {
                return Err(if version < tracked {
                    format!(
                        "kakehashi: the edit was computed against version {version} of \
                         the virtual document, but the bridge's tracked version is now \
                         {tracked}; applying it could misplace the edit"
                    )
                } else {
                    format!(
                        "kakehashi: the edit claims version {version} of the virtual \
                         document, but the bridge has only tracked up to version \
                         {tracked}; the edit cannot be validated"
                    )
                });
            }
        }
        Ok(())
    }
}

/// The number of `documentChanges` entries in an applyEdit's edit — the index
/// space of the editor's `failedChange` answer. The forwarding loop compares
/// this count before and after translation: translation can REMOVE entries
/// (no-op virtual entries via [`remove_empty_virtual_entries`], foreign
/// no-op entries via the transform), which shifts the indices of everything
/// after them, so a `failedChange` computed by the editor against the
/// forwarded array would misindex the downstream's original array. It never
/// reorders or inserts, so an unchanged count means the indices still align.
pub(super) fn document_change_count(params: &ApplyWorkspaceEditParams) -> usize {
    match &params.edit.document_changes {
        None => 0,
        Some(DocumentChanges::Edits(edits)) => edits.len(),
        Some(DocumentChanges::Operations(ops)) => ops.len(),
    }
}

/// Remove no-op (empty) virtual-document entries so no raw virtual URI ever
/// crosses the editor boundary. An empty entry skips the translation path
/// ([`collect_virtual_uris`] ignores it), so on a real-file-only forward it
/// would otherwise reach the editor verbatim — and LSP does not require
/// clients to skip URI resolution for an empty `TextDocumentEdit`, so a
/// `kakehashi://` URI could make the client fail the whole apply (or open a
/// phantom document). Call AFTER version validation: a versioned no-op's
/// precondition is honored before its carrier is dropped. Dropping a no-op
/// shifts the `documentChanges` indices the editor's `failedChange` refers
/// to; the forwarding loop detects the count change and drops `failedChange`
/// from the relayed response rather than relaying a misaligned index (see
/// [`document_change_count`] and the module docs).
fn remove_empty_virtual_entries(edit: &mut WorkspaceEdit) {
    if let Some(changes) = &mut edit.changes {
        changes.retain(|uri, edits| {
            !(edits.is_empty() && VirtualDocumentUri::is_virtual_uri(uri.as_str()))
        });
        // Drop the map entirely rather than forwarding `Some({})`: a client
        // without `documentChanges` support processes `changes` (LSP 3.13's
        // preference rule only orders the two when both are meaningful), and
        // an empty map there would read as "whole edit is a no-op", silently
        // losing the real edits carried in documentChanges.
        if changes.is_empty() {
            edit.changes = None;
        }
    }
    let keep = |e: &TextDocumentEdit| {
        !(e.edits.is_empty() && VirtualDocumentUri::is_virtual_uri(e.text_document.uri.as_str()))
    };
    match &mut edit.document_changes {
        None => {}
        Some(DocumentChanges::Edits(edits)) => edits.retain(keep),
        Some(DocumentChanges::Operations(ops)) => ops.retain(|op| match op {
            DocumentChangeOperation::Edit(e) => keep(e),
            DocumentChangeOperation::Op(_) => true,
        }),
    }
    // Mirror the `changes` handling above: clients that prefer
    // `documentChanges` when both fields are present would read a forwarded
    // `Some([])` as "whole edit is a no-op" and drop the real edits still
    // carried in `changes`.
    let emptied = match &edit.document_changes {
        None => false,
        Some(DocumentChanges::Edits(edits)) => edits.is_empty(),
        Some(DocumentChanges::Operations(ops)) => ops.is_empty(),
    };
    if emptied {
        edit.document_changes = None;
    }
}

/// The `(virtual URI, version)` of every VERSIONED `TextDocumentEdit` that
/// targets a virtual document — INCLUDING entries with an empty edit vector,
/// whose version is still a per-document precondition on the apply (see
/// [`ApplyEditTranslator::validate_virtual_document_versions`]). Only
/// `documentChanges` can carry versions; the `changes` map cannot.
fn versioned_virtual_text_document_edits(edit: &WorkspaceEdit) -> Vec<(&str, i32)> {
    let mut versioned = Vec::new();
    let doc_edits: Box<dyn Iterator<Item = &tower_lsp_server::ls_types::TextDocumentEdit> + '_> =
        match &edit.document_changes {
            None => Box::new(std::iter::empty()),
            Some(DocumentChanges::Edits(edits)) => Box::new(edits.iter()),
            Some(DocumentChanges::Operations(ops)) => {
                Box::new(ops.iter().filter_map(|op| match op {
                    DocumentChangeOperation::Edit(e) => Some(e),
                    DocumentChangeOperation::Op(_) => None,
                }))
            }
        };
    for e in doc_edits {
        let uri = e.text_document.uri.as_str();
        if !VirtualDocumentUri::is_virtual_uri(uri) {
            continue;
        }
        if let Some(version) = e.text_document.version {
            versioned.push((uri, version));
        }
    }
    versioned
}

fn ensure_editable_region(contiguous: bool) -> Result<(), String> {
    if contiguous {
        Ok(())
    } else {
        Err("kakehashi: the edit targets a non-contiguous combined injection; it cannot be applied without overwriting host text between injected regions".to_string())
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
    // Bound the edit to the region on both ends, per line, so a pass-through
    // host-URI edit can't reach host text above/below the region or into a
    // line's prefix (blockquote `> `).
    if !workspace_edit_within_region(&params.edit, host_uri, offset, region_end) {
        return Err(
            "kakehashi: the edit extends outside the injected region; it cannot be applied to the \
             host document without corrupting surrounding text"
                .to_string(),
        );
    }
    // The transform translates ranges but emits newText verbatim: an edit that
    // spans or inserts lines in a line-prefixed (e.g. blockquote) region would
    // strip the prefixes it overlaps and leave the inserted lines unprefixed;
    // an edit reaching a character-0 region end without a trailing newline
    // would merge content into the closing fence.
    if !workspace_edit_preserves_line_prefixes(&params.edit, host_uri, offset, region_end) {
        return Err(
            "kakehashi: the edit would break the host document's structure around the \
             injected region (its line prefixes, e.g. a blockquote's, or the closing \
             fence); it cannot be applied without corrupting the host document"
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
/// edit vector is a no-op and does NOT count as touching its URI: an edit
/// that is real-file-only but carries a stray empty virtual entry must still
/// forward (the caller removes the no-op carrier via
/// [`remove_empty_virtual_entries`] before this runs), not be routed down
/// the virtual-translation path (and then fail `applied: false`). File
/// operations always count — a create/rename/delete is a real change even
/// with no accompanying text edits.
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

    #[test]
    fn non_contiguous_combined_region_rejects_apply_edit() {
        let reason = ensure_editable_region(false).expect_err("masked gaps are not editable");
        assert!(reason.contains("non-contiguous combined injection"));
        assert!(ensure_editable_region(true).is_ok());
    }

    fn translator() -> ApplyEditTranslator {
        translator_with_bridge(Arc::new(BridgeCoordinator::new()))
    }

    fn translator_with_bridge(bridge: Arc<BridgeCoordinator>) -> ApplyEditTranslator {
        ApplyEditTranslator::new(
            Arc::new(DocumentStore::new()),
            Arc::new(LanguageCoordinator::new()),
            bridge,
        )
    }

    fn test_connection() -> ConnectionKey {
        ConnectionKey::for_server("lua_ls")
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
            .translate(original.clone(), &test_connection())
            .await
            .expect("real-file edit must pass through");
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn strips_bridge_local_versions_from_forwarded_real_file_edits() {
        // A host-layer downstream's document versions live in the BRIDGE's
        // version space (didOpen starts its own counter at 1), not the
        // editor's. Relaying a versioned documentChanges edit verbatim makes
        // version-checking editors (Neovim) skip it as stale — the version
        // must be nulled ("apply without check") on the editor-ward relay.
        let original: ApplyWorkspaceEditParams = serde_json::from_value(json!({
            "edit": { "documentChanges": [{
                "textDocument": { "uri": "file:///project/doc.md", "version": 2 },
                "edits": [text_edit(3)]
            }] }
        }))
        .unwrap();
        let out = translator()
            .translate(original, &test_connection())
            .await
            .expect("real-file edit must pass through");
        match out.edit.document_changes.as_ref().unwrap() {
            DocumentChanges::Edits(edits) => {
                assert_eq!(
                    edits[0].text_document.version, None,
                    "bridge-local version must not reach the editor"
                );
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[tokio::test]
    async fn rejects_edit_for_unknown_virtual_document() {
        // A well-formed virtual URI no open document maps to: forwarding it
        // would hand the editor a URI it can't apply — answer locally.
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let original = params_with_edit(json!({ "changes": { uri.clone(): [text_edit(0)] } }));
        let reason = translator()
            .translate(original, &test_connection())
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
            .translate(original, &test_connection())
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
                "textDocument": { "uri": uri, "version": null },
                "edits": [text_edit(1)]
            }]
        }));
        let reason = translator()
            .translate(original, &test_connection())
            .await
            .unwrap_err();
        assert!(
            reason.contains("unknown virtual document"),
            "dedup must reach the single-region path, not the multi-region reject: {reason}"
        );
    }

    /// Bridge with the test virtual document registered on `connection`
    /// (tracked version starts at 1, as after didOpen), returning the typed
    /// virtual URI for version bumps.
    async fn bridge_with_open_document(
        region_id: &str,
        connection: &ConnectionKey,
    ) -> (Arc<BridgeCoordinator>, VirtualDocumentUri) {
        let bridge = Arc::new(BridgeCoordinator::new());
        let host_url = url::Url::parse("file:///project/doc.md").unwrap();
        let typed_uri = VirtualDocumentUri::new(&host_uri(), "lua", region_id);
        bridge
            .register_opened_document_for_test(&host_url, &typed_uri, connection)
            .await;
        (bridge, typed_uri)
    }

    #[tokio::test]
    async fn rejects_stale_virtual_document_version() {
        // The downstream computed the edit against version 2, but the bridge's
        // tracked version advanced to 3 (a content-changing didChange): the
        // edit's coordinates target replaced content and applying it could
        // misplace the edit. Reject with applied:false, never strip-and-apply.
        let connection = test_connection();
        let (bridge, typed_uri) =
            bridge_with_open_document("01ARZ3NDEKTSV4RRFFQ69G5FAV", &connection).await;
        for expected in [2, 3] {
            assert_eq!(
                bridge
                    .increment_document_version_for_test(&typed_uri, &connection)
                    .await,
                Some(expected)
            );
        }
        let params = params_with_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": typed_uri.to_uri_string(), "version": 2 },
                "edits": [text_edit(0)]
            }]
        }));

        let reason = translator_with_bridge(bridge)
            .translate(params, &connection)
            .await
            .expect_err("a stale versioned edit must be rejected");
        assert!(
            reason.contains("version 2") && reason.contains("tracked version is now 3"),
            "reason should name both versions: {reason}"
        );
    }

    #[tokio::test]
    async fn rejects_virtual_document_version_ahead_of_tracked() {
        // A version the bridge never announced on this connection (tracked is
        // 1, the edit claims 5): bookkeeping is broken somewhere — fail closed
        // rather than un-version and apply.
        let connection = test_connection();
        let (bridge, typed_uri) =
            bridge_with_open_document("01ARZ3NDEKTSV4RRFFQ69G5FAV", &connection).await;
        let params = params_with_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": typed_uri.to_uri_string(), "version": 5 },
                "edits": [text_edit(0)]
            }]
        }));

        let reason = translator_with_bridge(bridge)
            .translate(params, &connection)
            .await
            .expect_err("a version ahead of the tracked one must be rejected");
        assert!(
            reason.contains("version 5") && reason.contains("tracked up to version 1"),
            "reason should name both versions: {reason}"
        );
    }

    #[tokio::test]
    async fn rejects_versioned_edit_for_document_not_open_on_this_connection() {
        // The document is open on ANOTHER connection only (this one was
        // purged/closed since the downstream saw the doc): the content basis
        // for the versioned edit is gone on the requesting connection — fail
        // closed. Version spaces are per connection, so the sibling's tracked
        // version must not vouch for this connection's edit.
        let other = ConnectionKey::for_server("emmylua");
        let (bridge, typed_uri) =
            bridge_with_open_document("01ARZ3NDEKTSV4RRFFQ69G5FAV", &other).await;
        let params = params_with_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": typed_uri.to_uri_string(), "version": 1 },
                "edits": [text_edit(0)]
            }]
        }));

        let reason = translator_with_bridge(bridge)
            .translate(params, &test_connection())
            .await
            .expect_err(
                "a versioned edit for a doc this connection no longer has must be rejected",
            );
        assert!(
            reason.contains("no longer open on this connection"),
            "reason should name the failure: {reason}"
        );
    }

    #[tokio::test]
    async fn current_version_passes_validation() {
        // A version matching the tracked one proceeds as before: validation
        // lets it through to the translation pipeline, whose next stage
        // (region-offset resolution against an empty document store) fails
        // with the REGION error — proof the version gate did not fire.
        let connection = test_connection();
        let (bridge, typed_uri) =
            bridge_with_open_document("01ARZ3NDEKTSV4RRFFQ69G5FAV", &connection).await;
        assert_eq!(
            bridge
                .increment_document_version_for_test(&typed_uri, &connection)
                .await,
            Some(2)
        );
        let params = params_with_edit(json!({
            "documentChanges": [{
                "textDocument": { "uri": typed_uri.to_uri_string(), "version": 2 },
                "edits": [text_edit(0)]
            }]
        }));

        let reason = translator_with_bridge(bridge)
            .translate(params, &connection)
            .await
            .expect_err("empty document store cannot resolve the region");
        assert!(
            reason.contains("the injected region changed"),
            "a matching version must pass the gate and fail only at region \
             resolution: {reason}"
        );
    }

    #[tokio::test]
    async fn versioned_empty_virtual_entry_is_still_a_validated_precondition() {
        // A versioned virtual TextDocumentEdit with an EMPTY edit vector is a
        // pure precondition per spec ("only apply the rest if this document is
        // still at version N"): its version must be validated even though it
        // carries no edits of its own — here the doc is untracked, so the
        // whole edit (including the real-file part) fails closed.
        let connection = test_connection();
        let params = params_with_edit(json!({
            "documentChanges": [
                {
                    "textDocument": {
                        "uri": virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
                        "version": 99
                    },
                    "edits": []
                },
                {
                    "textDocument": { "uri": "file:///project/main.rs", "version": null },
                    "edits": [text_edit(3)]
                }
            ]
        }));

        let reason = translator()
            .translate(params, &connection)
            .await
            .expect_err("a versioned no-op entry is a precondition and must be validated");
        assert!(
            reason.contains("no longer open on this connection"),
            "reason should name the failure: {reason}"
        );
    }

    #[tokio::test]
    async fn unversioned_empty_virtual_entry_does_not_trip_validation() {
        // An UNVERSIONED empty virtual entry asserts nothing (no version, no
        // edits): the real-file-only edit around it must still pass through,
        // and the empty entry must not route it down the virtual path either
        // (collect_virtual_uris ignores empty entries). The no-op carrier is
        // REMOVED on the forward — LSP doesn't require clients to skip URI
        // resolution for empty entries, so a raw kakehashi:// URI (in either
        // shape) could fail the whole apply editor-side.
        let vuri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let params = params_with_edit(json!({
            "changes": { vuri.clone(): [] },
            "documentChanges": [
                {
                    "textDocument": { "uri": vuri, "version": null },
                    "edits": []
                },
                {
                    "textDocument": { "uri": "file:///project/main.rs", "version": null },
                    "edits": [text_edit(3)]
                }
            ]
        }));

        let out = translator()
            .translate(params, &test_connection())
            .await
            .expect("an unversioned no-op virtual entry must not trip anything");
        assert_eq!(
            out.edit.changes, None,
            "the emptied `changes` map must be dropped, not forwarded as `Some({{}})` \
             (a documentChanges-unaware client would read that as a no-op edit)"
        );
        match out.edit.document_changes.as_ref().unwrap() {
            DocumentChanges::Edits(edits) => {
                assert_eq!(edits.len(), 1, "only the real-file edit survives");
                assert_eq!(
                    edits[0].text_document.uri.as_str(),
                    "file:///project/main.rs"
                );
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[tokio::test]
    async fn emptied_document_changes_is_dropped_not_forwarded_as_some_empty() {
        // When every documentChanges entry is a removed no-op virtual carrier
        // but `changes` still holds a real edit, forwarding
        // `documentChanges: []` would overshadow `changes` for clients that
        // prefer documentChanges when both are present — the real edit would
        // silently no-op. The emptied container must become None.
        let vuri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let params = params_with_edit(json!({
            "changes": { "file:///project/main.rs": [text_edit(3)] },
            "documentChanges": [
                {
                    "textDocument": { "uri": vuri, "version": null },
                    "edits": []
                }
            ]
        }));

        let out = translator()
            .translate(params, &test_connection())
            .await
            .expect("a removable no-op virtual entry must not fail the apply");
        assert_eq!(
            out.edit.document_changes, None,
            "an emptied documentChanges must be dropped, not forwarded as `Some([])`"
        );
        let changes = out.edit.changes.expect("the real edit must survive");
        assert!(
            changes.contains_key(&"file:///project/main.rs".parse().unwrap()),
            "the real-file edit in `changes` survives: {changes:?}"
        );
    }

    #[tokio::test]
    async fn rejects_conflicting_versions_for_the_same_virtual_document() {
        // Two entries for the same document claiming different versions are
        // self-contradictory (at most one can be current). Rejected BEFORE any
        // tracker read: per-entry reads could interleave with a didChange bump
        // so that versions 2 and 3 both matched, once each.
        let connection = test_connection();
        let (bridge, typed_uri) =
            bridge_with_open_document("01ARZ3NDEKTSV4RRFFQ69G5FAV", &connection).await;
        let params = params_with_edit(json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": typed_uri.to_uri_string(), "version": 1 },
                    "edits": [text_edit(0)]
                },
                {
                    "textDocument": { "uri": typed_uri.to_uri_string(), "version": 2 },
                    "edits": [text_edit(1)]
                }
            ]
        }));

        let reason = translator_with_bridge(bridge)
            .translate(params, &connection)
            .await
            .expect_err("conflicting versions for one document must be rejected");
        assert!(
            reason.contains("two different versions"),
            "reason should name the failure: {reason}"
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
            reason.contains("outside the injected region"),
            "reason should name the failure: {reason}"
        );
    }

    #[test]
    fn transform_params_to_host_rejects_edit_before_region_start() {
        // A pass-through host-URI edit above the region: keyed to the HOST doc
        // (not the virtual URI), so the transform leaves it untranslated. Its
        // range sits before the region start — must be rejected, not forwarded.
        let mut params = params_with_edit(json!({
            "changes": { "file:///project/doc.md": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 2, "character": 0 }
                },
                "newText": "sneaky"
            }] }
        }));
        // Region starts at host line 10; the host edit above it (lines 0-2) is
        // fully below the region start.
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let reason = transform_params_to_host(
            &mut params,
            &uri,
            &host_uri(),
            &RegionOffset::new(10, 0),
            Position::new(10, 11),
        )
        .expect_err("a host edit before the region start must reject the whole edit");
        assert!(
            reason.contains("outside the injected region"),
            "reason should name the failure: {reason}"
        );
    }

    #[test]
    fn transform_params_to_host_rejects_prefix_breaking_edit() {
        // A blockquote region (per-line `> ` prefixes, width 2): a downstream
        // multi-line replacement translates to a host range containing the
        // interior lines' prefixes, but its newText carries none — applying it
        // would strip the prefixes and break the blockquote. Reject with
        // applied:false, never corrupt.
        let uri = virtual_uri("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let mut params = params_with_edit(json!({
            "changes": { uri.clone(): [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 1, "character": 4 }
                },
                "newText": "import a\nimport b"
            }] }
        }));

        let reason = transform_params_to_host(
            &mut params,
            &uri,
            &host_uri(),
            &RegionOffset::with_per_line_offsets(10, vec![2, 2]),
            Position::new(11, 6),
        )
        .expect_err("a prefix-breaking edit must reject the whole edit");
        assert!(
            reason.contains("line prefixes"),
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
