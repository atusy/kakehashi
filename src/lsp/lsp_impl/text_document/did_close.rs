//! didClose notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidCloseTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
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
        // and stop serializing with the in-flight one. The guard is dropped right
        // after the removal — the remaining cleanups touch other subsystems.
        {
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            // Drop stored captures lineage BEFORE the removal and INSIDE the
            // edit-lock section (captures-protocol): `store_lineage` inserts
            // under this same lock, so clearing here cannot wipe a lineage a
            // fast reopen's `full` stored after this close completed — that
            // insert can only start once this section is over, against the
            // reopened document's fresh open generation. (A didOpen landing
            // *inside* this section is a wider lifecycle race; for captures it
            // degrades to the self-healing "delta answers null → client calls
            // full again", never to stale data.)
            self.captures_cache.retain(|key, _| key.0 != uri);
            self.documents.remove(&uri);
        }

        // Clean up all caches for this document (semantic tokens, injections, requests)
        self.cache.remove_document(&uri);

        // Clean up region ID mappings for this document (lazy-node-identity-tracking)
        self.bridge.cleanup(&uri);

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
