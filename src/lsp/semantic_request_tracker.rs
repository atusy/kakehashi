//! Request tracking for semantic token operations to support cancellation and debouncing.
//!
//! # Cancellation Support
//!
//! When a new semantic token request arrives for a URI, it supersedes any in-flight request
//! for that URI. Handlers check `is_active()` at strategic points and exit early if superseded.
//!
//! This addresses the primary use case: users typing rapidly generate many semantic token
//! requests, and we want to skip computation for obsolete requests.
//!
//! `is_active()` only gates the handler at checkpoints — it cannot stop the
//! already-running blocking compute of the superseded request. Each request
//! therefore also carries a [`CancelToken`]: superseding a request (or closing
//! the document) flips the previous request's token, and the compute polls it to
//! bail out mid-flight instead of running to completion (see [`crate::cancel`]).

use crate::cancel::CancelToken;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

/// The tracking record for the most recent request on a URI: its id (for
/// `is_active` checkpoints) and the cancel token its compute polls.
#[derive(Debug, Clone)]
struct ActiveRequest {
    id: u64,
    cancel: CancelToken,
}

/// Monotonically increasing request ID for tracking
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Generates a unique request ID
fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst)
}

/// Tracks active semantic token requests to support cancellation
#[derive(Debug, Clone)]
pub struct SemanticRequestTracker {
    /// Maps URI to the most recent active request (id + cancel token)
    active_requests: Arc<DashMap<Url, ActiveRequest>>,
}

impl Default for SemanticRequestTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticRequestTracker {
    /// Creates a new request tracker
    pub fn new() -> Self {
        Self {
            active_requests: Arc::new(DashMap::new()),
        }
    }

    /// Starts tracking a new request for the given URI.
    ///
    /// Returns the request ID (for `is_active` checkpoints) and a fresh
    /// [`CancelToken`] the request's compute should poll. Automatically
    /// supersedes any previous request for the same URI — and flips that
    /// previous request's token so its still-running compute bails out.
    pub fn start_request(&self, uri: &Url) -> (u64, CancelToken) {
        let request_id = next_request_id();
        let cancel = CancelToken::default();
        let previous = self.active_requests.insert(
            uri.clone(),
            ActiveRequest {
                id: request_id,
                cancel: cancel.clone(),
            },
        );
        // Superseding a request means its result is no longer wanted: cancel its
        // in-flight compute so it stops burning CPU (the `is_active` checkpoints
        // alone can't interrupt the blocking work).
        if let Some(previous) = previous {
            previous.cancel.cancel();
        }
        (request_id, cancel)
    }

    /// Checks if a request is still active (not superseded by a newer one).
    /// Returns true if the request should continue, false if it should abort.
    pub fn is_active(&self, uri: &Url, request_id: u64) -> bool {
        self.active_requests
            .get(uri)
            .map(|entry| entry.id == request_id)
            .unwrap_or(false)
    }

    /// Finishes a request, removing it from tracking if it's still the active one.
    /// This prevents memory leaks from completed requests.
    pub fn finish_request(&self, uri: &Url, request_id: u64) {
        self.active_requests
            .remove_if(uri, |_, entry| entry.id == request_id);
    }

    /// Cancels all requests for a given URI.
    /// Useful when a document is closed: the removed request's token is flipped
    /// so any in-flight compute for the now-gone document stops.
    pub fn cancel_all_for_uri(&self, uri: &Url) {
        if let Some((_, removed)) = self.active_requests.remove(uri) {
            removed.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_tracking_basic() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (req1, _token1) = tracker.start_request(&uri);
        assert!(tracker.is_active(&uri, req1), "Request should be active");

        tracker.finish_request(&uri, req1);
        assert!(!tracker.is_active(&uri, req1), "Request should be finished");
    }

    #[test]
    fn test_request_superseding() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (req1, _token1) = tracker.start_request(&uri);
        assert!(
            tracker.is_active(&uri, req1),
            "First request should be active"
        );

        // Start a new request - should supersede the first
        let (req2, _token2) = tracker.start_request(&uri);
        assert!(
            !tracker.is_active(&uri, req1),
            "First request should be superseded"
        );
        assert!(
            tracker.is_active(&uri, req2),
            "Second request should be active"
        );
    }

    #[test]
    fn test_superseding_cancels_previous_token() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (_req1, token1) = tracker.start_request(&uri);
        assert!(
            !token1.is_cancelled(),
            "First token should start uncancelled"
        );

        // Superseding must flip the previous request's token so its compute bails.
        let (_req2, token2) = tracker.start_request(&uri);
        assert!(
            token1.is_cancelled(),
            "Superseded request's token must be cancelled"
        );
        assert!(
            !token2.is_cancelled(),
            "New request's token must stay active"
        );
    }

    #[test]
    fn test_multiple_uris() {
        let tracker = SemanticRequestTracker::new();
        let uri1 = Url::parse("file:///test1.lua").unwrap();
        let uri2 = Url::parse("file:///test2.lua").unwrap();

        let (req1, _t1) = tracker.start_request(&uri1);
        let (req2, _t2) = tracker.start_request(&uri2);

        assert!(tracker.is_active(&uri1, req1), "Request 1 should be active");
        assert!(tracker.is_active(&uri2, req2), "Request 2 should be active");

        // Requests for different URIs don't interfere
        let (req3, _t3) = tracker.start_request(&uri1);
        assert!(
            !tracker.is_active(&uri1, req1),
            "Request 1 should be superseded"
        );
        assert!(
            tracker.is_active(&uri2, req2),
            "Request 2 should still be active"
        );
        assert!(tracker.is_active(&uri1, req3), "Request 3 should be active");
    }

    #[test]
    fn test_cancel_all_for_uri() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (req, token) = tracker.start_request(&uri);
        tracker.cancel_all_for_uri(&uri);
        assert!(!tracker.is_active(&uri, req), "Request should be cancelled");
        assert!(
            token.is_cancelled(),
            "Closing the document must cancel the in-flight token"
        );
    }
}
