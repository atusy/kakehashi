//! Debounced diagnostic triggers for ADR-0020 Phase 3.
//!
//! This module provides debouncing for `didChange` events, triggering synthetic
//! diagnostics after a configurable delay. Each document has an independent
//! debounce timer that resets on each change.
//!
//! # Architecture
//!
//! ```text
//! didChange event
//!       │
//!       ▼
//! schedule_debounced_diagnostic()
//!       │
//!       ├─► Cancel previous timer (if any)
//!       │
//!       └─► Capture snapshot data immediately
//!               │
//!               └─► Spawn new timer task
//!                       │
//!                       ├─► Wait debounce duration (500ms default)
//!                       │
//!                       └─► Execute diagnostic collection and publish
//! ```
//!
//! # Key Design Decision: Snapshot at Schedule Time
//!
//! The diagnostic snapshot data is captured when `schedule_debounced_diagnostic`
//! is called, not when the timer fires. This ensures:
//!
//! 1. **Consistency**: The snapshot matches the document state that triggered the change
//! 2. **Simplicity**: No need for `self` reference in the timer callback
//! 3. **Correctness**: Even if document changes again, the superseding logic
//!    (via `SyntheticDiagnosticsManager`) ensures only the latest diagnostics publish
//!
//! # Relationship to SyntheticDiagnosticsManager
//!
//! - `DebouncedDiagnosticsManager`: Debounce timers, cancellation on new change
//! - `SyntheticDiagnosticsManager`: Task superseding (via AbortHandle), prevents stale publishes

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::task::AbortHandle;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::Uri;
use url::Url;

use super::bridge::LanguageServerPool;
use super::lsp_impl::bridge_context::DocumentRequestContext;
use super::lsp_impl::text_document::publish_diagnostic::collect_push_diagnostics;
use super::synthetic_diagnostics::SyntheticDiagnosticsManager;

/// Default debounce duration for `didChange` events (500ms).
///
/// This value balances responsiveness with avoiding excessive diagnostic
/// requests during rapid typing. It matches common IDE debounce patterns.
pub(crate) const DEFAULT_DEBOUNCE_DURATION: Duration = Duration::from_millis(500);

/// Logging target for debounced diagnostics.
const LOG_TARGET: &str = "kakehashi::debounced_diag";

/// Information captured at debounce schedule time for later execution.
///
/// This contains everything needed to execute the diagnostic collection
/// and publishing when the debounce timer fires.
struct DebouncedDiagnosticData {
    /// The document URI (url::Url for internal use)
    uri: Url,
    /// The document URI (ls_types::Uri for LSP notification)
    lsp_uri: Uri,
    /// LSP client for publishing diagnostics
    client: Client,
    /// Pre-captured per-region contexts (None if document has no injections)
    snapshot_data: Option<Vec<DocumentRequestContext>>,
    /// Bridge pool for sending diagnostic requests
    bridge_pool: Arc<LanguageServerPool>,
    /// Reference to synthetic diagnostics manager for task registration
    synthetic_diagnostics: Arc<SyntheticDiagnosticsManager>,
}

/// Manager for debounced diagnostic triggers.
///
/// Tracks per-document debounce timers. When a timer expires, it executes
/// the diagnostic collection that was scheduled with `schedule`.
///
/// # Thread Safety
///
/// Uses `DashMap` for lock-free concurrent access from multiple tokio tasks.
pub(crate) struct DebouncedDiagnosticsManager {
    /// Active debounce timers per document.
    /// The AbortHandle allows cancelling the timer when a new change arrives.
    active_timers: DashMap<Url, AbortHandle>,

    /// Duration to wait after the last change before triggering diagnostics.
    debounce_duration: Duration,
}

impl Default for DebouncedDiagnosticsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DebouncedDiagnosticsManager {
    /// Create a new manager with the default debounce duration.
    pub(crate) fn new() -> Self {
        Self::with_duration(DEFAULT_DEBOUNCE_DURATION)
    }

