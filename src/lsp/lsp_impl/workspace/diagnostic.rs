//! `workspace/diagnostic` routing and cancellation.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{WorkspaceDiagnosticParams, WorkspaceDiagnosticReportResult};

use super::super::{Kakehashi, lock_settings_reload};

impl Kakehashi {
    pub(crate) async fn workspace_diagnostic_impl(
        &self,
        params: WorkspaceDiagnosticParams,
    ) -> Result<WorkspaceDiagnosticReportResult> {
        let reload = lock_settings_reload().await;
        let settings_snapshot = self.settings_manager.load_settings_pair();
        let settings = std::sync::Arc::clone(&settings_snapshot.settings);
        let settings_generation = settings_snapshot.generation;
        let pool = self.bridge.pool_arc();
        let workspace_generation = pool.workspace_generation();
        drop(reload);
        let upstream_id = crate::lsp::current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            upstream_id.clone(),
        );
        let admit = || self.settings_manager.settings_generation() == settings_generation;
        let language_for_uri = |uri: &str| {
            url::Url::parse(uri)
                .ok()
                .and_then(|uri| self.document_language(&uri))
        };
        let dispatch = pool.dispatch_workspace_diagnostic_cancellable(
            params,
            &settings,
            upstream_id,
            &admit,
            workspace_generation,
            self.bridge.cancel_forwarder(),
            &language_for_uri,
        );
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                result = dispatch => result.map_err(crate::error::map_workspace_diagnostic_error),
            },
            None => dispatch
                .await
                .map_err(crate::error::map_workspace_diagnostic_error),
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
        let request_generation = cancel_forwarder.register_request_for_test(upstream_id.clone());
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
        cancel_forwarder.unregister_request_for_test(&upstream_id, request_generation);
    }

    #[tokio::test]
    async fn incomplete_workspace_diagnostic_handler_returns_internal_error() {
        let pool = Arc::new(LanguageServerPool::new());
        let upstream_id = UpstreamId::Number(43);
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        handle.dynamic_capabilities().register(vec![Registration {
            id: "diagnostics".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        pool.insert_test_connection(key.clone(), Arc::clone(&handle))
            .await;

        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, Arc::clone(&pool), cancel_forwarder.clone())
        });
        let server = service.inner();
        let request_generation = cancel_forwarder.register_request_for_test(upstream_id.clone());
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
            Some(Id::Number(43)),
            server.workspace_diagnostic_impl(params),
        );
        let replace = async {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while pool.upstream_request_count(&upstream_id) != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("provider request must register");
            let replacement = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
            pool.insert_test_connection(key, replacement).await;
            let downstream_id = handle.router().lookup_downstream_ids(&upstream_id)[0];
            let _ = handle.router().route(serde_json::json!({
                "jsonrpc": "2.0",
                "id": downstream_id.as_i64(),
                "result": { "items": [] }
            }));
        };
        let (result, ()) = tokio::join!(request, replace);

        assert_eq!(result.unwrap_err().code, ErrorCode::InternalError);
        assert_eq!(pool.upstream_request_count(&upstream_id), 0);
        assert_eq!(handle.router().pending_count(), 0);
        cancel_forwarder.unregister_request_for_test(&upstream_id, request_generation);
    }
}
