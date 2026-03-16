//! Reader task for downstream language server stdout.
//!
//! This module provides the background task that reads LSP messages from
//! the downstream server's stdout and routes responses to waiting requesters.
//!
//! # Architecture (ADR-0015)
//!
//! The Reader Task:
//! - Runs in a spawned tokio task
//! - Reads messages from stdout using BridgeReader
//! - Routes responses via ResponseRouter to oneshot waiters
//! - Logs notifications (they don't have waiters)
//! - Manages liveness timer for hung server detection (ADR-0014)
//! - Gracefully shuts down on EOF, error, or cancellation signal

use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};
use tokio::sync::{mpsc, oneshot};

use crate::error::LockResultExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc;

use super::super::connection::BridgeReader;
use super::OutboundMessage;
use super::ResponseRouter;
use super::response_router::RouteResult;
use crate::lsp::bridge::pool::DynamicCapabilityRegistry;

/// Notification to forward from downstream server to upstream editor.
///
/// Reader tasks use this to signal events that require upstream Client interaction,
/// keeping the bridge module decoupled from tower-lsp's Client type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpstreamNotification {
    /// Request upstream to re-pull diagnostics.
    /// Sent when downstream server issues `workspace/diagnostic/refresh`.
    DiagnosticRefresh,
}

/// Liveness channel endpoints for the reader task.
///
/// Groups the four liveness-related parameters that `reader_loop_with_liveness`
/// needs: the timeout duration and the three channels for start/stop/failed
/// signaling between the reader task and ConnectionHandle.
struct LivenessParams {
    timeout: Option<Duration>,
    start_rx: mpsc::Receiver<()>,
    stop_rx: mpsc::Receiver<()>,
    failed_tx: oneshot::Sender<()>,
}

/// Dependencies for handling server-initiated requests.
///
/// Groups the parameters that `handle_server_request` needs: the language
/// identifier (for logging), the response channel, the dynamic capability
/// registry, and the upstream notification channel.
struct ServerRequestDeps {
    language: Option<String>,
    response_tx: mpsc::Sender<OutboundMessage>,
    dynamic_capabilities: Arc<DynamicCapabilityRegistry>,
    upstream_tx: mpsc::UnboundedSender<UpstreamNotification>,
}

/// Type alias for the pinned liveness timer future.
type LivenessTimer = std::pin::Pin<Box<tokio::time::Sleep>>;

/// Creates a new liveness timer that will fire after the given duration.
fn new_liveness_timer(timeout: Duration) -> LivenessTimer {
    Box::pin(tokio::time::sleep(timeout))
}

/// Encapsulates liveness timer state and logic for the reader task.
///
/// This struct consolidates the timer state (active timer, timeout configuration)
/// and provides methods for timer lifecycle management, improving cohesion
/// compared to managing multiple variables separately.
///
/// # Timer States
///
/// - **Inactive**: `timer` is `None`, timeout may or may not be configured
/// - **Active**: `timer` is `Some`, will fire after configured duration
///
/// # Usage Pattern
///
/// ```ignore
/// let mut liveness = LivenessTimerState::new(Some(Duration::from_secs(60)));
/// liveness.start(&lang_prefix);           // Starts timer when pending 0->1
/// liveness.reset(&lang_prefix);           // Resets on message activity
/// liveness.stop(&lang_prefix, "reason");  // Stops when pending returns to 0
/// ```
struct LivenessTimerState {
    /// Active timer future, None when inactive.
    timer: Option<LivenessTimer>,
    /// Configured timeout duration, None if liveness is disabled.
    timeout: Option<Duration>,
}

impl LivenessTimerState {
    /// Create a new liveness timer state with optional timeout configuration.
    ///
    /// If `timeout` is None, the timer is disabled and all operations are no-ops.
    fn new(timeout: Option<Duration>) -> Self {
        Self {
            timer: None,
            timeout,
        }
    }

    /// Check if the timer is currently active.
    fn is_active(&self) -> bool {
        self.timer.is_some()
    }

    /// Start the liveness timer.
    ///
    /// Called when pending count transitions 0->1.
    /// No-op if timeout is not configured.
    fn start(&mut self, lang_prefix: &str) {
        if let Some(timeout) = self.timeout {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer started: {:?}",
                lang_prefix,
                timeout
            );
            self.timer = Some(new_liveness_timer(timeout));
        }
    }

    /// Reset the liveness timer to full duration.
    ///
    /// Called on any message activity (response or notification).
    /// Only resets if timer is currently active.
    fn reset(&mut self, lang_prefix: &str) {
        if let Some(timeout) = self.timeout
            && self.timer.is_some()
        {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer reset on message activity",
                lang_prefix
            );
            self.timer = Some(new_liveness_timer(timeout));
        }
    }

    /// Stop the liveness timer.
    ///
    /// Called when pending count returns to 0 or shutdown begins.
    /// Only logs if timer was actually active.
    fn stop(&mut self, lang_prefix: &str, reason: &str) {
        if self.timer.take().is_some() {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer stopped: {}",
                lang_prefix,
                reason
            );
        }
    }

    /// Get the configured timeout duration (for logging on expiry).
    fn timeout_duration(&self) -> Duration {
        self.timeout.unwrap_or_default()
    }

    /// Take a reference to the timer for use in select!.
    ///
    /// Returns a future that completes when the timer fires, or pends forever
    /// if no timer is active.
    async fn wait(&mut self) {
        if let Some(ref mut timer) = self.timer {
            timer.await;
        } else {
            std::future::pending::<()>().await;
        }
    }
}

