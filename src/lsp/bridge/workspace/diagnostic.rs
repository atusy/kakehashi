//! Workspace-diagnostic fan-out with deterministic full-report aggregation.

use std::collections::BTreeMap;
#[cfg(test)]
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
use crate::lsp::bridge::protocol::{
    JsonRpcNotification, JsonRpcRequest, RequestId, response_has_jsonrpc_error,
};
use crate::lsp::request_id::CancelForwarder;

const DIAGNOSTIC_METHOD: &str = "workspace/diagnostic";
const DIAGNOSTIC_REGISTRATION_METHOD: &str = "textDocument/diagnostic";
const DYNAMIC_REGISTRATION_SETTLE: Duration = Duration::from_millis(100);

fn workspace_diagnostic_cancel(request_id: RequestId) -> JsonRpcNotification<Value> {
    JsonRpcNotification::new(
        "$/cancelRequest",
        serde_json::json!({ "id": request_id.as_i64() }),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiagnosticProvider {
    identifier: Option<String>,
    has_static_provider: bool,
    dynamic_registration_ids: Vec<String>,
}

struct CompletedDiagnosticProducer {
    pull_attempt: Option<WorkspaceDiagnosticPullAttempt>,
    provider_plan: Vec<DiagnosticProvider>,
    handle: Arc<ConnectionHandle>,
    generation: u64,
    report: WorkspaceDiagnosticReport,
    provider_reports: Option<Vec<ProviderDiagnosticReport>>,
    virtual_uris: Arc<VirtualUriObserver>,
}

struct WorkspaceDiagnosticPullAttempt {
    handle: Arc<ConnectionHandle>,
    upstream_tx: tokio::sync::mpsc::UnboundedSender<crate::lsp::bridge::UpstreamNotification>,
}

impl Drop for WorkspaceDiagnosticPullAttempt {
    fn drop(&mut self) {
        if self
            .handle
            .dynamic_capabilities()
            .mark_workspace_diagnostic_pull_completed()
        {
            let _ = self
                .upstream_tx
                .send(crate::lsp::bridge::UpstreamNotification::DiagnosticProviderChanged);
        }
    }
}

struct ProviderDiagnosticReport {
    identifier: Option<String>,
    report: WorkspaceDiagnosticReport,
}

struct RootedDiagnosticReport {
    server: String,
    spawn_root: Option<String>,
    provider_identifiers: Vec<Option<String>>,
    report: WorkspaceDiagnosticReport,
}

fn reconcile_overlapping_root_reports(
    reports: impl IntoIterator<Item = RootedDiagnosticReport>,
) -> Vec<WorkspaceDiagnosticReport> {
    struct RootedDiagnosticItem {
        server: String,
        spawn_root: Option<String>,
        provider_identifiers: Vec<Option<String>>,
        item: WorkspaceDocumentDiagnosticReport,
    }

    fn item_uri(item: &WorkspaceDocumentDiagnosticReport) -> &str {
        match item {
            WorkspaceDocumentDiagnosticReport::Full(report) => report.uri.as_str(),
            WorkspaceDocumentDiagnosticReport::Unchanged(report) => report.uri.as_str(),
        }
    }

    fn containing_root_depth(root: Option<&str>, document_uri: &str) -> Option<usize> {
        let root = url::Url::parse(root?).ok()?;
        let document = url::Url::parse(document_uri).ok()?;
        if root.scheme() != document.scheme()
            || root.username() != document.username()
            || root.password() != document.password()
            || root.host_str() != document.host_str()
            || root.port_or_known_default() != document.port_or_known_default()
        {
            return None;
        }
        let mut root_segments = root.path_segments()?.collect::<Vec<_>>();
        while root_segments.last() == Some(&"") {
            root_segments.pop();
        }
        let document_segments = document.path_segments()?.collect::<Vec<_>>();
        document_segments
            .starts_with(&root_segments)
            .then_some(root_segments.len())
    }

    let reports = reports.into_iter().collect::<Vec<_>>();
    let mut preferred_roots = BTreeMap::new();
    for report in &reports {
        for item in &report.report.items {
            let uri = item_uri(item);
            let key = (
                report.server.clone(),
                report.provider_identifiers.clone(),
                uri.to_owned(),
            );
            // Coverage belongs to every matching producer, including one that
            // returned no item for this URI. Otherwise an empty child-root
            // report cannot suppress a stale parent-root diagnostic.
            for producer in reports.iter().filter(|producer| {
                producer.server == report.server
                    && producer.provider_identifiers == report.provider_identifiers
            }) {
                let containing_depth = containing_root_depth(producer.spawn_root.as_deref(), uri);
                let producer_reported_uri = producer
                    .report
                    .items
                    .iter()
                    .any(|item| item_uri(item) == uri);
                if containing_depth.is_none() && !producer_reported_uri {
                    continue;
                }
                let candidate = (
                    containing_depth.is_some(),
                    containing_depth.unwrap_or_default(),
                    producer.spawn_root.clone().unwrap_or_default(),
                );
                preferred_roots
                    .entry(key.clone())
                    .and_modify(|current| {
                        if candidate > *current {
                            *current = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
        }
    }
    reports
        .into_iter()
        .flat_map(|report| {
            report
                .report
                .items
                .into_iter()
                .map(move |item| RootedDiagnosticItem {
                    server: report.server.clone(),
                    spawn_root: report.spawn_root.clone(),
                    provider_identifiers: report.provider_identifiers.clone(),
                    item,
                })
        })
        .filter(|item| {
            let uri = item_uri(&item.item);
            preferred_roots
                .get(&(
                    item.server.clone(),
                    item.provider_identifiers.clone(),
                    uri.to_owned(),
                ))
                .is_none_or(|(_, _, root)| item.spawn_root.as_deref().unwrap_or_default() == root)
        })
        .map(|item| WorkspaceDiagnosticReport {
            items: vec![item.item],
        })
        .collect()
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

fn diagnostic_providers_from_registrations<'a>(
    handle: &ConnectionHandle,
    registrations: impl Iterator<Item = &'a Registration>,
) -> Vec<DiagnosticProvider> {
    let mut providers = static_workspace_diagnostic_identifier(handle)
        .map(|identifier| DiagnosticProvider {
            identifier,
            has_static_provider: true,
            dynamic_registration_ids: Vec::new(),
        })
        .into_iter()
        .collect::<Vec<_>>();
    let mut registrations = registrations
        .filter(|registration| registration.method == DIAGNOSTIC_REGISTRATION_METHOD)
        .collect::<Vec<_>>();
    registrations.sort_by(|left, right| left.id.cmp(&right.id));
    for registration in registrations {
        let Some(identifier) = dynamic_workspace_diagnostic_identifier(registration) else {
            continue;
        };
        if let Some(provider) = providers
            .iter_mut()
            .find(|provider| provider.identifier == identifier)
        {
            provider
                .dynamic_registration_ids
                .push(registration.id.clone());
        } else {
            providers.push(DiagnosticProvider {
                identifier,
                has_static_provider: false,
                dynamic_registration_ids: vec![registration.id.clone()],
            });
        }
    }
    providers
}

fn diagnostic_providers(handle: &ConnectionHandle) -> Vec<DiagnosticProvider> {
    let registrations = handle
        .dynamic_capabilities()
        .registrations_for_method(DIAGNOSTIC_REGISTRATION_METHOD);
    diagnostic_providers_from_registrations(handle, registrations.iter())
}

async fn diagnostic_providers_after_registration_settle(
    handle: &ConnectionHandle,
    admit: &(dyn Fn() -> bool + Sync),
) -> Vec<DiagnosticProvider> {
    // Subscribe before the first snapshot so a registration committed between
    // the read and `changed()` cannot be missed. LSP has no "registration set
    // complete" signal, so every connection's first provider plan gets one
    // short settle window, including when a static or early dynamic provider
    // is already visible. A later registration also schedules a forced
    // upstream refresh from the reader path.
    let registry = handle.dynamic_capabilities();
    let _ = registry
        .diagnostic_registration_settle()
        .get_or_try_init(|| async {
            let mut changes = registry.subscribe_changes();
            let deadline = tokio::time::Instant::now() + DYNAMIC_REGISTRATION_SETTLE;
            loop {
                if !admit() {
                    return Err(());
                }
                if tokio::time::timeout_at(deadline, changes.changed())
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        })
        .await;
    diagnostic_providers(handle)
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

#[cfg(test)]
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

fn combine_complete_provider_reports(
    reports: impl IntoIterator<
        Item = (
            Option<String>,
            io::Result<Option<WorkspaceDiagnosticReport>>,
        ),
    >,
) -> io::Result<Option<Vec<ProviderDiagnosticReport>>> {
    let mut completed = Vec::new();
    let mut incomplete = false;
    let mut server_cancelled = None;
    for (identifier, report) in reports {
        let report = match report {
            Ok(Some(report)) => report,
            Ok(None) => {
                incomplete = true;
                continue;
            }
            Err(error)
                if error.get_ref().is_some_and(|error| {
                    error.is::<crate::error::WorkspaceDiagnosticServerCancelled>()
                }) =>
            {
                if server_cancelled.is_none() {
                    server_cancelled = Some(error);
                }
                continue;
            }
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        if !report
            .items
            .iter()
            .all(|item| matches!(item, WorkspaceDocumentDiagnosticReport::Full(_)))
        {
            incomplete = true;
            continue;
        }
        completed.push(ProviderDiagnosticReport { identifier, report });
    }
    if let Some(error) = server_cancelled {
        Err(error)
    } else if incomplete {
        Ok(None)
    } else {
        Ok(Some(completed))
    }
}

#[cfg(test)]
fn collect_complete_server_contributions<T>(
    contributions: impl IntoIterator<Item = io::Result<Vec<T>>>,
) -> io::Result<Vec<T>> {
    Ok(contributions
        .into_iter()
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect())
}

fn collect_complete_root_producers(
    producers: impl IntoIterator<Item = io::Result<Option<CompletedDiagnosticProducer>>>,
) -> io::Result<Vec<CompletedDiagnosticProducer>> {
    let mut completed = Vec::new();
    let mut incomplete = false;
    let mut first_error = None;
    let mut server_cancelled = None;
    for producer in producers {
        match producer {
            Ok(Some(producer)) => completed.push(producer),
            Ok(None) => incomplete = true,
            Err(error)
                if error.get_ref().is_some_and(|error| {
                    error.is::<crate::error::WorkspaceDiagnosticServerCancelled>()
                }) =>
            {
                if server_cancelled.is_none() {
                    server_cancelled = Some(error);
                }
            }
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    if let Some(error) = server_cancelled.or(first_error) {
        Err(error)
    } else if incomplete {
        Err(io::Error::other(
            "workspace diagnostic producer set was incomplete",
        ))
    } else {
        Ok(completed)
    }
}

impl LanguageServerPool {
    fn mark_workspace_diagnostic_pulls_completed<'a>(
        &self,
        handles: impl Iterator<Item = &'a Arc<ConnectionHandle>>,
    ) {
        let mut refresh = false;
        for handle in handles {
            refresh |= handle
                .dynamic_capabilities()
                .mark_workspace_diagnostic_pull_completed();
        }
        if refresh {
            let _ = self
                .upstream_tx()
                .send(crate::lsp::bridge::UpstreamNotification::DiagnosticProviderChanged);
        }
    }

    fn collect_completed_workspace_diagnostic_requests(
        &self,
        contributions: impl IntoIterator<Item = io::Result<Vec<CompletedDiagnosticProducer>>>,
    ) -> io::Result<Vec<CompletedDiagnosticProducer>> {
        let mut completed = Vec::new();
        let mut first_error = None;
        let mut server_cancelled = None;
        for contribution in contributions {
            match contribution {
                Ok(mut contribution) => completed.append(&mut contribution),
                Err(error)
                    if error.get_ref().is_some_and(|error| {
                        error.is::<crate::error::WorkspaceDiagnosticServerCancelled>()
                    }) =>
                {
                    if server_cancelled.is_none() {
                        server_cancelled = Some(error);
                    }
                }
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = server_cancelled.or(first_error) {
            self.mark_workspace_diagnostic_pulls_completed(
                completed.iter().map(|producer| &producer.handle),
            );
            return Err(error);
        }
        Ok(completed)
    }

    fn collect_completed_workspace_diagnostic_provider_reports(
        &self,
        handle: &Arc<ConnectionHandle>,
        reports: impl IntoIterator<
            Item = (
                Option<String>,
                io::Result<Option<WorkspaceDiagnosticReport>>,
            ),
        >,
    ) -> io::Result<Option<Vec<ProviderDiagnosticReport>>> {
        let completed = combine_complete_provider_reports(reports);
        if !matches!(completed, Ok(Some(_))) {
            self.mark_workspace_diagnostic_pulls_completed(std::iter::once(handle));
        }
        completed
    }

    fn release_changed_provider_refreshes<'a>(
        &self,
        providers: impl Iterator<Item = (&'a Arc<ConnectionHandle>, &'a Vec<DiagnosticProvider>)>,
    ) {
        let refresh = providers.fold(false, |refresh, (handle, expected)| {
            let changed = diagnostic_providers(handle) != *expected;
            let released = changed
                && handle
                    .dynamic_capabilities()
                    .take_workspace_diagnostic_registration_refresh();
            refresh || released
        });
        if refresh {
            let _ = self
                .upstream_tx()
                .send(crate::lsp::bridge::UpstreamNotification::DiagnosticProviderChanged);
        }
    }

    #[cfg(test)]
    async fn aggregate_admitted_workspace_diagnostic_reports<F>(
        &self,
        requests: impl IntoIterator<Item = F>,
        admit: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<WorkspaceDiagnosticReportResult>
    where
        F: Future<Output = Option<CompletedDiagnosticProducer>>,
    {
        let reports = join_all(requests)
            .await
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| io::Error::other("workspace diagnostic producer set was incomplete"))?;
        self.aggregate_admitted_completed_workspace_diagnostic_reports(reports, admit)
            .await
    }

    async fn aggregate_admitted_completed_workspace_diagnostic_reports(
        &self,
        reports: impl IntoIterator<Item = CompletedDiagnosticProducer>,
        admit: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<WorkspaceDiagnosticReportResult> {
        let connections = self.connections().await;
        if !admit() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic admission expired before final aggregation",
            ));
        }
        let mut admitted = reports
            .into_iter()
            .map(|completed| {
                let key = completed.handle.key();
                connections
                    .get(key)
                    .is_some_and(|live| {
                        Arc::ptr_eq(live, &completed.handle)
                            && live.state() == ConnectionState::Ready
                            && self.document_connection_generation(key) == completed.generation
                    })
                    .then_some(completed)
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "workspace diagnostic producer changed before final aggregation",
                )
            })?;
        drop(connections);
        if admitted
            .iter()
            .any(|completed| diagnostic_providers(&completed.handle) != completed.provider_plan)
        {
            self.release_changed_provider_refreshes(
                admitted
                    .iter()
                    .map(|completed| (&completed.handle, &completed.provider_plan)),
            );
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic provider plan changed before final aggregation",
            ));
        }
        if !admit() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic admission expired during final aggregation",
            ));
        }
        // Keep every attempt guard alive through all final fences. Any early
        // return or caller cancellation drops these guards and releases a cold
        // producer's deferred dynamic-registration refresh.
        let _pull_attempts = admitted
            .iter_mut()
            .filter_map(|completed| completed.pull_attempt.take())
            .collect::<Vec<_>>();
        let provenance_observers = admitted
            .iter()
            .map(|completed| Arc::clone(&completed.virtual_uris))
            .collect::<Vec<_>>();
        let mut provider_plans = admitted
            .iter()
            .map(|completed| {
                (
                    Arc::clone(&completed.handle),
                    completed.generation,
                    completed.provider_plan.clone(),
                )
            })
            .collect::<Vec<_>>();
        provider_plans.sort_by_key(|(handle, _, _)| handle.key().to_string());
        let _provenance_guards = provenance_observers
            .iter()
            .map(|observer| observer.provenance_read_guard())
            .collect::<Vec<_>>();
        let provenance_revisions = provenance_observers
            .iter()
            .map(|observer| observer.provenance_revision())
            .collect::<Vec<_>>();
        let reports = admitted.into_iter().flat_map(|completed| {
            let server = completed.handle.key().server().to_owned();
            let spawn_root = completed.handle.spawn_root().map(str::to_owned);
            match completed.provider_reports {
                Some(provider_reports) => provider_reports
                    .into_iter()
                    .map(|provider_report| RootedDiagnosticReport {
                        server: server.clone(),
                        spawn_root: spawn_root.clone(),
                        provider_identifiers: vec![provider_report.identifier],
                        report: sanitize_report(provider_report.report, &completed.virtual_uris),
                    })
                    .collect::<Vec<_>>(),
                None => {
                    let mut provider_identifiers = completed
                        .provider_plan
                        .iter()
                        .map(|provider| provider.identifier.clone())
                        .collect::<Vec<_>>();
                    provider_identifiers.sort();
                    provider_identifiers.dedup();
                    vec![RootedDiagnosticReport {
                        server,
                        spawn_root,
                        provider_identifiers,
                        report: sanitize_report(completed.report, &completed.virtual_uris),
                    }]
                }
            }
        });
        let reports = reconcile_overlapping_root_reports(reports);
        let result = aggregate_reports(reports);
        drop(_provenance_guards);
        if !admit() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic admission expired after final aggregation",
            ));
        }
        // Report processing above can be linear in the complete workspace
        // payload, so do not retain the pool-wide connection lock through it.
        // Reacquire both short final fences afterward: a provenance revision
        // change rejects the already-sanitized result instead of leaking a URI.
        let connections = self.connections().await;
        let _provenance_guards = provenance_observers
            .iter()
            .map(|observer| observer.provenance_read_guard())
            .collect::<Vec<_>>();
        let provider_guards = provider_plans
            .iter()
            .map(|(handle, _, _)| handle.dynamic_capabilities().registrations_read())
            .collect::<Vec<_>>();
        let provenance_stale = provenance_observers
            .iter()
            .zip(provenance_revisions)
            .any(|(observer, revision)| observer.provenance_revision() != revision);
        let producers_stale = provider_plans.iter().zip(&provider_guards).any(
            |((handle, generation, plan), registrations)| {
                connections.get(handle.key()).is_none_or(|live| {
                    !Arc::ptr_eq(live, handle)
                        || live.state() != ConnectionState::Ready
                        || live
                            .dynamic_capabilities()
                            .has_workspace_diagnostic_reader_exited()
                        || self.document_connection_generation(handle.key()) != *generation
                        || diagnostic_providers_from_registrations(handle, registrations.values())
                            != *plan
                })
            },
        );
        if provenance_stale || producers_stale || !admit() {
            drop(provider_guards);
            drop(connections);
            self.release_changed_provider_refreshes(
                provider_plans
                    .iter()
                    .map(|(handle, _, plan)| (handle, plan)),
            );
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic producer or admission expired after final aggregation",
            ));
        }
        let mut contribution_guards = provider_plans
            .iter()
            .filter(|(_, _, plan)| !plan.is_empty())
            .map(|(handle, _, _)| {
                handle
                    .dynamic_capabilities()
                    .workspace_diagnostic_lifecycle_lock()
            })
            .collect::<Vec<_>>();
        if contribution_guards
            .iter()
            .any(|guard| guard.reader_exited())
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic producer exited during final acceptance",
            ));
        }
        for guard in &mut contribution_guards {
            guard.mark_contributed();
        }
        drop(contribution_guards);
        for (handle, _, _) in &provider_plans {
            let refresh_deferred_registration = handle
                .dynamic_capabilities()
                .mark_workspace_diagnostic_pull_completed();
            if refresh_deferred_registration {
                let _ = self
                    .upstream_tx()
                    .send(crate::lsp::bridge::UpstreamNotification::DiagnosticProviderChanged);
            }
        }
        Ok(result)
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

    #[cfg(test)]
    pub(crate) async fn dispatch_workspace_diagnostic(
        &self,
        params: WorkspaceDiagnosticParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
        admit: &(dyn Fn() -> bool + Sync),
        request_workspace_generation: u64,
    ) -> io::Result<WorkspaceDiagnosticReportResult> {
        self.dispatch_workspace_diagnostic_inner(
            params,
            settings,
            upstream_id,
            admit,
            request_workspace_generation,
            None,
        )
        .await
    }

    pub(crate) async fn dispatch_workspace_diagnostic_cancellable(
        &self,
        params: WorkspaceDiagnosticParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
        admit: &(dyn Fn() -> bool + Sync),
        request_workspace_generation: u64,
        cancel_forwarder: &CancelForwarder,
    ) -> io::Result<WorkspaceDiagnosticReportResult> {
        self.dispatch_workspace_diagnostic_inner(
            params,
            settings,
            upstream_id,
            admit,
            request_workspace_generation,
            Some(cancel_forwarder),
        )
        .await
    }

    async fn dispatch_workspace_diagnostic_inner(
        &self,
        mut params: WorkspaceDiagnosticParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
        admit: &(dyn Fn() -> bool + Sync),
        request_workspace_generation: u64,
        cancel_forwarder: Option<&CancelForwarder>,
    ) -> io::Result<WorkspaceDiagnosticReportResult> {
        if request_workspace_generation & 1 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic snapshot is not stable",
            ));
        }
        // Provider result IDs, identifiers, and progress tokens are scoped to
        // one server. The bridge aggregates several producers into one full
        // response, so none can be forwarded across that boundary.
        params.identifier = None;
        params.previous_result_ids.clear();
        params.partial_result_params.partial_result_token = None;
        params.work_done_progress_params.work_done_token = None;
        let Ok(mut params) = serde_json::to_value(params) else {
            return Ok(aggregate_reports(std::iter::empty()));
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
                let pull_attempts = Arc::new(std::sync::Mutex::new(Vec::<
                    WorkspaceDiagnosticPullAttempt,
                >::new()));
                let observed_attempts = Arc::clone(&pull_attempts);
                let upstream_tx = self.upstream_tx();
                let on_acquired = move |handle: &Arc<ConnectionHandle>| {
                    let mut attempts = observed_attempts
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if attempts
                        .iter()
                        .any(|attempt| Arc::ptr_eq(&attempt.handle, handle))
                    {
                        return;
                    }
                    attempts.push(WorkspaceDiagnosticPullAttempt {
                        handle: Arc::clone(handle),
                        upstream_tx: upstream_tx.clone(),
                    });
                };
                let (handles, workspace_generation) = self
                    .get_or_create_workspace_connections_wait_ready_admitted_observed(
                        &name,
                        &config,
                        Duration::from_secs(INIT_TIMEOUT_SECS),
                        admit,
                        &on_acquired,
                    )
                    .await?;
                if workspace_generation != request_workspace_generation {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "workspace changed during diagnostic producer acquisition",
                    ));
                }
                let workspace_admit =
                    || admit() && self.workspace_generation() == workspace_generation;
                let producers = handles.into_iter().map(|handle| {
                    let params = params.clone();
                    let upstream_id = upstream_id.clone();
                    let pull_attempts = Arc::clone(&pull_attempts);
                    async move {
                        let pull_attempt = {
                            let mut attempts = pull_attempts
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let index = attempts
                                .iter()
                                .position(|attempt| Arc::ptr_eq(&attempt.handle, &handle))
                                .expect("every acquired diagnostic handle has an attempt guard");
                            attempts.swap_remove(index)
                        };
                        let generation = self.document_connection_generation(handle.key());
                        let virtual_uris = Arc::new(
                            self.observe_virtual_uris_for_connection(handle.key(), generation),
                        );
                        let providers = diagnostic_providers_after_registration_settle(
                            &handle,
                            &workspace_admit,
                        )
                        .await;
                        let requests = providers.iter().cloned().map(|provider| {
                            let params = params_for_provider(params.clone(), &provider);
                            let identifier = provider.identifier.clone();
                            let upstream_id = upstream_id.clone();
                            let handle = Arc::clone(&handle);
                            async move {
                                (
                                    identifier,
                                    self.send_workspace_diagnostic_request(
                                        &handle,
                                        generation,
                                        params,
                                        upstream_id,
                                        provider,
                                        Some(&workspace_admit),
                                        cancel_forwarder,
                                    )
                                    .await,
                                )
                            }
                        });
                        let Some(provider_reports) = self
                            .collect_completed_workspace_diagnostic_provider_reports(
                                &handle,
                                join_all(requests).await,
                            )?
                        else {
                            return Ok::<Option<CompletedDiagnosticProducer>, io::Error>(None);
                        };
                        if !self
                            .workspace_diagnostic_producer_is_live(&handle, generation)
                            .await
                            || !workspace_admit()
                        {
                            self.mark_workspace_diagnostic_pulls_completed(std::iter::once(
                                &handle,
                            ));
                            return Ok::<Option<CompletedDiagnosticProducer>, io::Error>(None);
                        }
                        Ok(Some(CompletedDiagnosticProducer {
                            pull_attempt: Some(pull_attempt),
                            provider_plan: providers,
                            handle,
                            generation,
                            report: WorkspaceDiagnosticReport::default(),
                            provider_reports: Some(provider_reports),
                            virtual_uris,
                        }))
                    }
                });
                collect_complete_root_producers(join_all(producers).await)
            }
        });

        let workspace_admit =
            || admit() && self.workspace_generation() == request_workspace_generation;
        let reports =
            self.collect_completed_workspace_diagnostic_requests(join_all(requests).await)?;
        self.aggregate_admitted_completed_workspace_diagnostic_reports(reports, &workspace_admit)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_workspace_diagnostic_request(
        &self,
        handle: &Arc<ConnectionHandle>,
        expected_generation: u64,
        params: Value,
        upstream_id: Option<UpstreamId>,
        provider: DiagnosticProvider,
        admit: Option<&(dyn Fn() -> bool + Sync)>,
        cancel_forwarder: Option<&CancelForwarder>,
    ) -> io::Result<Option<WorkspaceDiagnosticReport>> {
        let key = handle.key();
        let (request_id, response_rx) = match (upstream_id.clone(), cancel_forwarder) {
            (Some(upstream_id), Some(cancel_forwarder)) => {
                cancel_forwarder.register_downstream_request_if_current(upstream_id, handle)?
            }
            _ => self.register_request_for_handle_with_upstream(upstream_id.clone(), handle)?,
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
        if response
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
        {
            let _ = handle.send_notification(workspace_diagnostic_cancel(request_id));
        }
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
        if !diagnostic_providers(handle).contains(&provider) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace diagnostic provider changed before response acceptance",
            ));
        }
        if response_has_jsonrpc_error(&response, DIAGNOSTIC_METHOD) {
            if let Some(error) =
                crate::error::WorkspaceDiagnosticServerCancelled::from_response(&response)
            {
                return Err(io::Error::new(io::ErrorKind::Interrupted, error));
            }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tower_lsp_server::ls_types::{
        Diagnostic, DiagnosticRelatedInformation, FullDocumentDiagnosticReport, Location, Position,
        Range, Registration, UnchangedDocumentDiagnosticReport, Unregistration, Uri,
        WorkspaceFolder, WorkspaceUnchangedDocumentDiagnosticReport,
    };

    use super::*;
    use crate::lsp::bridge::ConnectionKey;
    use crate::lsp::bridge::pool::test_helpers::{
        create_handle_advertising_workspace_diagnostics,
        create_handle_advertising_workspace_diagnostics_with_state, create_handle_with_key,
        record_test_spawn_root, seed_test_client_root, transition_handle_to_ready,
    };
    use crate::lsp::bridge::protocol::VirtualDocumentUri;

    #[test]
    fn workspace_diagnostic_timeout_cancel_preserves_the_exact_request_id() {
        assert_eq!(
            serde_json::to_value(workspace_diagnostic_cancel(RequestId::new(37))).unwrap(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": { "id": 37 }
            })
        );
    }

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
    fn overlapping_roots_use_the_most_specific_producer_per_document() {
        let provider_identifiers = vec![Some("rust".to_owned())];
        let reports = reconcile_overlapping_root_reports([
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace".into()),
                provider_identifiers: provider_identifiers.clone(),
                report: WorkspaceDiagnosticReport {
                    items: vec![
                        full("file:///workspace/root.rs", None, "parent-root"),
                        full("file:///workspace/nested/doc.rs", None, "parent-nested"),
                        full("file:///generated/shared.rs", None, "parent-external"),
                    ],
                },
            },
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace/nested/".into()),
                provider_identifiers,
                report: WorkspaceDiagnosticReport {
                    items: vec![
                        full("file:///workspace/nested/doc.rs", None, "nested-root"),
                        full("file:///generated/shared.rs", None, "nested-external"),
                    ],
                },
            },
        ]);
        let WorkspaceDiagnosticReportResult::Report(report) = aggregate_reports(reports) else {
            panic!("final report")
        };
        let messages = report
            .items
            .iter()
            .map(|item| match item {
                WorkspaceDocumentDiagnosticReport::Full(report) => (
                    report.uri.as_str(),
                    report.full_document_diagnostic_report.items[0]
                        .message
                        .as_str(),
                ),
                WorkspaceDocumentDiagnosticReport::Unchanged(_) => panic!("full report"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            [
                ("file:///generated/shared.rs", "nested-external"),
                ("file:///workspace/nested/doc.rs", "nested-root"),
                ("file:///workspace/root.rs", "parent-root"),
            ]
        );
    }

    #[test]
    fn empty_nested_report_clears_the_parent_diagnostic_in_its_coverage() {
        let provider_identifiers = vec![Some("rust".to_owned())];
        let reports = reconcile_overlapping_root_reports([
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace".into()),
                provider_identifiers: provider_identifiers.clone(),
                report: WorkspaceDiagnosticReport {
                    items: vec![full(
                        "file:///workspace/nested/clean.rs",
                        None,
                        "stale-parent",
                    )],
                },
            },
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace/nested".into()),
                provider_identifiers,
                report: WorkspaceDiagnosticReport::default(),
            },
        ]);

        let WorkspaceDiagnosticReportResult::Report(report) = aggregate_reports(reports) else {
            panic!("final report")
        };
        assert!(
            report.items.is_empty(),
            "the empty nested producer owns and clears diagnostics below its root"
        );
    }

    #[test]
    fn empty_disjoint_root_does_not_suppress_an_external_uri_reported_elsewhere() {
        let provider_identifiers = vec![Some("rust".to_owned())];
        let reports = reconcile_overlapping_root_reports([
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace/a".into()),
                provider_identifiers: provider_identifiers.clone(),
                report: WorkspaceDiagnosticReport {
                    items: vec![full("file:///generated/shared.rs", None, "reported-by-a")],
                },
            },
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace/z".into()),
                provider_identifiers,
                report: WorkspaceDiagnosticReport::default(),
            },
        ]);

        let WorkspaceDiagnosticReportResult::Report(report) = aggregate_reports(reports) else {
            panic!("final report")
        };
        assert_eq!(report.items.len(), 1);
    }

    #[test]
    fn overlapping_root_reconciliation_preserves_independent_providers() {
        let uri = "file:///workspace/nested/doc.rs";
        let reports = reconcile_overlapping_root_reports([
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace".into()),
                provider_identifiers: vec![Some("compiler".into())],
                report: WorkspaceDiagnosticReport {
                    items: vec![full(uri, None, "parent-compiler")],
                },
            },
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace/nested".into()),
                provider_identifiers: vec![Some("compiler".into())],
                report: WorkspaceDiagnosticReport {
                    items: vec![full(uri, None, "nested-compiler")],
                },
            },
            RootedDiagnosticReport {
                server: "diagnostics".into(),
                spawn_root: Some("file:///workspace".into()),
                provider_identifiers: vec![Some("linter".into())],
                report: WorkspaceDiagnosticReport {
                    items: vec![full(uri, None, "parent-linter")],
                },
            },
            RootedDiagnosticReport {
                server: "other".into(),
                spawn_root: Some("file:///workspace".into()),
                provider_identifiers: vec![Some("compiler".into())],
                report: WorkspaceDiagnosticReport {
                    items: vec![full(uri, None, "other-server")],
                },
            },
        ]);
        let WorkspaceDiagnosticReportResult::Report(report) = aggregate_reports(reports) else {
            panic!("final report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full report")
        };
        assert_eq!(
            report
                .full_document_diagnostic_report
                .items
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["nested-compiler", "parent-linter", "other-server"]
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
    fn producer_combination_is_atomic_across_provider_failures() {
        let successful = || WorkspaceDiagnosticReport {
            items: vec![full("file:///workspace/a.rs", Some(1), "alpha")],
        };

        assert!(
            combine_complete_provider_reports([
                (Some("alpha".into()), Ok(Some(successful()))),
                (
                    Some("zeta".into()),
                    Err(io::Error::other("provider failed")),
                ),
            ])
            .unwrap()
            .is_none()
        );
        assert!(
            combine_complete_provider_reports([
                (Some("alpha".into()), Ok(Some(successful()))),
                (Some("zeta".into()), Ok(None)),
            ])
            .unwrap()
            .is_none()
        );
        let reports =
            combine_complete_provider_reports([(Some("alpha".into()), Ok(Some(successful())))])
                .unwrap()
                .expect("complete provider set");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].identifier.as_deref(), Some("alpha"));
        assert_eq!(reports[0].report.items.len(), 1);
    }

    #[test]
    fn server_combination_is_atomic_across_producer_failures() {
        let contributions = collect_complete_server_contributions([
            Ok(vec!["alpha"]),
            Err(io::Error::other("server failed")),
        ]);
        assert!(contributions.is_err());
        assert_eq!(
            collect_complete_server_contributions([Ok(vec!["alpha"]), Ok(vec!["beta"])]).unwrap(),
            ["alpha", "beta"]
        );
    }

    #[tokio::test]
    async fn failed_server_releases_a_successful_cold_producers_deferred_refresh() {
        let pool = LanguageServerPool::new();
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("successful"),
        )
        .await;
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        handle.dynamic_capabilities().register(vec![Registration {
            id: "late".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        assert!(
            !handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );
        let successful = CompletedDiagnosticProducer {
            pull_attempt: None,
            provider_plan: Vec::new(),
            handle: Arc::clone(&handle),
            generation,
            report: WorkspaceDiagnosticReport::default(),
            provider_reports: None,
            virtual_uris,
        };

        let result = pool.collect_completed_workspace_diagnostic_requests([
            Ok(vec![successful]),
            Err(io::Error::other("another server failed")),
        ]);

        assert!(result.is_err());
        assert!(
            !handle
                .dynamic_capabilities()
                .take_workspace_diagnostic_registration_refresh(),
            "the rejected aggregate must consume the deferred refresh"
        );
        assert!(
            handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh(),
            "the rejected cold pull must be marked complete"
        );
    }

    #[tokio::test]
    async fn failed_cold_producer_releases_its_own_deferred_refresh() {
        let pool = LanguageServerPool::new();
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("failed"))
                .await;
        handle.dynamic_capabilities().register(vec![Registration {
            id: "late".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        assert!(
            !handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );

        let reports = pool.collect_completed_workspace_diagnostic_provider_reports(
            &handle,
            [(None, Err(io::Error::other("provider failed")))],
        );

        assert!(reports.unwrap().is_none());
        assert!(
            !handle
                .dynamic_capabilities()
                .take_workspace_diagnostic_registration_refresh()
        );
        assert!(
            handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );
    }

    #[tokio::test]
    async fn cancelled_producer_attempt_releases_its_deferred_refresh() {
        let pool = Arc::new(LanguageServerPool::new());
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("cancelled"),
        )
        .await;
        assert!(
            !handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task_pool = Arc::clone(&pool);
        let task_handle = Arc::clone(&handle);
        let task = tokio::spawn(async move {
            let _attempt = WorkspaceDiagnosticPullAttempt {
                handle: task_handle,
                upstream_tx: task_pool.upstream_tx(),
            };
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;

        assert!(
            !handle
                .dynamic_capabilities()
                .take_workspace_diagnostic_registration_refresh()
        );
        assert!(
            handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );
    }

    #[tokio::test]
    async fn incomplete_same_server_sibling_drops_successful_attempt_guards() {
        let pool = LanguageServerPool::new();
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("siblings"),
        )
        .await;
        assert!(
            !handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );
        let generation = pool.document_connection_generation(handle.key());
        let completed = CompletedDiagnosticProducer {
            pull_attempt: Some(WorkspaceDiagnosticPullAttempt {
                handle: Arc::clone(&handle),
                upstream_tx: pool.upstream_tx(),
            }),
            provider_plan: Vec::new(),
            handle: Arc::clone(&handle),
            generation,
            report: WorkspaceDiagnosticReport::default(),
            provider_reports: None,
            virtual_uris: Arc::new(
                pool.observe_virtual_uris_for_connection(handle.key(), generation),
            ),
        };

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [
                    std::future::ready(Some(completed)),
                    std::future::ready(None),
                ],
                &|| true,
            )
            .await;

        assert!(result.is_err());
        assert!(
            !handle
                .dynamic_capabilities()
                .take_workspace_diagnostic_registration_refresh()
        );
    }

    #[tokio::test]
    async fn final_fence_rejection_drops_the_attempt_guard() {
        let pool = LanguageServerPool::new();
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("final-fence"),
        )
        .await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        assert!(
            !handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );
        let generation = pool.document_connection_generation(handle.key());
        let completed = CompletedDiagnosticProducer {
            pull_attempt: Some(WorkspaceDiagnosticPullAttempt {
                handle: Arc::clone(&handle),
                upstream_tx: pool.upstream_tx(),
            }),
            provider_plan: Vec::new(),
            handle: Arc::clone(&handle),
            generation,
            report: WorkspaceDiagnosticReport::default(),
            provider_reports: None,
            virtual_uris: Arc::new(
                pool.observe_virtual_uris_for_connection(handle.key(), generation),
            ),
        };

        let result = pool
            .aggregate_admitted_completed_workspace_diagnostic_reports([completed], &|| false)
            .await;

        assert!(result.is_err());
        assert!(
            !handle
                .dynamic_capabilities()
                .take_workspace_diagnostic_registration_refresh()
        );
    }

    #[test]
    fn server_cancelled_retrigger_is_propagated_without_refresh_support() {
        let response = |data: Option<Value>| {
            let mut error = serde_json::json!({
                "code": -32802,
                "message": "cancelled"
            });
            if let Some(data) = data {
                error["data"] = data;
            }
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "error": error })
        };

        for data in [None, Some(serde_json::json!({ "retriggerRequest": true }))] {
            let downstream = crate::error::WorkspaceDiagnosticServerCancelled::from_response(
                &response(data.clone()),
            )
            .expect("retriggering ServerCancelled");
            let combined = combine_complete_provider_reports([(
                None,
                Err(io::Error::new(io::ErrorKind::Interrupted, downstream)),
            )]);
            let error = match combined {
                Err(error) => error,
                Ok(_) => panic!("ServerCancelled must escape provider aggregation"),
            };
            let upstream = crate::error::map_workspace_diagnostic_error(error);
            assert_eq!(upstream.code.code(), -32802);
            assert_eq!(upstream.data, data);
        }
        let typed =
            crate::error::WorkspaceDiagnosticServerCancelled::from_response(&response(None))
                .unwrap();
        let provider_error = combine_complete_provider_reports([
            (None, Err(io::Error::other("ordinary failure"))),
            (None, Err(io::Error::new(io::ErrorKind::Interrupted, typed))),
        ]);
        let provider_error = match provider_error {
            Err(error) => error,
            Ok(_) => panic!("later ServerCancelled must win provider aggregation"),
        };
        assert_eq!(
            crate::error::map_workspace_diagnostic_error(provider_error)
                .code
                .code(),
            -32802
        );

        let typed =
            crate::error::WorkspaceDiagnosticServerCancelled::from_response(&response(None))
                .unwrap();
        let pool = LanguageServerPool::new();
        let server_error: io::Result<Vec<CompletedDiagnosticProducer>> = pool
            .collect_completed_workspace_diagnostic_requests([
                Err(io::Error::other("ordinary failure")),
                Err(io::Error::new(io::ErrorKind::Interrupted, typed)),
            ]);
        let server_error = match server_error {
            Err(error) => error,
            Ok(_) => panic!("later ServerCancelled must win server aggregation"),
        };
        assert_eq!(
            crate::error::map_workspace_diagnostic_error(server_error)
                .code
                .code(),
            -32802
        );

        let typed =
            crate::error::WorkspaceDiagnosticServerCancelled::from_response(&response(None))
                .unwrap();
        let root_error = collect_complete_root_producers([
            Err(io::Error::other("ordinary root failure")),
            Ok(None),
            Err(io::Error::new(io::ErrorKind::Interrupted, typed)),
        ]);
        let root_error = match root_error {
            Err(error) => error,
            Ok(_) => panic!("later ServerCancelled must win root aggregation"),
        };
        assert_eq!(
            crate::error::map_workspace_diagnostic_error(root_error)
                .code
                .code(),
            -32802
        );
        assert!(
            crate::error::WorkspaceDiagnosticServerCancelled::from_response(&response(Some(
                serde_json::json!({ "retriggerRequest": false })
            )))
            .is_none()
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
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let admitted = Arc::new(AtomicBool::new(true));
        let fast_completed = Arc::new(AtomicBool::new(false));
        let (release_slow, slow_released) = tokio::sync::oneshot::channel();
        let requests: Vec<
            Pin<Box<dyn Future<Output = Option<CompletedDiagnosticProducer>> + Send>>,
        > = vec![
            {
                let fast_completed = Arc::clone(&fast_completed);
                let handle = Arc::clone(&handle);
                let virtual_uris = Arc::clone(&virtual_uris);
                Box::pin(async move {
                    fast_completed.store(true, Ordering::SeqCst);
                    Some(CompletedDiagnosticProducer {
                        pull_attempt: None,
                        provider_plan: Vec::new(),
                        handle,
                        generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full("file:///workspace/fast.rs", Some(1), "stale")],
                        },
                        provider_reports: None,
                        virtual_uris,
                    })
                })
            },
            {
                let handle = Arc::clone(&handle);
                let virtual_uris = Arc::clone(&virtual_uris);
                Box::pin(async move {
                    let _ = slow_released.await;
                    Some(CompletedDiagnosticProducer {
                        pull_attempt: None,
                        provider_plan: Vec::new(),
                        handle,
                        generation,
                        report: WorkspaceDiagnosticReport { items: Vec::new() },
                        provider_reports: None,
                        virtual_uris,
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

        assert!(request.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn final_aggregation_resanitizes_virtual_uris_issued_after_a_fast_response() {
        use std::pin::Pin;

        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let leaked_uri = "kakehashi-virt:///late.lua";
        let fast_completed = Arc::new(AtomicBool::new(false));
        let (release_slow, slow_released) = tokio::sync::oneshot::channel();
        let requests: Vec<
            Pin<Box<dyn Future<Output = Option<CompletedDiagnosticProducer>> + Send>>,
        > = vec![
            {
                let handle = Arc::clone(&handle);
                let virtual_uris = Arc::clone(&virtual_uris);
                let fast_completed = Arc::clone(&fast_completed);
                Box::pin(async move {
                    fast_completed.store(true, Ordering::Release);
                    Some(CompletedDiagnosticProducer {
                        pull_attempt: None,
                        provider_plan: Vec::new(),
                        handle,
                        generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full(leaked_uri, None, "must not escape")],
                        },
                        provider_reports: None,
                        virtual_uris,
                    })
                })
            },
            {
                let handle = Arc::clone(&handle);
                let virtual_uris = Arc::clone(&virtual_uris);
                Box::pin(async move {
                    let _ = slow_released.await;
                    Some(CompletedDiagnosticProducer {
                        pull_attempt: None,
                        provider_plan: Vec::new(),
                        handle,
                        generation,
                        report: WorkspaceDiagnosticReport::default(),
                        provider_reports: None,
                        virtual_uris,
                    })
                })
            },
        ];
        let request_pool = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            request_pool
                .aggregate_admitted_workspace_diagnostic_reports(requests, &|| true)
                .await
        });
        while !fast_completed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        virtual_uris.insert(leaked_uri.into());
        release_slow.send(()).unwrap();

        let WorkspaceDiagnosticReportResult::Report(report) = request.await.unwrap().unwrap()
        else {
            panic!("full report")
        };
        assert!(
            report.items.is_empty(),
            "final provenance must include virtual URIs issued after an early response"
        );
    }

    #[tokio::test]
    async fn final_aggregation_rechecks_admission_after_building_result() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let checks = AtomicUsize::new(0);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan: Vec::new(),
                    handle,
                    generation,
                    report: WorkspaceDiagnosticReport {
                        items: vec![full("file:///workspace/live.rs", None, "live")],
                    },
                    provider_reports: None,
                    virtual_uris,
                }))],
                &|| checks.fetch_add(1, Ordering::SeqCst) < 2,
            )
            .await;

        assert!(
            result.is_err(),
            "the post-build admission fence must reject"
        );
        assert_eq!(checks.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn final_aggregation_rechecks_producer_liveness_after_building_result() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let checks = AtomicUsize::new(0);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan: Vec::new(),
                    handle: Arc::clone(&handle),
                    generation,
                    report: WorkspaceDiagnosticReport {
                        items: vec![full("file:///workspace/stale.rs", None, "stale")],
                    },
                    provider_reports: None,
                    virtual_uris,
                }))],
                &|| {
                    if checks.fetch_add(1, Ordering::SeqCst) == 2 {
                        handle.begin_shutdown();
                    }
                    true
                },
            )
            .await;

        assert!(
            result.is_err(),
            "a producer that exits during aggregation must be rejected at final acceptance"
        );
        assert_eq!(checks.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn empty_accepted_aggregation_does_not_mark_a_contributing_producer() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));

        pool.aggregate_admitted_workspace_diagnostic_reports(
            [std::future::ready(Some(CompletedDiagnosticProducer {
                pull_attempt: None,
                provider_plan: Vec::new(),
                handle: Arc::clone(&handle),
                generation,
                report: WorkspaceDiagnosticReport::default(),
                provider_reports: None,
                virtual_uris,
            }))],
            &|| true,
        )
        .await
        .expect("accepted diagnostic aggregate");

        assert!(
            !handle
                .dynamic_capabilities()
                .has_workspace_diagnostic_contributed(),
            "an empty provider plan must not arm the reader-exit refresh"
        );
    }

    #[tokio::test]
    async fn changed_provider_plan_releases_a_deferred_cold_refresh() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        handle.dynamic_capabilities().register(vec![Registration {
            id: "late".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        assert!(
            !handle
                .dynamic_capabilities()
                .request_or_defer_workspace_diagnostic_registration_refresh()
        );

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan: Vec::new(),
                    handle: Arc::clone(&handle),
                    generation,
                    report: WorkspaceDiagnosticReport::default(),
                    provider_reports: None,
                    virtual_uris,
                }))],
                &|| true,
            )
            .await;

        assert!(
            result.is_err(),
            "the changed provider plan rejects this pull"
        );
        assert!(
            !handle
                .dynamic_capabilities()
                .has_workspace_diagnostic_contributed()
        );
        assert!(
            !handle
                .dynamic_capabilities()
                .take_workspace_diagnostic_registration_refresh(),
            "the rejection must consume the deferred refresh for immediate delivery"
        );
    }

    #[tokio::test]
    async fn provenance_read_guard_blocks_new_virtual_uri_issuance() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let leaked_uri = "kakehashi-virt:///during-sanitize.lua";
        let guard = virtual_uris.provenance_read_guard();
        let observer = Arc::clone(&virtual_uris);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            observer.insert_provenance_for_test(leaked_uri.into());
            finished_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            finished_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "URI issuance must wait while sanitization owns the provenance read fence"
        );
        drop(guard);
        finished_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("URI issuance should resume after response acceptance");
        writer.join().unwrap();
        assert!(virtual_uris.contains(leaked_uri));
    }

    #[tokio::test]
    async fn final_acceptance_rejects_provenance_changed_after_sanitizing() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key).await;
        pool.connections()
            .await
            .insert(handle.key().clone(), Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let checks = AtomicUsize::new(0);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan: Vec::new(),
                    handle,
                    generation,
                    report: WorkspaceDiagnosticReport::default(),
                    provider_reports: None,
                    virtual_uris: Arc::clone(&virtual_uris),
                }))],
                &|| {
                    if checks.fetch_add(1, Ordering::SeqCst) == 2 {
                        virtual_uris.insert_provenance_for_test(
                            "kakehashi-virt:///after-sanitize.lua".into(),
                        );
                    }
                    true
                },
            )
            .await;

        assert!(
            result.is_err(),
            "a provenance change after sanitization must reject the response"
        );
    }

    #[tokio::test]
    async fn final_aggregation_rejects_a_replaced_producer() {
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
        let stale_virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(&stale_key, stale_generation));
        let live_virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(live.key(), live_generation));
        let replacement = create_handle_with_key(ConnectionState::Ready, stale_key.clone()).await;
        pool.connections().await.insert(stale_key, replacement);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [
                    std::future::ready(Some(CompletedDiagnosticProducer {
                        pull_attempt: None,
                        provider_plan: Vec::new(),
                        handle: stale,
                        generation: stale_generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full("file:///workspace/stale.rs", Some(1), "stale")],
                        },
                        provider_reports: None,
                        virtual_uris: stale_virtual_uris,
                    })),
                    std::future::ready(Some(CompletedDiagnosticProducer {
                        pull_attempt: None,
                        provider_plan: Vec::new(),
                        handle: live,
                        generation: live_generation,
                        report: WorkspaceDiagnosticReport {
                            items: vec![full("file:///workspace/live.rs", Some(1), "live")],
                        },
                        provider_reports: None,
                        virtual_uris: live_virtual_uris,
                    })),
                ],
                &|| true,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn final_aggregation_rejects_a_changed_provider_plan() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        handle.dynamic_capabilities().register(vec![Registration {
            id: "alpha-registration".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "alpha",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        let provider_plan = diagnostic_providers(&handle);
        pool.connections().await.insert(key, Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        handle
            .dynamic_capabilities()
            .unregister(vec![Unregistration {
                id: "alpha-registration".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            }]);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan,
                    handle,
                    generation,
                    report: WorkspaceDiagnosticReport::default(),
                    provider_reports: None,
                    virtual_uris,
                }))],
                &|| true,
            )
            .await;

        let error = result.expect_err("a completed stale plan must not be aggregated");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn final_aggregation_rejects_a_reader_that_exited_after_responding() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_advertising_workspace_diagnostics(key.clone(), None).await;
        let provider_plan = diagnostic_providers(&handle);
        pool.connections().await.insert(key, Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        handle
            .dynamic_capabilities()
            .mark_workspace_diagnostic_reader_exited();

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan,
                    handle,
                    generation,
                    report: WorkspaceDiagnosticReport::default(),
                    provider_reports: None,
                    virtual_uris,
                }))],
                &|| true,
            )
            .await;

        let error = result.expect_err("a response from an exited reader must not be accepted");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn final_aggregation_does_not_partially_arm_contributions() {
        let pool = LanguageServerPool::new();
        let first = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::for_server("alpha"),
            None,
        )
        .await;
        let exited = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::for_server("zeta"),
            None,
        )
        .await;
        for handle in [&first, &exited] {
            pool.connections()
                .await
                .insert(handle.key().clone(), Arc::clone(handle));
        }
        exited
            .dynamic_capabilities()
            .mark_workspace_diagnostic_reader_exited();
        let completed = [&first, &exited].map(|handle| {
            let generation = pool.document_connection_generation(handle.key());
            CompletedDiagnosticProducer {
                pull_attempt: None,
                provider_plan: diagnostic_providers(handle),
                handle: Arc::clone(handle),
                generation,
                report: WorkspaceDiagnosticReport::default(),
                provider_reports: None,
                virtual_uris: Arc::new(
                    pool.observe_virtual_uris_for_connection(handle.key(), generation),
                ),
            }
        });

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                completed
                    .into_iter()
                    .map(|completed| std::future::ready(Some(completed))),
                &|| true,
            )
            .await;

        assert!(result.is_err());
        assert!(
            !first
                .dynamic_capabilities()
                .has_workspace_diagnostic_contributed(),
            "an earlier producer must remain unarmed when a later producer cannot commit"
        );
    }

    #[tokio::test]
    async fn final_aggregation_rechecks_provider_plans_after_building_result() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("diagnostics");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        handle.dynamic_capabilities().register(vec![Registration {
            id: "alpha-registration".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "alpha",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        let provider_plan = diagnostic_providers(&handle);
        pool.connections().await.insert(key, Arc::clone(&handle));
        let generation = pool.document_connection_generation(handle.key());
        let virtual_uris =
            Arc::new(pool.observe_virtual_uris_for_connection(handle.key(), generation));
        let checks = AtomicUsize::new(0);

        let result = pool
            .aggregate_admitted_workspace_diagnostic_reports(
                [std::future::ready(Some(CompletedDiagnosticProducer {
                    pull_attempt: None,
                    provider_plan,
                    handle: Arc::clone(&handle),
                    generation,
                    report: WorkspaceDiagnosticReport::default(),
                    provider_reports: None,
                    virtual_uris,
                }))],
                &|| {
                    if checks.fetch_add(1, Ordering::SeqCst) == 2 {
                        handle
                            .dynamic_capabilities()
                            .unregister(vec![Unregistration {
                                id: "alpha-registration".into(),
                                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
                            }]);
                    }
                    true
                },
            )
            .await;

        let error = result.expect_err("a plan changed during aggregation must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(checks.load(Ordering::SeqCst), 3);
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
    async fn provider_plan_waits_for_post_initialize_dynamic_registration() {
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("diagnostics"),
        )
        .await;
        let providers = diagnostic_providers_after_registration_settle(&handle, &|| true);
        tokio::pin!(providers);
        assert!(futures::poll!(providers.as_mut()).is_pending());
        handle.dynamic_capabilities().register(vec![Registration {
            id: "dynamic".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "dynamic",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);

        assert_eq!(
            providers.await,
            vec![DiagnosticProvider {
                identifier: Some("dynamic".into()),
                has_static_provider: false,
                dynamic_registration_ids: vec!["dynamic".into()],
            }]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn provider_plan_collects_sequential_registrations_through_settle_deadline() {
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("diagnostics"),
        )
        .await;
        handle.dynamic_capabilities().register(vec![Registration {
            id: "alpha".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "alpha",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        let providers = diagnostic_providers_after_registration_settle(&handle, &|| true);
        tokio::pin!(providers);
        assert!(futures::poll!(providers.as_mut()).is_pending());
        tokio::time::advance(DYNAMIC_REGISTRATION_SETTLE / 2).await;
        handle.dynamic_capabilities().register(vec![Registration {
            id: "beta".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "beta",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        assert!(futures::poll!(providers.as_mut()).is_pending());
        tokio::time::advance(DYNAMIC_REGISTRATION_SETTLE / 2).await;

        assert_eq!(
            providers.await,
            vec![
                DiagnosticProvider {
                    identifier: Some("alpha".into()),
                    has_static_provider: false,
                    dynamic_registration_ids: vec!["alpha".into()],
                },
                DiagnosticProvider {
                    identifier: Some("beta".into()),
                    has_static_provider: false,
                    dynamic_registration_ids: vec!["beta".into()],
                },
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn provider_plan_settles_only_once_for_an_incapable_connection() {
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("diagnostics"),
        )
        .await;

        assert!(
            diagnostic_providers_after_registration_settle(&handle, &|| true)
                .await
                .is_empty()
        );
        let second = diagnostic_providers_after_registration_settle(&handle, &|| true);
        tokio::pin!(second);
        assert!(matches!(
            futures::poll!(second.as_mut()),
            std::task::Poll::Ready(providers) if providers.is_empty()
        ));
    }

    #[tokio::test]
    async fn stale_request_does_not_consume_the_registration_settle_window() {
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("diagnostics"),
        )
        .await;

        assert!(
            diagnostic_providers_after_registration_settle(&handle, &|| false)
                .await
                .is_empty()
        );
        let providers = diagnostic_providers_after_registration_settle(&handle, &|| true);
        tokio::pin!(providers);
        assert!(futures::poll!(providers.as_mut()).is_pending());
        handle.dynamic_capabilities().register(vec![Registration {
            id: "dynamic".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "dynamic",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        assert_eq!(providers.await.len(), 1);
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
                    pool_for_request.workspace_generation(),
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

        let WorkspaceDiagnosticReportResult::Report(report) = request.await.unwrap().unwrap()
        else {
            panic!("full report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full document report")
        };
        assert_eq!(report.version, None);
        assert_eq!(report.full_document_diagnostic_report.items.len(), 2);
    }

    #[tokio::test]
    async fn dispatch_fails_after_producer_replacement() {
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
                    pool_for_request.workspace_generation(),
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

        assert!(request.await.unwrap().is_err());
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
    async fn sender_rejects_a_response_after_provider_unregistration() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("diagnostics");
        let producer = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        producer.dynamic_capabilities().register(vec![Registration {
            id: "alpha-registration".into(),
            method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            register_options: Some(serde_json::json!({
                "identifier": "alpha",
                "workspaceDiagnostics": true,
                "interFileDependencies": true
            })),
        }]);
        let provider = diagnostic_providers(&producer)
            .into_iter()
            .next()
            .expect("dynamic provider");
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
                    provider,
                    None,
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
        producer
            .dynamic_capabilities()
            .unregister(vec![Unregistration {
                id: "alpha-registration".into(),
                method: DIAGNOSTIC_REGISTRATION_METHOD.into(),
            }]);
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [] }
        }));

        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
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
                .dispatch_workspace_diagnostic(
                    params,
                    &settings,
                    None,
                    &|| true,
                    pool_for_request.workspace_generation(),
                )
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
            request.await.unwrap().unwrap(),
            WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport::default())
        );
    }

    #[test]
    fn provider_combination_rejects_unusable_unchanged_reports() {
        let report = WorkspaceDiagnosticReport {
            items: vec![WorkspaceDocumentDiagnosticReport::Unchanged(
                WorkspaceUnchangedDocumentDiagnosticReport {
                    uri: Uri::from_str("file:///workspace/a.rs").unwrap(),
                    version: None,
                    unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                        result_id: "private".into(),
                    },
                },
            )],
        };

        assert!(
            combine_complete_provider_reports([(None, Ok(Some(report)))])
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dispatch_uses_client_fallback_when_shared_producer_cannot_follow_workspace() {
        let pool = Arc::new(LanguageServerPool::new());
        seed_test_client_root(&pool, "file:///workspace");
        let shared = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::shared("diagnostics"),
            None,
        )
        .await;
        record_test_spawn_root(&shared, "file:///workspace/project-a");
        let fallback = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::new("diagnostics", None),
            None,
        )
        .await;
        pool.connections().await.extend([
            (shared.key().clone(), Arc::clone(&shared)),
            (fallback.key().clone(), Arc::clone(&fallback)),
        ]);
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-diagnostics".into()]),
                languages: Some(Vec::new()),
                prefer_shared_instance: Some(true),
                ..Default::default()
            },
        );
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();
        let request_pool = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            request_pool
                .dispatch_workspace_diagnostic(
                    params,
                    &settings,
                    None,
                    &|| true,
                    request_pool.workspace_generation(),
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !fallback.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace pull reaches the client-root fallback");
        assert!(
            !shared.router().is_sent(request_id),
            "a marker-rooted incapable shared producer must not own a workspace pull"
        );
        let _ = fallback.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [] }
        }));

        assert_eq!(
            request.await.unwrap().unwrap(),
            WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport::default())
        );
    }

    #[tokio::test]
    async fn dispatch_queries_every_client_root_when_producer_lacks_workspace_folders() {
        let pool = Arc::new(LanguageServerPool::new());
        let folder_a = WorkspaceFolder {
            uri: Uri::from_str("file:///workspace/a").unwrap(),
            name: "a".into(),
        };
        let folder_b = WorkspaceFolder {
            uri: Uri::from_str("file:///workspace/b").unwrap(),
            name: "b".into(),
        };
        pool.set_root_uri(Some(folder_a.uri.to_string()));
        pool.set_workspace_folders(Some(vec![folder_a, folder_b]));
        let fallback = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::new("diagnostics", None),
            None,
        )
        .await;
        record_test_spawn_root(&fallback, "file:///workspace/a");
        let secondary = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::new("diagnostics", Some("file:///workspace/b".into())),
            None,
        )
        .await;
        record_test_spawn_root(&secondary, "file:///workspace/b");
        pool.connections().await.extend([
            (fallback.key().clone(), Arc::clone(&fallback)),
            (secondary.key().clone(), Arc::clone(&secondary)),
        ]);
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
        let request_pool = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            request_pool
                .dispatch_workspace_diagnostic(
                    params,
                    &settings,
                    None,
                    &|| true,
                    request_pool.workspace_generation(),
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !fallback.router().is_sent(request_id) || !secondary.router().is_sent(request_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace pull reaches every client-root producer");
        for handle in [&fallback, &secondary] {
            let _ = handle.router().route(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": { "items": [] }
            }));
        }

        assert_eq!(
            request.await.unwrap().unwrap(),
            WorkspaceDiagnosticReportResult::Report(WorkspaceDiagnosticReport::default())
        );
    }

    #[tokio::test]
    async fn dispatch_reconciles_reports_from_overlapping_client_roots() {
        let pool = Arc::new(LanguageServerPool::new());
        let parent_folder = WorkspaceFolder {
            uri: Uri::from_str("file:///workspace").unwrap(),
            name: "parent".into(),
        };
        let nested_folder = WorkspaceFolder {
            uri: Uri::from_str("file:///workspace/nested").unwrap(),
            name: "nested".into(),
        };
        pool.set_root_uri(Some(parent_folder.uri.to_string()));
        pool.set_workspace_folders(Some(vec![parent_folder, nested_folder]));
        let parent = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::new("diagnostics", None),
            None,
        )
        .await;
        record_test_spawn_root(&parent, "file:///workspace");
        let nested = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::new("diagnostics", Some("file:///workspace/nested".into())),
            None,
        )
        .await;
        record_test_spawn_root(&nested, "file:///workspace/nested");
        pool.connections().await.extend([
            (parent.key().clone(), Arc::clone(&parent)),
            (nested.key().clone(), Arc::clone(&nested)),
        ]);
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
        let request_pool = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            request_pool
                .dispatch_workspace_diagnostic(
                    params,
                    &settings,
                    None,
                    &|| true,
                    request_pool.workspace_generation(),
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !parent.router().is_sent(request_id) || !nested.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace pull reaches both overlapping roots");
        for (handle, message) in [(&parent, "parent"), (&nested, "nested")] {
            let report = WorkspaceDiagnosticReport {
                items: vec![full("file:///workspace/nested/doc.rs", Some(1), message)],
            };
            let _ = handle.router().route(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": report,
            }));
        }

        let WorkspaceDiagnosticReportResult::Report(report) = request.await.unwrap().unwrap()
        else {
            panic!("full workspace report")
        };
        let WorkspaceDocumentDiagnosticReport::Full(report) = &report.items[0] else {
            panic!("full document report")
        };
        assert_eq!(
            report.full_document_diagnostic_report.items[0].message,
            "nested"
        );
        assert_eq!(report.full_document_diagnostic_report.items.len(), 1);
    }

    #[tokio::test]
    async fn dispatch_rejects_a_workspace_update_in_progress_before_send() {
        let pool = Arc::new(LanguageServerPool::new());
        seed_test_client_root(&pool, "file:///workspace");
        let producer = create_handle_advertising_workspace_diagnostics(
            ConnectionKey::shared("diagnostics"),
            None,
        )
        .await;
        record_test_spawn_root(&producer, "file:///workspace");
        pool.connections()
            .await
            .insert(producer.key().clone(), Arc::clone(&producer));
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "diagnostics".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-diagnostics".into()]),
                languages: Some(Vec::new()),
                prefer_shared_instance: Some(true),
                ..Default::default()
            },
        );

        let connections = pool.connections().await;
        let updating_pool = Arc::clone(&pool);
        let update = tokio::spawn(async move {
            let change = updating_pool
                .apply_workspace_folder_change(
                    vec![WorkspaceFolder {
                        uri: Uri::from_str("file:///replacement").unwrap(),
                        name: "replacement".into(),
                    }],
                    &[],
                )
                .await
                .expect("non-empty change");
            change.finish();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.workspace_generation() & 1 == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace update publishes its in-progress generation");
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();
        let error = pool
            .dispatch_workspace_diagnostic(
                params,
                &settings,
                None,
                &|| true,
                pool.workspace_generation(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            !producer.router().is_sent(RequestId::new(2)),
            "an in-progress workspace snapshot must not reach the wire"
        );

        drop(connections);
        update.await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_fails_when_workspace_changes_during_fanout() {
        let pool = Arc::new(LanguageServerPool::new());
        seed_test_client_root(&pool, "file:///workspace");
        let first =
            create_handle_advertising_workspace_diagnostics(ConnectionKey::shared("alpha"), None)
                .await;
        let second =
            create_handle_advertising_workspace_diagnostics(ConnectionKey::shared("zeta"), None)
                .await;
        record_test_spawn_root(&first, "file:///workspace");
        record_test_spawn_root(&second, "file:///workspace");
        pool.connections().await.extend([
            (first.key().clone(), Arc::clone(&first)),
            (second.key().clone(), Arc::clone(&second)),
        ]);
        let mut settings = WorkspaceSettings::default();
        for name in ["alpha", "zeta"] {
            settings.language_servers.insert(
                name.into(),
                crate::config::settings::BridgeServerConfig {
                    cmd: Some(vec![format!("mock-{name}")]),
                    languages: Some(Vec::new()),
                    prefer_shared_instance: Some(true),
                    ..Default::default()
                },
            );
        }
        let params: WorkspaceDiagnosticParams = serde_json::from_value(serde_json::json!({
            "previousResultIds": []
        }))
        .unwrap();
        let request_generation = pool.workspace_generation();
        let admit_calls = Arc::new(AtomicUsize::new(0));
        let request_pool = Arc::clone(&pool);
        let request_admit_calls = Arc::clone(&admit_calls);
        let request = tokio::spawn(async move {
            let admit = || {
                request_admit_calls.fetch_add(1, Ordering::AcqRel);
                true
            };
            request_pool
                .dispatch_workspace_diagnostic(params, &settings, None, &admit, request_generation)
                .await
        });
        let request_id = RequestId::new(2);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !first.router().is_sent(request_id) || !second.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both workspace producers receive the request");

        let admits_before_first_response = admit_calls.load(Ordering::Acquire);
        let _ = first.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [{
                "kind": "full",
                "uri": "file:///workspace/stale.rs",
                "version": 1,
                "items": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "message": "stale"
                }]
            }] }
        }));
        tokio::time::timeout(Duration::from_secs(1), async {
            while admit_calls.load(Ordering::Acquire) == admits_before_first_response {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first producer report passes its response fence");

        pool.apply_workspace_folder_change(
            vec![WorkspaceFolder {
                uri: Uri::from_str("file:///replacement").unwrap(),
                name: "replacement".into(),
            }],
            &[],
        )
        .await
        .expect("non-empty change")
        .finish();
        let _ = second.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": { "items": [] }
        }));

        assert!(request.await.unwrap().is_err());
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

        let error = pool
            .dispatch_workspace_diagnostic(
                params,
                &settings,
                None,
                &|| false,
                pool.workspace_generation(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(pool.connections().await.is_empty());
    }
}
