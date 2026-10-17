//! Debounced synthetic-diagnostic triggers for `didChange` (pull-first-diagnostic-forwarding Phase 3).
//!
//! Each document has its own timer that resets on every change; firing after the
//! debounce delay (default 500ms) collects and publishes diagnostics.
//!
//! Snapshot data is captured at *schedule* time, not when the timer fires, so the
//! published diagnostics match the document state that triggered them and the
//! timer callback needs no `self` reference. Newer schedules supersede older
//! in-flight publishes via `SyntheticDiagnosticsManager`'s `AbortHandle`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::task::AbortHandle;
use url::Url;

use super::bridge::{BridgeCoordinator, LanguageServerPool};

use super::lsp_impl::DiagnosticPublisher;
use super::lsp_impl::text_document::publish_diagnostic::{
    DiagnosticSnapshot, PullLayerOutcome, collect_push_diagnostics,
};
use super::synthetic_diagnostics::{SyntheticDiagnosticTrigger, SyntheticDiagnosticsManager};

/// Default debounce duration for `didChange` events.
///
/// This value balances responsiveness with avoiding excessive diagnostic
/// requests during rapid typing. It matches common IDE debounce patterns and is
/// the fallback when `diagnostics_debounce_ms` is unset — shares the single source
/// of truth with the config default ([`crate::config::settings::DEFAULT_DEBOUNCE_MS`]).
pub(crate) const DEFAULT_DEBOUNCE_DURATION: Duration =
    Duration::from_millis(crate::config::settings::DEFAULT_DEBOUNCE_MS);

/// Logging target for debounced diagnostics.
const LOG_TARGET: &str = "kakehashi::debounced_diag";

/// Information captured at debounce schedule time for later execution.
///
/// This contains everything needed to execute the diagnostic collection
/// and publishing when the debounce timer fires.
struct DebouncedDiagnosticData {
    /// The document URI (url::Url for internal use)
    uri: Url,
    /// Pre-captured per-region contexts (None if document has no injections)
    snapshot_data: Option<DiagnosticSnapshot>,
    /// Bridge pool for sending diagnostic requests
    bridge_pool: Arc<LanguageServerPool>,
    /// Bridge coordinator — used to eager re-sync the host document to `_self`
    /// host servers at the debounced cadence (#431).
    bridge: Arc<BridgeCoordinator>,
    /// Document store — lets the eager re-sync read the host document's *current*
    /// text at sync time (under the bridge's `host_documents` lock) so a late
    /// re-sync sends the latest text rather than this fire's snapshot (#422).
    documents: Arc<crate::document::DocumentStore>,
    /// Open lifetime captured when the timer was scheduled. A close/reopen at
    /// the same URI must not let this prior lifetime register new work.
    incarnation: u64,
    content_version: u64,
    settings_generation: u64,
    /// What the collection supersedes among this version's others.
    trigger: SyntheticDiagnosticTrigger,
    /// Takes the snapshot again when the timer fires, in place of
    /// `snapshot_data`, for the version it was scheduled for.
    resnapshot: Option<Resnapshot>,
    /// Reference to synthetic diagnostics manager for task registration
    synthetic_diagnostics: Arc<SyntheticDiagnosticsManager>,
    /// The single proactive publisher: feeds the pull result into the cache and
    /// republishes the merged set (push-propagation-diagnostic-forwarding).
    publisher: Arc<DiagnosticPublisher>,
    #[cfg(test)]
    execution_gate: Option<Arc<DebounceExecutionGate>>,
}

/// The document's diagnostic snapshot as of when a timer fires, given the
/// `(incarnation, content_version)` it was scheduled for: `None` once the
/// document moved past it.
pub(crate) type Resnapshot = Box<dyn FnOnce(u64, u64) -> Option<DiagnosticSnapshot> + Send>;

#[cfg(test)]
pub(crate) struct DebounceExecutionGate {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
    /// Whether the collection that ran had a snapshot.
    pub(crate) collected_with_snapshot: std::sync::Mutex<Option<bool>>,
}

/// Manager for debounced diagnostic triggers.
///
/// Tracks per-document debounce timers, firing the scheduled diagnostic
/// collection on expiry. Uses `DashMap` for concurrent access from multiple
/// tokio tasks via sharded locks (no single global lock).
pub(crate) struct DebouncedDiagnosticsManager {
    /// Active debounce timers per document.
    /// The AbortHandle allows cancelling the timer when a new change arrives.
    active_timers: DashMap<Url, Timer>,

    /// Milliseconds to wait after the last change before triggering diagnostics.
    /// Atomic so a `diagnostics_debounce_ms` config change applies to subsequent
    /// schedules without rebuilding the manager (it lives behind an `Arc`). Read at
    /// schedule time; in-flight timers keep the value they were spawned with.
    debounce_millis: AtomicU64,
    #[cfg(test)]
    execution_gate: std::sync::Mutex<Option<Arc<DebounceExecutionGate>>>,
}

