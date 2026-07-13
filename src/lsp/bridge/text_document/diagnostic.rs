//! Diagnostic request handling for bridge connections.
//!
//! This module provides pull diagnostic request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! Like document symbol, diagnostic requests operate on the entire document -
//! they don't take a position parameter.
//!
//! Implements Pull-first diagnostic forwarding (pull-first-diagnostic-forwarding Phase 1).
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;
use std::time::Duration;

use crate::config::settings::BridgeServerConfig;
use crate::error::LockResultExt;
use tower_lsp_server::ls_types::{Diagnostic, DocumentDiagnosticReport};
use url::Url;

use super::super::pool::{INIT_TIMEOUT_SECS, LanguageServerPool, UpstreamId};
use tower_lsp_server::ls_types::TextDocumentIdentifier;

use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, jsonrpc_error_summary,
    response_has_jsonrpc_error, virtual_uri_to_lsp_uri,
};

impl LanguageServerPool {
    /// Send a diagnostic request and wait for the response.
    ///
    /// Unlike other request types that fail fast when a server is initializing,
    /// diagnostic requests wait for the server to become Ready so users see
    /// diagnostics appear rather than empty results. After the wait and capability
    /// check, delegates to
    /// [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_diagnostic_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Vec<Diagnostic>> {
        let (host_generation, request_sequence) = self.begin_diagnostic_pull(host_uri);
        // Pre-work: wait for server to become Ready (unlike other handlers that fail fast).
        let handle = self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
                Some(host_uri),
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await?;

        // Skip if server doesn't advertise pull diagnostic support
        // (checked via both static capabilities and dynamic registrations).
        if !handle.has_capability("textDocument/diagnostic") {
            log::debug!(
                target: "kakehashi::bridge",
                "[{}] Server does not support textDocument/diagnostic, skipping",
                server_name
            );
            return Ok(Vec::new());
        }

        // Server is Ready and supports diagnostics — proceed with standard lifecycle.
        // Use execute_bridge_request_with_handle to reuse the pre-fetched handle,
        // avoiding a redundant HashMap lookup.
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(host_uri)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let virtual_uri = VirtualDocumentUri::new(&host_uri_lsp, injection_language, region_id);
        let cache_key = (handle.key().clone(), virtual_uri.to_uri_string());
        let snapshot = self.diagnostic_pull_snapshot(
            &cache_key,
            host_uri.as_str(),
            host_generation,
            request_sequence,
        );
        let previous_result_id = snapshot
            .baseline
            .as_ref()
            .map(|entry| entry.result_id.clone());

        let report = self
            .execute_bridge_request_with_handle(
                handle,
                host_uri,
                injection_language,
                region_id,
                &offset,
                virtual_content,
                upstream_request_id,
                |virtual_uri, request_id| {
                    build_diagnostic_request(virtual_uri, request_id, previous_result_id.as_deref())
                },
                |response, _ctx| {
                    // A downstream JSON-RPC error response is a request failure, not
                    // "no diagnostics" — propagate it as `Err` so CLI mode's
                    // request-error sink can surface it (mirrors the formatting
                    // path; LSP mode just logs and the layer stays empty).
                    if response_has_jsonrpc_error(&response, "textDocument/diagnostic") {
                        return Err(io::Error::other(format!(
                            "downstream server '{server_name}' answered textDocument/diagnostic \
                         with an error response: {}",
                            jsonrpc_error_summary(&response)
                        )));
                    }
                    // A present-but-malformed payload (absent `result`, unknown
                    // report kind, missing/garbled `items`) is likewise a request
                    // failure; `null` clears the baseline and `unchanged` reuses it.
                    parse_downstream_diagnostic_report(response)
                },
            )
            .await??;

