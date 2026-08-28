//! Workspace-diagnostic fan-out with deterministic full-report aggregation.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use serde_json::Value;
use tower_lsp_server::ls_types::{
    DiagnosticRegistrationOptions, DiagnosticServerCapabilities, Registration,
    WorkspaceDiagnosticParams, WorkspaceDiagnosticReport, WorkspaceDiagnosticReportResult,
    WorkspaceDocumentDiagnosticReport, WorkspaceFullDocumentDiagnosticReport,
};

use crate::config::settings::WorkspaceSettings;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::pool::{
    ConnectionHandle, ConnectionState, INIT_TIMEOUT_SECS, LanguageServerPool, UpstreamId,
    VirtualUriObserver,
};
use crate::lsp::bridge::protocol::{JsonRpcRequest, response_has_jsonrpc_error};

const DIAGNOSTIC_METHOD: &str = "workspace/diagnostic";
const DIAGNOSTIC_REGISTRATION_METHOD: &str = "textDocument/diagnostic";

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiagnosticProvider {
    identifier: Option<String>,
    has_static_provider: bool,
    dynamic_registration_ids: Vec<String>,
}

struct CompletedDiagnosticProducer {
    handle: Arc<ConnectionHandle>,
    generation: u64,
    report: WorkspaceDiagnosticReport,
}

impl DiagnosticProvider {
    fn identifier(&self) -> Option<&str> {
        self.identifier.as_deref()
    }
}

fn static_workspace_diagnostic_identifier(handle: &ConnectionHandle) -> Option<Option<String>> {
    match handle
        .server_capabilities()
        .and_then(|capabilities| capabilities.diagnostic_provider.as_ref())
    {
        Some(DiagnosticServerCapabilities::Options(options)) if options.workspace_diagnostics => {
            Some(options.identifier.clone())
        }
        Some(DiagnosticServerCapabilities::RegistrationOptions(options)) => options
            .diagnostic_options
            .workspace_diagnostics
            .then(|| options.diagnostic_options.identifier.clone()),
        _ => None,
    }
}

fn dynamic_workspace_diagnostic_identifier(registration: &Registration) -> Option<Option<String>> {
    let options: DiagnosticRegistrationOptions =
        serde_json::from_value(registration.register_options.clone()?).ok()?;
    options
        .diagnostic_options
        .workspace_diagnostics
        .then_some(options.diagnostic_options.identifier)
}

fn diagnostic_providers(handle: &ConnectionHandle) -> Vec<DiagnosticProvider> {
    let mut providers = static_workspace_diagnostic_identifier(handle)
        .map(|identifier| DiagnosticProvider {
            identifier,
            has_static_provider: true,
            dynamic_registration_ids: Vec::new(),
        })
        .into_iter()
        .collect::<Vec<_>>();
    let mut registrations = handle
        .dynamic_capabilities()
        .registrations_for_method(DIAGNOSTIC_REGISTRATION_METHOD);
    registrations.sort_by(|left, right| left.id.cmp(&right.id));
    for registration in registrations {
        let Some(identifier) = dynamic_workspace_diagnostic_identifier(&registration) else {
            continue;
        };
        if let Some(provider) = providers
            .iter_mut()
            .find(|provider| provider.identifier == identifier)
        {
            provider.dynamic_registration_ids.push(registration.id);
        } else {
            providers.push(DiagnosticProvider {
                identifier,
                has_static_provider: false,
                dynamic_registration_ids: vec![registration.id],
            });
        }
    }
    providers
}

fn params_for_provider(mut params: Value, provider: &DiagnosticProvider) -> Value {
    if let (Some(params), Some(identifier)) = (params.as_object_mut(), provider.identifier()) {
        params.insert("identifier".into(), Value::String(identifier.into()));
    }
    params
}

fn sanitize_diagnostics(
    report: &mut WorkspaceFullDocumentDiagnosticReport,
    virtual_uris: &VirtualUriObserver,
) {
    for diagnostic in &mut report.full_document_diagnostic_report.items {
        if let Some(related) = &mut diagnostic.related_information {
            related.retain(|information| !virtual_uris.contains(information.location.uri.as_str()));
            if related.is_empty() {
                diagnostic.related_information = None;
            }
        }
    }
}

