//! Tower middleware that stores each incoming LSP request ID in task-local
//! storage so downstream bridge requests can reuse the upstream ID (ls-bridge-server-pool-coordination).
//!
//! Also intercepts `$/cancelRequest`: forwards it to downstream servers via
//! `CancelForwarder`, and notifies any handler that subscribed with
//! `CancelForwarder::subscribe()` via a oneshot so it can abort and return
//! `RequestCancelled`.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde::Deserialize as _;
use tokio::sync::oneshot;
use tower::Service;
use tower_lsp_server::jsonrpc::{Id, Request, Response};

use crate::error::LockResultExt;

use super::bridge::LanguageServerPool;
use super::bridge::UpstreamId;

tokio::task_local! {
    /// Task-local storage for the current upstream request ID.
    ///
    /// This is set by RequestIdCapture before delegating to the inner service,
    /// allowing downstream bridge code to access the original request ID.
    pub static CURRENT_REQUEST_ID: Option<Id>;
}

/// Receiver for cancel notifications.
///
/// This is returned by `CancelForwarder::subscribe()` and can be awaited to receive
/// notification when the request is cancelled. The receiver completes when:
/// - A `$/cancelRequest` notification arrives for this request ID
/// - The sender is dropped (e.g., the request completes normally)
pub(crate) type CancelReceiver = oneshot::Receiver<()>;

/// Returned when a request ID already has an active subscriber: each ID
/// supports only one. Hitting this is a programming error (subscribed twice
/// without unsubscribing). To allow multiple, switch the registry to
/// `HashMap<UpstreamId, Vec<oneshot::Sender<()>>>` and iterate in `notify_cancel`.
#[derive(Debug, Clone)]
pub(crate) struct AlreadySubscribedError(pub(crate) UpstreamId);

impl std::fmt::Display for AlreadySubscribedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request ID {} is already subscribed for cancellation",
            self.0
        )
    }
}

impl std::error::Error for AlreadySubscribedError {}

/// Registry of cancel notification subscribers.
///
/// Tracks accepted requests, handler subscribers, and cancellations that arrived
/// in the short interval between those two events.
#[derive(Default)]
struct CancelSubscriberState {
    subscribers: HashMap<UpstreamId, (Option<u64>, oneshot::Sender<()>)>,
    active_requests: HashMap<UpstreamId, u64>,
    pending_cancellations: HashMap<UpstreamId, u64>,
    delivered_cancellations: HashMap<UpstreamId, u64>,
    next_generation: u64,
}

type CancelSubscriberRegistry = std::sync::Mutex<CancelSubscriberState>;

/// Forwards cancel requests to downstream language servers.
///
/// This type wraps an `Arc<LanguageServerPool>` and provides a method to forward
/// cancel notifications. It is shared between Kakehashi and the RequestIdCapture
/// middleware.
///
/// Additionally, it maintains a registry of subscribers that want to be notified
/// when their request is cancelled. This enables handlers to immediately abort
/// and return `RequestCancelled` error to the client.
///
/// Use `CancelForwarder::new()` within the crate, or `Kakehashi::cancel_forwarder()`
/// to create an instance.
#[derive(Clone)]
pub struct CancelForwarder {
    pool: Arc<LanguageServerPool>,
    /// Registry of subscribers waiting for cancel notifications.
    ///
    /// When a `$/cancelRequest` arrives, we look up the sender and notify it.
    /// The entry is removed when notified or when the subscriber unsubscribes.
    subscribers: Arc<CancelSubscriberRegistry>,
}

impl CancelForwarder {
    /// Create a new cancel forwarder wrapping the given pool.
    pub fn new(pool: Arc<LanguageServerPool>) -> Self {
        Self {
            pool,
            subscribers: Arc::new(std::sync::Mutex::new(CancelSubscriberState::default())),
        }
    }

    /// Forward a cancel request to the downstream server(s) for `upstream_id`,
    /// waking the upstream subscriber (`notify_cancel`) along the way.
    /// Per LSP best-effort semantics, "not forwardable" cases (ID not in registry,
    /// connection not ready) return `Ok(())`; only real I/O write errors are `Err`.
    ///
    /// The subscriber is notified only after the pool has captured the cancel
    /// targets: the woken handler's cleanup (`unregister_all_for_upstream_id`,
    /// dropping request futures) destroys the registry and router state the
    /// forwarding pass reads, so notifying first could silently drop the
    /// downstream `$/cancelRequest` (capture-before-notify; see
    /// `forward_cancel_by_upstream_id_with_notify`).
    #[cfg(test)]
    pub(crate) async fn forward_cancel(&self, upstream_id: UpstreamId) -> std::io::Result<()> {
        let generation = self.request_generation(&upstream_id);
        self.forward_cancel_for_generation(upstream_id, generation)
            .await
    }

