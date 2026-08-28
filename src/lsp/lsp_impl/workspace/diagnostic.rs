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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::lsp::bridge::{ConnectionKey, LanguageServerPool, UpstreamId};
    use crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard;

    #[tokio::test]
    async fn workspace_diagnostic_sweep_removes_every_provider_registration() {
        let pool = Arc::new(LanguageServerPool::new());
        let upstream_id = UpstreamId::Number(42);
        let provider = crate::lsp::bridge::test_helpers::create_handle_with_key(
            crate::lsp::bridge::ConnectionState::Ready,
            ConnectionKey::for_server("diagnostics"),
        )
        .await;
        pool.register_upstream_request_for_handle(upstream_id.clone(), &provider);
        pool.register_upstream_request_for_handle(upstream_id.clone(), &provider);
        assert_eq!(pool.upstream_request_count(&upstream_id), 2);

        let sweep = UpstreamRegistrySweepGuard::new(Arc::clone(&pool), Some(upstream_id.clone()));
        drop(sweep);

        assert_eq!(pool.upstream_request_count(&upstream_id), 0);
    }
}
