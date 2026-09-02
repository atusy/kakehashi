//! Background diagnostic collection on `didSave`/`didOpen` (pull-first-diagnostic-forwarding Phase 2):
//! pull internally, push via `textDocument/publishDiagnostics`.
//!
//! Rapid-fire events supersede each other — `SyntheticDiagnosticsManager`
//! aborts the prior in-flight task via `AbortHandle` so only the latest
//! collection publishes, and uses `DashMap` for concurrent access via sharded
//! locks (no single global lock).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::{DashMap, mapref::entry::Entry};
use tokio::task::AbortHandle;
use url::Url;

/// Tracks active synthetic diagnostic tasks per document.
///
/// When a new task is spawned for a document, any existing task for that
/// document is aborted (superseded). This ensures only the latest diagnostic
/// collection publishes results.
#[derive(Default)]
pub(crate) struct SyntheticDiagnosticsManager {
    /// Map from document URI to its latest ordering key and optional active
    /// handle. A completed task leaves a key tombstone until didClose.
    active_tasks: DashMap<Url, ActiveTask>,
    registration_lock: std::sync::Mutex<()>,
    shutdown: AtomicBool,
}

struct ActiveTask {
    key: SyntheticDiagnosticTaskKey,
    abort_handle: Option<AbortHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SyntheticDiagnosticTrigger {
    Open,
    Change,
    Save,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SyntheticDiagnosticTaskKey {
    incarnation: u64,
    content_version: u64,
    settings_generation: u64,
    trigger: SyntheticDiagnosticTrigger,
}

impl ActiveTask {
    fn is_finished(&self) -> bool {
        self.abort_handle
            .as_ref()
            .is_some_and(AbortHandle::is_finished)
    }

    fn abort(&self) {
        if let Some(handle) = &self.abort_handle {
            handle.abort();
        }
    }
}

impl SyntheticDiagnosticsManager {
    /// Create a new manager.
    pub(crate) fn new() -> Self {
        Self {
            active_tasks: DashMap::new(),
            registration_lock: std::sync::Mutex::new(()),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Register a new diagnostic task for a document, superseding (and aborting)
    /// any existing task and returning its `AbortHandle`.
    ///
    /// Also opportunistically cleans up finished tasks to prevent memory buildup.
    #[cfg(test)]
    pub(crate) fn register_task(&self, uri: Url, abort_handle: AbortHandle) -> bool {
        self.register_task_for_lineage(uri, 0, 0, 0, SyntheticDiagnosticTrigger::Open, abort_handle)
    }

    /// Register work for one document version and trigger. Lexicographic
    /// lineage ordering prevents late old-lifetime/version work from replacing
    /// newer work; at the same version and settings generation, save outranks
    /// change, which outranks open.
    pub(crate) fn register_task_for_lineage(
        &self,
        uri: Url,
        incarnation: u64,
        content_version: u64,
        settings_generation: u64,
        trigger: SyntheticDiagnosticTrigger,
        abort_handle: AbortHandle,
    ) -> bool {
        let _registration = self.registration_lock.lock().unwrap();
        if self.shutdown.load(Ordering::Acquire) {
            abort_handle.abort();
            return false;
        }
        // Opportunistic cleanup: remove entries for tasks that have completed.
        // This prevents memory buildup from documents that were saved but not re-saved.
        // We limit to a small number to avoid blocking the registration.
        self.cleanup_finished_tasks(5);

        let key = SyntheticDiagnosticTaskKey {
            incarnation,
            content_version,
            settings_generation,
            trigger,
        };
        match self.active_tasks.entry(uri) {
            Entry::Vacant(entry) => {
                entry.insert(ActiveTask {
                    key,
                    abort_handle: Some(abort_handle),
                });
                true
            }
            Entry::Occupied(mut entry) => {
                if entry.get().key > key {
                    // Newer document/trigger work already owns the URI. Discard
                    // the late older task instead of aborting its successor.
                    abort_handle.abort();
                    return false;
                }
                let previous = entry.insert(ActiveTask {
                    key,
                    abort_handle: Some(abort_handle),
                });
                previous.abort();
                log::debug!(
                    target: "kakehashi::synthetic_diag",
                    "Superseded previous diagnostic task"
                );
                true
            }
        }
    }

    /// Spawn work behind a start gate, then release it only if registration
    /// wins. Rejected and post-shutdown tasks never execute their future.
    pub(crate) fn spawn_task_for_lineage<F>(
        &self,
        uri: Url,
        incarnation: u64,
        content_version: u64,
        settings_generation: u64,
        trigger: SyntheticDiagnosticTrigger,
        future: F,
    ) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if start_rx.await.is_ok() {
                future.await;
            }
        });
        let accepted = self.register_task_for_lineage(
            uri,
            incarnation,
            content_version,
            settings_generation,
            trigger,
            task.abort_handle(),
        );
        if accepted {
            let _ = start_tx.send(());
        }
        accepted
    }

    /// Drop handles for tasks that have finished while preserving each URI's
    /// latest ordering key as a high-watermark until didClose.
    ///
    /// Called opportunistically during registration to prevent memory buildup.
    /// Limited to avoid O(n) scan on every registration.
    fn cleanup_finished_tasks(&self, limit: usize) {
        let to_remove = self.finished_task_uris(limit);
        let cleaned = self.remove_if_still_finished(to_remove);

        if cleaned > 0 {
            log::trace!(
                target: "kakehashi::synthetic_diag",
                "Released {} finished diagnostic task handles",
                cleaned
            );
        }
    }

    fn finished_task_uris(&self, limit: usize) -> Vec<Url> {
        // Collect keys to clean without holding multiple references during iteration.
        let mut uris = Vec::with_capacity(limit);
        for entry in self.active_tasks.iter() {
            if entry.value().is_finished() {
                uris.push(entry.key().clone());
                if uris.len() >= limit {
                    break;
                }
            }
        }
        uris
    }

    fn remove_if_still_finished(&self, uris: Vec<Url>) -> usize {
        let mut cleaned = 0;
        for uri in uris {
            // Recheck under the entry guard so a newly registered task cannot
            // lose its handle after the scan observed an older finished one.
            if let Some(mut task) = self.active_tasks.get_mut(&uri)
                && task.is_finished()
            {
                task.abort_handle = None;
                cleaned += 1;
            }
        }
        cleaned
    }

    /// Check if there's an active task for a document.
    ///
    /// Useful for debugging and tests.
    #[cfg(test)]
    pub(crate) fn has_active_task(&self, uri: &Url) -> bool {
        self.active_tasks
            .get(uri)
            .is_some_and(|task| task.abort_handle.is_some())
    }

    /// Abort all active tasks and clear the map.
    ///
    /// Called during server shutdown to clean up.
    pub(crate) fn abort_all(&self) {
        let _registration = self.registration_lock.lock().unwrap();
        self.shutdown.store(true, Ordering::Release);
        for entry in self.active_tasks.iter() {
            entry.value().abort();
        }
        self.active_tasks.clear();
    }

    /// Remove the entry for a document and abort any active task.
    ///
    /// Called when a document is closed. The task is aborted since publishing
    /// diagnostics for a closed document would be wasteful.
    pub(crate) fn remove_document(&self, uri: &Url) {
        if let Some((_, task)) = self.active_tasks.remove(uri) {
            task.abort();
            log::debug!(
                target: "kakehashi::synthetic_diag",
                "Aborted diagnostic task for closed document"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_task(abort_handle: AbortHandle) -> ActiveTask {
        ActiveTask {
            key: SyntheticDiagnosticTaskKey {
                incarnation: 0,
                content_version: 0,
                settings_generation: 0,
                trigger: SyntheticDiagnosticTrigger::Open,
            },
            abort_handle: Some(abort_handle),
        }
    }

    #[tokio::test]
    async fn test_register_supersedes_previous() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Spawn a task that just sleeps (simulating slow diagnostic collection)
        let task1 = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            42
        });
        let handle1 = task1.abort_handle();

        // Register task 1
        assert!(manager.register_task(uri.clone(), handle1.clone()));
        assert!(manager.has_active_task(&uri));

        // Spawn and register task 2
        let task2 = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            43
        });
        let handle2 = task2.abort_handle();

