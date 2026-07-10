//! Response routing for pending LSP requests.
//!
//! Tracks pending requests and routes incoming responses to their waiters via
//! oneshot channels (ls-bridge-message-ordering). A requester registers before sending, then awaits
//! the receiver without holding any Mutex; the Reader Task calls `route()` on arrival.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::LockResultExt;

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

/// Thread-safe router that delivers each downstream response to a registered
/// oneshot waiter. Single Reader Task calls `route()` per incoming message.
///
/// Also keeps bidirectional `upstream ↔ downstream` ID maps so cancel
/// forwarding (translate, send, cleanup) is O(1) at register, route, and remove.
pub(crate) struct ResponseRouter {
    /// All router state protected by a single mutex to avoid lock contention
    /// and simplify reasoning about thread safety.
    state: std::sync::Mutex<ResponseRouterState>,
}

/// Internal state for ResponseRouter, protected by a single mutex.
struct ResponseRouterState {
    /// Advances whenever downstream-progressing requests transition 0 -> 1.
    liveness_epoch: u64,
    /// Pending requests waiting for responses.
    pending: HashMap<RequestId, PendingRequest>,
    /// Maps upstream request ID (from client) to downstream request IDs (to LS).
    ///
    /// Used for $/cancelRequest forwarding: when the client cancels request 42,
    /// we look up the downstream IDs 42 spawned and forward a cancel for each.
    ///
    /// One upstream id can map to SEVERAL in-flight downstream requests on the
    /// same connection — e.g. concurrent injection regions of one formatting
    /// request, each issuing its own downstream request. A single-value map
    /// would let the latest registration overwrite earlier ones, so a cancel
    /// could target the wrong request and completing one request would orphan
    /// its siblings' mappings (PR #347 review). `Vec` keeps registration order
    /// for deterministic behavior; entries are tiny (a handful per upstream).
    ///
    /// Supports both numeric and string IDs per LSP spec.
    upstream_to_downstream: HashMap<UpstreamId, Vec<RequestId>>,
    /// Reverse mapping: downstream request ID -> upstream request ID.
    ///
    /// Enables O(1) cleanup when a response is routed or a request is removed.
    downstream_to_upstream: HashMap<RequestId, UpstreamId>,
}