    /// Create a new manager with a custom debounce duration.
    pub(crate) fn with_duration(debounce_duration: Duration) -> Self {
        Self {
            active_timers: DashMap::new(),
            debounce_duration,
        }
    }

    /// Schedule a debounced diagnostic for a document.
    ///
    /// If there's an existing timer for this document, it's cancelled and
    /// a new timer is started. When the timer expires, the diagnostic
    /// collection and publishing is executed with the pre-captured data.
    ///
    /// # Arguments
    /// * `uri` - The document URI (url::Url)
    /// * `lsp_uri` - The document URI (ls_types::Uri)
    /// * `client` - LSP client for publishing
    /// * `snapshot_data` - Pre-captured per-region contexts (None if no injections)
    /// * `bridge_pool` - Pool for downstream server communication
    /// * `synthetic_diagnostics` - Manager for task superseding
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn schedule(
        &self,
        uri: Url,
        lsp_uri: Uri,
        client: Client,
        snapshot_data: Option<Vec<DocumentRequestContext>>,
        bridge_pool: Arc<LanguageServerPool>,
        synthetic_diagnostics: Arc<SyntheticDiagnosticsManager>,
    ) {
        // Opportunistic cleanup: remove finished timers to prevent unbounded growth.
        // This is O(n) but runs infrequently and keeps the map from accumulating
        // stale entries for documents that completed their debounce cycle.
        const CLEANUP_THRESHOLD: usize = 32;
        if self.active_timers.len() > CLEANUP_THRESHOLD {
            self.active_timers.retain(|_, handle| !handle.is_finished());
        }

        // Cancel existing timer if any
        if let Some((_, prev_handle)) = self.active_timers.remove(&uri) {
            prev_handle.abort();
            log::trace!(
                target: LOG_TARGET,
                "Cancelled previous debounce timer for {}",
                uri
            );
        }

        let data = DebouncedDiagnosticData {
            uri: uri.clone(),
            lsp_uri,
            client,
            snapshot_data,
            bridge_pool,
            synthetic_diagnostics,
        };

        let duration = self.debounce_duration;

        // Spawn the debounce timer task
        let task = tokio::spawn(async move {
            // Wait for debounce duration
            tokio::time::sleep(duration).await;

            log::debug!(
                target: LOG_TARGET,
                "Debounce timer expired for {}, triggering diagnostic",
                data.uri
            );

            // Execute diagnostic collection and publishing
            execute_debounced_diagnostic(data).await;
        });

        // Register the timer
        self.active_timers.insert(uri, task.abort_handle());
    }

    /// Cancel the debounce timer for a document.
    ///
    /// Called when a document is closed - no need for debounced diagnostics.
    pub(crate) fn cancel(&self, uri: &Url) {
        if let Some((_, handle)) = self.active_timers.remove(uri) {
            handle.abort();
            log::trace!(
                target: LOG_TARGET,
                "Cancelled debounce timer for closed document {}",
                uri
            );
        }
    }

    /// Cancel all active debounce timers.
    ///
    /// Called during server shutdown.
    pub(crate) fn cancel_all(&self) {
        for entry in self.active_timers.iter() {
            entry.value().abort();
        }
        self.active_timers.clear();
        log::debug!(
            target: LOG_TARGET,
            "Cancelled all debounce timers"
        );
    }

    /// Check if there's an active timer for a document.
    ///
    /// Useful for testing.
    #[cfg(test)]
    pub(crate) fn has_active_timer(&self, uri: &Url) -> bool {
        self.active_timers.contains_key(uri)
    }

    /// Get the number of active timers.
    ///
    /// Useful for testing.
    #[cfg(test)]
    pub(crate) fn active_timer_count(&self) -> usize {
        self.active_timers.len()
    }
}

