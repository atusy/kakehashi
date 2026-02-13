//! Connection handle for downstream language servers.
//!
//! This module provides the per-connection wrapper with state management,
//! request routing, and shutdown logic per ADR-0015.
//!
//! # Single-Writer Loop Architecture (ADR-0015)
//!
//! Each connection uses a channel-based single-writer pattern:
//! - `tx`: mpsc channel sender for outbound messages (FIFO ordering)
//! - `writer_handle`: Owns the SplitConnectionWriter, manages writer task
//!
//! Messages flow: Handler → tx channel → Writer Task → stdin
//!
//! This ensures strict FIFO ordering and non-blocking sends.

use std::io;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use log::warn;
use tokio::sync::mpsc;
use tower_lsp_server::ls_types::ServerCapabilities;

use super::connection_action::BridgeError;
use super::dynamic_capability_registry::DynamicCapabilityRegistry;
use super::{ConnectionState, UpstreamId};
use crate::lsp::bridge::actor::{
    OUTBOUND_QUEUE_CAPACITY, OutboundMessage, ReaderTaskHandle, ResponseRouter, WriterTaskHandle,
};
use crate::lsp::bridge::connection::SplitConnectionWriter;
use crate::lsp::bridge::protocol::{RequestId, build_exit_notification, build_shutdown_request};

/// Result of attempting to send a notification.
///
/// This enum preserves the distinction between temporary backpressure (queue full)
/// and terminal failure (channel closed), which is important for callers that
/// need to distinguish between these cases per the MessageSender trait contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationSendResult {
    /// Notification was successfully queued
    Queued,
    /// Queue is full (temporary backpressure - caller may retry)
    QueueFull,
    /// Channel is closed (terminal failure - writer task exited)
    ChannelClosed,
}

/// Handle wrapping a connection with its state (ADR-0015 per-connection state).
///
/// Each connection has its own lifecycle state that transitions:
/// - Initializing: spawn started, awaiting initialize response
/// - Ready: initialize/initialized handshake complete
/// - Failed: initialization failed (timeout, error, etc.)
///
/// # Architecture (ADR-0015 Single-Writer Loop)
///
/// Uses channel-based message passing for FIFO-ordered writes:
/// - `tx`: Channel sender for outbound messages (notifications and requests)
/// - `writer_handle`: Manages the writer task lifecycle and provides graceful shutdown
/// - `router`: Routes responses to oneshot waiters
/// - `reader_handle`: Background task reading from stdout
///
/// Request flow:
/// 1. Register request ID with router to get oneshot receiver
/// 2. Queue message via `send_request()` (non-blocking)
/// 3. Writer task writes to stdin in FIFO order
/// 4. Await oneshot receiver (no Mutex held)
pub(crate) struct ConnectionHandle {
    /// Connection state - uses std::sync::RwLock for fast, synchronous state checks
    state: std::sync::RwLock<ConnectionState>,
    /// Watch channel for async state change notifications.
    ///
    /// Subscribers can wait for state transitions (e.g., Initializing -> Ready)
    /// using `wait_for_ready()`. The Sender is stored here; receivers are created
    /// via `state_watch.subscribe()`.
    state_watch: tokio::sync::watch::Sender<ConnectionState>,
    /// Channel sender for outbound messages (ADR-0015 single-writer pattern).
    ///
    /// All notifications and requests are queued here and written to stdin
    /// by the writer task in FIFO order.
    tx: mpsc::Sender<OutboundMessage>,
    /// Handle to the writer task (ADR-0015).
    ///
    /// Manages the writer task lifecycle:
    /// - Owns the SplitConnectionWriter (including child process handle)
    /// - Provides graceful shutdown via 3-phase protocol
    /// - RAII cleanup on drop
    writer_handle: std::sync::Mutex<Option<WriterTaskHandle>>,
    /// Router for pending request tracking
    router: Arc<ResponseRouter>,
    /// Handle to the reader task.
    ///
    /// Used for:
    /// - RAII cleanup on drop (cancels reader task)
    /// - Liveness timer start notification (pending 0->1)
    reader_handle: ReaderTaskHandle,
    /// Atomic counter for generating unique downstream request IDs.
    ///
    /// Each upstream request may have the same ID (from different contexts),
    /// so we generate unique IDs for downstream requests to avoid
    /// "duplicate request ID" errors in the ResponseRouter.
    next_request_id: AtomicI64,
    /// Server capabilities from the initialize response.
    ///
    /// Set once after successful LSP handshake, read by request handlers
    /// to skip requests for unsupported capabilities.
    /// Uses OnceLock for "set once, read many" semantics
    /// (same pattern as SettingsManager::client_capabilities).
    server_capabilities: OnceLock<ServerCapabilities>,
    /// Dynamic capability registrations from server-initiated `client/registerCapability` requests.
    ///
    /// Updated by the reader task, queried by request handlers via `has_capability()`.
    dynamic_capabilities: Arc<DynamicCapabilityRegistry>,
}

