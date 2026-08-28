//! Workspace-diagnostic fan-out with deterministic full-report aggregation.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use futures::future::join_all;
use serde_json::Value;
use tower_lsp_server::ls_types::{
    DiagnosticServerCapabilities, WorkspaceDiagnosticParams, WorkspaceDiagnosticReport,
    WorkspaceDiagnosticReportResult, WorkspaceDocumentDiagnosticReport,
    WorkspaceFullDocumentDiagnosticReport,
};

use crate::config::settings::WorkspaceSettings;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::pool::{ConnectionHandle, ConnectionState, LanguageServerPool, UpstreamId};
use crate::lsp::bridge::protocol::{
    JsonRpcRequest, VirtualDocumentUri, response_has_jsonrpc_error,
};

const DIAGNOSTIC_METHOD: &str = "workspace/diagnostic";

fn has_static_workspace_diagnostics(handle: &ConnectionHandle) -> bool {
    match handle
        .server_capabilities()
        .and_then(|capabilities| capabilities.diagnostic_provider.as_ref())
    {
        Some(DiagnosticServerCapabilities::Options(options)) => options.workspace_diagnostics,
        Some(DiagnosticServerCapabilities::RegistrationOptions(options)) => {
            options.diagnostic_options.workspace_diagnostics
        }
        None => false,
    }
}

fn sanitize_diagnostics(report: &mut WorkspaceFullDocumentDiagnosticReport) {
    for diagnostic in &mut report.full_document_diagnostic_report.items {
        if let Some(related) = &mut diagnostic.related_information {
            related.retain(|information| {
                VirtualDocumentUri::region_id_of(information.location.uri.as_str()).is_none()
            });
            if related.is_empty() {
                diagnostic.related_information = None;
            }
        }
    }
}

fn aggregate_reports(
    reports: impl IntoIterator<Item = WorkspaceDiagnosticReport>,
) -> WorkspaceDiagnosticReportResult {
    let mut by_uri: BTreeMap<String, WorkspaceFullDocumentDiagnosticReport> = BTreeMap::new();
    for report in reports {
        for item in report.items {
            let WorkspaceDocumentDiagnosticReport::Full(mut incoming) = item else {
                // previousResultIds are never forwarded, so a compliant producer
                // cannot answer Unchanged. Without its private baseline there is
                // no full report the bridge can safely reconstruct.
                continue;
            };
            if VirtualDocumentUri::region_id_of(incoming.uri.as_str()).is_some() {
                // Internal virtual URIs are not editor documents. Open-document
                // diagnostics already flow through textDocument/diagnostic where
                // the current region offset is available for host translation.
                continue;
            }
            incoming.full_document_diagnostic_report.result_id = None;
            sanitize_diagnostics(&mut incoming);
            let key = incoming.uri.as_str().to_owned();
            match by_uri.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(incoming);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    match incoming.version.cmp(&current.version) {
                        std::cmp::Ordering::Greater => {
                            entry.insert(incoming);
                        }
                        std::cmp::Ordering::Equal => current
                            .full_document_diagnostic_report
                            .items
                            .extend(incoming.full_document_diagnostic_report.items),
                        std::cmp::Ordering::Less => {}
                    }
                }
            }
        }
    }
    WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport {
        items: by_uri
            .into_values()
            .map(WorkspaceDocumentDiagnosticReport::Full)
            .collect(),
    })
}

impl LanguageServerPool {
    async fn workspace_diagnostic_producer_is_live(
        &self,
        handle: &Arc<ConnectionHandle>,
        expected_generation: u64,
    ) -> bool {
        let key = handle.key();
        let connections = self.connections().await;
        connections
            .get(key)
            .is_some_and(|live| Arc::ptr_eq(live, handle) && live.state() == ConnectionState::Ready)
            && self.document_connection_generation(key) == expected_generation
    }

    pub(crate) async fn dispatch_workspace_diagnostic(
        &self,
        mut params: WorkspaceDiagnosticParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
        admit: &(dyn Fn() -> bool + Sync),
    ) -> WorkspaceDiagnosticReportResult {
        // Provider result IDs, identifiers, and progress tokens are scoped to
        // one server. The bridge aggregates several producers into one full
        // response, so none can be forwarded across that boundary.
        params.identifier = None;
        params.previous_result_ids.clear();
        params.partial_result_params.partial_result_token = None;
        params.work_done_progress_params.work_done_token = None;
        let Ok(mut params) = serde_json::to_value(params) else {
            return aggregate_reports(std::iter::empty());
        };
        if let Some(params) = params.as_object_mut() {
            // ls-types 0.0.6 serializes the optional identifier as JSON null,
            // but the LSP wire type is `identifier?: string`, not string|null.
            params.remove("identifier");
            params.remove("partialResultToken");
            params.remove("workDoneToken");
        }

        let mut servers: Vec<_> = settings
            .language_servers
            .keys()
            .filter(|name| name.as_str() != crate::config::WILDCARD_KEY)
            .filter(|name| crate::config::is_server_spawnable(&settings.language_servers, name))
            .filter_map(|name| {
                resolve_with_wildcard(
                    &settings.language_servers,
                    name,
                    merge_bridge_server_configs,
                )
                .map(|config| (name.clone(), config))
            })
            .collect();
        servers.sort_by(|left, right| left.0.cmp(&right.0));

        let requests = servers.into_iter().map(|(name, config)| {
            let params = params.clone();
            let upstream_id = upstream_id.clone();
            async move {
                let handle = self
                    .get_or_create_connection_admitted(&name, &config, None, admit)
                    .await
                    .ok()?;
                let generation = self.document_connection_generation(handle.key());
                self.send_workspace_diagnostic_request(&handle, generation, params, upstream_id)
                    .await
                    .ok()?
            }
        });

        aggregate_reports(join_all(requests).await.into_iter().flatten())
    }