fn sanitize_report(
    mut report: WorkspaceDiagnosticReport,
    virtual_uris: &VirtualUriObserver,
) -> WorkspaceDiagnosticReport {
    report.items.retain_mut(|item| match item {
        WorkspaceDocumentDiagnosticReport::Full(report) => {
            if virtual_uris.contains(report.uri.as_str()) {
                return false;
            }
            sanitize_diagnostics(report, virtual_uris);
            true
        }
        WorkspaceDocumentDiagnosticReport::Unchanged(report) => {
            !virtual_uris.contains(report.uri.as_str())
        }
    });
    report
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
            incoming.full_document_diagnostic_report.result_id = None;
            let key = incoming.uri.as_str().to_owned();
            match by_uri.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    // A downstream version belongs to Kakehashi's synthetic
                    // synchronization stream, not the editor's document
                    // version namespace and cannot cross the bridge.
                    incoming.version = None;
                    entry.insert(incoming);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    current
                        .full_document_diagnostic_report
                        .items
                        .extend(incoming.full_document_diagnostic_report.items);
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

fn combine_producer_reports(
    reports: impl IntoIterator<Item = WorkspaceDiagnosticReport>,
) -> WorkspaceDiagnosticReport {
    let mut by_uri: BTreeMap<String, WorkspaceFullDocumentDiagnosticReport> = BTreeMap::new();
    for report in reports {
        for item in report.items {
            let WorkspaceDocumentDiagnosticReport::Full(mut incoming) = item else {
                continue;
            };
            incoming.full_document_diagnostic_report.result_id = None;
            incoming.version = None;
            let key = incoming.uri.as_str().to_owned();
            match by_uri.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(incoming);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    // Provider requests are not serialized with each URI's
                    // close/reopen lifecycle. Downstream versions reset on
                    // reopen, so even two concrete numbers are not enough to
                    // prove that one report supersedes another.
                    entry
                        .get_mut()
                        .full_document_diagnostic_report
                        .items
                        .extend(incoming.full_document_diagnostic_report.items);
                }
            }
        }
    }
    WorkspaceDiagnosticReport {
        items: by_uri
            .into_values()
            .map(WorkspaceDocumentDiagnosticReport::Full)
            .collect(),
    }
}

