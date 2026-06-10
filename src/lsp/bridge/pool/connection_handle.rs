//! Per-connection wrapper for downstream language servers — state management,
//! request routing, and shutdown logic (ls-bridge-message-ordering).
//!
//! Outbound messages flow Handler → `tx` channel → writer task → stdin through a
//! single-writer loop, giving strict FIFO ordering and non-blocking sends.

use std::io;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tower_lsp_server::ls_types::{
    ColorProviderCapability, DeclarationCapability, FoldingRangeProviderCapability,
    HoverProviderCapability, ImplementationProviderCapability,
    LinkedEditingRangeServerCapabilities, OneOf, RenameOptions, ServerCapabilities,
    TypeDefinitionProviderCapability,
};

use super::connection_action::BridgeError;
use super::dynamic_capability_registry::DynamicCapabilityRegistry;
use super::{ConnectionState, UpstreamId};
use crate::error::LockResultExt;
use crate::lsp::bridge::actor::{
    OUTBOUND_QUEUE_CAPACITY, OutboundMessage, ReaderTaskHandle, ResponseRouter, WriterTaskHandle,
};
use crate::lsp::bridge::connection::SplitConnectionWriter;
use crate::lsp::bridge::protocol::{
    JsonRpcNotification, JsonRpcRequest, RequestId, build_exit_notification, build_shutdown_request,
};

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
    /// Payload could not be serialized to JSON (indicates a bug in the caller)
    SerializationFailed,
}

/// Per-connection state + I/O actors (ls-bridge-message-ordering single-writer loop).
///
/// Lifecycle: `Initializing` → `Ready` (after initialize/initialized) or
/// `Failed` (timeout/error). Shutdown then drives `begin_shutdown`
/// (`Initializing`/`Ready` → `Closing`) and `complete_shutdown`
/// (`Closing`/`Failed` → `Closed`, terminal). FIFO writes flow `tx` → writer
/// task → stdin; the reader task pushes incoming responses through `router` to
/// oneshot waiters so callers can await without holding any Mutex.
pub(crate) struct ConnectionHandle {
    /// Connection state - uses std::sync::RwLock for fast, synchronous state checks
    state: std::sync::RwLock<ConnectionState>,
    /// Watch channel for async state change notifications.
    ///
    /// Subscribers can wait for state transitions (e.g., Initializing -> Ready)
    /// using `wait_for_ready()`. The Sender is stored here; receivers are created
    /// via `state_watch.subscribe()`.
    state_watch: tokio::sync::watch::Sender<ConnectionState>,
    /// Channel sender for outbound messages (ls-bridge-message-ordering single-writer pattern).
    ///
    /// All notifications and requests are queued here and written to stdin
    /// by the writer task in FIFO order.
    tx: mpsc::Sender<OutboundMessage>,
    /// Writer task handle (ls-bridge-message-ordering): owns the `SplitConnectionWriter` (and child
    /// process), drives the 3-phase graceful shutdown, and does RAII cleanup on drop.
    writer_handle: std::sync::Mutex<Option<WriterTaskHandle>>,
    /// Router for pending request tracking
    router: Arc<ResponseRouter>,
    /// Reader task handle: RAII cancels the task on drop and signals the liveness
    /// timer start on the pending 0->1 transition.
    reader_handle: ReaderTaskHandle,
    /// Atomic counter for unique downstream request IDs. Upstream requests from
    /// different contexts may share an ID, so we mint fresh downstream IDs to avoid
    /// "duplicate request ID" collisions in the ResponseRouter.
    next_request_id: AtomicI64,
    /// Server capabilities from the initialize response, used to skip unsupported
    /// requests. `OnceLock` for set-once/read-many.
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

    /// Create a new ConnectionHandle with a specific initial state, spawning the
    /// writer task (ls-bridge-message-ordering). Even handshake messages flow through the channel so
    /// all writes keep FIFO ordering and avoid races against the writer task.
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

