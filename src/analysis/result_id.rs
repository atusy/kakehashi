//! Monotonic `result_id` for LSP `SemanticTokens` responses — lets the client
//! request `semanticTokens/full/delta` against a known baseline.
//!
//! Not to be confused with `region_id` (`NodeTracker`): `result_id` is a
//! global sequential integer per response (always changes), used by the LSP
//! client; `region_id` is a position-keyed ULID per injection region (stable
//! across edits when positions shift correctly), used internally to route to
//! the right virtual document.

use std::sync::atomic::{AtomicU64, Ordering};

static TOKEN_RESULT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique, monotonically increasing `result_id` (thread-safe) for the
/// `result_id` field of `SemanticTokens`/`SemanticTokensDelta` responses. Not for
/// injection region identification — see `NodeTracker::get_or_create()` for those
/// (position-based ULIDs).
pub fn next_result_id() -> String {
    TOKEN_RESULT_COUNTER
        .fetch_add(1, Ordering::SeqCst)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_result_id_returns_string() {
        let id: String = next_result_id();
        assert!(!id.is_empty(), "result_id should not be empty");
    }

    #[test]
    fn test_next_result_id_monotonic() {
        let id1: u64 = next_result_id().parse().expect("should be numeric");
        let id2: u64 = next_result_id().parse().expect("should be numeric");
        let id3: u64 = next_result_id().parse().expect("should be numeric");

        assert!(
            id2 > id1,
            "id2 ({}) should be greater than id1 ({})",
            id2,
            id1
        );
        assert!(
            id3 > id2,
            "id3 ({}) should be greater than id2 ({})",
            id3,
            id2
        );
    }

    #[test]
    fn test_next_result_id_concurrent() {
        use std::collections::HashSet;
        use std::sync::Arc;
        use std::thread;

        const NUM_THREADS: usize = 10;
        const IDS_PER_THREAD: usize = 100;

        let results: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let results = Arc::clone(&results);
                thread::spawn(move || {
                    for _ in 0..IDS_PER_THREAD {
                        let id = next_result_id();
                        results.lock().unwrap().push(id);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("thread should not panic");
        }

        let all_ids = results.lock().unwrap();
        let unique_ids: HashSet<_> = all_ids.iter().collect();

        assert_eq!(
            all_ids.len(),
            unique_ids.len(),
            "All {} IDs should be unique, but found {} duplicates",
            all_ids.len(),
            all_ids.len() - unique_ids.len()
        );
    }

    /// Verifies that the result_id format is suitable for LSP semantic tokens.
    /// The ID should be a numeric string that can be compared for equality.
    #[test]
    fn test_semantic_tokens_full_uses_atomic_id() {
        // Verify the format matches what LSP expects: a simple string
        let id1 = next_result_id();
        let id2 = next_result_id();

        // IDs should be parseable as numbers
        assert!(
            id1.parse::<u64>().is_ok(),
            "result_id should be numeric: {}",
            id1
        );
        assert!(
            id2.parse::<u64>().is_ok(),
            "result_id should be numeric: {}",
            id2
        );

        // IDs should be different
        assert_ne!(id1, id2, "Sequential IDs should be different");

        // IDs should not contain the old format patterns
        assert!(!id1.starts_with("v"), "Should not use old format: {}", id1);
        assert!(!id1.contains("_"), "Should not use old format: {}", id1);
    }
}