        assert!(manager.register_task(uri.clone(), handle2));

        // Task 1 should be aborted - yield to let the abort propagate
        tokio::task::yield_now().await;
        assert!(handle1.is_finished());

        // Wait for task 2 to complete
        let result = task2.await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 43);
    }

    #[tokio::test]
    async fn late_old_incarnation_cannot_abort_reopened_document_task() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///reopened.md").unwrap();
        let reopened_task = tokio::spawn(std::future::pending::<()>());
        let reopened_handle = reopened_task.abort_handle();
        manager.register_task_for_lineage(
            uri.clone(),
            2,
            0,
            0,
            SyntheticDiagnosticTrigger::Open,
            reopened_handle.clone(),
        );

        let late_old_task = tokio::spawn(std::future::pending::<()>());
        let late_old_handle = late_old_task.abort_handle();
        manager.register_task_for_lineage(
            uri,
            1,
            0,
            0,
            SyntheticDiagnosticTrigger::Save,
            late_old_handle.clone(),
        );

        tokio::task::yield_now().await;
        assert!(
            late_old_handle.is_finished(),
            "late work for the closed lifetime must discard itself"
        );
        assert!(
            !reopened_handle.is_finished(),
            "late old-lifetime work must not supersede the reopened document"
        );
        reopened_handle.abort();
    }

    #[tokio::test]
    async fn late_older_version_cannot_abort_saved_task_in_the_same_incarnation() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///saved.md").unwrap();
        let saved_task = tokio::spawn(std::future::pending::<()>());
        let saved_handle = saved_task.abort_handle();
        manager.register_task_for_lineage(
            uri.clone(),
            1,
            2,
            0,
            SyntheticDiagnosticTrigger::Save,
            saved_handle.clone(),
        );

        let late_open_task = tokio::spawn(std::future::pending::<()>());
        let late_open_handle = late_open_task.abort_handle();
        manager.register_task_for_lineage(
            uri,
            1,
            1,
            0,
            SyntheticDiagnosticTrigger::Open,
            late_open_handle.clone(),
        );

        tokio::task::yield_now().await;
        assert!(late_open_handle.is_finished());
        assert!(!saved_handle.is_finished());
        saved_handle.abort();
    }

    #[tokio::test]
    async fn late_same_version_change_cannot_abort_saved_task() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///saved-same-version.md").unwrap();
        let saved_task = tokio::spawn(std::future::pending::<()>());
        let saved_handle = saved_task.abort_handle();
        manager.register_task_for_lineage(
            uri.clone(),
            1,
            2,
            0,
            SyntheticDiagnosticTrigger::Save,
            saved_handle.clone(),
        );

        let late_change_task = tokio::spawn(std::future::pending::<()>());
        let late_change_handle = late_change_task.abort_handle();
        manager.register_task_for_lineage(
            uri,
            1,
            2,
            0,
            SyntheticDiagnosticTrigger::Change,
            late_change_handle.clone(),
        );

        tokio::task::yield_now().await;
        assert!(
            late_change_handle.is_finished(),
            "same-version debounce work must yield to the save trigger"
        );
        assert!(
            !saved_handle.is_finished(),
            "same-version debounce work must not abort the save trigger"
        );
        saved_handle.abort();
    }

    #[tokio::test]
    async fn completed_save_key_still_rejects_same_version_change() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///completed-save.md").unwrap();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        assert!(manager.spawn_task_for_lineage(
            uri.clone(),
            1,
            2,
            0,
            SyntheticDiagnosticTrigger::Save,
            async move {
                let _ = finished_tx.send(());
            },
        ));
        finished_rx.await.unwrap();
        tokio::task::yield_now().await;
        manager.cleanup_finished_tasks(5);

        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let task_ran = std::sync::Arc::clone(&ran);
        assert!(!manager.spawn_task_for_lineage(
            uri,
            1,
            2,
            0,
            SyntheticDiagnosticTrigger::Change,
            async move {
                task_ran.store(true, Ordering::Release);
            },
        ));
        tokio::task::yield_now().await;
        assert!(!ran.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn newer_settings_generation_supersedes_completed_save() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///reconfigured-after-save.md").unwrap();
        let (saved_tx, saved_rx) = tokio::sync::oneshot::channel();
        assert!(manager.spawn_task_for_lineage(
            uri.clone(),
            1,
            2,
            1,
            SyntheticDiagnosticTrigger::Save,
            async move {
                let _ = saved_tx.send(());
            },
        ));
        saved_rx.await.unwrap();
        tokio::task::yield_now().await;
        manager.cleanup_finished_tasks(5);

        let (change_tx, change_rx) = tokio::sync::oneshot::channel();
        assert!(manager.spawn_task_for_lineage(
            uri,
            1,
            2,
            2,
            SyntheticDiagnosticTrigger::Change,
            async move {
                let _ = change_tx.send(());
            },
        ));
        change_rx.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_lower_priority_task_never_starts() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///start-gated.md").unwrap();
        let saved_task = tokio::spawn(std::future::pending::<()>());
        let saved_handle = saved_task.abort_handle();
        manager.register_task_for_lineage(
            uri.clone(),
            1,
            1,
            0,
            SyntheticDiagnosticTrigger::Save,
            saved_handle.clone(),
        );

        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let task_ran = std::sync::Arc::clone(&ran);
        assert!(!manager.spawn_task_for_lineage(
            uri,
            1,
            1,
            0,
            SyntheticDiagnosticTrigger::Open,
            async move {
                task_ran.store(true, Ordering::Release);
            },
        ));
        tokio::task::yield_now().await;
        assert!(!ran.load(Ordering::Acquire));
        saved_handle.abort();
    }

    #[tokio::test]
    async fn shutdown_rejects_tasks_before_they_start() {
        let manager = SyntheticDiagnosticsManager::new();
        manager.abort_all();
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let task_ran = std::sync::Arc::clone(&ran);
        assert!(!manager.spawn_task_for_lineage(
            Url::parse("file:///after-shutdown.md").unwrap(),
            1,
            1,
            0,
            SyntheticDiagnosticTrigger::Save,
            async move {
                task_ran.store(true, Ordering::Release);
            },
        ));
        tokio::task::yield_now().await;
        assert!(!ran.load(Ordering::Acquire));
        assert!(manager.active_tasks.is_empty());
    }

    #[tokio::test]
    async fn test_remove_document_aborts_task() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///test.md").unwrap();

        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let handle = task.abort_handle();

        manager.register_task(uri.clone(), handle.clone());
        assert!(manager.has_active_task(&uri));

        manager.remove_document(&uri);
        assert!(!manager.has_active_task(&uri));
        // Yield to let the abort propagate
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }

    #[tokio::test]
    async fn test_abort_all() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri1 = Url::parse("file:///test1.md").unwrap();
        let uri2 = Url::parse("file:///test2.md").unwrap();

        let task1 = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let task2 = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        let handle1 = task1.abort_handle();
        let handle2 = task2.abort_handle();

        manager.register_task(uri1, handle1.clone());
        manager.register_task(uri2, handle2.clone());

        manager.abort_all();

        // Yield to let the aborts propagate
        tokio::task::yield_now().await;
        assert!(handle1.is_finished());
        assert!(handle2.is_finished());
    }

    #[tokio::test]
    async fn cleanup_finished_tasks_releases_handles_but_keeps_keys() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///finished.md").unwrap();

        let task = tokio::spawn(async {});
        let handle = task.abort_handle();
        task.await.unwrap();
        assert!(handle.is_finished());

        manager
            .active_tasks
            .insert(uri.clone(), active_task(handle));
        manager.cleanup_finished_tasks(5);

        assert!(!manager.has_active_task(&uri));
        assert!(manager.active_tasks.contains_key(&uri));
    }

    #[tokio::test]
    async fn cleanup_finished_tasks_preserves_unfinished_entries() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///active.md").unwrap();

        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let handle = task.abort_handle();

        manager
            .active_tasks
            .insert(uri.clone(), active_task(handle.clone()));
        manager.cleanup_finished_tasks(5);

        assert!(manager.has_active_task(&uri));
        assert!(!handle.is_finished());
        handle.abort();
    }

    #[tokio::test]
    async fn cleanup_finished_tasks_preserves_replacement_after_scan() {
        let manager = SyntheticDiagnosticsManager::new();
        let uri = Url::parse("file:///replaced.md").unwrap();

        let finished_task = tokio::spawn(async {});
        let finished_handle = finished_task.abort_handle();
        finished_task.await.unwrap();
        assert!(finished_handle.is_finished());

        manager
            .active_tasks
            .insert(uri.clone(), active_task(finished_handle.clone()));
        let stale_cleanup_candidates = manager.finished_task_uris(5);
        assert_eq!(stale_cleanup_candidates, vec![uri.clone()]);

        let replacement_task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let replacement_handle = replacement_task.abort_handle();
        manager
            .active_tasks
            .insert(uri.clone(), active_task(replacement_handle.clone()));

        let removed = manager.remove_if_still_finished(stale_cleanup_candidates);

        assert_eq!(removed, 0);
        assert!(manager.has_active_task(&uri));
        assert!(!replacement_handle.is_finished());
        replacement_handle.abort();
    }

    #[tokio::test]
    async fn register_task_performs_opportunistic_cleanup() {
        let manager = SyntheticDiagnosticsManager::new();
        let finished_uri = Url::parse("file:///finished.md").unwrap();
        let new_uri = Url::parse("file:///new.md").unwrap();

        let finished_task = tokio::spawn(async {});
        let finished_handle = finished_task.abort_handle();
        finished_task.await.unwrap();
        assert!(finished_handle.is_finished());
        manager
            .active_tasks
            .insert(finished_uri.clone(), active_task(finished_handle));

        let new_task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });
        let new_handle = new_task.abort_handle();

        manager.register_task(new_uri.clone(), new_handle.clone());

        assert!(!manager.has_active_task(&finished_uri));
        assert!(manager.has_active_task(&new_uri));
        assert!(!new_handle.is_finished());
        new_handle.abort();
    }
}