    async fn forward_cancel_for_generation(
        &self,
        upstream_id: UpstreamId,
        generation: Option<u64>,
    ) -> std::io::Result<()> {
        let validate_forwarder = self.clone();
        let notify_forwarder = self.clone();
        let validate_id = upstream_id.clone();
        let notify_id = upstream_id.clone();
        self.pool
            .forward_cancel_by_upstream_id_if_current(
                upstream_id,
                move || validate_forwarder.request_generation(&validate_id) == generation,
                move || {
                    notify_forwarder.notify_cancel_for_generation(&notify_id, generation);
                },
            )
            .await
    }

    /// Forward a client `window/workDoneProgress/cancel` to the downstream that
    /// owns the (bridge-minted) progress `token`. Best-effort, like
    /// [`CancelForwarder::forward_cancel`].
    pub(crate) async fn forward_work_done_cancel(
        &self,
        token: tower_lsp_server::ls_types::NumberOrString,
    ) {
        self.pool.forward_work_done_cancel(token).await;
    }

    /// Return a oneshot receiver that fires when `$/cancelRequest` arrives for
    /// `upstream_id`. Race it against the request future with `tokio::select!`.
    ///
    /// Returns [`AlreadySubscribedError`] if a subscriber is already registered
    /// — we reject rather than overwrite so the prior receiver isn't orphaned.
    /// The entry is removed automatically on cancel delivery or `unsubscribe()`.
    pub(crate) fn subscribe(
        &self,
        upstream_id: UpstreamId,
    ) -> Result<CancelReceiver, AlreadySubscribedError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut subscribers = self
                .subscribers
                .lock()
                .recover_poison("CancelForwarder::subscribe");
            let generation = subscribers.active_requests.get(&upstream_id).copied();
            if generation.is_some()
                && subscribers
                    .delivered_cancellations
                    .get(&upstream_id)
                    .copied()
                    == generation
            {
                return Err(AlreadySubscribedError(upstream_id));
            }
            if let Some(generation) = generation
                && subscribers.pending_cancellations.get(&upstream_id).copied() == Some(generation)
            {
                subscribers.pending_cancellations.remove(&upstream_id);
                subscribers
                    .delivered_cancellations
                    .insert(upstream_id, generation);
                let _ = tx.send(());
                return Ok(rx);
            }
            match subscribers.subscribers.entry(upstream_id) {
                Entry::Occupied(entry) => return Err(AlreadySubscribedError(entry.key().clone())),
                Entry::Vacant(entry) => {
                    entry.insert((generation, tx));
                }
            }
        }
        Ok(rx)
    }

    /// Unsubscribe from cancel notifications for an upstream request ID, called on
    /// normal completion to drop the subscriber entry. Otherwise the entry is
    /// cleaned up on cancel delivery or when the `CancelForwarder` is dropped;
    /// calling it after a cancel notification is a harmless no-op.
    pub(crate) fn unsubscribe(&self, upstream_id: &UpstreamId) {
        let mut subscribers = self
            .subscribers
            .lock()
            .recover_poison("CancelForwarder::unsubscribe");
        subscribers.subscribers.remove(upstream_id);
    }

    /// Notify a subscriber that its request was cancelled, or retain the signal
    /// while an accepted request has not subscribed yet. Returns whether the ID
    /// belongs to an active request or subscriber.
    #[cfg(test)]
    pub(crate) fn notify_cancel(&self, upstream_id: &UpstreamId) -> bool {
        let generation = self.request_generation(upstream_id);
        self.notify_cancel_for_generation(upstream_id, generation)
    }

    fn notify_cancel_for_generation(
        &self,
        upstream_id: &UpstreamId,
        generation: Option<u64>,
    ) -> bool {
        let sender = {
            let mut subscribers = self
                .subscribers
                .lock()
                .recover_poison("CancelForwarder::notify_cancel_for_generation");
            if subscribers.active_requests.get(upstream_id).copied() != generation {
                return false;
            }
            if let Some(generation) = generation
                && subscribers
                    .delivered_cancellations
                    .get(upstream_id)
                    .copied()
                    == Some(generation)
            {
                return true;
            }
            let sender = subscribers
                .subscribers
                .remove(upstream_id)
                .filter(|(subscriber_generation, _)| *subscriber_generation == generation)
                .map(|(_, sender)| sender);
            if sender.is_some()
                && let Some(generation) = generation
            {
                subscribers
                    .delivered_cancellations
                    .insert(upstream_id.clone(), generation);
            }
            if sender.is_none()
                && let Some(generation) = generation
            {
                subscribers
                    .pending_cancellations
                    .insert(upstream_id.clone(), generation);
                return true;
            }
            sender
        };
        if let Some(tx) = sender {
            // Send notification (ignore if receiver dropped)
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    fn register_request(&self, upstream_id: UpstreamId) -> u64 {
        let mut state = self
            .subscribers
            .lock()
            .recover_poison("CancelForwarder::register_request");
        let generation = state.next_generation;
        state.next_generation = state.next_generation.wrapping_add(1);
        state.active_requests.insert(upstream_id, generation);
        generation
    }

    fn unregister_request(&self, upstream_id: &UpstreamId, generation: u64) {
        let mut state = self
            .subscribers
            .lock()
            .recover_poison("CancelForwarder::unregister_request");
        if state.active_requests.get(upstream_id) == Some(&generation) {
            state.active_requests.remove(upstream_id);
            state.pending_cancellations.remove(upstream_id);
            state.delivered_cancellations.remove(upstream_id);
            state.subscribers.remove(upstream_id);
        }
    }

    fn request_generation(&self, upstream_id: &UpstreamId) -> Option<u64> {
        self.subscribers
            .lock()
            .recover_poison("CancelForwarder::request_generation")
            .active_requests
            .get(upstream_id)
            .copied()
    }
}

struct ActiveRequestGuard {
    cancel_forwarder: CancelForwarder,
    upstream_id: UpstreamId,
    generation: u64,
}

impl ActiveRequestGuard {
    fn new(cancel_forwarder: CancelForwarder, upstream_id: UpstreamId) -> Self {
        let generation = cancel_forwarder.register_request(upstream_id.clone());
        Self {
            cancel_forwarder,
            upstream_id,
            generation,
        }
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.cancel_forwarder
            .unregister_request(&self.upstream_id, self.generation);
    }
}

/// RAII: drop calls `unsubscribe()`, so an early return on a code path that
/// took a cancel subscription doesn't leak it. `unsubscribe()` is idempotent,
/// so cancel-then-drop is safe.
pub(crate) struct CancelSubscriptionGuard<'a> {
    cancel_forwarder: &'a CancelForwarder,
    upstream_id: UpstreamId,
}

