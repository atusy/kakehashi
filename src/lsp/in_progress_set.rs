//! Thread-safe set for tracking in-progress operations.
//!
//! This module provides `InProgressSet<T>`, a generic concurrent set for
//! tracking operations that are currently in progress to prevent duplicates.

use crate::error::LockResultExt;
use std::collections::HashSet;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// A thread-safe set for tracking in-progress operations.
///
/// This type is cheaply cloneable via `Arc` for sharing across async tasks.
/// It provides atomic "try to start" semantics that return whether the caller
/// was the one to initiate the operation.
///
/// # Example
///
/// ```ignore
/// let tracker: InProgressSet<String> = InProgressSet::new();
///
/// // First caller wins
/// assert!(tracker.try_start("task1"));
/// assert!(!tracker.try_start("task1")); // Already in progress
///
/// // Mark as complete
/// tracker.finish("task1");
/// assert!(tracker.try_start("task1")); // Can start again
/// ```
#[derive(Clone)]
pub struct InProgressSet<T> {
    items: Arc<Mutex<HashSet<T>>>,
}

impl<T: Eq + Hash + Clone> InProgressSet<T> {
    /// Create a new empty `InProgressSet`.
    pub fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Try to start an operation. Returns `true` if this call started the operation,
    /// `false` if it was already in progress.
    pub fn try_start(&self, item: &T) -> bool {
        self.items
            .lock()
            .recover_poison("InProgressSet::try_start")
            .insert(item.clone())
    }

    /// Mark an operation as complete, removing it from the in-progress set.
    pub fn finish(&self, item: &T) {
        self.items
            .lock()
            .recover_poison("InProgressSet::finish")
            .remove(item);
    }
}

impl<T: Eq + Hash + Clone> Default for InProgressSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl<T: Eq + Hash + Clone> InProgressSet<T> {
        /// Check if an item is currently in progress (test helper).
        fn is_in_progress(&self, item: &T) -> bool {
            self.items
                .lock()
                .recover_poison("InProgressSet::is_in_progress")
                .contains(item)
        }
    }

    #[test]
    fn should_track_in_progress_items() {
        let tracker: InProgressSet<String> = InProgressSet::new();

        // Initially not in progress
        assert!(!tracker.is_in_progress(&"lua".to_string()));

        // Try to start - should succeed
        assert!(tracker.try_start(&"lua".to_string()));

        // Now it's in progress
        assert!(tracker.is_in_progress(&"lua".to_string()));

        // Second try should fail (already in progress)
        assert!(!tracker.try_start(&"lua".to_string()));

        // Mark as complete
        tracker.finish(&"lua".to_string());

        // No longer in progress
        assert!(!tracker.is_in_progress(&"lua".to_string()));
    }

    #[test]
    fn should_handle_multiple_items() {
        let tracker: InProgressSet<String> = InProgressSet::new();

        assert!(tracker.try_start(&"a".to_string()));
        assert!(tracker.try_start(&"b".to_string()));
        assert!(tracker.try_start(&"c".to_string()));

        assert!(tracker.is_in_progress(&"a".to_string()));
        assert!(tracker.is_in_progress(&"b".to_string()));
        assert!(tracker.is_in_progress(&"c".to_string()));

        tracker.finish(&"b".to_string());

        assert!(tracker.is_in_progress(&"a".to_string()));
        assert!(!tracker.is_in_progress(&"b".to_string()));
        assert!(tracker.is_in_progress(&"c".to_string()));
    }
}
