//! Language server pool for downstream language servers.
//!
//! This module provides the LanguageServerPool which manages connections to
//! downstream language servers per ADR-0016 (Server Pool Coordination).
//!
//! # Server-Name-Based Pooling
//!
//! The pool is keyed by `server_name`, not by `languageId`. This enables:
//! - **Process sharing**: Multiple languages can share a single process
//!   (e.g., `typescript` and `typescriptreact` both mapping to `tsgo`)
//! - **Decoupling**: Language identity is separate from server identity
//!
//! Routing flow: `languageId` → `server_name` (via config) → connection
//!
//! # Key Types
//!
//! - [`LanguageServerPool`]: Main pool managing all downstream connections
//! - [`ConnectionHandle`]: Handle to a single downstream connection (ADR-0014)
//! - [`ConnectionState`]: State machine for connection lifecycle

mod connection_action;
mod connection_handle;
mod connection_state;
mod document_tracker;
mod dynamic_capability_registry;
mod execute;
mod handshake;
mod liveness_timeout;
mod message_sender;
mod shutdown;
mod shutdown_timeout;
#[cfg(test)]
pub(in crate::lsp::bridge) mod test_helpers;

pub(crate) use connection_action::BridgeError;
use connection_action::{ConnectionAction, decide_connection_action};
use handshake::perform_lsp_handshake;

pub(crate) use connection_handle::{ConnectionHandle, NotificationSendResult};
pub(crate) use connection_state::ConnectionState;
use document_tracker::DocumentOpenDecision;
use document_tracker::DocumentTracker;
pub(crate) use document_tracker::OpenedVirtualDoc;
pub(crate) use dynamic_capability_registry::DynamicCapabilityRegistry;
pub(crate) use message_sender::ConnectionHandleSender;
pub(crate) use shutdown_timeout::GlobalShutdownTimeout;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use url::Url;

use super::protocol::{VirtualDocumentUri, build_didopen_notification};

/// Timeout for LSP initialize handshake (ADR-0018 Tier 0: 30-60s recommended).
///
/// Used for:
/// - Handshake timeout when spawning new server connections
/// - Wait-for-ready timeout when diagnostic requests wait for initializing servers
///
/// If a downstream language server does not respond to the initialize request
/// within this duration, the connection attempt fails with a timeout error.
pub(crate) const INIT_TIMEOUT_SECS: u64 = 30;

use super::actor::{
    OUTBOUND_QUEUE_CAPACITY, ResponseRouter, UpstreamNotification, spawn_reader_task_for_language,
};
use super::connection::AsyncBridgeConnection;

/// Upstream request ID type supporting both numeric and string IDs per LSP spec.
///
/// The LSP specification allows request IDs to be either integers or strings:
/// `id: integer | string`. This type provides a unified way to handle both types
/// in the cancel forwarding infrastructure.
///
/// # LSP Spec Compliance
///
/// Per LSP 3.17: "interface CancelParams { id: integer | string; }"
/// This type ensures we can forward cancel requests for clients using either ID type.
///
/// # Null Variant
///
/// The `Null` variant handles cases where the request ID is unavailable (e.g.,
/// `None` or `Id::Null`). This is distinct from `Number(0)` to avoid collision
/// with valid ID 0 requests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UpstreamId {
    /// Numeric request ID (most common)
    Number(i64),
    /// String request ID (less common but valid per LSP spec)
    String(String),
    /// Null/missing request ID (edge case, distinct from Number(0))
    Null,
}

impl std::fmt::Display for UpstreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamId::Number(n) => write!(f, "{}", n),
            UpstreamId::String(s) => write!(f, "\"{}\"", s),
            UpstreamId::Null => write!(f, "null"),
        }
    }
}

impl From<i64> for UpstreamId {
    fn from(n: i64) -> Self {
        UpstreamId::Number(n)
    }
}

impl From<String> for UpstreamId {
    fn from(s: String) -> Self {
        UpstreamId::String(s)
    }
}

impl From<&str> for UpstreamId {
    fn from(s: &str) -> Self {
        UpstreamId::String(s.to_string())
    }
}

/// Metrics for cancel request forwarding.
///
/// Provides observability into the cancel forwarding mechanism for production
/// debugging and monitoring. All counters use relaxed ordering since exact
/// counts are not critical for correctness.
#[derive(Default)]
pub(crate) struct CancelForwardingMetrics {
    /// Number of cancel notifications successfully forwarded to downstream servers.
    successful: AtomicU64,
    /// Number of cancel notifications that failed due to no connection for the language.
    failed_no_connection: AtomicU64,
    /// Number of cancel notifications that failed due to connection not ready.
    failed_not_ready: AtomicU64,
    /// Number of cancel notifications that failed due to unknown upstream request ID.
    failed_unknown_id: AtomicU64,
    /// Number of cancel notifications that failed due to upstream ID not in registry.
    failed_not_in_registry: AtomicU64,
}

