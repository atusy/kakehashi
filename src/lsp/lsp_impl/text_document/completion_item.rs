//! completionItem/resolve implementation for Kakehashi.
//!
//! Routes the resolve request to the single downstream server that produced
//! the completion item, identified by the Kakehashi envelope embedded in
//! `CompletionItem.data` during the original completion fan-out.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::CompletionItem;

use super::super::Kakehashi;
use super::super::bridge_context::current_upstream_id;

impl Kakehashi {
    /// Handle a `completionItem/resolve` request.
    ///
    /// Delegates to the pool's `dispatch_completion_resolve`, which strips the
    /// envelope, routes to the origin server, transforms coordinates, and
    /// re-envelopes the result. Falls back gracefully at every failure point.
    pub(crate) async fn completion_resolve_impl(
        &self,
        params: CompletionItem,
    ) -> Result<CompletionItem> {
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let item = pool
            .dispatch_completion_resolve(params, &settings, upstream_id)
            .await;
        Ok(item)
    }
}
