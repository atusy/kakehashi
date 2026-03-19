use tower_lsp_server::ls_types::Uri;
use url::Url;

use crate::lsp::lsp_impl::Kakehashi;

pub(crate) struct DiagnosticScheduler<'a> {
    server: &'a Kakehashi,
}

impl<'a> DiagnosticScheduler<'a> {
    pub(crate) fn new(server: &'a Kakehashi) -> Self {
        Self { server }
    }

    /// Schedule a debounced diagnostic for a document (ADR-0020 Phase 3).
    ///
    /// This schedules a diagnostic collection to run after a debounce delay.
    /// If another change arrives before the delay expires, the previous timer
    /// is cancelled and a new one is started.
    ///
    /// The diagnostic snapshot is captured immediately (at schedule time) to
    /// ensure consistency with the document state that triggered the change.
    pub(crate) fn schedule_debounced_diagnostic(&self, uri: Url, lsp_uri: Uri) {
        let snapshot_data = self.server.prepare_diagnostic_snapshot(&uri);

        self.server.debounced_diagnostics.schedule(
            uri,
            lsp_uri,
            self.server.client.clone(),
            snapshot_data,
            self.server.bridge.pool_arc(),
            std::sync::Arc::clone(&self.server.synthetic_diagnostics),
        );
    }
}
