//! Response routing for pending LSP requests.
//!
//! This module provides the ResponseRouter which tracks pending requests
//! and routes incoming responses to their corresponding waiters via oneshot channels.
//!
//! # Architecture (ADR-0015)
//!
//! The ResponseRouter enables non-blocking response waiting:
//! - Before sending a request, register it via `register(id)` to get a oneshot Receiver
//! - The Reader Task calls `route(response)` when a response arrives
//! - The original requester awaits on the Receiver without holding any Mutex

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::oneshot;

use super::super::pool::UpstreamId;
use super::super::protocol::RequestId;

/// Result of attempting to route a response to a pending request.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteResult {
    /// Response was successfully delivered to the waiting receiver.
    Delivered,
    /// Request ID was found but the receiver was dropped (requester gave up).
    ReceiverDropped,
    /// No pending request found for this ID, or message was not a response
    /// (e.g., notification without an ID field).
    NotFound,
}

/// Routes responses to pending requests via oneshot channels.
///
/// Thread-safe router that tracks in-flight requests and delivers responses
/// to their waiters. Designed for use with a single Reader Task that calls
/// `route()` for each incoming response.
///
/// # Cancel Forwarding Support
///
/// The router also maintains a bidirectional mapping between upstream and downstream
/// request IDs via `upstream_to_downstream` and `downstream_to_upstream`. This enables
/// O(1) $/cancelRequest forwarding and cleanup:
/// - When a request is registered with an upstream ID, both mappings are stored
/// - When a cancel notification arrives, `lookup_downstream_id()` translates it in O(1)
/// - When a request completes (via `route()` or `remove()`), cleanup is O(1)
///
/// # Usage
///
/// ```ignore
/// let router = ResponseRouter::new();
/// let rx = router.register(request_id);  // Before sending request
/// // ... send request to downstream server ...
/// let response = rx.await?;  // Wait without holding Mutex
/// ```
pub(crate) struct ResponseRouter {
    /// All router state protected by a single mutex to avoid lock contention
    /// and simplify reasoning about thread safety.
    state: std::sync::Mutex<ResponseRouterState>,
}

/// Internal state for ResponseRouter, protected by a single mutex.
struct ResponseRouterState {
    /// Pending requests waiting for responses.
    pending: HashMap<RequestId, oneshot::Sender<serde_json::Value>>,
    /// Maps upstream request ID (from client) to downstream request ID (to LS).
    ///
    /// Used for $/cancelRequest forwarding: when the client cancels request 42,
    /// we look up that 42 maps to downstream ID 7, and forward the cancel to LS.
    ///
    /// Supports both numeric and string IDs per LSP spec.
    upstream_to_downstream: HashMap<UpstreamId, RequestId>,
    /// Reverse mapping: downstream request ID -> upstream request ID.
    ///
    /// Enables O(1) cleanup when a response is routed or a request is removed.
    downstream_to_upstream: HashMap<RequestId, UpstreamId>,
}

