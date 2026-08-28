//! `workspace/symbol` search and `workspaceSymbol/resolve` routing.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{WorkspaceSymbol, WorkspaceSymbolParams, WorkspaceSymbolResponse};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn workspace_symbol_impl(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let pool = self.bridge.pool_arc();
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            upstream_id.clone(),
        );
        let dispatch = pool.dispatch_workspace_symbol(params, &settings, upstream_id);
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                result = dispatch => Ok(result),
            },
            None => Ok(dispatch.await),
        }
    }

    pub(crate) async fn workspace_symbol_resolve_impl(
        &self,
        symbol: WorkspaceSymbol,
    ) -> Result<WorkspaceSymbol> {
        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let pool = self.bridge.pool_arc();
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            upstream_id.clone(),
        );
        let dispatch = pool.dispatch_workspace_symbol_resolve(symbol, &settings, upstream_id);
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