        let mut diagnostics = self.resolve_diagnostic_pull_report(
            &cache_key,
            snapshot,
            report,
            self.is_virtual_doc_open_on_connection(&cache_key.1, &cache_key.0),
        );
        for diagnostic in &mut diagnostics {
            transform_diagnostic(diagnostic, &offset, host_uri.as_str());
        }
        Ok(diagnostics)
    }

    pub(super) fn resolve_diagnostic_pull_report(
        &self,
        cache_key: &(super::super::pool::ConnectionKey, String),
        snapshot: super::super::pool::DiagnosticPullSnapshot,
        report: DownstreamDiagnosticReport,
        document_is_open: bool,
    ) -> Vec<Diagnostic> {
        let _guard = self
            .diagnostic_pull_lock
            .lock()
            .recover_poison("LanguageServerPool::diagnostic_pull_lock");
        let current_document_generation = self
            .diagnostic_document_generations
            .get(cache_key)
            .map(|entry| *entry)
            .unwrap_or(0);
        let lineage_is_current = document_is_open
            && snapshot.settings_generation
                == self
                    .diagnostic_pull_generations
                    .get(&cache_key.0)
                    .map(|entry| *entry)
                    .unwrap_or(0)
            && snapshot.document_generation == current_document_generation
            && snapshot.connection_generation == self.document_connection_generation(&cache_key.0);
        let lineage_is_current = lineage_is_current
            && snapshot.host_generation
                == self
                    .diagnostic_host_generations
                    .get(&snapshot.host_uri)
                    .map(|entry| *entry)
                    .unwrap_or(0);
        if !lineage_is_current {
            return match report {
                DownstreamDiagnosticReport::Full { diagnostics, .. } => diagnostics,
                DownstreamDiagnosticReport::Unchanged { .. } | DownstreamDiagnosticReport::Null => {
                    Vec::new()
                }
            };
        }
        let may_apply = self
            .diagnostic_pull_applied_sequences
            .get(cache_key)
            .is_none_or(|sequence| *sequence < snapshot.request_sequence);
        match report {
            DownstreamDiagnosticReport::Full {
                result_id,
                diagnostics,
            } => {
                if may_apply && let Some(result_id) = result_id {
                    self.store_diagnostic_baseline_if_current(
                        cache_key,
                        snapshot.request_sequence,
                        result_id,
                        diagnostics.clone(),
                    );
                } else if may_apply {
                    self.diagnostic_pull_baselines.remove(cache_key);
                }
                if may_apply {
                    self.diagnostic_pull_applied_sequences
                        .insert(cache_key.clone(), snapshot.request_sequence);
                }
                diagnostics
            }
            DownstreamDiagnosticReport::Unchanged { result_id } => {
                if may_apply && let Some(baseline) = &snapshot.baseline {
                    self.store_diagnostic_baseline_if_current(
                        cache_key,
                        snapshot.request_sequence,
                        result_id,
                        baseline.diagnostics.as_ref().clone(),
                    );
                }
                if may_apply {
                    self.diagnostic_pull_applied_sequences
                        .insert(cache_key.clone(), snapshot.request_sequence);
                }
                diagnostics_for_unchanged_baseline(
                    snapshot
                        .baseline
                        .as_ref()
                        .map(|entry| entry.diagnostics.as_slice()),
                )
            }
            DownstreamDiagnosticReport::Null => {
                if may_apply {
                    self.diagnostic_pull_baselines.remove(cache_key);
                    self.diagnostic_pull_applied_sequences
                        .insert(cache_key.clone(), snapshot.request_sequence);
                }
                Vec::new()
            }
        }
    }

    fn store_diagnostic_baseline_if_current(
        &self,
        cache_key: &(super::super::pool::ConnectionKey, String),
        request_sequence: u64,
        result_id: String,
        diagnostics: Vec<Diagnostic>,
    ) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.diagnostic_pull_baselines.entry(cache_key.clone()) {
            Entry::Occupied(mut entry) if entry.get().request_sequence < request_sequence => {
                entry.insert(super::super::pool::DiagnosticPullBaseline {
                    result_id,
                    diagnostics: std::sync::Arc::new(diagnostics),
                    request_sequence,
                });
                true
            }
            Entry::Vacant(entry) => {
                entry.insert(super::super::pool::DiagnosticPullBaseline {
                    result_id,
                    diagnostics: std::sync::Arc::new(diagnostics),
                    request_sequence,
                });
                true
            }
            Entry::Occupied(_) => false,
        }
    }
}