impl ResponseRouter {
    /// Create a new empty ResponseRouter.
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(ResponseRouterState {
                pending: HashMap::new(),
                upstream_to_downstream: HashMap::new(),
                downstream_to_upstream: HashMap::new(),
            }),
        }
    }

    /// Register a pending request and return a receiver for the response.
    ///
    /// Must be called before sending the request to ensure the response
    /// can be routed when it arrives.
    ///
    /// Returns `None` if a request with this ID is already pending (duplicate ID).
    pub(crate) fn register(&self, id: RequestId) -> Option<oneshot::Receiver<serde_json::Value>> {
        self.register_with_upstream(id, None)
    }

    /// Register a pending request with upstream ID mapping for cancel forwarding.
    ///
    /// Like `register()`, but also stores a mapping from the upstream (client) request ID
    /// to the downstream (language server) request ID. This enables $/cancelRequest
    /// forwarding by translating upstream IDs to downstream IDs.
    ///
    /// # Arguments
    /// * `downstream_id` - The request ID used for the downstream language server
    /// * `upstream_id` - The original request ID from the upstream client (None for internal requests)
    ///
    /// Returns `None` if a request with this downstream ID is already pending.
    pub(crate) fn register_with_upstream(
        &self,
        downstream_id: RequestId,
        upstream_id: Option<UpstreamId>,
    ) -> Option<oneshot::Receiver<serde_json::Value>> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Prevent duplicate registration
        if state.pending.contains_key(&downstream_id) {
            return None;
        }

        state.pending.insert(downstream_id, tx);

        // Store bidirectional mapping if upstream_id is provided
        if let Some(upstream) = upstream_id {
            state
                .upstream_to_downstream
                .insert(upstream.clone(), downstream_id);
            state.downstream_to_upstream.insert(downstream_id, upstream);
        }

        Some(rx)
    }

    /// Look up the downstream request ID for an upstream request ID.
    ///
    /// Used by $/cancelRequest forwarding to translate the client's request ID
    /// to the language server's request ID. O(1) lookup.
    ///
    /// Returns `None` if no mapping exists (request not found or already completed).
    pub(crate) fn lookup_downstream_id(&self, upstream_id: &UpstreamId) -> Option<RequestId> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.upstream_to_downstream.get(upstream_id).copied()
    }

    /// Route a response to its pending request.
    ///
    /// Extracts the request ID from the response and sends it to the
    /// corresponding waiter. Returns a `RouteResult` indicating what happened.
    ///
    /// Also cleans up the bidirectional cancel map entries for this request ID in O(1).
    ///
    /// # Returns
    /// - `RouteResult::Delivered` - Response was delivered to the waiting receiver
    /// - `RouteResult::ReceiverDropped` - ID was found but receiver was dropped (requester cancelled)
    /// - `RouteResult::NotFound` - No pending request for this ID
    pub(crate) fn route(&self, response: serde_json::Value) -> RouteResult {
        let id = match RequestId::from_json(&response) {
            Some(id) => id,
            None => return RouteResult::NotFound, // Not a response (notification), skip
        };

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let tx = state.pending.remove(&id);

        // Clean up bidirectional cancel map entries in O(1)
        Self::remove_cancel_mapping_inner(&mut state, id);

        match tx {
            Some(sender) => {
                if sender.send(response).is_ok() {
                    RouteResult::Delivered
                } else {
                    RouteResult::ReceiverDropped
                }
            }
            None => RouteResult::NotFound,
        }
    }

    /// Remove bidirectional cancel map entries for a downstream request ID.
    ///
    /// O(1) cleanup: looks up upstream ID via downstream_to_upstream, then removes
    /// both directions.
    ///
    /// Note: This is an internal helper that requires the caller to hold the lock.
    fn remove_cancel_mapping_inner(state: &mut ResponseRouterState, downstream_id: RequestId) {
        if let Some(upstream_id) = state.downstream_to_upstream.remove(&downstream_id) {
            state.upstream_to_downstream.remove(&upstream_id);
        }
    }

    /// Get the number of pending requests.
    ///
    /// Used for liveness timeout management (ADR-0014):
    /// - Timer starts when pending transitions 0 -> 1
    /// - Timer stops when pending transitions to 0
    pub(crate) fn pending_count(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.len()
    }

    /// Remove a pending request without sending a response.
    ///
    /// Used for cleanup when a request fails before being sent to the downstream server.
    /// Also cleans up the bidirectional cancel map entries in O(1).
    ///
    /// Returns `true` if the request was removed, `false` if it wasn't pending.
    pub(crate) fn remove(&self, id: RequestId) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let removed = state.pending.remove(&id).is_some();

        if removed {
            Self::remove_cancel_mapping_inner(&mut state, id);
        }

        removed
    }

    /// Fail a single pending request with an error response.
    ///
    /// Called when a write fails or the connection is closing (ADR-0015). The request
    /// is removed from pending and the waiter receives an error response.
    ///
    /// Uses `REQUEST_FAILED` (-32803) for queue/write errors, which is distinct
    /// from `INTERNAL_ERROR` (-32603) used by `fail_all()` for connection failures.
    ///
    /// Returns `true` if the request was found and failed, `false` if not pending.
    ///
    /// # Idempotency
    ///
    /// This method is idempotent: calling it multiple times for the same request ID
    /// is safe and will simply return `false` on subsequent calls. This property is
    /// critical for avoiding double-cleanup races between sender cleanup and writer
    /// task cleanup (see ADR-0015 Appendix A).
    pub(crate) fn fail_request(&self, id: RequestId, reason: &str) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        let tx = state.pending.remove(&id);

        // Clean up bidirectional cancel map entries in O(1)
        Self::remove_cancel_mapping_inner(&mut state, id);

        // Release lock before sending to avoid holding it during channel operations
        drop(state);

        match tx {
            Some(sender) => {
                let error_response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.as_i64(),
                    "error": {
                        "code": -32803, // REQUEST_FAILED per LSP spec
                        "message": format!("bridge: {}", reason)
                    }
                });
                let _ = sender.send(error_response);
                true
            }
            None => false,
        }
    }

    /// Fail all pending requests with an internal error response.
    ///
    /// Called when the connection fails (e.g., reader task panic, liveness timeout)
    /// to ensure all waiters receive a response per LSP guarantee.
    ///
    /// # Cancel Map Cleanup
    ///
    /// This method clears both cancel map directions (upstream_to_downstream and
    /// downstream_to_upstream) since all requests are being completed.
    ///
    /// Note: The `LanguageServerPool.upstream_request_registry` (which maps upstream
    /// ID -> language) is NOT cleared by this method because:
    /// 1. The ResponseRouter doesn't have access to the pool
    /// 2. Stale entries are harmless - `forward_cancel_by_upstream_id()` checks
    ///    connection state before forwarding, so stale entries fail gracefully
    /// 3. Entries are cleaned up when new requests reuse the same upstream IDs
    ///
    /// This is an intentional design tradeoff: keeping the registry separate from
    /// the per-connection router simplifies the architecture at the cost of
    /// temporarily stale entries that have no runtime impact.
    pub(crate) fn fail_all(&self, error_message: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<_> = state.pending.drain().collect();

        // Clear both cancel map directions
        state.upstream_to_downstream.clear();
        state.downstream_to_upstream.clear();

        // Release lock before sending to avoid holding it during channel operations
        drop(state);

        for (id, tx) in entries {
            let error_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.as_i64(),
                "error": {
                    "code": -32603, // InternalError
                    "message": error_message
                }
            });
            let _ = tx.send(error_response);
        }
    }
}