impl<'a> CancelSubscriptionGuard<'a> {
    /// Create a new guard that will unsubscribe on drop.
    pub(crate) fn new(cancel_forwarder: &'a CancelForwarder, upstream_id: UpstreamId) -> Self {
        Self {
            cancel_forwarder,
            upstream_id,
        }
    }
}

impl Drop for CancelSubscriptionGuard<'_> {
    fn drop(&mut self) {
        self.cancel_forwarder.unsubscribe(&self.upstream_id);
    }
}

/// Tower Service wrapper that captures request IDs from incoming LSP requests.
///
/// This middleware extracts the request ID from each incoming request and stores
/// it in task-local storage before delegating to the inner service. This allows
/// bridge code to access the upstream request ID when making downstream requests.
///
/// Additionally, it intercepts `$/cancelRequest` notifications and forwards them
/// to downstream language servers via the `CancelForwarder`.
pub struct RequestIdCapture<S> {
    inner: S,
    cancel_forwarder: Option<CancelForwarder>,
}

impl<S> RequestIdCapture<S> {
    /// Create a new RequestIdCapture wrapping the given service.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            cancel_forwarder: None,
        }
    }

    /// Create a new RequestIdCapture with a cancel forwarder.
    ///
    /// The cancel forwarder is used to forward `$/cancelRequest` notifications
    /// to downstream language servers.
    pub fn with_cancel_forwarder(inner: S, cancel_forwarder: CancelForwarder) -> Self {
        Self {
            inner,
            cancel_forwarder: Some(cancel_forwarder),
        }
    }
}