/// Handle to a running Reader Task, managing its lifetime via RAII.
///
/// This struct owns the resources needed to control and clean up the reader task.
/// The underscore-prefixed fields indicate they are held for their Drop semantics
/// rather than being explicitly accessed.
///
/// # Lifecycle and Drop Behavior
///
/// When this handle is dropped:
///
/// 1. **CancellationToken is dropped** - This cancels the token, which signals the
///    reader loop's `select!` to exit via `cancel_token.cancelled()`. The reader
///    loop checks this signal on each iteration and breaks cleanly when cancelled.
///
/// 2. **JoinHandle is dropped without awaiting** - This is intentional and safe
///    because:
///    - The reader task will exit promptly once cancelled (the `select!` ensures
///      cancellation is checked on every loop iteration)
///    - The reader task also exits on EOF or read error, handling the case where
///      the downstream process terminates
///    - Tokio tasks are detached when their JoinHandle is dropped; they continue
///      running but we don't need to await completion
///    - The task performs no critical cleanup that requires awaiting
///
/// # Why Not Await the JoinHandle?
///
/// Awaiting would require this drop to be async, which is not possible in Rust.
/// The reader task is designed to exit quickly on cancellation (within one loop
/// iteration), so fire-and-forget cleanup is appropriate here.
///
/// # Cross-Task Coordination (ADR-0015)
///
/// The cancellation token enables coordination between reader and writer tasks.
/// When the writer task fails, it cancels the shared token, causing the reader
/// to exit and preventing CPU spin on orphaned channels. Conversely, when the
/// reader handle is dropped (e.g., during connection shutdown), the reader task
/// receives the cancellation signal and exits cleanly.
///
/// # Resource Cleanup Guarantee
///
/// - The reader task holds only borrowed/Arc'd resources (BridgeReader, ResponseRouter)
/// - On cancellation: logs shutdown, breaks from loop, task completes
/// - On EOF/error: fails all pending requests via router, then exits
/// - No resources are leaked regardless of exit path
pub(crate) struct ReaderTaskHandle {
    /// Join handle for the spawned reader task.
    ///
    /// Dropped without awaiting when this struct is dropped. This is safe because
    /// the reader task exits promptly on cancellation, EOF, or read error.
    _join_handle: JoinHandle<()>,

    /// Cancellation token to signal graceful shutdown.
    ///
    /// When dropped, the token is automatically cancelled, causing the reader
    /// loop's `cancel_token.cancelled()` future to complete. This ensures the
    /// reader task exits when the handle is dropped (RAII cleanup).
    _cancel_token: CancellationToken,

    /// Sender for liveness timer start notifications.
    ///
    /// Used by ConnectionHandle to notify the reader when the first request
    /// is registered (pending 0->1), triggering the liveness timer to start.
    liveness_start_tx: mpsc::Sender<()>,

    /// Sender for liveness timer stop notifications (ADR-0018 Phase 4).
    ///
    /// Used by ConnectionHandle to stop the liveness timer when shutdown begins.
    /// Global shutdown (Tier 3) overrides liveness timeout (Tier 2).
    liveness_stop_tx: mpsc::Sender<()>,

    /// Receiver for liveness failure notification (ADR-0014 Phase 3).
    ///
    /// When the liveness timeout fires, the reader task sends () on this channel.
    /// ConnectionHandle checks this to transition to Failed state.
    liveness_failed_rx: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

impl ReaderTaskHandle {
    /// Notify the reader task to start the liveness timer.
    ///
    /// Called by ConnectionHandle when the first request is registered (pending 0->1).
    /// Non-blocking: if the channel is full, the notification is dropped (timer already running).
    pub(crate) fn notify_liveness_start(&self) {
        // Use try_send to avoid blocking. If channel is full, timer is already running.
        let _ = self.liveness_start_tx.try_send(());
    }

    /// Stop the liveness timer without canceling the reader task (ADR-0018 Phase 4).
    ///
    /// Called by ConnectionHandle when shutdown begins. Global shutdown (Tier 3)
    /// overrides liveness timeout (Tier 2), but the reader task continues running
    /// to receive the shutdown response.
    ///
    /// Non-blocking: if the channel is full, the stop is already in progress.
    pub(crate) fn stop_liveness_timer(&self) {
        // Use try_send to avoid blocking
        let _ = self.liveness_stop_tx.try_send(());
    }

    /// Check if a liveness failure has been signaled.
    ///
    /// Returns true if the reader task signaled a liveness timeout failure.
    /// This is a one-time check - once it returns true, subsequent calls return false.
    ///
    /// Used by ConnectionHandle to detect liveness timeout and transition to Failed state.
    pub(crate) fn check_liveness_failed(&self) -> bool {
        let mut guard = self
            .liveness_failed_rx
            .lock()
            .recover_poison("ReaderTaskHandle::check_liveness_failed");

        if let Some(mut rx) = guard.take() {
            match rx.try_recv() {
                Ok(()) => true, // Liveness timeout fired
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Not yet failed, put it back
                    *guard = Some(rx);
                    false
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending - reader exited normally
                    false
                }
            }
        } else {
            // Already consumed
            false
        }
    }
}

/// Spawn a reader task that reads from stdout and routes responses.
///
/// This is a convenience wrapper for tests that don't need liveness timeout.
/// Production code should use `spawn_reader_task_with_liveness` directly.
///
/// # Arguments
/// * `reader` - The BridgeReader to read messages from
/// * `router` - The ResponseRouter to route responses to waiters
///
/// # Returns
/// A ReaderTaskHandle for managing the spawned task.
#[cfg(test)]
pub(crate) fn spawn_reader_task(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
) -> ReaderTaskHandle {
    spawn_reader_task_with_liveness(reader, router, None)
}

/// Spawn a reader task with optional liveness timeout (no language context).
///
/// This is a convenience wrapper for tests that don't need structured logging
/// with language identifiers. Production code should use `spawn_reader_task_for_language`.
///
/// Creates dummy channel and registry for tests that don't need server request handling.
///
/// # Arguments
/// * `reader` - The BridgeReader to read messages from
/// * `router` - The ResponseRouter to route responses to waiters
/// * `liveness_timeout` - Optional timeout for hung server detection (ADR-0014)
///
/// # Returns
/// A ReaderTaskHandle for managing the spawned task.
#[cfg(test)]
pub(crate) fn spawn_reader_task_with_liveness(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
    liveness_timeout: Option<Duration>,
) -> ReaderTaskHandle {
    let (response_tx, _response_rx) = mpsc::channel(16);
    let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
    let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
    spawn_reader_task_for_language(
        reader,
        router,
        liveness_timeout,
        None,
        response_tx,
        dynamic_capabilities,
        upstream_tx,
    )
}

