//! Debounced diagnostic scheduling helpers.

use tower_lsp_server::ls_types::Uri;
use url::Url;

use super::Kakehashi;

impl Kakehashi {
    /// Schedule a debounced diagnostic for a document (ADR-0020 Phase 3).
    ///
    /// This schedules a diagnostic collection to run after a debounce delay.
    /// If another change arrives before the delay expires, the previous timer
    /// is cancelled and a new one is started.
    ///
    /// The diagnostic snapshot is captured immediately (at schedule time) to
    /// ensure consistency with the document state that triggered the change.
    pub(super) fn schedule_debounced_diagnostic(&self, uri: Url, lsp_uri: Uri) {
        // Capture snapshot data synchronously (same as spawn_synthetic_diagnostic_task)
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);

        // Schedule the debounced diagnostic
        self.debounced_diagnostics.schedule(
            uri,
            lsp_uri,
            self.client.clone(),
            snapshot_data,
            self.bridge.pool_arc(),
            std::sync::Arc::clone(&self.synthetic_diagnostics),
        );
    }
}
