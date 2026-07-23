//! Revision-scoped request tracking for semantic token cancellation.
//!
//! # Cancellation Support
//!
//! When a request for a newer document/settings scope arrives, it supersedes
//! every in-flight request for the prior scope. Concurrent consumers of the
//! same scope remain active so they can join one shared semantic artifact.
//!
//! This addresses the primary use case: users typing rapidly generate many semantic token
//! requests, and we want to skip computation for obsolete requests.
//!
//! `is_active()` gates request-local response shaping. Each scope also carries
//! a [`CancelToken`] that wakes parked obsolete consumers; the snapshot-owned
//! artifact slot has its own producer token so cancelling one consumer cannot
//! abort work another same-scope consumer still needs.

use crate::cancel::CancelToken;
use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;

/// Exact live-input scope shared by full, delta, and range consumers.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SemanticRequestScope {
    pub(crate) incarnation: u64,
    pub(crate) generation: u64,
    pub(crate) content_version: u64,
}

/// Tracking record for the newest scope on one URI.
#[derive(Debug)]
struct ActiveRequest {
    scope: SemanticRequestScope,
    consumers: ConsumerIds,
    cancel: CancelToken,
}

/// One request is the steady state; keep it inline and allocate only when
/// same-scope full/delta/range actually overlap.
#[derive(Debug)]
struct ConsumerIds {
    first: u64,
    additional: Vec<u64>,
}

impl ConsumerIds {
    fn new(first: u64) -> Self {
        Self {
            first,
            additional: Vec::new(),
        }
    }

    fn push(&mut self, id: u64) {
        if self.first == 0 {
            self.first = id;
        } else {
            self.additional.push(id);
        }
    }

    fn contains(&self, id: u64) -> bool {
        self.first == id || self.additional.contains(&id)
    }

    fn remove(&mut self, id: u64) {
        if self.first == id {
            self.first = self.additional.pop().unwrap_or(0);
        } else {
            self.additional.retain(|candidate| *candidate != id);
        }
    }
}

/// Monotonically increasing request ID for tracking
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Generates a unique request ID
fn next_request_id() -> u64 {
    loop {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::SeqCst);
        if id != 0 {
            return id;
        }
    }
}