/// `DocumentDiagnosticParams` for the wire, omitting absent optionals.
///
/// `ls_types::DocumentDiagnosticParams` serializes `identifier: None` and
/// `previous_result_id: None` as explicit JSON `null`s (no
/// `skip_serializing_if`), and strictly-decoding downstreams (typescript-go's
/// Go unmarshaler) reject those nulls with InvalidParams on every pull. The
/// spec marks both fields optional, so absent is the interoperable encoding.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticRequestParams {
    text_document: TextDocumentIdentifier,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_result_id: Option<String>,
}

/// Build a JSON-RPC diagnostic request for a downstream language server.
///
/// Like `textDocument/documentSymbol`, this request operates on the entire
/// document; an optional `previous_result_id` enables incremental updates.
fn build_diagnostic_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
    previous_result_id: Option<&str>,
) -> JsonRpcRequest<DiagnosticRequestParams> {
    let params = DiagnosticRequestParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        previous_result_id: previous_result_id.map(|s| s.to_string()),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/diagnostic", params)
}

/// Parse a successful JSON-RPC diagnostic response and transform coordinates to
/// host document space.
///
/// The caller ([`LanguageServerPool::send_diagnostic_request`]) handles a
/// JSON-RPC *error* response separately (it propagates as `Err`), so this only
/// inspects the `result`. Mirroring [`transform_formatting_response_to_host`],
/// a present-but-malformed payload is a request **failure** (`Err`): an absent
/// `result` member (protocol violation), or a non-null `result` that does not
/// deserialize as a `DocumentDiagnosticReport` (no/unknown `kind`, a `full`
/// report missing `items`, a malformed `unchanged` report, or `items` that
/// aren't `Diagnostic[]`) — validated by the same typed deserialize the host
/// parser uses. A `null` result is authoritative empty; a valid `unchanged`
/// report is resolved against the request's cached baseline by the caller.
/// Collapsing a malformed payload into the same empty value as "no
/// capability" would let a broken downstream server pass as "nothing wrong" —
/// the fan-in counts `Err`s, which CLI mode maps onto its `2` exit code and
/// the editor path logs at WARNING. Only related info matching `host_uri` is
/// transformed.
#[cfg(test)]
fn transform_diagnostic_response_to_host(
    response: serde_json::Value,
    offset: &RegionOffset,
    host_uri: &str,
) -> io::Result<Vec<Diagnostic>> {
    let report = parse_downstream_diagnostic_report(response)?;
    let mut diagnostics = match report {
        DownstreamDiagnosticReport::Full { diagnostics, .. } => diagnostics,
        DownstreamDiagnosticReport::Unchanged { .. } | DownstreamDiagnosticReport::Null => {
            Vec::new()
        }
    };
    for diag in &mut diagnostics {
        transform_diagnostic(diag, offset, host_uri);
    }
    Ok(diagnostics)
}

pub(super) enum DownstreamDiagnosticReport {
    Full {
        result_id: Option<String>,
        diagnostics: Vec<Diagnostic>,
    },
    Unchanged {
        result_id: String,
    },
    Null,
}

