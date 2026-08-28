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

    use tower_lsp_server::LspService;
    use tower_lsp_server::jsonrpc::{ErrorCode, Id};
    use tower_lsp_server::ls_types::Registration;

    use crate::config::WorkspaceSettings;
    use crate::lsp::Kakehashi;
    use crate::lsp::bridge::test_helpers::create_handle_with_key;
    use crate::lsp::bridge::{ConnectionKey, ConnectionState, LanguageServerPool, UpstreamId};
    use crate::lsp::request_id::{CURRENT_REQUEST_ID, CancelForwarder};

    #[tokio::test]
    async fn cancelled_workspace_diagnostic_handler_sweeps_every_provider_registration() {
        let pool = Arc::new(LanguageServerPool::new());
        let upstream_id = UpstreamId::Number(42);
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        handle.dynamic_capabilities().register(vec![
            Registration {
                id: "alpha".into(),
                method: "textDocument/diagnostic".into(),
                register_options: Some(serde_json::json!({
                    "identifier": "alpha",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "zeta".into(),
                method: "textDocument/diagnostic".into(),
                register_options: Some(serde_json::json!({
                    "identifier": "zeta",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
        ]);
        pool.insert_test_connection(key, Arc::clone(&handle)).await;

        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, Arc::clone(&pool), cancel_forwarder.clone())
        });
        let server = service.inner();
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-diagnostics".into()]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(settings);
        let params = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();

        let request = CURRENT_REQUEST_ID.scope(
            Some(Id::Number(42)),
            server.workspace_diagnostic_impl(params),
        );
        let cancel = async {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while pool.upstream_request_count(&upstream_id) != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("both provider requests must register");
            cancel_forwarder
                .forward_cancel(upstream_id.clone())
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(request, cancel);

        assert_eq!(result.unwrap_err().code, ErrorCode::RequestCancelled);
        assert_eq!(pool.upstream_request_count(&upstream_id), 0);
        assert_eq!(handle.router().pending_count(), 0);
        assert!(
            handle
                .router()
                .lookup_downstream_ids(&upstream_id)
                .is_empty()
        );
    }
}