impl<S> Service<Request> for RequestIdCapture<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // Extract the request ID before delegating
        let request_id = req.id().cloned();

        // Check if this is a $/cancelRequest notification and forward to downstream
        // Per LSP spec, cancel params.id can be either integer or string
        let cancel_forwarder = self.cancel_forwarder.clone();
        let is_cancel_notification = matches!(
            req.method(),
            "$/cancelRequest" | "window/workDoneProgress/cancel"
        );
        let active_request = if is_cancel_notification {
            None
        } else {
            cancel_forwarder.clone().and_then(|forwarder| {
                let upstream_id = match request_id.as_ref()? {
                    Id::Number(id) => UpstreamId::Number(*id),
                    Id::String(id) => UpstreamId::String(id.clone()),
                    Id::Null => return None,
                };
                Some(ActiveRequestGuard::new(forwarder, upstream_id))
            })
        };
        if req.method() == "$/cancelRequest"
            && let Some(forwarder) = cancel_forwarder.as_ref()
            && let Some(params) = req.params()
        {
            // Extract the ID as either numeric or string (per LSP spec: integer | string)
            let id_to_cancel = params
                .get("id")
                .and_then(|v| v.as_i64())
                .map(UpstreamId::Number)
                .or_else(|| {
                    params
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| UpstreamId::String(s.to_string()))
                });

            if let Some(upstream_id) = id_to_cancel
                && let Some(generation) = forwarder.request_generation(&upstream_id)
            {
                let forwarder = forwarder.clone();
                // Fire-and-forget: spawn without tracking JoinHandle.
                //
                // This is intentional for $/cancelRequest because:
                // 1. LSP notifications don't expect responses (fire-and-forget by spec)
                // 2. Cancel is "best effort" - failures are logged but non-fatal
                // 3. We must not block the main request flow
                // 4. Graceful shutdown doesn't need to wait for cancels - the downstream
                //    server will clean up its own state when it shuts down
                //
                // Upstream subscribers are notified inside forward_cancel, AFTER
                // it captures the downstream targets — notifying here first would
                // let the woken handler tear down the registry/router state the
                // forwarding pass is about to read (capture-before-notify).
                tokio::spawn(async move {
                    if let Err(e) = forwarder
                        .forward_cancel_for_generation(upstream_id.clone(), Some(generation))
                        .await
                    {
                        // Log the error but don't fail - cancel forwarding is best-effort
                        log::debug!(
                            target: "kakehashi::cancel",
                            "Failed to forward cancel for request {}: {}",
                            upstream_id,
                            e
                        );
                    }
                });
            }
        }

        // Intercept window/workDoneProgress/cancel and route it to the downstream
        // that owns the progress token (window-work-done-progress bridging).
        // Like $/cancelRequest above, this is a client notification we mirror
        // downstream while still delegating to the inner service.
        if req.method() == "window/workDoneProgress/cancel"
            && let Some(forwarder) = cancel_forwarder.as_ref()
            && let Some(params) = req.params()
            && let Ok(token) =
                tower_lsp_server::ls_types::NumberOrString::deserialize(&params["token"])
        {
            let forwarder = forwarder.clone();
            // Fire-and-forget: cancel is a best-effort notification (same
            // rationale as $/cancelRequest forwarding above).
            tokio::spawn(async move {
                forwarder.forward_work_done_cancel(token).await;
            });
        }

        // Call inner service and get the future
        let inner_fut = self.inner.call(req);

        Box::pin(async move {
            let _active_request = active_request;
            // Set the task-local request ID and await the inner future
            CURRENT_REQUEST_ID.scope(request_id, inner_fut).await
        })
    }
}

/// Get the current request ID from task-local storage.
///
/// Returns None if called outside of a request context or if the request was
/// a notification (which has no ID).
fn get_current_request_id() -> Option<Id> {
    CURRENT_REQUEST_ID.try_with(|id| id.clone()).ok().flatten()
}

