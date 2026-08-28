//! `workspace/diagnostic` routing and cancellation.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{WorkspaceDiagnosticParams, WorkspaceDiagnosticReportResult};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn workspace_diagnostic_impl(
        &self,
        params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        let settings_snapshot = self.settings_manager.load_settings_pair();
        let settings = std::sync::Arc::clone(&settings_snapshot.settings);
        let settings_generation = settings_snapshot.generation;
        let upstream_id = crate::lsp::current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let pool = self.bridge.pool_arc();
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            upstream_id.clone(),
        );
        let admit = || self.settings_manager.settings_generation() == settings_generation;
        let dispatch = pool.dispatch_workspace_diagnostic(params, &settings, upstream_id, &admit);
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                result = dispatch => Ok(result),
            },
            None => Ok(dispatch.await),
        }
    }
}
