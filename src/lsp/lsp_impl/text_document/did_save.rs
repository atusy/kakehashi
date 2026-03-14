//! didSave notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidSaveTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    /// Handle textDocument/didSave notification.
    ///
    /// ADR-0020 Phase 2: Triggers synthetic diagnostic push.
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

        // Spawn background task for synthetic diagnostic collection
        self.spawn_synthetic_diagnostic_task(uri, lsp_uri);

        self.notifier().log_info("file saved!").await;
    }
}