struct PendingRequest {
    response_tx: oneshot::Sender<serde_json::Value>,
    delivery: RequestDelivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestDelivery {
    Queued,
    Writing,
    Sent,
    CancelledQueued,
}

impl ResponseRouter {
    /// Create a new empty ResponseRouter.
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(ResponseRouterState {
                liveness_epoch: 0,
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
    /// Like `register()`, but also stores an upstream→downstream ID mapping so
    /// `$/cancelRequest` can be translated and forwarded. `upstream_id` is `None`
    /// for internal requests. Returns `None` if the downstream ID is already pending.
    pub(crate) fn register_with_upstream(
        &self,
        downstream_id: RequestId,
        upstream_id: Option<UpstreamId>,
    ) -> Option<oneshot::Receiver<serde_json::Value>> {
        self.register_with_upstream_liveness(downstream_id, upstream_id)
            .map(|(rx, _)| rx)
    }

    /// Register and atomically report whether this request transitions the
    /// set of downstream-progressing requests from empty to non-empty.
    pub(crate) fn register_with_upstream_liveness(
        &self,
        downstream_id: RequestId,
        upstream_id: Option<UpstreamId>,
    ) -> Option<(oneshot::Receiver<serde_json::Value>, Option<u64>)> {
        let (tx, rx) = oneshot::channel();
        let mut state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::register_with_upstream");

        // Prevent duplicate registration
        if state.pending.contains_key(&downstream_id) {
            return None;
        }
        let starts_liveness = state
            .pending
            .values()
            .all(|pending| pending.delivery == RequestDelivery::CancelledQueued);
        let liveness_epoch = starts_liveness.then(|| {
            state.liveness_epoch = state.liveness_epoch.wrapping_add(1);
            state.liveness_epoch
        });

        state.pending.insert(
            downstream_id,
            PendingRequest {
                response_tx: tx,
                delivery: RequestDelivery::Queued,
            },
        );

        // Store bidirectional mapping if upstream_id is provided. Appending
        // (not inserting) keeps every concurrent downstream request for this
        // upstream id cancellable, not just the latest one.
        if let Some(upstream) = upstream_id {
            state
                .upstream_to_downstream
                .entry(upstream.clone())
                .or_default()
                .push(downstream_id);
            state.downstream_to_upstream.insert(downstream_id, upstream);
        }

        Some((rx, liveness_epoch))
    }

    pub(crate) fn liveness_epoch(&self) -> u64 {
        self.state
            .lock()
            .recover_poison("ResponseRouter::liveness_epoch")
            .liveness_epoch
    }

    /// Look up every in-flight downstream request ID for an upstream request
    /// ID, in registration order (empty when none).
    ///
    /// Used by `$/cancelRequest` forwarding to translate the client's request ID
    /// to the language server's. Does NOT remove the entries — a cancelled
    /// request must still receive its response.
    #[cfg(test)]
    pub(crate) fn lookup_downstream_ids(&self, upstream_id: &UpstreamId) -> Vec<RequestId> {
        let state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::lookup_downstream_ids");
        state
            .upstream_to_downstream
            .get(upstream_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Mark every queued request for `upstream_id` so the writer skips it, and
    /// return only IDs whose write already started or completed and therefore
    /// still require a downstream `$/cancelRequest` notification.
    pub(crate) fn prepare_cancel_by_upstream(
        &self,
        upstream_id: &UpstreamId,
    ) -> (bool, Vec<RequestId>) {
        let mut state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::prepare_cancel_by_upstream");
        let Some(ids) = state.upstream_to_downstream.get(upstream_id).cloned() else {
            return (false, Vec::new());
        };
        let mut sent = Vec::new();
        for id in ids {
            let Some(pending) = state.pending.get_mut(&id) else {
                continue;
            };
            match pending.delivery {
                RequestDelivery::Queued => {
                    pending.delivery = RequestDelivery::CancelledQueued;
                }
                RequestDelivery::Writing | RequestDelivery::Sent => sent.push(id),
                RequestDelivery::CancelledQueued => {}
            }
        }
        (true, sent)
    }

    /// Atomically claim a tracked request immediately before the writer starts
    /// its bytes. Returns `false` for a request cancelled or removed while it
    /// was queued, so no orphan request reaches the downstream server.
    pub(crate) fn claim_for_write(&self, id: RequestId) -> bool {
        let mut state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::claim_for_write");
        let Some(pending) = state.pending.get_mut(&id) else {
            return false;
        };
        match pending.delivery {
            RequestDelivery::Queued => {
                pending.delivery = RequestDelivery::Writing;
                return true;
            }
            RequestDelivery::Writing | RequestDelivery::Sent => return false,
            RequestDelivery::CancelledQueued => {}
        }

        let pending = state.pending.remove(&id);
        Self::remove_cancel_mapping_inner(&mut state, id);
        drop(state);
        if let Some(pending) = pending {
            let _ = pending.response_tx.send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.as_i64(),
                "error": {
                    "code": -32800,
                    "message": "bridge: request cancelled before downstream write"
                }
            }));
        }
        false
    }

    /// Record successful completion of the writer-side request write.
    pub(crate) fn mark_sent(&self, id: RequestId) {
        let mut state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::mark_sent");
        if let Some(pending) = state.pending.get_mut(&id)
            && pending.delivery == RequestDelivery::Writing
        {
            pending.delivery = RequestDelivery::Sent;
        }
    }

    /// Route a response to its pending request, also cleaning up both cancel-map
    /// directions for this ID in O(1).
    pub(crate) fn route(&self, response: serde_json::Value) -> RouteResult {
        let id = match RequestId::from_json(&response) {
            Some(id) => id,
            None => return RouteResult::NotFound, // Not a response (notification), skip
        };

        let mut state = self.state.lock().recover_poison("ResponseRouter::route");
        let pending = state.pending.remove(&id);

        // Clean up bidirectional cancel map entries in O(1)
        Self::remove_cancel_mapping_inner(&mut state, id);

        match pending {
            Some(pending) => {
                if pending.response_tx.send(response).is_ok() {
                    RouteResult::Delivered
                } else {
                    RouteResult::ReceiverDropped
                }
            }
            None => RouteResult::NotFound,
        }
    }

    /// Remove both cancel-map directions for a downstream request ID.
    ///
    /// Only this downstream id leaves the upstream's list — completing one
    /// request must not orphan the cancel mappings of its still-in-flight
    /// siblings sharing the same upstream id. Caller must hold the lock.
    fn remove_cancel_mapping_inner(state: &mut ResponseRouterState, downstream_id: RequestId) {
        if let Some(upstream_id) = state.downstream_to_upstream.remove(&downstream_id)
            && let Some(ids) = state.upstream_to_downstream.get_mut(&upstream_id)
        {
            ids.retain(|id| *id != downstream_id);
            if ids.is_empty() {
                state.upstream_to_downstream.remove(&upstream_id);
            }
        }
    }

    /// Get the number of pending requests.
    ///
    /// Used for liveness timeout management (ls-bridge-async-connection):
    /// - Timer starts when pending transitions 0 -> 1
    /// - Timer stops when pending transitions to 0
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        let state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::pending_count");
        state.pending.len()
    }