/// RAII guard that removes a pending ResponseRouter entry on drop.
///
/// When an async task is aborted (e.g., via `JoinSet::abort_all()`), the
/// future is dropped at its next `.await` point. Without this guard, the
/// ResponseRouter entry for the in-flight request would remain until the
/// downstream server responds (oneshot send fails silently) or the
/// connection drops (`fail_all()`).
///
/// The guard is disarmed after `wait_for_response` completes, because at
/// that point the router has already consumed or cleaned up the entry.
pub(crate) struct RouterCleanupGuard {
    router: Arc<ResponseRouter>,
    /// When `Some`, Drop will remove the router entry for this ID.
    /// Cleared by `disarm()` after wait_for_response completes.
    request_id: Option<RequestId>,
}

impl RouterCleanupGuard {
    pub(crate) fn new(router: Arc<ResponseRouter>, request_id: RequestId) -> Self {
        Self {
            router,
            request_id: Some(request_id),
        }
    }

    /// Prevent the guard from cleaning up on drop.
    ///
    /// Call this after `wait_for_response` completes, because at that point
    /// the router has already consumed or cleaned up the entry.
    pub(crate) fn disarm(&mut self) {
        self.request_id.take();
    }
}

impl Drop for RouterCleanupGuard {
    fn drop(&mut self) {
        if let Some(id) = self.request_id {
            self.router.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_router_has_no_pending_requests() {
        let router = ResponseRouter::new();
        assert_eq!(router.pending_count(), 0);
    }

    #[test]
    fn register_returns_receiver_and_increments_pending() {
        let router = ResponseRouter::new();
        let id = RequestId::new(1);

        let rx = router.register(id);
        assert!(rx.is_some(), "register should return Some(receiver)");
        assert_eq!(router.pending_count(), 1);
    }

    #[test]
    fn register_duplicate_id_returns_none() {
        let router = ResponseRouter::new();
        let id = RequestId::new(1);

        let rx1 = router.register(id);
        assert!(rx1.is_some());

        let rx2 = router.register(id);
        assert!(rx2.is_none(), "duplicate ID should return None");
        assert_eq!(router.pending_count(), 1, "count should not increase");
    }

    #[tokio::test]
    async fn route_delivers_response_to_waiter() {
        let router = ResponseRouter::new();
        let id = RequestId::new(42);

        let rx = router.register(id).expect("register should succeed");

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "contents": "Hello" }
        });