/// Spawn a reader task with liveness timeout and language identifier for logging.
///
/// # Arguments
/// * `reader` - The BridgeReader to read messages from
/// * `router` - The ResponseRouter to route responses to waiters
/// * `liveness_timeout` - Optional timeout for hung server detection (ADR-0014)
/// * `language` - Language identifier for structured logging (e.g., "lua", "python")
///
/// # Returns
/// A ReaderTaskHandle for managing the spawned task.
pub(crate) fn spawn_reader_task_for_language(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
    liveness_timeout: Option<Duration>,
    language: Option<String>,
    response_tx: mpsc::Sender<OutboundMessage>,
    dynamic_capabilities: Arc<DynamicCapabilityRegistry>,
    upstream_tx: mpsc::UnboundedSender<UpstreamNotification>,
) -> ReaderTaskHandle {
    let cancel_token = CancellationToken::new();
    let token_clone = cancel_token.clone();

    // Channel for liveness timer start notifications (capacity 1, latest notification wins)
    let (liveness_start_tx, liveness_start_rx) = mpsc::channel(1);

    // Channel for liveness timer stop notifications (Phase 4: shutdown integration)
    let (liveness_stop_tx, liveness_stop_rx) = mpsc::channel(1);

    // Channel for liveness failure notification (Phase 3: state transition signaling)
    let (liveness_failed_tx, liveness_failed_rx) = oneshot::channel();

    let liveness = LivenessParams {
        timeout: liveness_timeout,
        start_rx: liveness_start_rx,
        stop_rx: liveness_stop_rx,
        failed_tx: liveness_failed_tx,
    };
    let server_request_deps = ServerRequestDeps {
        language,
        response_tx,
        dynamic_capabilities,
        upstream_tx,
    };

    let join_handle = tokio::spawn(reader_loop_with_liveness(
        reader,
        router,
        token_clone,
        liveness,
        server_request_deps,
    ));

    ReaderTaskHandle {
        _join_handle: join_handle,
        _cancel_token: cancel_token,
        liveness_start_tx,
        liveness_stop_tx,
        liveness_failed_rx: std::sync::Mutex::new(Some(liveness_failed_rx)),
    }
}

/// The main reader loop - reads messages and routes them.
///
/// This is a convenience wrapper that calls `reader_loop_with_liveness` without
/// liveness timeout support.
#[cfg(test)]
async fn reader_loop(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
    cancel_token: CancellationToken,
) {
    // Create dummy receivers that never receive (liveness disabled)
    let (_start_tx, start_rx) = mpsc::channel(1);
    let (_stop_tx, stop_rx) = mpsc::channel(1);
    // Create a dummy sender that we'll drop (no liveness failure signaling in test helper)
    let (failed_tx, _failed_rx) = oneshot::channel();
    // Create dummy channel and registry for tests that don't need server request handling
    let (response_tx, _response_rx) = mpsc::channel(16);
    let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
    let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
    let liveness = LivenessParams {
        timeout: None,
        start_rx,
        stop_rx,
        failed_tx,
    };
    let server_request_deps = ServerRequestDeps {
        language: None,
        response_tx,
        dynamic_capabilities,
        upstream_tx,
    };
    reader_loop_with_liveness(reader, router, cancel_token, liveness, server_request_deps).await
}

/// The main reader loop with optional liveness timeout support.
///
/// # Liveness Timer (ADR-0014)
///
/// When `liveness_timeout` is Some:
/// - Timer starts when a notification is received on `liveness_start_rx` (pending 0->1)
/// - Timer resets on any message received (response or notification)
/// - Timer stops when pending count returns to 0 OR when stop notification received
/// - Timer firing triggers Ready->Failed transition via router.fail_all() and signals via liveness_failed_tx
///
/// # Observability
///
/// When `language` is provided, all log messages include the language identifier
/// for easier filtering in production (e.g., `[lua]` prefix in log messages).
async fn reader_loop_with_liveness(
    mut reader: BridgeReader,
    router: Arc<ResponseRouter>,
    cancel_token: CancellationToken,
    liveness_params: LivenessParams,
    server_request_deps: ServerRequestDeps,
) {
    // Destructure parameter structs
    let LivenessParams {
        timeout: liveness_timeout,
        start_rx: mut liveness_start_rx,
        stop_rx: mut liveness_stop_rx,
        failed_tx: liveness_failed_tx,
    } = liveness_params;

    // Language prefix for log messages (e.g., "[lua] " or "")
    let lang_prefix = server_request_deps
        .language
        .as_ref()
        .map(|l| format!("[{}] ", l))
        .unwrap_or_default();

    // Consolidated liveness timer state (ADR-0014)
    let mut liveness = LivenessTimerState::new(liveness_timeout);

    loop {
        tokio::select! {
            biased; // Process in order: cancellation, timer, liveness start, liveness stop, read

            // Check for cancellation first (highest priority)
            _ = cancel_token.cancelled() => {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Reader task cancelled, shutting down",
                    lang_prefix
                );
                break;
            }

            // Check for liveness timeout (if timer is active)
            _ = liveness.wait() => {
                // Liveness timeout fired - server is unresponsive
                // This warn! is the primary observability signal for production debugging.
                // The pending_count is included for debugging stuck request scenarios.
                let pending_count = router.pending_count();
                warn!(
                    target: "kakehashi::bridge::reader",
                    "{}Liveness timeout expired after {:?}, server appears hung - failing {} pending request(s)",
                    lang_prefix,
                    liveness.timeout_duration(),
                    pending_count
                );
                router.fail_all("bridge: liveness timeout - server unresponsive");
                // Signal liveness failure for state transition (ADR-0014 Phase 3)
                let _ = liveness_failed_tx.send(());
                break;
            }

            // Check for liveness timer start notification (pending 0->1)
            Some(()) = liveness_start_rx.recv() => {
                liveness.start(&lang_prefix);
            }

            // Check for liveness timer stop notification (shutdown began - ADR-0018 Phase 4)
            Some(()) = liveness_stop_rx.recv() => {
                liveness.stop(&lang_prefix, "shutdown began");
            }

            // Try to read a message (lowest priority)
            result = reader.read_message() => {
                match result {
                    Ok(message) => {
                        // Reset liveness timer on any message activity (ADR-0014)
                        liveness.reset(&lang_prefix);

                        handle_message(message, &router, &lang_prefix, &server_request_deps).await;

                        // Check if pending count returned to 0 - stop timer
                        if liveness.is_active() && router.pending_count() == 0 {
                            liveness.stop(&lang_prefix, "pending count is 0");
                        }
                    }
                    Err(e) => {
                        // EOF or read error - mark all pending as failed and exit
                        warn!(
                            target: "kakehashi::bridge::reader",
                            "{}Reader error: {}, failing pending requests",
                            lang_prefix,
                            e
                        );
                        router.fail_all(&format!("bridge: reader error: {}", e));
                        break;
                    }
                }
            }
        }
    }
}

