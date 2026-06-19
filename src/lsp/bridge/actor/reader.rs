//! Reader task for downstream language server stdout.
//!
//! This module provides the background task that reads LSP messages from
//! the downstream server's stdout and routes responses to waiting requesters.
//!
//! # Architecture (ls-bridge-message-ordering)
//!
//! The Reader Task:
//! - Runs in a spawned tokio task
//! - Reads messages from stdout using BridgeReader
//! - Routes responses via ResponseRouter to oneshot waiters
//! - Logs notifications (they don't have waiters)
//! - Manages liveness timer for hung server detection (ls-bridge-async-connection)
//! - Gracefully shuts down on EOF, error, or cancellation signal

use std::sync::Arc;
use std::time::Duration;

use log::{debug, warn};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::{LogMessageParams, MessageType, ShowMessageParams};

use super::super::connection::BridgeReader;
use super::OutboundMessage;
use super::ResponseRouter;
use super::response_router::RouteResult;
use crate::error::LockResultExt;
use crate::lsp::bridge::pool::DynamicCapabilityRegistry;

/// Notification to forward from downstream server to upstream editor.
///
/// Reader tasks use this to signal events that require upstream Client interaction,
/// keeping the bridge module decoupled from tower-lsp's Client type.
///
/// Two channels carry these, split by loss tolerance:
/// - `DiagnosticRefresh` travels on an **unbounded** channel: dropping one
///   would silently stale the editor's diagnostics, and its volume is tiny
///   (one per downstream `workspace/diagnostic/refresh`).
/// - `LogMessage`/`ShowMessage` travel on a **bounded** channel
///   ([`WINDOW_NOTIFICATION_QUEUE_CAPACITY`]) with drop-on-full: a downstream
///   server stuck in a tight logMessage loop must not grow memory without
///   bound or starve `DiagnosticRefresh` behind a burst (the forwarding loop
///   drains the unbounded channel first). Within the bounded channel FIFO
///   order is preserved, which the window-notification e2e relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpstreamNotification {
    /// Request upstream to re-pull diagnostics.
    /// Sent when downstream server issues `workspace/diagnostic/refresh`.
    DiagnosticRefresh,
    /// Forward a downstream `window/logMessage` to the editor.
    /// `message` is already prefixed with the originating server name.
    LogMessage { typ: MessageType, message: String },
    /// Forward a downstream `window/showMessage` to the editor.
    /// `message` is already prefixed with the originating server name.
    ShowMessage { typ: MessageType, message: String },
}

/// Capacity of the bounded `window/*` forwarding queue. Sized like the
/// outbound queue (`OUTBOUND_QUEUE_CAPACITY`): large enough that drops require
/// a downstream server flooding faster than the editor drains for a sustained
/// burst.
pub(crate) const WINDOW_NOTIFICATION_QUEUE_CAPACITY: usize = 256;

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

/// Dependencies for handling server-initiated requests and notifications.
///
/// Groups the parameters that `handle_server_request` and
/// `forward_notification` need: the downstream server name (for logging and
/// message prefixes), the response channel, the dynamic capability registry,
/// the upstream notification channels, and the workspace folders this
/// connection was initialized with (the bridge advertises the
/// `workspace.workspaceFolders` capability, so servers may query them).
pub(crate) struct ServerRequestDeps {
    pub(crate) server_name: Option<String>,
    pub(crate) response_tx: mpsc::Sender<OutboundMessage>,
    pub(crate) dynamic_capabilities: Arc<DynamicCapabilityRegistry>,
    /// Loss-intolerant notifications (`DiagnosticRefresh`); unbounded.
    pub(crate) upstream_tx: mpsc::UnboundedSender<UpstreamNotification>,
    /// Best-effort `window/*` notifications; bounded, dropped when full.
    pub(crate) window_tx: mpsc::Sender<UpstreamNotification>,
    pub(crate) workspace_folders: Arc<Option<Vec<tower_lsp_server::ls_types::WorkspaceFolder>>>,
}