/// Tracks active semantic token requests to support cancellation
#[derive(Debug, Clone)]
pub struct SemanticRequestTracker {
    /// Maps each URI to its newest scope and same-scope consumers.
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
    /// Same-scope consumers share the revision token and remain independently
    /// active. A newer scope replaces and cancels the old one. An obsolete
    /// scope arriving after its successor receives an already-cancelled token
    /// and is never installed.
    pub fn start_request(&self, uri: &Url, scope: SemanticRequestScope) -> (u64, CancelToken) {
        let request_id = next_request_id();
        if let Some(mut entry) = self.active_requests.get_mut(uri) {
            match scope.cmp(&entry.scope) {
                std::cmp::Ordering::Equal => {
                    entry.consumers.push(request_id);
                    return (request_id, entry.cancel.clone());
                }
                std::cmp::Ordering::Less => {
                    let cancelled = CancelToken::default();
                    cancelled.cancel();
                    return (request_id, cancelled);
                }
                std::cmp::Ordering::Greater => {
                    let previous_cancel = entry.cancel.clone();
                    let cancel = CancelToken::default();
                    *entry = ActiveRequest {
                        scope,
                        consumers: ConsumerIds::new(request_id),
                        cancel: cancel.clone(),
                    };
                    previous_cancel.cancel();
                    return (request_id, cancel);
                }
            }
        }

        let cancel = CancelToken::default();
        let active = ActiveRequest {
            scope,
            consumers: ConsumerIds::new(request_id),
            cancel: cancel.clone(),
        };
        match self.active_requests.entry(uri.clone()) {
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(active);
                (request_id, cancel)
            }
            dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
                match scope.cmp(&occupied.get().scope) {
                    std::cmp::Ordering::Equal => {
                        occupied.get_mut().consumers.push(request_id);
                        (request_id, occupied.get().cancel.clone())
                    }
                    std::cmp::Ordering::Less => {
                        cancel.cancel();
                        (request_id, cancel)
                    }
                    std::cmp::Ordering::Greater => {
                        let previous_cancel = occupied.get().cancel.clone();
                        occupied.insert(active);
                        previous_cancel.cancel();
                        (request_id, cancel)
                    }
                }
            }
        }
    }

    /// Checks if a request is still active (not superseded by a newer one).
    /// Returns true if the request should continue, false if it should abort.
    pub fn is_active(&self, uri: &Url, request_id: u64) -> bool {
        self.active_requests
            .get(uri)
            .is_some_and(|entry| entry.consumers.contains(request_id))
    }

    /// Detach one consumer. The newest empty scope marker stays installed so a
    /// delayed obsolete request cannot replace it and revive old work.
    pub fn finish_request(&self, uri: &Url, request_id: u64) {
        if let Some(mut entry) = self.active_requests.get_mut(uri) {
            entry.consumers.remove(request_id);
        }
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

    fn scope(version: u64) -> SemanticRequestScope {
        SemanticRequestScope {
            incarnation: 1,
            generation: 0,
            content_version: version,
        }
    }

    #[test]
    fn test_request_tracking_basic() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (req1, _token1) = tracker.start_request(&uri, scope(1));
        assert!(tracker.is_active(&uri, req1), "Request should be active");

        tracker.finish_request(&uri, req1);
        assert!(!tracker.is_active(&uri, req1), "Request should be finished");
    }

    #[test]
    fn test_request_superseding() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (req1, _token1) = tracker.start_request(&uri, scope(1));
        assert!(
            tracker.is_active(&uri, req1),
            "First request should be active"
        );

        // Start a new request - should supersede the first
        let (req2, _token2) = tracker.start_request(&uri, scope(2));
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

        let (_req1, token1) = tracker.start_request(&uri, scope(1));
        assert!(
            !token1.is_cancelled(),
            "First token should start uncancelled"
        );

        // Superseding must flip the previous request's token so its compute bails.
        let (_req2, token2) = tracker.start_request(&uri, scope(2));
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

        let (req1, _t1) = tracker.start_request(&uri1, scope(1));
        let (req2, _t2) = tracker.start_request(&uri2, scope(1));

        assert!(tracker.is_active(&uri1, req1), "Request 1 should be active");
        assert!(tracker.is_active(&uri2, req2), "Request 2 should be active");

        // Requests for different URIs don't interfere
        let (req3, _t3) = tracker.start_request(&uri1, scope(2));
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

        let (req, token) = tracker.start_request(&uri, scope(1));
        tracker.cancel_all_for_uri(&uri);
        assert!(!tracker.is_active(&uri, req), "Request should be cancelled");
        assert!(
            token.is_cancelled(),
            "Closing the document must cancel the in-flight token"
        );
    }

    #[test]
    fn same_scope_consumers_do_not_supersede_each_other() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (first, token1) = tracker.start_request(&uri, scope(7));
        let (second, token2) = tracker.start_request(&uri, scope(7));

        assert!(tracker.is_active(&uri, first));
        assert!(tracker.is_active(&uri, second));
        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());
    }

    #[test]
    fn delayed_obsolete_scope_cannot_cancel_newer_consumers() {
        let tracker = SemanticRequestTracker::new();
        let uri = Url::parse("file:///test.lua").unwrap();

        let (current, current_token) = tracker.start_request(&uri, scope(8));
        let (obsolete, obsolete_token) = tracker.start_request(&uri, scope(7));

        assert!(tracker.is_active(&uri, current));
        assert!(!tracker.is_active(&uri, obsolete));
        assert!(!current_token.is_cancelled());
        assert!(obsolete_token.is_cancelled());
    }
}
