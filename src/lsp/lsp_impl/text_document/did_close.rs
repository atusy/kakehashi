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

        // Remove the document from the store when it's closed
        // This ensures that reopening the file will properly reinitialize everything
        self.documents.remove(&uri);

        // Clean up all caches for this document (semantic tokens, injections, requests)
        self.cache.remove_document(&uri);

        // Clean up region ID mappings for this document (ADR-0019)
        self.bridge.cleanup(&uri);

        // Abort any in-progress synthetic diagnostic task for this document (ADR-0020 Phase 2)
        self.synthetic_diagnostics.remove_document(&uri);

        // Cancel any pending debounced diagnostic for this document (ADR-0020 Phase 3)
        self.debounced_diagnostics.cancel(&uri);

        // Cancel any eager-open tasks for this document (prevents orphaned didOpen)
        self.bridge.cancel_eager_open(&uri);

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

        self.notifier().log_info("file closed!").await;
    }
}
