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

    /// Atomically admit one downstream request against this upstream request's
    /// exact middleware generation. The dispatch gate is shared with raw
    /// `$/cancelRequest` capture: registration wins and is captured, or cancel
    /// wins and leaves a generation-scoped tombstone that rejects this late
    /// registration before it reaches the downstream writer.
    pub(crate) fn register_downstream_request_if_current(
        &self,
        upstream_id: UpstreamId,
        handle: &Arc<crate::lsp::bridge::ConnectionHandle>,
    ) -> std::io::Result<(
        crate::lsp::bridge::RequestId,
        tokio::sync::oneshot::Receiver<serde_json::Value>,
    )> {
        let _dispatch_guard = self
            .dispatch_gate
            .lock()
            .recover_poison("CancelForwarder::register_downstream_request_if_current");
        let admissible = {
            let state = self
                .subscribers
                .lock()
                .recover_poison("CancelForwarder::register_downstream_request_if_current");
            let Some(generation) = state.active_requests.get(&upstream_id).copied() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "upstream request is no longer active",
                ));
            };
            state.pending_cancellations.get(&upstream_id).copied() != Some(generation)
                && state.delivered_cancellations.get(&upstream_id).copied() != Some(generation)
        };
        if !admissible {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "upstream request was cancelled before downstream admission",
            ));
        }
        self.pool
            .register_request_for_handle_with_upstream(Some(upstream_id), handle)
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
    pub(crate) fn forward_cancel(&self, upstream_id: UpstreamId) -> std::io::Result<()> {
        let generation = self.request_generation(&upstream_id);
        self.forward_cancel_for_generation(upstream_id, generation)
    }

    fn forward_cancel_for_generation(
        &self,
        upstream_id: UpstreamId,
        generation: Option<u64>,
    ) -> std::io::Result<()> {
        let validate_forwarder = self.clone();
        let notify_forwarder = self.clone();
        let validate_id = upstream_id.clone();
        let notify_id = upstream_id.clone();
        self.pool.forward_cancel_by_upstream_id_if_current_sync(
            upstream_id,
            move || validate_forwarder.request_generation(&validate_id) == generation,
            move || {
                notify_forwarder.notify_cancel_for_generation(&notify_id, generation);
            },
        )
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

    /// Atomically release one subscriber and report whether cancellation was
    /// already delivered for the request's active generation. A cancellation
    /// arriving after this lock is released becomes pending for the next
    /// subscriber, so middleware can hand cancellation ownership downstream
    /// without an unsubscribe gap.
    pub(crate) fn unsubscribe_and_take_cancelled(&self, upstream_id: &UpstreamId) -> bool {
        let mut subscribers = self
            .subscribers
            .lock()
            .recover_poison("CancelForwarder::unsubscribe_and_take_cancelled");
        subscribers.subscribers.remove(upstream_id);
        let generation = subscribers.active_requests.get(upstream_id).copied();
        generation.is_some()
            && subscribers
                .delivered_cancellations
                .get(upstream_id)
                .copied()
                == generation
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
        self.notify_cancel_for_generation_with_hook(upstream_id, generation, || {})
    }

    fn notify_cancel_for_generation_with_hook(
        &self,
        upstream_id: &UpstreamId,
        generation: Option<u64>,
        after_registry: impl FnOnce(),
    ) -> bool {
        let handled = {
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
            if let Some(tx) = sender {
                let delivered = tx.send(()).is_ok();
                if let Some(generation) = generation {
                    let cancellations = if delivered {
                        &mut subscribers.delivered_cancellations
                    } else {
                        &mut subscribers.pending_cancellations
                    };
                    cancellations.insert(upstream_id.clone(), generation);
                }
                true
            } else if let Some(generation) = generation {
                subscribers
                    .pending_cancellations
                    .insert(upstream_id.clone(), generation);
                true
            } else {
                false
            }
        };
        after_registry();
        handled
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

    #[cfg(test)]
    pub(crate) fn register_request_for_test(&self, upstream_id: UpstreamId) -> u64 {
        self.register_request(upstream_id)
    }

    #[cfg(test)]
    pub(crate) fn unregister_request_for_test(&self, upstream_id: &UpstreamId, generation: u64) {
        self.unregister_request(upstream_id, generation);
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
        // Everything below runs synchronously inside `call`, which tower-lsp's
        // transport invokes from its single stdin reader task in wire order
        // (`concurrency_level` bounds the returned futures, not `call`). That
        // alone orders generation registration, cancel capture, and tower-lsp's
        // raw-ID cancellation against a reused JSON-RPC ID; no extra lock is
        // needed here, and one would only sit uncontended on the hot path.
        let cancel_forwarder = self.cancel_forwarder.clone();
        // Extract the request ID before delegating
        let request_id = req.id().cloned();

        // Check if this is a $/cancelRequest notification and forward to downstream
        // Per LSP spec, cancel params.id can be either integer or string
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
        let cancel_request = if req.method() == "$/cancelRequest"
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
                Some((forwarder.clone(), upstream_id, generation))
            } else {
                None
            }
        } else {
            None
        };

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

        // tower-lsp applies raw-ID cancellation synchronously inside `call`.
        // Capture downstream targets, notify generation-scoped subscribers, and
        // enqueue downstream cancels first; production registrations retain the
        // exact handles needed to do this without awaiting the connections map.
        if let Some((forwarder, upstream_id, generation)) = cancel_request
            && let Err(error) =
                forwarder.forward_cancel_for_generation(upstream_id.clone(), Some(generation))
        {
            log::debug!(
                target: "kakehashi::cancel",
                "Failed to forward cancel for request {}: {}",
                upstream_id,
                error
            );
        }
        let inner_fut = self.inner.call(req);

        Box::pin(async move {
            let _active_request = active_request;
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
    use crate::lsp::bridge::ConnectionKey;
    use crate::lsp::bridge::ConnectionState;
    use crate::lsp::bridge::test_helpers::create_handle_with_key;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

    #[derive(Clone, Default)]
    struct SynchronousCallService {
        calls: Arc<AtomicUsize>,
        cancel_rx: Arc<std::sync::Mutex<Option<CancelReceiver>>>,
        cancel_was_forwarded_before_inner: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Service<Request> for SynchronousCallService {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request) -> Self::Future {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if req.method() == "$/cancelRequest"
                && let Some(mut cancel_rx) = self.cancel_rx.lock().unwrap().take()
            {
                self.cancel_was_forwarded_before_inner
                    .store(matches!(cancel_rx.try_recv(), Ok(())), Ordering::SeqCst);
            }
            std::future::ready(Ok(None))
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

        // The inner service is still called after synchronous forwarding.
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
    async fn cancelled_dropped_receiver_can_resubscribe_in_same_generation() {
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

        drop(forwarder.subscribe(upstream_id.clone()).unwrap());
        assert!(forwarder.notify_cancel(&upstream_id));
        forwarder
            .subscribe(upstream_id.clone())
            .expect("failed delivery must be retained")
            .await
            .expect("retained cancellation is delivered");

        drop(request_future);
    }

    #[tokio::test]
    async fn failed_delivery_recovery_is_atomic_with_resubscription() {
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
        let generation = forwarder.request_generation(&upstream_id);

        drop(forwarder.subscribe(upstream_id.clone()).unwrap());
        let replacement = std::thread::scope(|scope| {
            let (registry_done_tx, registry_done_rx) = std::sync::mpsc::sync_channel(0);
            let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
            let notify_id = upstream_id.clone();
            let notify_forwarder = forwarder.clone();
            let notify = scope.spawn(move || {
                notify_forwarder.notify_cancel_for_generation_with_hook(
                    &notify_id,
                    generation,
                    || {
                        registry_done_tx.send(()).unwrap();
                        resume_rx.recv().unwrap();
                    },
                )
            });

            registry_done_rx.recv().unwrap();
            let replacement = forwarder.subscribe(upstream_id.clone());
            resume_tx.send(()).unwrap();
            assert!(notify.join().unwrap());
            replacement
        })
        .expect("failed delivery must become pending before resubscription");
        replacement
            .await
            .expect("replacement observes cancellation");

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
            .unwrap();

        assert!(matches!(
            new_request_cancel.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(new_request);
    }

    #[tokio::test]
    async fn middleware_forwards_before_synchronous_inner_cancel() {
        let inner = SynchronousCallService::default();
        let calls = Arc::clone(&inner.calls);
        let cancel_rx_slot = Arc::clone(&inner.cancel_rx);
        let forwarded_before_inner = Arc::clone(&inner.cancel_was_forwarded_before_inner);
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);
        let mut service = RequestIdCapture::with_cancel_forwarder(inner, forwarder.clone());
        let upstream_id = UpstreamId::Number(123);
        let request = || {
            Request::build("textDocument/hover")
                .params(serde_json::json!({}))
                .id(123i64)
                .finish()
        };

        let old_request = service.call(request());
        *cancel_rx_slot.lock().unwrap() = Some(forwarder.subscribe(upstream_id.clone()).unwrap());
        let delayed_cancel = service.call(
            Request::build("$/cancelRequest")
                .params(serde_json::json!({ "id": 123 }))
                .finish(),
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the inner cancel call must run synchronously after forwarding"
        );
        assert!(
            forwarded_before_inner.load(Ordering::SeqCst),
            "generation-scoped forwarding must notify before the inner cancel call"
        );
        drop(old_request);

        let new_request = service.call(request());
        let mut new_request_cancel = forwarder.subscribe(upstream_id).unwrap();
        delayed_cancel.await.unwrap();

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
    async fn cancellation_handoff_observes_delivered_or_retains_later_cancel() {
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(pool);

        let delivered_id = UpstreamId::Number(42);
        let _delivered_guard = ActiveRequestGuard::new(forwarder.clone(), delivered_id.clone());
        let _delivered_rx = forwarder.subscribe(delivered_id.clone()).unwrap();
        assert!(forwarder.notify_cancel(&delivered_id));
        assert!(forwarder.unsubscribe_and_take_cancelled(&delivered_id));

        let later_id = UpstreamId::Number(43);
        let _later_guard = ActiveRequestGuard::new(forwarder.clone(), later_id.clone());
        let _outer_rx = forwarder.subscribe(later_id.clone()).unwrap();
        assert!(!forwarder.unsubscribe_and_take_cancelled(&later_id));
        assert!(forwarder.notify_cancel(&later_id));
        let mut inner_rx = forwarder.subscribe(later_id).unwrap();
        assert!(matches!(inner_rx.try_recv(), Ok(())));
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

    #[tokio::test]
    async fn cancelled_generation_rejects_late_downstream_registration_but_id_reuse_does_not() {
        let pool = Arc::new(LanguageServerPool::new());
        let forwarder = CancelForwarder::new(Arc::clone(&pool));
        let upstream_id = UpstreamId::Number(42);
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("diagnostics"),
        )
        .await;

        let generation = forwarder.register_request(upstream_id.clone());
        forwarder
            .forward_cancel_for_generation_sync(upstream_id.clone(), Some(generation))
            .unwrap();
        let error = forwarder
            .register_downstream_request_if_current(upstream_id.clone(), &handle)
            .expect_err("cancel capture must fence a later provider admission");
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(handle.router().pending_count(), 0);
        assert_eq!(pool.upstream_request_count(&upstream_id), 0);

        forwarder.unregister_request(&upstream_id, generation);
        let replacement_generation = forwarder.register_request(upstream_id.clone());
        assert_ne!(replacement_generation, generation);
        let (request_id, _response) = forwarder
            .register_downstream_request_if_current(upstream_id.clone(), &handle)
            .expect("a replacement generation may reuse the raw JSON-RPC ID");
        assert_eq!(handle.router().pending_count(), 1);
        handle.router().remove(request_id);
        pool.unregister_upstream_request(&upstream_id, handle.key());
        forwarder.unregister_request(&upstream_id, replacement_generation);
    }
}
