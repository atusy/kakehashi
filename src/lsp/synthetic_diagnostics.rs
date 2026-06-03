//! Background diagnostic collection on `didSave`/`didOpen` (pull-first-diagnostic-forwarding Phase 2):
//! pull internally, push via `textDocument/publishDiagnostics`.
//!
//! Rapid-fire events supersede each other — `SyntheticDiagnosticsManager`
//! aborts the prior in-flight task via `AbortHandle` so only the latest
//! collection publishes, and uses `DashMap` for concurrent access via sharded
//! locks (no single global lock).

use dashmap::DashMap;
use tokio::task::AbortHandle;
use url::Url;

/// Tracks active synthetic diagnostic tasks per document.
///
/// When a new task is spawned for a document, any existing task for that
/// document is aborted (superseded). This ensures only the latest diagnostic
/// collection publishes results.
#[derive(Default)]
pub(crate) struct SyntheticDiagnosticsManager {
    /// Map from document URI to the AbortHandle of the active diagnostic task.
    /// When a new task starts, the previous task (if any) is aborted.
    active_tasks: DashMap<Url, AbortHandle>,
}

impl SyntheticDiagnosticsManager {
    /// Create a new manager.
    pub(crate) fn new() -> Self {
        Self {
            active_tasks: DashMap::new(),
        }
    }

    /// Register a new diagnostic task for a document, superseding (and aborting)
    /// any existing task and returning its `AbortHandle`.
    ///
    /// Also opportunistically cleans up finished tasks to prevent memory buildup.
    pub(crate) fn register_task(&self, uri: Url, abort_handle: AbortHandle) -> Option<AbortHandle> {
        // Opportunistic cleanup: remove entries for tasks that have completed.
        // This prevents memory buildup from documents that were saved but not re-saved.
        // We limit to a small number to avoid blocking the registration.
        self.cleanup_finished_tasks(5);

        let previous = self.active_tasks.insert(uri, abort_handle);

        if let Some(ref prev_handle) = previous {
            // Abort the previous task - it's now superseded
            prev_handle.abort();
            log::debug!(
                target: "kakehashi::synthetic_diag",
                "Superseded previous diagnostic task"
            );
        }

        previous
    }

    /// Remove entries for tasks that have finished.
    ///
    /// Called opportunistically during registration to prevent memory buildup.
    /// Limited to avoid O(n) scan on every registration.
    fn cleanup_finished_tasks(&self, limit: usize) {
        let mut cleaned = 0;
        // Collect keys to remove to avoid holding multiple references during iteration
        let mut to_remove = Vec::with_capacity(limit);

        for entry in self.active_tasks.iter() {
            if entry.value().is_finished() {
                to_remove.push(entry.key().clone());
                cleaned += 1;
                if cleaned >= limit {
                    break;
                }
            }
        }

        for uri in to_remove {
            self.active_tasks.remove(&uri);
        }

        if cleaned > 0 {
            log::trace!(
                target: "kakehashi::synthetic_diag",
                "Cleaned up {} finished diagnostic task entries",
                cleaned
            );
        }
    }

    /// Check if there's an active task for a document.
    ///
    /// Useful for debugging and tests.
    #[cfg(test)]
    pub(crate) fn has_active_task(&self, uri: &Url) -> bool {
        self.active_tasks.contains_key(uri)
    }

    /// Abort all active tasks and clear the map.
    ///
    /// Called during server shutdown to clean up.
    pub(crate) fn abort_all(&self) {
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
        if let Some((_, handle)) = self.active_tasks.remove(uri) {
            handle.abort();
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
        let superseded = manager.register_task(uri.clone(), handle1.clone());
        assert!(superseded.is_none());
        assert!(manager.has_active_task(&uri));

        // Spawn and register task 2
        let task2 = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            43
        });
        let handle2 = task2.abort_handle();

        let superseded = manager.register_task(uri.clone(), handle2);
        assert!(superseded.is_some());

        // Task 1 should be aborted - yield to let the abort propagate
        tokio::task::yield_now().await;
        assert!(handle1.is_finished());

        // Wait for task 2 to complete
        let result = task2.await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 43);
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
}