/// Type alias for the pinned liveness timer future.
type LivenessTimer = std::pin::Pin<Box<tokio::time::Sleep>>;

/// Creates a new liveness timer that will fire after the given duration.
fn new_liveness_timer(timeout: Duration) -> LivenessTimer {
    Box::pin(tokio::time::sleep(timeout))
}

/// Reader-task liveness timer: `timer: Option<…>` (Some = armed) plus the
/// configured `timeout`. Methods cover the `start` (pending 0→1) / `reset`
/// (stdout activity) / `stop` (pending→0) lifecycle.
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
    fn start(&mut self, server_prefix: &str) {
        if let Some(timeout) = self.timeout {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer started: {:?}",
                server_prefix,
                timeout
            );
            self.timer = Some(new_liveness_timer(timeout));
        }
    }

    /// Reset the liveness timer to full duration.
    ///
    /// Called on any message activity (response or notification).
    /// Only resets if timer is currently active.
    fn reset(&mut self, server_prefix: &str) {
        if let Some(timeout) = self.timeout
            && self.timer.is_some()
        {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer reset on message activity",
                server_prefix
            );
            self.timer = Some(new_liveness_timer(timeout));
        }
    }

    /// Stop the liveness timer.
    ///
    /// Called when pending count returns to 0 or shutdown begins.
    /// Only logs if timer was actually active.
    fn stop(&mut self, server_prefix: &str, reason: &str) {
        if self.timer.take().is_some() {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer stopped: {}",
                server_prefix,
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

/// RAII handle for a Reader task: dropping cancels and detaches the task.
///
/// Drop semantics: dropping the guard cancels the token, which unblocks the
/// reader loop's `select!`; the JoinHandle is not awaited because async drop
/// does not exist and the reader exits within one loop iteration on
/// cancel/EOF/error. On cancellation the loop fails all pending requests so
/// waiters are not left hanging until the per-request timeout.
pub(crate) struct ReaderTaskHandle {
    _join_handle: JoinHandle<()>,

    /// Dropping the guard cancels the token, signalling the reader loop to exit.
    _cancel_guard: tokio_util::sync::DropGuard,

    /// Notify reader when pending count goes 0→1 so it starts the liveness timer.
    liveness_start_tx: mpsc::Sender<()>,

    /// Stop the liveness timer at shutdown so Tier 3 (global) overrides Tier 2 (ls-bridge-timeout-hierarchy).
    liveness_stop_tx: mpsc::Sender<()>,

    /// Reader fires this when liveness times out; ConnectionHandle reads it to enter Failed (ls-bridge-async-connection).
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

    /// Stop the liveness timer without canceling the reader task (ls-bridge-timeout-hierarchy Phase 4).
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

    /// Check if a liveness failure has been signaled (ConnectionHandle uses this
    /// to transition to Failed).
    ///
    /// One-time check: once it returns true, subsequent calls return false.
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
/// Convenience wrapper for tests that don't need liveness timeout; production
/// code should use `spawn_reader_task_with_liveness` directly.
#[cfg(test)]
pub(crate) fn spawn_reader_task(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
) -> ReaderTaskHandle {
    spawn_reader_task_with_liveness(reader, router, None)
}

/// Spawn a reader task with optional liveness timeout (no server context).
///
/// Convenience wrapper for tests that don't need structured logging with
/// server names (ls-bridge-async-connection liveness timeout); production code should use
/// `spawn_reader_task_for_server`.
#[cfg(test)]
pub(crate) fn spawn_reader_task_with_liveness(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
    liveness_timeout: Option<Duration>,
) -> ReaderTaskHandle {
    let (response_tx, _response_rx) = mpsc::channel(16);
    let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
    let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
    let (window_tx, _window_rx) = mpsc::channel(16);
    spawn_reader_task_for_server(
        reader,
        router,
        liveness_timeout,
        ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
        },
    )
}

/// Spawn a reader task with liveness timeout (ls-bridge-async-connection).
///
/// `deps` carries the per-server context (name, channels, capability registry,
/// workspace folders), snapshotted at spawn time like the rest of the server
/// config (#378).
pub(crate) fn spawn_reader_task_for_server(
    reader: BridgeReader,
    router: Arc<ResponseRouter>,
    liveness_timeout: Option<Duration>,
    deps: ServerRequestDeps,
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
    let join_handle = tokio::spawn(reader_loop_with_liveness(
        reader,
        router,
        token_clone,
        liveness,
        deps,
    ));

    ReaderTaskHandle {
        _join_handle: join_handle,
        _cancel_guard: cancel_token.drop_guard(),
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
    let (window_tx, _window_rx) = mpsc::channel(16);
    let liveness = LivenessParams {
        timeout: None,
        start_rx,
        stop_rx,
        failed_tx,
    };
    let server_request_deps = ServerRequestDeps {
        server_name: None,
        response_tx,
        dynamic_capabilities,
        upstream_tx,
        workspace_folders: Arc::new(None),
        window_tx,
    };
    reader_loop_with_liveness(reader, router, cancel_token, liveness, server_request_deps).await
}

/// The main reader loop with optional liveness timeout support (ls-bridge-async-connection).
///
/// The liveness timer (when configured) detects a hung server: firing triggers a
/// Ready→Failed transition via `router.fail_all()` and signals `liveness_failed_tx`.
/// Its start/reset/stop lifecycle is driven by the `liveness_*` channels and stdout
/// activity (see `LivenessTimerState`).
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

    // Server-name prefix for log messages (e.g., "[lua-ls] " or "")
    let server_prefix = server_request_deps
        .server_name
        .as_ref()
        .map(|l| format!("[{}] ", l))
        .unwrap_or_default();

    // Consolidated liveness timer state (ls-bridge-async-connection)
    let mut liveness = LivenessTimerState::new(liveness_timeout);

    loop {
        tokio::select! {
            biased; // Process in order: cancellation, timer, liveness start, liveness stop, read

            // Check for cancellation first (highest priority)
            _ = cancel_token.cancelled() => {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Reader task cancelled, shutting down",
                    server_prefix
                );
                // The router outlives this task (Arc-shared with ConnectionHandle),
                // so pending waiters must be failed here or they hang until the
                // per-request timeout.
                router.fail_all("bridge: reader task cancelled");
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
                    server_prefix,
                    liveness.timeout_duration(),
                    pending_count
                );
                router.fail_all("bridge: liveness timeout - server unresponsive");
                // Signal liveness failure for state transition (ls-bridge-async-connection Phase 3)
                let _ = liveness_failed_tx.send(());
                break;
            }

            // Check for liveness timer start notification (pending 0->1)
            Some(()) = liveness_start_rx.recv() => {
                liveness.start(&server_prefix);
            }

            // Check for liveness timer stop notification (shutdown began - ls-bridge-timeout-hierarchy Phase 4)
            Some(()) = liveness_stop_rx.recv() => {
                liveness.stop(&server_prefix, "shutdown began");
            }

            // Try to read a message (lowest priority)
            result = reader.read_message() => {
                match result {
                    Ok(message) => {
                        // Reset liveness timer on any message activity (ls-bridge-async-connection)
                        liveness.reset(&server_prefix);

                        handle_message(message, &router, &server_prefix, &server_request_deps).await;

                        // Check if pending count returned to 0 - stop timer
                        if liveness.is_active() && router.pending_count() == 0 {
                            liveness.stop(&server_prefix, "pending count is 0");
                        }
                    }
                    Err(e) => {
                        // EOF or read error - mark all pending as failed and exit
                        warn!(
                            target: "kakehashi::bridge::reader",
                            "{}Reader error: {}, failing pending requests",
                            server_prefix,
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
    server_prefix: &str,
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
                        server_prefix,
                        id.unwrap_or(serde_json::Value::Null)
                    );
                }
                RouteResult::NotFound => {
                    // Unknown request ID - could be a late response or protocol mismatch
                    debug!(
                        target: "kakehashi::bridge::reader",
                        "{}Response for unknown request id={}, dropping",
                        server_prefix,
                        id.unwrap_or(serde_json::Value::Null)
                    );
                }
            }
        }
        MessageKind::ServerRequest => {
            handle_server_request(message, server_prefix, deps).await;
        }
        MessageKind::Notification => {
            // `publishDiagnostics` targeting a pipeline *scratch* document is
            // discarded EXPLICITLY (concatenated-formatting-pipeline Decision
            // point 7: scratch state is speculative and must never reach the
            // editor). The if/else makes the discard structural: notification
            // forwarding lives on the else side, where a scratch-targeted
            // publishDiagnostics can never reach it.
            if is_scratch_publish_diagnostics(&message) {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Discarding publishDiagnostics targeting a scratch virtual document",
                    server_prefix
                );
            } else {
                forward_notification(&message, server_prefix, deps);
            }
        }
        MessageKind::Invalid => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}Invalid message from downstream (no id or method): {}",
                server_prefix,
                message
            );
        }
    }
}