        let result = router.route(response.clone());
        assert_eq!(
            result,
            RouteResult::Delivered,
            "route should return Delivered"
        );

        let received = rx.await.expect("receiver should get response");
        assert_eq!(received["id"], 42);
        assert_eq!(received["result"]["contents"], "Hello");
    }

    #[test]
    fn route_returns_not_found_for_unknown_id() {
        let router = ResponseRouter::new();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 999,
            "result": null
        });

        let result = router.route(response);
        assert_eq!(
            result,
            RouteResult::NotFound,
            "route should return NotFound for unknown ID"
        );
    }

    #[test]
    fn route_returns_not_found_for_notification() {
        let router = ResponseRouter::new();
        let id = RequestId::new(1);
        let _rx = router.register(id);

        // Notification has no "id" field
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {}
        });

        let result = router.route(notification);
        assert_eq!(
            result,
            RouteResult::NotFound,
            "route should return NotFound for notifications"
        );
        assert_eq!(router.pending_count(), 1, "pending request should remain");
    }

    #[tokio::test]
    async fn fail_all_sends_error_to_all_waiters() {
        let router = ResponseRouter::new();

        let rx1 = router.register(RequestId::new(1)).unwrap();
        let rx2 = router.register(RequestId::new(2)).unwrap();

        router.fail_all("connection lost");

        assert_eq!(router.pending_count(), 0, "all pending should be cleared");

        let response1 = rx1.await.expect("should receive error response");
        assert_eq!(response1["error"]["code"], -32603);
        assert_eq!(response1["error"]["message"], "connection lost");
        assert_eq!(response1["id"], 1);

        let response2 = rx2.await.expect("should receive error response");
        assert_eq!(response2["error"]["code"], -32603);
        assert_eq!(response2["id"], 2);
    }

    #[tokio::test]
    async fn route_after_receiver_dropped_returns_receiver_dropped() {
        let router = ResponseRouter::new();
        let id = RequestId::new(1);

        let rx = router.register(id).unwrap();
        drop(rx); // Simulate requester giving up

        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        });

        let result = router.route(response);
        assert_eq!(
            result,
            RouteResult::ReceiverDropped,
            "route should return ReceiverDropped when receiver dropped"
        );
        assert_eq!(router.pending_count(), 0, "pending should be cleared");
    }

    #[test]
    fn remove_clears_pending_request() {
        let router = ResponseRouter::new();
        let id = RequestId::new(1);

        let _rx = router.register(id).unwrap();
        assert_eq!(router.pending_count(), 1);

        let removed = router.remove(id);
        assert!(removed, "remove should return true for pending request");
        assert_eq!(router.pending_count(), 0, "pending should be cleared");
    }

    #[test]
    fn remove_returns_false_for_unknown_id() {
        let router = ResponseRouter::new();
        let id = RequestId::new(999);

        let removed = router.remove(id);
        assert!(!removed, "remove should return false for unknown ID");
    }

    // ========================================
    // CancelMap tests (ADR-0015 Cancel Forwarding)
    // ========================================

    /// Test that register_with_upstream stores upstream->downstream mapping.
    ///
    /// When registering a request with an upstream ID, the router should store
    /// the mapping so we can later look up the downstream ID for cancel forwarding.
    #[test]
    fn register_with_upstream_stores_mapping() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);

        let rx = router.register_with_upstream(downstream_id, Some(upstream_id.clone()));
        assert!(rx.is_some(), "register_with_upstream should succeed");
        assert_eq!(router.pending_count(), 1);

        // Should be able to look up downstream ID by upstream ID
        let looked_up = router.lookup_downstream_id(&upstream_id);
        assert_eq!(
            looked_up,
            Some(downstream_id),
            "lookup should return downstream ID for upstream ID"
        );
    }

    /// Test that register_with_upstream with None upstream_id behaves like register.
    #[test]
    fn register_with_upstream_none_behaves_like_register() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);

        let rx = router.register_with_upstream(downstream_id, None);
        assert!(rx.is_some(), "register_with_upstream should succeed");
        assert_eq!(router.pending_count(), 1);
    }

    /// Test that lookup_downstream_id returns None for unknown upstream ID.
    #[test]
    fn lookup_downstream_id_returns_none_for_unknown() {
        let router = ResponseRouter::new();

        let result = router.lookup_downstream_id(&UpstreamId::Number(999));
        assert_eq!(
            result, None,
            "lookup should return None for unknown upstream ID"
        );
    }

    /// Test that route() removes the cancel map entry.
    ///
    /// After routing a response, the cancel map entry should be removed
    /// because the request is complete and no longer needs cancel forwarding.
    #[tokio::test]
    async fn route_removes_cancel_map_entry() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);

        let rx = router
            .register_with_upstream(downstream_id, Some(upstream_id.clone()))
            .expect("should register");

        // Verify mapping exists before route
        assert_eq!(
            router.lookup_downstream_id(&upstream_id),
            Some(downstream_id)
        );

        // Route the response
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": null
        });
        let result = router.route(response);
        assert_eq!(result, RouteResult::Delivered, "route should succeed");

        // Verify mapping is removed after route
        assert_eq!(
            router.lookup_downstream_id(&upstream_id),
            None,
            "cancel map entry should be removed after route"
        );

        // Clean up receiver
        let _ = rx.await;
    }

    /// Test that remove() also removes the cancel map entry.
    #[test]
    fn remove_also_removes_cancel_map_entry() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);

        let _rx = router
            .register_with_upstream(downstream_id, Some(upstream_id.clone()))
            .expect("should register");

        // Verify mapping exists before remove
        assert_eq!(
            router.lookup_downstream_id(&upstream_id),
            Some(downstream_id)
        );

        // Remove the pending request
        let removed = router.remove(downstream_id);
        assert!(removed, "remove should succeed");

        // Verify mapping is removed
        assert_eq!(
            router.lookup_downstream_id(&upstream_id),
            None,
            "cancel map entry should be removed after remove"
        );
    }

    /// Test that fail_all() clears the cancel map.
    #[tokio::test]
    async fn fail_all_clears_cancel_map() {
        let router = ResponseRouter::new();

        let _rx1 = router
            .register_with_upstream(RequestId::new(1), Some(UpstreamId::Number(100)))
            .unwrap();
        let _rx2 = router
            .register_with_upstream(RequestId::new(2), Some(UpstreamId::Number(200)))
            .unwrap();

        // Verify mappings exist
        assert!(
            router
                .lookup_downstream_id(&UpstreamId::Number(100))
                .is_some()
        );
        assert!(
            router
                .lookup_downstream_id(&UpstreamId::Number(200))
                .is_some()
        );

        router.fail_all("connection lost");

        // Verify mappings are cleared
        assert_eq!(
            router.lookup_downstream_id(&UpstreamId::Number(100)),
            None,
            "cancel map should be cleared by fail_all"
        );
        assert_eq!(router.lookup_downstream_id(&UpstreamId::Number(200)), None);
    }

    /// Test that lookup_downstream_id does NOT remove the pending entry.
    ///
    /// This is critical for cancel forwarding: when we look up the downstream ID
    /// to forward a cancel notification, we must NOT remove the pending entry
    /// because we still need to receive the response (which may come before,
    /// after, or instead of an error response from the downstream server).
    ///
    /// Per LSP spec, a cancelled request should still receive a response
    /// (either the normal result or an error with code -32800 RequestCancelled).
    #[tokio::test]
    async fn lookup_downstream_id_preserves_pending_entry() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);

        let rx = router
            .register_with_upstream(downstream_id, Some(upstream_id.clone()))
            .expect("should register");

        // Verify initial state
        assert_eq!(router.pending_count(), 1);
        assert_eq!(
            router.lookup_downstream_id(&upstream_id),
            Some(downstream_id)
        );

        // Look up the downstream ID (as we would when forwarding a cancel)
        let looked_up = router.lookup_downstream_id(&upstream_id);
        assert_eq!(looked_up, Some(downstream_id));

        // Key assertion: pending entry should still exist after lookup
        assert_eq!(
            router.pending_count(),
            1,
            "lookup_downstream_id should NOT remove the pending entry"
        );

        // We should still be able to route a response
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "cancelled_but_still_responded": true }
        });
        let result = router.route(response);
        assert_eq!(
            result,
            RouteResult::Delivered,
            "should still be able to route response after cancel lookup"
        );

        // Verify we receive the response
        let received = rx.await.expect("receiver should still get response");
        assert_eq!(received["id"], 42);
        assert!(
            received["result"]["cancelled_but_still_responded"]
                .as_bool()
                .unwrap()
        );

        // Now the pending entry should be removed (by route, not by lookup)
        assert_eq!(router.pending_count(), 0);
    }

    /// Test that cancel map entry persists after lookup.
    ///
    /// The cancel map entry should only be removed when the request completes
    /// (via route() or remove()), not when it's looked up for cancel forwarding.
    /// This ensures we can still clean up properly when the response arrives.
    #[test]
    fn cancel_map_entry_persists_after_lookup() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);

        let _rx = router
            .register_with_upstream(downstream_id, Some(upstream_id.clone()))
            .expect("should register");

        // Look up the downstream ID multiple times
        for _ in 0..3 {
            let result = router.lookup_downstream_id(&upstream_id);
            assert_eq!(
                result,
                Some(downstream_id),
                "cancel map entry should persist after lookup"
            );
        }
    }

    // ========================================
    // fail_request tests (ADR-0015 Single-Writer Loop)
    // ========================================

    /// Test that fail_request sends REQUEST_FAILED error to waiter.
    #[tokio::test]
    async fn fail_request_sends_request_failed_error() {
        let router = ResponseRouter::new();
        let id = RequestId::new(42);

        let rx = router.register(id).expect("should register");

        let removed = router.fail_request(id, "queue full");
        assert!(
            removed,
            "fail_request should return true for pending request"
        );

        let response = rx.await.expect("should receive error response");
        assert_eq!(response["error"]["code"], -32803); // REQUEST_FAILED
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("queue full")
        );
        assert_eq!(response["id"], 42);
    }

    /// Test that fail_request returns false for unknown ID.
    #[test]
    fn fail_request_returns_false_for_unknown_id() {
        let router = ResponseRouter::new();
        let id = RequestId::new(999);

        let removed = router.fail_request(id, "test");
        assert!(!removed, "fail_request should return false for unknown ID");
    }

    /// Test that fail_request is idempotent (critical for double-cleanup races).
    ///
    /// ADR-0015 Appendix A: Cleanup operations MUST be idempotent to handle
    /// concurrent cleanup from sender and writer task.
    #[test]
    fn fail_request_is_idempotent() {
        let router = ResponseRouter::new();
        let id = RequestId::new(42);

        let _rx = router.register(id).expect("should register");

        // First call removes the entry
        let first = router.fail_request(id, "first");
        assert!(first);

        // Second call is a no-op
        let second = router.fail_request(id, "second");
        assert!(!second, "fail_request should be idempotent");
    }

    /// Test that fail_request cleans up cancel map.
    #[test]
    fn fail_request_cleans_up_cancel_map() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);

        let _rx = router
            .register_with_upstream(downstream_id, Some(upstream_id.clone()))
            .expect("should register");

        // Verify mapping exists
        assert!(router.lookup_downstream_id(&upstream_id).is_some());

        // Fail the request
        router.fail_request(downstream_id, "test");

        // Cancel mapping should be cleaned up
        assert_eq!(router.lookup_downstream_id(&upstream_id), None);
    }

    /// Test double cleanup is safe (both remove and fail_request on same ID).
    ///
    /// ADR-0015 Appendix A: When a request fails, cleanup can happen from two places:
    /// 1. Sender cleanup: send_request() failure calls router.remove()
    /// 2. Writer cleanup: writer task calls router.fail_request() on write error
    ///
    /// This test verifies that exactly one cleanup succeeds regardless of order.
    #[tokio::test]
    async fn double_cleanup_is_safe() {
        let router = std::sync::Arc::new(ResponseRouter::new());
        let id = RequestId::new(42);

        let _rx = router.register(id).expect("should register");

        // Simulate both sender and writer task trying to clean up
        let router1 = router.clone();
        let router2 = router.clone();

        let (result1, result2) = tokio::join!(async move { router1.remove(id) }, async move {
            router2.fail_request(id, "concurrent cleanup")
        });

        // Exactly one should succeed
        assert!(
            (result1 && !result2) || (!result1 && result2),
            "Exactly one cleanup should succeed: remove={}, fail_request={}",
            result1,
            result2
        );
    }
}