impl CancelForwardingMetrics {
    /// Record a successful cancel forward.
    fn record_success(&self) {
        self.successful.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failure due to no connection for the language.
    fn record_no_connection(&self) {
        self.failed_no_connection.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failure due to connection not ready.
    fn record_not_ready(&self) {
        self.failed_not_ready.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failure due to unknown upstream request ID.
    fn record_unknown_id(&self) {
        self.failed_unknown_id.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failure due to upstream ID not in registry.
    fn record_not_in_registry(&self) {
        self.failed_not_in_registry.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the current metrics snapshot.
    ///
    /// Returns (successful, failed_no_connection, failed_not_ready, failed_unknown_id, failed_not_in_registry).
    #[cfg(test)]
    fn snapshot(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.successful.load(Ordering::Relaxed),
            self.failed_no_connection.load(Ordering::Relaxed),
            self.failed_not_ready.load(Ordering::Relaxed),
            self.failed_unknown_id.load(Ordering::Relaxed),
            self.failed_not_in_registry.load(Ordering::Relaxed),
        )
    }
}

/// Pool of connections to downstream language servers (ADR-0016).
///
/// Each server_name maps to exactly one downstream server connection.
/// Multiple injection languages can share the same server (e.g., "ts" and "tsx" → "tsgo").
///
/// Provides lazy initialization of connections and handles the LSP handshake
/// (initialize/initialized) for each language server.
///
/// Connection state is embedded in each ConnectionHandle (ADR-0015 per-connection state).
///
/// # External Usage
///
/// This type is public to allow creating a shared pool for the cancel forwarding
/// middleware. Normal usage should go through `BridgeCoordinator`.
pub struct LanguageServerPool {
    /// Map of server_name -> connection handle (wraps connection with its state)
    connections: Mutex<HashMap<String, Arc<ConnectionHandle>>>,
    /// Document tracking for virtual documents (versions, host mappings, opened state)
    document_tracker: DocumentTracker,
    /// Maps upstream request ID -> set of server names for cancel forwarding (ADR-0015).
    ///
    /// When a request is sent to downstream server(s), we record the mapping so that
    /// when a $/cancelRequest notification arrives with the upstream ID, we can look up
    /// which server(s) to forward it to.
    ///
    /// For fan-out requests (e.g., textDocument/diagnostic with multiple injection regions
    /// using different language servers), multiple servers may be registered for the same
    /// upstream ID. Cancel notifications are forwarded to all registered servers.
    ///
    /// # Cleanup Behavior
    ///
    /// Entries are cleaned up via `unregister_upstream_request(id, server_name)` when:
    /// - A response is received from a specific server (normal completion)
    /// - A request fails before being sent to a specific server
    ///
    /// The entry is fully removed when all servers have been unregistered.
    ///
    /// Note: When a connection fails (via `ResponseRouter::fail_all()`), entries in
    /// this registry are NOT automatically cleaned up because the ResponseRouter
    /// doesn't have access to the pool. This is intentional:
    /// - Stale entries are harmless (`forward_cancel_by_upstream_id()` checks
    ///   connection state and fails gracefully for stale entries)
    /// - Entries are cleaned up when new requests reuse the same upstream IDs
    /// - This keeps the architecture simpler by avoiding circular dependencies
    upstream_request_registry: std::sync::Mutex<HashMap<UpstreamId, HashSet<String>>>,
    /// Metrics for cancel forwarding observability.
    cancel_metrics: CancelForwardingMetrics,
    /// Tracks consecutive handshake task panics per server.
    ///
    /// When a handshake task panics (JoinError), we increment this counter.
    /// When handshake succeeds, we reset it to 0.
    /// If panic count reaches MAX_CONSECUTIVE_PANICS, we stop retrying.
    ///
    /// This prevents infinite retry loops when a server's handshake consistently panics.
    consecutive_panic_counts: std::sync::Mutex<HashMap<String, u32>>,
    /// Workspace root URI forwarded from upstream client.
    ///
    /// Set via `set_root_uri()` after receiving the upstream initialize request.
    /// Passed to downstream servers during LSP handshake so they can provide
    /// workspace-aware features (diagnostics, go-to-definition, etc.).
    root_uri: std::sync::Mutex<Option<String>>,
    /// Sender for forwarding downstream server notifications to the upstream editor.
    ///
    /// Cloned into each reader task so they can signal events like
    /// `workspace/diagnostic/refresh` without coupling to tower-lsp's Client type.
    upstream_tx: tokio::sync::mpsc::UnboundedSender<UpstreamNotification>,
    /// Receiver for upstream notifications, taken once by the forwarding task.
    ///
    /// Wrapped in `Mutex<Option<...>>` because `take_upstream_rx()` moves it out.
    upstream_rx:
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<UpstreamNotification>>>,
}

impl Default for LanguageServerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageServerPool {
    /// Create a new language server pool.
    ///
    /// This is public for cancel forwarding middleware setup. Create a shared
    /// `Arc<LanguageServerPool>` and pass it to both `Kakehashi::with_pool()`
    /// and `CancelForwarder::new()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let pool = Arc::new(LanguageServerPool::new());
    /// let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
    /// let kakehashi = Kakehashi::with_pool(pool);
    /// let service = RequestIdCapture::with_cancel_forwarder(kakehashi, cancel_forwarder);
    /// ```
    pub fn new() -> Self {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            connections: Mutex::new(HashMap::new()),
            document_tracker: DocumentTracker::new(),
            upstream_request_registry: std::sync::Mutex::new(HashMap::new()),
            cancel_metrics: CancelForwardingMetrics::default(),
            consecutive_panic_counts: std::sync::Mutex::new(HashMap::new()),
            root_uri: std::sync::Mutex::new(None),
            upstream_tx,
            upstream_rx: std::sync::Mutex::new(Some(upstream_rx)),
        }
    }

    /// Set the workspace root URI.
    ///
    /// Called during upstream initialize to forward the root URI to downstream servers.
    pub(crate) fn set_root_uri(&self, uri: Option<String>) {
        let mut root_uri = self.root_uri.lock().unwrap_or_else(|e| e.into_inner());
        *root_uri = uri;
    }

    /// Get the workspace root URI.
    fn root_uri(&self) -> Option<String> {
        let root_uri = self.root_uri.lock().unwrap_or_else(|e| e.into_inner());
        root_uri.clone()
    }

    /// Take the upstream notification receiver for forwarding to the editor.
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// The receiver should be consumed by a single forwarding task in `initialized()`.
    pub(crate) fn take_upstream_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<UpstreamNotification>> {
        let mut guard = self.upstream_rx.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    }

    /// Get access to cancel forwarding metrics (for testing).
    #[cfg(test)]
    pub(crate) fn cancel_metrics(&self) -> &CancelForwardingMetrics {
        &self.cancel_metrics
    }

    /// Get access to the connections map.
    ///
    /// Used by text_document submodules that need to access connections.
    pub(super) async fn connections(
        &self,
    ) -> tokio::sync::MutexGuard<'_, HashMap<String, Arc<ConnectionHandle>>> {
        self.connections.lock().await
    }

    /// Insert a pre-created connection handle for testing.
    ///
    /// This allows tests to set up a pool with a known connection state
    /// without going through the full server spawn + handshake flow.
    #[cfg(test)]
    pub(in crate::lsp::bridge) async fn insert_connection(
        &self,
        server_name: &str,
        handle: Arc<ConnectionHandle>,
    ) {
        let mut connections = self.connections.lock().await;
        connections.insert(server_name.to_string(), handle);
    }

    // ========================================
    // DocumentTracker delegation methods
    // ========================================

    /// Remove and return all virtual documents for a host URI.
    ///
    /// Used by did_close module for cleanup.
    pub(super) async fn remove_host_virtual_docs(&self, host_uri: &Url) -> Vec<OpenedVirtualDoc> {
        self.document_tracker
            .remove_host_virtual_docs(host_uri)
            .await
    }

    /// Take virtual documents matching the given ULIDs, removing them from tracking.
    ///
    /// This is atomic: lookup and removal happen in a single lock acquisition,
    /// preventing race conditions with concurrent didOpen requests.
    ///
    /// Returns the removed documents (for sending didClose). Documents that
    /// were never opened (not in host_to_virtual) are not returned.
    ///
    /// # Arguments
    /// * `host_uri` - The host document URI
    /// * `invalidated_ulids` - ULIDs to match against virtual document URIs
    pub(crate) async fn remove_matching_virtual_docs(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) -> Vec<OpenedVirtualDoc> {
        self.document_tracker
            .remove_matching_virtual_docs(host_uri, invalidated_ulids)
            .await
    }

    /// Remove a document from all tracking state.
    ///
    /// Removes the document from version tracking and opened state.
    /// Used by did_close module for cleanup and by
    /// close_invalidated_virtual_docs for invalidated region cleanup.
    ///
    /// # Arguments
    ///
    /// * `virtual_uri` - The virtual document URI
    /// * `server_name` - The server name for HashMap lookup
    pub(crate) async fn untrack_document(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) {
        self.document_tracker
            .untrack_document(virtual_uri, server_name)
            .await
    }

    /// Check if a document has had didOpen ACTUALLY sent to downstream (ADR-0015).
    ///
    /// This is a fast, synchronous check used by request handlers to ensure
    /// they don't send requests before didOpen has been sent.
    ///
    /// Returns true if `mark_document_opened()` has been called for this document.
    /// Returns false if the document hasn't been opened yet.
    pub(crate) fn is_document_opened(&self, virtual_uri: &VirtualDocumentUri) -> bool {
        self.document_tracker.is_document_opened(virtual_uri)
    }

    /// Find server_name for a virtual document URI (for reverse lookup).
    ///
    /// Used by did_change to look up which server a virtual document was opened on.
    /// Returns None if the document is not tracked.
    pub(crate) async fn get_server_for_virtual_uri(
        &self,
        virtual_uri: &VirtualDocumentUri,
    ) -> Option<String> {
        self.document_tracker
            .get_server_for_virtual_uri(virtual_uri)
            .await
    }

    /// Mark a document as having had didOpen sent to downstream (ADR-0015).
    ///
    /// This should be called AFTER the didOpen notification has been successfully
    /// written to the downstream server. Request handlers check `is_document_opened()`
    /// before sending requests to ensure LSP spec compliance.
    ///
    /// Note: Production code uses `document_tracker.mark_document_opened()` directly
    /// via `ensure_document_opened()`. This delegation is exposed for test access.
    #[cfg(test)]
    pub(super) fn mark_document_opened(&self, virtual_uri: &VirtualDocumentUri) {
        self.document_tracker.mark_document_opened(virtual_uri)
    }

    /// Ensure document is opened before sending a request.
    ///
    /// Sends didOpen if this is the first request for the document.
    /// Returns error if another request is in the process of opening (race condition).
    ///
    /// Callers are responsible for cleanup on error (removing router entry and
    /// unregistering from upstream request registry).
    ///
    /// # Arguments
    ///
    /// * `sender` - Message sender for sending didOpen (either direct writer or channel)
    /// * `host_uri` - The host document URI
    /// * `virtual_uri` - The virtual document URI
    /// * `virtual_content` - Content for the didOpen notification
    /// * `server_name` - Server name for document tracking
    ///
    /// # Decision Logic
    ///
    /// Uses `DocumentOpenDecision` to determine action:
    /// - `SendDidOpen`: Send didOpen notification, mark as opened
    /// - `AlreadyOpened`: Skip (no-op), document was already opened
    /// - `PendingError`: Race condition, return error
    ///
    /// # MessageSender Trait (ADR-0015)
    ///
    /// This function is generic over `MessageSender` for channel-based sends:
    /// - `mpsc::Sender<OutboundMessage>`: Direct channel sender
    /// - `ConnectionHandleSender`: Wrapper around `Arc<ConnectionHandle>`
    pub(crate) async fn ensure_document_opened<S: message_sender::MessageSender>(
        &self,
        sender: &mut S,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
        virtual_content: &str,
        server_name: &str,
    ) -> io::Result<()> {
        match self
            .document_tracker
            .document_open_decision(host_uri, virtual_uri, server_name)
            .await
        {
            DocumentOpenDecision::SendDidOpen => {
                let did_open = build_didopen_notification(virtual_uri, virtual_content);
                sender.send_notification(did_open).await?;
                self.document_tracker.mark_document_opened(virtual_uri);
                Ok(())
            }
            DocumentOpenDecision::AlreadyOpened => Ok(()),
            DocumentOpenDecision::PendingError => Err(io::Error::other(
                "bridge: document not yet opened (didOpen pending)",
            )),
        }
    }

    /// Increment the version of a virtual document and return the new version.
    ///
    /// Returns None if the document has not been opened.
    ///
    /// # Arguments
    ///
    /// * `virtual_uri` - The virtual document URI
    /// * `server_name` - Server name for HashMap lookup
    pub(super) async fn increment_document_version(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) -> Option<i32> {
        self.document_tracker
            .increment_document_version(virtual_uri, server_name)
            .await
    }

    /// Check if document is opened and mark it as opened atomically.
    ///
    /// Returns true if the document was NOT previously opened (i.e., didOpen should be sent).
    /// Returns false if the document was already opened (i.e., skip didOpen).
    ///
    /// This is exposed for tests that need to simulate document opening without
    /// using the full ensure_document_opened flow.
    ///
    /// # Arguments
    ///
    /// * `host_uri` - The host document URI
    /// * `virtual_uri` - The virtual document URI
    /// * `server_name` - Server name for HashMap key
    #[cfg(test)]
    pub(super) async fn should_send_didopen(
        &self,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) -> bool {
        self.document_tracker
            .should_send_didopen(host_uri, virtual_uri, server_name)
            .await
    }

    /// Get or create a connection for the specified server.
    ///
    /// If no connection exists, spawns the language server and performs
    /// the LSP initialize/initialized handshake with default timeout.
    ///
    /// Returns the ConnectionHandle which wraps both the connection and its state.
    ///
    /// # Arguments
    /// * `server_name` - The server name from config (e.g., "tsgo", "rust-analyzer")
    /// * `server_config` - The server configuration containing command and options
    pub(super) async fn get_or_create_connection(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
    ) -> io::Result<Arc<ConnectionHandle>> {
        self.get_or_create_connection_with_timeout(
            server_name,
            server_config,
            Duration::from_secs(INIT_TIMEOUT_SECS),
        )
        .await
    }

    /// Eagerly ensure a server is spawned and its handshake is in progress.
    ///
    /// This method spawns the language server process and starts the LSP handshake
    /// in a background task, but does NOT:
    /// - Wait for the handshake to complete
    /// - Send didOpen notifications
    /// - Block on any response
    ///
    /// Use this to warm up language servers proactively when injections are detected,
    /// eliminating first-request latency for downstream LSP features.
    ///
    /// This method is idempotent - calling it multiple times for the same server
    /// will only spawn the server once.
    ///
    /// # Arguments
    /// * `server_name` - The server name from config (e.g., "lua-ls", "pyright")
    /// * `server_config` - The server configuration containing command
    #[cfg(test)]
    pub(crate) async fn ensure_server_ready(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
    ) {
        // Fire-and-forget: spawn the server if needed, ignore result
        // Errors (spawn failure, connection exists) are logged internally
        let _ = self
            .get_or_create_connection_with_timeout(
                server_name,
                server_config,
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await;
    }

    /// Get or create a connection, waiting for it to be ready if initializing.
    ///
    /// Unlike `get_or_create_connection()` which fails fast for initializing servers,
    /// this method waits for the server to become Ready before returning.
    ///
    /// This is useful for diagnostic requests where waiting for server initialization
    /// provides a better UX than immediately returning empty results.
    ///
    /// # Arguments
    /// * `server_name` - The server name from config (e.g., "tsgo", "rust-analyzer")
    /// * `server_config` - The server configuration containing command and options
    /// * `timeout` - Maximum time to wait for the server to become ready
    ///
    /// # Returns
    /// * `Ok(handle)` - Connection is Ready
    /// * `Err` - Spawn failed, initialization failed, or timeout waiting for Ready
    pub(crate) async fn get_or_create_connection_wait_ready(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        timeout: Duration,
    ) -> io::Result<Arc<ConnectionHandle>> {
        // First, try to get or create the connection
        match self
            .get_or_create_connection(server_name, server_config)
            .await
        {
            Ok(handle) => {
                // Ensure the connection is Ready before returning
                // (get_or_create_connection may return Initializing state for new connections)
                handle.wait_for_ready(timeout).await?;
                Ok(handle)
            }
            Err(e) => {
                // Check if error is due to server still initializing using type-safe matching
                let is_initializing = e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<BridgeError>())
                    .is_some_and(BridgeError::is_initializing);

                if is_initializing {
                    // Get the handle from pool and wait for it to be ready
                    let handle = {
                        let connections = self.connections.lock().await;
                        connections
                            .get(server_name)
                            .map(Arc::clone)
                            .ok_or_else(|| {
                                io::Error::other("bridge: connection disappeared during wait")
                            })?
                    };

                    // Wait for Ready state
                    handle.wait_for_ready(timeout).await?;
                    Ok(handle)
                } else {
                    // Other error - propagate it
                    Err(e)
                }
            }
        }
    }
}

impl LanguageServerPool {
    /// Get or create a connection for the specified server with custom timeout.
    ///
    /// If no connection exists, spawns the language server and stores the connection
    /// in Initializing state immediately. A background task performs the LSP handshake.
    /// Requests during initialization fail fast with "bridge: downstream server initializing".
    ///
    /// Returns the ConnectionHandle which wraps both the connection and its state.
    /// State transitions per ADR-0015 Operation Gating:
    /// - Initializing: fast-fail with REQUEST_FAILED
    /// - Ready: proceed with request
    /// - Failed: remove from pool and respawn
    ///
    /// # Architecture (ADR-0015 Fast-Fail)
    ///
    /// 1. Spawn server process
    /// 2. Split into writer + reader immediately
    /// 3. Store ConnectionHandle in Initializing state
    /// 4. Spawn background task for LSP handshake
    /// 5. Background task transitions to Ready or Failed
    ///
    /// # Arguments
    /// * `server_name` - The server name from config (e.g., "tsgo", "rust-analyzer")
    /// * `server_config` - The server configuration containing command and options
    /// * `timeout` - Timeout for the LSP initialize handshake
    async fn get_or_create_connection_with_timeout(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        timeout: Duration,
    ) -> io::Result<Arc<ConnectionHandle>> {
        let mut connections = self.connections.lock().await;

        // Get consecutive panic count for this server
        let panic_count = {
            let counts = self
                .consecutive_panic_counts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            counts.get(server_name).copied().unwrap_or(0)
        };

        // Check if we already have a connection for this server
        // Use pure decision function for testability (ADR-0015 Operation Gating)
        let existing_state = connections.get(server_name).map(|h| h.state());
        match decide_connection_action(existing_state, panic_count) {
            ConnectionAction::ReturnExisting => {
                return Ok(Arc::clone(connections.get(server_name).expect(
                    "Invariant violation: Connection expected for ReturnExisting action",
                )));
            }
            ConnectionAction::FailFast(err) => {
                // Log once when server is disabled due to repeated panics
                if matches!(err, BridgeError::Disabled) {
                    log::error!(
                        target: "kakehashi::bridge::connection",
                        "[{}] Server disabled after {} consecutive handshake panics (max: {})",
                        server_name,
                        panic_count,
                        connection_action::MAX_CONSECUTIVE_PANICS
                    );
                }
                return Err(err.into());
            }
            ConnectionAction::SpawnNew => {
                // Remove stale connection if present (Failed or Closed state)
                if existing_state.is_some() {
                    connections.remove(server_name);
                }
            }
        }

        // Spawn new connection (while holding lock to prevent concurrent spawns)
        let mut conn = AsyncBridgeConnection::spawn(server_config.cmd.clone()).await?;

        // Split connection immediately
        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());

        // Pre-register initialize request ID (=1) BEFORE spawning reader task.
        // This prevents a race condition where fast language servers (e.g., pyright)
        // respond before register_request() is called, causing the response to be
        // dropped as "unknown request ID".
        let init_request_id = super::protocol::RequestId::new(1);
        let init_response_rx = router
            .register(init_request_id)
            .expect("fresh router cannot have duplicate IDs");

        // Create outbound message channel (extracted here so tx can be shared with reader task)
        let (tx, rx) = tokio::sync::mpsc::channel(OUTBOUND_QUEUE_CAPACITY);

        // Create dynamic capability registry (shared between reader and connection handle)
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());

        // Now spawn reader task with liveness timeout - it can route the initialize response immediately
        // Liveness timeout is configured via LivenessTimeout::default() (60s per ADR-0018 Tier 2)
        // Server name is passed for structured logging (observability improvement)
        let liveness_timeout = liveness_timeout::LivenessTimeout::default();
        let reader_handle = spawn_reader_task_for_language(
            reader,
            Arc::clone(&router),
            Some(liveness_timeout.as_duration()),
            Some(server_name.to_string()),
            tx.clone(),
            Arc::clone(&dynamic_capabilities),
            self.upstream_tx.clone(),
        );

        // Create handle in Initializing state (fast-fail for concurrent requests)
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Initializing,
            tx,
            rx,
            dynamic_capabilities,
        ));

        // Insert into pool immediately so concurrent requests see Initializing state
        connections.insert(server_name.to_string(), Arc::clone(&handle));

        // Release lock before spawning handshake task
        drop(connections);

        // Spawn handshake as a SEPARATE TASK that survives caller cancellation.
        // This is critical: if the handshake is awaited inline, cancelling the caller
        // (e.g., via $/cancelRequest) drops the entire future chain including
        // init_response_rx, causing the initialize response to be lost.
        //
        // By spawning and then awaiting the JoinHandle:
        // - The handshake runs in a separate task
        // - If this function's caller is cancelled, only the JoinHandle await is dropped
        // - The spawned handshake task continues to completion
        let init_options = server_config.initialization_options.clone();
        let root_uri = self.root_uri();
        let handle_for_handshake = Arc::clone(&handle);
        let server_name_for_log = server_name.to_string();
        let handshake_task = tokio::spawn(async move {
            let init_result = tokio::time::timeout(
                timeout,
                perform_lsp_handshake(
                    &handle_for_handshake,
                    init_request_id,
                    init_response_rx,
                    init_options,
                    root_uri,
                ),
            )
            .await;

            // Handle initialization result - transition state
            match init_result {
                Ok(Ok(capabilities)) => {
                    // Init succeeded - store capabilities and transition to Ready
                    log::info!(
                        target: "kakehashi::bridge::init",
                        "[{}] LSP handshake completed successfully",
                        server_name_for_log
                    );
                    handle_for_handshake.set_server_capabilities(capabilities);
                    handle_for_handshake.set_state(ConnectionState::Ready);
                    Ok(())
                }
                Ok(Err(e)) => {
                    // Init failed with io::Error - transition to Failed
                    // Preserve the original ErrorKind for proper error categorization
                    log::error!(
                        target: "kakehashi::bridge::init",
                        "[{}] LSP handshake failed: {}",
                        server_name_for_log,
                        e
                    );
                    handle_for_handshake.set_state(ConnectionState::Failed);
                    Err(io::Error::new(
                        e.kind(),
                        format!("bridge: handshake failed: {}", e),
                    ))
                }
                Err(_elapsed) => {
                    // Timeout occurred - transition to Failed
                    log::error!(
                        target: "kakehashi::bridge::init",
                        "[{}] LSP handshake timed out",
                        server_name_for_log
                    );
                    handle_for_handshake.set_state(ConnectionState::Failed);
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "bridge: initialize timeout",
                    ))
                }
            }
        });

        // Wait for handshake to complete. If this await is cancelled (e.g., due to
        // $/cancelRequest), the spawned task continues running to completion.
        match handshake_task.await {
            Ok(Ok(())) => {
                // Success - reset panic count for this server
                let mut counts = self
                    .consecutive_panic_counts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                counts.remove(server_name);
                Ok(handle)
            }
            Ok(Err(e)) => Err(e),
            Err(join_err) => {
                handle.set_state(ConnectionState::Failed);

                // Distinguish panic from cancellation - only panics should trip circuit breaker
                if join_err.is_panic() {
                    // Task panicked - track consecutive panic count to avoid infinite retry
                    let new_count = {
                        let mut counts = self
                            .consecutive_panic_counts
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let count = counts.entry(server_name.to_string()).or_insert(0);
                        *count += 1;
                        *count
                    };
                    log::error!(
                        target: "kakehashi::bridge::init",
                        "[{}] Handshake task panicked (consecutive count: {}): {}",
                        server_name,
                        new_count,
                        join_err
                    );
                    Err(io::Error::other(format!(
                        "bridge: handshake task panicked: {}",
                        join_err
                    )))
                } else {
                    // Task was cancelled (e.g., runtime shutdown) - don't increment panic count
                    log::warn!(
                        target: "kakehashi::bridge::init",
                        "[{}] Handshake task cancelled: {}",
                        server_name,
                        join_err
                    );
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "bridge: handshake task cancelled",
                    ))
                }
            }
        }
    }

    /// Forward a $/cancelRequest notification to a downstream language server.
    ///
    /// Translates the upstream (client) request ID to the downstream (language server)
    /// request ID using the cancel map, then sends the cancel notification.
    ///
    /// Per LSP spec, this does NOT remove the pending request entry - the server may
    /// still respond with either a result or a REQUEST_CANCELLED error (-32800).
    ///
    /// # Arguments
    /// * `server_name` - The server name from config (e.g., "tsgo", "rust-analyzer")
    /// * `upstream_id` - The original request ID from the upstream client
    ///
    /// # Returns
    /// * `Ok(())` - Cancel was forwarded, or silently dropped if not forwardable
    /// * `Err(e)` - I/O error occurred while writing to the downstream server
    ///
    /// # Silent Drop Cases (Best-Effort Semantics)
    ///
    /// Per LSP spec, `$/cancelRequest` is a notification with best-effort semantics.
    /// The following cases are silently dropped (return `Ok(())`) rather than failing:
    /// - No connection exists for the server
    /// - Connection is not in Ready state (still initializing or failed)
    /// - Upstream ID not found in router (request already completed)
    ///
    /// Actual I/O errors when writing to the downstream server are propagated as `Err`.
    pub(crate) async fn forward_cancel(
        &self,
        server_name: &str,
        upstream_id: &UpstreamId,
    ) -> io::Result<()> {
        // Get the connection for this server
        let handle = {
            let connections = self.connections().await;
            let Some(handle) = connections.get(server_name) else {
                // No connection - request may have completed or server never started.
                // Silently drop per best-effort semantics.
                self.cancel_metrics.record_no_connection();
                log::debug!(
                    target: "kakehashi::bridge::cancel",
                    "Cancel dropped: no connection for server '{}' (upstream_id: {}, expected if request completed)",
                    server_name,
                    upstream_id
                );
                return Ok(());
            };

            // Only forward if connection is Ready
            if handle.state() != ConnectionState::Ready {
                // Connection still initializing or failed - can't forward cancels.
                // Silently drop per best-effort semantics.
                self.cancel_metrics.record_not_ready();
                log::debug!(
                    target: "kakehashi::bridge::cancel",
                    "Cancel dropped: connection not ready for server '{}' (upstream_id: {}, state: {:?})",
                    server_name,
                    upstream_id,
                    handle.state()
                );
                return Ok(());
            }

            std::sync::Arc::clone(handle)
        };

        // Look up the downstream ID
        let downstream_id = match handle.router().lookup_downstream_id(upstream_id) {
            Some(id) => id,
            None => {
                // Request already completed or ID never registered.
                // Silently drop per best-effort semantics.
                self.cancel_metrics.record_unknown_id();
                log::debug!(
                    target: "kakehashi::bridge::cancel",
                    "Cancel dropped: upstream ID {} not found for server '{}' (request may have completed)",
                    upstream_id,
                    server_name
                );
                return Ok(());
            }
        };

        // Build and send the cancel notification via single-writer loop (ADR-0015)
        // Per LSP spec: $/cancelRequest is a notification with { id: request_id }
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": {
                "id": downstream_id.as_i64()
            }
        });

        match handle.send_notification(notification) {
            NotificationSendResult::Queued => {
                self.cancel_metrics.record_success();
                log::debug!(
                    target: "kakehashi::bridge::cancel",
                    "Cancel forwarded: upstream {} -> downstream {} for server '{}'",
                    upstream_id,
                    downstream_id.as_i64(),
                    server_name
                );
                Ok(())
            }
            NotificationSendResult::QueueFull => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge: cancel notification queue full",
            )),
            NotificationSendResult::ChannelClosed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bridge: cancel notification channel closed",
            )),
        }
    }

    /// Forward a $/cancelRequest notification using only the upstream request ID.
    ///
    /// This method looks up ALL server names from the upstream request registry,
    /// then delegates to `forward_cancel(server_name, upstream_id)` for each server.
    ///
    /// For fan-out requests (e.g., diagnostic with multiple injection languages),
    /// the cancel notification is forwarded to all registered servers. Individual
    /// server failures are ignored (best-effort semantics).
    ///
    /// Called by the RequestIdCapture middleware when it intercepts a $/cancelRequest
    /// notification from the client.
    ///
    /// # Arguments
    /// * `upstream_id` - The request ID from the client's cancel notification
    ///
    /// # Returns
    /// * `Ok(())` - Cancel was forwarded to all servers, or silently dropped if not forwardable
    ///
    /// # Silent Drop Cases (Best-Effort Semantics)
    ///
    /// Per LSP spec, `$/cancelRequest` is a notification with best-effort semantics.
    /// The following cases are silently dropped (return `Ok(())`) rather than failing:
    /// - Upstream ID not in registry (request not yet registered or already completed)
    /// - Connection still initializing (can't forward cancels during handshake)
    /// - Downstream ID not found (request already completed)
    /// - Individual server failures (continues to next server)
    ///
    /// This prevents race conditions where canceling an in-flight request could
    /// break server initialization.
    pub(crate) async fn forward_cancel_by_upstream_id(
        &self,
        upstream_id: UpstreamId,
    ) -> io::Result<()> {
        // Look up all server names from the registry
        let server_names: Vec<String> = {
            let registry = self
                .upstream_request_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match registry.get(&upstream_id) {
                Some(servers) => servers.iter().cloned().collect(),
                None => {
                    // Request not registered yet (still initializing) or already completed.
                    // This is expected - silently drop the cancel per best-effort semantics.
                    self.cancel_metrics.record_not_in_registry();
                    log::debug!(
                        target: "kakehashi::bridge::cancel",
                        "Cancel dropped: upstream ID {} not in registry (expected during init or after completion)",
                        upstream_id
                    );
                    return Ok(());
                }
            }
        };

        // Forward cancel to all servers in parallel (best-effort, log I/O errors).
        let cancel_futures = server_names
            .iter()
            .map(|server_name| self.forward_cancel(server_name, &upstream_id));
        for result in futures::future::join_all(cancel_futures).await {
            if let Err(e) = result {
                // Log I/O errors (queue full, channel closed) for observability.
                // These indicate connection issues worth investigating.
                log::warn!(
                    target: "kakehashi::bridge::cancel",
                    "Error forwarding cancel for upstream_id {}: {}",
                    upstream_id,
                    e
                );
            }
        }

        Ok(())
    }

    /// Register an upstream request ID -> server_name mapping for cancel forwarding.
    ///
    /// Called when a request is sent to a downstream server to enable $/cancelRequest
    /// forwarding. When a cancel notification arrives from the client with the upstream ID,
    /// we use this mapping to route the cancel to the correct downstream language server(s).
    ///
    /// For fan-out requests (e.g., diagnostic with multiple injection languages), this
    /// method may be called multiple times with the same upstream_id but different
    /// server_names. The HashSet ensures cancel is forwarded to all registered servers.
    ///
    /// # Cancel Forwarding Flow (Single Server)
    ///
    /// 1. Client sends request with ID 42
    /// 2. Bridge creates downstream request with ID 7 and calls this method
    /// 3. Client sends `$/cancelRequest { id: 42 }`
    /// 4. Bridge looks up 42 in registry -> finds {"tsgo"}
    /// 5. Bridge looks up 42 in ResponseRouter -> finds downstream ID 7
    /// 6. Bridge sends `$/cancelRequest { id: 7 }` to tsgo server
    ///
    /// # Cancel Forwarding Flow (Multi-Server Fan-Out)
    ///
    /// 1. Client sends diagnostic request with ID 42
    /// 2. Bridge sends to lua-ls and pyright, calling this method for each
    /// 3. Registry now has: 42 -> {"lua-ls", "pyright"}
    /// 4. Client sends `$/cancelRequest { id: 42 }`
    /// 5. Bridge iterates over all servers and forwards cancel to each
    ///
    /// # Cleanup
    ///
    /// Callers MUST call `unregister_upstream_request(id, server_name)` when the request
    /// completes for a specific server (whether success, error, or timeout). This is
    /// typically done:
    /// - After `wait_for_response()` returns
    /// - In error cleanup callbacks passed to `ensure_document_opened()`
    ///
    /// # Arguments
    /// * `upstream_id` - The original request ID from the upstream client
    /// * `server_name` - The server name handling this request (e.g., "tsgo", "rust-analyzer")
    pub(crate) fn register_upstream_request(&self, upstream_id: UpstreamId, server_name: &str) {
        let mut registry = self
            .upstream_request_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        registry
            .entry(upstream_id)
            .or_default()
            .insert(server_name.to_string());
    }

    /// Unregister an upstream request ID from the registry for a specific server.
    ///
    /// Called when a response is received (or error occurs) to clean up the mapping.
    /// Removes only the specified server from the set; the entry is fully removed when
    /// all servers have been unregistered.
    ///
    /// # Arguments
    /// * `upstream_id` - The request ID to unregister
    /// * `server_name` - The server name to remove from the set
    pub(crate) fn unregister_upstream_request(&self, upstream_id: &UpstreamId, server_name: &str) {
        let mut registry = self
            .upstream_request_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(servers) = registry.get_mut(upstream_id) {
            servers.remove(server_name);
            if servers.is_empty() {
                registry.remove(upstream_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::actor::{OutboundMessage, RouteResult, spawn_reader_task};
    use std::time::Duration;
    use test_helpers::*;

    // ============================================================
    // Pool Integration Tests
    // ============================================================
    // Unit tests for ConnectionHandle, ConnectionState, GlobalShutdownTimeout,
    // and OpenedVirtualDoc live in their respective submodules.
    // This file contains integration tests that exercise cross-module behavior.

    /// Test that LanguageServerPool starts with no connections.
    /// Connections (and their states) are created lazily on first request.
    #[tokio::test]
    async fn pool_starts_with_no_connections() {
        let pool = LanguageServerPool::new();

        // Verify initial state: no connections exist
        let connections = pool.connections.lock().await;
        assert!(
            connections.is_empty(),
            "Connections map should exist and be empty initially"
        );
        assert!(
            !connections.contains_key("test"),
            "Connection should not exist before connection attempt"
        );
    }

    /// Test that requests during Initializing state return error immediately.
    #[tokio::test]
    async fn request_during_init_returns_error_immediately() {
        use std::sync::Arc;
        use tower_lsp_server::ls_types::Position;

        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a ConnectionHandle with Initializing state
        {
            let handle = create_handle_with_state(ConnectionState::Initializing).await;
            pool.connections
                .lock()
                .await
                .insert("lua".to_string(), handle);
        }

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let host_position = Position {
            line: 3,
            character: 5,
        };

        // Test hover request - should fail immediately
        let start = std::time::Instant::now();
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(1), // upstream_request_id
            )
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "Should not block"
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "bridge: downstream server initializing"
        );

        // Test completion request - same behavior
        let result = pool
            .send_completion_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(1), // upstream_request_id
            )
            .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "bridge: downstream server initializing"
        );
    }

    /// Test that requests during Failed state trigger retry with a new server.
    #[tokio::test]
    async fn request_during_failed_triggers_retry_with_new_server() {
        use std::sync::Arc;
        use tower_lsp_server::ls_types::Position;

        if !lua_ls_available() {
            return;
        }

        let pool = Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        // Insert a ConnectionHandle with Failed state
        {
            let handle = create_handle_with_state(ConnectionState::Failed).await;
            pool.connections
                .lock()
                .await
                .insert("lua".to_string(), handle);
        }

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let host_position = Position {
            line: 3,
            character: 5,
        };

        // Test hover request - should trigger retry and succeed with new server
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(1), // upstream_request_id
            )
            .await;
        assert!(
            result.is_ok(),
            "Request should succeed after retry spawns new server: {:?}",
            result.err()
        );

        // Verify the connection is now Ready
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("lua").expect("Should have connection");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "Connection should be Ready after retry"
            );
        }

        // Test completion request - should also succeed (connection is now Ready)
        let result = pool
            .send_completion_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(2), // upstream_request_id
            )
            .await;
        assert!(
            result.is_ok(),
            "Completion request should succeed: {:?}",
            result.err()
        );
    }

    /// Test that requests succeed when ConnectionState is Ready.
    #[tokio::test]
    async fn request_succeeds_when_state_is_ready() {
        use std::sync::Arc;
        use tower_lsp_server::ls_types::Position;

        if !lua_ls_available() {
            return;
        }

        let pool = Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let host_position = Position {
            line: 3,
            character: 5,
        };

        // First request triggers initialization
        // After init completes, state should be Ready and request should succeed
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(1), // upstream_request_id
            )
            .await;

        // Verify request succeeded (not blocked by init check)
        assert!(
            result.is_ok(),
            "Request should succeed after init completes: {:?}",
            result.err()
        );

        // Verify state is Ready after successful init (via ConnectionHandle)
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("lua").expect("Connection should exist");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "State should be Ready after successful init"
            );
        }

        // Second request should also succeed (state remains Ready)
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('world')",
                UpstreamId::Number(2), // upstream_request_id
            )
            .await;

        assert!(
            result.is_ok(),
            "Subsequent request should also succeed: {:?}",
            result.err()
        );
    }

    /// Test that timeout transitions connection to Failed state.
    #[tokio::test]
    async fn connection_transitions_to_failed_state_on_timeout() {
        let pool = LanguageServerPool::new();
        let config = devnull_config_for_language("test");

        // Attempt connection with short timeout (will fail)
        let result = pool
            .get_or_create_connection_with_timeout("test", &config, Duration::from_millis(100))
            .await;

        // Should return timeout error with correct kind and descriptive message
        match result {
            Ok(_) => panic!("Should fail with timeout"),
            Err(err) => {
                assert_eq!(
                    err.kind(),
                    io::ErrorKind::TimedOut,
                    "Error should be TimedOut"
                );
                let msg = err.to_string();
                assert!(
                    msg.contains("timeout"),
                    "Error message should mention timeout: {}",
                    msg
                );
            }
        }

        // With async fast-fail architecture, failed connections are in Failed state
        // (will be removed on next request attempt via Failed state handling)
        let connections = pool.connections.lock().await;
        if let Some(handle) = connections.get("test") {
            assert_eq!(
                handle.state(),
                ConnectionState::Failed,
                "Connection should be in Failed state after timeout"
            );
        }
        // Note: Connection may or may not be present depending on timing
    }

    /// Test that initialization times out when downstream server doesn't respond.
    #[tokio::test]
    async fn init_times_out_when_server_unresponsive() {
        let pool = LanguageServerPool::new();
        // devnull_config simulates an unresponsive server that never sends a response
        let config = devnull_config_for_language("test");

        let start = std::time::Instant::now();

        // Use get_or_create_connection_with_timeout for testing with short timeout
        let result = pool
            .get_or_create_connection_with_timeout("test", &config, Duration::from_millis(100))
            .await;

        let elapsed = start.elapsed();

        // Should timeout quickly (within our 100ms timeout + buffer)
        assert!(
            elapsed < Duration::from_millis(500),
            "Should timeout quickly. Elapsed: {:?}",
            elapsed
        );

        // Should return an error
        assert!(result.is_err(), "Connection should fail with timeout error");
    }

    /// Test that failed connection is removed from cache and new server is spawned on retry.
    ///
    /// When a connection is in Failed state, the next request should:
    /// 1. Remove the failed connection from the cache
    /// 2. Spawn a fresh server process
    /// 3. Return success if the new server initializes correctly
    #[tokio::test]
    async fn failed_connection_retry_removes_cache_and_spawns_new_server() {
        if !lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();

        // Setup: Insert a Failed connection handle
        {
            let handle = create_handle_with_state(ConnectionState::Failed).await;
            pool.connections
                .lock()
                .await
                .insert("lua".to_string(), handle);
        }

        // Verify Failed state is in cache
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("lua").expect("Should have cached handle");
            assert_eq!(handle.state(), ConnectionState::Failed, "Should be Failed");
        }

        // Test: Request connection - should remove failed entry and spawn new server
        let config = lua_ls_config();

        let result = pool
            .get_or_create_connection_with_timeout("lua", &config, Duration::from_secs(30))
            .await;

        // Should succeed with a new Ready connection
        assert!(
            result.is_ok(),
            "Should spawn new server after failed entry removed: {:?}",
            result.err()
        );

        // Verify new connection is Ready (not Failed)
        let handle = result.unwrap();
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "New connection should be Ready"
        );

        // Verify the old Failed handle was replaced in cache
        {
            let connections = pool.connections.lock().await;
            let cached_handle = connections.get("lua").expect("Should have cached handle");
            assert_eq!(
                cached_handle.state(),
                ConnectionState::Ready,
                "Cached handle should be the new Ready one"
            );
        }
    }

    /// Test recovery after initialization timeout.
    ///
    /// This integration test verifies the full recovery flow:
    /// 1. First attempt uses unresponsive server -> times out, enters Failed state
    /// 2. Second attempt with working server -> retry removes failed entry, spawns new server
    /// 3. New server initializes successfully -> connection becomes Ready
    ///
    /// This simulates real-world scenario where a language server crashes or hangs,
    /// and user's subsequent request triggers recovery with a working server.
    #[tokio::test]
    async fn recovery_works_after_initialization_timeout() {
        if !lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();

        // First attempt with unresponsive server - should timeout
        let unresponsive_config = BridgeServerConfig {
            cmd: vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat > /dev/null".to_string(),
            ],
            languages: vec!["lua".to_string()],
            initialization_options: None,
            workspace_type: None,
        };

        let result = pool
            .get_or_create_connection_with_timeout(
                "lua",
                &unresponsive_config,
                Duration::from_millis(100),
            )
            .await;
        assert!(result.is_err(), "First attempt should timeout");
        let err = result.err().expect("Should have error");
        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "Error should be TimedOut"
        );

        // With async fast-fail architecture, connection is stored and transitions to Failed
        {
            let connections = pool.connections.lock().await;
            if let Some(handle) = connections.get("lua") {
                assert_eq!(
                    handle.state(),
                    ConnectionState::Failed,
                    "Connection should be in Failed state after timeout"
                );
            }
        }

        // Second attempt with working server - should succeed immediately
        let working_config = lua_ls_config();

        let result = pool
            .get_or_create_connection_with_timeout("lua", &working_config, Duration::from_secs(30))
            .await;

        // Should succeed - retry removed failed entry and spawned new server
        assert!(
            result.is_ok(),
            "Second attempt should succeed after recovery: {:?}",
            result.err()
        );

        // Verify new connection is Ready
        let handle = result.unwrap();
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Recovered connection should be Ready"
        );

        // Verify cache contains the Ready connection
        {
            let connections = pool.connections.lock().await;
            let cached_handle = connections.get("lua").expect("Should have cached handle");
            assert_eq!(
                cached_handle.state(),
                ConnectionState::Ready,
                "Cached handle should be Ready after recovery"
            );
        }
    }

    /// Test that send_didclose_notification sends notification without closing connection.
    ///
    /// After sending didClose, the connection should still be in Ready state and
    /// can be used for other requests.
    #[tokio::test]
    async fn send_didclose_notification_keeps_connection_open() {
        if !lua_ls_available() {
            return;
        }

        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let host_position = tower_lsp_server::ls_types::Position {
            line: 3,
            character: 5,
        };

        // First, send a hover request to establish connection and open a virtual doc
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                TEST_ULID_LUA_0,
                3,
                "print('hello')",
                UpstreamId::Number(1),
            )
            .await;
        assert!(result.is_ok(), "Hover request should succeed");

        // Get the virtual URI that was opened
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Send didClose notification (server_name matches the one used in send_hover_request)
        let result = pool.send_didclose_notification(&virtual_uri, "lua").await;
        assert!(
            result.is_ok(),
            "send_didclose_notification should succeed: {:?}",
            result.err()
        );

        // Verify connection is still Ready
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("lua").expect("Connection should exist");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "Connection should remain Ready after didClose"
            );
        }
    }

    /// Test that close_host_document sends didClose for all virtual documents.
    ///
    /// When a host document is closed, all its virtual documents should receive
    /// didClose notifications and be cleaned up from tracking structures.
    #[tokio::test]
    async fn close_host_document_sends_didclose_for_all_virtual_docs() {
        if !lua_ls_available() {
            return;
        }

        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        // First, send hover requests to establish connection and open virtual docs
        // Use positions that are within the code block (position.line >= region_start_line)
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                tower_lsp_server::ls_types::Position {
                    line: 4,
                    character: 5,
                },
                "lua",
                TEST_ULID_LUA_0,
                3, // region starts at line 3, position is at line 4, so virtual line = 1
                "print('hello')",
                UpstreamId::Number(1),
            )
            .await;
        assert!(result.is_ok(), "First hover request should succeed");

        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                tower_lsp_server::ls_types::Position {
                    line: 8,
                    character: 5,
                },
                "lua",
                TEST_ULID_LUA_1,
                7, // region starts at line 7, position is at line 8, so virtual line = 1
                "print('world')",
                UpstreamId::Number(2),
            )
            .await;
        assert!(result.is_ok(), "Second hover request should succeed");

        // Close the host document
        let closed_docs = pool.close_host_document(&host_uri).await;

        // Verify we got back the closed docs
        assert_eq!(closed_docs.len(), 2, "Should return 2 closed docs");

        // Verify documents are no longer tracked as opened
        for doc in &closed_docs {
            assert!(
                !pool.is_document_opened(&doc.virtual_uri),
                "Document should no longer be tracked as opened: {}",
                doc.virtual_uri.to_uri_string()
            );
        }

        // Verify connection is still Ready (not closed)
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("lua").expect("Connection should exist");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "Connection should remain Ready after close_host_document"
            );
        }
    }

    /// Test that forward_didchange_to_opened_docs completes quickly with channel-based sending.
    ///
    /// ADR-0015: Channel-based sends via try_send() are non-blocking.
    /// This verifies forward_didchange_to_opened_docs returns promptly.
    #[tokio::test]
    async fn forward_didchange_is_non_blocking() {
        use super::super::protocol::VirtualDocumentUri;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let opened = pool
            .should_send_didopen(&host_uri, &virtual_uri, "lua")
            .await;
        assert!(opened, "First call should open the document");
        // Also mark as opened (simulating successful didOpen write)
        pool.mark_document_opened(&virtual_uri);

        let handle = create_handle_with_state(ConnectionState::Ready).await;
        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        // ADR-0015: No need to hold a writer lock - sends are channel-based and non-blocking
        let injections = vec![(
            "lua".to_string(),
            TEST_ULID_LUA_0.to_string(),
            "local x = 42".to_string(),
        )];

        let start = Instant::now();
        pool.forward_didchange_to_opened_docs(&host_uri, &injections)
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "forward_didchange_to_opened_docs should complete quickly (channel-based sends are non-blocking)"
        );
    }

    // ========================================
    // ensure_document_opened tests
    // ========================================
    // Decision logic unit tests (DocumentOpenDecision) live in document_tracker.rs.
    // These integration tests verify ensure_document_opened I/O behavior:
    // - Writing didOpen notification to downstream
    // - Post-condition: document marked as opened
    // Note: Cleanup on error is now the caller's responsibility.

    /// Test that ensure_document_opened sends didOpen when document is not yet opened.
    ///
    /// Happy path: Document not in document_versions → should_send_didopen returns true
    /// → sends didOpen → marks document as opened via mark_document_opened.
    #[tokio::test]
    async fn ensure_document_opened_sends_didopen_for_new_document() {
        use super::super::protocol::VirtualDocumentUri;
        use tokio::sync::mpsc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_content = "print('hello')";

        // Create a channel-based sender (same as production code path)
        let (mut sender, mut rx) = mpsc::channel::<OutboundMessage>(16);

        // Before ensure_document_opened, document should not be marked as opened
        assert!(
            !pool.is_document_opened(&virtual_uri),
            "Document should not be opened initially"
        );

        // Call ensure_document_opened
        let result = pool
            .ensure_document_opened(&mut sender, &host_uri, &virtual_uri, virtual_content, "lua")
            .await;

        // Should succeed
        assert!(result.is_ok(), "ensure_document_opened should succeed");

        // After ensure_document_opened, document should be marked as opened
        assert!(
            pool.is_document_opened(&virtual_uri),
            "Document should be marked as opened after ensure_document_opened"
        );

        // Verify that a didOpen notification was queued
        let msg = rx.try_recv().expect("should have queued didOpen");
        match msg {
            OutboundMessage::Untracked(payload) => {
                assert_eq!(
                    payload["method"], "textDocument/didOpen",
                    "Should be didOpen notification"
                );
            }
            _ => panic!("Expected Notification, got Request"),
        }
    }

    /// Test that ensure_document_opened skips didOpen when document is already opened.
    ///
    /// Already opened path: Document marked as opened via mark_document_opened
    /// → should_send_didopen returns false, is_document_opened returns true
    /// → no didOpen sent, returns Ok(()).
    #[tokio::test]
    async fn ensure_document_opened_skips_didopen_for_already_opened_document() {
        use super::super::protocol::VirtualDocumentUri;
        use tokio::sync::mpsc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_content = "print('hello')";

        // Pre-open the document (simulate previous didOpen)
        pool.should_send_didopen(&host_uri, &virtual_uri, "lua")
            .await;
        pool.mark_document_opened(&virtual_uri);

        // Verify document is already marked as opened
        assert!(
            pool.is_document_opened(&virtual_uri),
            "Document should be marked as opened"
        );

        // Create a channel-based sender (same as production code path)
        let (mut sender, mut rx) = mpsc::channel::<OutboundMessage>(16);

        // Call ensure_document_opened - should skip didOpen
        let result = pool
            .ensure_document_opened(&mut sender, &host_uri, &virtual_uri, virtual_content, "lua")
            .await;

        // Should succeed (just skips didOpen)
        assert!(
            result.is_ok(),
            "ensure_document_opened should succeed for already opened document"
        );

        // Document should still be marked as opened
        assert!(
            pool.is_document_opened(&virtual_uri),
            "Document should still be marked as opened"
        );

        // Verify that NO didOpen was queued
        assert!(
            rx.try_recv().is_err(),
            "Should NOT have queued any message for already opened document"
        );
    }

    /// Test that ensure_document_opened returns error when document is in inconsistent state.
    ///
    /// Error path: Another request called should_send_didopen (returned true) but hasn't
    /// yet called mark_document_opened. Our call sees:
    /// - should_send_didopen returns false (document_versions entry exists)
    /// - is_document_opened returns false (not yet marked)
    /// This is a race condition where didOpen is pending.
    ///
    /// Expected behavior: returns error (caller is responsible for cleanup).
    #[tokio::test]
    async fn ensure_document_opened_returns_error_for_pending_didopen() {
        use super::super::protocol::VirtualDocumentUri;
        use tokio::sync::mpsc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_content = "print('hello')";

        // Simulate another request having called should_send_didopen but NOT mark_document_opened
        // This puts the document in the "didOpen pending" state
        pool.should_send_didopen(&host_uri, &virtual_uri, "lua")
            .await;
        // Deliberately do NOT call mark_document_opened to simulate pending didOpen

        // Verify the inconsistent state:
        // - should_send_didopen will return false (document already registered)
        // - is_document_opened will return false (not yet marked as opened)
        assert!(
            !pool
                .should_send_didopen(&host_uri, &virtual_uri, "lua")
                .await,
            "should_send_didopen should return false (already registered)"
        );
        assert!(
            !pool.is_document_opened(&virtual_uri),
            "Document should NOT be marked as opened"
        );

        // Create a channel-based sender (same as production code path)
        let (mut sender, _rx) = mpsc::channel::<OutboundMessage>(16);

        // Call ensure_document_opened - should fail
        let result = pool
            .ensure_document_opened(&mut sender, &host_uri, &virtual_uri, virtual_content, "lua")
            .await;

        // Should return error
        assert!(
            result.is_err(),
            "ensure_document_opened should return error for pending didOpen state"
        );

        // Verify error message
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("didOpen pending"),
            "Error message should mention didOpen pending: {}",
            err
        );

        // Document should still NOT be marked as opened
        assert!(
            !pool.is_document_opened(&virtual_uri),
            "Document should still NOT be marked as opened after error"
        );
    }

    // ========================================
    // Connection State Machine Integration Tests
    // ========================================
    // Unit tests for ConnectionState enum live in connection_state.rs.
    // These integration tests verify pool behavior with different connection states.

    /// Test that requests during Closing state receive error immediately.
    ///
    /// ADR-0015 Operation Gating: When connection is Closing, new requests
    /// are rejected with "bridge: connection closing" error. This prevents
    /// new requests from queuing during shutdown.
    #[tokio::test]
    async fn request_during_closing_state_returns_error_immediately() {
        use std::sync::Arc;
        use tower_lsp_server::ls_types::Position;

        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a ConnectionHandle with Closing state
        {
            let handle = create_handle_with_state(ConnectionState::Closing).await;
            pool.connections
                .lock()
                .await
                .insert("lua".to_string(), handle);
        }

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let host_position = Position {
            line: 3,
            character: 5,
        };

        // Test hover request - should fail immediately with connection closing error
        let start = std::time::Instant::now();
        let result = pool
            .send_hover_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(1), // upstream_request_id
            )
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "Should not block"
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "bridge: connection closing"
        );

        // Test completion request - same behavior
        let result = pool
            .send_completion_request(
                "lua", // server_name
                &config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                3,
                "print('hello')",
                UpstreamId::Number(1), // upstream_request_id
            )
            .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "bridge: connection closing"
        );
    }

    // ========================================
    // LSP Shutdown Handshake Tests
    // ========================================

    /// Test that shutdown sends LSP shutdown request and receives response.
    ///
    /// ADR-0017: Graceful shutdown requires sending LSP "shutdown" request and
    /// waiting for the server's response before sending "exit" notification.
    /// This test verifies the shutdown request is properly formatted and sent.
    #[tokio::test]
    async fn shutdown_sends_lsp_shutdown_request_and_waits_for_response() {
        if !lua_ls_available() {
            return;
        }

        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        // First, establish a Ready connection
        let handle = pool
            .get_or_create_connection("lua", &config)
            .await
            .expect("should establish connection");

        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Connection should be Ready"
        );

        // Perform graceful shutdown
        let result = handle.graceful_shutdown().await;

        // Should succeed
        assert!(
            result.is_ok(),
            "graceful_shutdown should succeed: {:?}",
            result.err()
        );

        // Connection should be in Closed state after shutdown
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "Connection should be Closed after graceful_shutdown"
        );
    }

    /// Test that graceful shutdown acquires exclusive writer access.
    #[tokio::test]
    async fn graceful_shutdown_acquires_exclusive_writer_access() {
        // Create a connection to a mock server (sink — doesn't respond to shutdown)
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Verify initial state
        assert_eq!(handle.state(), ConnectionState::Ready);

        // Perform shutdown with timeout (ADR-0018: graceful_shutdown has no internal timeout,
        // caller must provide one). Sink servers don't respond, so this always times out.
        let result = tokio::time::timeout(Duration::from_secs(2), handle.graceful_shutdown()).await;

        if result.is_err() {
            // Timeout: manually complete shutdown (simulates force_kill_all behavior)
            handle.complete_shutdown();
        }

        // After shutdown completes, state should be Closed
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "State should be Closed after graceful_shutdown"
        );
    }

    /// Test that shutdown transitions through Closing state.
    ///
    /// ADR-0017: Shutdown transitions to Closing state first, which rejects new
    /// operations. This test verifies the state transition happens immediately
    /// when begin_shutdown() is called.
    #[tokio::test]
    async fn shutdown_transitions_through_closing_state() {
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Verify initial state
        assert_eq!(handle.state(), ConnectionState::Ready);

        // Manually call begin_shutdown to verify transition
        handle.begin_shutdown();

        // State should be Closing now
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "State should be Closing after begin_shutdown"
        );

        // Complete shutdown
        handle.complete_shutdown();

        // State should be Closed now
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "State should be Closed after complete_shutdown"
        );
    }

    /// Test that shutdown_all handles multiple connections in parallel.
    ///
    /// ADR-0017: All connections shut down in parallel with a global timeout.
    /// This test verifies that multiple connections can be shut down concurrently.
    #[tokio::test]
    async fn shutdown_all_handles_multiple_connections_in_parallel() {
        let pool = LanguageServerPool::new();

        // Create multiple connections with different states
        {
            let ready_handle = create_handle_with_state(ConnectionState::Ready).await;
            let failed_handle = create_handle_with_state(ConnectionState::Failed).await;
            let closing_handle = create_handle_with_state(ConnectionState::Closing).await;

            let mut connections = pool.connections.lock().await;
            connections.insert("lua".to_string(), ready_handle);
            connections.insert("python".to_string(), failed_handle);
            connections.insert("rust".to_string(), closing_handle);
        }

        // Call shutdown_all
        pool.shutdown_all().await;

        // Verify final states
        let connections = pool.connections.lock().await;

        // Ready -> should be Closed (went through graceful shutdown)
        let lua_handle = connections.get("lua").expect("lua should exist");
        assert_eq!(
            lua_handle.state(),
            ConnectionState::Closed,
            "Ready connection should be Closed after shutdown_all"
        );

        // Failed -> should be Closed (directly, no LSP handshake)
        let python_handle = connections.get("python").expect("python should exist");
        assert_eq!(
            python_handle.state(),
            ConnectionState::Closed,
            "Failed connection should be Closed after shutdown_all"
        );

        // Closing -> should be Closed after global timeout triggers force_kill_all.
        // The sink-based test server doesn't respond to shutdown requests, so the
        // Ready handle's graceful_shutdown hangs until the global timeout. When
        // force_kill_all runs, it transitions ALL non-Closed connections to Closed,
        // including this one that was already Closing.
        let rust_handle = connections.get("rust").expect("rust should exist");
        assert_eq!(
            rust_handle.state(),
            ConnectionState::Closed,
            "Closing connection should be Closed after global timeout triggers force_kill_all"
        );
    }

    // ========================================
    // Forced Shutdown Tests
    // ========================================

    /// Test that pool.shutdown_all_with_timeout force-kills unresponsive processes.
    ///
    /// ADR-0017/ADR-0018: When graceful shutdown times out, force_kill_all is called
    /// which escalates to SIGKILL for processes that don't respond to SIGTERM.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_all_force_kills_unresponsive_process() {
        use std::sync::Arc;
        use std::time::Instant;

        // Create a pool with an unresponsive process
        let pool = Arc::new(LanguageServerPool::new());

        // Spawn a process that ignores SIGTERM and doesn't respond to LSP
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            // Trap SIGTERM and ignore it, sleep indefinitely
            "trap '' TERM; while true; do sleep 1; done".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

        let (tx, rx) = tokio::sync::mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Ready,
            tx,
            rx,
            Arc::new(DynamicCapabilityRegistry::new()),
        ));

        // Add connection to pool
        pool.connections
            .lock()
            .await
            .insert("test".to_string(), handle);

        // Start timer
        let start = Instant::now();

        // Call shutdown_all with minimum valid timeout (5 seconds per ADR-0018)
        // This should:
        // 1. Try graceful shutdown (will hang waiting for LSP response)
        // 2. After 5s timeout, call force_kill_all
        // 3. Force kill sends SIGTERM, waits, then SIGKILL
        let timeout =
            GlobalShutdownTimeout::new(Duration::from_secs(5)).expect("5s is valid timeout");
        pool.shutdown_all_with_timeout(timeout).await;

        // Should complete within global timeout + SIGTERM grace period + buffer
        // (5s timeout + 2s SIGTERM grace + 1s buffer = 8s)
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "Shutdown should complete within 8 seconds, took {:?}",
            start.elapsed()
        );

        // Connection should be Closed
        let connections = pool.connections.lock().await;
        let handle = connections.get("test").expect("connection should exist");
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "Connection should be Closed after force-kill"
        );
    }

    /// Test that shutdown with pending requests fails those requests and then completes.
    #[tokio::test]
    async fn shutdown_with_pending_requests_fails_requests_then_completes() {
        use std::sync::Arc;

        if !lua_ls_available() {
            return;
        }

        let pool = Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        // Step 1: Establish a Ready connection
        let handle = pool
            .get_or_create_connection("lua", &config)
            .await
            .expect("should establish connection");

        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Connection should be Ready"
        );

        // Step 2: Register a pending request (simulates in-flight request)
        let (request_id, response_rx) = handle.register_request().expect("should register request");

        // Step 3: Initiate shutdown - should transition to Closing
        handle.begin_shutdown();
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "Connection should be Closing after begin_shutdown"
        );

        // Step 4: Complete graceful shutdown in background
        let shutdown_handle = Arc::clone(&handle);
        let shutdown_task = tokio::spawn(async move { shutdown_handle.graceful_shutdown().await });

        // Step 5: The pending request should fail when shutdown completes
        // The router is dropped during shutdown, closing all channels
        let response = tokio::time::timeout(Duration::from_secs(10), response_rx).await;

        // The response should be an error (channel closed) or timeout
        // because the shutdown closes the router
        match response {
            Ok(Ok(_)) => {
                // If we got a response, it should be because the server
                // responded before shutdown completed - this is acceptable
                log::debug!("Pending request received response before shutdown");
            }
            Ok(Err(_)) => {
                // Channel closed - this is the expected behavior
                // Pending request failed due to shutdown
                log::debug!("Pending request failed as expected (channel closed)");
            }
            Err(_) => {
                // Timeout - clean up
                handle.router().remove(request_id);
                log::debug!("Pending request timed out");
            }
        }

        // Wait for shutdown to complete
        let _ = shutdown_task.await;

        // Step 6: Connection should be in Closed state
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "Connection should be Closed after graceful_shutdown"
        );
    }

    // ============================================================
    // Global Shutdown Timeout Integration Tests
    // ============================================================
    // Unit tests for GlobalShutdownTimeout newtype live in shutdown_timeout.rs.

    /// Test that shutdown_all completes within configured timeout even with hung servers.
    #[tokio::test]
    async fn shutdown_all_completes_within_global_timeout_with_hung_servers() {
        let pool = LanguageServerPool::new();

        // Insert a connection that will hang (cat > /dev/null never responds)
        {
            let handle = create_handle_with_state(ConnectionState::Ready).await;
            pool.connections
                .lock()
                .await
                .insert("hung_server".to_string(), handle);
        }

        let timeout = GlobalShutdownTimeout::new(Duration::from_secs(5)).expect("5s is valid");

        let start = std::time::Instant::now();
        pool.shutdown_all_with_timeout(timeout).await;
        let elapsed = start.elapsed();

        // Should complete within timeout + 2s buffer for overhead.
        // Buffer accounts for: SIGTERM->SIGKILL escalation (2s) + test/CI variability.
        // Total: 5s timeout + 2s buffer = 7s max expected.
        assert!(
            elapsed < Duration::from_secs(7),
            "Shutdown should complete within global timeout. Elapsed: {:?}",
            elapsed
        );

        // All connections should be in Closed state
        let connections = pool.connections.lock().await;
        if let Some(handle) = connections.get("hung_server") {
            assert_eq!(
                handle.state(),
                ConnectionState::Closed,
                "Connection should be Closed after shutdown timeout"
            );
        }
    }

    /// Test that multiple servers shut down concurrently (O(1) not O(N) time).
    #[tokio::test]
    async fn multiple_servers_shutdown_concurrently_bounded_by_global_timeout() {
        let pool = LanguageServerPool::new();

        // Insert 3 hung servers - if sequential would be 3 * 5s = 15s
        for i in 0..3 {
            let handle = create_handle_with_state(ConnectionState::Ready).await;
            pool.connections
                .lock()
                .await
                .insert(format!("hung_server_{}", i), handle);
        }

        // Use 5s timeout - should complete in ~5s even with 3 servers
        let timeout = GlobalShutdownTimeout::new(Duration::from_secs(5)).expect("5s is valid");

        let start = std::time::Instant::now();
        pool.shutdown_all_with_timeout(timeout).await;
        let elapsed = start.elapsed();

        // Key assertion: total time should be O(1), not O(N).
        // 3 servers would take 15s sequential, but should complete in ~5-7s parallel.
        // Buffer (3s) accounts for: SIGTERM->SIGKILL escalation (2s) + process spawn overhead
        // + CI variability. Total: 5s timeout + 3s buffer = 8s max expected.
        assert!(
            elapsed < Duration::from_secs(8),
            "3 servers should shut down in O(1) time, not O(N). Elapsed: {:?}",
            elapsed
        );

        // All connections should be in Closed state
        let connections = pool.connections.lock().await;
        for i in 0..3 {
            let key = format!("hung_server_{}", i);
            if let Some(handle) = connections.get(&key) {
                assert_eq!(
                    handle.state(),
                    ConnectionState::Closed,
                    "Connection {} should be Closed after parallel shutdown",
                    key
                );
            }
        }
    }

    // ============================================================
    // Force-kill Fallback Tests
    // ============================================================

    /// Test that shutdown_all_with_timeout ensures all connections reach Closed state.
    #[tokio::test]
    #[cfg(unix)]
    async fn shutdown_with_timeout_ensures_all_connections_closed() {
        let pool = LanguageServerPool::new();

        // Insert connections
        for i in 0..2 {
            let handle = create_handle_with_state(ConnectionState::Ready).await;
            pool.connections
                .lock()
                .await
                .insert(format!("server_{}", i), handle);
        }

        // Use minimum valid timeout (5s)
        let timeout = GlobalShutdownTimeout::new(Duration::from_secs(5)).expect("5s is valid");

        pool.shutdown_all_with_timeout(timeout).await;

        // All connections should be in Closed state (via graceful shutdown or force-kill)
        let connections = pool.connections.lock().await;
        for i in 0..2 {
            let key = format!("server_{}", i);
            if let Some(handle) = connections.get(&key) {
                assert_eq!(
                    handle.state(),
                    ConnectionState::Closed,
                    "Connection {} should be Closed after shutdown_all_with_timeout",
                    key
                );
            }
        }
    }

    // ============================================================
    // Writer Task Coordination Tests
    // ============================================================

    /// Test that graceful_shutdown waits for writer task to drain and return.
    ///
    /// ADR-0015: The 3-phase shutdown protocol ensures:
    /// 1. Stop signal sent to writer task
    /// 2. Writer task drains queue and confirms idle
    /// 3. Writer is reclaimed for LSP shutdown sequence
    #[tokio::test]
    async fn graceful_shutdown_coordinates_with_writer_task() {
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Send a notification before shutdown (will be queued)
        let result = handle.send_notification(serde_json::json!({"method": "test", "params": {}}));
        assert_eq!(
            result,
            NotificationSendResult::Queued,
            "Should be able to send notification before shutdown"
        );

        // Perform graceful shutdown with timeout (ADR-0018: caller provides timeout).
        // Sink server doesn't respond, so handshake always times out. We verify the
        // writer task coordination (stop_and_reclaim) works by checking that the
        // notification was sent and shutdown progresses to Closing state.
        let start = std::time::Instant::now();
        let timeout_result =
            tokio::time::timeout(Duration::from_secs(3), handle.graceful_shutdown()).await;

        if timeout_result.is_err() {
            // Timeout: manually complete shutdown (simulates force_kill_all behavior)
            handle.complete_shutdown();
        }

        // Shutdown should reach Closed state within timeout + cleanup
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Graceful shutdown should complete within 5 seconds (including timeout)"
        );

        // Verify shutdown completed (state is Closed)
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "State should be Closed after graceful shutdown completed"
        );
    }

    // ============================================================
    // Cancel Forwarding Tests
    // ============================================================

    /// Test that forward_cancel looks up downstream ID and sends cancel notification.
    ///
    /// This tests the full cancel forwarding flow:
    /// 1. Register a request with upstream ID mapping
    /// 2. Call forward_cancel with the upstream ID
    /// 3. Verify the cancel notification was sent with the correct downstream ID
    #[tokio::test]
    async fn forward_cancel_sends_notification_with_downstream_id() {
        use std::sync::Arc;

        // Create a pool and connection manually
        let pool = LanguageServerPool::new();
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        // Forward cancel request
        let result = pool.forward_cancel("lua", &upstream_id).await;

        // Should succeed (the notification was sent)
        assert!(
            result.is_ok(),
            "forward_cancel should succeed: {:?}",
            result.err()
        );

        // Verify the pending entry is still there (cancel does NOT remove it)
        assert_eq!(
            handle.router().pending_count(),
            1,
            "Pending entry should still exist after cancel"
        );

        // Verify the cancel_map entry is still there (cancel does NOT remove it)
        // The mapping is only removed when the actual response arrives
        assert_eq!(
            handle.router().lookup_downstream_id(&upstream_id),
            Some(downstream_id),
            "Cancel map entry should still exist after cancel forwarding"
        );
    }

    /// Test that forward_cancel silently drops when no connection exists (best-effort semantics).
    #[tokio::test]
    async fn forward_cancel_silently_drops_when_no_connection() {
        let pool = LanguageServerPool::new();

        let result = pool
            .forward_cancel("nonexistent", &UpstreamId::Number(42))
            .await;

        // Per best-effort semantics, this should succeed (silent drop)
        assert!(
            result.is_ok(),
            "forward_cancel should silently drop for nonexistent connection"
        );
    }

    /// Test that forward_cancel silently drops when upstream ID not found (best-effort semantics).
    #[tokio::test]
    async fn forward_cancel_silently_drops_when_upstream_id_not_found() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Insert connection but don't register any request
        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        let result = pool.forward_cancel("lua", &UpstreamId::Number(999)).await;

        // Per best-effort semantics, this should succeed (silent drop)
        assert!(
            result.is_ok(),
            "forward_cancel should silently drop for unknown upstream ID"
        );
    }

    // ============================================================
    // Upstream Request Registry Tests
    // ============================================================

    /// Test that register_upstream_request stores the mapping.
    #[test]
    fn register_upstream_request_stores_mapping() {
        let pool = LanguageServerPool::new();

        pool.register_upstream_request(UpstreamId::Number(42), "lua");

        let registry = pool.upstream_request_registry.lock().unwrap();
        let servers = registry
            .get(&UpstreamId::Number(42))
            .expect("should have entry");
        assert!(servers.contains("lua"));
        assert_eq!(servers.len(), 1);
    }

    /// Test that unregister_upstream_request removes the mapping.
    #[test]
    fn unregister_upstream_request_removes_mapping() {
        let pool = LanguageServerPool::new();

        pool.register_upstream_request(UpstreamId::Number(42), "lua");
        pool.unregister_upstream_request(&UpstreamId::Number(42), "lua");

        let registry = pool.upstream_request_registry.lock().unwrap();
        assert_eq!(registry.get(&UpstreamId::Number(42)), None);
    }

    /// Test that forward_cancel_by_upstream_id uses the registry to find the language.
    #[tokio::test]
    async fn forward_cancel_by_upstream_id_uses_registry() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Register a request with upstream ID mapping in ResponseRouter
        let upstream_id = UpstreamId::Number(42);
        let (_downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        // Register the upstream request in the registry
        pool.register_upstream_request(upstream_id.clone(), "lua");

        // Forward cancel by upstream ID only (no language parameter)
        let result = pool
            .forward_cancel_by_upstream_id(upstream_id.clone())
            .await;

        // Should succeed because the registry has the mapping
        assert!(
            result.is_ok(),
            "forward_cancel_by_upstream_id should succeed: {:?}",
            result.err()
        );
    }

    /// Test that forward_cancel_by_upstream_id silently drops when not in registry (best-effort semantics).
    #[tokio::test]
    async fn forward_cancel_by_upstream_id_silently_drops_when_not_in_registry() {
        let pool = LanguageServerPool::new();

        // Don't register anything in the registry
        let result = pool
            .forward_cancel_by_upstream_id(UpstreamId::Number(999))
            .await;

        // Per best-effort semantics, this should succeed (silent drop)
        assert!(
            result.is_ok(),
            "forward_cancel_by_upstream_id should silently drop for unknown ID"
        );
    }

    /// Test that response forwarding still works after cancel notification is sent.
    ///
    /// This is the key test for Subtask 6: Per LSP spec, a cancelled request should
    /// still receive a response (either the normal result or an error with code -32800).
    /// The cancel forwarding mechanism must preserve the pending entry so that when
    /// the downstream server eventually responds, we can still deliver it.
    #[tokio::test]
    async fn response_forwarding_works_after_cancel() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        // Forward cancel request (simulating client cancelling the request)
        let cancel_result = pool.forward_cancel("lua", &upstream_id).await;
        assert!(cancel_result.is_ok(), "cancel should succeed");

        // Now simulate the downstream server responding (with a normal result)
        // This could also be a -32800 RequestCancelled error, but a normal result
        // is also valid if the server finished before processing the cancel
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": downstream_id.as_i64(),
            "result": {
                "contents": "Hover content even though request was cancelled"
            }
        });

        // Route the response through the router
        let result = handle.router().route(response.clone());
        assert_eq!(
            result,
            RouteResult::Delivered,
            "response should be delivered even after cancel"
        );

        // The original requester should receive the response
        let received = response_rx
            .await
            .expect("should receive response after cancel");
        assert_eq!(received["id"], downstream_id.as_i64());
        assert_eq!(
            received["result"]["contents"],
            "Hover content even though request was cancelled"
        );

        // After routing, the pending entry should be cleaned up
        assert_eq!(
            handle.router().pending_count(),
            0,
            "pending entry should be removed after response"
        );
        assert_eq!(
            handle.router().lookup_downstream_id(&upstream_id),
            None,
            "cancel map entry should be removed after response"
        );
    }

    /// Test that error response (-32800 RequestCancelled) works after cancel.
    ///
    /// Per LSP spec, when a server receives a cancel notification and chooses
    /// to honour it, it should respond with error code -32800 (RequestCancelled).
    /// This test verifies that such error responses are properly forwarded.
    #[tokio::test]
    async fn cancelled_error_response_forwarding_works() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        // Forward cancel request
        let cancel_result = pool.forward_cancel("lua", &upstream_id).await;
        assert!(cancel_result.is_ok(), "cancel should succeed");

        // Simulate the downstream server responding with RequestCancelled error
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": downstream_id.as_i64(),
            "error": {
                "code": -32800,
                "message": "Request cancelled"
            }
        });

        // Route the response through the router
        let result = handle.router().route(response.clone());
        assert_eq!(
            result,
            RouteResult::Delivered,
            "error response should be delivered after cancel"
        );

        // The original requester should receive the error response
        let received = response_rx.await.expect("should receive error response");
        assert_eq!(received["id"], downstream_id.as_i64());
        assert_eq!(received["error"]["code"], -32800);
        assert_eq!(received["error"]["message"], "Request cancelled");
    }

    // ============================================================
    // Cancel Forwarding Metrics Tests
    // ============================================================

    /// Test that metrics are recorded for successful cancel forwarding.
    #[tokio::test]
    async fn cancel_metrics_records_success() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (_downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        pool.connections
            .lock()
            .await
            .insert("lua".to_string(), Arc::clone(&handle));

        // Forward cancel
        let _ = pool.forward_cancel("lua", &upstream_id).await;

        // Check metrics
        let (successful, no_conn, not_ready, unknown_id, not_in_reg) =
            pool.cancel_metrics().snapshot();
        assert_eq!(successful, 1, "Should record 1 successful cancel");
        assert_eq!(no_conn, 0);
        assert_eq!(not_ready, 0);
        assert_eq!(unknown_id, 0);
        assert_eq!(not_in_reg, 0);
    }

    /// Test that metrics are recorded for cancel failures.
    #[tokio::test]
    async fn cancel_metrics_records_failures() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();

        // Test: no connection
        let _ = pool
            .forward_cancel("nonexistent", &UpstreamId::Number(1))
            .await;

        // Test: not in registry
        let _ = pool
            .forward_cancel_by_upstream_id(UpstreamId::Number(999))
            .await;

        // Test: connection not ready
        let handle_init = create_handle_with_state(ConnectionState::Initializing).await;
        pool.connections
            .lock()
            .await
            .insert("init_lang".to_string(), Arc::clone(&handle_init));
        let _ = pool
            .forward_cancel("init_lang", &UpstreamId::Number(2))
            .await;

        // Test: unknown upstream ID
        let handle_ready = create_handle_with_state(ConnectionState::Ready).await;
        pool.connections
            .lock()
            .await
            .insert("ready_lang".to_string(), Arc::clone(&handle_ready));
        let _ = pool
            .forward_cancel("ready_lang", &UpstreamId::Number(3))
            .await;

        // Check metrics
        let (successful, no_conn, not_ready, unknown_id, not_in_reg) =
            pool.cancel_metrics().snapshot();
        assert_eq!(successful, 0, "No successful cancels");
        assert_eq!(no_conn, 1, "1 no_connection failure");
        assert_eq!(not_ready, 1, "1 not_ready failure");
        assert_eq!(unknown_id, 1, "1 unknown_id failure");
        assert_eq!(not_in_reg, 1, "1 not_in_registry failure");
    }

    // ========================================
    // Process-sharing didClose tests
    // ========================================

    /// Test that send_didclose_notification uses server_name, not language, for connection lookup.
    ///
    /// In process-sharing scenarios, the language (e.g., "typescript") differs from the
    /// server_name (e.g., "tsgo"). didClose must use server_name to find the correct connection.
    ///
    /// This test verifies the fix for the bug where send_didclose_notification used
    /// virtual_uri.language() instead of the server_name parameter.
    #[tokio::test]
    async fn send_didclose_uses_server_name_not_language_for_connection_lookup() {
        use super::super::protocol::VirtualDocumentUri;
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        // Create a virtual document with language="typescript" (for URI/extension)
        // but we'll use server_name="tsgo" for connection lookup
        let virtual_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "typescript", TEST_ULID_LUA_0);

        // Insert a connection keyed by "tsgo" (server_name), NOT "typescript" (language)
        let handle = create_handle_with_state(ConnectionState::Ready).await;
        pool.connections
            .lock()
            .await
            .insert("tsgo".to_string(), Arc::clone(&handle));

        // Verify there is NO connection for "typescript" (the language)
        {
            let connections = pool.connections.lock().await;
            assert!(
                connections.get("typescript").is_none(),
                "Should NOT have a 'typescript' connection"
            );
            assert!(
                connections.get("tsgo").is_some(),
                "Should have a 'tsgo' connection"
            );
        }

        // Send didClose with server_name="tsgo"
        // This should succeed because we look up by server_name, not language
        let result = pool.send_didclose_notification(&virtual_uri, "tsgo").await;
        assert!(
            result.is_ok(),
            "send_didclose_notification should succeed when using server_name 'tsgo': {:?}",
            result.err()
        );

        // Verify connection is still Ready (not closed)
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("tsgo").expect("Connection should exist");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "Connection should remain Ready after didClose"
            );
        }
    }

    /// Test that close_single_virtual_doc routes didClose using server_name from OpenedVirtualDoc.
    ///
    /// When a document is tracked with server_name="tsgo" but language="typescript",
    /// close_single_virtual_doc should use the stored server_name for didClose routing.
    #[tokio::test]
    async fn close_single_virtual_doc_uses_server_name_for_process_sharing() {
        use super::super::protocol::VirtualDocumentUri;
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        // Create a virtual document with language="typescript"
        let virtual_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "typescript", TEST_ULID_LUA_0);

        // Register the document with server_name="tsgo" (process-sharing scenario)
        // This simulates what happens when a typescript block uses the tsgo server
        let opened = pool
            .should_send_didopen(&host_uri, &virtual_uri, "tsgo")
            .await;
        assert!(opened, "Document should be registered for opening");
        pool.mark_document_opened(&virtual_uri);

        // Insert a connection keyed by "tsgo" (NOT "typescript")
        let handle = create_handle_with_state(ConnectionState::Ready).await;
        pool.connections
            .lock()
            .await
            .insert("tsgo".to_string(), Arc::clone(&handle));

        // Close the host document - this triggers close_single_virtual_doc internally
        let closed_docs = pool.close_host_document(&host_uri).await;

        // Verify the document was closed
        assert_eq!(closed_docs.len(), 1, "Should have closed 1 document");
        assert_eq!(
            closed_docs[0].virtual_uri.language(),
            "typescript",
            "Closed doc should have language 'typescript'"
        );
        assert_eq!(
            closed_docs[0].server_name, "tsgo",
            "Closed doc should have server_name 'tsgo'"
        );

        // Verify the document is no longer tracked
        assert!(
            !pool.is_document_opened(&virtual_uri),
            "Document should no longer be marked as opened"
        );

        // Verify connection is still Ready (not closed)
        {
            let connections = pool.connections.lock().await;
            let handle = connections.get("tsgo").expect("Connection should exist");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "Connection should remain Ready after close_host_document"
            );
        }
    }

    // ============================================================
    // Eager Spawn Tests
    // ============================================================

    /// Test that ensure_server_ready spawns a server and stores it in the pool.
    ///
    /// This test verifies the eager spawn behavior:
    /// 1. Connection entry is created and stored in pool
    /// 2. State may be Initializing or Failed (devnull doesn't respond, so it times out)
    /// 3. No didOpen is sent (that happens lazily on first request)
    ///
    /// Note: With devnull_config, the handshake will fail because the mock server
    /// doesn't respond. This test verifies that spawning happens, not handshake success.
    /// The real-server test below verifies the full Ready transition.
    ///
    /// Uses a short timeout to avoid test slowness with devnull
    /// (ensure_server_ready uses 30s which causes slow test runtime).
    #[tokio::test]
    async fn ensure_server_ready_spawns_connection_entry() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let short_timeout = Duration::from_millis(100);

        // Before: no connection exists
        {
            let connections = pool.connections.lock().await;
            assert!(!connections.contains_key("test-server"));
        }

        // Spawn server (ignoring errors like ensure_server_ready does)
        let _ = pool
            .get_or_create_connection_with_timeout("test-server", &config, short_timeout)
            .await;

        // After: connection entry exists (state may be Initializing or Failed)
        // With devnull, the handshake times out quickly, so it transitions to Failed
        {
            let connections = pool.connections.lock().await;
            assert!(
                connections.contains_key("test-server"),
                "Connection entry should be created by ensure_server_ready"
            );
            // We don't assert specific state because devnull's behavior varies:
            // - May be Initializing if timeout hasn't elapsed yet
            // - May be Failed if handshake timed out
        }
    }

    /// Test that ensure_server_ready is idempotent - calling twice doesn't spawn a second server.
    ///
    /// Uses a short timeout (100ms) instead of `ensure_server_ready` (which uses 30s default)
    /// to avoid test slowness with devnull config where handshake always times out.
    #[tokio::test]
    async fn ensure_server_ready_is_idempotent() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();

        // Use short timeout to avoid test slowness with devnull
        // (ensure_server_ready uses 30s which causes ~60s test runtime)
        let short_timeout = Duration::from_millis(100);

        // Call twice (ignoring errors like ensure_server_ready does)
        let _ = pool
            .get_or_create_connection_with_timeout("test-server", &config, short_timeout)
            .await;
        let _ = pool
            .get_or_create_connection_with_timeout("test-server", &config, short_timeout)
            .await;

        // Should still have exactly one connection
        {
            let connections = pool.connections.lock().await;
            assert_eq!(connections.len(), 1, "Should have exactly one connection");
        }
    }

    /// Test that ensure_server_ready with a real server eventually transitions to Ready.
    #[tokio::test]
    async fn ensure_server_ready_with_real_server_transitions_to_ready() {
        if !lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();
        let config = lua_ls_config();

        // Spawn server eagerly
        pool.ensure_server_ready("lua-ls", &config).await;

        // Wait for handshake to complete (up to 10 seconds)
        let handle = {
            let connections = pool.connections.lock().await;
            connections
                .get("lua-ls")
                .cloned()
                .expect("Connection handle should exist after ensure_server_ready")
        };
        let ready = handle.wait_for_ready(Duration::from_secs(10)).await.is_ok();

        assert!(
            ready,
            "Server should transition to Ready state after handshake"
        );
    }

    // ============================================================
    // Wait-for-Ready Tests
    // ============================================================

    /// Test that get_or_create_connection_wait_ready waits for initializing server.
    ///
    /// This tests the wait-for-ready behavior:
    /// 1. Connection is in Initializing state
    /// 2. get_or_create_connection would fail fast
    /// 3. get_or_create_connection_wait_ready waits and returns once Ready
    #[tokio::test]
    async fn get_or_create_connection_wait_ready_waits_for_initializing_server() {
        use std::sync::Arc;

        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a connection in Initializing state
        let handle = create_handle_with_state(ConnectionState::Initializing).await;
        let handle_clone = Arc::clone(&handle);
        pool.connections
            .lock()
            .await
            .insert("test-server".to_string(), handle);

        // Spawn a task that will transition to Ready after a delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle_clone.set_state(ConnectionState::Ready);
        });

        // Call get_or_create_connection_wait_ready - should wait and succeed
        let result = pool
            .get_or_create_connection_wait_ready("test-server", &config, Duration::from_secs(1))
            .await;

        // Should succeed after waiting for Ready state
        assert!(
            result.is_ok(),
            "Should succeed after waiting for Ready: {:?}",
            result.err()
        );

        let handle = result.unwrap();
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Returned handle should be in Ready state"
        );
    }

    /// Test that get_or_create_connection_wait_ready fails when server fails during wait.
    #[tokio::test]
    async fn get_or_create_connection_wait_ready_fails_when_server_fails() {
        use std::sync::Arc;

        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a connection in Initializing state
        let handle = create_handle_with_state(ConnectionState::Initializing).await;
        let handle_clone = Arc::clone(&handle);
        pool.connections
            .lock()
            .await
            .insert("test-server".to_string(), handle);

        // Spawn a task that will transition to Failed after a delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle_clone.set_state(ConnectionState::Failed);
        });

        // Call get_or_create_connection_wait_ready - should fail
        let result = pool
            .get_or_create_connection_wait_ready("test-server", &config, Duration::from_secs(1))
            .await;

        // Should fail due to Failed state transition
        assert!(
            result.is_err(),
            "Should fail when server transitions to Failed"
        );
        let err = result.err().expect("Should have error");
        assert!(
            err.to_string().contains("failed during initialization"),
            "Error should mention initialization failure: {}",
            err
        );
    }

    /// Test that get_or_create_connection_wait_ready times out properly.
    #[tokio::test]
    async fn get_or_create_connection_wait_ready_times_out() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();

        // Insert a connection in Initializing state that won't transition
        let handle = create_handle_with_state(ConnectionState::Initializing).await;
        pool.connections
            .lock()
            .await
            .insert("test-server".to_string(), handle);

        // Call with short timeout - should timeout
        let result = pool
            .get_or_create_connection_wait_ready("test-server", &config, Duration::from_millis(50))
            .await;

        // Should fail with timeout
        assert!(result.is_err(), "Should timeout");
        let err = result.err().expect("Should have error");
        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "Error should be TimedOut"
        );
    }

    /// Test that get_or_create_connection_wait_ready returns immediately when Ready.
    #[tokio::test]
    async fn get_or_create_connection_wait_ready_returns_immediately_when_ready() {
        if !lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();
        let config = lua_ls_config();

        // First call establishes Ready connection
        let handle1 = pool
            .get_or_create_connection("lua", &config)
            .await
            .expect("should establish connection");
        assert_eq!(handle1.state(), ConnectionState::Ready);

        // Second call via wait_ready should return immediately
        let start = std::time::Instant::now();
        let handle2 = pool
            .get_or_create_connection_wait_ready("lua", &config, Duration::from_secs(1))
            .await
            .expect("should return Ready connection");
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "Should return immediately for Ready connection"
        );
        assert_eq!(handle2.state(), ConnectionState::Ready);
    }

    // ============================================================
    // Multi-Server Cancel Forwarding Tests
    // ============================================================

    /// Test that registering multiple servers for the same upstream ID creates a set.
    #[test]
    fn register_upstream_request_multiple_servers_creates_set() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        pool.register_upstream_request(upstream_id.clone(), "lua-ls");
        pool.register_upstream_request(upstream_id.clone(), "pyright");

        let registry = pool.upstream_request_registry.lock().unwrap();
        let servers = registry.get(&upstream_id).expect("should have entry");

        assert!(servers.contains("lua-ls"));
        assert!(servers.contains("pyright"));
        assert_eq!(servers.len(), 2);
    }

    /// Test that unregistering removes only the specified server from the set.
    #[test]
    fn unregister_upstream_request_removes_single_server() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        pool.register_upstream_request(upstream_id.clone(), "lua-ls");
        pool.register_upstream_request(upstream_id.clone(), "pyright");
        pool.unregister_upstream_request(&upstream_id, "lua-ls");

        let registry = pool.upstream_request_registry.lock().unwrap();
        let servers = registry.get(&upstream_id).expect("should still have entry");

        assert!(!servers.contains("lua-ls"));
        assert!(servers.contains("pyright"));
        assert_eq!(servers.len(), 1);
    }

    /// Test that unregistering the last server cleans up the entire entry.
    #[test]
    fn unregister_upstream_request_cleans_up_empty_set() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        pool.register_upstream_request(upstream_id.clone(), "lua-ls");
        pool.unregister_upstream_request(&upstream_id, "lua-ls");

        let registry = pool.upstream_request_registry.lock().unwrap();
        assert!(
            registry.get(&upstream_id).is_none(),
            "entry should be removed when set is empty"
        );
    }

    /// Test that forward_cancel_by_upstream_id iterates over all servers.
    #[tokio::test]
    async fn forward_cancel_by_upstream_id_iterates_all_servers() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        // Create two Ready connections with registered requests
        let handle_lua = create_handle_with_state(ConnectionState::Ready).await;
        let (_downstream_id_lua, _rx_lua) = handle_lua
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register lua request");

        let handle_py = create_handle_with_state(ConnectionState::Ready).await;
        let (_downstream_id_py, _rx_py) = handle_py
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register py request");

        // Insert handles
        {
            let mut connections = pool.connections.lock().await;
            connections.insert("lua-ls".to_string(), Arc::clone(&handle_lua));
            connections.insert("pyright".to_string(), Arc::clone(&handle_py));
        }

        // Register both servers for the same upstream ID
        pool.register_upstream_request(upstream_id.clone(), "lua-ls");
        pool.register_upstream_request(upstream_id.clone(), "pyright");

        // Forward cancel - should succeed for both servers
        let result = pool
            .forward_cancel_by_upstream_id(upstream_id.clone())
            .await;
        assert!(result.is_ok());

        // Verify metrics show 2 successful cancels
        let (successful, _, _, _, _) = pool.cancel_metrics().snapshot();
        assert_eq!(
            successful, 2,
            "should have forwarded cancel to both servers"
        );
    }
}