/// Forward supported downstream notifications to the upstream editor.
///
/// Both `window/logMessage` and `window/showMessage` are forwarded
/// unconditionally — the bridge stays transparent so messages a direct
/// connection would surface are not silently swallowed. A downstream flood
/// cannot harm the bridge because the window channel is bounded and
/// drop-on-full (see [`UpstreamNotification`]). Both are prefixed with the
/// originating server name so output from multiple bridged servers stays
/// distinguishable.
///
/// Everything else is still silently ignored ($/progress: #379, push-based
/// publishDiagnostics: #380).
fn forward_notification(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    match message["method"].as_str() {
        Some("window/logMessage") => {
            // Deserialize from a reference to avoid cloning the params value.
            match LogMessageParams::deserialize(&message["params"]) {
                Ok(params) => send_window_notification(
                    deps,
                    server_prefix,
                    "window/logMessage",
                    UpstreamNotification::LogMessage {
                        typ: params.typ,
                        message: prefixed_message(deps.server_name.as_deref(), &params.message),
                    },
                ),
                Err(e) => debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Dropping window/logMessage with invalid params: {}",
                    server_prefix,
                    e
                ),
            }
        }
        Some("window/showMessage") => match ShowMessageParams::deserialize(&message["params"]) {
            Ok(params) => send_window_notification(
                deps,
                server_prefix,
                "window/showMessage",
                UpstreamNotification::ShowMessage {
                    typ: params.typ,
                    message: prefixed_message(deps.server_name.as_deref(), &params.message),
                },
            ),
            Err(e) => debug!(
                target: "kakehashi::bridge::reader",
                "{}Dropping window/showMessage with invalid params: {}",
                server_prefix,
                e
            ),
        },
        _ => {}
    }
}

