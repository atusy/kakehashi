//! didChange notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidChangeTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};
use crate::language::node_tracker::EditInfo;
use crate::lsp::text_sync::apply_content_changes_with_edits;

impl Kakehashi {
    pub(crate) async fn did_change_impl(&self, params: DidChangeTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didChange: {}", lsp_uri.as_str());
            return;
        };

        // Serialize edits to this document, acquired as the FIRST `.await` of
        // the handler. `didChange` handlers are dispatched concurrently and the
        // read-of-old-text → reparse → persist cycle below is not atomic, so
        // without this a later edit can read the same stale base text as an
        // earlier one and apply its range to the wrong state (corrupting the
        // text, and — before clamping — panicking in `replace_range`). Taking
        // the lock before any other `.await` removes the known pre-lock yield,
        // so handlers acquire it in first-poll order. That is a strong practical
        // mitigation, not a hard guarantee of JSON-RPC wire order (tower-lsp
        // first-polls buffered futures); hard ingress-level ordering is tracked
        // in https://github.com/atusy/kakehashi/issues/342. Other documents are
        // unaffected.
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;

        self.notifier()
            .log_trace(format!("[DID_CHANGE] START uri={}", uri))
            .await;

        // Retrieve the stored document's current text (the base for the diff). The
        // language is re-detected by the off-ingress reparse from the stored
        // `language_id`, so it is not needed here.
        let old_text = {
            let doc = self.documents.get(&uri);
            match doc {
                // `text_arc()` is a refcount bump, not a full copy of the pre-edit
                // text — cheap on every keystroke (#498).
                Some(d) => d.text_arc(),
                None => {
                    self.notifier()
                        .log_warning("Document not found for change event")
                        .await;
                    // We created an edit-lock entry above for a document that
                    // doesn't exist (a stray/reordered notification). Drop it so
                    // the map can't grow unboundedly from such notifications.
                    self.documents.remove_edit_lock(&uri);
                    return;
                }
            }
        };

        // Apply content changes and build tree-sitter edits
        let (text, edits) = apply_content_changes_with_edits(&old_text, params.content_changes);

        // lazy-node-identity-tracking: Apply START-priority invalidation to node tracker.
        // Use InputEdits directly for precise invalidation when available,
        // fall back to diff-based approach for full document sync.
        //
        // This must be called AFTER content changes are applied (so we have new
        // text) but BEFORE the tree is cleared and the off-ingress reparse is
        // scheduled (so node-position sync happens against the pre-edit state).
        let invalidated_ulids = if edits.is_empty() {
            // Full document sync: no InputEdits available, reconstruct from diff
            self.bridge.apply_text_diff(&uri, &old_text, &text)
        } else {
            // Incremental sync: use InputEdits directly (precise, no over-invalidation)
            let edit_infos: Vec<EditInfo> = edits.iter().map(EditInfo::from).collect();
            self.bridge.apply_input_edits(&uri, &edit_infos)
        };

        // Invalidate injection caches for regions overlapping with edits. Must run
        // here (pre-edit invalidation) before the off-ingress reparse loop
        // repopulates the injection map from the freshly parsed tree.
        self.cache.invalidate_for_edits(&uri, &edits);

        // Apply the edit to the store and CLEAR the reader-visible tree
        // synchronously, here under the edit lock (per-document-parse-actor ADR).
        // Clearing the visible tree (rather than leaving the pre-edit one) is what
        // keeps readers safe once the parse is off-ingress: a virt/native reader now
        // sees *no* tree until the reparse lands (empty / on-demand fallback) instead
        // of a stale tree that predates this edit — turning the #342/#374 stale-tree
        // race into benign emptiness. The document exists (checked above) and the
        // edit lock serializes didClose, so this update is in-place, not a
        // resurrection.
        //
        // The pre-edit tree is NOT discarded: with `edits` applied it is stashed as
        // the off-ingress reparse's incremental seed (`pending_seed`, read only by
        // `reparse_latest`, never by readers), so a single edit reparses
        // incrementally rather than from scratch. A full-text sync (`edits` empty)
        // keeps no seed and parses from scratch (#348).
        let ticket = crate::lsp::current_writer_ticket();
        self.documents
            .apply_edit_clearing_tree(uri.clone(), text, &edits);

        // NOTE: We intentionally do NOT invalidate the semantic token cache here.
        // The cached tokens (with their result_id) are needed for delta calculations.
        // When semanticTokens/full/delta arrives with previousResultId, we look up
        // the cached tokens to compute the delta. If we invalidated here, the delta
        // request would always fall back to full tokenization.
        //
        // The cache is validated at lookup time via result_id matching, so stale
        // tokens won't be returned for mismatched result_ids.

        // lazy-node-identity-tracking: Close invalidated virtual documents.
        // Send didClose notifications to downstream LSs for orphaned docs. Stays in
        // the handler (text-derived). It closes the **invalidated** (old) region
        // ulids, whereas the scheduler loop's forward targets the **current** region
        // ulids — disjoint virtual documents — so even though the off-ingress loop
        // can forward a later edit's content before an earlier edit's close
        // completes, the two never act on the same downstream document.
        self.injection_coordinator()
            .close_invalidated_virtual_docs(&uri, &invalidated_ulids)
            .await;

        // Schedule the OFF-INGRESS reparse: this replaces the inline parse_document,
        // the post-parse process_injections (didChange forwarding + injected-language
        // processing + eager bridge spawn), the geometry re-merge republish, AND the
        // debounced diagnostic — all of which need the fresh tree and so run in the
        // spawned, coalescing parse loop instead of holding the writer ticket.
        //
        // The debounced diagnostic in particular MUST run post-parse, not here: this
        // handler just cleared the tree, and `prepare_diagnostic_snapshot` returns
        // `None` without one (`Document::snapshot()` requires a tree). A `None`
        // snapshot makes the debounce a no-op, skipping the on-edit host re-sync
        // (#431) that keeps a push-only `_self` host server's diagnostics following
        // edits. The handler returns without waiting on the parse.
        self.schedule_reparse(uri, ticket);

        // NOTE: We intentionally do NOT call semantic_tokens_refresh() here.
        // LSP clients already request new tokens after didChange (via semanticTokens/full/delta).
        // Calling refresh would be redundant and can cause deadlocks with synchronous clients
        // like vim-lsp on Vim, which cannot respond to server requests while processing.

        self.notifier().log_info("file changed!").await;
    }
}