    /// Generate a unique downstream request ID (from 2; ID=1 is reserved for
    /// initialize). Uniqueness keeps the ResponseRouter unambiguous even when
    /// multiple upstream requests share the same ID.
    pub(crate) fn next_request_id(&self) -> i64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    // ========================================
    // Message Sending (ls-bridge-message-ordering Single-Writer Loop)
    // ========================================

    /// Queue a notification (fire-and-forget). Non-blocking: if the queue is
    /// full, the notification is dropped with WARN logging per ls-bridge-message-ordering
    /// backpressure semantics; see `NotificationSendResult` for the
    /// success/queue-full/channel-closed/serialization-failed outcomes.
    pub(crate) fn send_notification<P: serde::Serialize>(
        &self,
        notification: JsonRpcNotification<P>,
    ) -> NotificationSendResult {
        let payload = match serde_json::to_value(notification) {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    target: "kakehashi::bridge",
                    "Failed to serialize notification: {}", e
                );
                return NotificationSendResult::SerializationFailed;
            }
        };
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

    /// Queue a pre-registered request for the writer task. Non-blocking; returns
    /// `QueueFull` immediately rather than awaiting capacity. On any send error
    /// the router entry is removed (`router.remove()` is idempotent, so this is
    /// safe against concurrent cleanup by the writer task).
    pub(crate) fn send_request<P: serde::Serialize>(
        &self,
        request: JsonRpcRequest<P>,
        request_id: RequestId,
    ) -> Result<(), BridgeError> {
        let payload = match serde_json::to_value(request) {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    target: "kakehashi::bridge",
                    "Failed to serialize request: {}", e
                );
                self.router.remove(request_id);
                return Err(BridgeError::SerializationFailed);
            }
        };
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

    /// Send a raw payload for echo-server tests.
    ///
    /// Echo-server tests need to send a message that, when echoed back, is
    /// classified as a `Response` (has `id`, no `method`). Since `send_request`
    /// only accepts typed `JsonRpcRequest<P>` (which always includes `method`),
    /// this test helper bypasses the type constraint to send response-shaped
    /// payloads through the channel.
    #[cfg(test)]
    pub(crate) fn send_raw_for_echo_test(
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
                self.router.remove(request_id);
                Err(BridgeError::QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.router.remove(request_id);
                Err(BridgeError::ChannelClosed)
            }
        }
    }

    /// Get the current connection state. Uses a `std::sync::RwLock` for fast
    /// synchronous reads and recovers from poisoned locks per project convention.
    pub(crate) fn state(&self) -> ConnectionState {
        *self.state.read().recover_poison("ConnectionHandle::state")
    }

    /// Await `Ready` (up to `timeout`) instead of failing fast — used by
    /// diagnostic requests that prefer to wait through initialization.
    /// `TimedOut` on expiry; `Other` if the connection fails or shuts down.
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

    /// Set the connection state, recovering from poisoned locks per project
    /// convention and notifying watchers via the watch channel.
    pub(super) fn set_state(&self, new_state: ConnectionState) {
        *self
            .state
            .write()
            .recover_poison("ConnectionHandle::set_state") = new_state;
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

    /// Whether the downstream server supports `method`, via dynamic registration
    /// (preferred — may arrive after initialize) or static initialize capabilities.
    ///
    /// When extending the match: use `is_some()` only for struct-only fields
    /// (e.g. `completion_provider`); fields that are a `bool`-or-struct enum
    /// (e.g. `HoverProviderCapability`, `OneOf<bool, …>`) must use `matches!`
    /// to reject the explicit `false` / `Simple(false)` variant.
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
            "textDocument/hover" => matches!(
                caps.hover_provider,
                Some(HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_))
            ),
            "textDocument/completion" => caps.completion_provider.is_some(),
            "completionItem/resolve" => caps
                .completion_provider
                .as_ref()
                .and_then(|opts| opts.resolve_provider)
                .unwrap_or(false),
            "textDocument/definition" => {
                matches!(
                    caps.definition_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/typeDefinition" => matches!(
                caps.type_definition_provider,
                Some(
                    TypeDefinitionProviderCapability::Simple(true)
                        | TypeDefinitionProviderCapability::Options(_)
                )
            ),
            "textDocument/declaration" => matches!(
                caps.declaration_provider,
                Some(
                    DeclarationCapability::Simple(true)
                        | DeclarationCapability::RegistrationOptions(_)
                        | DeclarationCapability::Options(_)
                )
            ),
            "textDocument/implementation" => matches!(
                caps.implementation_provider,
                Some(
                    ImplementationProviderCapability::Simple(true)
                        | ImplementationProviderCapability::Options(_)
                )
            ),
            "textDocument/references" => {
                matches!(
                    caps.references_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/documentHighlight" => {
                matches!(
                    caps.document_highlight_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/signatureHelp" => caps.signature_help_provider.is_some(),
            "textDocument/rename" => {
                matches!(
                    caps.rename_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/prepareRename" => matches!(
                caps.rename_provider,
                Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    ..
                }))
            ),
            "textDocument/moniker" => {
                matches!(
                    caps.moniker_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/inlayHint" => {
                matches!(
                    caps.inlay_hint_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/linkedEditingRange" => matches!(
                caps.linked_editing_range_provider,
                Some(
                    LinkedEditingRangeServerCapabilities::Simple(true)
                        | LinkedEditingRangeServerCapabilities::Options(_)
                        | LinkedEditingRangeServerCapabilities::RegistrationOptions(_)
                )
            ),
            "textDocument/codeLens" => caps.code_lens_provider.is_some(),
            "textDocument/documentLink" => caps.document_link_provider.is_some(),
            "textDocument/foldingRange" => matches!(
                caps.folding_range_provider,
                Some(
                    FoldingRangeProviderCapability::Simple(true)
                        | FoldingRangeProviderCapability::FoldingProvider(_)
                        | FoldingRangeProviderCapability::Options(_)
                )
            ),
            "textDocument/formatting" => {
                matches!(
                    caps.document_formatting_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/rangeFormatting" => {
                matches!(
                    caps.document_range_formatting_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/documentSymbol" => {
                matches!(
                    caps.document_symbol_provider,
                    Some(OneOf::Left(true) | OneOf::Right(_))
                )
            }
            "textDocument/documentColor" | "textDocument/colorPresentation" => matches!(
                caps.color_provider,
                Some(
                    ColorProviderCapability::Simple(true)
                        | ColorProviderCapability::ColorProvider(_)
                        | ColorProviderCapability::Options(_)
                )
            ),
            _ => false,
        }
    }

    /// Begin graceful shutdown: transition to Closing (rejecting new requests) and
    /// stop the liveness timer, since global shutdown (Tier 3) overrides liveness
    /// (Tier 2) per ls-bridge-timeout-hierarchy. Only the timer is stopped — the reader task keeps
    /// running to receive the shutdown response. Valid from Ready or Initializing
    /// (ls-bridge-message-ordering/ls-bridge-graceful-shutdown).
    pub(crate) fn begin_shutdown(&self) {
        // Stop the liveness timer (but not the reader task) per ls-bridge-timeout-hierarchy
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
    /// Valid from Closing or Failed states per ls-bridge-message-ordering/ls-bridge-graceful-shutdown.
    pub(crate) fn complete_shutdown(&self) {
        self.set_state(ConnectionState::Closed);
    }

    /// LSP graceful shutdown: Closing → stop writer → shutdown/exit → force-kill → Closed (ls-bridge-graceful-shutdown).
    ///
    /// The writer task is reclaimed via a 3-phase stop (signal → idle confirm → receive)
    /// before sending `shutdown`/`exit` so nothing else writes to stdin concurrently (ls-bridge-message-ordering).
    /// Force-kill and the `Closed` transition always run even if the LSP handshake fails,
    /// so a stuck server cannot leave us in `Closing`.
    ///
    /// No internal timeout (ls-bridge-timeout-hierarchy): the caller (`shutdown_all_with_timeout`) enforces the
    /// global budget so a slow server can use leftover time without N×timeout multiplication.
    pub(crate) async fn graceful_shutdown(&self) -> io::Result<()> {
        // 1. Transition to Closing state
        self.begin_shutdown();

        // 2. Stop writer task and reclaim the writer via 3-phase protocol
        // This ensures no concurrent writes to stdin during shutdown
        let writer_handle = {
            let mut guard = self
                .writer_handle
                .lock()
                .recover_poison("ConnectionHandle::graceful_shutdown");
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
            // Per ls-bridge-timeout-hierarchy: graceful_shutdown has no internal timeout
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

    /// Register a new request and return (request_id, response_receiver). On the
    /// first pending request (0->1), notifies the reader to start the liveness
    /// timer (ls-bridge-async-connection). Errors only on a duplicate ID (should never happen).
    pub(crate) fn register_request(
        &self,
    ) -> io::Result<(RequestId, tokio::sync::oneshot::Receiver<serde_json::Value>)> {
        self.register_request_with_upstream(None)
    }

    /// Like `register_request()`, but also records the upstream→downstream ID
    /// mapping in the router's cancel_map so `$/cancelRequest` can be translated
    /// and forwarded (`None` for internal requests).
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

    /// Wait for a response with a 30-second timeout, removing the pending router
    /// entry on timeout. If the reader signaled a liveness timeout, transitions
    /// the connection to Failed (ls-bridge-async-connection Phase 3).
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
                // If so, transition to Failed state (ls-bridge-async-connection Phase 3)
                if self.reader_handle.check_liveness_failed() {
                    self.set_state(ConnectionState::Failed);
                }
                Ok(response)
            }
            Ok(Err(_)) => {
                // Channel closed - check if due to liveness timeout
                // If so, transition to Failed state (ls-bridge-async-connection Phase 3)
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

    /// Test that ConnectionHandle wraps connection with state (ls-bridge-message-ordering).
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

        // Can send notification via channel (ls-bridge-message-ordering)
        let notification = JsonRpcNotification::new("test", serde_json::json!({}));
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

    /// Test that liveness timeout triggers Ready->Failed state transition (ls-bridge-async-connection Phase 3).
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

    /// Test that begin_shutdown() cancels the active liveness timer (ls-bridge-timeout-hierarchy Phase 4).
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

    /// Test that liveness timer does not start in Closing state (ls-bridge-timeout-hierarchy Phase 4).
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
    /// ls-bridge-async-connection: Timer resets on any stdout activity.
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
        let payload1 = json!({"jsonrpc": "2.0", "id": request_id1.as_i64(), "result": null});
        handle
            .send_raw_for_echo_test(payload1, request_id1)
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
        let payload2 = json!({"jsonrpc": "2.0", "id": request_id2.as_i64(), "result": null});
        handle
            .send_raw_for_echo_test(payload2, request_id2)
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
    /// ls-bridge-message-ordering Cancel Forwarding: When registering a request with an upstream ID,
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
        let looked_up = handle.router().lookup_downstream_ids(&upstream_id);
        assert_eq!(
            looked_up,
            vec![downstream_id],
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
    // Position-based Capability Check Tests (table-driven)
    // ========================================

    /// Table-driven test: has_capability returns true for each supported method
    /// when the corresponding capability is enabled.
    ///
    /// Each entry sets exactly one capability field, then asserts has_capability
    /// returns true for the matching method.
    #[tokio::test]
    async fn has_capability_returns_true_for_enabled_providers() {
        use tower_lsp_server::ls_types::{
            ColorProviderCapability, ColorProviderOptions, CompletionOptions,
            DeclarationCapability, DeclarationOptions, DeclarationRegistrationOptions,
            DocumentLinkOptions, HoverProviderCapability, ImplementationProviderCapability, OneOf,
            SignatureHelpOptions, StaticTextDocumentColorProviderOptions,
            TextDocumentRegistrationOptions, TypeDefinitionProviderCapability,
        };

        type CapCase = (&'static str, Box<dyn Fn(&mut ServerCapabilities)>);
        let cases: Vec<CapCase> = vec![
            (
                "textDocument/hover",
                Box::new(|c| {
                    c.hover_provider = Some(HoverProviderCapability::Simple(true));
                }),
            ),
            (
                "textDocument/completion",
                Box::new(|c| {
                    c.completion_provider = Some(CompletionOptions::default());
                }),
            ),
            (
                "textDocument/definition",
                Box::new(|c| {
                    c.definition_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/typeDefinition",
                Box::new(|c| {
                    c.type_definition_provider =
                        Some(TypeDefinitionProviderCapability::Simple(true));
                }),
            ),
            (
                "textDocument/declaration",
                Box::new(|c| {
                    c.declaration_provider = Some(DeclarationCapability::Simple(true));
                }),
            ),
            (
                "textDocument/implementation",
                Box::new(|c| {
                    c.implementation_provider =
                        Some(ImplementationProviderCapability::Simple(true));
                }),
            ),
            (
                "textDocument/references",
                Box::new(|c| {
                    c.references_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/documentHighlight",
                Box::new(|c| {
                    c.document_highlight_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/signatureHelp",
                Box::new(|c| {
                    c.signature_help_provider = Some(SignatureHelpOptions::default());
                }),
            ),
            (
                "textDocument/documentLink",
                Box::new(|c| {
                    c.document_link_provider = Some(DocumentLinkOptions {
                        resolve_provider: None,
                        work_done_progress_options: Default::default(),
                    });
                }),
            ),
            (
                "textDocument/rename",
                Box::new(|c| {
                    c.rename_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/prepareRename",
                Box::new(|c| {
                    c.rename_provider = Some(OneOf::Right(RenameOptions {
                        prepare_provider: Some(true),
                        work_done_progress_options: Default::default(),
                    }));
                }),
            ),
            (
                "textDocument/moniker",
                Box::new(|c| {
                    c.moniker_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/inlayHint",
                Box::new(|c| {
                    c.inlay_hint_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/documentSymbol",
                Box::new(|c| {
                    c.document_symbol_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/documentColor",
                Box::new(|c| {
                    c.color_provider = Some(ColorProviderCapability::Simple(true));
                }),
            ),
            (
                "textDocument/colorPresentation",
                Box::new(|c| {
                    c.color_provider = Some(ColorProviderCapability::Simple(true));
                }),
            ),
            (
                "textDocument/documentColor",
                Box::new(|c| {
                    c.color_provider = Some(ColorProviderCapability::ColorProvider(
                        ColorProviderOptions {},
                    ));
                }),
            ),
            // Declaration — RegistrationOptions variant
            (
                "textDocument/declaration",
                Box::new(|c| {
                    c.declaration_provider = Some(DeclarationCapability::RegistrationOptions(
                        DeclarationRegistrationOptions {
                            declaration_options: DeclarationOptions {
                                work_done_progress_options: Default::default(),
                            },
                            text_document_registration_options: TextDocumentRegistrationOptions {
                                document_selector: None,
                            },
                            static_registration_options: Default::default(),
                        },
                    ));
                }),
            ),
            // Declaration — Options variant
            (
                "textDocument/declaration",
                Box::new(|c| {
                    c.declaration_provider =
                        Some(DeclarationCapability::Options(DeclarationOptions {
                            work_done_progress_options: Default::default(),
                        }));
                }),
            ),
            // DocumentColor — Options variant (StaticTextDocumentColorProviderOptions)
            (
                "textDocument/documentColor",
                Box::new(|c| {
                    c.color_provider = Some(ColorProviderCapability::Options(
                        StaticTextDocumentColorProviderOptions {
                            document_selector: None,
                            id: None,
                        },
                    ));
                }),
            ),
        ];

        for (method, set_cap) in &cases {
            let handle = spawn_sink_handle().await;
            let mut caps = ServerCapabilities::default();
            set_cap(&mut caps);
            handle.set_server_capabilities(caps);

            assert!(
                handle.has_capability(method),
                "has_capability({method}) should return true when enabled",
            );
        }
    }

    /// Table-driven test: has_capability returns false when capabilities are
    /// explicitly disabled via `Simple(false)` / `OneOf::Left(false)`.
    ///
    /// LSP spec allows servers to advertise `Some(false)` to explicitly disable
    /// a capability.
    #[tokio::test]
    async fn has_capability_returns_false_for_explicitly_disabled() {
        use tower_lsp_server::ls_types::{
            ColorProviderCapability, DeclarationCapability, HoverProviderCapability,
            ImplementationProviderCapability, OneOf, TypeDefinitionProviderCapability,
        };

        type CapCase = (&'static str, Box<dyn Fn(&mut ServerCapabilities)>);
        let cases: Vec<CapCase> = vec![
            (
                "textDocument/hover",
                Box::new(|c| {
                    c.hover_provider = Some(HoverProviderCapability::Simple(false));
                }),
            ),
            (
                "textDocument/definition",
                Box::new(|c| {
                    c.definition_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/typeDefinition",
                Box::new(|c| {
                    c.type_definition_provider =
                        Some(TypeDefinitionProviderCapability::Simple(false));
                }),
            ),
            (
                "textDocument/declaration",
                Box::new(|c| {
                    c.declaration_provider = Some(DeclarationCapability::Simple(false));
                }),
            ),
            (
                "textDocument/implementation",
                Box::new(|c| {
                    c.implementation_provider =
                        Some(ImplementationProviderCapability::Simple(false));
                }),
            ),
            (
                "textDocument/references",
                Box::new(|c| {
                    c.references_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/documentHighlight",
                Box::new(|c| {
                    c.document_highlight_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/rename",
                Box::new(|c| {
                    c.rename_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/prepareRename",
                Box::new(|c| {
                    // rename supported but prepareRename not advertised
                    c.rename_provider = Some(OneOf::Left(true));
                }),
            ),
            (
                "textDocument/moniker",
                Box::new(|c| {
                    c.moniker_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/inlayHint",
                Box::new(|c| {
                    c.inlay_hint_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/documentSymbol",
                Box::new(|c| {
                    c.document_symbol_provider = Some(OneOf::Left(false));
                }),
            ),
            (
                "textDocument/documentColor",
                Box::new(|c| {
                    c.color_provider = Some(ColorProviderCapability::Simple(false));
                }),
            ),
        ];

        for (method, set_cap) in &cases {
            let handle = spawn_sink_handle().await;
            let mut caps = ServerCapabilities::default();
            set_cap(&mut caps);
            handle.set_server_capabilities(caps);

            assert!(
                !handle.has_capability(method),
                "has_capability({method}) should return false when explicitly disabled",
            );
        }
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
            "textDocument/documentLink",
            "textDocument/rename",
            "textDocument/prepareRename",
            "textDocument/moniker",
            "textDocument/inlayHint",
            "textDocument/documentColor",
            "textDocument/colorPresentation",
        ];
        for method in methods {
            assert!(
                !handle.has_capability(method),
                "has_capability({}) should return false with default capabilities",
                method
            );
        }
    }

    // ========================================
    // completionItem/resolve capability tests
    // ========================================

    #[tokio::test]
    async fn completion_resolve_capability_true_when_resolve_provider_true() {
        use tower_lsp_server::ls_types::CompletionOptions;

        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(handle.has_capability("completionItem/resolve"));
    }

    #[tokio::test]
    async fn completion_resolve_capability_false_when_resolve_provider_false() {
        use tower_lsp_server::ls_types::CompletionOptions;

        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(!handle.has_capability("completionItem/resolve"));
    }

    #[tokio::test]
    async fn completion_resolve_capability_false_when_resolve_provider_absent() {
        use tower_lsp_server::ls_types::CompletionOptions;

        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                resolve_provider: None,
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(!handle.has_capability("completionItem/resolve"));
    }

    #[tokio::test]
    async fn completion_resolve_capability_false_when_no_completion_provider() {
        let handle = spawn_sink_handle().await;
        handle.set_server_capabilities(ServerCapabilities::default());

        assert!(!handle.has_capability("completionItem/resolve"));
    }
}