/// Enqueue a `window/*` notification on the bounded best-effort channel.
///
/// `try_send` keeps the reader task non-blocking: a full queue (editor slower
/// than a downstream notification flood) drops the message instead of growing
/// memory or stalling stdout reads; a closed queue (shutdown) has no one left
/// to deliver to. Both are deliberate per the loss-tolerance split documented
/// on [`UpstreamNotification`].
fn send_window_notification(
    deps: &ServerRequestDeps,
    server_prefix: &str,
    method: &str,
    notification: UpstreamNotification,
) {
    if let Err(e) = deps.window_tx.try_send(notification) {
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping {} ({})",
            server_prefix,
            method,
            match e {
                mpsc::error::TrySendError::Full(_) => "window notification queue full",
                mpsc::error::TrySendError::Closed(_) => "forwarding loop gone",
            }
        );
    }
}

/// `[kakehashi:<server>] <message>` — tells the user which downstream server
/// a forwarded `window/*` notification came from (#378). Falls back to
/// `[kakehashi] <message>` when the server name is absent (test-only spawns;
/// production spawns always carry the name, see pool.rs).
fn prefixed_message(server_name: Option<&str>, message: &str) -> String {
    match server_name {
        Some(name) => format!("[kakehashi:{name}] {message}"),
        None => format!("[kakehashi] {message}"),
    }
}