pub(super) fn parse_downstream_diagnostic_report(
    mut response: serde_json::Value,
) -> io::Result<DownstreamDiagnosticReport> {
    // Absent `result` with no error (the caller already promoted an error
    // response to `Err`) is a protocol violation — a request failure.
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        return Err(io::Error::other(
            "diagnostic response carries neither result nor error (protocol violation)",
        ));
    };
    if result.is_null() {
        return Ok(DownstreamDiagnosticReport::Null);
    }

    // Validate the whole report via the typed `DocumentDiagnosticReport`
    // (identical to the host parser, `parse_host_diagnostic_response`, so the
    // two paths reject the same shapes): an absent/unknown `kind`, a `full`
    // report missing `items`, a malformed `unchanged` report or
    // `relatedDocuments`, or `items` that don't deserialize as `Diagnostic[]`
    // is a malformed payload (request failure). A valid `unchanged` report is
    // the authoritative empty. `relatedDocuments` (diagnostics for OTHER
    // documents) are dropped — they have no place in this region's report.
    match serde_json::from_value::<DocumentDiagnosticReport>(result) {
        Ok(DocumentDiagnosticReport::Full(report)) => Ok(DownstreamDiagnosticReport::Full {
            result_id: report.full_document_diagnostic_report.result_id,
            diagnostics: report.full_document_diagnostic_report.items,
        }),
        Ok(DocumentDiagnosticReport::Unchanged(report)) => {
            Ok(DownstreamDiagnosticReport::Unchanged {
                result_id: report.unchanged_document_diagnostic_report.result_id,
            })
        }
        Err(err) => {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to deserialize diagnostic report: {err}"
            );
            Err(io::Error::other(format!(
                "malformed diagnostic result from downstream server: {err}"
            )))
        }
    }
}

fn diagnostics_for_unchanged_baseline(baseline: Option<&[Diagnostic]>) -> Vec<Diagnostic> {
    baseline.unwrap_or_default().to_vec()
}