/// Execute diagnostic collection and publishing after debounce timer expires.
///
/// This is the core logic that runs when a debounce timer fires.
/// It mirrors `spawn_synthetic_diagnostic_task` but with pre-captured data.
async fn execute_debounced_diagnostic(data: DebouncedDiagnosticData) {
    let DebouncedDiagnosticData {
        uri,
        lsp_uri,
        client,
        snapshot_data,
        bridge_pool,
        synthetic_diagnostics,
    } = data;

    // Spawn the actual diagnostic task (similar to spawn_synthetic_diagnostic_task)
    // This task is registered with SyntheticDiagnosticsManager for superseding
    let uri_clone = uri.clone();
    let task = tokio::spawn(async move {
        let diagnostics =
            collect_push_diagnostics(snapshot_data, &bridge_pool, &uri_clone, LOG_TARGET).await;

        let Some(diagnostics) = diagnostics else {
            log::debug!(
                target: LOG_TARGET,
                "No diagnostics to collect for {} (no snapshot data)",
                uri_clone
            );
            return;
        };

        log::debug!(
            target: LOG_TARGET,
            "Collected {} diagnostics for {} (debounced)",
            diagnostics.len(),
            uri_clone
        );

        // Publish diagnostics
        client.publish_diagnostics(lsp_uri, diagnostics, None).await;
    });

    // Register with SyntheticDiagnosticsManager for superseding
    // If a didSave or another debounced didChange triggers while this is running,
    // the new task will supersede this one
    synthetic_diagnostics.register_task(uri, task.abort_handle());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_initial_state_and_cancel_noop() {
        let manager = DebouncedDiagnosticsManager::with_duration(Duration::from_millis(50));
        let uri = Url::parse("file:///test.md").unwrap();

        // Verify initial state: no active timers
        assert!(!manager.has_active_timer(&uri));

        // Cancel on non-existent timer should be a no-op (no panic)
        manager.cancel(&uri);
        assert!(!manager.has_active_timer(&uri));
    }

    #[tokio::test]
    async fn test_cancel_stops_timer() {
        let manager = DebouncedDiagnosticsManager::with_duration(Duration::from_millis(100));
        let uri = Url::parse("file:///test.md").unwrap();

        // Manually insert a timer for testing cancel behavior
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        manager
            .active_timers
            .insert(uri.clone(), task.abort_handle());

        assert!(manager.has_active_timer(&uri));

        manager.cancel(&uri);
        assert!(!manager.has_active_timer(&uri));
    }

    #[tokio::test]
    async fn test_cancel_all() {
        let manager = DebouncedDiagnosticsManager::with_duration(Duration::from_millis(100));
        let uri1 = Url::parse("file:///test1.md").unwrap();
        let uri2 = Url::parse("file:///test2.md").unwrap();

        // Manually insert timers for testing
        let task1 = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let task2 = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let handle1 = task1.abort_handle();
        let handle2 = task2.abort_handle();

        manager.active_timers.insert(uri1.clone(), handle1.clone());
        manager.active_timers.insert(uri2.clone(), handle2.clone());

        assert_eq!(manager.active_timer_count(), 2);

        manager.cancel_all();

        assert_eq!(manager.active_timer_count(), 0);

        // Yield to let aborts propagate
        tokio::task::yield_now().await;
        assert!(handle1.is_finished());
        assert!(handle2.is_finished());
    }

    #[tokio::test]
    async fn test_schedule_replaces_previous_timer() {
        let manager = DebouncedDiagnosticsManager::with_duration(Duration::from_millis(100));
        let uri = Url::parse("file:///test.md").unwrap();

        // Manually insert a timer
        let task1 = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let handle1 = task1.abort_handle();
        manager.active_timers.insert(uri.clone(), handle1.clone());

        // Insert another timer for the same URI (simulating what schedule does)
        let task2 = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let handle2 = task2.abort_handle();

        // Remove and abort previous
        if let Some((_, prev_handle)) = manager.active_timers.remove(&uri) {
            prev_handle.abort();
        }
        manager.active_timers.insert(uri.clone(), handle2.clone());

        // Yield to let abort propagate
        tokio::task::yield_now().await;

        assert!(handle1.is_finished(), "first timer should be aborted");
        assert!(
            !handle2.is_finished(),
            "second timer should still be running"
        );

        // Cleanup
        manager.cancel_all();
    }
}