impl ConnectionHandle {
    /// Create a new ConnectionHandle in Ready state (test helper).
    ///
    /// Used in tests where we need a connection handle without going through
    /// the full initialization flow. Spawns the writer task immediately.
    #[cfg(test)]
    pub(super) fn new(
        writer: SplitConnectionWriter,
        router: Arc<ResponseRouter>,
        reader_handle: ReaderTaskHandle,
    ) -> Self {
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        Self::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            dynamic_capabilities,
        )
    }

    /// Create a new ConnectionHandle with a specific initial state.
    ///
    /// Spawns the writer task to consume outbound messages from the channel.
    /// The writer task owns the SplitConnectionWriter and writes messages
    /// to stdin in FIFO order.
    ///
    /// # Writer Task Lifetime (ADR-0015)
    ///
    /// The writer task is spawned when the ConnectionHandle is created. All messages,
    /// including those sent during the LSP handshake, flow through the channel to
    /// ensure FIFO ordering and eliminate race conditions.
    ///
    /// # State Transitions (ADR-0015)
    /// - Start in `Initializing` state during LSP handshake
    /// - Transition to `Ready` on successful initialization
    /// - Transition to `Failed` on timeout or error
    pub(super) fn with_state(
        writer: SplitConnectionWriter,
        router: Arc<ResponseRouter>,
        reader_handle: ReaderTaskHandle,
        initial_state: ConnectionState,
        tx: mpsc::Sender<OutboundMessage>,
        rx: mpsc::Receiver<OutboundMessage>,
        dynamic_capabilities: Arc<DynamicCapabilityRegistry>,
    ) -> Self {
        use crate::lsp::bridge::actor::spawn_writer_task;

        // Spawn the writer task - it owns the writer and writes from the channel
        let writer_handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        let (state_watch, _receiver) = tokio::sync::watch::channel(initial_state);
        Self {
            state: std::sync::RwLock::new(initial_state),
            state_watch,
            tx,
            writer_handle: std::sync::Mutex::new(Some(writer_handle)),
            router,
            reader_handle,
            // Start at 2 because ID=1 is reserved for the initialize request
            // which is pre-registered before spawning the reader task.
            next_request_id: AtomicI64::new(2),
            server_capabilities: OnceLock::new(),
            dynamic_capabilities,
        }
    }

    /// Generate a unique downstream request ID.
    ///
    /// Each call returns the next ID in the sequence (2, 3, 4, ...).
    /// ID=1 is reserved for the initialize request.
    /// This ensures unique IDs for the ResponseRouter even when multiple
    /// upstream requests have the same ID.
    pub(crate) fn next_request_id(&self) -> i64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    // ========================================
    // Message Sending (ADR-0015 Single-Writer Loop)
    // ========================================

    /// Send a notification to the downstream server.
    ///
    /// Non-blocking: if the queue is full, the notification is dropped with WARN logging.
    /// This is per ADR-0015 backpressure semantics - notifications are fire-and-forget.
    ///
    /// # Returns
    /// - `NotificationSendResult::Queued` if the notification was queued successfully
    /// - `NotificationSendResult::QueueFull` if the queue is full (temporary backpressure)
    /// - `NotificationSendResult::ChannelClosed` if the channel is closed (terminal failure)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let notification = json!({"jsonrpc": "2.0", "method": "textDocument/didChange", ...});
    /// handle.send_notification(notification); // Fire-and-forget
    /// ```
    pub(crate) fn send_notification(&self, payload: serde_json::Value) -> NotificationSendResult {
        match self.tx.try_send(OutboundMessage::Untracked(payload)) {
            Ok(()) => NotificationSendResult::Queued,
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Notification dropped: queue full (capacity {})",
                    OUTBOUND_QUEUE_CAPACITY
                );
                NotificationSendResult::QueueFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Notification dropped: channel closed"
                );
                NotificationSendResult::ChannelClosed
            }
        }
    }

    /// Send a request to the downstream server.
    ///
    /// Non-blocking: if the queue is full, returns `BridgeError::QueueFull`.
    /// The request must already be registered with the router before calling this.
    ///
    /// # Cleanup Guarantee
    ///
    /// On failure, this method removes the router registration. The `router.remove()`
    /// call is idempotent, so it's safe even if the writer task has already cleaned
    /// up this entry due to a concurrent failure.
    ///
    /// # Arguments
    ///
    /// * `payload` - The JSON-RPC request payload
    /// * `request_id` - The request ID (must be pre-registered with router)
    ///
    /// # Returns
    ///
    /// * `Ok(())` if the request was queued successfully
    /// * `Err(BridgeError::QueueFull)` if the queue is full
    /// * `Err(BridgeError::ChannelClosed)` if the writer channel is closed
    ///
    /// # Example
    ///
    /// ```ignore
    /// // First register with router to get the response channel
    /// let (request_id, response_rx) = handle.register_request()?;
    /// let request = build_hover_request(request_id, ...);
    ///
    /// // Then queue the request
    /// handle.send_request(request, request_id)?;
    ///
    /// // Finally wait for response
    /// let response = handle.wait_for_response(request_id, response_rx).await?;
    /// ```
    pub(crate) fn send_request(
        &self,
        payload: serde_json::Value,
        request_id: RequestId,
    ) -> Result<(), BridgeError> {
        match self.tx.try_send(OutboundMessage::Tracked {
            payload,
            request_id,
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Clean up router registration on failure (idempotent)
                self.router.remove(request_id);
                Err(BridgeError::QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Clean up router registration on failure (idempotent)
                self.router.remove(request_id);
                Err(BridgeError::ChannelClosed)
            }
        }
    }

    /// Get the current connection state.
    ///
    /// Uses std::sync::RwLock for fast, non-blocking read access.
    /// Recovers from poisoned locks with logging per project convention.
    pub(crate) fn state(&self) -> ConnectionState {
        match self.state.read() {
            Ok(guard) => *guard,
            Err(poisoned) => {
                warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned state lock in ConnectionHandle::state()"
                );
                *poisoned.into_inner()
            }
        }
    }

    /// Wait for the connection to reach Ready state with timeout.
    ///
    /// This method is useful for diagnostic requests that want to wait for
    /// an initializing server instead of failing fast.
    ///
    /// # Returns
    /// - `Ok(())` if the connection reaches Ready state
    /// - `Err` with:
    ///   - `ErrorKind::TimedOut` if the timeout expires
    ///   - `ErrorKind::Other` if the server fails or shuts down during wait
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Wait up to 30s for server to be ready
    /// handle.wait_for_ready(Duration::from_secs(30)).await?;
    /// // Now safe to send requests
    /// ```
    pub(crate) async fn wait_for_ready(&self, timeout: Duration) -> io::Result<()> {
        let mut receiver = self.state_watch.subscribe();

        let wait_future = async {
            loop {
                // Copy state immediately to avoid holding borrow across await
                let current_state = *receiver.borrow();
                match current_state {
                    ConnectionState::Ready => return Ok(()),
                    ConnectionState::Failed => {
                        return Err(io::Error::other(
                            "bridge: server failed during initialization",
                        ));
                    }
                    ConnectionState::Closing | ConnectionState::Closed => {
                        return Err(io::Error::other("bridge: server shutdown during wait"));
                    }
                    ConnectionState::Initializing => {
                        // Wait for state change notification
                        if receiver.changed().await.is_err() {
                            return Err(io::Error::other("bridge: state channel closed"));
                        }
                    }
                }
            }
        };

        tokio::time::timeout(timeout, wait_future)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "bridge: wait for ready timeout")
            })?
    }

    /// Set the connection state.
    ///
    /// Used for state transitions during async initialization:
    /// - Initializing -> Ready (on successful init)
    /// - Initializing -> Failed (on timeout/error)
    ///
    /// Recovers from poisoned locks with logging per project convention.
    /// Also notifies all watchers of the state change via the watch channel.
    pub(super) fn set_state(&self, new_state: ConnectionState) {
        match self.state.write() {
            Ok(mut guard) => *guard = new_state,
            Err(poisoned) => {
                warn!(
                    target: "kakehashi::lock_recovery",
                    "Recovered from poisoned state lock in ConnectionHandle::set_state()"
                );
                *poisoned.into_inner() = new_state;
            }
        }
        // Notify watchers of state change. send_replace() is non-blocking and
        // always succeeds (it replaces the current value regardless of receivers).
        self.state_watch.send_replace(new_state);
    }

    /// Store server capabilities from the initialize response.
    ///
    /// Called once after successful LSP handshake, before transitioning to Ready.
    /// Subsequent calls are ignored (OnceLock semantics).
    pub(super) fn set_server_capabilities(&self, capabilities: ServerCapabilities) {
        // OnceLock::set() returns Err if already set - ignore since handshake
        // happens exactly once per connection.
        let _ = self.server_capabilities.set(capabilities);
    }

    /// Access the server capabilities from the initialize response.
    ///
    /// Returns `None` if capabilities haven't been set yet (server still initializing).
    /// Callers use typed field access (e.g., `c.diagnostic_provider.as_ref()`) for
    /// compile-time-safe capability checks.
    pub(crate) fn server_capabilities(&self) -> Option<&ServerCapabilities> {
        self.server_capabilities.get()
    }

    /// Access the dynamic capability registry for this connection.
    pub(crate) fn dynamic_capabilities(&self) -> &DynamicCapabilityRegistry {
        &self.dynamic_capabilities
    }

    /// Check if the downstream server supports a given LSP method,
    /// either via static capabilities (initialize response) or dynamic registration.
    ///
    /// Dynamic registrations take precedence since they may arrive after initialize.
    ///
    /// To add a new method, add a match arm mapping the LSP method string to the
    /// corresponding field in `ServerCapabilities`:
    /// ```ignore
    /// "textDocument/hover" => caps.hover_provider.is_some(),
    /// ```
    pub(crate) fn has_capability(&self, method: &str) -> bool {
        // Check dynamic registrations first (may arrive after initialize)
        if self.dynamic_capabilities().has_registration(method) {
            return true;
        }
        // Fall back to static capabilities from initialize response
        let Some(caps) = self.server_capabilities() else {
            return false;
        };
        match method {
            "textDocument/diagnostic" => caps.diagnostic_provider.is_some(),
            "textDocument/hover" => caps.hover_provider.is_some(),
            "textDocument/completion" => caps.completion_provider.is_some(),
            "textDocument/definition" => caps.definition_provider.is_some(),
            "textDocument/typeDefinition" => caps.type_definition_provider.is_some(),
            "textDocument/declaration" => caps.declaration_provider.is_some(),
            "textDocument/implementation" => caps.implementation_provider.is_some(),
            "textDocument/references" => caps.references_provider.is_some(),
            "textDocument/documentHighlight" => caps.document_highlight_provider.is_some(),
            "textDocument/signatureHelp" => caps.signature_help_provider.is_some(),
            "textDocument/rename" => caps.rename_provider.is_some(),
            "textDocument/moniker" => caps.moniker_provider.is_some(),
            "textDocument/inlayHint" => caps.inlay_hint_provider.is_some(),
            _ => false,
        }
    }

    /// Begin graceful shutdown of the connection.
    ///
    /// Transitions the connection to Closing state, which:
    /// - Rejects new requests with "bridge: connection closing" error
    /// - Signals that LSP shutdown/exit handshake should begin
    /// - Stops the liveness timer (ADR-0018: global shutdown overrides liveness)
    ///
    /// Note: The reader task continues running to receive the shutdown response.
    /// Only the liveness timer is disabled, not the entire reader.
    ///
    /// Valid from Ready or Initializing states per ADR-0015/ADR-0017.
    pub(crate) fn begin_shutdown(&self) {
        // Stop the liveness timer (but not the reader task) per ADR-0018
        // Global shutdown (Tier 3) overrides liveness timeout (Tier 2)
        // Reader continues running to receive shutdown response
        self.reader_handle.stop_liveness_timer();
        self.set_state(ConnectionState::Closing);
    }

    /// Complete the shutdown sequence.
    ///
    /// Transitions the connection to Closed state (terminal).
    /// Called after LSP shutdown/exit handshake completes or times out.
    ///
    /// Valid from Closing or Failed states per ADR-0015/ADR-0017.
    pub(crate) fn complete_shutdown(&self) {
        self.set_state(ConnectionState::Closed);
    }

    /// Perform graceful shutdown with LSP handshake (ADR-0017).
    ///
    /// Implements the LSP shutdown sequence:
    /// 1. Transition to Closing state (new operations rejected)
    /// 2. Stop writer task and reclaim the writer via 3-phase protocol
    /// 3. Send LSP "shutdown" request directly and wait for response
    /// 4. Send LSP "exit" notification directly
    /// 5. Force kill process (Unix: SIGTERM→SIGKILL escalation)
    /// 6. Transition to Closed state
    ///
    /// # Writer Task Synchronization (ADR-0015)
    ///
    /// The writer task must be stopped BEFORE sending shutdown/exit to ensure
    /// no concurrent writes to stdin. The 3-phase protocol:
    /// 1. Signal stop to writer task
    /// 2. Wait for idle confirmation (queue drained)
    /// 3. Receive writer back for direct use
    ///
    /// # Cleanup Guarantee
    ///
    /// Steps 5-6 (force kill and state transition) are **always executed**,
    /// even if the LSP handshake fails. This prevents connections from getting
    /// stuck in the Closing state.
    ///
    /// # Returns
    /// - Ok(()) if shutdown completed (gracefully or via force-kill)
    /// - Err only if the method couldn't complete at all (shouldn't happen)
    ///
    /// # Timeout Behavior
    ///
    /// This method has **no internal timeout** per ADR-0018. It waits indefinitely
    /// for the shutdown response. The caller (shutdown_all_with_timeout) is
    /// responsible for enforcing the global shutdown timeout.
    ///
    /// This design ensures:
    /// - Fast servers complete quickly without artificial delays
    /// - Slow servers use remaining time from the global budget
    /// - Single timeout ceiling prevents timeout multiplication (N * 5s)
    pub(crate) async fn graceful_shutdown(&self) -> io::Result<()> {
        // 1. Transition to Closing state
        self.begin_shutdown();

        // 2. Stop writer task and reclaim the writer via 3-phase protocol
        // This ensures no concurrent writes to stdin during shutdown
        let writer_handle = {
            let mut guard = self.writer_handle.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };

        let mut maybe_writer = if let Some(mut handle) = writer_handle {
            handle.stop_and_reclaim().await
        } else {
            None
        };

        // 3-4. Perform LSP handshake if we have the writer
        // Wrapped in async block to ensure cleanup (steps 5-6) always runs
        let handshake_result: io::Result<()> = async {
            let writer = match maybe_writer.as_mut() {
                Some(w) => w,
                None => {
                    log::warn!(
                        target: "kakehashi::bridge",
                        "Writer task lost during shutdown, skipping LSP handshake"
                    );
                    return Ok(());
                }
            };

            // 3. Send LSP shutdown request directly (writer task is stopped)
            let (request_id, response_rx) = self.register_request()?;
            let shutdown_request = build_shutdown_request(request_id);

            writer.write_message(&shutdown_request).await?;

            // Wait for shutdown response (no timeout - global timeout handles this)
            // Per ADR-0018: graceful_shutdown has no internal timeout
            match response_rx.await {
                Ok(_response) => {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "Shutdown response received, sending exit notification"
                    );
                }
                Err(_) => {
                    log::warn!(
                        target: "kakehashi::bridge",
                        "Shutdown response channel closed"
                    );
                }
            }

            // 4. Send exit notification directly (no response expected)
            let exit_notification = build_exit_notification();
            // Best effort - if this fails, process will be killed anyway
            let _ = writer.write_message(&exit_notification).await;

            Ok(())
        }
        .await;

        // 5. Force kill the process with platform-appropriate escalation
        // This ensures the process is terminated even if it ignores exit notification
        // ALWAYS executed, even if handshake failed
        //
        // Unix: SIGTERM->SIGKILL escalation with 2s grace period
        // Windows: TerminateProcess directly (no grace period)
        if let Some(ref mut writer) = maybe_writer {
            writer.force_kill_with_escalation().await;
        }

        // 6. Transition to Closed state
        // ALWAYS executed, even if handshake failed
        self.complete_shutdown();

        // Log handshake errors but return Ok since shutdown completed (via force-kill if needed)
        if let Err(e) = &handshake_result {
            log::debug!(
                target: "kakehashi::bridge",
                "LSP handshake had error during shutdown (connection force-killed): {}",
                e
            );
        }

        // Always return Ok - the connection is now Closed regardless of handshake result
        Ok(())
    }

    /// Get the response router for registering pending requests.
    pub(crate) fn router(&self) -> &Arc<ResponseRouter> {
        &self.router
    }

    /// Register a new request and return (request_id, response_receiver).
    ///
    /// Generates a unique request ID and registers it with the router.
    /// If this is the first pending request (pending 0->1), notifies the reader
    /// task to start the liveness timer (ADR-0014).
    ///
    /// Returns error if registration fails (should never happen with unique IDs).
    pub(crate) fn register_request(
        &self,
    ) -> io::Result<(RequestId, tokio::sync::oneshot::Receiver<serde_json::Value>)> {
        self.register_request_with_upstream(None)
    }

    /// Register a new request with upstream ID mapping for cancel forwarding.
    ///
    /// Like `register_request()`, but also stores a mapping from the upstream (client)
    /// request ID to the downstream (language server) request ID in the router's cancel_map.
    /// This enables $/cancelRequest forwarding by translating upstream IDs to downstream IDs.
    ///
    /// # Arguments
    /// * `upstream_id` - The original request ID from the upstream client (None for internal requests)
    ///
    /// Returns error if registration fails (should never happen with unique IDs).
    pub(crate) fn register_request_with_upstream(
        &self,
        upstream_id: Option<UpstreamId>,
    ) -> io::Result<(RequestId, tokio::sync::oneshot::Receiver<serde_json::Value>)> {
        // Check if this will be the first pending request (0->1 transition)
        //
        // SAFETY: This check is not atomic with register(), but the race is benign:
        // - If two threads both see pending_count()==0 and both call notify_liveness_start(),
        //   the second notification is dropped (try_send on capacity-1 channel).
        // - If thread A sees 0, thread B sees A's registration (count=1), only A notifies,
        //   which is correct behavior.
        // Either way, the timer starts exactly once when pending goes 0->1.
        let was_empty = self.router().pending_count() == 0;

        let request_id = RequestId::new(self.next_request_id());
        let response_rx = self
            .router()
            .register_with_upstream(request_id, upstream_id)
            .ok_or_else(|| io::Error::other("bridge: duplicate request ID"))?;

        // If pending went 0->1 and we're in Ready state, start liveness timer
        if was_empty && self.state() == ConnectionState::Ready {
            self.reader_handle.notify_liveness_start();
        }

        Ok((request_id, response_rx))
    }

    /// Wait for a response with timeout, cleaning up on timeout.
    ///
    /// Takes the oneshot receiver and request ID, waits for response with
    /// 30-second timeout. On timeout, removes the pending entry from router.
    ///
    /// Also checks for liveness timeout failure and transitions to Failed state
    /// if the reader task signaled a liveness timeout (ADR-0014 Phase 3).
    pub(crate) async fn wait_for_response(
        &self,
        request_id: RequestId,
        response_rx: tokio::sync::oneshot::Receiver<serde_json::Value>,
    ) -> io::Result<serde_json::Value> {
        use tokio::time::timeout;

        const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

        match timeout(REQUEST_TIMEOUT, response_rx).await {
            Ok(Ok(response)) => {
                // Check if this was an error response from liveness timeout
                // If so, transition to Failed state (ADR-0014 Phase 3)
                if self.reader_handle.check_liveness_failed() {
                    self.set_state(ConnectionState::Failed);
                }
                Ok(response)
            }
            Ok(Err(_)) => {
                // Channel closed - check if due to liveness timeout
                // If so, transition to Failed state (ADR-0014 Phase 3)
                if self.reader_handle.check_liveness_failed() {
                    self.set_state(ConnectionState::Failed);
                }
                Err(io::Error::other("bridge: response channel closed"))
            }
            Err(_) => {
                // Timeout - clean up pending entry
                self.router().remove(request_id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "bridge: request timeout",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::actor::{ResponseRouter, spawn_reader_task};
    use crate::lsp::bridge::connection::AsyncBridgeConnection;

    /// Create a default DynamicCapabilityRegistry for tests that don't need it.
    fn default_dynamic_caps() -> Arc<DynamicCapabilityRegistry> {
        Arc::new(DynamicCapabilityRegistry::new())
    }

    /// Test that ConnectionHandle provides unique request IDs via atomic counter.
    ///
    /// Each call to next_request_id() should return a unique, incrementing value.
    /// This is critical for avoiding "duplicate request ID" errors when multiple
    /// upstream requests have the same ID (they come from different contexts).
    #[tokio::test]
    async fn connection_handle_provides_unique_request_ids() {
        // Create a mock server process to get a real connection
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat".to_string(),
        ])
        .await
        .expect("should spawn cat process");

        // Split connection and spawn reader task
        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Wrap in ConnectionHandle
        let handle = ConnectionHandle::new(writer, router, reader_handle);

        // Get multiple request IDs - they should be unique and incrementing
        // Note: IDs start at 2 because ID=1 is reserved for the initialize request
        let id1 = handle.next_request_id();
        let id2 = handle.next_request_id();
        let id3 = handle.next_request_id();

        assert_eq!(
            id1, 2,
            "First user request ID should be 2 (1 is reserved for initialize)"
        );
        assert_eq!(id2, 3, "Second user request ID should be 3");
        assert_eq!(id3, 4, "Third user request ID should be 4");
    }

    /// Test that ConnectionHandle wraps connection with state (ADR-0015).
    /// State should start as Ready (since constructor is called after init handshake),
    /// and can transition via set_state().
    #[tokio::test]
    async fn connection_handle_wraps_connection_with_state() {
        // Create a mock server process to get a real connection
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat".to_string(),
        ])
        .await
        .expect("should spawn cat process");

        // Split connection and spawn reader task (new architecture)
        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Wrap in ConnectionHandle
        let handle = ConnectionHandle::new(writer, router, reader_handle);

        // Initial state should be Ready (ConnectionHandle is created after init handshake)
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Initial state should be Ready"
        );

        // Can transition to Failed
        handle.set_state(ConnectionState::Failed);
        assert_eq!(
            handle.state(),
            ConnectionState::Failed,
            "State should transition to Failed"
        );

        // Can send notification via channel (ADR-0015)
        let notification = serde_json::json!({"method": "test", "params": {}});
        let result = handle.send_notification(notification);
        assert_eq!(
            result,
            NotificationSendResult::Queued,
            "Should be able to send notification"
        );

        // Can access router
        let _router = handle.router();
        // Router is accessible (test passes if no panic)
    }

    /// Test that liveness timeout triggers Ready->Failed state transition (ADR-0014 Phase 3).
    ///
    /// When the liveness timer fires:
    /// 1. router.fail_all() sends error responses to pending requests
    /// 2. ConnectionHandle transitions to Failed state
    /// 3. Failed state triggers SpawnNew action on next request
    #[tokio::test]
    async fn liveness_timeout_transitions_to_failed_state() {
        use crate::lsp::bridge::actor::spawn_reader_task_with_liveness;
        use std::time::Duration;

        // Create an unresponsive server (consumes input, never outputs)
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Spawn reader with short liveness timeout
        let reader_handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(50)),
        );

        // Create ConnectionHandle in Ready state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Verify initial state is Ready
        assert_eq!(handle.state(), ConnectionState::Ready);

        // Register a request - this will notify the reader to start the liveness timer
        let (request_id, response_rx) = handle.register_request().expect("should register request");

        // Wait for response - this will block until liveness timeout fires
        // The response will be an error from router.fail_all()
        let result = handle.wait_for_response(request_id, response_rx).await;

        // Response should be Ok (error response from fail_all is still delivered)
        let response = result.expect("should receive error response from fail_all");
        assert!(
            response.get("error").is_some(),
            "Response should be an error: {:?}",
            response
        );
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("liveness timeout"),
            "Error should mention liveness timeout"
        );

        // After liveness timeout, connection should be in Failed state (Phase 3)
        assert_eq!(
            handle.state(),
            ConnectionState::Failed,
            "Connection should transition to Failed state on liveness timeout"
        );
    }

    /// Test that begin_shutdown() cancels the active liveness timer (ADR-0018 Phase 4).
    ///
    /// When global shutdown begins, the liveness timer should be disabled because
    /// global shutdown (Tier 3) overrides liveness timeout (Tier 2).
    #[tokio::test]
    async fn begin_shutdown_cancels_liveness_timer() {
        use crate::lsp::bridge::actor::spawn_reader_task_with_liveness;
        use std::time::Duration;

        // Create an unresponsive server
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Spawn reader with short liveness timeout
        let reader_handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(50)),
        );

        // Create ConnectionHandle in Ready state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Register a request to start the liveness timer
        let (_request_id, _response_rx) = handle.register_request().expect("should register");

        // Immediately begin shutdown - this should cancel the liveness timer
        handle.begin_shutdown();

        // Wait longer than the liveness timeout would have been
        tokio::time::sleep(Duration::from_millis(100)).await;

        // State should be Closing (from begin_shutdown), NOT Failed (from liveness timeout)
        // This proves the liveness timer was cancelled
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "State should be Closing, not Failed - liveness timer should have been cancelled"
        );
    }

    /// Test that liveness timer does not start in Closing state (ADR-0018 Phase 4).
    ///
    /// Once shutdown begins, new requests should not start the liveness timer
    /// because global shutdown (Tier 3) overrides liveness timeout (Tier 2).
    ///
    /// Test strategy: Register a request in Closing state, wait beyond timeout
    /// duration, verify connection stays in Closing (not Failed).
    #[tokio::test(start_paused = true)]
    async fn liveness_timer_does_not_start_in_closing_state() {
        use crate::lsp::bridge::actor::spawn_reader_task_with_liveness;
        use std::time::Duration;

        // Create an echo server
        let mut conn = AsyncBridgeConnection::spawn(vec!["cat".to_string()])
            .await
            .expect("should spawn cat process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Short timeout so we can wait past it
        let timeout = Duration::from_millis(50);

        // Spawn reader with liveness timeout
        let reader_handle =
            spawn_reader_task_with_liveness(reader, Arc::clone(&router), Some(timeout));

        // Create ConnectionHandle in Closing state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Closing,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Register a request - this should NOT start the liveness timer
        // because register_request checks `state == Ready` before notifying
        let result = handle.register_request();
        assert!(
            result.is_ok(),
            "register_request should succeed even in Closing state"
        );
        let (_request_id, _rx) = result.unwrap();

        // Advance time well beyond the timeout duration
        // If timer started, the connection would transition to Failed
        tokio::time::advance(timeout * 3).await;
        tokio::task::yield_now().await; // Allow pending tasks to execute

        // Connection should still be Closing, not Failed
        // This proves the liveness timer never started
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "Connection should remain Closing - liveness timer should NOT have started"
        );
    }

    /// Integration test: liveness timer resets on response activity.
    ///
    /// ADR-0014: Timer resets on any stdout activity.
    /// This verifies the full stack: request -> response -> timer reset.
    ///
    /// Test strategy: Send requests that will be echoed back, verify that
    /// receiving responses keeps the connection alive past the original timeout.
    #[tokio::test]
    async fn liveness_timer_resets_on_response_integration() {
        use crate::lsp::bridge::actor::spawn_reader_task_with_liveness;
        use serde_json::json;
        use std::time::Duration;

        // Create an echo server (cat echoes everything back)
        let mut conn = AsyncBridgeConnection::spawn(vec!["cat".to_string()])
            .await
            .expect("should spawn cat process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Spawn reader with liveness timeout (200ms)
        // This is long enough to avoid races but short enough for fast tests
        let reader_handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(200)),
        );

        // Create ConnectionHandle in Ready state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Register first request - this starts the liveness timer
        let (request_id1, response_rx1) = handle.register_request().expect("should register");

        // Send a response-shaped message that will be echoed back.
        // We deliberately omit "method" so the echo is classified as a Response
        // (not a ServerRequest), matching the routing expected by this test.
        let request1 = json!({
            "jsonrpc": "2.0",
            "id": request_id1.as_i64(),
            "result": null
        });
        handle
            .send_request(request1, request_id1)
            .expect("should send request");

        // Wait 100ms (half the timeout), then check we received the response
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Response should arrive (echo server echoes the request as-is)
        // This response resets the liveness timer
        let response1 = tokio::time::timeout(Duration::from_millis(50), response_rx1)
            .await
            .expect("should not timeout waiting for response")
            .expect("should receive response");

        // Verify we got the echoed response
        assert!(response1.get("id").is_some(), "Response should have id");

        // Connection should still be Ready (timer was reset when response arrived)
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Connection should be Ready after first response"
        );

        // Register second request to keep pending > 0 (timer stays active after reset)
        let (request_id2, response_rx2) = handle.register_request().expect("should register");

        // Send response-shaped message for echo (see first request comment)
        let request2 = json!({
            "jsonrpc": "2.0",
            "id": request_id2.as_i64(),
            "result": null
        });
        handle
            .send_request(request2, request_id2)
            .expect("should send request");

        // Wait another 150ms - this is past the original 200ms timeout from the start
        // but within 200ms of the timer reset from the first response
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Connection should STILL be Ready because timer was reset
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Timer should have reset on first response - connection should still be Ready"
        );

        // Receive second response to clean up
        let _response2 = tokio::time::timeout(Duration::from_millis(50), response_rx2)
            .await
            .expect("should not timeout")
            .expect("should receive response");

        // Still Ready after all responses
        assert_eq!(handle.state(), ConnectionState::Ready);
    }

    /// Test that register_request_with_upstream passes upstream ID to ResponseRouter.
    ///
    /// ADR-0015 Cancel Forwarding: When registering a request with an upstream ID,
    /// the router should store the mapping so we can later look up the downstream ID.
    #[tokio::test]
    async fn register_request_with_upstream_stores_mapping() {
        // Create a mock server process
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Create ConnectionHandle in Ready state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            default_dynamic_caps(),
        );

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        // Verify the mapping was stored in the router
        let looked_up = handle.router().lookup_downstream_id(&upstream_id);
        assert_eq!(
            looked_up,
            Some(downstream_id),
            "Router should have upstream->downstream mapping"
        );
    }

    /// Test that register_request_with_upstream with None works like register_request.
    #[tokio::test]
    async fn register_request_with_upstream_none_works() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            default_dynamic_caps(),
        );

        // Register without upstream ID
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(None)
            .expect("should register request");

        // Should have registered normally
        assert!(
            downstream_id.as_i64() >= 2,
            "Should have valid downstream ID"
        );
        assert_eq!(handle.router().pending_count(), 1);
    }

    /// Test that wait_for_ready returns immediately when already Ready.
    #[tokio::test]
    async fn wait_for_ready_returns_immediately_when_ready() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Create handle already in Ready state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            default_dynamic_caps(),
        );

        // Should return immediately
        let result = handle.wait_for_ready(Duration::from_millis(10)).await;
        assert!(result.is_ok(), "Should succeed immediately when Ready");
    }

    /// Test that wait_for_ready waits for Initializing -> Ready transition.
    #[tokio::test]
    async fn wait_for_ready_waits_for_initialization() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Create handle in Initializing state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Initializing,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Spawn a task that will transition to Ready after a delay
        let handle_clone = Arc::clone(&handle);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            handle_clone.set_state(ConnectionState::Ready);
        });

        // Wait for ready - should complete after state transition
        let result = handle.wait_for_ready(Duration::from_secs(1)).await;
        assert!(
            result.is_ok(),
            "Should succeed after transitioning to Ready"
        );
        assert_eq!(handle.state(), ConnectionState::Ready);
    }

    /// Test that wait_for_ready returns error on Failed state.
    #[tokio::test]
    async fn wait_for_ready_fails_on_failed_state() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Create handle in Initializing state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Initializing,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Spawn a task that will transition to Failed
        let handle_clone = Arc::clone(&handle);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            handle_clone.set_state(ConnectionState::Failed);
        });

        // Wait for ready - should fail due to Failed state
        let result = handle.wait_for_ready(Duration::from_secs(1)).await;
        assert!(result.is_err(), "Should fail when transitioning to Failed");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("failed during initialization"),
            "Error should mention initialization failure: {}",
            err
        );
    }

    /// Test that wait_for_ready times out correctly.
    #[tokio::test]
    async fn wait_for_ready_times_out() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Create handle in Initializing state that won't transition
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Initializing,
            tx,
            rx,
            default_dynamic_caps(),
        );

        // Wait with short timeout - should timeout
        let result = handle.wait_for_ready(Duration::from_millis(50)).await;
        assert!(result.is_err(), "Should timeout");
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// Test that wait_for_ready returns error on Closing/Closed state.
    #[tokio::test]
    async fn wait_for_ready_fails_on_shutdown() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        // Create handle in Initializing state
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Initializing,
            tx,
            rx,
            default_dynamic_caps(),
        ));

        // Spawn a task that will transition to Closing
        let handle_clone = Arc::clone(&handle);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            handle_clone.set_state(ConnectionState::Closing);
        });

        // Wait for ready - should fail due to shutdown
        let result = handle.wait_for_ready(Duration::from_secs(1)).await;
        assert!(result.is_err(), "Should fail when transitioning to Closing");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("shutdown during wait"),
            "Error should mention shutdown: {}",
            err
        );
    }

    // ========================================
    // Server Capabilities Tests
    // ========================================

    /// Create a ConnectionHandle in Ready state with a sink process.
    ///
    /// Spawns `cat > /dev/null` as a non-echoing server for tests that
    /// only need the handle's in-memory state (capabilities, request IDs, etc.)
    /// without reading responses back.
    async fn spawn_sink_handle() -> ConnectionHandle {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));
        ConnectionHandle::new(writer, router, reader_handle)
    }

    /// Test that server_capabilities returns None before set_server_capabilities is called.
    #[tokio::test]
    async fn server_capabilities_returns_none_before_init() {
        let handle = spawn_sink_handle().await;

        // Before setting capabilities, accessor should return None
        assert!(handle.server_capabilities().is_none());
    }

    /// Test that server_capabilities returns typed struct with set fields.
    #[tokio::test]
    async fn server_capabilities_returns_typed_struct() {
        use tower_lsp_server::ls_types::{
            CompletionOptions, DiagnosticOptions, DiagnosticServerCapabilities,
            HoverProviderCapability,
        };

        let handle = spawn_sink_handle().await;

        handle.set_server_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![".".to_string()]),
                ..Default::default()
            }),
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
                ..Default::default()
            })),
            ..Default::default()
        });

        let caps = handle.server_capabilities().expect("should be set");
        assert!(caps.hover_provider.is_some());
        assert!(caps.completion_provider.is_some());
        assert!(caps.diagnostic_provider.is_some());
    }

    /// Test that default capabilities have all fields as None.
    #[tokio::test]
    async fn server_capabilities_default_has_none_fields() {
        let handle = spawn_sink_handle().await;

        handle.set_server_capabilities(ServerCapabilities::default());

        let caps = handle.server_capabilities().expect("should be set");
        assert!(caps.hover_provider.is_none());
        assert!(caps.diagnostic_provider.is_none());
        assert!(caps.completion_provider.is_none());
    }

    // ========================================
    // Unified Capability Check Tests
    // ========================================

    /// Test has_capability returns false when neither static nor dynamic.
    #[tokio::test]
    async fn has_capability_returns_false_when_no_capabilities() {
        let handle = spawn_sink_handle().await;
        // No capabilities set, no dynamic registrations
        assert!(!handle.has_capability("textDocument/diagnostic"));
    }

    /// Test has_capability returns true with static capability only.
    #[tokio::test]
    async fn has_capability_returns_true_with_static_only() {
        use tower_lsp_server::ls_types::{DiagnosticOptions, DiagnosticServerCapabilities};
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                DiagnosticOptions::default(),
            )),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/diagnostic"));
    }

    /// Test has_capability returns true with dynamic registration only.
    #[tokio::test]
    async fn has_capability_returns_true_with_dynamic_only() {
        use tower_lsp_server::ls_types::Registration;
        let handle = spawn_sink_handle().await;
        // No static capabilities set
        handle.dynamic_capabilities().register(vec![Registration {
            id: "diag-1".to_string(),
            method: "textDocument/diagnostic".to_string(),
            register_options: None,
        }]);
        assert!(handle.has_capability("textDocument/diagnostic"));
    }

    /// Test has_capability returns true when both static and dynamic.
    #[tokio::test]
    async fn has_capability_returns_true_with_both() {
        use tower_lsp_server::ls_types::{
            DiagnosticOptions, DiagnosticServerCapabilities, Registration,
        };
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                DiagnosticOptions::default(),
            )),
            ..Default::default()
        });
        handle.dynamic_capabilities().register(vec![Registration {
            id: "diag-1".to_string(),
            method: "textDocument/diagnostic".to_string(),
            register_options: None,
        }]);
        assert!(handle.has_capability("textDocument/diagnostic"));
    }

    /// Test has_capability returns false for unknown methods (not mapped to static caps).
    #[tokio::test]
    async fn has_capability_returns_false_for_unknown_method() {
        use tower_lsp_server::ls_types::{DiagnosticOptions, DiagnosticServerCapabilities};
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                DiagnosticOptions::default(),
            )),
            ..Default::default()
        });
        // This method has no static capability mapping
        assert!(!handle.has_capability("textDocument/someUnknownMethod"));
    }

    // ========================================
    // Position-based Capability Check Tests
    // ========================================

    /// Test has_capability returns true for hover when hover_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_hover() {
        use tower_lsp_server::ls_types::HoverProviderCapability;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/hover"));
    }

    /// Test has_capability returns true for completion when completion_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_completion() {
        use tower_lsp_server::ls_types::CompletionOptions;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/completion"));
    }

    /// Test has_capability returns true for definition when definition_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_definition() {
        use tower_lsp_server::ls_types::OneOf;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/definition"));
    }

    /// Test has_capability returns true for typeDefinition when type_definition_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_type_definition() {
        use tower_lsp_server::ls_types::{OneOf, TypeDefinitionProviderCapability};
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/typeDefinition"));
    }

    /// Test has_capability returns true for declaration when declaration_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_declaration() {
        use tower_lsp_server::ls_types::DeclarationCapability;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            declaration_provider: Some(DeclarationCapability::Simple(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/declaration"));
    }

    /// Test has_capability returns true for implementation when implementation_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_implementation() {
        use tower_lsp_server::ls_types::ImplementationProviderCapability;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/implementation"));
    }

    /// Test has_capability returns true for references when references_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_references() {
        use tower_lsp_server::ls_types::OneOf;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            references_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/references"));
    }

    /// Test has_capability returns true for documentHighlight when provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_document_highlight() {
        use tower_lsp_server::ls_types::OneOf;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            document_highlight_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/documentHighlight"));
    }

    /// Test has_capability returns true for signatureHelp when provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_signature_help() {
        use tower_lsp_server::ls_types::SignatureHelpOptions;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            signature_help_provider: Some(SignatureHelpOptions::default()),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/signatureHelp"));
    }

    /// Test has_capability returns true for rename when rename_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_rename() {
        use tower_lsp_server::ls_types::OneOf;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            rename_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/rename"));
    }

    /// Test has_capability returns true for moniker when moniker_provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_moniker() {
        use tower_lsp_server::ls_types::OneOf;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            moniker_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/moniker"));
    }

    /// Test has_capability returns true for inlayHint when provider is set.
    #[tokio::test]
    async fn has_capability_returns_true_for_inlay_hint() {
        use tower_lsp_server::ls_types::OneOf;
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            inlay_hint_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        assert!(handle.has_capability("textDocument/inlayHint"));
    }

    /// Test has_capability returns false for each method when the provider is NOT set.
    #[tokio::test]
    async fn has_capability_returns_false_for_unset_providers() {
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities::default());

        let methods = [
            "textDocument/hover",
            "textDocument/completion",
            "textDocument/definition",
            "textDocument/typeDefinition",
            "textDocument/declaration",
            "textDocument/implementation",
            "textDocument/references",
            "textDocument/documentHighlight",
            "textDocument/signatureHelp",
            "textDocument/rename",
            "textDocument/moniker",
            "textDocument/inlayHint",
        ];
        for method in methods {
            assert!(
                !handle.has_capability(method),
                "has_capability({}) should return false with default capabilities",
                method
            );
        }
    }
}
