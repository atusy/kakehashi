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
use url::Url;

use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

use super::diagnostic::collect_region_diagnostics;

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