    /// Requests that can still make downstream progress. Cancelled queued
    /// entries remain only until the FIFO writer discards them.
    pub(crate) fn awaiting_downstream_count(&self) -> usize {
        let state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::awaiting_downstream_count");
        state
            .pending
            .values()
            .filter(|pending| pending.delivery != RequestDelivery::CancelledQueued)
            .count()
    }

    /// Remove a pending request without sending a response.
    ///
    /// Used for cleanup when a request fails before being sent to the downstream server.
    /// Also cleans up the bidirectional cancel map entries in O(1).
    ///
    /// Returns `true` if the request was removed, `false` if it wasn't pending.
    pub(crate) fn remove(&self, id: RequestId) -> bool {
        let mut state = self.state.lock().recover_poison("ResponseRouter::remove");
        let removed = state.pending.remove(&id).is_some();

        if removed {
            Self::remove_cancel_mapping_inner(&mut state, id);
        }

        removed
    }

    /// Fail a single pending request with an error response.
    ///
    /// Called when a write fails or the connection is closing (ls-bridge-message-ordering). Uses
    /// `REQUEST_FAILED` (-32803) for queue/write errors, distinct from the
    /// `INTERNAL_ERROR` (-32603) `fail_all()` uses for connection failures.
    ///
    /// Idempotent: subsequent calls for the same ID return `false`. This is critical
    /// for avoiding double-cleanup races between sender and writer task cleanup
    /// (ls-bridge-message-ordering Appendix A).
    pub(crate) fn fail_request(&self, id: RequestId, reason: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::fail_request");

        let pending = state.pending.remove(&id);

        // Clean up bidirectional cancel map entries in O(1)
        Self::remove_cancel_mapping_inner(&mut state, id);

        // Release lock before sending to avoid holding it during channel operations
        drop(state);

        match pending {
            Some(pending) => {
                let error_response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id.as_i64(),
                    "error": {
                        "code": -32803, // REQUEST_FAILED per LSP spec
                        "message": format!("bridge: {}", reason)
                    }
                });
                let _ = pending.response_tx.send(error_response);
                true
            }
            None => false,
        }
    }

    /// Fail every pending request with an internal-error response (called on
    /// reader panic, liveness timeout, …) so each waiter sees a response per
    /// LSP guarantee, and clear both cancel-map directions.
    ///
    /// `LanguageServerPool.upstream_request_registry` is intentionally not
    /// touched here — the router can't see the pool, and stale entries are
    /// harmless because `forward_cancel_by_upstream_id` gates on connection
    /// state and entries get overwritten when the next upstream ID reuses them.
    pub(crate) fn fail_all(&self, error_message: &str) {
        let mut state = self.state.lock().recover_poison("ResponseRouter::fail_all");
        let entries: Vec<_> = state.pending.drain().collect();

        // Clear both cancel map directions
        state.upstream_to_downstream.clear();
        state.downstream_to_upstream.clear();

        // Release lock before sending to avoid holding it during channel operations
        drop(state);

        for (id, pending) in entries {
            let error_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.as_i64(),
                "error": {
                    "code": -32603, // InternalError
                    "message": error_message
                }
            });
            let _ = pending.response_tx.send(error_response);
        }
    }

    /// Atomically fail the connection only when at least one request still
    /// expects downstream progress. A queued request already marked cancelled
    /// is waiting only for the FIFO writer to discard it, so it must not make a
    /// healthy idle server fail its liveness check.
    pub(crate) fn fail_all_if_awaiting_downstream(&self, error_message: &str) -> Option<usize> {
        let mut state = self
            .state
            .lock()
            .recover_poison("ResponseRouter::fail_all_if_awaiting_downstream");
        let awaiting = state
            .pending
            .values()
            .filter(|pending| pending.delivery != RequestDelivery::CancelledQueued)
            .count();
        if awaiting == 0 {
            return None;
        }
        let entries: Vec<_> = state.pending.drain().collect();
        state.upstream_to_downstream.clear();
        state.downstream_to_upstream.clear();
        drop(state);

        for (id, pending) in entries {
            let error_response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id.as_i64(),
                "error": {
                    "code": -32603,
                    "message": error_message
                }
            });
            let _ = pending.response_tx.send(error_response);
        }
        Some(awaiting)
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
    // CancelMap tests (ls-bridge-message-ordering Cancel Forwarding)
    // ========================================

    #[tokio::test]
    async fn cancelled_queued_request_is_not_claimed_for_write() {
        let router = ResponseRouter::new();
        let downstream_id = RequestId::new(42);
        let upstream_id = UpstreamId::Number(100);
        let response = router
            .register_with_upstream(downstream_id, Some(upstream_id.clone()))
            .expect("request registration should succeed");

        let (known, sent_ids) = router.prepare_cancel_by_upstream(&upstream_id);

        assert!(known);
        assert!(
            sent_ids.is_empty(),
            "a request still in the writer queue needs no downstream cancel"
        );
        assert!(
            !router.claim_for_write(downstream_id),
            "the writer must skip a request cancelled before dequeue"
        );
        let cancelled = response.await.expect("queued cancellation should answer");
        assert_eq!(cancelled["error"]["code"], -32800);
    }

    #[test]
    fn cancelled_queued_entry_does_not_hide_new_downstream_work() {
        let router = ResponseRouter::new();
        let cancelled_id = RequestId::new(1);
        let upstream_id = UpstreamId::Number(10);
        let _cancelled_rx = router
            .register_with_upstream(cancelled_id, Some(upstream_id.clone()))
            .unwrap();
        router.prepare_cancel_by_upstream(&upstream_id);

        assert_eq!(router.pending_count(), 1);
        assert_eq!(router.awaiting_downstream_count(), 0);
        let (_live_rx, liveness_epoch) = router
            .register_with_upstream_liveness(RequestId::new(2), None)
            .unwrap();
        assert_eq!(liveness_epoch, Some(2));
        assert_eq!(router.pending_count(), 2);
        assert_eq!(router.awaiting_downstream_count(), 1);
    }

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
        let looked_up = router.lookup_downstream_ids(&upstream_id);
        assert_eq!(
            looked_up,
            vec![downstream_id],
            "lookup should return downstream ID for upstream ID"
        );
    }

    /// One upstream id with several concurrent downstream requests (parallel
    /// injection regions of one client request on the same connection): the
    /// cancel map must track ALL of them, in registration order. The old
    /// single-value map let the latest registration overwrite earlier ones,
    /// so a timeout-driven cancel could target the wrong downstream request
    /// (PR #347 review).
    #[test]
    fn cancel_map_tracks_multiple_downstream_ids_per_upstream() {
        let router = ResponseRouter::new();
        let upstream_id = UpstreamId::Number(100);

        let _rx1 = router
            .register_with_upstream(RequestId::new(1), Some(upstream_id.clone()))
            .expect("first registration should succeed");
        let _rx2 = router
            .register_with_upstream(RequestId::new(2), Some(upstream_id.clone()))
            .expect("second registration should succeed");

        assert_eq!(
            router.lookup_downstream_ids(&upstream_id),
            vec![RequestId::new(1), RequestId::new(2)],
            "both in-flight downstream requests must stay cancellable"
        );
    }

    /// Completing one downstream request must not orphan the cancel mapping
    /// of a still-in-flight sibling sharing the same upstream id. The old
    /// single-value map removed the whole upstream entry when ANY of its
    /// requests completed, leaving the sibling uncancellable.
    #[tokio::test]
    async fn routing_one_response_keeps_sibling_cancel_mappings() {
        let router = ResponseRouter::new();
        let upstream_id = UpstreamId::Number(100);

        let rx1 = router
            .register_with_upstream(RequestId::new(1), Some(upstream_id.clone()))
            .expect("first registration should succeed");
        let _rx2 = router
            .register_with_upstream(RequestId::new(2), Some(upstream_id.clone()))
            .expect("second registration should succeed");

        let result = router.route(json!({"jsonrpc": "2.0", "id": 1, "result": null}));
        assert_eq!(result, RouteResult::Delivered);
        let _ = rx1.await;

        assert_eq!(
            router.lookup_downstream_ids(&upstream_id),
            vec![RequestId::new(2)],
            "sibling's cancel mapping must survive the other request's completion"
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

        let result = router.lookup_downstream_ids(&UpstreamId::Number(999));
        assert!(
            result.is_empty(),
            "lookup should return no ids for unknown upstream ID"
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
            router.lookup_downstream_ids(&upstream_id),
            vec![downstream_id]
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
        assert!(
            router.lookup_downstream_ids(&upstream_id).is_empty(),
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
            router.lookup_downstream_ids(&upstream_id),
            vec![downstream_id]
        );

        // Remove the pending request
        let removed = router.remove(downstream_id);
        assert!(removed, "remove should succeed");

        // Verify mapping is removed
        assert!(
            router.lookup_downstream_ids(&upstream_id).is_empty(),
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
            !router
                .lookup_downstream_ids(&UpstreamId::Number(100))
                .is_empty()
        );
        assert!(
            !router
                .lookup_downstream_ids(&UpstreamId::Number(200))
                .is_empty()
        );

        router.fail_all("connection lost");

        // Verify mappings are cleared
        assert!(
            router
                .lookup_downstream_ids(&UpstreamId::Number(100))
                .is_empty(),
            "cancel map should be cleared by fail_all"
        );
        assert!(
            router
                .lookup_downstream_ids(&UpstreamId::Number(200))
                .is_empty()
        );
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
            router.lookup_downstream_ids(&upstream_id),
            vec![downstream_id]
        );

        // Look up the downstream ID (as we would when forwarding a cancel)
        let looked_up = router.lookup_downstream_ids(&upstream_id);
        assert_eq!(looked_up, vec![downstream_id]);

        // Key assertion: pending entry should still exist after lookup
        assert_eq!(
            router.pending_count(),
            1,
            "lookup_downstream_ids should NOT remove the pending entry"
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
            let result = router.lookup_downstream_ids(&upstream_id);
            assert_eq!(
                result,
                vec![downstream_id],
                "cancel map entry should persist after lookup"
            );
        }
    }

    // ========================================
    // fail_request tests (ls-bridge-message-ordering Single-Writer Loop)
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
    /// ls-bridge-message-ordering Appendix A: Cleanup operations MUST be idempotent to handle
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
        assert!(!router.lookup_downstream_ids(&upstream_id).is_empty());

        // Fail the request
        router.fail_request(downstream_id, "test");

        // Cancel mapping should be cleaned up
        assert!(router.lookup_downstream_ids(&upstream_id).is_empty());
    }

    /// Test double cleanup is safe (both remove and fail_request on same ID).
    ///
    /// ls-bridge-message-ordering Appendix A: When a request fails, cleanup can happen from two places:
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