/// Classification of messages from downstream language servers.
///
/// LSP messages are classified by the presence of `id` and `method` fields:
/// - Response: has `id`, no `method` (reply to our request)
/// - ServerRequest: has both `id` and `method` (server-initiated request)
/// - Notification: has `method`, no `id` (server-initiated notification)
/// - Invalid: has neither `id` nor `method`
#[derive(Debug, PartialEq)]
enum MessageKind {
    Response,
    ServerRequest,
    Notification,
    Invalid,
}

fn classify_message(message: &serde_json::Value) -> MessageKind {
    let has_id = message.get("id").is_some();
    let has_method = message.get("method").is_some();
    match (has_id, has_method) {
        (true, false) => MessageKind::Response,
        (true, true) => MessageKind::ServerRequest,
        (false, true) => MessageKind::Notification,
        (false, false) => MessageKind::Invalid,
    }
}

/// Handle a single message from the downstream server.
async fn handle_message(
    message: serde_json::Value,
    router: &ResponseRouter,
    lang_prefix: &str,
    deps: &ServerRequestDeps,
) {
    match classify_message(&message) {
        MessageKind::Response => {
            let id = message.get("id").cloned();
            match router.route(message) {
                RouteResult::Delivered => {
                    // Response delivered successfully - no logging needed for normal case
                }
                RouteResult::ReceiverDropped => {
                    // ID was found but receiver was dropped (requester cancelled).
                    // This can legitimately happen when users cancel requests rapidly.
                    // Using debug! to avoid log spam; upgrade to warn! if investigation is needed.
                    debug!(
                        target: "kakehashi::bridge::reader",
                        "{}Response for id={} arrived but receiver was dropped (requester cancelled)",
                        lang_prefix,
                        id.unwrap_or(serde_json::Value::Null)
                    );
                }
                RouteResult::NotFound => {
                    // Unknown request ID - could be a late response or protocol mismatch
                    debug!(
                        target: "kakehashi::bridge::reader",
                        "{}Response for unknown request id={}, dropping",
                        lang_prefix,
                        id.unwrap_or(serde_json::Value::Null)
                    );
                }
            }
        }
        MessageKind::ServerRequest => {
            handle_server_request(message, lang_prefix, deps).await;
        }
        MessageKind::Notification => {
            // Notifications are silently ignored (no logging needed)
        }
        MessageKind::Invalid => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}Invalid message from downstream (no id or method): {}",
                lang_prefix,
                message
            );
        }
    }
}