/// Transform a single typed Diagnostic by applying the region offset to its range.
///
/// Also transforms relatedInformation locations if present, filtering out entries
/// that reference virtual URIs (which clients cannot resolve).
fn transform_diagnostic(diag: &mut Diagnostic, offset: &RegionOffset, host_uri: &str) {
    // Transform main range
    translate_virtual_range_to_host(&mut diag.range, offset);

    // Transform related information
    if let Some(related) = &mut diag.related_information {
        related.retain_mut(|info| {
            let uri_str = info.location.uri.as_str();
            if VirtualDocumentUri::is_virtual_uri(uri_str) {
                // Virtual URI - filter out this entry
                return false;
            }

            // Only transform ranges for entries that reference the same host document.
            // Related info pointing to other files (e.g., imported modules) should
            // keep their original coordinates since they're not in the injection region.
            if uri_str == host_uri {
                translate_virtual_range_to_host(&mut info.location.range, offset);
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;

    fn test_snapshot(
        pool: &LanguageServerPool,
        key: &(super::super::super::pool::ConnectionKey, String),
    ) -> super::super::super::pool::DiagnosticPullSnapshot {
        let host = Url::parse("file:///host.md").unwrap();
        let (host_generation, request_sequence) = pool.begin_diagnostic_pull(&host);
        pool.diagnostic_pull_snapshot(key, host.as_str(), host_generation, request_sequence)
    }

    fn resolve_current(
        pool: &LanguageServerPool,
        key: &(super::super::super::pool::ConnectionKey, String),
        baseline: Option<super::super::super::pool::DiagnosticPullBaseline>,
        report: DownstreamDiagnosticReport,
    ) -> Vec<Diagnostic> {
        let mut snapshot = test_snapshot(pool, key);
        snapshot.baseline = baseline;
        pool.resolve_diagnostic_pull_report(key, snapshot, report, true)
    }

    #[test]
    fn unchanged_baseline_is_reanchored_with_the_current_offset() {
        let baseline = vec![Diagnostic {
            range: tower_lsp_server::ls_types::Range::new(
                tower_lsp_server::ls_types::Position::new(1, 2),
                tower_lsp_server::ls_types::Position::new(1, 5),
            ),
            message: "cached".to_string(),
            ..Default::default()
        }];

        let mut diagnostics = diagnostics_for_unchanged_baseline(Some(&baseline));
        for diagnostic in &mut diagnostics {
            transform_diagnostic(diagnostic, &RegionOffset::new(20, 0), "file:///host.md");
        }

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, 21);
        assert_eq!(
            baseline[0].range.start.line, 1,
            "cache remains virtual-local"
        );
    }

    #[test]
    fn full_report_establishes_baseline_and_unchanged_reuses_it() {
        let pool = LanguageServerPool::new();
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls"),
            "kakehashi-virtual://region-1".to_string(),
        );
        let diagnostics = vec![Diagnostic {
            message: "cached".to_string(),
            ..Default::default()
        }];

        assert_eq!(
            resolve_current(
                &pool,
                &key,
                None,
                DownstreamDiagnosticReport::Full {
                    result_id: Some("r1".to_string()),
                    diagnostics: diagnostics.clone(),
                },
            ),
            diagnostics
        );
        let baseline = pool
            .diagnostic_pull_baselines
            .get(&key)
            .map(|entry| entry.value().clone());
        assert_eq!(
            baseline.as_ref().map(|entry| entry.result_id.as_str()),
            Some("r1")
        );
        assert_eq!(
            resolve_current(
                &pool,
                &key,
                baseline,
                DownstreamDiagnosticReport::Unchanged {
                    result_id: "r2".to_string(),
                },
            ),
            diagnostics
        );
        assert_eq!(
            pool.diagnostic_pull_baselines.get(&key).unwrap().result_id,
            "r2",
            "unchanged advances to the resultId returned for the next pull"
        );
    }

    #[test]
    fn late_full_report_cannot_replace_a_newer_baseline() {
        let pool = LanguageServerPool::new();
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls"),
            "kakehashi-virtual://region-1".to_string(),
        );
        let old = super::super::super::pool::DiagnosticPullBaseline {
            result_id: "r1".to_string(),
            diagnostics: std::sync::Arc::new(vec![Diagnostic::default()]),
            request_sequence: 1,
        };
        pool.diagnostic_pull_baselines.insert(
            key.clone(),
            super::super::super::pool::DiagnosticPullBaseline {
                result_id: "r2".to_string(),
                diagnostics: std::sync::Arc::new(vec![Diagnostic {
                    message: "new".to_string(),
                    ..Default::default()
                }]),
                request_sequence: 2,
            },
        );

        resolve_current(
            &pool,
            &key,
            Some(old),
            DownstreamDiagnosticReport::Full {
                result_id: Some("late".to_string()),
                diagnostics: vec![Diagnostic {
                    message: "late".to_string(),
                    ..Default::default()
                }],
            },
        );

        let current = pool.diagnostic_pull_baselines.get(&key).unwrap();
        assert_eq!(current.result_id, "r2");
        assert_eq!(current.diagnostics[0].message, "new");
    }

    #[test]
    fn newer_request_wins_in_both_response_completion_orders() {
        fn final_result_id(newer_completes_first: bool) -> String {
            let pool = LanguageServerPool::new();
            let key = (
                super::super::super::pool::ConnectionKey::for_server("lua_ls"),
                "kakehashi-virtual://region-1".to_string(),
            );
            pool.diagnostic_pull_baselines.insert(
                key.clone(),
                super::super::super::pool::DiagnosticPullBaseline {
                    result_id: "r1".to_string(),
                    diagnostics: std::sync::Arc::new(Vec::new()),
                    request_sequence: 0,
                },
            );
            pool.diagnostic_pull_applied_sequences
                .insert(key.clone(), 0);
            let older = test_snapshot(&pool, &key);
            let newer = test_snapshot(&pool, &key);
            let resolve = |snapshot, result_id: &str| {
                pool.resolve_diagnostic_pull_report(
                    &key,
                    snapshot,
                    DownstreamDiagnosticReport::Full {
                        result_id: Some(result_id.to_string()),
                        diagnostics: Vec::new(),
                    },
                    true,
                );
            };
            if newer_completes_first {
                resolve(newer, "r3");
                resolve(older, "r2");
            } else {
                resolve(older, "r2");
                resolve(newer, "r3");
            }
            pool.diagnostic_pull_baselines
                .get(&key)
                .unwrap()
                .result_id
                .clone()
        }

        assert_eq!(final_result_id(false), "r3");
        assert_eq!(final_result_id(true), "r3");
    }

    #[test]
    fn stale_lineage_cannot_remove_a_newer_baseline() {
        let pool = LanguageServerPool::new();
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls"),
            "kakehashi-virtual://region-1".to_string(),
        );
        pool.diagnostic_pull_baselines.insert(
            key.clone(),
            super::super::super::pool::DiagnosticPullBaseline {
                result_id: "r1".to_string(),
                diagnostics: std::sync::Arc::new(Vec::new()),
                request_sequence: 3,
            },
        );
        pool.diagnostic_pull_applied_sequences
            .insert(key.clone(), 3);
        let old = super::super::super::pool::DiagnosticPullBaseline {
            result_id: "r1".to_string(),
            diagnostics: std::sync::Arc::new(Vec::new()),
            request_sequence: 1,
        };

        resolve_current(&pool, &key, Some(old), DownstreamDiagnosticReport::Null);

        assert_eq!(
            pool.diagnostic_pull_baselines.get(&key).unwrap().result_id,
            "r1"
        );
    }

    #[test]
    fn document_close_fences_a_response_from_the_previous_incarnation() {
        let pool = LanguageServerPool::new();
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls"),
            "kakehashi-virtual://region-1".to_string(),
        );
        let snapshot = test_snapshot(&pool, &key);
        pool.invalidate_diagnostic_document(&key);

        pool.resolve_diagnostic_pull_report(
            &key,
            snapshot,
            DownstreamDiagnosticReport::Full {
                result_id: Some("old".to_string()),
                diagnostics: Vec::new(),
            },
            true,
        );

        assert!(!pool.diagnostic_pull_baselines.contains_key(&key));
    }

    #[test]
    fn host_close_fences_a_first_pull_not_yet_tracked_downstream() {
        let pool = LanguageServerPool::new();
        let host = Url::parse("file:///host.md").unwrap();
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls"),
            "kakehashi-virtual://region-1".to_string(),
        );
        let (host_generation, request_sequence) = pool.begin_diagnostic_pull(&host);
        // Close lands while the first pull awaits connection readiness, before
        // it can create a downstream cache key/snapshot.
        pool.invalidate_diagnostic_host(&host);
        let snapshot =
            pool.diagnostic_pull_snapshot(&key, host.as_str(), host_generation, request_sequence);

        // No downstream document/baseline existed when close ran.
        pool.resolve_diagnostic_pull_report(
            &key,
            snapshot,
            DownstreamDiagnosticReport::Full {
                result_id: Some("old-incarnation".to_string()),
                diagnostics: Vec::new(),
            },
            true,
        );

        assert!(!pool.diagnostic_pull_baselines.contains_key(&key));
    }

    #[test]
    fn settings_invalidation_is_atomic_with_baseline_snapshot_and_resolution() {
        let pool = LanguageServerPool::new();
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls"),
            "kakehashi-virtual://region-1".to_string(),
        );
        pool.diagnostic_pull_baselines.insert(
            key.clone(),
            super::super::super::pool::DiagnosticPullBaseline {
                result_id: "r1".to_string(),
                diagnostics: std::sync::Arc::new(vec![Diagnostic {
                    message: "old settings".to_string(),
                    ..Default::default()
                }]),
                request_sequence: 1,
            },
        );
        let snapshot = test_snapshot(&pool, &key);

        pool.invalidate_diagnostic_connections(std::slice::from_ref(&key.0));
        let diagnostics = pool.resolve_diagnostic_pull_report(
            &key,
            snapshot,
            DownstreamDiagnosticReport::Unchanged {
                result_id: "r2".to_string(),
            },
            true,
        );

        assert!(diagnostics.is_empty());
        assert!(!pool.diagnostic_pull_baselines.contains_key(&key));
    }

    #[test]
    fn unrelated_connection_invalidation_preserves_in_flight_unchanged() {
        let pool = LanguageServerPool::new();
        let affected = super::super::super::pool::ConnectionKey::for_server("lua_ls_a");
        let key = (
            super::super::super::pool::ConnectionKey::for_server("lua_ls_b"),
            "kakehashi-virtual://region-1".to_string(),
        );
        pool.diagnostic_pull_baselines.insert(
            key.clone(),
            super::super::super::pool::DiagnosticPullBaseline {
                result_id: "r1".to_string(),
                diagnostics: std::sync::Arc::new(vec![Diagnostic {
                    message: "unrelated".to_string(),
                    ..Default::default()
                }]),
                request_sequence: 0,
            },
        );
        pool.diagnostic_pull_applied_sequences
            .insert(key.clone(), 0);
        let snapshot = test_snapshot(&pool, &key);

        pool.invalidate_diagnostic_connections(std::slice::from_ref(&affected));
        let diagnostics = pool.resolve_diagnostic_pull_report(
            &key,
            snapshot,
            DownstreamDiagnosticReport::Unchanged {
                result_id: "r2".to_string(),
            },
            true,
        );

        assert_eq!(diagnostics[0].message, "unrelated");
        assert_eq!(
            pool.diagnostic_pull_baselines.get(&key).unwrap().result_id,
            "r2"
        );
    }

    #[test]
    fn diagnostic_response_transforms_range_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 5 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "syntax error",
                        "severity": 1
                    },
                    {
                        "range": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 3, "character": 5 }
                        },
                        "message": "undefined variable",
                        "severity": 2
                    }
                ]
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .unwrap();

        assert_eq!(diagnostics.len(), 2);

        // First diagnostic: line 0 + 5 = 5
        assert_eq!(diagnostics[0].range.start.line, 5);
        assert_eq!(diagnostics[0].range.end.line, 5);
        assert_eq!(diagnostics[0].range.start.character, 5); // character unchanged
        assert_eq!(diagnostics[0].message, "syntax error");

        // Second diagnostic: lines 2,3 + 5 = 7,8
        assert_eq!(diagnostics[1].range.start.line, 7);
        assert_eq!(diagnostics[1].range.end.line, 8);
    }

    #[test]
    fn diagnostic_response_transforms_related_information_for_same_host() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "unused variable 'x'",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///test.md",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "'x' is declared here"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics = transform_diagnostic_response_to_host(
            response,
            &RegionOffset::new(3, 0),
            "file:///test.md",
        )
        .unwrap();

        assert_eq!(diagnostics.len(), 1);

        // Main diagnostic range transformed
        assert_eq!(diagnostics[0].range.start.line, 3);

        // Related information location range transformed (same host file)
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.range.start.line, 8); // 5 + 3
        assert_eq!(related[0].location.range.end.line, 8);
    }

    #[test]
    fn diagnostic_response_preserves_related_info_for_different_file() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "type mismatch",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///other_module.lua",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "expected type defined here"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics = transform_diagnostic_response_to_host(
            response,
            &RegionOffset::new(3, 0),
            "file:///test.md",
        )
        .unwrap();

        assert_eq!(diagnostics.len(), 1);

        // Main diagnostic range transformed
        assert_eq!(diagnostics[0].range.start.line, 3);

        // Related info pointing to different file should NOT be transformed
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.uri.as_str(), "file:///other_module.lua");
        assert_eq!(related[0].location.range.start.line, 5); // unchanged!
        assert_eq!(related[0].location.range.end.line, 5);
    }

    #[test]
    fn spurious_diagnostic_unchanged_without_baseline_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "unchanged",
                "resultId": "prev-123"
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .expect("spurious unchanged without a baseline stays empty, not a failure");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_null_result_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .expect("null is an authoritative empty result, not a failure");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_missing_result_is_request_failure() {
        // Neither `result` nor `error` member — a protocol violation. (An
        // *error* response is promoted to `Err` by the caller before transform
        // runs, so transform only ever sees the no-result-no-error shape here.)
        let response = json!({ "jsonrpc": "2.0", "id": 42 });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(
            transformed.is_err(),
            "a response with neither result nor error must be a request failure"
        );
    }

    #[test]
    fn diagnostic_response_malformed_items_is_request_failure() {
        // A non-`Diagnostic[]` `items` is a malformed payload — must be `Err`
        // (request failure), not the lenient empty, so CLI mode can exit 2 for
        // a broken downstream server.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": "not_an_array"
            }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_unknown_kind_is_request_failure() {
        // An unrecognized report `kind` is a malformed payload — `Err`, not the
        // lenient empty.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "kind": "partial", "items": [] }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_full_report_missing_items_is_request_failure() {
        // A `full` report MUST carry `items`; its absence is malformed → `Err`.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "kind": "full" }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_report_without_kind_is_request_failure() {
        // The LSP report `kind` tag is required; a present report that omits it
        // (e.g. `{ "items": [] }`) is malformed → `Err`, matching the host
        // parser's tagged-enum deserialize. Without this the region path would
        // wrongly read it as an empty `full` report and miss the exit-2 failure.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "items": [] }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_unchanged_without_result_id_is_request_failure() {
        // An `unchanged` report MUST carry a `resultId`; its absence is
        // malformed. The whole-report typed deserialize rejects it, keeping the
        // region path's strictness identical to the host parser (a hand-rolled
        // `kind`-only check would have wrongly accepted it as an empty result).
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "kind": "unchanged" }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_empty_items_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": []
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .expect("an empty items array is an authoritative empty result, not a failure");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_filters_related_info_with_virtual_uris() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "unused variable",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///lua/kakehashi-virtual-uri-region-0.lua",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "this is a virtual URI - should be filtered"
                            },
                            {
                                "location": {
                                    "uri": "file:///real/file.lua",
                                    "range": {
                                        "start": { "line": 10, "character": 0 },
                                        "end": { "line": 10, "character": 5 }
                                    }
                                },
                                "message": "this is a real file URI - should be kept"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(3, 0), "unused")
                .unwrap();

        assert_eq!(diagnostics.len(), 1);
        let related = diagnostics[0].related_information.as_ref().unwrap();

        // Only the real file URI entry should remain
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.uri.as_str(), "file:///real/file.lua");
        // The range for real file entry is NOT transformed (different from host URI)
        assert_eq!(related[0].location.range.start.line, 10); // unchanged
    }

    #[test]
    fn diagnostic_response_filters_all_related_info_with_virtual_uris() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "unused variable",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///lua/kakehashi-virtual-uri-region-0.lua",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "first virtual URI"
                            },
                            {
                                "location": {
                                    "uri": "file:///python/kakehashi-virtual-uri-region-1.py",
                                    "range": {
                                        "start": { "line": 10, "character": 0 },
                                        "end": { "line": 10, "character": 5 }
                                    }
                                },
                                "message": "second virtual URI"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(3, 0), "unused")
                .unwrap();

        assert_eq!(diagnostics.len(), 1);
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert!(related.is_empty());
    }

    #[test]
    fn diagnostic_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_diagnostic_request(&virtual_uri, RequestId::new(42), None);

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn diagnostic_request_has_correct_method_and_structure() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_diagnostic_request(&virtual_uri, RequestId::new(123), None);

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/diagnostic");
        // Diagnostic request has no position parameter (whole-document operation)
        assert!(
            json["params"].get("position").is_none(),
            "Diagnostic request should not have position parameter"
        );
        // Without previous_result_id, the field must be ABSENT, not null:
        // strict downstream decoders (typescript-go) reject explicit nulls.
        assert!(
            json["params"].get("previousResultId").is_none(),
            "Diagnostic request without previous_result_id must omit previousResultId"
        );
        assert!(
            json["params"].get("identifier").is_none(),
            "Diagnostic request must omit the unused identifier field"
        );
    }

    #[test]
    fn diagnostic_request_includes_previous_result_id_when_provided() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_diagnostic_request(&virtual_uri, RequestId::new(123), Some("prev-result-123"));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["previousResultId"], "prev-result-123");
    }

    #[test]
    fn diagnostic_response_near_max_line_saturates() {
        // u32::MAX because lsp_types::Position.line is u32
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": u32::MAX - 10, "character": 0 },
                            "end": { "line": u32::MAX - 5, "character": 10 }
                        },
                        "message": "diagnostic at very large line number"
                    }
                ]
            }
        });

        // This should not panic due to overflow
        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(100, 0), "unused")
                .unwrap();

        // Values should saturate at u32::MAX
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, u32::MAX);
        assert_eq!(diagnostics[0].range.end.line, u32::MAX);
    }
}
