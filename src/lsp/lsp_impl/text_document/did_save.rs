//! didSave notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidSaveTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    /// Handle textDocument/didSave notification.
    ///
    /// pull-first-diagnostic-forwarding Phase 2: Triggers synthetic diagnostic push.
    /// Collects diagnostics from downstream servers and publishes via publishDiagnostics.
    pub(crate) async fn did_save_impl(&self, params: DidSaveTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didSave: {}", lsp_uri.as_str());
            return;
        };

        log::debug!(
            target: "kakehashi::synthetic_diag",
            "didSave received for {}",
            uri
        );

        // Serialize the host save with document edits and snapshot the latest
        // text. Host didSave is textless, so every recipient must first receive
        // any pending full-text didChange on the same downstream queue.
        let edit_lock = self.documents.edit_lock(&uri);
        let edit_guard = edit_lock.lock().await;
        let host_text = self.documents.get(&uri).map(|document| document.text_arc());

        // Forward didSave to both bridge layers, in host-before-virt order.
        // Each path only touches an already-open document and excludes servers
        // that require save text, which kakehashi does not advertise upstream
        // (#357).
        let pool = self.bridge.pool_arc();
        if let Some(host_text) = host_text {
            pool.sync_and_notify_host_did_save(&uri, &host_text).await;
        }
        drop(edit_guard);
        pool.forward_did_save_to_virtual_docs(&uri).await;

        // Ensure a fresh tree before the synthetic task snapshots it: a save
        // batched right after an edit (autosave / format-on-save) races the
        // off-ingress reparse, and `prepare_diagnostic_snapshot` returns `None`
        // without a tree — making the synthetic diagnostic a no-op for the virt
        // layer.
        self.ensure_document_parsed(&uri).await;

        // Spawn background task for synthetic diagnostic collection
        self.diagnostic_scheduler()
            .spawn_synthetic_diagnostic_task(uri);

        self.notifier().log_info("file saved!").await;
    }
}