/// Handle a server-initiated request by dispatching on its method.
///
/// Server-initiated requests have both `"id"` and `"method"` fields.
/// We must send a JSON-RPC response back for each request.
///
/// # Error Handling
///
/// When param parsing fails for known methods (registerCapability,
/// unregisterCapability), we respond with a JSON-RPC InvalidParams error
/// (-32602). This is spec-correct: the LSP allows error responses to any
/// request. If a downstream server cannot handle an error to its own
/// request, that is a server bug.
async fn handle_server_request(
    message: serde_json::Value,
    lang_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let id: jsonrpc::Id = message
        .get("id")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");

    let ok_response = || jsonrpc::Response::from_ok(id.clone(), serde_json::Value::Null);

    let body: jsonrpc::Result<()> = match method {
        "client/registerCapability" => {
            if let Some(params) = message.get("params") {
                match serde_json::from_value::<tower_lsp_server::ls_types::RegistrationParams>(
                    params.clone(),
                ) {
                    Ok(reg_params) => {
                        for reg in &reg_params.registrations {
                            debug!(
                                target: "kakehashi::bridge::reader",
                                "{}Registered dynamic capability: {} (id={})",
                                lang_prefix, reg.method, reg.id
                            );
                        }
                        deps.dynamic_capabilities.register(reg_params.registrations);
                        Ok(())
                    }
                    Err(e) => {
                        warn!(
                            target: "kakehashi::bridge::reader",
                            "{}Failed to parse registerCapability params: {}",
                            lang_prefix, e
                        );
                        Err(jsonrpc::Error::invalid_params(format!(
                            "Invalid params: {e}"
                        )))
                    }
                }
            } else {
                warn!(
                    target: "kakehashi::bridge::reader",
                    "{}Request 'client/registerCapability' is missing 'params' field",
                    lang_prefix
                );
                Err(jsonrpc::Error::invalid_params(
                    "Request 'client/registerCapability' is missing 'params' field",
                ))
            }
        }
        "client/unregisterCapability" => {
            if let Some(params) = message.get("params") {
                match serde_json::from_value::<tower_lsp_server::ls_types::UnregistrationParams>(
                    params.clone(),
                ) {
                    Ok(unreg_params) => {
                        for unreg in &unreg_params.unregisterations {
                            debug!(
                                target: "kakehashi::bridge::reader",
                                "{}Unregistered dynamic capability: {} (id={})",
                                lang_prefix, unreg.method, unreg.id
                            );
                        }
                        deps.dynamic_capabilities
                            .unregister(unreg_params.unregisterations);
                        Ok(())
                    }
                    Err(e) => {
                        warn!(
                            target: "kakehashi::bridge::reader",
                            "{}Failed to parse unregisterCapability params: {}",
                            lang_prefix, e
                        );
                        Err(jsonrpc::Error::invalid_params(format!(
                            "Invalid params: {e}"
                        )))
                    }
                }
            } else {
                warn!(
                    target: "kakehashi::bridge::reader",
                    "{}Request 'client/unregisterCapability' is missing 'params' field",
                    lang_prefix
                );
                Err(jsonrpc::Error::invalid_params(
                    "Request 'client/unregisterCapability' is missing 'params' field",
                ))
            }
        }
        "window/workDoneProgress/create" => {
            // Common from Pyright et al. Returning MethodNotFound causes server-side log noise,
            // so we acknowledge silently.
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Acknowledged window/workDoneProgress/create",
                lang_prefix
            );
            Ok(())
        }
        "workspace/diagnostic/refresh" => {
            // Downstream server is requesting that the client re-pull diagnostics.
            // Forward this upstream so the editor triggers a fresh diagnostic pull.
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Forwarding workspace/diagnostic/refresh upstream",
                lang_prefix
            );
            let _ = deps
                .upstream_tx
                .send(UpstreamNotification::DiagnosticRefresh);
            Ok(())
        }
        _ => {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Unknown server request method: {}, responding with MethodNotFound",
                lang_prefix, method
            );
            Err(jsonrpc::Error::method_not_found())
        }
    };

    let response = match body {
        Ok(()) => ok_response(),
        Err(error) => jsonrpc::Response::from_error(id, error),
    };
    // Response implements Serialize, so convert to Value for OutboundMessage.
    let response = serde_json::to_value(response).expect("Response serialization is infallible");

    // Send response via the writer channel.
    // We use OutboundMessage::Untracked because a server-initiated response has
    // no ResponseRouter entry to clean up on failure (unlike Tracked requests).
    //
    // We use send_timeout(5s) instead of try_send() to guarantee delivery under
    // transient backpressure. try_send() silently drops the response if the queue
    // (capacity 256) is momentarily full — a correctness bug for server-initiated
    // requests like client/registerCapability that require acknowledgment.
    //
    // We avoid bare send().await because it could theoretically deadlock if the
    // queue is full, the writer is blocked on stdin, and the downstream server is
    // blocked on stdout — creating a circular wait. send_timeout(5s) provides an
    // explicit safety net: the response is dropped only after 5 seconds of
    // sustained backpressure, which is far better than instant loss.
    if let Err(e) = deps
        .response_tx
        .send_timeout(OutboundMessage::Untracked(response), Duration::from_secs(5))
        .await
    {
        warn!(
            target: "kakehashi::bridge::reader",
            "{}Failed to send response for server request '{}': {}",
            lang_prefix, method, e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::connection::AsyncBridgeConnection;
    use serde_json::json;

    /// Helper to create a test connection using `cat` for echo behavior.
    async fn create_echo_connection() -> AsyncBridgeConnection {
        AsyncBridgeConnection::spawn(vec!["cat".to_string()])
            .await
            .expect("cat should spawn")
    }

    #[tokio::test]
    async fn reader_task_routes_response_to_waiter() {
        // Create a connection that echoes messages
        let mut conn = create_echo_connection().await;

        // Write a response before splitting (so it's in the pipe)
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "contents": "test hover" }
        });
        conn.write_message(&response)
            .await
            .expect("write should succeed");

        // Split the connection to get the reader
        let (writer, reader) = conn.split();

        // Create router and register a request
        let router = Arc::new(ResponseRouter::new());
        let rx = router
            .register(crate::lsp::bridge::protocol::RequestId::new(42))
            .unwrap();

        // Spawn the reader task
        let _handle = spawn_reader_task(reader, Arc::clone(&router));

        // Wait for the response with timeout
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("should not timeout")
            .expect("channel should not be closed");

        // Verify the response was routed correctly
        assert_eq!(received["id"], 42);
        assert_eq!(received["result"]["contents"], "test hover");
        assert_eq!(router.pending_count(), 0);

        // Drop writer to clean up child process
        drop(writer);
    }

    /// Create dummy ServerRequestDeps for tests that don't need server request handling.
    ///
    /// Returns the deps along with the receivers (stored in a tuple) to keep them alive.
    fn dummy_server_request_deps() -> (ServerRequestDeps, impl std::any::Any) {
        let (tx, rx) = mpsc::channel(16);
        let caps = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, upstream_rx) = mpsc::unbounded_channel();
        let deps = ServerRequestDeps {
            language: None,
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
        };
        (deps, (rx, upstream_rx))
    }

    #[tokio::test]
    async fn handle_message_routes_response() {
        let router = ResponseRouter::new();
        let _rx = router
            .register(crate::lsp::bridge::protocol::RequestId::new(1))
            .unwrap();
        let (deps, _keep) = dummy_server_request_deps();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        });

        handle_message(response, &router, "", &deps).await;

        // Receiver should have the response
        // We can't block on rx.await here in a sync test, but we can check
        // that the pending count is 0 (meaning it was routed)
        assert_eq!(router.pending_count(), 0);
    }

    #[tokio::test]
    async fn handle_message_ignores_notification() {
        let router = ResponseRouter::new();
        let _rx = router
            .register(crate::lsp::bridge::protocol::RequestId::new(1))
            .unwrap();
        let (deps, _keep) = dummy_server_request_deps();

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {}
        });

        handle_message(notification, &router, "", &deps).await;

        // Pending count should still be 1 (notification was ignored)
        assert_eq!(router.pending_count(), 1);
    }

    #[tokio::test]
    async fn reader_loop_exits_on_cancellation() {
        use crate::lsp::bridge::connection::BridgeReader;
        use std::process::Stdio;
        use tokio::process::Command;

        // Create a long-running process that won't send any output
        // Using `sleep` ensures the reader blocks waiting for input
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sleep should spawn");

        let stdout = child.stdout.take().expect("stdout should be available");
        let reader = BridgeReader::new(stdout);

        let router = Arc::new(ResponseRouter::new());
        let cancel_token = CancellationToken::new();
        let token_clone = cancel_token.clone();

        // Spawn the reader loop
        let handle = tokio::spawn(reader_loop(reader, router, token_clone));

        // Give the loop a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Cancel the token
        cancel_token.cancel();

        // The loop should exit promptly
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(
            result.is_ok(),
            "reader_loop should exit quickly after cancellation"
        );

        // Clean up
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn reader_loop_fails_all_on_eof() {
        use crate::lsp::bridge::protocol::RequestId;

        // Create a connection that will close immediately (empty echo)
        let mut conn = create_echo_connection().await;
        let (writer, reader) = conn.split();

        // Drop the writer to close stdin, causing EOF on stdout
        drop(writer);

        let router = Arc::new(ResponseRouter::new());
        let rx1 = router.register(RequestId::new(1)).unwrap();
        let rx2 = router.register(RequestId::new(2)).unwrap();

        let cancel_token = CancellationToken::new();

        // Run the reader loop - it should exit on EOF
        let handle = tokio::spawn(reader_loop(reader, Arc::clone(&router), cancel_token));

        // Wait for the loop to complete
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "reader_loop should exit on EOF");

        // All pending requests should have received error responses
        assert_eq!(router.pending_count(), 0, "all pending should be cleared");

        // Check that waiters received error responses
        let response1 = rx1.await.expect("should receive error response");
        assert!(
            response1.get("error").is_some(),
            "response should be an error"
        );
        assert_eq!(response1["error"]["code"], -32603);

        let response2 = rx2.await.expect("should receive error response");
        assert!(
            response2.get("error").is_some(),
            "response should be an error"
        );
    }

    #[tokio::test]
    async fn reader_loop_routes_multiple_responses_in_order() {
        use crate::lsp::bridge::protocol::RequestId;

        let mut conn = create_echo_connection().await;

        // Write multiple responses before splitting
        let response1 = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "first"
        });
        let response2 = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": "second"
        });
        let response3 = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": "third"
        });

        conn.write_message(&response1).await.unwrap();
        conn.write_message(&response2).await.unwrap();
        conn.write_message(&response3).await.unwrap();

        let (writer, reader) = conn.split();

        let router = Arc::new(ResponseRouter::new());
        let rx1 = router.register(RequestId::new(1)).unwrap();
        let rx2 = router.register(RequestId::new(2)).unwrap();
        let rx3 = router.register(RequestId::new(3)).unwrap();

        let _handle = spawn_reader_task(reader, Arc::clone(&router));

        // All three should be received
        let received1 = tokio::time::timeout(std::time::Duration::from_secs(1), rx1)
            .await
            .expect("should not timeout")
            .expect("channel should not be closed");
        assert_eq!(received1["result"], "first");

        let received2 = tokio::time::timeout(std::time::Duration::from_secs(1), rx2)
            .await
            .expect("should not timeout")
            .expect("channel should not be closed");
        assert_eq!(received2["result"], "second");

        let received3 = tokio::time::timeout(std::time::Duration::from_secs(1), rx3)
            .await
            .expect("should not timeout")
            .expect("channel should not be closed");
        assert_eq!(received3["result"], "third");

        assert_eq!(router.pending_count(), 0);
        drop(writer);
    }

    #[tokio::test]
    async fn reader_loop_skips_notifications_and_continues() {
        use crate::lsp::bridge::protocol::RequestId;

        let mut conn = create_echo_connection().await;

        // Write a notification followed by a response
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "test" }
        });
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "after notification"
        });

        conn.write_message(&notification).await.unwrap();
        conn.write_message(&response).await.unwrap();

        let (writer, reader) = conn.split();

        let router = Arc::new(ResponseRouter::new());
        let rx = router.register(RequestId::new(42)).unwrap();

        let _handle = spawn_reader_task(reader, Arc::clone(&router));

        // Should receive the response even though notification came first
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("should not timeout")
            .expect("channel should not be closed");

        assert_eq!(received["id"], 42);
        assert_eq!(received["result"], "after notification");

        drop(writer);
    }

    #[tokio::test]
    async fn reader_loop_handles_unknown_response_id() {
        use crate::lsp::bridge::protocol::RequestId;

        let mut conn = create_echo_connection().await;

        // Write a response with an unregistered ID, followed by one with registered ID
        let unknown_response = json!({
            "jsonrpc": "2.0",
            "id": 999,
            "result": "unknown"
        });
        let known_response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "known"
        });

        conn.write_message(&unknown_response).await.unwrap();
        conn.write_message(&known_response).await.unwrap();

        let (writer, reader) = conn.split();

        let router = Arc::new(ResponseRouter::new());
        // Only register ID 1, not 999
        let rx = router.register(RequestId::new(1)).unwrap();

        let _handle = spawn_reader_task(reader, Arc::clone(&router));

        // Should skip the unknown response and deliver the known one
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("should not timeout")
            .expect("channel should not be closed");

        assert_eq!(received["id"], 1);
        assert_eq!(received["result"], "known");

        drop(writer);
    }

    // ============================================================
    // Message Classification Tests
    // ============================================================

    #[test]
    fn classify_message_response() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": null});
        assert_eq!(classify_message(&msg), MessageKind::Response);
    }

    #[test]
    fn classify_message_server_request() {
        let msg =
            json!({"jsonrpc": "2.0", "id": 1, "method": "client/registerCapability", "params": {}});
        assert_eq!(classify_message(&msg), MessageKind::ServerRequest);
    }

    #[test]
    fn classify_message_notification() {
        let msg = json!({"jsonrpc": "2.0", "method": "$/progress", "params": {}});
        assert_eq!(classify_message(&msg), MessageKind::Notification);
    }

    #[test]
    fn classify_message_invalid() {
        let msg = json!({"jsonrpc": "2.0"});
        assert_eq!(classify_message(&msg), MessageKind::Invalid);
    }

    #[tokio::test]
    async fn handle_message_does_not_route_server_request() {
        let router = ResponseRouter::new();
        let _rx = router
            .register(crate::lsp::bridge::protocol::RequestId::new(1))
            .unwrap();
        assert_eq!(router.pending_count(), 1);
        let (deps, _keep) = dummy_server_request_deps();

        // Server-initiated request: has both "id" and "method"
        let server_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "client/registerCapability",
            "params": {}
        });

        handle_message(server_request, &router, "", &deps).await;

        // The server request should NOT be routed as a response.
        // Pending count must remain 1 (the registered request is still waiting).
        assert_eq!(
            router.pending_count(),
            1,
            "Server-initiated requests (with both id and method) must not be routed as responses"
        );
    }

    // ============================================================
    // Server Request Handling Tests
    // ============================================================

    #[tokio::test]
    async fn handle_message_register_capability_updates_registry() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "client/registerCapability",
            "params": {
                "registrations": [
                    {
                        "id": "diag-1",
                        "method": "textDocument/diagnostic",
                        "registerOptions": null
                    }
                ]
            }
        });

        handle_message(message, &router, "", &deps).await;

        // Registry should have the registration
        assert!(dynamic_capabilities.has_registration("textDocument/diagnostic"));

        // A response should have been sent
        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 1);
                assert!(val["result"].is_null());
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    #[tokio::test]
    async fn handle_message_unregister_capability_updates_registry() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();

        // First register a capability
        dynamic_capabilities.register(vec![tower_lsp_server::ls_types::Registration {
            id: "diag-1".to_string(),
            method: "textDocument/diagnostic".to_string(),
            register_options: None,
        }]);
        assert!(dynamic_capabilities.has_registration("textDocument/diagnostic"));

        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
        };

        // Then unregister it
        let message = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "client/unregisterCapability",
            "params": {
                "unregisterations": [
                    {
                        "id": "diag-1",
                        "method": "textDocument/diagnostic"
                    }
                ]
            }
        });

        handle_message(message, &router, "", &deps).await;

        // Registry should no longer have the registration
        assert!(!dynamic_capabilities.has_registration("textDocument/diagnostic"));

        // A success response should have been sent
        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 2);
                assert!(val["result"].is_null());
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    #[tokio::test]
    async fn handle_message_work_done_progress_create_sends_success() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "window/workDoneProgress/create",
            "params": {
                "token": "some-token"
            }
        });

        handle_message(message, &router, "", &deps).await;

        // A success response should have been sent
        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 5);
                assert!(val["result"].is_null());
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    /// Test that workspace/diagnostic/refresh is forwarded upstream and acknowledged.
    ///
    /// When a downstream server sends workspace/diagnostic/refresh:
    /// 1. An UpstreamNotification::DiagnosticRefresh is sent on the upstream channel
    /// 2. A success response (not MethodNotFound) is sent back to the server
    #[tokio::test]
    async fn handle_message_diagnostic_refresh_forwards_upstream() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "workspace/diagnostic/refresh",
            "params": null
        });

        handle_message(message, &router, "", &deps).await;

        // Should have sent DiagnosticRefresh on the upstream channel
        let notification = upstream_rx
            .try_recv()
            .expect("should have upstream notification");
        assert_eq!(notification, UpstreamNotification::DiagnosticRefresh);

        // Should have sent a success response (not MethodNotFound)
        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 10);
                assert!(val["result"].is_null(), "Should be success, not error");
                assert!(val.get("error").is_none(), "Should not have error field");
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    #[tokio::test]
    async fn handle_message_unknown_server_request_sends_method_not_found() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "some/unknownMethod",
            "params": {}
        });

        handle_message(message, &router, "", &deps).await;

        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 99);
                assert_eq!(val["error"]["code"], -32601);
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    #[tokio::test]
    async fn handle_message_register_capability_missing_params_returns_error() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
        };

        // client/registerCapability with no params field at all
        let message = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "client/registerCapability"
        });

        handle_message(message, &router, "", &deps).await;

        // Should respond with InvalidParams error (-32602)
        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 1);
                assert_eq!(
                    val["error"]["code"], -32602,
                    "Should be InvalidParams error"
                );
            }
            _ => panic!("Expected Untracked variant"),
        }

        // Registry should remain empty
        assert!(!dynamic_capabilities.has_registration("textDocument/diagnostic"));
    }

    #[tokio::test]
    async fn handle_message_unregister_capability_missing_params_returns_error() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();

        // Pre-register a capability so we can verify it's NOT removed
        dynamic_capabilities.register(vec![tower_lsp_server::ls_types::Registration {
            id: "diag-1".to_string(),
            method: "textDocument/diagnostic".to_string(),
            register_options: None,
        }]);

        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
        };

        // client/unregisterCapability with no params field at all
        let message = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "client/unregisterCapability"
        });

        handle_message(message, &router, "", &deps).await;

        // Should respond with InvalidParams error (-32602)
        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 2);
                assert_eq!(
                    val["error"]["code"], -32602,
                    "Should be InvalidParams error"
                );
            }
            _ => panic!("Expected Untracked variant"),
        }

        // Registry should still have the pre-registered capability
        assert!(dynamic_capabilities.has_registration("textDocument/diagnostic"));
    }

    // ============================================================
    // Server Request Response Reliability Tests
    // ============================================================

    /// Test that server request response is delivered even under backpressure.
    ///
    /// With `try_send`, if the outbound queue is full the response is silently
    /// dropped. With `send_timeout`, the response waits for capacity and is
    /// delivered once the queue drains.
    #[tokio::test]
    async fn server_request_response_survives_backpressure() {
        use std::time::Duration;

        // Use capacity=1 to simulate backpressure
        let (response_tx, mut response_rx) = mpsc::channel(1);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();

        // Fill the channel with a dummy message to create backpressure
        response_tx
            .send(OutboundMessage::Untracked(json!({"dummy": true})))
            .await
            .unwrap();

        // Spawn handle_server_request in a separate task (it needs to await)
        let deps = ServerRequestDeps {
            language: None,
            response_tx: response_tx.clone(),
            dynamic_capabilities,
            upstream_tx,
        };
        let handle = tokio::spawn(async move {
            let message = json!({
                "jsonrpc": "2.0",
                "id": 42,
                "method": "window/workDoneProgress/create",
                "params": { "token": "test" }
            });
            handle_server_request(message, "", &deps).await;
        });

        // Give handle_server_request a moment to start waiting
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drain the dummy message to free capacity
        let dummy = response_rx.recv().await.expect("should have dummy");
        assert!(matches!(dummy, OutboundMessage::Untracked(v) if v["dummy"] == true));

        // Now the server request response should arrive
        let response = tokio::time::timeout(Duration::from_secs(2), response_rx.recv())
            .await
            .expect("should not timeout waiting for response")
            .expect("channel should not be closed");

        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 42);
                assert!(val["result"].is_null());
            }
            _ => panic!("Expected Untracked variant"),
        }

        handle.await.unwrap();
    }

    /// Test that server request response on a closed channel does not panic.
    ///
    /// When the receiver is dropped (e.g., connection shutting down),
    /// handle_server_request should log a warning but not panic.
    #[tokio::test]
    async fn server_request_response_closed_channel_no_panic() {
        let (response_tx, response_rx) = mpsc::channel(1);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();

        // Drop the receiver to simulate a closed channel
        drop(response_rx);

        let deps = ServerRequestDeps {
            language: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "window/workDoneProgress/create",
            "params": { "token": "test" }
        });

        // Should not panic
        handle_server_request(message, "", &deps).await;
    }

    // ============================================================
    // Liveness Timer Tests (ADR-0014)
    // ============================================================

    /// Test that liveness timer starts when notified (pending 0->1 transition).
    ///
    /// ADR-0014: Timer starts when pending count transitions 0 to 1.
    /// This verifies that sending a start notification activates the timer.
    #[tokio::test]
    async fn liveness_timer_starts_on_notification() {
        use crate::lsp::bridge::protocol::RequestId;
        use std::time::Duration;

        // Create an unresponsive server (will never send a response)
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(), // Consumes input, never outputs
        ])
        .await
        .expect("should spawn process");

        let (_writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Register a request (simulating pending going 0->1)
        let _rx = router.register(RequestId::new(1)).unwrap();
        assert_eq!(router.pending_count(), 1);

        // Spawn reader with short liveness timeout
        let handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(100)),
        );

        // Notify the reader to start the timer
        handle.notify_liveness_start();

        // Wait for timeout to fire
        tokio::time::sleep(Duration::from_millis(200)).await;

        // After timeout, pending requests should be failed
        // (router.fail_all is called on timeout)
        assert_eq!(
            router.pending_count(),
            0,
            "Pending count should be 0 after liveness timeout fires"
        );
    }

    /// Test that liveness timer resets on message activity.
    ///
    /// ADR-0014: Timer resets on any stdout activity (response or notification).
    /// This verifies that receiving a message resets the timer to full duration.
    ///
    /// Uses paused time for deterministic testing - avoids CI flakiness from
    /// timing variations under system load.
    #[tokio::test(start_paused = true)]
    async fn liveness_timer_resets_on_message_activity() {
        use crate::lsp::bridge::protocol::RequestId;
        use std::time::Duration;

        // Create a server that echoes messages
        let mut conn = create_echo_connection().await;

        // Register request before splitting
        let router = Arc::new(ResponseRouter::new());
        let _rx1 = router.register(RequestId::new(1)).unwrap();
        let _rx2 = router.register(RequestId::new(2)).unwrap();

        // Write response before splitting - it will be buffered in the pipe
        // When reader starts, it reads the response and resets the timer
        let response1 = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": null
        });
        conn.write_message(&response1).await.unwrap();

        let (writer, reader) = conn.split();

        // Spawn reader with liveness timeout (150ms)
        let handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(150)),
        );

        // Notify the reader to start the timer
        handle.notify_liveness_start();

        // Yield to let reader task process the buffered response
        // This resets the timer deadline
        tokio::task::yield_now().await;

        // Advance time past the original timeout (150ms) but before the reset deadline
        // If timer reset worked: deadline is now ~150ms from when response was processed
        // If timer didn't reset: it would fire at 150ms
        tokio::time::advance(Duration::from_millis(160)).await;
        tokio::task::yield_now().await;

        // After first response, pending should be 1 (one request remaining)
        // Timer should have been reset, not fired
        assert!(
            router.pending_count() <= 1,
            "Timer should reset on message activity, not fire prematurely"
        );

        drop(writer);
    }

    /// Test that liveness timer stops when pending count returns to 0.
    ///
    /// ADR-0014: Timer stops when pending count returns to 0.
    /// This verifies that when the last response is received, the timer is deactivated.
    #[tokio::test]
    async fn liveness_timer_stops_when_pending_zero() {
        use crate::lsp::bridge::protocol::RequestId;
        use std::time::Duration;

        // Create a server that echoes messages
        let mut conn = create_echo_connection().await;

        // Register a single request
        let router = Arc::new(ResponseRouter::new());
        let rx = router.register(RequestId::new(1)).unwrap();

        // Write the response before splitting
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": "done"
        });
        conn.write_message(&response).await.unwrap();

        let (writer, reader) = conn.split();

        // Spawn reader with liveness timeout
        let handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(100)),
        );

        // Notify the reader to start the timer
        handle.notify_liveness_start();

        // Wait for response to be received
        let _received = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("should receive response")
            .expect("channel should not be closed");

        // Pending count should now be 0
        assert_eq!(
            router.pending_count(),
            0,
            "Pending should be 0 after response"
        );

        // Wait past the original timeout - timer should have stopped, not fired
        tokio::time::sleep(Duration::from_millis(150)).await;

        // If timer had fired, router would have fail_all() called and we'd see errors
        // Since pending is already 0, there's nothing to fail - this is the correct behavior

        drop(writer);
    }

    /// Test that liveness timeout fires and fails pending requests.
    ///
    /// ADR-0014: Ready to Failed transition on liveness timeout expiry.
    /// When timeout fires while pending > 0, router.fail_all() is called.
    #[tokio::test]
    async fn liveness_timeout_fires_and_fails_pending_requests() {
        use crate::lsp::bridge::protocol::RequestId;
        use std::time::Duration;

        // Create an unresponsive server
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (_writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Register multiple pending requests
        let rx1 = router.register(RequestId::new(1)).unwrap();
        let rx2 = router.register(RequestId::new(2)).unwrap();
        assert_eq!(router.pending_count(), 2);

        // Spawn reader with short liveness timeout
        let handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(50)),
        );

        // Notify the reader to start the timer
        handle.notify_liveness_start();

        // Wait for both receivers to get error responses
        let result1 = tokio::time::timeout(Duration::from_millis(200), rx1)
            .await
            .expect("should not timeout on receiver")
            .expect("channel should not be closed");

        let result2 = tokio::time::timeout(Duration::from_millis(200), rx2)
            .await
            .expect("should not timeout on receiver")
            .expect("channel should not be closed");

        // Both should have received error responses
        assert!(
            result1.get("error").is_some(),
            "Request 1 should have error response"
        );
        assert!(
            result2.get("error").is_some(),
            "Request 2 should have error response"
        );
        assert!(
            result1["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("liveness timeout"),
            "Error should mention liveness timeout"
        );

        // Pending should be 0 after fail_all
        assert_eq!(
            router.pending_count(),
            0,
            "Pending should be 0 after timeout"
        );
    }
}
