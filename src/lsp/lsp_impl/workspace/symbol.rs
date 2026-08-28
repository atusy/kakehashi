//! `workspace/symbol` search and `workspaceSymbol/resolve` routing.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    SymbolTag, WorkspaceSymbol, WorkspaceSymbolParams, WorkspaceSymbolResponse,
};

use super::super::{Kakehashi, lock_settings_reload};

impl Kakehashi {
    pub(crate) async fn workspace_symbol_impl(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<WorkspaceSymbolResponse>> {
        let reload = lock_settings_reload().await;
        let settings_snapshot = self.settings_manager.load_settings_pair();
        let settings = std::sync::Arc::clone(&settings_snapshot.settings);
        let settings_generation = settings_snapshot.generation;
        let pool = self.bridge.pool_arc();
        let workspace_generation = pool.workspace_generation();
        drop(reload);
        let upstream_id = crate::lsp::current_upstream_id();
        let supports_tags = self
            .settings_manager
            .client_capabilities_lock()
            .get()
            .and_then(|capabilities| capabilities.workspace.as_ref())
            .and_then(|workspace| workspace.symbol.as_ref())
            .and_then(|symbol| symbol.tag_support.as_ref())
            .is_some_and(|support| support.value_set.contains(&SymbolTag::DEPRECATED));
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            upstream_id.clone(),
        );
        let admit = || self.settings_manager.settings_generation() == settings_generation;
        let dispatch = pool.dispatch_workspace_symbol_projected(
            params,
            &settings,
            upstream_id,
            supports_tags,
            &admit,
            workspace_generation,
            &self.documents,
            &self.language,
            &self.bridge,
        );
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
        let dispatch = pool.dispatch_workspace_symbol_resolve_projected(
            symbol,
            &settings,
            upstream_id,
            &self.documents,
            &self.language,
            &self.bridge,
        );
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