impl LanguageServerPool {
    async fn aggregate_admitted_workspace_diagnostic_reports<F>(
        &self,
        requests: impl IntoIterator<Item = F>,
        admit: &(dyn Fn() -> bool + Sync),
    ) -> WorkspaceDiagnosticReportResult
    where
        F: Future<Output = Option<CompletedDiagnosticProducer>>,
    {
        let reports: Vec<_> = join_all(requests).await.into_iter().flatten().collect();
        let connections = self.connections().await;
        if !admit() {
            return aggregate_reports(std::iter::empty());
        }
        let reports: Vec<_> = reports
            .into_iter()
            .filter_map(|completed| {
                let key = completed.handle.key();
                connections
                    .get(key)
                    .is_some_and(|live| {
                        Arc::ptr_eq(live, &completed.handle)
                            && live.state() == ConnectionState::Ready
                            && self.document_connection_generation(key) == completed.generation
                    })
                    .then_some(completed.report)
            })
            .collect();
        drop(connections);
        aggregate_reports(reports)
    }

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
                    .get_or_create_connection_wait_ready_admitted(
                        &name,
                        &config,
                        None,
                        Duration::from_secs(INIT_TIMEOUT_SECS),
                        admit,
                    )
                    .await
                    .ok()?;
                let generation = self.document_connection_generation(handle.key());
                let requests = diagnostic_providers(&handle).into_iter().map(|provider| {
                    let params = params_for_provider(params.clone(), &provider);
                    let upstream_id = upstream_id.clone();
                    let handle = Arc::clone(&handle);
                    async move {
                        self.send_workspace_diagnostic_request(
                            &handle,
                            generation,
                            params,
                            upstream_id,
                            provider,
                            Some(admit),
                        )
                        .await
                        .ok()
                        .flatten()
                    }
                });
                let report =
                    combine_producer_reports(join_all(requests).await.into_iter().flatten());
                if !self
                    .workspace_diagnostic_producer_is_live(&handle, generation)
                    .await
                    || !admit()
                {
                    return None;
                }
                Some(CompletedDiagnosticProducer {
                    handle,
                    generation,
                    report,
                })
            }
        });

        self.aggregate_admitted_workspace_diagnostic_reports(requests, admit)
            .await
    }

    async fn send_workspace_diagnostic_request(
        &self,
        handle: &Arc<ConnectionHandle>,
        expected_generation: u64,
        params: Value,
        upstream_id: Option<UpstreamId>,
        provider: DiagnosticProvider,
        admit: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> io::Result<Option<WorkspaceDiagnosticReport>> {
        let key = handle.key();
        let virtual_uris = self.observe_virtual_uris_for_connection(key, expected_generation);
        let (request_id, response_rx) =
            match self.register_request_for_handle_with_upstream(upstream_id.clone(), handle) {
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
            if admit.is_some_and(|admit| !admit()) {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "workspace diagnostic settings changed before request send",
                ));
            }
            let static_admitted = provider.has_static_provider
                && static_workspace_diagnostic_identifier(handle).as_ref()
                    == Some(&provider.identifier);
            let admitted = if static_admitted {
                Some(handle.send_request(request, request_id))
            } else {
                handle.dynamic_capabilities().with_registrations_by_id(
                    &provider.dynamic_registration_ids,
                    DIAGNOSTIC_REGISTRATION_METHOD,
                    |registrations| {
                        registrations
                            .iter()
                            .any(|registration| {
                                dynamic_workspace_diagnostic_identifier(registration).as_ref()
                                    == Some(&provider.identifier)
                            })
                            .then(|| handle.send_request(request, request_id))
                    },
                )
            };
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
        if admit.is_some_and(|admit| !admit()) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic settings changed before response acceptance",
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
            .map(|report| sanitize_report(report, &virtual_uris))
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tower_lsp_server::ls_types::{
        Diagnostic, DiagnosticRelatedInformation, FullDocumentDiagnosticReport, Location, Position,
        Range, UnchangedDocumentDiagnosticReport, Uri, WorkspaceUnchangedDocumentDiagnosticReport,
    };

    use super::*;
    use crate::lsp::bridge::ConnectionKey;
    use crate::lsp::bridge::pool::test_helpers::{
        create_handle_advertising_workspace_diagnostics,
        create_handle_advertising_workspace_diagnostics_with_state, create_handle_with_key,
        transition_handle_to_ready,
    };
    use crate::lsp::bridge::protocol::RequestId;
    use crate::lsp::bridge::protocol::VirtualDocumentUri;

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
    fn aggregation_merges_reports_without_exposing_downstream_versions() {
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
        assert_eq!(report.version, None);
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
    fn producer_combination_preserves_reports_across_version_resets() {
        let report = combine_producer_reports([
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(100), "old-incarnation")],
            },
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(1), "new-incarnation")],
            },
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", None, "closed")],
            },
        ]);
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.version, None);
        assert_eq!(report.full_document_diagnostic_report.result_id, None);
        assert_eq!(
            report
                .full_document_diagnostic_report
                .items
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["old-incarnation", "new-incarnation", "closed"]
        );
    }

    #[test]
    fn aggregation_preserves_reports_with_incomparable_versions() {
        let result = aggregate_reports([
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(3), "old")],
            },
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", Some(4), "new")],
            },
            WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/a.rs", None, "not-open")],
            },
        ]);
        let WorkspaceDiagnosticReportResult::Report(report) = result else {
            panic!("final report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.version, None);
        assert_eq!(
            report
                .full_document_diagnostic_report
                .items
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["old", "new", "not-open"]
        );
    }

    #[tokio::test]
    async fn final_aggregation_rejects_a_stale_settings_generation() {
        use std::pin::Pin;

        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        pool.connections().await.insert(key, Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let admitted = Arc::new(AtomicBool::new(true));
        let fast_completed = Arc::new(AtomicBool::new(false));
        let (release_slow, slow_released) = tokio::sync::oneshot::channel();
        let requests: Vec<
            Pin<Box<dyn Future<Output = Option<CompletedDiagnosticProducer>> + Send>>,
        > = vec![
            {
                let fast_completed = Arc::clone(&fast_completed);
                let handle = Arc::clone(&handle);
                Box::pin(async move {
                    fast_completed.store(true, Ordering::SeqCst);
                    Some(CompletedDiagnosticProducer {
                        handle,
                        generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full("file:///workspace/fast.rs", Some(1), "stale")],
                        },
                    })
                })
            },
            {
                let handle = Arc::clone(&handle);
                Box::pin(async move {
                    let _ = slow_released.await;
                    Some(CompletedDiagnosticProducer {
                        handle,
                        generation,
                        report: WorkspaceDiagnosticReport { items: Vec::new() },
                    })
                })
            },
        ];
        let pool_for_request = Arc::clone(&pool);
        let admitted_for_request = Arc::clone(&admitted);
        let request = tokio::spawn(async move {
            pool_for_request
                .aggregate_admitted_workspace_diagnostic_reports(requests, &|| {
                    admitted_for_request.load(Ordering::SeqCst)
                })
                .await
        });
        while !fast_completed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        admitted.store(false, Ordering::SeqCst);
        release_slow.send(()).unwrap();

        let WorkspaceDiagnosticReportResult::Report(report) = request.await.unwrap() else {
            panic!("full report")
        };
        assert!(report.items.is_empty());
    }

    #[tokio::test]
    async fn final_aggregation_discards_a_replaced_producer() {
        let pool = LanguageServerPool::new();
        let stale_key = ConnectionKey::for_server("alpha");
        let live_key = ConnectionKey::for_server("zeta");
        let stale = create_handle_with_key(ConnectionState::Ready, stale_key.clone()).await;
        let live = create_handle_with_key(ConnectionState::Ready, live_key.clone()).await;
        pool.connections().await.extend([
            (stale_key.clone(), Arc::clone(&stale)),
            (live_key, Arc::clone(&live)),
        ]);
        let stale_generation = pool.document_connection_generation(&stale_key);
        let live_generation = pool.document_connection_generation(live.key());
        let replacement = create_handle_with_key(ConnectionState::Ready, stale_key.clone()).await;
        pool.connections().await.insert(stale_key, replacement);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [
                    std::future::ready(Some(CompletedDiagnosticProducer {
                        handle: stale,
                        generation: stale_generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full("file:///workspace/stale.rs", Some(1), "stale")],
                        },
                    })),
                    std::future::ready(Some(CompletedDiagnosticProducer {
                        handle: live,
                        generation: live_generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full("file:///workspace/live.rs", Some(1), "live")],
                        },
                    })),
                ],
                &|| true,
            )
            .await;
        let WorkspaceDiagnosticReportResult::Report(report) = result else {
            panic!("full report")
        };
        assert_eq!(report.items.len(), 1);
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.uri.as_str(), "file:///workspace/live.rs");
    }

    #[tokio::test]
    async fn provider_plan_preserves_static_and_each_dynamic_identifier() {
        let handle = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::for_server("diagnostics"),
            Some("static"),
        )
        .await;
        handle.dynamic_capabilities().register(vec![
            Registration {
                id: "z-registration".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "zeta",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "a-registration".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "alpha",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "document-only".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "document",
                    "workspaceDiagnostics": false,
                    "interFileDependencies": false
                })),
            },
            Registration {
                id: "static-registration".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "static",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "no-identifier-a".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "no-identifier-z".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
        ]);

        let providers = diagnostic_providers(&handle);
        assert_eq!(
            providers,
            vec![
                DiagnosticProvider {
                    identifier: Some("static".into()),
                    has_static_provider: true,
                    dynamic_registration_ids: vec!["static-registration".into()],
                },
                DiagnosticProvider {
                    identifier: Some("alpha".into()),
                    has_static_provider: false,
                    dynamic_registration_ids: vec!["a-registration".into()],
                },
                DiagnosticProvider {
                    identifier: None,
                    has_static_provider: false,
                    dynamic_registration_ids: vec![
                        "no-identifier-a".into(),
                        "no-identifier-z".into()
                    ],
                },
                DiagnosticProvider {
                    identifier: Some("zeta".into()),
                    has_static_provider: false,
                    dynamic_registration_ids: vec!["z-registration".into()],
                },
            ]
        );
        let identifiers: Vec<_> = providers
            .iter()
            .map(|provider| {
                params_for_provider(serde_json::json!({ "previousResultIds": [] }), provider)
                    .get("identifier")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(
            identifiers,
            [
                Some("static".into()),
                Some("alpha".into()),
                None,
                Some("zeta".into())
            ]
        );
    }

    #[tokio::test]
    async fn dispatch_sends_one_request_for_each_dynamic_provider() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        handle.dynamic_capabilities().register(vec![
            Registration {
                id: "alpha".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "alpha",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "zeta".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "zeta",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
        ]);
        pool.connections().await.insert(key, Arc::clone(&handle));
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-diagnostics".into()]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();
        let upstream_id = UpstreamId::Number(42);
        let pool_for_request = Arc::clone(&pool);
        let upstream_for_request = upstream_id.clone();
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_diagnostic(
                    params,
                    &settings,
                    Some(upstream_for_request),
                    &|| true,
                )
                .await
        });

        let downstream_ids = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let ids = handle.router().lookup_downstream_ids(&upstream_id);
                if ids.len() == 2 && ids.iter().all(|id| handle.router().is_sent(*id)) {
                    break ids;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both provider requests reach Sent state");
        for (index, request_id) in downstream_ids.into_iter().enumerate() {
            let _ = handle.router().route(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request_id.as_i64(),
                "result": { "items": [{
                    "kind": "full",
                    "uri": "file:///workspace/shared.rs",
                    "version": 1,
                    "items": [{
                        "range": {
                            "start": { "line": 0, "character": index },
                            "end": { "line": 0, "character": index + 1 }
                        },
                        "message": format!("provider-{index}")
                    }]
                }] }
            }));
        }

        let WorkspaceDiagnosticReportResult::Report(report) = request.await.unwrap() else {
            panic!("full report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full document report")
        };
        assert_eq!(report.version, None);
        assert_eq!(report.full_document_diagnostic_report.items.len(), 2);
    }

    #[tokio::test]
    async fn dispatch_discards_completed_provider_after_producer_replacement() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        handle.dynamic_capabilities().register(vec![
            Registration {
                id: "alpha".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "alpha",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
            Registration {
                id: "zeta".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                register_options: Some(serde_json::json!({
                    "identifier": "zeta",
                    "workspaceDiagnostics": true,
                    "interFileDependencies": true
                })),
            },
        ]);
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&handle));
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-diagnostics".into()]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();
        let upstream_id = UpstreamId::Number(43);
        let pool_for_request = Arc::clone(&pool);
        let upstream_for_request = upstream_id.clone();
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_diagnostic(
                    params,
                    &settings,
                    Some(upstream_for_request),
                    &|| true,
                )
                .await
        });

        let downstream_ids = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let ids = handle.router().lookup_downstream_ids(&upstream_id);
                if ids.len() == 2 && ids.iter().all(|id| handle.router().is_sent(*id)) {
                    break ids;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both provider requests reach Sent state");
        let _ = handle.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": downstream_ids[0].as_i64(),
            "result": { "items": [{
                "kind": "full",
                "uri": "file:///workspace/shared.rs",
                "version": 1,
                "items": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "message": "completed-before-replacement"
                }]
            }] }
        }));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while pool.upstream_request_count(&upstream_id) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first provider response is accepted before replacement");

        let replacement = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        pool.connections().await.insert(key, replacement);
        let _ = handle.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": downstream_ids[1].as_i64(),
            "result": { "items": [] }
        }));

        let WorkspaceDiagnosticReportResult::Report(report) = request.await.unwrap() else {
            panic!("full report")
        };
        assert!(
            report.items.is_empty(),
            "a completed sibling report must not survive producer replacement"
        );
    }

    #[tokio::test]
    async fn sender_rejects_an_old_response_after_producer_replacement() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let producer = create_handle_advertising_workspace_diagnostics(key.clone(), None).await;
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&producer));
        let generation = pool.document_connection_generation(&key);
        let pool_for_request = Arc::clone(&pool);
        let producer_for_request = Arc::clone(&producer);
        let request = tokio::spawn(async move {
            pool_for_request
                .send_workspace_diagnostic_request(
                    &producer_for_request,
                    generation,
                    serde_json::json!({ "previousResultIds": [] }),
                    None,
                    DiagnosticProvider {
                        identifier: None,
                        has_static_provider: true,
                        dynamic_registration_ids: vec![],
                    },
                    None,
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace diagnostic reaches Sent state");

        let replacement = create_handle_advertising_workspace_diagnostics(key.clone(), None).await;
        pool.connections().await.insert(key, replacement);
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [] }
        }));

        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn sender_rejects_a_response_after_settings_change() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let producer = create_handle_advertising_workspace_diagnostics(key.clone(), None).await;
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&producer));
        let generation = pool.document_connection_generation(&key);
        let admitted = Arc::new(AtomicBool::new(true));
        let pool_for_request = Arc::clone(&pool);
        let producer_for_request = Arc::clone(&producer);
        let admitted_for_request = Arc::clone(&admitted);
        let request = tokio::spawn(async move {
            let admit = || admitted_for_request.load(Ordering::Acquire);
            pool_for_request
                .send_workspace_diagnostic_request(
                    &producer_for_request,
                    generation,
                    serde_json::json!({ "previousResultIds": [] }),
                    None,
                    DiagnosticProvider {
                        identifier: None,
                        has_static_provider: true,
                        dynamic_registration_ids: vec![],
                    },
                    Some(&admit),
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace diagnostic reaches Sent state");

        admitted.store(false, Ordering::Release);
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [] }
        }));

        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn dispatch_waits_for_an_existing_initializing_producer() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let producer = create_handle_advertising_workspace_diagnostics_with_state(
            ConnectionState::Initializing,
            key.clone(),
            None,
        )
        .await;
        pool.connections().await.insert(key, Arc::clone(&producer));
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-diagnostics".into()]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();
        let pool_for_request = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_diagnostic(params, &settings, None, &|| true)
                .await
        });

        tokio::task::yield_now().await;
        assert!(
            !request.is_finished(),
            "workspace pull must wait through initialization"
        );
        assert!(transition_handle_to_ready(&producer));
        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace diagnostic is sent after initialization");
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [] }
        }));

        assert_eq!(
            request.await.unwrap(),
            WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport::default())
        );
    }

    #[test]
    fn aggregation_ignores_unusable_unchanged_but_keeps_real_lookalike_uri() {
        let lookalike_uri = "file:///workspace/kakehashi-virtual-uri-region-0.lua";
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
                full(lookalike_uri, Some(1), "real lookalike"),
            ],
        }]);
        let WorkspaceDiagnosticReportResult::Report(report) = result else {
            panic!("final report")
        };
        assert_eq!(report.items.len(), 1);
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.uri.as_str(), lookalike_uri);
    }

    #[tokio::test]
    async fn sanitization_drops_only_uris_issued_to_the_exact_producer() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let generation = pool.document_connection_generation(&key);
        let host = url::Url::parse("file:///workspace/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &tower_lsp_server::ls_types::Uri::from_str(host.as_str()).unwrap(),
            "lua",
            "region-0",
        );
        pool.register_opened_document(&host, &virtual_uri, &key)
            .await;
        let observer = pool.observe_virtual_uris_for_connection(&key, generation);
        let real_uri = "file:///workspace/kakehashi-virtual-uri-real.lua";
        let mut real = full(real_uri, Some(1), "real");
        let WorkspaceDocumentDiagnosticReport::Full(real_report) = &mut real else {
            unreachable!()
        };
        real_report.full_document_diagnostic_report.items[0].related_information = Some(vec![
            DiagnosticRelatedInformation {
                location: Location::new(
                    Uri::from_str(&virtual_uri.to_uri_string()).unwrap(),
                    Range::default(),
                ),
                message: "internal".into(),
            },
            DiagnosticRelatedInformation {
                location: Location::new(Uri::from_str(real_uri).unwrap(), Range::default()),
                message: "real".into(),
            },
        ]);

        let sanitized = sanitize_report(
            WorkspaceDiagnosticReport {
                items: vec![
                    full(&virtual_uri.to_uri_string(), Some(1), "internal"),
                    real,
                ],
            },
            &observer,
        );

        assert_eq!(sanitized.items.len(), 1);
        let WorkspaceDocumentDiagnosticReport::Full(report) = &sanitized.items[0] else {
            panic!("full report")
        };
        assert_eq!(report.uri.as_str(), real_uri);
        assert_eq!(
            report.full_document_diagnostic_report.items[0]
                .related_information
                .as_ref()
                .unwrap()
                .iter()
                .map(|information| information.message.as_str())
                .collect::<Vec<_>>(),
            vec!["real"]
        );
    }

    #[tokio::test]
    async fn stale_settings_refuse_workspace_diagnostic_producer_admission() {
        let pool = LanguageServerPool::new();
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "stale-diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec![
                    std::env::current_exe()
                        .expect("current test executable")
                        .to_string_lossy()
                        .into_owned(),
                    "--help".into(),
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
        assert!(pool.connections().await.is_empty());
    }
}
