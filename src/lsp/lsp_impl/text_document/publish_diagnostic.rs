//! Synthetic push diagnostics for ADR-0020 Phase 2.
//!
//! This module handles proactive diagnostic publishing triggered by
//! `didSave` and `didOpen` events. Unlike pull diagnostics (`diagnostic.rs`),
//! these are pushed to the client via `textDocument/publishDiagnostics`.
//!
//! # Architecture
//!
//! ```text
//! didSave/didOpen
//!       │
//!       ▼
//! spawn_synthetic_diagnostic_task()
//!       │
//!       ├─► prepare_diagnostic_snapshot() [sync: per-region contexts]
//!       │
//!       └─► tokio::spawn [async: background task]
//!               │
//!               ▼
//!           JoinSet { collect_region_diagnostics() per region }
//!               │
//!               ▼
//!           client.publish_diagnostics()
//! ```
//!
//! # Superseding Pattern
//!
//! When multiple saves occur rapidly, earlier tasks are aborted via
//! `SyntheticDiagnosticsManager` to prevent stale diagnostics from
//! being published. Only the latest task completes.

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::ls_types::Uri;
use url::Url;

use crate::config::settings::AggregationStrategy;
use crate::language::InjectionResolver;
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

use super::super::Kakehashi;
use super::diagnostic::collect_region_diagnostics;

/// Logging target for synthetic push diagnostics.
const LOG_TARGET: &str = "kakehashi::synthetic_diag";

impl Kakehashi {
    /// Spawn a background task to collect and publish diagnostics.
    ///
    /// ADR-0020 Phase 2: Synthetic push on didSave/didOpen.
    ///
    /// The task:
    /// 1. Registers itself with `SyntheticDiagnosticsManager` (superseding any previous task)
    /// 2. Collects diagnostics via fan-out to downstream servers
    /// 3. Publishes diagnostics via `textDocument/publishDiagnostics`
    ///
    /// # Arguments
    /// * `uri` - The document URI (url::Url for internal use)
    /// * `lsp_uri` - The document URI (ls_types::Uri for LSP notification)
    pub(crate) fn spawn_synthetic_diagnostic_task(&self, uri: Url, lsp_uri: Uri) {
        // Clone what we need for the background task
        let client = self.client.clone();

        // Get snapshot data before spawning (extracts all necessary data synchronously)
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);
        let bridge_pool = self.bridge.pool_arc();
        let uri_clone = uri.clone();

        // Spawn the background task
        //
        // Note: There's a brief window between spawn and register_task where the task
        // could complete before being registered. This is benign because:
        // 1. AbortHandle.abort() is a no-op for already-completed tasks
        // 2. The superseding logic still works correctly (abort on stale handle is safe)
        // 3. Completing tasks don't need cleanup since they ran to completion
        let task = tokio::spawn(async move {
            let diagnostics =
                collect_push_diagnostics(snapshot_data, &bridge_pool, &uri_clone, LOG_TARGET).await;

            let Some(diagnostics) = diagnostics else {
                return;
            };

            log::debug!(
                target: LOG_TARGET,
                "Collected {} diagnostics for {}",
                diagnostics.len(),
                uri_clone
            );

            // Publish diagnostics
            client.publish_diagnostics(lsp_uri, diagnostics, None).await;
        });

        // Register the task (superseding any previous task for this document)
        self.synthetic_diagnostics
            .register_task(uri, task.abort_handle());
    }

    /// Prepare per-region diagnostic contexts for a background task.
    ///
    /// This extracts all necessary data synchronously before spawning,
    /// avoiding lifetime issues with `self` references in async tasks.
    /// Each region gets its own `DocumentRequestContext` with aggregation
    /// priorities resolved for `"textDocument/publishDiagnostics"`.
    ///
    /// # Returns
    ///
    /// - `None`: Document doesn't exist, has no snapshot, or lacks language/injection configuration
    /// - `Some(Vec::new())`: Document exists but has no injection regions (caller should clear diagnostics)
    /// - `Some(vec![...])`: Injection regions found, ready for diagnostic requests
    ///
    /// Used by both immediate synthetic diagnostics (didSave/didOpen) and
    /// debounced diagnostics (didChange).
    pub(crate) fn prepare_diagnostic_snapshot(
        &self,
        uri: &Url,
    ) -> Option<Vec<DocumentRequestContext>> {
        // Get document snapshot
        let snapshot = {
            let doc = self.documents.get(uri)?;
            doc.snapshot()?
        };

        // Get language for document
        let language_name = self.get_language_for_document(uri)?;

        // Get injection query
        let injection_query = self.language.get_injection_query(&language_name)?;

        // Collect all injection regions
        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.region_id_tracker(),
            uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Some(Vec::new());
        }

        // Build one DocumentRequestContext per region (same structure as diagnostic_impl)
        let mut contexts = Vec::new();
        for resolved in all_regions {
            let configs = self
                .get_all_bridge_configs_for_language(&language_name, &resolved.injection_language);
            if configs.is_empty() {
                continue;
            }

            let priorities = self.resolve_aggregation_priorities(
                &language_name,
                &resolved.injection_language,
                "textDocument/publishDiagnostics",
            );
            let strategy = self.resolve_aggregation_strategy(
                &language_name,
                &resolved.injection_language,
                "textDocument/publishDiagnostics",
                AggregationStrategy::Concatenated,
            );

            contexts.push(DocumentRequestContext {
                uri: uri.clone(),
                resolved,
                configs,
                upstream_request_id: None, // Push diagnostics are synthetic
                priorities,
                strategy,
            });
        }

        Some(contexts)
    }
}

/// Collect diagnostics from all regions using priority-aware aggregation.
///
/// Shared logic for both immediate (didSave/didOpen) and debounced (didChange)
/// push diagnostics. Returns `None` if there's no snapshot data, or
/// `Some(diagnostics)` (possibly empty to clear previous diagnostics).
pub(crate) async fn collect_push_diagnostics(
    snapshot_data: Option<Vec<DocumentRequestContext>>,
    pool: &Arc<LanguageServerPool>,
    uri: &Url,
    log_target: &'static str,
) -> Option<Vec<tower_lsp_server::ls_types::Diagnostic>> {
    let region_contexts = snapshot_data?;

    if region_contexts.is_empty() {
        log::debug!(
            target: log_target,
            "No injection regions or bridge configs found in {}",
            uri
        );
        // Return empty to signal caller should clear diagnostics
        return Some(Vec::new());
    }

    let mut join_set = JoinSet::new();
    for region_ctx in region_contexts {
        let pool = Arc::clone(pool);
        join_set.spawn(async move { collect_region_diagnostics(&region_ctx, pool).await });
    }

    let mut all_diagnostics = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(diags) => all_diagnostics.extend(diags),
            Err(join_err) => {
                log::warn!(
                    target: log_target,
                    "Diagnostic region task panicked: {}",
                    join_err
                );
            }
        }
    }

    Some(all_diagnostics)
}