/// Extract the upstream request ID from task-local storage.
///
/// Converts the tower-lsp `Id` (set by RequestIdCapture middleware) into
/// our domain `UpstreamId`. Returns `None` for null or missing IDs.
pub(crate) fn current_upstream_id() -> Option<UpstreamId> {
    match get_current_request_id() {
        Some(Id::Number(n)) => Some(UpstreamId::Number(n)),
        Some(Id::String(s)) => Some(UpstreamId::String(s)),
        None | Some(Id::Null) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Mock service that records whether it was called and captures the request ID
    /// from task-local storage during the call.
    #[derive(Clone)]
    struct MockService {
        captured_id: Arc<Mutex<Option<Option<Id>>>>,
    }

    impl MockService {
        fn new() -> Self {
            Self {
                captured_id: Arc::new(Mutex::new(None)),
            }
        }

        async fn get_captured_id(&self) -> Option<Option<Id>> {
            self.captured_id.lock().await.clone()
        }
    }

    impl Service<Request> for MockService {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request) -> Self::Future {
            let captured_id = Arc::clone(&self.captured_id);
            Box::pin(async move {
                // Capture the current request ID from task-local storage
                let id = get_current_request_id();
                *captured_id.lock().await = Some(id);
                Ok(None)
            })
        }
    }

    #[tokio::test]
    async fn captures_numeric_request_id() {
        let mock = MockService::new();
        let mut service = RequestIdCapture::new(mock.clone());

        // Create a request with numeric ID
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id(42i64)
            .finish();

        // Call the service
        let _ = service.call(request).await;

        // Verify the ID was captured
        let captured = mock.get_captured_id().await;
        assert_eq!(captured, Some(Some(Id::Number(42))));
    }

    #[tokio::test]
    async fn captures_string_request_id() {
        let mock = MockService::new();
        let mut service = RequestIdCapture::new(mock.clone());

        // Create a request with string ID
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id("test-id-123")
            .finish();

        // Call the service
        let _ = service.call(request).await;

        // Verify the ID was captured
        let captured = mock.get_captured_id().await;
        assert_eq!(captured, Some(Some(Id::String("test-id-123".to_string()))));
    }

    #[tokio::test]
    async fn handles_notification_without_id() {
        let mock = MockService::new();
        let mut service = RequestIdCapture::new(mock.clone());

        // Create a notification (no ID)
        let notification = Request::build("initialized")
            .params(serde_json::json!({}))
            .finish();

        // Call the service
        let _ = service.call(notification).await;

        // Verify no ID was captured (notification has None)
        let captured = mock.get_captured_id().await;
        assert_eq!(captured, Some(None));
    }

    #[tokio::test]
    async fn request_id_not_available_outside_context() {
        // Without being inside a request context, ID should be None
        let id = get_current_request_id();
        assert_eq!(id, None);
    }

    // ========================================
    // CancelForwarder tests
    // ========================================

    /// Test that with_cancel_forwarder creates a middleware that forwards cancels.
    ///
    /// We can't easily mock CancelForwarder (it requires a real LanguageServerPool),
    /// so we test that:
    /// 1. The middleware is constructed correctly
    /// 2. Cancel notifications are intercepted (not passed through unchanged)
    /// 3. Non-cancel requests work normally
    #[tokio::test]
    async fn with_cancel_forwarder_passes_non_cancel_requests() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder);

        // Create a hover request (not a cancel)
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id(42i64)
            .finish();

        // Call the service
        let _ = service.call(request).await;

        // Verify the request was passed through and ID captured
        let captured = mock.get_captured_id().await;
        assert_eq!(captured, Some(Some(Id::Number(42))));
    }

    #[tokio::test]
    async fn cancel_notification_is_intercepted() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder);

        // Create a $/cancelRequest notification
        let request = Request::build("$/cancelRequest")
            .params(serde_json::json!({ "id": 123 }))
            .finish();

        // Call the service
        let result = service.call(request).await;

        // The notification should be processed (no error)
        assert!(result.is_ok());

        // The inner service was still called (tower-lsp needs to see it too)
        let captured = mock.get_captured_id().await;
        assert!(captured.is_some(), "Inner service should still be called");

        // Note: We can't verify the forward happened without a real pool setup,
        // but we've verified the middleware processes the cancel notification.
    }

    #[tokio::test]
    async fn cancel_before_handler_subscription_is_delivered() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock, forwarder.clone());
        let upstream_id = UpstreamId::Number(123);

        // `call` means the middleware has accepted this request, but deliberately
        // leave its future unpolled so the handler cannot have subscribed yet.
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id(123i64)
            .finish();
        let request_future = service.call(request);

        assert!(
            forwarder.notify_cancel(&upstream_id),
            "an accepted request must retain cancellation until its handler subscribes"
        );
        let cancel = forwarder.subscribe(upstream_id).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(50), cancel)
            .await
            .expect("retained cancellation should be delivered on subscribe")
            .expect("cancel sender should signal rather than drop");

        drop(request_future);
    }

    #[tokio::test]
    async fn retained_cancel_rejects_a_second_subscription_in_same_request() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock, forwarder.clone());
        let upstream_id = UpstreamId::Number(123);
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id(123i64)
            .finish();
        let request_future = service.call(request);

        assert!(forwarder.notify_cancel(&upstream_id));
        let cancel = forwarder.subscribe(upstream_id.clone()).unwrap();
        cancel.await.expect("retained cancel is delivered");

        let duplicate = forwarder.subscribe(upstream_id.clone());
        assert!(
            matches!(duplicate, Err(AlreadySubscribedError(id)) if id == upstream_id),
            "a cancelled active generation must not create a fresh receiver"
        );

        drop(request_future);
    }

    #[tokio::test]
    async fn repeated_cancel_does_not_recreate_pending_state_after_delivery() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock, forwarder.clone());
        let upstream_id = UpstreamId::Number(123);
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id(123i64)
            .finish();
        let request_future = service.call(request);

        assert!(forwarder.notify_cancel(&upstream_id));
        forwarder
            .subscribe(upstream_id.clone())
            .unwrap()
            .await
            .unwrap();
        assert!(forwarder.notify_cancel(&upstream_id));

        let state = forwarder
            .subscribers
            .lock()
            .recover_poison("repeated cancel test");
        assert!(!state.pending_cancellations.contains_key(&upstream_id));
        drop(state);
        drop(request_future);
    }

    #[tokio::test]
    async fn dropped_request_clears_retained_cancellation() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock, forwarder.clone());
        let upstream_id = UpstreamId::Number(123);
        let request = Request::build("textDocument/hover")
            .params(serde_json::json!({}))
            .id(123i64)
            .finish();
        let request_future = service.call(request);

        assert!(forwarder.notify_cancel(&upstream_id));
        drop(request_future);

        let mut reused_id_cancel = forwarder.subscribe(upstream_id).unwrap();
        assert!(
            matches!(
                reused_id_cancel.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "dropping the accepted request must not poison later reuse of its ID"
        );
    }

    #[tokio::test]
    async fn delayed_cancel_does_not_hit_reused_request_id() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock, forwarder.clone());
        let upstream_id = UpstreamId::Number(123);
        let request = || {
            Request::build("textDocument/hover")
                .params(serde_json::json!({}))
                .id(123i64)
                .finish()
        };

        let old_request = service.call(request());
        let old_generation = forwarder.request_generation(&upstream_id);
        drop(old_request);

        let new_request = service.call(request());
        let mut new_request_cancel = forwarder.subscribe(upstream_id.clone()).unwrap();
        forwarder
            .forward_cancel_for_generation(upstream_id, old_generation)
            .await
            .unwrap();

        assert!(matches!(
            new_request_cancel.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(new_request);
    }

    #[tokio::test]
    async fn cancel_for_unknown_id_is_not_retained() {
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let upstream_id = UpstreamId::String("not-active".to_string());

        assert!(!forwarder.notify_cancel(&upstream_id));
        let mut later_subscription = forwarder.subscribe(upstream_id).unwrap();
        assert!(matches!(
            later_subscription.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn work_done_progress_cancel_is_intercepted() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder);

        // A window/workDoneProgress/cancel notification (token may be int or string).
        let request = Request::build("window/workDoneProgress/cancel")
            .params(serde_json::json!({ "token": "kakehashi/bridge/progress/0" }))
            .finish();

        let result = service.call(request).await;
        assert!(result.is_ok());

        // The inner service is still invoked (tower-lsp must see it too).
        let captured = mock.get_captured_id().await;
        assert!(captured.is_some(), "Inner service should still be called");
    }

    /// End-to-end through the middleware: a client `window/workDoneProgress/cancel`
    /// is routed to the owning downstream's writer with its ORIGINAL token.
    #[tokio::test]
    async fn work_done_progress_cancel_reaches_owning_downstream() {
        use crate::lsp::bridge::OutboundMessage;
        use tower_lsp_server::ls_types::NumberOrString;

        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());

        // Register a downstream token with an observable writer.
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
        let conn = pool.progress_registry().new_connection_id();
        let (upstream_token, _) =
            pool.progress_registry()
                .register(conn, NumberOrString::Number(1), writer_tx);
        let NumberOrString::String(upstream_token) = upstream_token else {
            panic!("upstream token is a string");
        };

        let forwarder = CancelForwarder::new(Arc::clone(&pool));
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder);

        let request = Request::build("window/workDoneProgress/cancel")
            .params(serde_json::json!({ "token": upstream_token }))
            .finish();
        service.call(request).await.unwrap();

        // The middleware spawns the forward fire-and-forget; await its delivery.
        let sent = tokio::time::timeout(std::time::Duration::from_secs(2), writer_rx.recv())
            .await
            .expect("cancel should reach downstream within timeout")
            .expect("writer channel open");
        let OutboundMessage::Untracked(val) = sent else {
            panic!("Expected Untracked");
        };
        assert_eq!(val["method"], "window/workDoneProgress/cancel");
        assert_eq!(val["params"]["token"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn cancel_forwarder_handles_missing_id_in_params() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder);

        // Create a $/cancelRequest with no id parameter (malformed)
        let request = Request::build("$/cancelRequest")
            .params(serde_json::json!({}))
            .finish();

        // Should not crash, just skip forwarding
        let result = service.call(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn malformed_cancel_message_id_does_not_replace_active_generation() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let upstream_id = UpstreamId::Number(123);
        let generation = forwarder.register_request(upstream_id.clone());
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder.clone());

        let request = Request::build("$/cancelRequest")
            .id(123i64)
            .params(serde_json::json!({ "id": 123 }))
            .finish();
        service.call(request).await.unwrap();

        assert_eq!(forwarder.request_generation(&upstream_id), Some(generation));
    }

    #[tokio::test]
    async fn cancel_forwarder_handles_string_id() {
        let mock = MockService::new();
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(mock.clone(), forwarder);

        // Create a $/cancelRequest with string id (supported per LSP 3.17 spec)
        let request = Request::build("$/cancelRequest")
            .params(serde_json::json!({ "id": "string-id" }))
            .finish();

        // Should extract UpstreamId::String and attempt forwarding
        let result = service.call(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn middleware_without_forwarder_ignores_cancel() {
        let mock = MockService::new();
        // Create middleware without cancel forwarder
        let mut service = RequestIdCapture::new(mock.clone());

        // Create a $/cancelRequest notification
        let request = Request::build("$/cancelRequest")
            .params(serde_json::json!({ "id": 123 }))
            .finish();

        // Should work without crash (cancel just isn't forwarded)
        let result = service.call(request).await;
        assert!(result.is_ok());

        // Inner service was still called
        let captured = mock.get_captured_id().await;
        assert!(captured.is_some());
    }

    #[tokio::test]
    async fn subscribe_returns_error_on_duplicate() {
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let upstream_id = UpstreamId::Number(42);

        // First subscription should succeed
        let result1 = forwarder.subscribe(upstream_id.clone());
        assert!(result1.is_ok());

        // Second subscription with same ID should fail
        let result2 = forwarder.subscribe(upstream_id.clone());
        assert!(result2.is_err());

        // Verify error contains the correct ID
        let err = result2.unwrap_err();
        assert!(matches!(err, AlreadySubscribedError(id) if id == upstream_id));
    }

    #[tokio::test]
    async fn subscribe_succeeds_after_unsubscribe() {
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let upstream_id = UpstreamId::Number(42);

        // First subscription
        let _rx1 = forwarder.subscribe(upstream_id.clone()).unwrap();

        // Unsubscribe
        forwarder.unsubscribe(&upstream_id);

        // Second subscription should now succeed
        let result = forwarder.subscribe(upstream_id);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn subscribe_succeeds_after_notify_cancel() {
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let upstream_id = UpstreamId::Number(42);

        // First subscription
        let _rx1 = forwarder.subscribe(upstream_id.clone()).unwrap();

        // Cancel notification removes the subscriber
        let notified = forwarder.notify_cancel(&upstream_id);
        assert!(notified);

        // Second subscription should now succeed
        let result = forwarder.subscribe(upstream_id);
        assert!(result.is_ok());
    }
}