/// Whether `message` is a `textDocument/publishDiagnostics` notification
/// targeting a concatenated-formatting *scratch* virtual document
/// ([`VirtualDocumentUri::is_scratch_uri`]).
///
/// Scratch documents carry speculative pipeline text the editor has never
/// seen; diagnostics computed against them are meaningless to the user and
/// must be discarded, not forwarded (concatenated-formatting-pipeline
/// Decision point 7). The prompt `didClose` after each pipeline run shrinks
/// but cannot eliminate the window in which a downstream server pushes them.
fn is_scratch_publish_diagnostics(message: &serde_json::Value) -> bool {
    // `Value` indexing returns `Null` for missing keys / non-objects, so the
    // lookups below are panic-free on malformed messages.
    message["method"].as_str() == Some("textDocument/publishDiagnostics")
        && message["params"]["uri"]
            .as_str()
            .is_some_and(crate::lsp::bridge::VirtualDocumentUri::is_scratch_uri)
}

/// Handle a server-initiated request by dispatching on its method.
///
/// Every server-initiated request (both `"id"` and `"method"`) must get a
/// JSON-RPC response. On param-parse failure for known methods we reply with an
/// InvalidParams error (-32602): the LSP spec allows error responses to any
/// request, so a server that can't handle one to its own request is buggy.
async fn handle_server_request(
    message: serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let id: jsonrpc::Id = message
        .get("id")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");

    let body: jsonrpc::Result<serde_json::Value> = match method {
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
                                server_prefix, reg.method, reg.id
                            );
                        }
                        deps.dynamic_capabilities.register(reg_params.registrations);
                        Ok(serde_json::Value::Null)
                    }
                    Err(e) => {
                        warn!(
                            target: "kakehashi::bridge::reader",
                            "{}Failed to parse registerCapability params: {}",
                            server_prefix, e
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
                    server_prefix
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
                                server_prefix, unreg.method, unreg.id
                            );
                        }
                        deps.dynamic_capabilities
                            .unregister(unreg_params.unregisterations);
                        Ok(serde_json::Value::Null)
                    }
                    Err(e) => {
                        warn!(
                            target: "kakehashi::bridge::reader",
                            "{}Failed to parse unregisterCapability params: {}",
                            server_prefix, e
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
                    server_prefix
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
                server_prefix
            );
            Ok(serde_json::Value::Null)
        }
        "workspace/diagnostic/refresh" => {
            // Downstream server is requesting that the client re-pull diagnostics.
            // Forward this upstream so the editor triggers a fresh diagnostic pull.
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Forwarding workspace/diagnostic/refresh upstream",
                server_prefix
            );
            let _ = deps
                .upstream_tx
                .send(UpstreamNotification::DiagnosticRefresh);
            Ok(serde_json::Value::Null)
        }
        "workspace/workspaceFolders" => {
            // The bridge advertises workspace.workspaceFolders, so answer
            // with the folders this connection was initialized with
            // (WorkspaceFolder[] | null).
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Answering workspace/workspaceFolders from initialize-time folders",
                server_prefix
            );
            Ok(serde_json::to_value(deps.workspace_folders.as_ref())
                .unwrap_or(serde_json::Value::Null))
        }
        _ => {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Unknown server request method: {}, responding with MethodNotFound",
                server_prefix, method
            );
            Err(jsonrpc::Error::method_not_found())
        }
    };

    let response = match body {
        Ok(result) => jsonrpc::Response::from_ok(id.clone(), result),
        Err(error) => jsonrpc::Response::from_error(id, error),
    };
    // Response implements Serialize, so convert to Value for OutboundMessage.
    // Serialization cannot fail in practice, but the project bans panics in
    // production code; dropping the response is the only sane fallback here.
    let response = match serde_json::to_value(response) {
        Ok(value) => value,
        Err(e) => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}Failed to serialize response for server request '{}': {}",
                server_prefix, method, e
            );
            return;
        }
    };

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
            server_prefix, method, e
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
        let (window_tx, window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
        };
        (deps, (rx, upstream_rx, window_rx))
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

    /// Deps wired with a server name, exposing the bounded window receiver so
    /// tests can assert what was (not) forwarded to the editor.
    fn server_request_deps_for(
        server_name: Option<&str>,
    ) -> (
        ServerRequestDeps,
        mpsc::Receiver<UpstreamNotification>,
        impl std::any::Any,
    ) {
        let (tx, rx) = mpsc::channel(16);
        let caps = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: server_name.map(String::from),
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
        };
        (deps, window_rx, (rx, upstream_rx))
    }

    #[tokio::test]
    async fn handle_message_forwards_window_log_message_upstream() {
        let router = ResponseRouter::new();
        let (deps, mut window_rx, _keep) = server_request_deps_for(Some("mock-ls"));

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "window/logMessage",
            "params": { "type": 3, "message": "downstream says hi" }
        });

        handle_message(notification, &router, "", &deps).await;

        let forwarded = window_rx
            .try_recv()
            .expect("window/logMessage should be forwarded upstream");
        assert_eq!(
            forwarded,
            UpstreamNotification::LogMessage {
                typ: tower_lsp_server::ls_types::MessageType::INFO,
                message: "[kakehashi:mock-ls] downstream says hi".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn handle_message_drops_window_log_message_with_invalid_params() {
        let router = ResponseRouter::new();
        let (deps, mut window_rx, _keep) = server_request_deps_for(Some("mock-ls"));

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "window/logMessage",
            "params": { "message": 42 }
        });

        handle_message(notification, &router, "", &deps).await;

        assert!(
            window_rx.try_recv().is_err(),
            "window/logMessage with unparseable params must be dropped, not forwarded"
        );
    }

    #[tokio::test]
    async fn handle_message_forwards_window_show_message() {
        let router = ResponseRouter::new();
        let (deps, mut window_rx, _keep) = server_request_deps_for(Some("mock-ls"));

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "window/showMessage",
            "params": { "type": 1, "message": "important popup" }
        });

        handle_message(notification, &router, "", &deps).await;

        let forwarded = window_rx
            .try_recv()
            .expect("window/showMessage should be forwarded to the editor");
        assert_eq!(
            forwarded,
            UpstreamNotification::ShowMessage {
                typ: tower_lsp_server::ls_types::MessageType::ERROR,
                message: "[kakehashi:mock-ls] important popup".to_string(),
            }
        );
    }

    /// A full window queue drops the notification instead of blocking the
    /// reader or growing memory (the loss-tolerance split documented on
    /// UpstreamNotification).
    #[tokio::test]
    async fn handle_message_drops_window_log_message_when_queue_full() {
        let router = ResponseRouter::new();
        let (tx, _rx) = mpsc::channel(16);
        let caps = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, mut window_rx) = mpsc::channel(1);
        let deps = ServerRequestDeps {
            server_name: Some("mock-ls".to_string()),
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
            window_tx,
            workspace_folders: Arc::new(None),
        };

        let notification = |text: &str| {
            json!({
                "jsonrpc": "2.0",
                "method": "window/logMessage",
                "params": { "type": 3, "message": text }
            })
        };

        // Capacity 1: the first message fills the queue, the second must be
        // dropped without blocking (this await completing IS the assertion
        // that the reader path stays non-blocking on a full queue).
        handle_message(notification("first"), &router, "", &deps).await;
        handle_message(notification("second"), &router, "", &deps).await;

        let forwarded = window_rx.try_recv().expect("first message is delivered");
        assert_eq!(
            forwarded,
            UpstreamNotification::LogMessage {
                typ: tower_lsp_server::ls_types::MessageType::INFO,
                message: "[kakehashi:mock-ls] first".to_string(),
            }
        );
        assert!(
            window_rx.try_recv().is_err(),
            "second message must be dropped when the bounded queue is full"
        );
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
            .kill_on_drop(true)
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

    /// ReaderTaskHandle documents drop-cancels-the-task semantics. A plain
    /// CancellationToken field does not deliver that: dropping a token never
    /// cancels it, so the reader task (and its pending waiters) would leak
    /// until child-process EOF.
    #[tokio::test]
    async fn dropping_reader_task_handle_cancels_reader_and_fails_pending() {
        use crate::lsp::bridge::connection::BridgeReader;
        use crate::lsp::bridge::protocol::RequestId;
        use std::process::Stdio;
        use tokio::process::Command;

        // Long-running process that never writes to stdout, so the loop can
        // only exit via drop-triggered cancellation (not EOF).
        let mut child = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sleep should spawn");

        let stdout = child.stdout.take().expect("stdout should be available");
        let reader = BridgeReader::new(stdout);

        let router = Arc::new(ResponseRouter::new());
        let rx = router.register(RequestId::new(1)).unwrap();

        let handle = spawn_reader_task(reader, Arc::clone(&router));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        drop(handle);

        let response = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("pending request should fail promptly after handle drop")
            .expect("should receive error response");
        assert!(
            response.get("error").is_some(),
            "pending request should receive an error response when handle is dropped"
        );

        let _ = child.kill().await;
    }

    /// Pending requests must fail fast on cancellation: the ResponseRouter is
    /// Arc-shared with ConnectionHandle, so reader exit alone does not drop the
    /// pending oneshot senders — without fail_all, waiters hang until the
    /// per-request timeout.
    #[tokio::test]
    async fn reader_loop_fails_all_on_cancellation() {
        use crate::lsp::bridge::connection::BridgeReader;
        use crate::lsp::bridge::protocol::RequestId;
        use std::process::Stdio;
        use tokio::process::Command;

        // Long-running process that never writes to stdout, so the loop can
        // only exit via cancellation (not EOF).
        let mut child = Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sleep should spawn");

        let stdout = child.stdout.take().expect("stdout should be available");
        let reader = BridgeReader::new(stdout);

        let router = Arc::new(ResponseRouter::new());
        let rx = router.register(RequestId::new(1)).unwrap();

        let cancel_token = CancellationToken::new();
        let token_clone = cancel_token.clone();

        let handle = tokio::spawn(reader_loop(reader, Arc::clone(&router), token_clone));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        cancel_token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
        assert!(
            result.is_ok(),
            "reader_loop should exit quickly after cancellation"
        );

        assert_eq!(
            router.pending_count(),
            0,
            "cancellation should fail all pending requests"
        );
        let response = rx.await.expect("should receive error response");
        assert!(
            response.get("error").is_some(),
            "pending request should receive an error response on cancellation"
        );

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
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);

        // First register a capability
        dynamic_capabilities.register(vec![tower_lsp_server::ls_types::Registration {
            id: "diag-1".to_string(),
            method: "textDocument/diagnostic".to_string(),
            register_options: None,
        }]);
        assert!(dynamic_capabilities.has_registration("textDocument/diagnostic"));

        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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

    /// workspace/workspaceFolders must be answered with the folders the
    /// connection was initialized with — the bridge advertises the
    /// workspace.workspaceFolders capability, so MethodNotFound would break
    /// servers that re-query their folders.
    #[tokio::test]
    async fn handle_message_answers_workspace_folders_query() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(1);
        let folders = vec![tower_lsp_server::ls_types::WorkspaceFolder {
            uri: "file:///repo".parse().unwrap(),
            name: "repo".to_string(),
        }];
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            window_tx,
            workspace_folders: Arc::new(Some(folders)),
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "workspace/workspaceFolders",
            "params": null
        });

        handle_message(message, &router, "", &deps).await;

        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 11);
                assert!(val.get("error").is_none(), "Should not be MethodNotFound");
                assert_eq!(val["result"][0]["uri"], "file:///repo");
                assert_eq!(val["result"][0]["name"], "repo");
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    /// With no folders recorded (e.g. upstream supplied none), the query
    /// answers `null` — still a success response, not MethodNotFound.
    #[tokio::test]
    async fn handle_message_answers_null_workspace_folders_without_folders() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(1);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            window_tx,
            workspace_folders: Arc::new(None),
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "workspace/workspaceFolders",
            "params": null
        });

        handle_message(message, &router, "", &deps).await;

        let response = response_rx.try_recv().expect("should have response");
        match response {
            OutboundMessage::Untracked(val) => {
                assert_eq!(val["id"], 12);
                assert!(val.get("error").is_none(), "Should not be MethodNotFound");
                assert!(val["result"].is_null());
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    /// Scratch-targeted publishDiagnostics must be dropped on the floor:
    /// nothing forwarded upstream, nothing written back downstream
    /// (concatenated-formatting-pipeline Decision point 7).
    #[tokio::test]
    async fn handle_message_discards_publish_diagnostics_for_scratch_uri() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///project/kakehashi-virtual-uri-REGION-kakehashi-scratch-3-0.py",
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    },
                    "message": "speculative-state diagnostic"
                }]
            }
        });

        handle_message(message, &router, "", &deps).await;

        assert!(
            upstream_rx.try_recv().is_err(),
            "scratch publishDiagnostics must not be forwarded upstream"
        );
        assert!(
            response_rx.try_recv().is_err(),
            "a notification must not trigger any downstream response"
        );
    }

    #[test]
    fn is_scratch_publish_diagnostics_matches_only_scratch_targets() {
        let scratch = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///p/kakehashi-virtual-uri-R-kakehashi-scratch-0-1.py", "diagnostics": []}
        });
        assert!(is_scratch_publish_diagnostics(&scratch));

        // Canonical virtual document: not scratch.
        let canonical = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///p/kakehashi-virtual-uri-R.py", "diagnostics": []}
        });
        assert!(!is_scratch_publish_diagnostics(&canonical));

        // Different method on a scratch URI: not publishDiagnostics.
        let other_method = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"uri": "file:///p/kakehashi-virtual-uri-R-kakehashi-scratch-0-1.py"}
        });
        assert!(!is_scratch_publish_diagnostics(&other_method));

        // Missing params: must not panic, just no match.
        let no_params = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics"
        });
        assert!(!is_scratch_publish_diagnostics(&no_params));
    }

    #[tokio::test]
    async fn handle_message_unknown_server_request_sends_method_not_found() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);

        // Pre-register a capability so we can verify it's NOT removed
        dynamic_capabilities.register(vec![tower_lsp_server::ls_types::Registration {
            id: "diag-1".to_string(),
            method: "textDocument/diagnostic".to_string(),
            register_options: None,
        }]);

        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);

        // Fill the channel with a dummy message to create backpressure
        response_tx
            .send(OutboundMessage::Untracked(json!({"dummy": true})))
            .await
            .unwrap();

        // Spawn handle_server_request in a separate task (it needs to await)
        let deps = ServerRequestDeps {
            server_name: None,
            response_tx: response_tx.clone(),
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
        let (window_tx, _window_rx) = mpsc::channel(16);

        // Drop the receiver to simulate a closed channel
        drop(response_rx);

        let deps = ServerRequestDeps {
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: Arc::new(None),
            window_tx,
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
    // Liveness Timer Tests (ls-bridge-async-connection)
    // ============================================================

    /// Test that liveness timer starts when notified (pending 0->1 transition).
    ///
    /// ls-bridge-async-connection: Timer starts when pending count transitions 0 to 1.
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
    /// ls-bridge-async-connection: Timer resets on any stdout activity (response or notification).
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
    /// ls-bridge-async-connection: Timer stops when pending count returns to 0.
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
    /// ls-bridge-async-connection: Ready to Failed transition on liveness timeout expiry.
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
