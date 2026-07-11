//! didClose notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidCloseTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(in crate::lsp::lsp_impl) async fn clear_document_state_on_close(&self, uri: &url::Url) {
        let edit_lock = self.documents.edit_lock(uri);
        let _edit_guard = edit_lock.lock().await;
        // Captures lineage and walk memos are installed under this same lock,
        // so a store that won first is cleared while one arriving later sees
        // the document gone and refuses to install obsolete state.
        self.captures_cache.retain(|key, _| key.0 != *uri);
        self.captures_walk_cache.retain(|key, _| key.0 != *uri);
        self.cancel_captures_walks_for_document(uri);
        self.documents.remove(uri);
        self.cache.remove_document(uri);
        // The match cache uses insert-then-verify. Clearing after document
        // removal ensures a straggler either inserted before this clear or
        // observes the closed document and self-removes.
        self.captures_match_cache.clear_document(uri);
    }

    pub(crate) async fn did_close_impl(&self, params: DidCloseTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didClose: {}", lsp_uri.as_str());
            return;
        };

        // Remove the document from the store when it's closed.
        // This ensures that reopening the file will properly reinitialize everything.
        //
        // Serialize the removal behind any in-flight edit by acquiring this
        // document's edit lock first (the same lock `did_change` holds across its
        // reparse). Without it, `remove` could drop the lock entry while an edit
        // still holds the old `Arc`, so a later edit would create a *fresh* lock
        // and stop serializing with the in-flight one. The guard is held
        // across the whole in-helper teardown (captures/walk cache clears,
        // walk cancellation, document + cache removal); only the cleanups
        // BELOW this call run outside the edit-lock section.
        self.clear_document_state_on_close(&uri).await;

        // Clean up region ID mappings for this document
        // (lazy-node-identity-tracking). This runs AFTER the removal above
        // and outside the edit-lock section, so a fast reopen's parse may
        // already have minted the NEW lifetime's ids — the probe (evaluated
        // under the tracker entry's lock) skips the removal in that case
        // instead of wiping a live index whose published ids would then
        // resolve null. See NodeTracker::cleanup.
        self.bridge
            .cleanup(&uri, || self.documents.latest_snapshot(&uri).is_some());

        // Abort any in-progress synthetic diagnostic task for this document (pull-first-diagnostic-forwarding Phase 2)
        self.synthetic_diagnostics.remove_document(&uri);

        // Cancel any pending debounced diagnostic for this document (pull-first-diagnostic-forwarding Phase 3)
        self.debounced_diagnostics.cancel(&uri);

        // Cancel any eager-open tasks for this document (prevents orphaned didOpen).
        // Host-layer eager-open is cancelled here too — before `close_host_bridge_document`
        // (the host-server didClose, further below) — so an in-flight host eager-open
        // still waiting for server readiness can't open a host doc whose didClose
        // already ran (#429).
        self.bridge.cancel_eager_open(&uri);
        self.bridge.cancel_host_eager_open(&uri);

        // Close all virtual documents associated with this host document
        // This sends didClose notifications to downstream language servers
        let closed_docs = self.bridge.close_host_document(&uri).await;
        if !closed_docs.is_empty() {
            log::debug!(
                target: "kakehashi::bridge",
                "Closed {} virtual documents for host {}",
                closed_docs.len(),
                uri
            );
        }

        // Drop the proactive diagnostic cache for this host and clear the editor's
        // diagnostics (push-propagation-diagnostic-forwarding). MUST run AFTER
        // close_host_document tears down host_to_virtual: a region push dequeued
        // before that teardown can still resolve its virtual URI and re-create the
        // cache entry, so clearing first would leave a resurrected, never-cleared
        // host. (Residual resolve→suspend→record micro-windows remain — both the
        // region path and the host path, whose `documents.get→record` can race the
        // `documents.remove()` above — closed only by the deferred tombstone/epoch
        // gate.)
        super::super::coordinator::DiagnosticPublisher::new(self)
            .clear_host(&uri)
            .await;

        // Close the host document itself on any servers it was opened on via
        // the host bridge (host-document-bridge).
        self.bridge
            .pool_arc()
            .close_host_bridge_document(&uri)
            .await;

        self.notifier().log_info("file closed!").await;
    }
}