    async fn send_workspace_diagnostic_request(
        &self,
        handle: &Arc<ConnectionHandle>,
        expected_generation: u64,
        params: Value,
        upstream_id: Option<UpstreamId>,
    ) -> io::Result<Option<WorkspaceDiagnosticReport>> {
        let key = handle.key();
        if let Some(id) = &upstream_id {
            self.register_upstream_request_for_handle(id.clone(), handle);
        }
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(request) => request,
                Err(error) => {
                    if let Some(id) = &upstream_id {
                        self.unregister_upstream_request(id, key);
                    }
                    return Err(error);
                }
            };
        let mut guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);
        let request = JsonRpcRequest::new(request_id.into(), DIAGNOSTIC_METHOD, params);
        {
            let connections = self.connections().await;
            if !connections.get(key).is_some_and(|live| {
                Arc::ptr_eq(live, handle) && live.state() == ConnectionState::Ready
            }) || self.document_connection_generation(key) != expected_generation
            {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "workspace diagnostic producer was replaced before admission",
                ));
            }
            let static_admitted = has_static_workspace_diagnostics(handle);
            let admitted = handle.dynamic_capabilities().with_registration_snapshot(
                "textDocument/diagnostic",
                "workspaceDiagnostics",
                |dynamic_diagnostics, dynamic_workspace| {
                    (static_admitted || (dynamic_diagnostics && dynamic_workspace))
                        .then(|| handle.send_request(request, request_id))
                },
            );
            let Some(send_result) = admitted else {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Ok(None);
            };
            if let Err(error) = send_result {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(error.into());
            }
        }
        let response = handle.wait_for_response(request_id, response_rx).await;
        guard.disarm();
        if let Some(id) = &upstream_id {
            self.unregister_upstream_request(id, key);
        }
        let response = response?;
        if !self
            .workspace_diagnostic_producer_is_live(handle, expected_generation)
            .await
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "workspace diagnostic producer was replaced before response acceptance",
            ));
        }
        if response_has_jsonrpc_error(&response, DIAGNOSTIC_METHOD) {
            return Ok(None);
        }
        let Some(result) = response.get("result") else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace diagnostic response omitted result",
            ));
        };
        if result == &Value::Null {
            return Ok(None);
        }
        serde_json::from_value(result.clone())
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tower_lsp_server::ls_types::{
        Diagnostic, FullDocumentDiagnosticReport, Position, Range,
        UnchangedDocumentDiagnosticReport, Uri, WorkspaceUnchangedDocumentDiagnosticReport,
    };

    use super::*;

    fn full(uri: &str, version: Option<i64>, message: &str) -> WorkspaceDocumentDiagnosticReport {
        WorkspaceDocumentDiagnosticReport::Full(WorkspaceFullDocumentDiagnosticReport {
            uri: Uri::from_str(uri).unwrap(),
            version,
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: Some(format!("private-{message}")),
                items: vec![Diagnostic::new_simple(
                    Range::new(Position::new(0, 0), Position::new(0, 1)),
                    message.into(),
                )],
            },
        })
    }

    #[test]
    fn aggregation_merges_equal_versions_and_drops_private_result_ids() {
        let result = aggregate_reports([
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(4), "alpha")],
            },
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(4), "zeta")],
            },
        ]);
        let WorkspaceDiagnosticReportResult::Report(report) = result else {
            panic!("final report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.full_document_diagnostic_report.result_id, None);
        assert_eq!(report.full_document_diagnostic_report.items.len(), 2);
        assert_eq!(
            report.full_document_diagnostic_report.items[0].message,
            "alpha"
        );
        assert_eq!(
            report.full_document_diagnostic_report.items[1].message,
            "zeta"
        );
    }

    #[test]
    fn aggregation_prefers_the_highest_document_version() {
        let result = aggregate_reports([
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(3), "old")],
            },
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(4), "new")],
            },
        ]);
        let WorkspaceDiagnosticReportResult::Report(report) = result else {
            panic!("final report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.version, Some(4));
        assert_eq!(
            report.full_document_diagnostic_report.items[0].message,
            "new"
        );
    }

    #[test]
    fn aggregation_ignores_unusable_unchanged_and_internal_virtual_reports() {
        let virtual_uri = "file:///workspace/kakehashi-virtual-uri-region-0.lua";
        let result = aggregate_reports([WorkspaceDiagnosticReport {
            items: vec![
                WorkspaceDocumentDiagnosticReport::Unchanged(
                    WorkspaceUnchangedDocumentDiagnosticReport {
                        uri: Uri::from_str("file:///workspace/a.rs").unwrap(),
                        version: None,
                        unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                            result_id: "private".into(),
                        },
                    },
                ),
                full(virtual_uri, Some(1), "virtual"),
            ],
        }]);
        let WorkspaceDiagnosticReportResult::Report(report) = result else {
            panic!("final report")
        };
        assert!(report.items.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_settings_refuse_workspace_diagnostic_producer_admission() {
        let pool = LanguageServerPool::new();
        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("stale-diagnostic-producer-started");
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "stale-diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "touch \"$1\"".into(),
                    "workspace-diagnostic-admission".into(),
                    sentinel.to_string_lossy().into_owned(),
                ]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();

        let response = pool
            .dispatch_workspace_diagnostic(params, &settings, None, &|| false)
            .await;

        assert_eq!(
            response,
            WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport::default())
        );
        assert!(
            !sentinel.exists(),
            "a superseded settings snapshot must not spawn its producer"
        );
    }
}