/// A document's pending debounce timer, and the collection it was
/// scheduled as.
struct Timer {
    handle: AbortHandle,
    /// `(incarnation, content_version, settings_generation)`.
    lineage: (u64, u64, u64),
    trigger: SyntheticDiagnosticTrigger,
}

impl Timer {
    /// Whether a schedule of `lineage` as `trigger` leaves this timer be: it
    /// ranks above it for the same text and settings, as one scheduled after
    /// held regions were sent ranks above an edit's whose snapshot was
    /// captured before (and so left those regions out).
    fn outranks(&self, lineage: (u64, u64, u64), trigger: SyntheticDiagnosticTrigger) -> bool {
        !self.handle.is_finished()
            && self.lineage == lineage
            && self.trigger.rank() > trigger.rank()
    }
}

#[cfg(test)]
impl From<AbortHandle> for Timer {
    fn from(handle: AbortHandle) -> Self {
        Self {
            handle,
            lineage: (0, 0, 0),
            trigger: SyntheticDiagnosticTrigger::Change,
        }
    }
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
            debounce_millis: AtomicU64::new(debounce_duration.as_millis() as u64),
            #[cfg(test)]
            execution_gate: std::sync::Mutex::new(None),
        }
    }

    /// Update the debounce delay (milliseconds) from the resolved
    /// `diagnostics_debounce_ms` setting. Applies to subsequent schedules.
    pub(crate) fn set_debounce_millis(&self, millis: u64) {
        self.debounce_millis.store(millis, Ordering::Relaxed);
    }

    /// The debounce delay (milliseconds) the next schedule will use.
    #[cfg(test)]
    pub(crate) fn debounce_millis(&self) -> u64 {
        self.debounce_millis.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn install_execution_gate(&self) -> Arc<DebounceExecutionGate> {
        let gate = Arc::new(DebounceExecutionGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            collected_with_snapshot: std::sync::Mutex::new(None),
        });
        *self.execution_gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    /// Schedule a debounced diagnostic for a document.
    ///
    /// Any existing timer for this document is cancelled and replaced, unless
    /// it outranks this one for the same version; on expiry the new timer
    /// publishes diagnostics from the pre-captured `snapshot_data` (or, with
    /// `resnapshot`, from the snapshot it takes then), ranked as `trigger`
    /// among the version's collections.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn schedule(
        &self,
        uri: Url,
        snapshot_data: Option<DiagnosticSnapshot>,
        bridge_pool: Arc<LanguageServerPool>,
        bridge: Arc<BridgeCoordinator>,
        synthetic_diagnostics: Arc<SyntheticDiagnosticsManager>,
        publisher: Arc<DiagnosticPublisher>,
        documents: Arc<crate::document::DocumentStore>,
        trigger: SyntheticDiagnosticTrigger,
        resnapshot: Option<Resnapshot>,
    ) {
        // Opportunistic cleanup: remove finished timers to prevent unbounded growth.
        // This is O(n) but runs infrequently and keeps the map from accumulating
        // stale entries for documents that completed their debounce cycle.
        const CLEANUP_THRESHOLD: usize = 32;
        if self.active_timers.len() > CLEANUP_THRESHOLD {
            self.active_timers
                .retain(|_, timer| !timer.handle.is_finished());
        }

        let Some((incarnation, content_version, settings_generation)) = snapshot_data
            .as_ref()
            .map(|snapshot| {
                (
                    snapshot.lineage.incarnation,
                    snapshot.lineage.content_version,
                    snapshot.lineage.settings_generation,
                )
            })
            .or_else(|| {
                documents
                    .get(&uri)
                    .map(|doc| (doc.incarnation(), doc.content_version(), 0))
            })
        else {
            if let Some((_, previous)) = self.active_timers.remove(&uri) {
                previous.handle.abort();
            }
            return;
        };
        let lineage = (incarnation, content_version, settings_generation);
        // The check and the replacement under the entry's lock: a schedule
        // racing this one either sees the timer this one leaves or replaces it.
        let entry = self.active_timers.entry(uri.clone());
        if let dashmap::mapref::entry::Entry::Occupied(previous) = &entry {
            if previous.get().outranks(lineage, trigger) {
                log::trace!(
                    target: LOG_TARGET,
                    "Kept the outranking debounce timer for {}",
                    uri
                );
                return;
            }
            previous.get().handle.abort();
            log::trace!(
                target: LOG_TARGET,
                "Cancelled previous debounce timer for {}",
                uri
            );
        }

        let data = DebouncedDiagnosticData {
            uri: uri.clone(),
            snapshot_data,
            bridge_pool,
            bridge,
            synthetic_diagnostics,
            publisher,
            documents,
            incarnation,
            content_version,
            settings_generation,
            trigger,
            resnapshot,
            #[cfg(test)]
            execution_gate: self.execution_gate.lock().unwrap().clone(),
        };

        let duration = Duration::from_millis(self.debounce_millis.load(Ordering::Relaxed));

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
        entry.insert(Timer {
            handle: task.abort_handle(),
            lineage,
            trigger,
        });
    }

    /// Cancel the debounce timer for a document.
    ///
    /// Called when a document is closed - no need for debounced diagnostics.
    pub(crate) fn cancel(&self, uri: &Url) {
        if let Some((_, timer)) = self.active_timers.remove(uri) {
            timer.handle.abort();
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
            entry.value().handle.abort();
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

    #[cfg(test)]
    pub(crate) fn timer_abort_handle(&self, uri: &Url) -> Option<AbortHandle> {
        self.active_timers
            .get(uri)
            .map(|entry| entry.value().handle.clone())
    }
}

/// Execute diagnostic collection and publishing after debounce timer expires.
///
/// This is the core logic that runs when a debounce timer fires.
/// It mirrors `DiagnosticScheduler::spawn_synthetic_diagnostic_task()` but with
/// pre-captured data.
async fn execute_debounced_diagnostic(data: DebouncedDiagnosticData) {
    let DebouncedDiagnosticData {
        uri,
        mut snapshot_data,
        bridge_pool,
        bridge,
        synthetic_diagnostics,
        publisher,
        documents,
        incarnation,
        content_version,
        settings_generation,
        trigger,
        resnapshot,
        #[cfg(test)]
        execution_gate,
    } = data;

    #[cfg(test)]
    if let Some(gate) = &execution_gate {
        gate.entered.notify_one();
        gate.release.notified().await;
    }

    // Serialize timer side effects with didClose's document removal. If this
    // body wins, it registers eager/synthetic work before close performs its
    // cancellation cleanup; if close wins, liveness/incarnation rejects this
    // prior timer. The new await is also a cancellation checkpoint after sleep.
    let edit_lock = documents.edit_lock(&uri);
    let edit_guard = edit_lock.lock().await;
    let current_lineage = documents
        .get(&uri)
        .map(|doc| (doc.incarnation(), doc.content_version()));
    if current_lineage != Some((incarnation, content_version)) {
        if current_lineage.is_none() {
            documents.remove_edit_lock_if_unshared(&uri, &edit_lock);
        }
        drop(edit_guard);
        return;
    }
    // At the version it was scheduled for, under the edit lock: what the
    // document's regions are now, not when the timer was scheduled. A region
    // held then may have been sent since, and collected at once; a snapshot
    // still leaving it out would replace that collection.
    if let Some(resnapshot) = resnapshot {
        snapshot_data = resnapshot(incarnation, content_version);
    }
    #[cfg(test)]
    if let Some(gate) = &execution_gate {
        *gate.collected_with_snapshot.lock().unwrap() = Some(snapshot_data.is_some());
    }

    // #431: re-sync the host document to its `_self` host servers at this debounced
    // cadence (after the user stops typing), so a push-only host server — which the
    // capability-gated pull below skips — re-analyzes the current text and pushes
    // fresh diagnostics instead of stale ones. Fire-and-forget (spawns per-server
    // tasks and returns) so it never head-of-line-blocks the pull on a slow
    // server's readiness; pull-capable servers fingerprint-dedup the extra sync.
    if let Some(host) = snapshot_data.as_ref().and_then(|s| s.host.as_ref()) {
        // Sync target comes from the host context itself (`host.uri`), not the
        // outer scheduled `uri` — they're equal today, but keying off the host
        // context keeps the sync correct if a scheduled URI ever differs from its
        // host document.
        //
        // The live reader lets the sync read the document's *current* text under
        // the bridge's `host_documents` lock, so when several debounce fires race
        // (rapid edits with pauses), the task that syncs last sends the latest
        // text — not this fire's `host.text` snapshot, which a newer edit may have
        // superseded. That closes the eager-sync stale-overwrite that left a
        // push-only host server analyzing rolled-back text (#422). `host.text`
        // stays the fallback if the document is closed mid-sync.
        let host_uri = host.uri.clone();
        let documents = Arc::clone(&documents);
        // Lock-ordering: this runs inside `sync_host_document`, i.e. under the
        // bridge `host_documents` lock — so the `DocumentStore` shard read below is
        // taken in the order `host_documents -> DocumentStore`. That is deadlock-free
        // because the *inverse* order never occurs: every host-request path
        // (`resolve_host_bridge_context`, formatting, the diagnostic pull, `did_change`)
        // resolves an OWNED snapshot/text and drops its `DocumentStore` ref BEFORE
        // calling any bridge method that acquires `host_documents`. The `Ref` taken
        // here is dropped within the closure (`text_arc()` cheaply clones the
        // document's `Arc<str>` — a refcount bump, no copy — #498), so it never
        // crosses an `.await`.
        let expected_incarnation = host.incarnation;
        let live_text_reader: crate::lsp::bridge::HostTextReader = Arc::new(move || {
            documents
                .get(&host_uri)
                .filter(|doc| doc.incarnation() == expected_incarnation)
                .map(|doc| (doc.text_arc(), doc.content_version()))
        });
        bridge.eager_sync_host_document_on_servers(
            &host.uri,
            &host.language_id,
            host.text.clone(),
            crate::lsp::bridge::HostRevision {
                incarnation: host.incarnation,
                content_version: host.content_version,
            },
            host.configs.clone(),
            Some(live_text_reader),
        );
    }

    // Spawn the actual diagnostic task (similar to DiagnosticScheduler::spawn_synthetic_diagnostic_task)
    // This task is registered with SyntheticDiagnosticsManager for superseding
    let uri_clone = uri.clone();
    let publish_documents = Arc::clone(&documents);
    let future = async move {
        let diagnostics =
            collect_push_diagnostics(snapshot_data, &bridge_pool, &uri_clone, LOG_TARGET).await;

        // Collection can outlive the timer body's entry gate. Serialize the
        // final liveness check + publication with didClose's
        // `clear_document_state_on_close` edit-lock section: publish-first is
        // cleared by the following close, while close-first observes no matching
        // lifetime and returns. didClose also aborts this registered task.
        let publish_edit_lock = publish_documents.edit_lock(&uri_clone);
        let publish_edit_guard = publish_edit_lock.lock().await;
        let current_lineage = publish_documents
            .get(&uri_clone)
            .map(|doc| (doc.incarnation(), doc.content_version()));
        if current_lineage != Some((incarnation, content_version)) {
            if current_lineage.is_none() {
                publish_documents.remove_edit_lock_if_unshared(&uri_clone, &publish_edit_lock);
            }
            drop(publish_edit_guard);
            return;
        }

        // Feed the host-event pull outcome into the cache and republish the merged
        // set (push-propagation-diagnostic-forwarding) — push slots survive.
        match diagnostics {
            PullLayerOutcome::Skip => {
                log::debug!(
                    target: LOG_TARGET,
                    "No diagnostics to collect for {} (no snapshot data)",
                    uri_clone
                );
            }
            PullLayerOutcome::Clear => publisher.clear_pull_layer(&uri_clone).await,
            PullLayerOutcome::Publish(diagnostics) => {
                log::debug!(
                    target: LOG_TARGET,
                    "Collected {} pull-layer diagnostics for {} (debounced)",
                    diagnostics.len(),
                    uri_clone
                );
                publisher.publish_pull_layer(&uri_clone, diagnostics).await;
            }
        }
        drop(publish_edit_guard);
    };

    // Register with SyntheticDiagnosticsManager for superseding
    // If a didSave or another debounced didChange triggers while this is running,
    // the new task will supersede this one
    synthetic_diagnostics.spawn_task_for_lineage(
        uri,
        incarnation,
        content_version,
        settings_generation,
        trigger,
        future,
    );
    drop(edit_guard);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_millis_defaults_and_reflects_config_updates() {
        let manager = DebouncedDiagnosticsManager::new();
        assert_eq!(
            manager.debounce_millis(),
            crate::config::settings::DEFAULT_DEBOUNCE_MS,
            "a fresh manager uses the runtime default"
        );

        // A config reload pushes a new value via set_debounce_millis; the next
        // schedule reads it (production wiring in `schedule_debounced_diagnostic`).
        manager.set_debounce_millis(120);
        assert_eq!(manager.debounce_millis(), 120);

        manager.set_debounce_millis(0);
        assert_eq!(manager.debounce_millis(), 0, "explicit zero is honored");
    }

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
            .insert(uri.clone(), task.abort_handle().into());

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

        manager
            .active_timers
            .insert(uri1.clone(), handle1.clone().into());
        manager
            .active_timers
            .insert(uri2.clone(), handle2.clone().into());

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
        manager
            .active_timers
            .insert(uri.clone(), handle1.clone().into());

        // Insert another timer for the same URI (simulating what schedule does)
        let task2 = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let handle2 = task2.abort_handle();

        // Remove and abort previous
        if let Some((_, prev_handle)) = manager.active_timers.remove(&uri) {
            prev_handle.handle.abort();
        }
        manager
            .active_timers
            .insert(uri.clone(), handle2.clone().into());

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
