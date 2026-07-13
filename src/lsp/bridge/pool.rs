//! Pool of downstream language-server connections (ls-bridge-server-pool-coordination), keyed by
//! [`ConnectionKey`] = `(server_name, resolved workspace root)` rather than
//! `languageId`. Multiple languages can share one process (e.g. `typescript`
//! and `typescriptreact` → `tsgo`), while the same server under different
//! marker roots gets its own process for multi-root monorepos (issue #382).
//! Routing: `languageId` → `server_name` (config) plus the document's resolved
//! root → connection.
//!
//! [`LanguageServerPool`] manages the connections; [`ConnectionHandle`]
//! (ls-bridge-async-connection) and [`ConnectionState`] track each connection's lifecycle.

mod command_origin_registry;
mod connection_action;
mod connection_handle;
mod connection_key;
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
pub(crate) mod test_helpers;

pub(crate) use command_origin_registry::CommandOriginRegistry;
pub(crate) use connection_action::BridgeError;
use connection_action::{ConnectionAction, decide_connection_action};
use handshake::perform_lsp_handshake;

pub(crate) use connection_handle::{ConnectionHandle, NotificationSendResult};
pub(crate) use connection_key::ConnectionKey;
pub(crate) use connection_state::ConnectionState;
use document_tracker::DocumentTracker;
pub(crate) use document_tracker::OpenedVirtualDoc;
pub(crate) use dynamic_capability_registry::DynamicCapabilityRegistry;
pub(crate) use message_sender::{ConnectionHandleSender, MessageSender};
pub(crate) use shutdown_timeout::GlobalShutdownTimeout;

fn same_launch_config(
    old: &crate::config::settings::BridgeServerConfig,
    new: &crate::config::settings::BridgeServerConfig,
) -> bool {
    use crate::config::settings::BridgeServerConfig;
    let BridgeServerConfig {
        cmd: old_cmd,
        languages: old_languages,
        initialization_options: old_initialization_options,
        settings: _,
        workspace_markers: old_workspace_markers,
        on_type_formatting_triggers: old_on_type_formatting_triggers,
        prefer_shared_instance: _,
        enabled: _,
    } = old;
    let BridgeServerConfig {
        cmd: new_cmd,
        languages: new_languages,
        initialization_options: new_initialization_options,
        settings: _,
        workspace_markers: new_workspace_markers,
        on_type_formatting_triggers: new_on_type_formatting_triggers,
        prefer_shared_instance: _,
        enabled: _,
    } = new;
    old_cmd == new_cmd
        && old_languages == new_languages
        && old_initialization_options == new_initialization_options
        && old_workspace_markers == new_workspace_markers
        && old_on_type_formatting_triggers == new_on_type_formatting_triggers
        && old.prefers_shared_instance() == new.prefers_shared_instance()
        && old.is_enabled() == new.is_enabled()
}

fn shutdown_invalidated_connection(key: ConnectionKey, handle: Arc<ConnectionHandle>) {
    tokio::spawn(async move {
        const RELOAD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
        let shutdown_handle = Arc::clone(&handle);
        let shutdown_task = tokio::spawn(async move { shutdown_handle.graceful_shutdown().await });
        let abort = shutdown_task.abort_handle();
        match tokio::time::timeout(RELOAD_SHUTDOWN_TIMEOUT, shutdown_task).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                log::error!(
                    target: "kakehashi::bridge",
                    "Shutdown task for invalidated {} connection failed: {}",
                    key,
                    error
                );
                handle.complete_shutdown();
            }
            Err(_) => {
                abort.abort();
                log::warn!(
                    target: "kakehashi::bridge",
                    "Timed out shutting down invalidated {} connection",
                    key
                );
                handle.complete_shutdown();
            }
        }
    });
}

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Mutex;
use url::Url;

use tower_lsp_server::ls_types::{CancelParams, NumberOrString};

use crate::error::LockResultExt;

use super::protocol::{
    JsonRpcNotification, RequestId, VirtualDocumentUri,
    build_did_change_configuration_notification, build_did_change_workspace_folders_notification,
    build_didopen_notification,
};

/// Timeout for the LSP initialize handshake and for wait-for-ready on an
/// initializing server (ls-bridge-timeout-hierarchy Tier 0: 30-60s recommended). A downstream
/// server that doesn't respond within this window fails with a timeout error.
pub(crate) const INIT_TIMEOUT_SECS: u64 = 30;

use super::actor::{
    OUTBOUND_QUEUE_CAPACITY, OutboundMessage, ResponseRouter, ServerRequestDeps,
    UpstreamNotification, UpstreamRequest, spawn_reader_task_for_server,
};
use super::connection::AsyncBridgeConnection;

/// Upstream request ID carrying both numeric and string variants, since LSP
/// defines `id: integer | string` (LSP 3.17 `CancelParams`). Both must be
/// supported so cancel forwarding works regardless of which ID type the client uses.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum UpstreamId {
    /// Numeric request ID (most common)
    Number(i64),
    /// String request ID (less common but valid per LSP spec)
    String(String),
}

impl std::fmt::Display for UpstreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamId::Number(n) => write!(f, "{}", n),
            UpstreamId::String(s) => write!(f, "\"{}\"", s),
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

/// Pool of connections to downstream language servers (ls-bridge-server-pool-coordination), one per
/// `server_name`, with connections lazily initialized via the LSP handshake and
/// per-connection state embedded in each `ConnectionHandle` (ls-bridge-message-ordering).
///
/// `pub` so a shared pool can be wired into the cancel forwarding middleware;
/// normal usage should go through `BridgeCoordinator`.
/// Sync state of one host document on one downstream server
/// (host-document-bridge): the version sent in the last `didOpen`/`didChange`
/// and a fingerprint of the synced text for cheap change detection.
pub(super) struct HostDocSyncState {
    pub(super) version: i32,
    pub(super) fingerprint: u64,
}

/// Cancellation-safe rollback for the pre-send virtual-document claim.
/// Eager-open tasks are deliberately aborted when a newer parse supersedes
/// them; dropping the handler between claim and FIFO enqueue must not leave a
/// phantom "opened" document that suppresses every later didOpen.
struct OpenClaimGuard {
    tracker: Arc<DocumentTracker>,
    claim: Arc<tokio::sync::Notify>,
    transition: Arc<tokio::sync::Mutex<()>>,
    transition_locks: Arc<OpenTransitionLocks>,
    host_uri: Url,
    virtual_uri: VirtualDocumentUri,
    connection_key: ConnectionKey,
    armed: bool,
}

type OpenTransitionLocks = DashMap<(ConnectionKey, String), Arc<tokio::sync::Mutex<()>>>;
type HostLifecycleLocks = DashMap<Url, Arc<tokio::sync::Mutex<()>>>;
pub(crate) struct HostVirtualContents {
    // The open lifetime that owns this container. Reopen replaces the whole
    // container, so a stale publisher holding the old DashMap guard can only
    // mutate detached state that current didOpen readers cannot observe.
    pub(crate) incarnation: u64,
    contents: DashMap<String, DashMap<String, Arc<str>>>,
}

type LatestVirtualContents = DashMap<Url, HostVirtualContents>;

impl OpenClaimGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OpenClaimGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let tracker = Arc::clone(&self.tracker);
        let claim = Arc::clone(&self.claim);
        let transition = Arc::clone(&self.transition);
        let transition_locks = Arc::clone(&self.transition_locks);
        let host_uri = self.host_uri.clone();
        let virtual_uri = self.virtual_uri.clone();
        let connection_key = self.connection_key.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let transition_guard = transition.lock().await;
                tracker
                    .rollback_open_claim_if(&host_uri, &virtual_uri, &connection_key, &claim)
                    .await;
                drop(transition_guard);
                let key = (connection_key, virtual_uri.to_uri_string());
                transition_locks.remove_if(&key, |_, current| {
                    Arc::ptr_eq(current, &transition) && Arc::strong_count(current) == 2
                });
            });
        }
    }
}

pub struct LanguageServerPool {
    /// Map of connection key `(server_name, root)` -> connection handle.
    ///
    /// Keyed by [`ConnectionKey`] rather than `server_name` alone so a
    /// multi-root monorepo spawns a separate downstream process per resolved
    /// workspace root (issue #382); documents sharing a root (or the
    /// client-root fallback) still share one process.
    connections: Mutex<HashMap<ConnectionKey, Arc<ConnectionHandle>>>,
    /// Gate that rejects **new** connection spawns once shutdown has begun.
    ///
    /// `shutdown_all` snapshots the live connections and tears them down, but a
    /// late eager spawn (an in-flight `process_injections` / eager-open task whose
    /// body started before `abort_all_eager_open` could cancel it) could otherwise
    /// create and insert a connection *after* that snapshot — escaping teardown and
    /// leaking a child process past shutdown. Set true at the very start of
    /// `shutdown_all_with_timeout`, and checked inside the `connections` lock in
    /// `get_or_create_connection_resolved`: holding that same lock makes the
    /// set-then-snapshot and the check-then-insert mutually exclusive, so a spawn
    /// either lands before the snapshot (and is torn down) or is rejected. The
    /// "lands before the snapshot ⟹ torn down" half relies on a freshly inserted
    /// handle being `Initializing`, which `shutdown_all_with_timeout`'s snapshot
    /// filter includes (alongside `Ready`); narrowing that filter would reopen the
    /// window. Additive to `abort_all_eager_open` + the per-task `CancellationToken`s,
    /// which already stop most eager spawns before they reach this point.
    shutting_down: AtomicBool,
    /// Document tracking for virtual documents (versions, host mappings, opened state)
    document_tracker: Arc<DocumentTracker>,
    /// Serializes didOpen enqueue/promotion, rollback, and didClose per exact
    /// downstream document so wire order matches tracker state transitions.
    open_transition_locks: Arc<OpenTransitionLocks>,
    /// Serializes host-lifetime replacement with incarnation-bound eager opens.
    host_lifecycle_locks: HostLifecycleLocks,
    /// Latest extracted content observed from host didChange for each injection.
    /// Interactive requests may carry an older snapshot while awaiting server
    /// startup; the didOpen path reads this after claiming the transition.
    latest_virtual_contents: LatestVirtualContents,
    /// Host-document sync state per `(uri, connection key)`
    /// (host-document-bridge): the real-URI documents opened on downstream
    /// servers via `bridge._self`, with their version and content
    /// fingerprint for lazy full-text re-sync.
    host_documents: Mutex<HashMap<(String, ConnectionKey), HostDocSyncState>>,
    /// Upstream request ID → set of downstream connections, for fan-out cancel
    /// forwarding (ls-bridge-message-ordering). Multiple connections can share an ID when a single
    /// upstream request (e.g. diagnostic) targets several injected languages.
    ///
    /// Cleaned per-connection via `unregister_upstream_request` on response or
    /// pre-send failure; the entry is removed when the last connection unregisters.
    /// Connection failure via `ResponseRouter::fail_all()` deliberately leaves
    /// entries dangling to avoid a router→pool back-reference — stale lookups
    /// fail gracefully and IDs get reused.
    upstream_request_registry: std::sync::Mutex<HashMap<UpstreamId, HashMap<ConnectionKey, usize>>>,
    /// Metrics for cancel forwarding observability.
    cancel_metrics: CancelForwardingMetrics,
    /// Consecutive handshake-task panics per connection (reset to 0 on success).
    /// At `MAX_CONSECUTIVE_PANICS` we stop retrying, preventing an infinite
    /// retry loop when a connection's handshake consistently panics.
    consecutive_panic_counts: std::sync::Mutex<HashMap<ConnectionKey, u32>>,
    /// Workspace root URI forwarded from upstream client.
    ///
    /// Seeded during initialize and updated when the primary workspace folder changes.
    /// Passed to downstream servers during LSP handshake so they can provide
    /// workspace-aware features (diagnostics, go-to-definition, etc.).
    root_uri: arc_swap::ArcSwap<Option<String>>,
    /// Current workspace folders from the upstream client. Seeded during
    /// initialize, then updated by `workspace/didChangeWorkspaceFolders` and
    /// snapshotted for each later downstream handshake.
    workspace_folders: super::WorkspaceFolderSet,
    /// Client capabilities forwarded from upstream client.
    ///
    /// Set once via `set_client_capabilities()` after receiving the upstream initialize request.
    /// Merged into bridge defaults during downstream LSP handshake so servers
    /// can provide richer responses (e.g., markdown docs, resolve support).
    client_capabilities: OnceLock<tower_lsp_server::ls_types::ClientCapabilities>,
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
    /// Sender for best-effort `window/*` notifications (#378).
    ///
    /// Bounded with drop-on-full so a log-flooding downstream server cannot
    /// grow memory or starve `DiagnosticRefresh` (loss-tolerance split
    /// documented on `UpstreamNotification`).
    window_tx: tokio::sync::mpsc::Sender<UpstreamNotification>,
    /// Receiver for window notifications, taken once by the forwarding task.
    window_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<UpstreamNotification>>>,
    /// Sender for downstream-initiated requests forwarded to the editor with a
    /// response relayed back (`window/showMessageRequest`, `window/showDocument`,
    /// `workspace/applyEdit` — answered locally when the editor lacks the
    /// capability).
    ///
    /// Cloned into each reader task. Unbounded: a dropped request would hang the
    /// downstream waiting for a response (loss-intolerant, like `upstream_tx`).
    upstream_request_tx: tokio::sync::mpsc::UnboundedSender<UpstreamRequest>,
    /// Receiver for upstream requests, taken once by the forwarding task.
    upstream_request_rx:
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<UpstreamRequest>>>,
    /// Remaps downstream-declared work-done progress tokens to globally unique
    /// upstream tokens (window-work-done-progress bridging). Shared with every
    /// reader task (to register/translate) and the cancel path (to route a
    /// client `window/workDoneProgress/cancel` to the owning downstream).
    progress_registry: Arc<super::ProgressRegistry>,
    /// Routes bridge-minted per-server client-progress tokens to their
    /// aggregator (ls-bridge-client-progress). Shared with every reader task (to
    /// route an incoming `$/progress`) and the dispatch path (register on
    /// fan-out / deregister on completion).
    client_progress_registry: Arc<super::ClientProgressRegistry>,
    /// Tracks downstream-initiated requests forwarded to the editor so a
    /// downstream `$/cancelRequest` (or connection death) can cancel the
    /// editor-bound request (#404) — capability-gated applyEdits are
    /// registered too, though answered locally. Shared with every reader task (to register /
    /// fire) and the forwarding loop (to await / drop). Distinct from
    /// `upstream_request_registry`, which is the *outbound* (editor → downstream)
    /// cancel direction.
    inbound_request_registry: super::InboundRequestRegistry,
    /// Palette command name → origin server (#628 palette-fired executeCommand).
    command_origins: Arc<CommandOriginRegistry>,
}

impl Default for LanguageServerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageServerPool {
    /// Create a new language server pool.
    ///
    /// `pub` for cancel forwarding middleware setup: a shared
    /// `Arc<LanguageServerPool>` is passed to both `Kakehashi::with_pool()` and
    /// `CancelForwarder::new()`.
    pub fn new() -> Self {
        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (window_tx, window_rx) =
            tokio::sync::mpsc::channel(super::actor::WINDOW_NOTIFICATION_QUEUE_CAPACITY);
        let (upstream_request_tx, upstream_request_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            connections: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            document_tracker: Arc::new(DocumentTracker::new()),
            open_transition_locks: Arc::new(DashMap::new()),
            host_lifecycle_locks: DashMap::new(),
            latest_virtual_contents: DashMap::new(),
            host_documents: Mutex::new(HashMap::new()),
            upstream_request_registry: std::sync::Mutex::new(HashMap::new()),
            cancel_metrics: CancelForwardingMetrics::default(),
            consecutive_panic_counts: std::sync::Mutex::new(HashMap::new()),
            root_uri: arc_swap::ArcSwap::new(Arc::new(None)),
            workspace_folders: super::WorkspaceFolderSet::new(None),
            client_capabilities: OnceLock::new(),
            upstream_tx,
            upstream_rx: std::sync::Mutex::new(Some(upstream_rx)),
            window_tx,
            window_rx: std::sync::Mutex::new(Some(window_rx)),
            upstream_request_tx,
            upstream_request_rx: std::sync::Mutex::new(Some(upstream_request_rx)),
            progress_registry: Arc::new(super::ProgressRegistry::new()),
            client_progress_registry: Arc::new(super::ClientProgressRegistry::new()),
            inbound_request_registry: super::InboundRequestRegistry::default(),
            command_origins: Arc::new(CommandOriginRegistry::default()),
        }
    }

    /// Registry mapping a downstream server's advertised palette command names to
    /// the connection that advertised them (#628 palette-fired executeCommand).
    pub(crate) fn command_origins(&self) -> &CommandOriginRegistry {
        &self.command_origins
    }

    /// The existing `Ready` connection for `key`, if one is live. Used to route a
    /// palette command back to the exact connection that advertised it (right
    /// workspace root/context) rather than spawning a fresh client-root one.
    pub(crate) async fn ready_connection_by_key(
        &self,
        key: &ConnectionKey,
    ) -> Option<Arc<ConnectionHandle>> {
        let connections = self.connections.lock().await;
        connections
            .get(key)
            .filter(|handle| handle.state() == ConnectionState::Ready)
            .map(Arc::clone)
    }

    /// Shared registry of in-flight forwarded requests, handed to the forwarding
    /// loop so it can await each request's cancel token and drop settled entries
    /// (#404).
    pub(crate) fn inbound_request_registry(&self) -> super::InboundRequestRegistry {
        self.inbound_request_registry.clone()
    }

    /// Shared work-done progress token registry (for seam tests that need to
    /// register a token and observe cancel routing).
    #[cfg(test)]
    pub(crate) fn progress_registry(&self) -> &Arc<super::ProgressRegistry> {
        &self.progress_registry
    }

    /// Routes bridge-minted client-progress tokens to their aggregator
    /// (ls-bridge-client-progress); shared with reader tasks (route incoming
    /// `$/progress`) and the dispatch path (register/deregister).
    pub(crate) fn client_progress_registry(&self) -> &Arc<super::ClientProgressRegistry> {
        &self.client_progress_registry
    }

    /// A clone of the upstream-notification sender, for emitting aggregated
    /// client progress (and its synthetic teardown `End`) to the editor.
    pub(crate) fn upstream_tx(
        &self,
    ) -> tokio::sync::mpsc::UnboundedSender<super::UpstreamNotification> {
        self.upstream_tx.clone()
    }

    /// Set the workspace root URI.
    ///
    /// Called during initialize and when the primary workspace folder changes.
    pub(crate) fn set_root_uri(&self, uri: Option<String>) {
        self.root_uri.store(Arc::new(uri));
    }

    /// Get the workspace root URI.
    fn root_uri(&self) -> Option<String> {
        self.root_uri.load().as_ref().clone()
    }

    /// Set the workspace folders.
    ///
    /// Called during upstream initialize to seed the workspace-folder snapshot.
    pub(crate) fn set_workspace_folders(
        &self,
        folders: Option<Vec<tower_lsp_server::ls_types::WorkspaceFolder>>,
    ) {
        self.workspace_folders.replace(folders);
    }

    /// Get the workspace folders.
    pub(crate) fn workspace_folders(
        &self,
    ) -> Option<Vec<tower_lsp_server::ls_types::WorkspaceFolder>> {
        self.workspace_folders.snapshot()
    }

    /// Update the upstream client workspace snapshot used by future
    /// client-fallback downstream connections.
    pub(crate) async fn apply_workspace_folder_change(
        &self,
        added: Vec<tower_lsp_server::ls_types::WorkspaceFolder>,
        removed: &[tower_lsp_server::ls_types::WorkspaceFolder],
    ) {
        self.workspace_folders.apply_change(added.clone(), removed);
        self.set_root_uri(
            self.workspace_folders()
                .and_then(|folders| folders.first().map(|folder| folder.uri.to_string())),
        );

        let connections = self.connections.lock().await;
        for handle in connections.values().filter(|handle| {
            handle.state() == ConnectionState::Ready && handle.supports_workspace_folder_changes()
        }) {
            let notification =
                build_did_change_workspace_folders_notification(added.clone(), removed.to_vec());
            if handle.send_notification(notification) == NotificationSendResult::Queued {
                handle
                    .workspace_folders()
                    .apply_change(added.clone(), removed);
            }
        }
    }

    /// Set the upstream client capabilities.
    ///
    /// Called once during upstream initialize to forward capabilities to downstream servers.
    /// Subsequent calls are ignored (OnceLock semantics).
    pub(crate) fn set_client_capabilities(
        &self,
        caps: tower_lsp_server::ls_types::ClientCapabilities,
    ) {
        let _ = self.client_capabilities.set(caps);
    }

    /// Get the upstream client capabilities.
    fn client_capabilities(&self) -> Option<tower_lsp_server::ls_types::ClientCapabilities> {
        self.client_capabilities.get().cloned()
    }

    /// Take the upstream notification receiver for forwarding to the editor.
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// The receiver should be consumed by a single forwarding task in `initialized()`.
    pub(crate) fn take_upstream_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<UpstreamNotification>> {
        let mut guard = self
            .upstream_rx
            .lock()
            .recover_poison("LanguageServerPool::take_upstream_rx");
        guard.take()
    }

    /// Take the window notification receiver for forwarding to the editor.
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// The receiver should be consumed by the same forwarding task as
    /// `take_upstream_rx`'s.
    pub(crate) fn take_window_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<UpstreamNotification>> {
        let mut guard = self
            .window_rx
            .lock()
            .recover_poison("LanguageServerPool::take_window_rx");
        guard.take()
    }

    /// Take the upstream request receiver for forwarding to the editor.
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// The receiver should be consumed by the same forwarding task as
    /// `take_upstream_rx`'s.
    pub(crate) fn take_upstream_request_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<UpstreamRequest>> {
        let mut guard = self
            .upstream_request_rx
            .lock()
            .recover_poison("LanguageServerPool::take_upstream_request_rx");
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
    ) -> tokio::sync::MutexGuard<'_, HashMap<ConnectionKey, Arc<ConnectionHandle>>> {
        self.connections.lock().await
    }

    /// Apply a resolved server-config reload to every live connection.
    ///
    /// `resolve` maps a server name to its newly merged config. A server that
    /// vanished, or whose spawn-time config changed, is removed and shut down;
    /// its next use therefore spawns from the new config. `settings` is the one
    /// runtime-mutable field: it is diffed against the connection's current
    /// settings cell and, on a change, the cell is re-stored — so a later
    /// `workspace/configuration` re-pull reflects it — and a best-effort
    /// `workspace/didChangeConfiguration` is pushed (carrying the new value, or
    /// `null` when settings were cleared). Unchanged servers get nothing, so a
    /// global reload does not storm every connection. The cell is updated before
    /// the push so a pull-model server's re-pull never races onto the old value.
    ///
    /// Returns the number of connections that changed (and were pushed), so the
    /// anti-storm behavior is observable to callers and tests.
    pub(crate) async fn propagate_settings(
        &self,
        resolve: impl Fn(&str) -> Option<crate::config::settings::BridgeServerConfig>,
    ) -> usize {
        let mut connections = self.connections.lock().await;
        let mut invalidated = Vec::new();
        let mut resolved = HashMap::new();
        for (key, handle) in connections.iter() {
            let config = resolve(key.server());
            let launch_changed = match (handle.launch_config(), config.as_ref()) {
                (Some(old), Some(new)) => !same_launch_config(old, new),
                (Some(_), None) => true,
                // Test-only handles created without a spawn snapshot retain the
                // legacy settings-propagation behavior.
                (None, _) => false,
            };
            if launch_changed {
                invalidated.push(key.clone());
            } else {
                resolved.insert(key.clone(), config.and_then(|config| config.settings));
            }
        }

        let mut stale_handles = Vec::new();
        for key in invalidated {
            if let Some(handle) = connections.get(&key) {
                handle.begin_shutdown();
            }
            self.host_documents
                .lock()
                .await
                .retain(|(_, connection_key), _| connection_key != &key);
            self.document_tracker.purge_connection(&key).await;
            self.purge_open_transition_locks(&key).await;
            if let Some(handle) = connections.remove(&key) {
                stale_handles.push((key, handle));
            }
        }

        let mut pushed = 0;
        for (key, handle) in connections.iter() {
            let resolved = resolved.remove(key).flatten();
            // Diff by value (Arc identity is irrelevant); skip unchanged servers
            // so an unchanged config reload pushes nothing.
            if resolved.as_ref() == handle.current_settings().as_deref() {
                continue;
            }
            // Always advance the cell so a later pull / the post-`initialized`
            // push (path a) reflects the change, even for a connection we don't
            // notify yet. Wrap once in `Arc` and store a cheap `Arc` clone; the
            // inner `Value` is cloned only for an actual push below.
            let resolved = resolved.map(Arc::new);
            handle.store_settings(resolved.clone());
            // Only notify a Ready connection: sending
            // `workspace/didChangeConfiguration` to a still-initializing server
            // would precede its `initialized` and violate LSP ordering. An
            // Initializing connection instead gets its (now-updated) cell pushed
            // by path (a) once the handshake completes. Count only a successful
            // enqueue, so the returned count never reports a dropped push.
            //
            // No `Arc::ptr_eq`-against-the-map liveness recheck is needed here:
            // this loop holds the `connections` lock and iterates the map's live
            // values directly (never retrieve-then-release), so a respawn — which
            // takes the same lock — cannot replace a handle mid-iteration.
            if handle.state() == ConnectionState::Ready {
                let payload = resolved
                    .as_deref()
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let result =
                    handle.send_notification(build_did_change_configuration_notification(payload));
                if result == NotificationSendResult::Queued {
                    pushed += 1;
                }
            }
        }
        drop(connections);
        for (key, handle) in stale_handles {
            shutdown_invalidated_connection(key, handle);
        }
        pushed
    }

    /// Among `candidates`, the server names that have a **live connection
    /// advertising pull diagnostics** — `textDocument/diagnostic`, via static
    /// initialize caps or a dynamic registration — checked WITHOUT creating a
    /// connection (push-propagation-diagnostic-forwarding "Per-server source and
    /// fallback", #425). Classification is therefore *live*: a server that
    /// registers/unregisters pull support mid-session is reclassified by the
    /// next call.
    ///
    /// The proactive publisher uses this to classify which cached push slots
    /// come from a pull-driven server, so the host-event pull (`PullLayer`) and
    /// that server's spontaneous push don't double-count it.
    ///
    /// Classification is **by server name, not by the producing connection**: a
    /// server with several `(server, root)` connections counts as pull-driven if
    /// *any* of them advertises the capability. Static initialize caps are
    /// identical across instances of one binary, so this only diverges if two
    /// instances hold *different dynamic* diagnostic registrations — an exotic
    /// case that could misclassify the non-registering instance's push. A precise
    /// per-`connection_id` classification belongs to the deferred per-source
    /// fan-in (push-propagation-diagnostic-forwarding); accepted and documented.
    pub(crate) async fn pull_driven_servers(
        &self,
        candidates: &std::collections::HashSet<&str>,
    ) -> std::collections::HashSet<String> {
        if candidates.is_empty() {
            return std::collections::HashSet::new();
        }
        let connections = self.connections.lock().await;
        let mut pull_driven = std::collections::HashSet::new();
        for handle in connections.values() {
            let server = handle.key().server();
            if candidates.contains(server)
                && !pull_driven.contains(server)
                && handle.has_capability("textDocument/diagnostic")
            {
                pull_driven.insert(server.to_string());
                // `pull_driven` only ever holds names from `candidates`, so once
                // every candidate has matched there is nothing left to find —
                // stop scanning the remaining connections.
                if pull_driven.len() == candidates.len() {
                    break;
                }
            }
        }
        pull_driven
    }

    /// Among `candidates`, the server names **known** not to support `method`:
    /// the server has at least one live connection past initialization (`Ready`)
    /// and NONE of its live connections advertises `method` (via static
    /// initialize caps or a dynamic registration) — checked WITHOUT creating a
    /// connection (capability-prefilter-fanout).
    ///
    /// The inverse of `has_capability`: fan-out builders use this to drop a
    /// server from a region's candidate set *before* spawning a task, so a
    /// request never spins up a task + connection lookup only to hit the
    /// per-handler capability gate and return an empty result.
    ///
    /// Deliberately conservative — a candidate is returned ONLY when it has at
    /// least one `Ready` connection AND none of its connections advertises
    /// `method`. Concretely:
    /// - **No live connection at all** → NOT returned. The capability is unknown
    ///   until the server is spawned + handshaked, so the caller keeps it and
    ///   lets the request spawn it, exactly as before this filter existed.
    /// - **Only non-`Ready` connections** (`Initializing`, `Failed`, `Closing`,
    ///   `Closed`) and no `Ready` one → NOT returned. `server_capabilities` may
    ///   not be in yet (`has_capability` is a premature `false`), and
    ///   `get_or_create_connection_wait_ready` waits for `Ready` (or respawns a
    ///   `Failed` one) before checking, so dropping here would skip a server that
    ///   will answer.
    ///
    /// # Soundness boundary and the across-roots limitation
    ///
    /// Classification is **by server name across roots**: a server is capable if
    /// ANY of its `(server, root)` connections advertises `method`, incapable if
    /// it has a `Ready` connection and NONE does. This is exactly sound **iff the
    /// capability is root-invariant** — the near-universal reality, since every
    /// instance of one binary reports identical *static* `initialize`
    /// capabilities (this is what makes a statically pull-incapable server like
    /// basedpyright safe to drop on every root). It is NOT precise about the
    /// serving root: if a `Ready` connection for `(server, rootA)` lacks `method`
    /// while `(server, rootB)` would advertise it, a request in rootB is dropped
    /// even though rootB has no connection yet or only an `Initializing` one — so
    /// the "non-`Ready` is never dropped" rule above holds only when that is the
    /// server's SOLE connection.
    ///
    /// That over-drop can fire only under genuine per-root capability
    /// **divergence** (per-root `initializationOptions` that change advertised
    /// static caps, or a per-instance *dynamic* registration) AND the serving
    /// root having no `Ready`-capable connection. It is impossible in a
    /// single-root workspace, and not permanent: any request via an exempt method
    /// (see `capability_prefilter_applies`) or any other path spawns the
    /// divergent root's instance and self-corrects the classification. Scoping to
    /// the request's `(server, root)` would need a per-server marker resolution
    /// (`resolve_marker_and_key`, filesystem I/O) on this hot path, which would
    /// erode the win this filter exists for; the per-handler capability gate
    /// remains authoritative for every server this does keep.
    pub(crate) async fn servers_known_incapable(
        &self,
        candidates: &std::collections::HashSet<&str>,
        method: &str,
    ) -> std::collections::HashSet<String> {
        if candidates.is_empty() {
            return std::collections::HashSet::new();
        }
        let connections = self.connections.lock().await;
        // Accumulate into `candidates`-lifetime references (via `candidates.get`),
        // NOT borrows of the `connections` map: that lets us drop the lock before
        // the final owned materialization, so the `connections` mutex — contended
        // by every bridge request — is held only for the scan, not the
        // `candidates.iter()/collect()` pass below.
        let mut capable: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut has_ready: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for handle in connections.values() {
            let Some(&candidate) = candidates.get(handle.key().server()) else {
                continue;
            };
            if handle.has_capability(method) {
                capable.insert(candidate);
            }
            if handle.state() == ConnectionState::Ready {
                has_ready.insert(candidate);
            }
        }
        drop(connections);
        candidates
            .iter()
            .filter(|&&name| has_ready.contains(name) && !capable.contains(name))
            .map(|&name| name.to_string())
            .collect()
    }

    /// Host-document sync state (host-document-bridge). Used by the host
    /// request path in `text_document/host.rs`.
    pub(super) async fn host_documents(
        &self,
    ) -> tokio::sync::MutexGuard<'_, HashMap<(String, ConnectionKey), HostDocSyncState>> {
        self.host_documents.lock().await
    }

    /// Insert a pre-created connection handle for testing.
    ///
    /// This allows tests to set up a pool with a known connection state
    /// without going through the full server spawn + handshake flow. The handle
    /// is keyed by its own `handle.key()` so lookups via the key (didChange,
    /// cancel) agree with the map. `pub(crate)` (not bridge-private) so the
    /// capability-prefilter regression test in `lsp_impl` can seed a Ready
    /// downstream (capability-prefilter-fanout).
    #[cfg(test)]
    pub(crate) async fn insert_connection(&self, handle: Arc<ConnectionHandle>) {
        let mut connections = self.connections.lock().await;
        connections.insert(handle.key().clone(), handle);
    }

    // ========================================
    // DocumentTracker delegation methods
    // ========================================

    /// Snapshot (without removing) every virtual document currently open for a
    /// host URI. Used by the save-notification fan-out (#357).
    pub(crate) async fn host_virtual_docs(&self, host_uri: &Url) -> Vec<OpenedVirtualDoc> {
        self.document_tracker.host_virtual_docs(host_uri).await
    }

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
    /// Atomic: lookup and removal happen in a single lock acquisition, preventing
    /// races with concurrent didOpen requests. Documents that were never opened
    /// (not in `host_to_virtual`) are not returned.
    pub(crate) async fn remove_matching_virtual_docs(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) -> Vec<OpenedVirtualDoc> {
        self.document_tracker
            .remove_matching_virtual_docs(host_uri, invalidated_ulids)
            .await
    }

    pub(crate) async fn remove_replaced_virtual_docs(
        &self,
        host_uri: &Url,
        expected_languages: &std::collections::HashMap<&str, &str>,
    ) -> Vec<OpenedVirtualDoc> {
        self.document_tracker
            .remove_replaced_virtual_docs(host_uri, expected_languages)
            .await
    }

    /// Remove a document from all tracking state (version tracking and opened state).
    pub(crate) async fn untrack_document(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
    ) {
        self.document_tracker
            .untrack_document(virtual_uri, connection_key)
            .await
    }

    /// Remove a virtual document's `host_to_virtual` registration.
    ///
    /// `untrack_document` clears `document_versions`, `opened_documents`, and
    /// the reverse index but deliberately leaves `host_to_virtual` for the
    /// host-close flow. Scratch documents (concatenated formatting pipeline) are
    /// addressed directly and never go through that flow, so they must remove
    /// their own `host_to_virtual` entry here to avoid lingering / double-close.
    pub(crate) async fn unregister_virtual_doc(
        &self,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
    ) {
        self.document_tracker
            .unregister_virtual_doc(host_uri, virtual_uri)
            .await
    }

    /// Whether didOpen has been enqueued on at least one downstream connection.
    ///
    /// Fast synchronous check used to gate operations on documents not yet known
    /// downstream. Pre-send claims deliberately remain invisible.
    pub(crate) fn is_document_opened(&self, virtual_uri: &VirtualDocumentUri) -> bool {
        self.document_tracker.is_document_opened(virtual_uri)
    }

    pub(super) fn is_document_opened_on_connection(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
    ) -> bool {
        self.document_tracker
            .is_document_opened_on_connection(virtual_uri, connection_key)
    }

    pub(super) fn document_connection_generation(&self, connection_key: &ConnectionKey) -> u64 {
        self.document_tracker.connection_generation(connection_key)
    }

    /// Whether the host document at `host_uri` has been synced (didOpen sent) to a
    /// `_self` host server named `server_name` — i.e. a `host_documents` sync-state
    /// entry exists for that `(uri, server)`. Used to verify host-layer eager open.
    #[cfg(test)]
    pub(crate) async fn is_host_document_opened(&self, host_uri: &Url, server_name: &str) -> bool {
        // Key exactly as `sync_host_document` does (`doc.uri.to_string()`) so the
        // lookup can never diverge from the map's key construction.
        let key = host_uri.to_string();
        self.host_documents()
            .await
            .keys()
            .any(|(uri, connection_key)| uri == &key && connection_key.server() == server_name)
    }

    /// The current sync version of the host document at `host_uri` on `server_name`,
    /// or `None` if not synced. `sync_host_document` sets v1 on the didOpen and
    /// bumps it on each content-changing didChange — so a value > 1 proves a re-sync
    /// happened. Used to verify on-edit host re-sync (#431).
    #[cfg(test)]
    pub(crate) async fn host_document_version(
        &self,
        host_uri: &Url,
        server_name: &str,
    ) -> Option<i32> {
        let key = host_uri.to_string();
        self.host_documents()
            .await
            .iter()
            .find(|((uri, connection_key), _)| {
                uri == &key && connection_key.server() == server_name
            })
            .map(|(_, state)| state.version)
    }

    /// Resolve a virtual-document URI string to its `(host_url, region_id)`
    /// (used by `window/showDocument` translation). See
    /// [`DocumentTracker::resolve_virtual_uri`].
    pub(super) async fn resolve_virtual_uri(&self, virtual_uri: &str) -> Option<(Url, String)> {
        self.document_tracker.resolve_virtual_uri(virtual_uri).await
    }

    /// The version currently tracked for a virtual document on one connection
    /// (used by the inbound `workspace/applyEdit` version validation). See
    /// [`DocumentTracker::document_version`].
    pub(super) async fn virtual_document_version(
        &self,
        virtual_uri: &str,
        connection_key: &ConnectionKey,
    ) -> Option<i32> {
        self.document_tracker
            .document_version(virtual_uri, connection_key)
            .await
    }

    /// Find ALL connections (`(server, root)` keys) that have opened a given
    /// virtual document URI.
    ///
    /// Used by did_change to forward notifications to every connection that has
    /// the document open, not just the first one found.
    pub(super) fn get_all_connections_for_virtual_uri(
        &self,
        virtual_uri: &VirtualDocumentUri,
    ) -> Vec<ConnectionKey> {
        self.document_tracker
            .get_all_connections_for_virtual_uri(virtual_uri)
    }

    pub(super) fn connections_opening_or_opened(
        &self,
        virtual_uri: &VirtualDocumentUri,
    ) -> Vec<ConnectionKey> {
        self.document_tracker
            .connections_opening_or_opened(virtual_uri)
    }

    pub(super) fn open_transition_lock(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
    ) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            &self
                .open_transition_locks
                .entry((connection_key.clone(), virtual_uri.to_uri_string()))
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    pub(super) fn remove_open_transition_lock_if_unshared(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
        transition: &Arc<tokio::sync::Mutex<()>>,
    ) {
        let key = (connection_key.clone(), virtual_uri.to_uri_string());
        self.open_transition_locks.remove_if(&key, |_, current| {
            Arc::ptr_eq(current, transition) && Arc::strong_count(current) == 2
        });
    }

    async fn purge_open_transition_locks(&self, connection_key: &ConnectionKey) {
        let transitions: Vec<_> = self
            .open_transition_locks
            .iter()
            .filter(|entry| &entry.key().0 == connection_key)
            .map(|entry| (entry.key().clone(), Arc::clone(entry.value())))
            .collect();
        for (key, transition) in transitions {
            let guard = transition.lock().await;
            drop(guard);
            self.open_transition_locks.remove_if(&key, |_, current| {
                Arc::ptr_eq(current, &transition) && Arc::strong_count(current) == 2
            });
        }
    }

    /// Resolve the exact `(server, root)` connection a document currently
    /// routes to, including shared-instance capability fallback.
    pub(super) async fn resolved_connection_key(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: &Url,
    ) -> ConnectionKey {
        self.resolve_acquire(server_name, server_config, Some(document_uri))
            .await
            .1
    }

    /// Membership check for the save fan-out liveness recheck (avoids the
    /// reverse-index `Vec<ConnectionKey>` clone): is the virtual document at
    /// `virtual_uri` (its URI string) open on `connection_key`?
    pub(super) fn is_virtual_doc_open_on_connection(
        &self,
        virtual_uri: &str,
        connection_key: &ConnectionKey,
    ) -> bool {
        self.document_tracker
            .is_virtual_doc_open_on_connection(virtual_uri, connection_key)
    }

    /// Register a document as successfully opened (test helper).
    ///
    /// Delegates to `DocumentTracker::register_opened_document()`.
    /// This sets version, host-to-virtual mapping, and opened state atomically.
    #[cfg(test)]
    pub(super) async fn register_opened_document(
        &self,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
    ) {
        self.document_tracker
            .register_opened_document(host_uri, virtual_uri, connection_key)
            .await
    }

    /// Send `didOpen` for the virtual document if not already opened, registering
    /// all tracking state on success. Callers handle error cleanup (router entry
    /// and upstream-request registry).
    ///
    /// Uses `try_claim_for_open()` as a compare-and-swap. Concurrent callers wait
    /// for the owner to enqueue didOpen or finish rollback, then either reuse the
    /// sent open or claim and retry. `host_to_virtual` is registered pre-send for
    /// close cleanup, while routing visibility is promoted only after enqueue.
    ///
    /// Generic over `MessageSender` (channel or `ConnectionHandleSender`) for
    /// the single-writer-loop architecture (ls-bridge-message-ordering).
    pub(crate) async fn ensure_document_opened<S: message_sender::MessageSender>(
        &self,
        sender: &mut S,
        host_uri: &Url,
        virtual_uri: &VirtualDocumentUri,
        virtual_content: &str,
        connection_key: &ConnectionKey,
    ) -> io::Result<()> {
        let transition = self.open_transition_lock(virtual_uri, connection_key);
        let transition_guard = transition.lock().await;
        let claim = if self
            .document_tracker
            .try_claim_for_open(virtual_uri, connection_key)
            .await
        {
            self.document_tracker
                .open_claim_waiter(virtual_uri, connection_key)
                .expect("successful claim must have an identity")
        } else {
            if self
                .document_tracker
                .is_document_opened_on_connection(virtual_uri, connection_key)
            {
                return Ok(());
            }
            // We own the transition lock, so a remaining pre-send claim has no
            // live owner (its task dropped before rollback ran). Finish that
            // rollback synchronously, then take a fresh claim.
            self.document_tracker
                .rollback_open_claim(host_uri, virtual_uri, connection_key)
                .await;
            if !self
                .document_tracker
                .try_claim_for_open(virtual_uri, connection_key)
                .await
            {
                drop(transition_guard);
                self.remove_open_transition_lock_if_unshared(
                    virtual_uri,
                    connection_key,
                    &transition,
                );
                return Err(io::Error::other(
                    "bridge: virtual document claim did not settle",
                ));
            }
            self.document_tracker
                .open_claim_waiter(virtual_uri, connection_key)
                .expect("successful reclaim must have an identity")
        };
        let mut claim_guard = OpenClaimGuard {
            tracker: Arc::clone(&self.document_tracker),
            claim: Arc::clone(&claim),
            transition: Arc::clone(&transition),
            transition_locks: Arc::clone(&self.open_transition_locks),
            host_uri: host_uri.clone(),
            virtual_uri: virtual_uri.clone(),
            connection_key: connection_key.clone(),
            armed: true,
        };
        // Register host_to_virtual BEFORE send so that close_host_document
        // can find this document even if the task is aborted after send.
        // The single-writer loop (ls-bridge-message-ordering) guarantees FIFO ordering, so
        // any subsequent didClose queued by close_host_document will arrive
        // after didOpen on the wire.
        self.document_tracker
            .register_pending_document(host_uri, virtual_uri, connection_key)
            .await;
        // Read as close to enqueue as possible, after pending registration's
        // awaits. If an edit publishes after this read, it observes the open
        // claim and serializes a didChange behind this didOpen transition.
        let current_content = self.latest_virtual_contents.get(host_uri).and_then(|host| {
            host.contents
                .get(virtual_uri.language())
                .and_then(|regions| {
                    regions
                        .get(virtual_uri.region_id())
                        .map(|entry| Arc::clone(entry.value()))
                })
        });
        let virtual_content = current_content.as_deref().unwrap_or(virtual_content);
        let did_open = build_didopen_notification(virtual_uri, virtual_content);
        if let Err(e) = sender.send_notification(did_open).await {
            self.document_tracker
                .rollback_open_claim_if(host_uri, virtual_uri, connection_key, &claim)
                .await;
            claim_guard.disarm();
            drop(claim_guard);
            drop(transition_guard);
            self.remove_open_transition_lock_if_unshared(virtual_uri, connection_key, &transition);
            return Err(e);
        }
        if !self
            .document_tracker
            .mark_open_sent(virtual_uri, connection_key, &claim)
        {
            claim_guard.disarm();
            drop(claim_guard);
            drop(transition_guard);
            self.remove_open_transition_lock_if_unshared(virtual_uri, connection_key, &transition);
            return Err(io::Error::other(
                "bridge: didOpen claim invalidated during enqueue",
            ));
        }
        claim_guard.disarm();
        // The didOpen was confirmed enqueued (the `MessageSender` maps `Queued` →
        // `Ok`), so seed the content fingerprint with the opened content. Without
        // this, the FIRST position-only host edit — which leaves this region's
        // content unchanged — would still re-send a didChange (and, for a server that
        // clears on didChange, flicker the diagnostics) because no fingerprint existed
        // to compare against (#422).
        self.document_tracker
            .record_sent_content_fingerprint(virtual_uri, connection_key, virtual_content)
            .await;
        Ok(())
    }

    pub(crate) fn record_latest_virtual_content(
        &self,
        host_uri: &Url,
        incarnation: u64,
        language: &str,
        region_id: &str,
        content: &str,
    ) {
        let Some(host) = self.latest_virtual_contents.get(host_uri) else {
            return;
        };
        if host.incarnation != incarnation {
            return;
        }
        if let Some(regions) = host.contents.get(language) {
            if regions
                .get(region_id)
                .is_some_and(|cached| cached.as_ref() == content)
            {
                return;
            }
            regions.insert(region_id.to_string(), Arc::<str>::from(content));
            return;
        }
        host.contents
            .entry(language.to_string())
            .or_default()
            .insert(region_id.to_string(), Arc::<str>::from(content));
    }

    pub(crate) fn host_lifecycle_lock(&self, host_uri: &Url) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.host_lifecycle_locks
                .entry(host_uri.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .value(),
        )
    }

    pub(crate) fn current_host_incarnation(&self, host_uri: &Url) -> Option<u64> {
        self.latest_virtual_contents
            .get(host_uri)
            .map(|host| host.incarnation)
    }

    pub(crate) fn existing_host_lifecycle_lock(
        &self,
        host_uri: &Url,
    ) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.host_lifecycle_locks
            .get(host_uri)
            .map(|entry| Arc::clone(entry.value()))
    }

    pub(crate) fn remove_host_lifecycle_lock_if_unshared(
        &self,
        host_uri: &Url,
        lifecycle: &Arc<tokio::sync::Mutex<()>>,
    ) {
        self.host_lifecycle_locks.remove_if(host_uri, |_, current| {
            Arc::ptr_eq(current, lifecycle)
                && Arc::strong_count(current) == 2
                && !self.latest_virtual_contents.contains_key(host_uri)
        });
    }

    pub(crate) async fn open_host_incarnation(&self, host_uri: &Url, incarnation: u64) {
        let lifecycle = self.host_lifecycle_lock(host_uri);
        let _guard = lifecycle.lock().await;
        self.latest_virtual_contents.insert(
            host_uri.clone(),
            HostVirtualContents {
                incarnation,
                contents: DashMap::new(),
            },
        );
    }

    pub(crate) async fn close_host_incarnation(&self, host_uri: &Url, incarnation: u64) {
        let lifecycle = self.host_lifecycle_lock(host_uri);
        let guard = lifecycle.lock().await;
        // A delayed close from an older lifetime must not clear a fast reopen's
        // cache container.
        self.latest_virtual_contents
            .remove_if(host_uri, |_, host| host.incarnation == incarnation);
        let host_closed = !self.latest_virtual_contents.contains_key(host_uri);
        drop(guard);
        if host_closed {
            self.remove_host_lifecycle_lock_if_unshared(host_uri, &lifecycle);
        }
    }

    pub(crate) fn clear_invalidated_virtual_contents(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) {
        let invalidated: std::collections::HashSet<String> =
            invalidated_ulids.iter().map(ToString::to_string).collect();
        if let Some(host) = self.latest_virtual_contents.get(host_uri) {
            host.contents.retain(|_, regions| {
                regions.retain(|region_id, _| !invalidated.contains(region_id));
                !regions.is_empty()
            });
        }
    }

    pub(crate) fn clear_replaced_virtual_contents(
        &self,
        host_uri: &Url,
        replaced: &[OpenedVirtualDoc],
    ) {
        if let Some(host) = self.latest_virtual_contents.get(host_uri) {
            for doc in replaced {
                let language = doc.virtual_uri.language();
                let region_id = doc.virtual_uri.region_id();
                let empty = if let Some(regions) = host.contents.get(language) {
                    regions.remove(region_id);
                    regions.is_empty()
                } else {
                    false
                };
                if empty {
                    host.contents.remove(language);
                }
            }
        }
    }

    /// Increment the version of a virtual document and return the new version,
    /// or `None` if the document has not been opened.
    ///
    /// Test-only: production sends always go through
    /// [`Self::increment_version_if_content_changed`] (the content-guarded path,
    /// #422); this bare delegation is retained for the version-tracking unit tests.
    #[cfg(test)]
    pub(super) async fn increment_document_version(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
    ) -> Option<i32> {
        self.document_tracker
            .increment_document_version(virtual_uri, connection_key)
            .await
    }

    /// Increment the version only when `content` differs from the last `didChange`
    /// sent to this connection for this virtual document; `None` skips the re-send
    /// (content unchanged, or the doc isn't registered for this connection). The
    /// content guard keeps a position-only host edit from re-analyzing every region
    /// (#422). Delegates to [`DocumentTracker::increment_version_if_content_changed`].
    pub(super) async fn increment_version_if_content_changed(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
        content: &str,
    ) -> Option<i32> {
        self.document_tracker
            .increment_version_if_content_changed(virtual_uri, connection_key, content)
            .await
    }

    /// Record `content`'s fingerprint as sent to this connection — call only after a
    /// confirmed-enqueued didChange (#422). Delegates to
    /// [`DocumentTracker::record_sent_content_fingerprint`].
    pub(super) async fn record_sent_content_fingerprint(
        &self,
        virtual_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
        content: &str,
    ) {
        self.document_tracker
            .record_sent_content_fingerprint(virtual_uri, connection_key, content)
            .await
    }

    /// Get or create a connection for the specified server, spawning the server
    /// and running the LSP handshake with the default timeout on a miss.
    ///
    /// `document_uri` selects the connection: it resolves to a
    /// `(server_name, root)` key (root_markers module), so documents under
    /// different marker roots get their own downstream process while documents
    /// sharing a root (or the client-root fallback) share one (issue #382). On a
    /// spawn it also seeds that connection's `workspaceMarkers` workspace root for the
    /// LSP handshake.
    pub(super) async fn get_or_create_connection(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: Option<&Url>,
    ) -> io::Result<Arc<ConnectionHandle>> {
        self.get_or_create_connection_with_timeout(
            server_name,
            server_config,
            document_uri,
            Duration::from_secs(INIT_TIMEOUT_SECS),
        )
        .await
    }

    /// Test helper: eagerly spawn a server and wait for connection.
    ///
    /// Wraps `get_or_create_connection_with_timeout` with fire-and-forget
    /// semantics. Production code uses `get_or_create_connection_wait_ready`
    /// directly via `eager_open_virtual_documents` instead.
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
                None,
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await;
    }

    /// Get or create a connection, waiting (up to `timeout`) for it to reach Ready
    /// instead of failing fast like `get_or_create_connection()`. Used by diagnostic
    /// requests, where waiting through initialization beats returning empty results.
    pub(crate) async fn get_or_create_connection_wait_ready(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: Option<&Url>,
        timeout: Duration,
    ) -> io::Result<Arc<ConnectionHandle>> {
        // `timeout` is the caller's overall budget; the incapable-shared divert
        // below acquires a second connection, so track elapsed time and hand it
        // only the remaining budget rather than a fresh full `timeout`.
        let start = std::time::Instant::now();
        // Resolve the marker workspace + key ONCE (one filesystem walk, plus the
        // shared-instance capability probe).
        let (marker, connection_key) = self
            .resolve_acquire(server_name, server_config, document_uri)
            .await;

        // Acquire and wait through initialization for the resolved key.
        let handle = self
            .acquire_resolved_wait_ready(
                server_name,
                server_config,
                connection_key,
                marker.clone(),
                timeout,
            )
            .await?;

        // The shared connection's capability is only known now that it is Ready.
        // If it came up incapable and does not already serve this root, the
        // shared-instance opt-in must degrade to a per-root instance (#391):
        // opening this document on a server that ignores
        // didChangeWorkspaceFolders would wedge the 2nd+ root. `resolve_acquire`
        // now sees the shared connection as Ready+incapable, so it re-resolves
        // to the per-root key; acquire that connection instead, within the
        // caller's remaining budget.
        if handle.key().is_shared() && !handle.supports_workspace_folder_changes() {
            let serves_this_root = marker
                .as_ref()
                .is_some_and(|(_root, folder)| handle.workspace_folders().contains(folder));
            if !serves_this_root {
                handle.log_incapable_fallback_once(server_name);
                let remaining = timeout.saturating_sub(start.elapsed());
                // Reuse the already-resolved `marker` to build the per-root key
                // directly — re-running `resolve_acquire` here would redo the
                // filesystem marker walk and re-lock `connections` for the same
                // result (the shared connection is now Ready+incapable).
                let per_root_key = ConnectionKey::new(
                    server_name,
                    marker
                        .as_ref()
                        .map(|(root, _folder)| root.as_str().to_owned()),
                );
                return self
                    .acquire_resolved_wait_ready(
                        server_name,
                        server_config,
                        per_root_key,
                        marker,
                        remaining,
                    )
                    .await;
            }
        }

        // Announce the (possibly newly-joined) root before the caller opens any
        // document. Idempotent if `acquire_resolved_wait_ready`'s ReturnExisting
        // path already announced; on the initializing-retry path it is the only
        // announce. Propagates a queue-full failure so the caller retries rather
        // than open a document for an unannounced root.
        self.announce_shared_root(&handle, &marker).await?;
        Ok(handle)
    }

    /// Acquire the connection for an already-resolved `(connection_key, marker)`
    /// and wait (up to `timeout`) for it to reach Ready, transparently waiting
    /// through a concurrent spawn that returns `Initializing`. Does NOT apply
    /// shared-instance routing or announce — callers layer that on top.
    async fn acquire_resolved_wait_ready(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        connection_key: ConnectionKey,
        marker: Option<(Url, tower_lsp_server::ls_types::WorkspaceFolder)>,
        timeout: Duration,
    ) -> io::Result<Arc<ConnectionHandle>> {
        match self
            .get_or_create_connection_resolved(
                server_name,
                server_config,
                connection_key.clone(),
                marker,
                // Bound the spawn handshake by the caller's budget, not a fresh
                // full INIT_TIMEOUT_SECS — so a cold spawn (including the
                // incapable-shared divert, which passes its REMAINING budget)
                // stays within `timeout` overall.
                timeout,
            )
            .await
        {
            Ok(handle) => {
                handle.wait_for_ready(timeout).await?;
                Ok(handle)
            }
            Err(e) => {
                let is_initializing = e
                    .get_ref()
                    .and_then(|inner| inner.downcast_ref::<BridgeError>())
                    .is_some_and(BridgeError::is_initializing);
                if !is_initializing {
                    return Err(e);
                }
                // A concurrent caller is spawning this exact key; wait on the
                // connection it routes to — no second filesystem walk here.
                let handle = {
                    let connections = self.connections.lock().await;
                    connections
                        .get(&connection_key)
                        .map(Arc::clone)
                        .ok_or_else(|| {
                            io::Error::other("bridge: connection disappeared during wait")
                        })?
                };
                handle.wait_for_ready(timeout).await?;
                Ok(handle)
            }
        }
    }
}

impl LanguageServerPool {
    /// Resolve the marker workspace AND the pool key `(server_name, root)` for a
    /// request on `document_uri` in a SINGLE filesystem walk — the one point of
    /// root resolution. The spawn handshake (`marker`) and every map lookup
    /// (`key`) derive from the same value, so a document always routes to the
    /// connection it is spawned at, even if the marker filesystem changes between
    /// calls (#382). Returning both lets a caller resolve once and reuse the key
    /// (e.g. `wait_ready`'s initializing-retry lookup) instead of walking twice.
    ///
    /// The root is the marker-derived workspace root when one is found —
    /// documents under different marker roots get separate downstream processes
    /// (issue #382) — or `None`, the client-root fallback shared by every
    /// document without a marker root.
    fn resolve_marker_and_key(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: Option<&Url>,
    ) -> (
        Option<(Url, tower_lsp_server::ls_types::WorkspaceFolder)>,
        ConnectionKey,
    ) {
        let marker = super::root_markers::resolve_marker_workspace(
            server_config.workspace_markers.as_deref(),
            document_uri,
        );
        let key = ConnectionKey::new(
            server_name,
            // `as_str().to_owned()` is one allocation; `String::from(root.clone())`
            // would clone the parsed `Url` first. The string is identical.
            marker
                .as_ref()
                .map(|(root, _folder)| root.as_str().to_owned()),
        );
        (marker, key)
    }

    /// The per-root pool key alone (sync, pool-independent), for callers that
    /// don't spawn. Does NOT apply shared-instance routing (#391) — use
    /// [`resolve_acquire`](Self::resolve_acquire) for that. Test-only: the
    /// production key always flows through `resolve_acquire`.
    #[cfg(test)]
    fn connection_key(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: Option<&Url>,
    ) -> ConnectionKey {
        self.resolve_marker_and_key(server_name, server_config, document_uri)
            .1
    }

    /// Resolve the `(marker, key)` a request on `document_uri` routes to,
    /// applying shared-instance routing (#391) on top of the per-root key.
    ///
    /// For a server without `preferSharedInstance`, this is exactly
    /// [`resolve_marker_and_key`](Self::resolve_marker_and_key) (per-root/#382).
    /// For an opt-in server it returns the shared-instance key — UNLESS a shared
    /// connection already exists, is `Ready`, and did NOT advertise the
    /// `workspaceFolders` capability. In that case kakehashi logs once and
    /// degrades to per-root instances, so a misconfigured opt-in never wedges
    /// the 2nd+ root on a server that ignores `didChangeWorkspaceFolders`. The
    /// fallback keeps the shared connection's OWN spawn root on the shared key
    /// (that connection is correctly rooted there and already serves it) and
    /// diverts only *other* roots to per-root keys — yielding true per-root
    /// isolation rather than splitting one root across two processes.
    ///
    /// A still-initializing shared connection is treated optimistically as
    /// shared, because its capability is not known yet. The two acquire paths
    /// then resolve the race once it is `Ready`: the fast-fail path returns the
    /// `Initializing` error before any `didOpen` and re-resolves on retry, and
    /// the wait path
    /// ([`get_or_create_connection_wait_ready`](Self::get_or_create_connection_wait_ready))
    /// re-checks the capability after waiting and diverts a new root to a
    /// per-root connection if the shared one came up incapable — so no document
    /// is ever opened on an incapable shared connection for a root it does not
    /// already serve.
    ///
    /// Briefly locks `connections` for the capability probe; the marker is still
    /// resolved with a single filesystem walk.
    async fn resolve_acquire(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: Option<&Url>,
    ) -> (
        Option<(Url, tower_lsp_server::ls_types::WorkspaceFolder)>,
        ConnectionKey,
    ) {
        let (marker, per_root_key) =
            self.resolve_marker_and_key(server_name, server_config, document_uri);
        if !server_config.prefers_shared_instance() {
            return (marker, per_root_key);
        }
        // A marker-less document (no marker root, non-file URI, no document
        // hint, or the `[]` kill switch) has no root to share and nothing to
        // announce, so it stays on the client-root fallback (`per_root_key`,
        // here a `ClientFallback` key). The shared key is reserved for
        // marker-rooted documents that join via `didChangeWorkspaceFolders`,
        // keeping `ConnectionRoot::Shared` distinct from the fallback.
        if marker.is_none() {
            return (marker, per_root_key);
        }

        let shared_key = ConnectionKey::shared(server_name);
        // Clone the shared handle out under the lock, then probe its capability
        // and folder set without nesting the folder-set lock under `connections`.
        let shared_handle = {
            let connections = self.connections.lock().await;
            connections.get(&shared_key).map(Arc::clone)
        };

        let key = match shared_handle {
            // A Ready shared connection that never advertised the capability
            // can't take on new roots via didChangeWorkspaceFolders.
            Some(handle)
                if handle.state() == ConnectionState::Ready
                    && !handle.supports_workspace_folder_changes() =>
            {
                handle.log_incapable_fallback_once(server_name);
                // Keep the shared connection's own spawn root on the shared key
                // (it is already rooted and serving it); divert every other root
                // to its own per-root process for true per-root isolation.
                let serves_this_root = marker
                    .as_ref()
                    .is_some_and(|(_root, folder)| handle.workspace_folders().contains(folder));
                if serves_this_root {
                    shared_key
                } else {
                    per_root_key
                }
            }
            // No shared connection yet, still initializing, or Ready+capable:
            // route to the shared instance.
            _ => shared_key,
        };

        (marker, key)
    }

    /// For a shared-instance connection (#391), record this acquisition's marker
    /// root in the connection's folder set and, when it is newly added and the
    /// downstream server advertised the `workspaceFolders` capability, announce
    /// it with `workspace/didChangeWorkspaceFolders`. The notification is queued
    /// through the single-writer loop, so a `didOpen` the caller sends next on
    /// the same connection follows it on the wire (FIFO), satisfying "announce
    /// the root, then open the document".
    ///
    /// Returns `Ok(())` when the root is announced (or no announce is needed).
    /// Returns an error only when the root is newly required but its
    /// `didChangeWorkspaceFolders` could not be queued (outbound queue full or
    /// closed): the caller must NOT then open documents on this connection,
    /// because they would reach the server without the preceding announcement.
    /// Surfacing it as an error makes the acquire fail and retry, re-attempting
    /// the announce once the queue drains.
    ///
    /// `Ok(())` no-op for per-root keys, marker-less acquisitions,
    /// capability-less servers, or a root already in the set — including a
    /// connection's own initialize-time root, so the first root never
    /// re-announces.
    async fn announce_shared_root(
        &self,
        handle: &Arc<ConnectionHandle>,
        marker: &Option<(Url, tower_lsp_server::ls_types::WorkspaceFolder)>,
    ) -> io::Result<()> {
        if !handle.key().is_shared() {
            return Ok(());
        }
        let Some((_root, folder)) = marker else {
            return Ok(());
        };
        if !handle.supports_workspace_folder_changes() {
            return Ok(());
        }
        let folder = folder.clone();
        // The closure runs under the folder-set lock and reports its send
        // outcome out here so the error kind can distinguish retryable
        // backpressure (queue full) from a dead/serialization-failed connection.
        let mut send_outcome = NotificationSendResult::Queued;
        // Hold the `connections` lock across the liveness check AND the
        // add+announce so a concurrent respawn (get_or_create's SpawnNew branch,
        // which purges this key's state) cannot interleave between verifying the
        // handle is still the live pool connection and queuing the
        // `didChangeWorkspaceFolders` — the same Arc::ptr_eq live-handle guard
        // the request (execute.rs) and eager-open (did_open.rs) send paths use.
        // `add_and_announce` is fully synchronous (the send is a non-blocking
        // try_send), so no `.await` is held under either lock; lock order is
        // connections → folder set, never the reverse.
        let connections = self.connections.lock().await;
        if !connections
            .get(handle.key())
            .is_some_and(|live| Arc::ptr_eq(live, handle))
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bridge: connection was replaced before didChangeWorkspaceFolders",
            ));
        }
        // Add + announce atomically under the folder-set lock: the folder is
        // committed only if the `didChangeWorkspaceFolders` actually queued, and
        // a concurrent caller cannot observe it as announced (and skip its own
        // announce) until that notification is on the FIFO ahead of any later
        // `didOpen`. If the queue is full/closed, nothing commits and the error
        // below makes this acquisition retry rather than open a document for an
        // unannounced root.
        let announced = handle
            .workspace_folders()
            .add_and_announce(folder.clone(), || {
                let result =
                    handle.send_notification(build_did_change_workspace_folders_notification(
                        vec![folder.clone()],
                        Vec::new(),
                    ));
                send_outcome = result;
                if result == NotificationSendResult::Queued {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "[{}] announced workspace folder {}",
                        handle.key(),
                        folder.uri.as_str()
                    );
                    true
                } else {
                    log::warn!(
                        target: "kakehashi::bridge",
                        "[{}] didChangeWorkspaceFolders for {} not queued ({:?})",
                        handle.key(),
                        folder.uri.as_str(),
                        result
                    );
                    false
                }
            });
        drop(connections);

        if announced {
            return Ok(());
        }
        // Map the send failure to a faithful error kind so callers recover
        // correctly: queue-full is retryable backpressure, but a closed channel
        // / serialization failure is terminal (the next acquire respawns rather
        // than retrying against a dead connection). Mirrors the bridge's other
        // notification error mapping (text_document/did_close).
        let kind = match send_outcome {
            NotificationSendResult::QueueFull => io::ErrorKind::WouldBlock,
            NotificationSendResult::ChannelClosed => io::ErrorKind::BrokenPipe,
            NotificationSendResult::SerializationFailed => io::ErrorKind::InvalidData,
            // Not reachable: `announced` is false only when the send did not queue.
            NotificationSendResult::Queued => io::ErrorKind::Other,
        };
        Err(io::Error::new(
            kind,
            format!(
                "bridge: could not queue didChangeWorkspaceFolders for {} ({:?})",
                folder.uri.as_str(),
                send_outcome
            ),
        ))
    }

    /// Fast-fail get-or-spawn (ls-bridge-message-ordering): resolves the
    /// connection's `(marker, key)` from `document_uri`, then delegates to
    /// [`get_or_create_connection_resolved`](Self::get_or_create_connection_resolved).
    async fn get_or_create_connection_with_timeout(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        document_uri: Option<&Url>,
        timeout: Duration,
    ) -> io::Result<Arc<ConnectionHandle>> {
        let (marker, connection_key) = self
            .resolve_acquire(server_name, server_config, document_uri)
            .await;
        self.get_or_create_connection_resolved(
            server_name,
            server_config,
            connection_key,
            marker,
            timeout,
        )
        .await
    }

    /// Get-or-spawn against an ALREADY-resolved `(connection_key, marker)` pair
    /// (resolve via [`resolve_marker_and_key`](Self::resolve_marker_and_key)), so
    /// a caller can resolve the marker workspace once and reuse the key. On miss:
    /// start the process, split reader+writer, store the handle as
    /// `Initializing`, and run the LSP initialize handshake in a background task
    /// that transitions Ready or Failed. Requests against Initializing fail with
    /// REQUEST_FAILED; Failed causes removal and respawn on the next call.
    async fn get_or_create_connection_resolved(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        connection_key: ConnectionKey,
        marker: Option<(Url, tower_lsp_server::ls_types::WorkspaceFolder)>,
        timeout: Duration,
    ) -> io::Result<Arc<ConnectionHandle>> {
        let mut connections = self.connections.lock().await;

        // Reject new spawns once shutdown has begun. Checked here, INSIDE the
        // `connections` lock that `shutdown_all` also takes to snapshot: if shutdown
        // set the flag first, this rejects (no escaping connection); if this inserts
        // first, shutdown's later snapshot sees the connection and tears it down.
        // Relaxed is sufficient — the lock provides the happens-before. Callers
        // (eager open / lazy request) already degrade gracefully on a connection
        // error, so an `Interrupted` here is a no-op at shutdown time.
        if self.shutting_down.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "bridge pool is shutting down; rejecting new connection spawn",
            ));
        }

        // Get consecutive panic count for this connection
        let panic_count = {
            let counts = self
                .consecutive_panic_counts
                .lock()
                .recover_poison("LanguageServerPool::get_or_create_connection_with_timeout");
            counts.get(&connection_key).copied().unwrap_or(0)
        };

        // Check if we already have a connection for this key in ONE lookup,
        // reused by the ReturnExisting arm below.
        // Use pure decision function for testability (ls-bridge-message-ordering Operation Gating)
        let existing = connections.get(&connection_key);
        let launch_config_changed = existing.is_some_and(|handle| {
            handle
                .launch_config()
                .is_some_and(|old| !same_launch_config(old, server_config))
        });
        let existing_state = if launch_config_changed {
            // Reuse the stale/closed cleanup path below. `Failed` maps to
            // SpawnNew, while the explicit flag also schedules process shutdown.
            Some(ConnectionState::Failed)
        } else {
            existing.map(|h| h.state())
        };
        match decide_connection_action(existing_state, panic_count) {
            ConnectionAction::ReturnExisting => {
                // `decide_connection_action` only returns ReturnExisting when a
                // connection exists for this key, so the lookup is an invariant;
                // surface it as an error rather than panicking in production
                // (no-panic convention) if that ever fails to hold.
                let handle = existing.cloned().ok_or_else(|| {
                    io::Error::other("bridge: connection disappeared before ReturnExisting")
                })?;
                // Release the pool lock before announcing: `announce_shared_root`
                // re-locks `connections` itself for its Arc::ptr_eq liveness
                // check, so it must not be held here (the tokio mutex is not
                // reentrant).
                drop(connections);
                self.announce_shared_root(&handle, &marker).await?;
                return Ok(handle);
            }
            ConnectionAction::FailFast(err) => {
                // Log once when server is disabled due to repeated panics
                if matches!(err, BridgeError::Disabled) {
                    log::error!(
                        target: "kakehashi::bridge::connection",
                        "[{}] Server disabled after {} consecutive handshake panics (max: {})",
                        connection_key,
                        panic_count,
                        connection_action::MAX_CONSECUTIVE_PANICS
                    );
                }
                return Err(err.into());
            }
            ConnectionAction::SpawnNew => {
                // Remove stale connection if present (Failed or Closed state)
                if existing_state.is_some() {
                    let invalidated_handle =
                        launch_config_changed.then(|| existing.cloned()).flatten();
                    if let Some(handle) = &invalidated_handle {
                        handle.begin_shutdown();
                    }
                    // Drop the dead connection's document state with it: the
                    // replacement process has nothing open, so the lazy host
                    // sync must re-send didOpen and the virt tracker must let
                    // the next request re-claim and re-open instead of trusting
                    // stale entries (host-document-bridge). Both purges are
                    // scoped to this exact `(server, root)` key so sibling roots
                    // sharing the server name keep their state. Lock order:
                    // connections → host_documents / document tracker,
                    // consistent with close_host_bridge_document's prefetch.
                    self.host_documents
                        .lock()
                        .await
                        .retain(|(_, key), _| key != &connection_key);
                    self.document_tracker
                        .purge_connection(&connection_key)
                        .await;
                    self.purge_open_transition_locks(&connection_key).await;
                    // Remove only after every async purge completes. If this
                    // acquire future is aborted at a cleanup await, the stale
                    // entry remains and the next acquire retries the idempotent
                    // purge instead of spawning over partial document state.
                    connections.remove(&connection_key);
                    if let Some(handle) = invalidated_handle {
                        shutdown_invalidated_connection(connection_key.clone(), handle);
                    }
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

        // workspaceMarkers workspace-root detection (root_markers module): the same
        // marker workspace resolved for the pool key above (entry-priority
        // order) also seeds the handshake — a marker hit overrides the
        // client-supplied root and folders; otherwise both fall back. Reusing
        // `marker` (rather than re-resolving) guarantees the spawned root
        // matches `connection_key`.
        // Resolved before the reader task spawns because the reader answers
        // downstream `workspace/workspaceFolders` queries with these folders.
        let (root_uri, init_folders) = super::root_markers::workspace_from_marker(marker, || {
            (self.root_uri(), self.workspace_folders())
        });
        log::debug!(
            target: "kakehashi::bridge::init",
            "[{}] workspace root for spawn: {:?}",
            server_name,
            root_uri
        );
        // Per-connection mutable folder set (workspace-folders module): seeded
        // with the spawn-time folders and shared with the reader so it can
        // answer downstream `workspace/workspaceFolders` pulls. Shared-instance
        // servers (#391) grow it as new roots join; the immutable spawn-time
        // `init_folders` still seeds the `initialize` handshake.
        let workspace_folders = super::WorkspaceFolderSet::new(init_folders.clone());

        // Now spawn reader task with liveness timeout - it can route the initialize response immediately
        // Liveness timeout is configured via LivenessTimeout::default() (60s per ls-bridge-timeout-hierarchy Tier 2)
        // Server name is passed for structured logging and notification prefixes
        // Per-connection settings cell, shared between the reader (answers
        // workspace/configuration pulls, path b) and the handle (re-stored on a
        // merge change, path c). Seeded from the resolved config
        // (downstream-settings-propagation).
        let settings_cell = Arc::new(arc_swap::ArcSwapOption::from(
            server_config.settings.clone().map(Arc::new),
        ));

        let liveness_timeout = liveness_timeout::LivenessTimeout::default();
        let reader_handle = spawn_reader_task_for_server(
            reader,
            Arc::clone(&router),
            Some(liveness_timeout.as_duration()),
            ServerRequestDeps {
                server_name: Some(server_name.to_string()),
                // The applyEdit version validation scopes downstream-supplied
                // versions to this connection's version space (PR-L).
                connection_key: connection_key.clone(),
                response_tx: tx.clone(),
                dynamic_capabilities: Arc::clone(&dynamic_capabilities),
                upstream_tx: self.upstream_tx.clone(),
                window_tx: self.window_tx.clone(),
                upstream_request_tx: self.upstream_request_tx.clone(),
                inbound_request_registry: self.inbound_request_registry.clone(),
                // #391: share the per-connection mutable folder set with the
                // reader (it answers workspace/workspaceFolders pulls).
                workspace_folders: workspace_folders.clone(),
                progress_registry: Arc::clone(&self.progress_registry),
                client_progress_registry: Arc::clone(&self.client_progress_registry),
                progress_connection_id: self.progress_registry.new_connection_id(),
                settings: Arc::clone(&settings_cell),
            },
        );

        // Create handle in Initializing state (fast-fail for concurrent requests).
        // The handle shares the reader's `workspace_folders` set so the
        // shared-instance path (#391) can grow it and announce new roots.
        let handle = Arc::new(ConnectionHandle::with_state(
            writer,
            router,
            reader_handle,
            ConnectionState::Initializing,
            tx,
            rx,
            dynamic_capabilities,
            connection_key.clone(),
            workspace_folders,
            settings_cell,
        ));
        handle.record_launch_config(server_config);

        // Insert into pool immediately so concurrent requests see Initializing state
        connections.insert(connection_key.clone(), Arc::clone(&handle));

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
        let client_capabilities = self.client_capabilities();
        // Only advertise `workspace.configuration` when this server actually has
        // settings to serve. Advertising it otherwise would flip an
        // `initializationOptions`-configured server to pull and get every
        // section answered `null` (downstream-settings-propagation).
        let advertise_configuration = server_config.settings.is_some();
        let handle_for_handshake = Arc::clone(&handle);
        let server_name_for_log = server_name.to_string();
        let command_origins = Arc::clone(&self.command_origins);
        let command_registration_key = connection_key.clone();
        let upstream_request_tx = self.upstream_request_tx.clone();
        // The editor accepts a dynamic `workspace/executeCommand` registration
        // only if it advertised `dynamicRegistration` (LSP spec). Compute once.
        let supports_dynamic_command_registration = self
            .client_capabilities()
            .and_then(|caps| caps.workspace)
            .and_then(|ws| ws.execute_command)
            .and_then(|ec| ec.dynamic_registration)
            .unwrap_or(false);
        let handshake_task = tokio::spawn(async move {
            let init_result = tokio::time::timeout(
                timeout,
                perform_lsp_handshake(
                    &handle_for_handshake,
                    init_request_id,
                    init_response_rx,
                    init_options,
                    root_uri,
                    init_folders,
                    client_capabilities,
                    advertise_configuration,
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
                    // Extract this server's advertised palette command names
                    // before `capabilities` is moved; they're registered AFTER
                    // the Ready flip below.
                    let palette_commands = capabilities
                        .execute_command_provider
                        .as_ref()
                        .map(|options| options.commands.clone());
                    handle_for_handshake.set_server_capabilities(capabilities);
                    // Path a: push this server's settings now that `initialized`
                    // has been sent, so push-model servers are configured even
                    // before they pull (downstream-settings-propagation). Queue it
                    // *before* flipping to Ready: a waiter unblocked by the Ready
                    // transition may enqueue `didOpen`, and the single-writer FIFO
                    // must carry the settings ahead of it so the server never
                    // processes a document under default config. Read the *live*
                    // cell, not the spawn-time value, so a path-c re-propagation
                    // that landed during the handshake is reflected and the push
                    // agrees with what (b) would answer. Best-effort: a dropped
                    // notification self-heals on the next pull.
                    //
                    // Push when we advertised `configuration` (so a clear to
                    // `None` during the handshake still reaches the server as
                    // `null` instead of leaving it on stale settings) OR when the
                    // cell now holds settings (added during the handshake). When
                    // neither holds there is nothing to communicate.
                    let current = handle_for_handshake.current_settings();
                    if advertise_configuration || current.is_some() {
                        let payload = current
                            .map(|s| (*s).clone())
                            .unwrap_or(serde_json::Value::Null);
                        handle_for_handshake.send_notification(
                            build_did_change_configuration_notification(payload),
                        );
                    }
                    if !handle_for_handshake.transition_initializing_to_ready() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "bridge: connection was invalidated during initialization",
                        ));
                    }
                    // Record advertised palette command names AFTER Ready, so a
                    // command fired without an action context (raw name) routes
                    // back to a Ready connection (#628). Dedup is by name across
                    // the whole session, so a respawn / second root re-registers
                    // nothing new.
                    if let Some(commands) = palette_commands {
                        let added = command_origins.register(&command_registration_key, commands);
                        // Advertise the NEWLY-added names upstream so the editor's
                        // palette lists them. Fire-and-forget; skipped when the
                        // client can't accept a dynamic registration.
                        if !added.is_empty()
                            && supports_dynamic_command_registration
                            && let Err(e) = upstream_request_tx
                                .send(UpstreamRequest::RegisterCommands { commands: added })
                        {
                            log::warn!(
                                target: "kakehashi::bridge",
                                "Failed to queue palette-command registration \
                                 (forwarding loop gone): {e}"
                            );
                        }
                    }
                    Ok(())
                }
                Ok(Err(e)) => {
                    if matches!(
                        handle_for_handshake.state(),
                        ConnectionState::Closing | ConnectionState::Closed
                    ) {
                        log::debug!(
                            target: "kakehashi::bridge::init",
                            "[{}] LSP handshake interrupted by connection invalidation: {}",
                            server_name_for_log,
                            e
                        );
                        return Err(e);
                    }
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
                // Success - reset panic count for this connection
                let mut counts = self
                    .consecutive_panic_counts
                    .lock()
                    .recover_poison("LanguageServerPool::get_or_create_connection_with_timeout");
                counts.remove(&connection_key);
                Ok(handle)
            }
            Ok(Err(e)) => Err(e),
            Err(join_err) => {
                handle.set_state(ConnectionState::Failed);

                // Distinguish panic from cancellation - only panics should trip circuit breaker
                if join_err.is_panic() {
                    // Task panicked - track consecutive panic count to avoid infinite retry
                    let new_count = {
                        let mut counts = self.consecutive_panic_counts.lock().recover_poison(
                            "LanguageServerPool::get_or_create_connection_with_timeout",
                        );
                        let count = counts.entry(connection_key.clone()).or_insert(0);
                        *count += 1;
                        *count
                    };
                    log::error!(
                        target: "kakehashi::bridge::init",
                        "[{}] Handshake task panicked (consecutive count: {}): {}",
                        connection_key,
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

    /// Send `$/cancelRequest` for a **specific** downstream request id on
    /// `server_name`'s connection — no upstream-id lookup involved.
    ///
    /// Used by the concatenated formatting pipeline's per-step timeout, which
    /// must cancel exactly the request it issued: routing through the
    /// upstream id would (a) find nothing — dropping the timed-out step
    /// future has already removed its cancel mapping via the router cleanup
    /// guard by the time any cancel task runs — and (b) fan out to sibling
    /// regions' in-flight requests sharing the same upstream id, spuriously
    /// cancelling unrelated work (PR #347 review).
    ///
    /// Best-effort like
    /// [`forward_cancel_by_upstream_id_with_notify`](Self::forward_cancel_by_upstream_id_with_notify):
    /// an absent or non-Ready connection silently drops the cancel; only real
    /// send failures bubble up as `Err`.
    pub(crate) async fn forward_cancel_downstream(
        &self,
        connection_key: &ConnectionKey,
        downstream_id: RequestId,
    ) -> io::Result<()> {
        let Some(handle) = self
            .ready_connection_for_cancel(
                connection_key,
                &format!("downstream_id: {}", downstream_id.as_i64()),
            )
            .await
        else {
            return Ok(());
        };
        self.send_cancel_notification(&handle, connection_key, downstream_id)
    }

    /// Fetch a connection for cancel forwarding, returning `None` (after
    /// best-effort metrics/logging) when there is no connection or it is not
    /// Ready. `request_desc` identifies the request in logs.
    async fn ready_connection_for_cancel(
        &self,
        connection_key: &ConnectionKey,
        request_desc: &str,
    ) -> Option<Arc<ConnectionHandle>> {
        let connections = self.connections().await;
        self.ready_handle_for_cancel_in(&connections, connection_key, request_desc)
    }

    /// Lock-free core of [`ready_connection_for_cancel`](Self::ready_connection_for_cancel):
    /// look up `connection_key` in an already-acquired connections map so callers
    /// fanning out over several connections can pay the lock acquisition once.
    fn ready_handle_for_cancel_in(
        &self,
        connections: &HashMap<ConnectionKey, Arc<ConnectionHandle>>,
        connection_key: &ConnectionKey,
        request_desc: &str,
    ) -> Option<Arc<ConnectionHandle>> {
        let Some(handle) = connections.get(connection_key) else {
            // No connection - request may have completed or server never started.
            // Silently drop per best-effort semantics.
            self.cancel_metrics.record_no_connection();
            log::debug!(
                target: "kakehashi::bridge::cancel",
                "Cancel dropped: no connection for '{}' ({}, expected if request completed)",
                connection_key,
                request_desc
            );
            return None;
        };

        // Only forward if connection is Ready
        if handle.state() != ConnectionState::Ready {
            // Connection still initializing or failed - can't forward cancels.
            // Silently drop per best-effort semantics.
            self.cancel_metrics.record_not_ready();
            log::debug!(
                target: "kakehashi::bridge::cancel",
                "Cancel dropped: connection not ready for '{}' ({}, state: {:?})",
                connection_key,
                request_desc,
                handle.state()
            );
            return None;
        }

        Some(Arc::clone(handle))
    }

    /// Build and send one `$/cancelRequest` notification for `downstream_id`
    /// via the single-writer loop (ls-bridge-message-ordering). The pending
    /// entry stays in place: the server may still respond with a result or
    /// `REQUEST_CANCELLED` (-32800).
    fn send_cancel_notification(
        &self,
        handle: &ConnectionHandle,
        connection_key: &ConnectionKey,
        downstream_id: RequestId,
    ) -> io::Result<()> {
        // Per LSP spec: $/cancelRequest is a notification with { id: request_id }
        let notification = JsonRpcNotification::new(
            "$/cancelRequest",
            CancelParams {
                // Safe: IDs are generated from an atomic counter starting at 0,
                // well within i32 range. ls-types constrains Number to i32.
                id: NumberOrString::Number(downstream_id.as_i64() as i32),
            },
        );

        match handle.send_notification(notification) {
            NotificationSendResult::Queued => {
                self.cancel_metrics.record_success();
                log::debug!(
                    target: "kakehashi::bridge::cancel",
                    "Cancel forwarded: downstream {} for '{}'",
                    downstream_id.as_i64(),
                    connection_key
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
            NotificationSendResult::SerializationFailed => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge: failed to serialize cancel notification",
            )),
        }
    }

    /// Fan out `$/cancelRequest` to every server registered for `upstream_id`,
    /// invoking `notify` after every `(connection, downstream ids)` target has
    /// been captured and before any cancel is sent. Invoked by
    /// `RequestIdCapture` (via `CancelForwarder::forward_cancel`) when the
    /// client cancels.
    ///
    /// LSP cancel is best-effort, so we silently return `Ok(())` (rather than
    /// erroring) when: the ID is unknown, the connection is still initializing,
    /// the downstream ID was already cleaned up, or an individual server send
    /// fails. This avoids racing cancel against handshake completion.
    ///
    /// Prepare-before-notify ordering is load-bearing: `notify` wakes the
    /// upstream handler (via `CancelForwarder::notify_cancel`), whose
    /// cancellation path immediately destroys the very state this lookup reads —
    /// `unregister_all_for_upstream_id` empties the registry, and dropping the
    /// in-flight request futures removes their router cancel mappings. Preparing
    /// first atomically marks queued work for writer-side skipping and captures
    /// only already-writing/sent IDs that still need a FIFO cancel notification;
    /// the later cleanup is then harmless.
    pub(crate) async fn forward_cancel_by_upstream_id_with_notify(
        &self,
        upstream_id: UpstreamId,
        notify: impl FnOnce(),
    ) -> io::Result<()> {
        // 1. Snapshot the connections registered for this upstream id.
        let connection_keys: Vec<ConnectionKey> = {
            let registry = self
                .upstream_request_registry
                .lock()
                .recover_poison("LanguageServerPool::forward_cancel_by_upstream_id");
            match registry.get(&upstream_id) {
                Some(connections) => connections.keys().cloned().collect(),
                None => {
                    // Request not registered yet (still initializing) or already completed.
                    // This is expected - silently drop the cancel per best-effort semantics.
                    self.cancel_metrics.record_not_in_registry();
                    log::debug!(
                        target: "kakehashi::bridge::cancel",
                        "Cancel dropped: upstream ID {} not in registry (expected during init or after completion)",
                        upstream_id
                    );
                    notify();
                    return Ok(());
                }
            }
        };

        // 2. Capture each server's connection handle and atomically classify its
        //    downstream ids under one connections-lock acquisition. Queued requests
        //    are marked for writer-side skipping before `notify`, and the time to
        //    wake the handler is bounded by one lock wait rather than one per server
        //    (PR #359 review).
        let mut targets: Vec<(ConnectionKey, Arc<ConnectionHandle>, Vec<RequestId>)> = Vec::new();
        {
            let connections = self.connections().await;
            for connection_key in connection_keys {
                let Some(handle) = self.ready_handle_for_cancel_in(
                    &connections,
                    &connection_key,
                    &format!("upstream_id: {upstream_id}"),
                ) else {
                    continue;
                };
                let (known, downstream_ids) =
                    handle.router().prepare_cancel_by_upstream(&upstream_id);
                if !known {
                    // Request already completed or ID never registered.
                    // Silently drop per best-effort semantics.
                    self.cancel_metrics.record_unknown_id();
                    log::debug!(
                        target: "kakehashi::bridge::cancel",
                        "Cancel dropped: upstream ID {} not found for '{}' (request may have completed)",
                        upstream_id,
                        connection_key
                    );
                    continue;
                }
                if !downstream_ids.is_empty() {
                    targets.push((connection_key, handle, downstream_ids));
                }
            }
            // Lock dropped here — notify and the sends below run without it.
        }

        // 3. Wake the upstream handler only now that targets are captured.
        notify();

        // 4. Send the cancels (best-effort, log I/O errors).
        for (connection_key, handle, downstream_ids) in targets {
            for downstream_id in downstream_ids {
                if let Err(e) =
                    self.send_cancel_notification(&handle, &connection_key, downstream_id)
                {
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
        }

        Ok(())
    }

    /// Forward a client `window/workDoneProgress/cancel` to the downstream that
    /// owns the (bridge-minted) `upstream_token`, rewriting it back to that
    /// downstream's original token. Best-effort, mirroring `$/cancelRequest`:
    /// an unknown token (already finished, or never ours) is silently dropped.
    pub(crate) async fn forward_work_done_cancel(&self, upstream_token: NumberOrString) {
        let Some((writer, downstream_token)) =
            self.progress_registry.resolve_cancel(&upstream_token)
        else {
            log::debug!(
                target: "kakehashi::bridge::progress",
                "workDoneProgress/cancel dropped: unknown upstream token {:?}",
                upstream_token
            );
            return;
        };

        // Build via the typed notification helper (like `$/cancelRequest` above)
        // for a consistent JSON-RPC shape, then serialize for the writer channel.
        let notification = JsonRpcNotification::new(
            "window/workDoneProgress/cancel",
            tower_lsp_server::ls_types::WorkDoneProgressCancelParams {
                token: downstream_token,
            },
        );
        let notification = serde_json::to_value(notification)
            .expect("WorkDoneProgressCancel notification serialization is infallible");

        // send_timeout (not try_send) to ride out transient backpressure, like
        // the reader's server-request responses. The downstream's writer drops
        // its receiver on teardown, so a dead connection just errors here.
        if let Err(e) = writer
            .send_timeout(
                OutboundMessage::Untracked(notification),
                std::time::Duration::from_secs(5),
            )
            .await
        {
            log::debug!(
                target: "kakehashi::bridge::progress",
                "Failed to forward workDoneProgress/cancel downstream: {}",
                e
            );
        }
    }

    /// Record `upstream_id → connection_key` so `$/cancelRequest` from the
    /// client can be forwarded to every downstream connection handling that ID.
    ///
    /// For fan-out (e.g. diagnostics dispatched to multiple injected languages),
    /// callers register the same `upstream_id` once per request. The
    /// per-connection count handles whole-document fan-out, where the SAME
    /// connection receives one request per injection region under one upstream
    /// id: the connection must stay registered until its last in-flight request
    /// completes.
    ///
    /// Callers MUST call `unregister_upstream_request` per request on completion
    /// (success, error, or timeout) — typically after `wait_for_response()` or
    /// in `ensure_document_opened()` error cleanup — to avoid leaking entries.
    pub(crate) fn register_upstream_request(
        &self,
        upstream_id: UpstreamId,
        connection_key: &ConnectionKey,
    ) {
        let mut registry = self
            .upstream_request_registry
            .lock()
            .recover_poison("LanguageServerPool::register_upstream_request");
        *registry
            .entry(upstream_id)
            .or_default()
            .entry(connection_key.clone())
            .or_insert(0) += 1;
    }

    /// Unregister one in-flight request for `(upstream_id, connection_key)`.
    /// The connection is removed once its count reaches zero; the upstream entry
    /// is fully removed once all connections have been unregistered.
    pub(crate) fn unregister_upstream_request(
        &self,
        upstream_id: &UpstreamId,
        connection_key: &ConnectionKey,
    ) {
        let mut registry = self
            .upstream_request_registry
            .lock()
            .recover_poison("LanguageServerPool::unregister_upstream_request");
        if let Some(connections) = registry.get_mut(upstream_id) {
            if let Some(count) = connections.get_mut(connection_key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    connections.remove(connection_key);
                }
            }
            if connections.is_empty() {
                registry.remove(upstream_id);
            }
        }
    }

    /// Remove all server entries for the given upstream request ID.
    ///
    /// Used after first-win dispatch to clean up stale entries from aborted tasks.
    /// Idempotent — safe to call even if entries were already removed.
    pub(crate) fn unregister_all_for_upstream_id(&self, upstream_id: Option<&UpstreamId>) {
        let Some(upstream_id) = upstream_id else {
            return;
        };
        let mut registry = self
            .upstream_request_registry
            .lock()
            .recover_poison("LanguageServerPool::unregister_all_for_upstream_id");
        registry.remove(upstream_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::actor::{OutboundMessage, RouteResult, spawn_reader_task};
    use crate::lsp::bridge::protocol::RegionOffset;
    use std::time::Duration;
    use test_helpers::*;

    // ============================================================
    // Pool Integration Tests
    // ============================================================
    // Unit tests for ConnectionHandle, ConnectionState, GlobalShutdownTimeout,
    // and OpenedVirtualDoc live in their respective submodules.
    // This file contains integration tests that exercise cross-module behavior.

    /// A client `window/workDoneProgress/cancel` for a bridge-minted token is
    /// routed to the owning downstream with its ORIGINAL token restored.
    #[tokio::test]
    async fn forward_work_done_cancel_routes_to_owning_downstream() {
        use tower_lsp_server::ls_types::NumberOrString;

        let pool = LanguageServerPool::new();
        let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);

        let conn = pool.progress_registry.new_connection_id();
        let downstream_token = NumberOrString::Number(1);
        let (upstream_token, _) =
            pool.progress_registry
                .register(conn, downstream_token.clone(), writer_tx);

        pool.forward_work_done_cancel(upstream_token).await;

        let sent = writer_rx
            .try_recv()
            .expect("cancel should be sent downstream");
        let OutboundMessage::Untracked(val) = sent else {
            panic!("Expected Untracked");
        };
        assert_eq!(val["method"], "window/workDoneProgress/cancel");
        // Original downstream token is restored (not the upstream one).
        assert_eq!(val["params"]["token"], serde_json::json!(1));
    }

    /// Cancelling an unknown token is a best-effort no-op (no panic, no send).
    #[tokio::test]
    async fn forward_work_done_cancel_unknown_token_is_noop() {
        use tower_lsp_server::ls_types::NumberOrString;

        let pool = LanguageServerPool::new();
        pool.forward_work_done_cancel(NumberOrString::String("nope".to_string()))
            .await;
        // Reaching here without panic is the assertion.
    }

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
            !connections.contains_key(&ConnectionKey::for_server("test")),
            "Connection should not exist before connection attempt"
        );
    }

    /// Per-root pooling (#382): documents under different marker roots resolve
    /// to distinct connection keys (→ separate downstream processes), while a
    /// document with no marker root resolves to the shared client-root fallback.
    #[test]
    fn connection_key_distinguishes_marker_roots() {
        let tmp = tempfile::tempdir().unwrap();
        // Two sibling projects, each its own `.git` marker root.
        let proj_a = tmp.path().join("a");
        let proj_b = tmp.path().join("b");
        std::fs::create_dir_all(proj_a.join("src")).unwrap();
        std::fs::create_dir_all(proj_b.join("src")).unwrap();
        std::fs::create_dir(proj_a.join(".git")).unwrap();
        std::fs::create_dir(proj_b.join(".git")).unwrap();
        let doc_a = Url::from_file_path(proj_a.join("src/main.py")).unwrap();
        let doc_b = Url::from_file_path(proj_b.join("src/main.py")).unwrap();

        let pool = LanguageServerPool::new();
        let config = devnull_config(); // workspace_markers: None → default [".git"]

        let key_a = pool.connection_key("pyright", &config, Some(&doc_a));
        let key_b = pool.connection_key("pyright", &config, Some(&doc_b));

        assert_ne!(
            key_a, key_b,
            "documents under different .git roots must get distinct connection keys"
        );
        // Same server name, same root → same key (e.g. a sibling file in proj A).
        let doc_a2 = Url::from_file_path(proj_a.join("src/other.py")).unwrap();
        assert_eq!(
            key_a,
            pool.connection_key("pyright", &config, Some(&doc_a2)),
            "documents sharing a marker root must share one connection key"
        );
        // Different server name under the same root → distinct connection.
        assert_ne!(
            key_a,
            pool.connection_key("ruff", &config, Some(&doc_a)),
            "different servers never share a connection"
        );
    }

    /// A document with no marker root (and the no-document case) resolves to the
    /// client-root fallback key, shared across all such documents — preserving
    /// the pre-#382 single-process behavior for non-monorepo layouts. (Distinct
    /// from the #391 `ConnectionKey::shared` instance key.)
    #[test]
    fn connection_key_falls_back_to_client_root_without_marker() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.git` anywhere up the tree from these orphan files.
        let orphan_1 = Url::from_file_path(tmp.path().join("one.py")).unwrap();
        let orphan_2 = Url::from_file_path(tmp.path().join("nested/two.py")).unwrap();

        let pool = LanguageServerPool::new();
        let config = devnull_config();

        let fallback = pool.connection_key("pyright", &config, Some(&orphan_1));
        assert_eq!(
            fallback,
            pool.connection_key("pyright", &config, Some(&orphan_2)),
            "marker-less documents must share the fallback connection"
        );
        assert_eq!(
            fallback,
            pool.connection_key("pyright", &config, None),
            "the no-document case must resolve to the same fallback key"
        );
    }

    /// Build `ServerCapabilities` that advertise the workspaceFolders +
    /// changeNotifications support the shared-instance opt-in (#391) requires.
    fn capable_workspace_folders_caps() -> tower_lsp_server::ls_types::ServerCapabilities {
        use tower_lsp_server::ls_types::{
            OneOf, ServerCapabilities, WorkspaceFoldersServerCapabilities,
            WorkspaceServerCapabilities,
        };
        ServerCapabilities {
            workspace: Some(WorkspaceServerCapabilities {
                workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                    supported: Some(true),
                    change_notifications: Some(OneOf::Left(true)),
                }),
                file_operations: None,
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn workspace_folder_change_updates_primary_root_for_future_spawns() {
        let pool = LanguageServerPool::new();
        let folder = |uri: &str| tower_lsp_server::ls_types::WorkspaceFolder {
            uri: uri.parse().unwrap(),
            name: uri.to_string(),
        };
        let original = folder("file:///original");
        let replacement = folder("file:///replacement");
        pool.set_root_uri(Some(original.uri.to_string()));
        pool.set_workspace_folders(Some(vec![original.clone()]));

        pool.apply_workspace_folder_change(vec![replacement.clone()], &[original])
            .await;

        assert_eq!(pool.root_uri().as_deref(), Some("file:///replacement"));
        assert_eq!(pool.workspace_folders(), Some(vec![replacement]));
    }

    #[tokio::test]
    async fn workspace_folder_change_forwards_only_to_capable_ready_connections() {
        let pool = LanguageServerPool::new();
        let capable =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("capable"))
                .await;
        capable.set_server_capabilities(capable_workspace_folders_caps());
        let incapable = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("incapable"),
        )
        .await;
        incapable.set_server_capabilities(Default::default());
        pool.insert_connection(Arc::clone(&capable)).await;
        pool.insert_connection(Arc::clone(&incapable)).await;
        let added = tower_lsp_server::ls_types::WorkspaceFolder {
            uri: "file:///added".parse().unwrap(),
            name: "added".to_string(),
        };

        pool.apply_workspace_folder_change(vec![added.clone()], &[])
            .await;

        assert_eq!(capable.workspace_folders().snapshot(), Some(vec![added]));
        assert_eq!(incapable.workspace_folders().snapshot(), None);
    }

    fn shared_config() -> crate::config::settings::BridgeServerConfig {
        crate::config::settings::BridgeServerConfig {
            prefer_shared_instance: Some(true),
            settings: None,
            ..devnull_config()
        }
    }

    /// A doc under a `.git` marker root, so the per-root key is a concrete
    /// Marker (not the client-root fallback).
    fn marker_rooted_doc() -> (tempfile::TempDir, Url) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let doc = Url::from_file_path(tmp.path().join("src/main.lua")).unwrap();
        (tmp, doc)
    }

    /// `preferSharedInstance` routes every document to the one shared-instance
    /// key while no shared connection exists yet (#391) — the optimistic path
    /// before any capability is known.
    #[tokio::test]
    async fn resolve_acquire_routes_opt_in_server_to_shared_key() {
        let (_tmp, doc) = marker_rooted_doc();
        let pool = LanguageServerPool::new();
        let config = shared_config();

        let (_marker, key) = pool.resolve_acquire("lua", &config, Some(&doc)).await;
        assert_eq!(key, ConnectionKey::shared("lua"));
        assert!(key.is_shared());

        // A non-opt-in server keeps its per-root key under the same conditions.
        let (_marker, per_root) = pool
            .resolve_acquire("lua", &devnull_config(), Some(&doc))
            .await;
        assert!(!per_root.is_shared());
    }

    /// A marker-less document (no `.git`, no document hint) has no root to
    /// share, so even with `preferSharedInstance` it stays on the client-root
    /// fallback rather than the shared key (#391).
    #[tokio::test]
    async fn resolve_acquire_keeps_marker_less_docs_on_client_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.git` up the tree from this orphan file.
        let orphan = Url::from_file_path(tmp.path().join("scratch.lua")).unwrap();
        let pool = LanguageServerPool::new();
        let config = shared_config();

        let (_marker, key) = pool.resolve_acquire("lua", &config, Some(&orphan)).await;
        assert!(
            !key.is_shared(),
            "a marker-less document must stay on the client-root fallback, not shared"
        );
        // The no-document case likewise stays off the shared key.
        let (_marker, no_doc_key) = pool.resolve_acquire("lua", &config, None).await;
        assert!(!no_doc_key.is_shared());
    }

    /// A capable, Ready shared connection keeps subsequent roots on the shared
    /// key (they join via didChangeWorkspaceFolders).
    #[tokio::test]
    async fn resolve_acquire_joins_capable_shared_connection() {
        let (_tmp, doc) = marker_rooted_doc();
        let pool = LanguageServerPool::new();
        let config = shared_config();

        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::shared("lua")).await;
        handle.set_server_capabilities(capable_workspace_folders_caps());
        pool.insert_connection(handle).await;

        let (_marker, key) = pool.resolve_acquire("lua", &config, Some(&doc)).await;
        assert_eq!(key, ConnectionKey::shared("lua"));
    }

    /// A Ready shared connection whose server never advertised the
    /// workspaceFolders capability makes the opt-in degrade to per-root
    /// instances (#391): a root the incapable shared connection does NOT already
    /// serve routes to a per-root key, not the shared one.
    #[tokio::test]
    async fn resolve_acquire_falls_back_to_per_root_when_shared_is_incapable() {
        let (_tmp, doc) = marker_rooted_doc();
        let pool = LanguageServerPool::new();
        let config = shared_config();

        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::shared("lua")).await;
        // Capabilities set, but WITHOUT workspaceFolders support. Its folder set
        // is empty, so it serves no root yet.
        handle.set_server_capabilities(tower_lsp_server::ls_types::ServerCapabilities::default());
        pool.insert_connection(handle).await;

        let (_marker, key) = pool.resolve_acquire("lua", &config, Some(&doc)).await;
        assert!(
            !key.is_shared(),
            "an incapable shared connection must fall back to a per-root key"
        );
        // And it is the same per-root key the non-opt-in path would pick.
        let per_root = pool.connection_key("lua", &devnull_config(), Some(&doc));
        assert_eq!(key, per_root);
    }

    /// The incapable-fallback must not split a single root across two processes:
    /// the shared connection's OWN spawn root stays on the shared key (it is
    /// already correctly rooted and serving it), while only *other* roots divert
    /// to per-root keys (#391).
    #[tokio::test]
    async fn resolve_acquire_keeps_incapable_shared_own_root_on_shared() {
        let (_tmp, doc) = marker_rooted_doc();
        let pool = LanguageServerPool::new();
        let config = shared_config();

        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::shared("lua")).await;
        handle.set_server_capabilities(tower_lsp_server::ls_types::ServerCapabilities::default());
        // Seed the shared connection's folder set with this doc's marker root,
        // as if it had spawned rooted there.
        let (marker, _) = pool.resolve_marker_and_key("lua", &config, Some(&doc));
        let own_root = marker.expect("doc has a marker root").1;
        handle
            .workspace_folders()
            .add_and_announce(own_root, || true);
        pool.insert_connection(handle).await;

        // A document under the shared connection's own root stays on shared.
        let (_marker, key) = pool.resolve_acquire("lua", &config, Some(&doc)).await;
        assert_eq!(
            key,
            ConnectionKey::shared("lua"),
            "the incapable shared connection's own root must stay on the shared key"
        );

        // A document under a DIFFERENT root still diverts to per-root.
        let (_tmp2, other_doc) = marker_rooted_doc();
        let (_marker, other_key) = pool.resolve_acquire("lua", &config, Some(&other_doc)).await;
        assert!(
            !other_key.is_shared(),
            "a root the incapable shared connection does not serve must go per-root"
        );
    }

    /// Two documents under different marker roots spawn two separate downstream
    /// connections in the pool rather than reusing one (#382). Uses a sink
    /// command and a short timeout: the handshake fails, but each acquisition
    /// still inserts its own keyed connection, which is what we assert.
    #[tokio::test]
    async fn different_marker_roots_create_separate_connections() {
        let tmp = tempfile::tempdir().unwrap();
        let proj_a = tmp.path().join("a");
        let proj_b = tmp.path().join("b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        std::fs::create_dir(proj_a.join(".git")).unwrap();
        std::fs::create_dir(proj_b.join(".git")).unwrap();
        let doc_a = Url::from_file_path(proj_a.join("main.py")).unwrap();
        let doc_b = Url::from_file_path(proj_b.join("main.py")).unwrap();

        let pool = LanguageServerPool::new();
        let config = devnull_config_for_language("python");

        // Sink server never completes the handshake; a short timeout fails fast.
        // Either way the keyed connection is inserted before the handshake runs.
        let _ = pool
            .get_or_create_connection_with_timeout(
                "pyright",
                &config,
                Some(&doc_a),
                Duration::from_millis(150),
            )
            .await;
        let _ = pool
            .get_or_create_connection_with_timeout(
                "pyright",
                &config,
                Some(&doc_b),
                Duration::from_millis(150),
            )
            .await;

        let connections = pool.connections.lock().await;
        assert_eq!(
            connections.len(),
            2,
            "each marker root must get its own pooled connection, not share one"
        );
        let key_a = pool.connection_key("pyright", &config, Some(&doc_a));
        let key_b = pool.connection_key("pyright", &config, Some(&doc_b));
        assert!(
            connections.contains_key(&key_a),
            "root A connection present"
        );
        assert!(
            connections.contains_key(&key_b),
            "root B connection present"
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
            let handle = create_handle_with_key(
                ConnectionState::Initializing,
                ConnectionKey::for_server("lua"),
            )
            .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("lua"), handle);
        }

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)), // upstream_request_id
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)), // upstream_request_id
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

        if !is_lua_ls_available() {
            return;
        }

        let pool = Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        // Insert a ConnectionHandle with Failed state
        {
            let handle =
                create_handle_with_key(ConnectionState::Failed, ConnectionKey::for_server("lua"))
                    .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("lua"), handle);
        }

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)), // upstream_request_id
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
            let handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Should have connection");
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(2)), // upstream_request_id
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

        if !is_lua_ls_available() {
            return;
        }

        let pool = Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)), // upstream_request_id
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
            let handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Connection should exist");
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
                RegionOffset::new(3, 0),
                "print('world')",
                Some(UpstreamId::Number(2)), // upstream_request_id
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
            .get_or_create_connection_with_timeout(
                "test",
                &config,
                None,
                Duration::from_millis(100),
            )
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
        if let Some(handle) = connections.get(&ConnectionKey::for_server("test")) {
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
            .get_or_create_connection_with_timeout(
                "test",
                &config,
                None,
                Duration::from_millis(100),
            )
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
        if !is_lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();

        // Setup: Insert a Failed connection handle
        {
            let handle =
                create_handle_with_key(ConnectionState::Failed, ConnectionKey::for_server("lua"))
                    .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("lua"), handle);
        }

        // Verify Failed state is in cache
        {
            let connections = pool.connections.lock().await;
            let handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Should have cached handle");
            assert_eq!(handle.state(), ConnectionState::Failed, "Should be Failed");
        }

        // Test: Request connection - should remove failed entry and spawn new server
        let config = lua_ls_config();

        let result = pool
            .get_or_create_connection_with_timeout("lua", &config, None, Duration::from_secs(30))
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
            let cached_handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Should have cached handle");
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
        if !is_lua_ls_available() {
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
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            enabled: None,
            settings: None,
        };

        let result = pool
            .get_or_create_connection_with_timeout(
                "lua",
                &unresponsive_config,
                None,
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
            if let Some(handle) = connections.get(&ConnectionKey::for_server("lua")) {
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
            .get_or_create_connection_with_timeout(
                "lua",
                &working_config,
                None,
                Duration::from_secs(30),
            )
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
            let cached_handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Should have cached handle");
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
        if !is_lua_ls_available() {
            return;
        }

        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)),
            )
            .await;
        assert!(result.is_ok(), "Hover request should succeed");

        // Get the virtual URI that was opened
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Send didClose notification (server_name matches the one used in send_hover_request)
        let result = pool
            .send_didclose_notification(&virtual_uri, &ConnectionKey::for_server("lua"))
            .await;
        assert!(
            result.is_ok(),
            "send_didclose_notification should succeed: {:?}",
            result.err()
        );

        // Verify connection is still Ready
        {
            let connections = pool.connections.lock().await;
            let handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Connection should exist");
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
        if !is_lua_ls_available() {
            return;
        }

        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;

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
                RegionOffset::new(3, 0), // region starts at line 3, position is at line 4, so virtual line = 1
                "print('hello')",
                Some(UpstreamId::Number(1)),
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
                RegionOffset::new(7, 0), // region starts at line 7, position is at line 8, so virtual line = 1
                "print('world')",
                Some(UpstreamId::Number(2)),
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
            let handle = connections
                .get(&ConnectionKey::for_server("lua"))
                .expect("Connection should exist");
            assert_eq!(
                handle.state(),
                ConnectionState::Ready,
                "Connection should remain Ready after close_host_document"
            );
        }
    }

    /// Test that forward_didchange_to_opened_docs completes quickly with channel-based sending.
    ///
    /// ls-bridge-message-ordering: Channel-based sends via try_send() are non-blocking.
    /// This verifies forward_didchange_to_opened_docs returns promptly.
    #[tokio::test]
    async fn forward_didchange_is_non_blocking() {
        use super::super::protocol::VirtualDocumentUri;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();

        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        // Simulate successful didOpen by registering the document
        pool.register_opened_document(&host_uri, &virtual_uri, &ConnectionKey::for_server("lua"))
            .await;

        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));

        // ls-bridge-message-ordering: No need to hold a writer lock - sends are channel-based and non-blocking
        use crate::lsp::bridge::coordinator::BridgeInjection;
        let injections = vec![BridgeInjection {
            language: "lua".to_string(),
            region_id: TEST_ULID_LUA_0.to_string(),
            content: "local x = 42".to_string(),
        }];

        let start = Instant::now();
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.forward_didchange_to_opened_docs(&host_uri, 1, &injections)
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "forward_didchange_to_opened_docs should complete quickly (channel-based sends are non-blocking)"
        );
    }

    // ========================================
    // ensure_document_opened tests
    // ========================================
    // Atomic claim logic tests (try_claim_for_open) live in document_tracker.rs.
    // These integration tests verify ensure_document_opened I/O behavior:
    // - Writing didOpen notification to downstream
    // - Post-condition: document marked as opened
    // - Rollback on send failure

    /// Test that ensure_document_opened sends didOpen when document is not yet opened.
    ///
    /// Happy path: Document not opened → try_claim_for_open succeeds
    /// → sends didOpen → marks document as opened.
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
            .ensure_document_opened(
                &mut sender,
                &host_uri,
                &virtual_uri,
                virtual_content,
                &ConnectionKey::for_server("lua"),
            )
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

    #[tokio::test]
    async fn ensure_document_opened_uses_edit_that_arrived_while_server_was_starting() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/stale-open.md").unwrap();
        let host_uri_lsp = url_to_uri(&host_uri);
        let virtual_uri = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua");

        pool.open_host_incarnation(&host_uri, 1).await;
        pool.forward_didchange_to_opened_docs(
            &host_uri,
            1,
            &[super::super::coordinator::BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_0.to_string(),
                content: "print('current')".to_string(),
            }],
        )
        .await;

        let (mut sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);
        pool.ensure_document_opened(
            &mut sender,
            &host_uri,
            &virtual_uri,
            "print('stale')",
            &connection_key,
        )
        .await
        .unwrap();

        let OutboundMessage::Untracked(message) = rx.try_recv().unwrap() else {
            panic!("didOpen must be an untracked notification");
        };
        assert_eq!(
            message["params"]["textDocument"]["text"],
            "print('current')"
        );
    }

    #[tokio::test]
    async fn stale_incarnation_cannot_repopulate_latest_virtual_content_after_fast_reopen() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/cache-fast-reopen.md").unwrap();
        let host_uri_lsp = url_to_uri(&host_uri);
        let virtual_uri = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua");

        pool.open_host_incarnation(&host_uri, 1).await;
        pool.close_host_incarnation(&host_uri, 1).await;
        pool.open_host_incarnation(&host_uri, 2).await;

        // An old process_injections task resumes after close + reopen.
        pool.record_latest_virtual_content(
            &host_uri,
            1,
            "lua",
            TEST_ULID_LUA_0,
            "print('old lifetime')",
        );

        let handle = create_handle_with_key(ConnectionState::Ready, connection_key.clone()).await;
        pool.insert_connection(handle).await;
        pool.eager_open_virtual_documents(
            "lua",
            &devnull_config(),
            &host_uri,
            &host_uri_lsp,
            1,
            vec![super::super::coordinator::BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_0.to_string(),
                content: "print('old lifetime')".to_string(),
            }],
        )
        .await;
        assert!(!pool.is_document_opened(&virtual_uri));
        assert!(pool.host_lifecycle_locks.contains_key(&host_uri));

        let (mut sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);
        pool.ensure_document_opened(
            &mut sender,
            &host_uri,
            &virtual_uri,
            "print('new lifetime')",
            &connection_key,
        )
        .await
        .unwrap();

        let OutboundMessage::Untracked(message) = rx.try_recv().unwrap() else {
            panic!("didOpen must be an untracked notification");
        };
        assert_eq!(
            message["params"]["textDocument"]["text"],
            "print('new lifetime')"
        );

        pool.close_host_incarnation(&host_uri, 2).await;
        pool.eager_open_virtual_documents(
            "lua",
            &devnull_config(),
            &host_uri,
            &host_uri_lsp,
            1,
            vec![super::super::coordinator::BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_0.to_string(),
                content: "print('old lifetime')".to_string(),
            }],
        )
        .await;
        assert!(pool.host_lifecycle_locks.is_empty());
    }

    #[tokio::test]
    async fn host_close_reclaims_unopened_latest_virtual_content() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/cache-close.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.forward_didchange_to_opened_docs(
            &host_uri,
            1,
            &[super::super::coordinator::BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_0.to_string(),
                content: "print('cached')".to_string(),
            }],
        )
        .await;
        assert_eq!(pool.latest_virtual_contents.len(), 1);

        pool.close_host_incarnation(&host_uri, 1).await;
        assert!(pool.close_host_document(&host_uri).await.is_empty());

        assert!(pool.latest_virtual_contents.is_empty());
        assert!(pool.host_lifecycle_locks.is_empty());
    }

    #[tokio::test]
    async fn unchanged_latest_virtual_content_reuses_allocation() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/cache-dedup.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0, "print('same')");
        let before = {
            let host = pool.latest_virtual_contents.get(&host_uri).unwrap();
            let regions = host.contents.get("lua").unwrap();
            Arc::clone(regions.get(TEST_ULID_LUA_0).unwrap().value())
        };

        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0, "print('same')");

        let after = {
            let host = pool.latest_virtual_contents.get(&host_uri).unwrap();
            let regions = host.contents.get("lua").unwrap();
            Arc::clone(regions.get(TEST_ULID_LUA_0).unwrap().value())
        };
        assert!(Arc::ptr_eq(&before, &after));
    }

    #[tokio::test]
    async fn invalidation_reclaims_only_matching_latest_virtual_content() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/cache-invalidation.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0, "first");
        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_1, "second");

        pool.close_invalidated_docs(&host_uri, &[TEST_ULID_LUA_0.parse::<ulid::Ulid>().unwrap()])
            .await;

        let contents = pool.latest_virtual_contents.get(&host_uri).unwrap();
        assert!(
            !contents
                .contents
                .get("lua")
                .is_some_and(|regions| regions.contains_key(TEST_ULID_LUA_0))
        );
        assert!(
            contents
                .contents
                .get("lua")
                .is_some_and(|regions| regions.contains_key(TEST_ULID_LUA_1))
        );
        drop(contents);

        pool.close_invalidated_docs(&host_uri, &[TEST_ULID_LUA_1.parse::<ulid::Ulid>().unwrap()])
            .await;
        let host = pool.latest_virtual_contents.get(&host_uri).unwrap();
        assert!(host.contents.is_empty());
        assert_eq!(host.incarnation, 1);
        drop(host);

        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0, "replacement");
        assert!(
            pool.latest_virtual_contents
                .get(&host_uri)
                .unwrap()
                .contents
                .get("lua")
                .is_some_and(|regions| regions.contains_key(TEST_ULID_LUA_0))
        );
    }

    #[tokio::test]
    async fn replacement_reclaims_only_old_language_virtual_content() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/cache-replacement.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0, "old");
        pool.record_latest_virtual_content(&host_uri, 1, "python", TEST_ULID_LUA_0, "new");
        let replaced = OpenedVirtualDoc {
            virtual_uri: VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0),
            connection_key: ConnectionKey::for_server("lua"),
            connection_generation: 0,
        };

        pool.clear_replaced_virtual_contents(&host_uri, &[replaced]);

        let host = pool.latest_virtual_contents.get(&host_uri).unwrap();
        assert!(!host.contents.contains_key("lua"));
        assert!(
            host.contents
                .get("python")
                .is_some_and(|regions| regions.contains_key(TEST_ULID_LUA_0))
        );
    }

    /// Test that ensure_document_opened skips didOpen when document is already opened.
    ///
    /// Already opened path: Document already claimed
    /// → try_claim_for_open returns false → no didOpen sent.
    #[tokio::test]
    async fn ensure_document_opened_skips_didopen_for_already_opened_document() {
        use super::super::protocol::VirtualDocumentUri;
        use tokio::sync::mpsc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_content = "print('hello')";

        // Pre-register the document (simulate previous successful didOpen)
        pool.register_opened_document(&host_uri, &virtual_uri, &ConnectionKey::for_server("lua"))
            .await;

        // Verify document is already marked as opened
        assert!(
            pool.is_document_opened(&virtual_uri),
            "Document should be marked as opened"
        );

        // Create a channel-based sender (same as production code path)
        let (mut sender, mut rx) = mpsc::channel::<OutboundMessage>(16);

        // Call ensure_document_opened - should skip didOpen
        let result = pool
            .ensure_document_opened(
                &mut sender,
                &host_uri,
                &virtual_uri,
                virtual_content,
                &ConnectionKey::for_server("lua"),
            )
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

    /// Test that ensure_document_opened rolls back the claim on send failure.
    ///
    /// When the channel is closed (BrokenPipe), the claim made by try_claim_for_open
    /// should be rolled back via unclaim_document, allowing a future retry.
    #[tokio::test]
    async fn ensure_document_opened_unclaims_on_send_failure() {
        use super::super::protocol::VirtualDocumentUri;
        use tokio::sync::mpsc;

        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let virtual_content = "print('hello')";

        // Create a channel and drop the receiver to simulate BrokenPipe
        let (mut sender, rx) = mpsc::channel::<OutboundMessage>(16);
        drop(rx);

        // Call ensure_document_opened — should fail with BrokenPipe
        let result = pool
            .ensure_document_opened(
                &mut sender,
                &host_uri,
                &virtual_uri,
                virtual_content,
                &ConnectionKey::for_server("lua"),
            )
            .await;

        assert!(result.is_err(), "Should fail with BrokenPipe");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);

        // The claim should have been rolled back — document is NOT opened
        assert!(
            !pool.is_document_opened(&virtual_uri),
            "Claim should be rolled back on send failure"
        );
        assert!(
            !pool.open_transition_locks.contains_key(&(
                ConnectionKey::for_server("lua"),
                virtual_uri.to_uri_string()
            )),
            "failed open must reclaim its transition lock entry"
        );
    }

    #[tokio::test]
    async fn respawn_purge_reclaims_successful_open_transition_locks() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/purged.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua");
        let (mut sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);
        pool.ensure_document_opened(
            &mut sender,
            &host_uri,
            &virtual_uri,
            "print('opened')",
            &connection_key,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_ok());
        assert!(
            pool.open_transition_locks
                .contains_key(&(connection_key.clone(), virtual_uri.to_uri_string()))
        );

        pool.document_tracker
            .purge_connection(&connection_key)
            .await;
        pool.purge_open_transition_locks(&connection_key).await;

        assert!(
            !pool
                .open_transition_locks
                .iter()
                .any(|entry| entry.key().0 == connection_key),
            "respawn cleanup must reclaim document transition locks"
        );
    }

    #[tokio::test]
    async fn ensure_document_opened_unclaims_when_task_is_aborted_before_enqueue() {
        struct BlockingSender {
            entered: Arc<tokio::sync::Notify>,
        }

        impl message_sender::MessageSender for BlockingSender {
            async fn send_notification<P: serde::Serialize + Send>(
                &mut self,
                _notification: JsonRpcNotification<P>,
            ) -> io::Result<()> {
                self.entered.notify_one();
                std::future::pending().await
            }
        }

        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/abort.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua");
        let entered = Arc::new(tokio::sync::Notify::new());
        let task = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            let virtual_uri = virtual_uri.clone();
            let connection_key = connection_key.clone();
            let entered = Arc::clone(&entered);
            tokio::spawn(async move {
                let mut sender = BlockingSender { entered };
                pool.ensure_document_opened(
                    &mut sender,
                    &host_uri,
                    &virtual_uri,
                    "print('old')",
                    &connection_key,
                )
                .await
            })
        };

        entered.notified().await;
        assert!(!pool.is_document_opened(&virtual_uri));
        assert!(
            pool.document_tracker
                .open_claim_waiter(&virtual_uri, &connection_key)
                .is_some()
        );
        let (sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(1);
        let retry = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            let virtual_uri = virtual_uri.clone();
            let connection_key = connection_key.clone();
            tokio::spawn(async move {
                let mut sender = sender;
                pool.ensure_document_opened(
                    &mut sender,
                    &host_uri,
                    &virtual_uri,
                    "print('new')",
                    &connection_key,
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "retry must wait for the first claim"
        );
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(1), retry)
            .await
            .expect("waiting retry should resume after abort rollback")
            .expect("retry task should not panic")
            .expect("retry should enqueue didOpen");
        assert!(rx.try_recv().is_ok(), "a later didOpen must be retryable");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            pool.document_tracker
                .is_document_opened_on_connection(&virtual_uri, &connection_key),
            "the aborted owner's delayed rollback must not erase the successor claim"
        );
    }

    #[tokio::test]
    async fn did_close_waits_for_open_enqueue_then_removes_sent_state() {
        struct GatedSender {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }

        impl message_sender::MessageSender for GatedSender {
            async fn send_notification<P: serde::Serialize + Send>(
                &mut self,
                _notification: JsonRpcNotification<P>,
            ) -> io::Result<()> {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(())
            }
        }

        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/open-close.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua");
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let opening = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            let virtual_uri = virtual_uri.clone();
            let connection_key = connection_key.clone();
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                let mut sender = GatedSender { entered, release };
                pool.ensure_document_opened(
                    &mut sender,
                    &host_uri,
                    &virtual_uri,
                    "print('hello')",
                    &connection_key,
                )
                .await
            })
        };
        entered.notified().await;
        let closing = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            tokio::spawn(async move { pool.close_host_document(&host_uri).await })
        };
        tokio::task::yield_now().await;
        assert!(
            !closing.is_finished(),
            "close must serialize behind didOpen"
        );

        release.notify_one();
        opening.await.unwrap().unwrap();
        let closed = closing.await.unwrap();
        assert_eq!(closed.len(), 1);
        assert!(!pool.is_document_opened(&virtual_uri));
        assert!(
            pool.get_all_connections_for_virtual_uri(&virtual_uri)
                .is_empty()
        );
        assert!(
            pool.document_tracker
                .open_claim_waiter(&virtual_uri, &connection_key)
                .is_none()
        );
    }

    #[tokio::test]
    async fn stale_connection_generation_close_does_not_untrack_reopened_document() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///test/reopened.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua");
        pool.register_opened_document(&host_uri, &virtual_uri, &connection_key)
            .await;
        let stale = pool.document_tracker.host_virtual_docs(&host_uri).await[0].clone();

        pool.document_tracker
            .purge_connection(&connection_key)
            .await;
        pool.register_opened_document(&host_uri, &virtual_uri, &connection_key)
            .await;
        assert_ne!(
            stale.connection_generation,
            pool.document_connection_generation(&connection_key)
        );

        pool.close_single_virtual_doc(&stale).await;
        assert!(
            pool.document_tracker
                .is_document_opened_on_connection(&virtual_uri, &connection_key),
            "a stale close must not erase the replacement generation's open state"
        );
    }

    // ========================================
    // Connection State Machine Integration Tests
    // ========================================
    // Unit tests for ConnectionState enum live in connection_state.rs.
    // These integration tests verify pool behavior with different connection states.

    /// Test that requests during Closing state receive error immediately.
    ///
    /// ls-bridge-message-ordering Operation Gating: When connection is Closing, new requests
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
            let handle =
                create_handle_with_key(ConnectionState::Closing, ConnectionKey::for_server("lua"))
                    .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("lua"), handle);
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)), // upstream_request_id
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
                RegionOffset::new(3, 0),
                "print('hello')",
                Some(UpstreamId::Number(1)), // upstream_request_id
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
    /// ls-bridge-graceful-shutdown: Graceful shutdown requires sending LSP "shutdown" request and
    /// waiting for the server's response before sending "exit" notification.
    /// This test verifies the shutdown request is properly formatted and sent.
    #[tokio::test]
    async fn shutdown_sends_lsp_shutdown_request_and_waits_for_response() {
        if !is_lua_ls_available() {
            return;
        }

        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        // First, establish a Ready connection
        let handle = pool
            .get_or_create_connection("lua", &config, None)
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

        // Perform shutdown with timeout (ls-bridge-timeout-hierarchy: graceful_shutdown has no internal timeout,
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
    /// ls-bridge-graceful-shutdown: Shutdown transitions to Closing state first, which rejects new
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
    /// ls-bridge-graceful-shutdown: All connections shut down in parallel with a global timeout.
    /// This test verifies that multiple connections can be shut down concurrently.
    #[tokio::test]
    async fn shutdown_all_handles_multiple_connections_in_parallel() {
        let pool = LanguageServerPool::new();

        // Create multiple connections with different states
        {
            let ready_handle =
                create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua"))
                    .await;
            let failed_handle = create_handle_with_key(
                ConnectionState::Failed,
                ConnectionKey::for_server("python"),
            )
            .await;
            let closing_handle =
                create_handle_with_key(ConnectionState::Closing, ConnectionKey::for_server("rust"))
                    .await;

            let mut connections = pool.connections.lock().await;
            connections.insert(ConnectionKey::for_server("lua"), ready_handle);
            connections.insert(ConnectionKey::for_server("python"), failed_handle);
            connections.insert(ConnectionKey::for_server("rust"), closing_handle);
        }

        // Call shutdown_all
        pool.shutdown_all().await;

        // Verify final states
        let connections = pool.connections.lock().await;

        // Ready -> should be Closed (went through graceful shutdown)
        let lua_handle = connections
            .get(&ConnectionKey::for_server("lua"))
            .expect("lua should exist");
        assert_eq!(
            lua_handle.state(),
            ConnectionState::Closed,
            "Ready connection should be Closed after shutdown_all"
        );

        // Failed -> should be Closed (directly, no LSP handshake)
        let python_handle = connections
            .get(&ConnectionKey::for_server("python"))
            .expect("python should exist");
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
        let rust_handle = connections
            .get(&ConnectionKey::for_server("rust"))
            .expect("rust should exist");
        assert_eq!(
            rust_handle.state(),
            ConnectionState::Closed,
            "Closing connection should be Closed after global timeout triggers force_kill_all"
        );
    }

    // ========================================
    // Forced Shutdown Tests
    // ========================================

    /// A spawn that reaches the pool after shutdown has begun must be rejected,
    /// not create a connection that escapes `shutdown_all`'s snapshot+teardown and
    /// leaks a child process. Guards the narrow terminal-time eager-spawn window
    /// (`LanguageServerPool::shutting_down`).
    #[tokio::test]
    async fn rejects_new_connection_spawn_after_shutdown() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();

        // Shutdown with no live connections still arms the new-spawn gate.
        pool.shutdown_all().await;

        let err = pool
            .get_or_create_connection_with_timeout("test", &config, None, Duration::from_secs(5))
            .await
            .map(|_| ()) // ConnectionHandle isn't Debug; discard it for expect_err
            .expect_err("a spawn after shutdown must be rejected");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);

        // Nothing was inserted — no connection escaped teardown.
        let connections = pool.connections.lock().await;
        assert!(
            connections.is_empty(),
            "a rejected spawn must not insert a connection"
        );
    }

    /// Test that pool.shutdown_all_with_timeout force-kills unresponsive processes.
    ///
    /// ls-bridge-graceful-shutdown/ls-bridge-timeout-hierarchy: When graceful shutdown times out, force_kill_all is called
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
            ConnectionKey::for_server("test"),
            crate::lsp::bridge::WorkspaceFolderSet::new(None),
            std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
        ));

        // Add connection to pool
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("test"), handle);

        // Start timer
        let start = Instant::now();

        // Call shutdown_all with minimum valid timeout (5 seconds per ls-bridge-timeout-hierarchy)
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
        let handle = connections
            .get(&ConnectionKey::for_server("test"))
            .expect("connection should exist");
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

        if !is_lua_ls_available() {
            return;
        }

        let pool = Arc::new(LanguageServerPool::new());
        let config = lua_ls_config();

        // Step 1: Establish a Ready connection
        let handle = pool
            .get_or_create_connection("lua", &config, None)
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
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("hung_server"),
            )
            .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("hung_server"), handle);
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
        if let Some(handle) = connections.get(&ConnectionKey::for_server("hung_server")) {
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
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server(format!("hung_server_{}", i)),
            )
            .await;
            pool.connections.lock().await.insert(
                ConnectionKey::for_server(format!("hung_server_{}", i)),
                handle,
            );
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
            let key = ConnectionKey::for_server(format!("hung_server_{}", i));
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
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server(format!("server_{}", i)),
            )
            .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server(format!("server_{}", i)), handle);
        }

        // Use minimum valid timeout (5s)
        let timeout = GlobalShutdownTimeout::new(Duration::from_secs(5)).expect("5s is valid");

        pool.shutdown_all_with_timeout(timeout).await;

        // All connections should be in Closed state (via graceful shutdown or force-kill)
        let connections = pool.connections.lock().await;
        for i in 0..2 {
            let key = ConnectionKey::for_server(format!("server_{}", i));
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
    /// ls-bridge-message-ordering: The 3-phase shutdown protocol ensures:
    /// 1. Stop signal sent to writer task
    /// 2. Writer task drains queue and confirms idle
    /// 3. Writer is reclaimed for LSP shutdown sequence
    #[tokio::test]
    async fn graceful_shutdown_coordinates_with_writer_task() {
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Send a notification before shutdown (will be queued)
        let notification =
            super::super::protocol::JsonRpcNotification::new("test", serde_json::json!({}));
        let result = handle.send_notification(notification);
        assert_eq!(
            result,
            NotificationSendResult::Queued,
            "Should be able to send notification before shutdown"
        );

        // Perform graceful shutdown with timeout (ls-bridge-timeout-hierarchy: caller provides timeout).
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
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");
        assert!(handle.router().claim_for_write(downstream_id));
        handle.router().mark_sent(downstream_id);

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        // Forward cancel request
        let result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), || {})
            .await;

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
            handle.router().lookup_downstream_ids(&upstream_id),
            vec![downstream_id],
            "Cancel map entry should still exist after cancel forwarding"
        );
    }

    /// `forward_cancel_downstream` sends the cancel for the EXACT downstream
    /// id it is given, with no upstream-id lookup — the formatting pipeline's
    /// per-step timeout uses it because the timed-out step's upstream-id
    /// mapping is already gone (router cleanup on future drop) and the
    /// upstream id can fan out to sibling regions' requests.
    #[tokio::test]
    async fn forward_cancel_downstream_sends_notification_for_specific_id() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Two in-flight requests sharing one upstream id (sibling regions).
        let upstream_id = UpstreamId::Number(42);
        let (first_id, _rx1) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register first request");
        let (_second_id, _rx2) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register second request");

        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));

        // Cancel exactly the first request by its downstream id.
        let result = pool
            .forward_cancel_downstream(&ConnectionKey::for_server("lua"), first_id)
            .await;
        assert!(
            result.is_ok(),
            "forward_cancel_downstream should succeed: {:?}",
            result.err()
        );

        // Pending entries and cancel mappings stay (cancel never removes them;
        // responses do).
        assert_eq!(handle.router().pending_count(), 2);
    }

    /// `forward_cancel_downstream` silently drops when there is no connection
    /// (best-effort semantics, same as forward_cancel).
    #[tokio::test]
    async fn forward_cancel_downstream_silently_drops_when_no_connection() {
        let pool = LanguageServerPool::new();

        let result = pool
            .forward_cancel_downstream(&ConnectionKey::for_server("nonexistent"), RequestId::new(7))
            .await;

        assert!(
            result.is_ok(),
            "best-effort cancel must not error on a missing connection: {:?}",
            result.err()
        );
    }

    /// Test that cancel forwarding silently drops when no connection exists (best-effort semantics).
    #[tokio::test]
    async fn forward_cancel_silently_drops_when_no_connection() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);
        pool.register_upstream_request(
            upstream_id.clone(),
            &ConnectionKey::for_server("nonexistent"),
        );

        let result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id, || {})
            .await;

        // Per best-effort semantics, this should succeed (silent drop)
        assert!(
            result.is_ok(),
            "forward_cancel should silently drop for nonexistent connection"
        );
    }

    /// Test that cancel forwarding silently drops when the upstream ID has no
    /// in-flight downstream request on the server (best-effort semantics).
    #[tokio::test]
    async fn forward_cancel_silently_drops_when_upstream_id_not_found() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Insert connection but don't register any request with the router
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));
        let upstream_id = UpstreamId::Number(999);
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        let result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id, || {})
            .await;

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

        pool.register_upstream_request(UpstreamId::Number(42), &ConnectionKey::for_server("lua"));

        let registry = pool.upstream_request_registry.lock().unwrap();
        let servers = registry
            .get(&UpstreamId::Number(42))
            .expect("should have entry");
        assert!(servers.contains_key(&ConnectionKey::for_server("lua")));
        assert_eq!(servers.len(), 1);
    }

    /// Test that unregister_upstream_request removes the mapping.
    #[test]
    fn unregister_upstream_request_removes_mapping() {
        let pool = LanguageServerPool::new();

        pool.register_upstream_request(UpstreamId::Number(42), &ConnectionKey::for_server("lua"));
        pool.unregister_upstream_request(
            &UpstreamId::Number(42),
            &ConnectionKey::for_server("lua"),
        );

        let registry = pool.upstream_request_registry.lock().unwrap();
        assert_eq!(registry.get(&UpstreamId::Number(42)), None);
    }

    /// `notify` wakes the upstream handler, whose cancellation path immediately
    /// tears down the registry entry (`unregister_all_for_upstream_id`). If the
    /// forwarder read the registry only after notifying, that cleanup could win
    /// the race and the downstream `$/cancelRequest` would silently vanish.
    /// The capture-before-notify contract makes the cleanup harmless.
    #[tokio::test]
    async fn forward_cancel_with_notify_survives_handler_cleanup_in_notify() {
        use std::sync::Arc;

        let pool = Arc::new(LanguageServerPool::new());
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");
        assert!(handle.router().claim_for_write(downstream_id));
        handle.router().mark_sent(downstream_id);

        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        // Simulate the handler waking on the cancel notification and running
        // its cleanup before the forwarding pass sends anything.
        let cleanup_pool = Arc::clone(&pool);
        let cleanup_id = upstream_id.clone();
        let result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), move || {
                cleanup_pool.unregister_all_for_upstream_id(Some(&cleanup_id));
            })
            .await;

        assert!(
            result.is_ok(),
            "forwarding should succeed despite cleanup in notify: {:?}",
            result.err()
        );
        // The cancel must still have been sent: the pending entry stays (cancel
        // never removes it) and the router mapping still names the downstream id.
        assert_eq!(
            handle.router().lookup_downstream_ids(&upstream_id),
            vec![downstream_id],
            "downstream request should have been captured before notify ran"
        );
        assert_eq!(
            pool.cancel_metrics.snapshot().0,
            1,
            "exactly one $/cancelRequest should have been sent downstream"
        );
    }

    /// Whole-document fan-out issues multiple concurrent requests to the SAME
    /// server under one upstream id (one per injection region). Completing one
    /// region must not unregister the server while a sibling request is still
    /// in flight, or `forward_cancel_by_upstream_id` would skip that server
    /// and the sibling's cancel would never reach it.
    #[test]
    fn unregister_upstream_request_keeps_server_while_sibling_requests_in_flight() {
        let pool = LanguageServerPool::new();

        pool.register_upstream_request(UpstreamId::Number(42), &ConnectionKey::for_server("lua"));
        pool.register_upstream_request(UpstreamId::Number(42), &ConnectionKey::for_server("lua"));

        pool.unregister_upstream_request(
            &UpstreamId::Number(42),
            &ConnectionKey::for_server("lua"),
        );
        {
            let registry = pool.upstream_request_registry.lock().unwrap();
            assert!(
                registry.contains_key(&UpstreamId::Number(42)),
                "server must stay registered while its second request is in flight"
            );
        }

        pool.unregister_upstream_request(
            &UpstreamId::Number(42),
            &ConnectionKey::for_server("lua"),
        );
        let registry = pool.upstream_request_registry.lock().unwrap();
        assert_eq!(registry.get(&UpstreamId::Number(42)), None);
    }

    /// Test that forward_cancel_by_upstream_id uses the registry to find the language.
    #[tokio::test]
    async fn forward_cancel_by_upstream_id_uses_registry() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Register a request with upstream ID mapping in ResponseRouter
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");
        assert!(handle.router().claim_for_write(downstream_id));
        handle.router().mark_sent(downstream_id);

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));

        // Register the upstream request in the registry
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        // Forward cancel by upstream ID only (no language parameter)
        let result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), || {})
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
            .forward_cancel_by_upstream_id_with_notify(UpstreamId::Number(999), || {})
            .await;

        // Per best-effort semantics, this should succeed (silent drop)
        assert!(
            result.is_ok(),
            "forward_cancel_by_upstream_id should silently drop for unknown ID"
        );
    }

    /// Per LSP spec, a cancelled request still receives a response (either the normal
    /// result or an error with code -32800). The cancel forwarding mechanism must
    /// preserve the pending entry so the eventual downstream response can be delivered.
    #[tokio::test]
    async fn response_forwarding_works_after_cancel() {
        use std::sync::Arc;

        let pool = LanguageServerPool::new();
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");
        assert!(handle.router().claim_for_write(downstream_id));
        handle.router().mark_sent(downstream_id);

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        // Forward cancel request (simulating client cancelling the request)
        let cancel_result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), || {})
            .await;
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
        assert!(
            handle
                .router()
                .lookup_downstream_ids(&upstream_id)
                .is_empty(),
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
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");

        // Insert the handle into the pool
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        // Forward cancel request
        let cancel_result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), || {})
            .await;
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
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;

        // Register a request with upstream ID
        let upstream_id = UpstreamId::Number(42);
        let (downstream_id, _response_rx) = handle
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register request");
        assert!(handle.router().claim_for_write(downstream_id));
        handle.router().mark_sent(downstream_id);

        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("lua"), Arc::clone(&handle));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua"));

        // Forward cancel
        let _ = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), || {})
            .await;

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
        pool.register_upstream_request(
            UpstreamId::Number(1),
            &ConnectionKey::for_server("nonexistent"),
        );
        let _ = pool
            .forward_cancel_by_upstream_id_with_notify(UpstreamId::Number(1), || {})
            .await;

        // Test: not in registry
        let _ = pool
            .forward_cancel_by_upstream_id_with_notify(UpstreamId::Number(999), || {})
            .await;

        // Test: connection not ready
        let handle_init = create_handle_with_key(
            ConnectionState::Initializing,
            ConnectionKey::for_server("init_lang"),
        )
        .await;
        pool.connections.lock().await.insert(
            ConnectionKey::for_server("init_lang"),
            Arc::clone(&handle_init),
        );
        pool.register_upstream_request(
            UpstreamId::Number(2),
            &ConnectionKey::for_server("init_lang"),
        );
        let _ = pool
            .forward_cancel_by_upstream_id_with_notify(UpstreamId::Number(2), || {})
            .await;

        // Test: unknown upstream ID
        let handle_ready = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("ready_lang"),
        )
        .await;
        pool.connections.lock().await.insert(
            ConnectionKey::for_server("ready_lang"),
            Arc::clone(&handle_ready),
        );
        pool.register_upstream_request(
            UpstreamId::Number(3),
            &ConnectionKey::for_server("ready_lang"),
        );
        let _ = pool
            .forward_cancel_by_upstream_id_with_notify(UpstreamId::Number(3), || {})
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
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("tsgo")).await;
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("tsgo"), Arc::clone(&handle));

        // Verify there is NO connection for "typescript" (the language)
        {
            let connections = pool.connections.lock().await;
            assert!(
                connections
                    .get(&ConnectionKey::for_server("typescript"))
                    .is_none(),
                "Should NOT have a 'typescript' connection"
            );
            assert!(
                connections
                    .get(&ConnectionKey::for_server("tsgo"))
                    .is_some(),
                "Should have a 'tsgo' connection"
            );
        }

        // Send didClose with server_name="tsgo"
        // This should succeed because we look up by server_name, not language
        let result = pool
            .send_didclose_notification(&virtual_uri, &ConnectionKey::for_server("tsgo"))
            .await;
        assert!(
            result.is_ok(),
            "send_didclose_notification should succeed when using server_name 'tsgo': {:?}",
            result.err()
        );

        // Verify connection is still Ready (not closed)
        {
            let connections = pool.connections.lock().await;
            let handle = connections
                .get(&ConnectionKey::for_server("tsgo"))
                .expect("Connection should exist");
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
        pool.register_opened_document(&host_uri, &virtual_uri, &ConnectionKey::for_server("tsgo"))
            .await;

        // Insert a connection keyed by "tsgo" (NOT "typescript")
        let handle =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("tsgo")).await;
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("tsgo"), Arc::clone(&handle));

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
            closed_docs[0].connection_key,
            ConnectionKey::for_server("tsgo"),
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
            let handle = connections
                .get(&ConnectionKey::for_server("tsgo"))
                .expect("Connection should exist");
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
            assert!(!connections.contains_key(&ConnectionKey::for_server("test-server")));
        }

        // Spawn server (ignoring errors like ensure_server_ready does)
        let _ = pool
            .get_or_create_connection_with_timeout("test-server", &config, None, short_timeout)
            .await;

        // After: connection entry exists (state may be Initializing or Failed)
        // With devnull, the handshake times out quickly, so it transitions to Failed
        {
            let connections = pool.connections.lock().await;
            assert!(
                connections.contains_key(&ConnectionKey::for_server("test-server")),
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
            .get_or_create_connection_with_timeout("test-server", &config, None, short_timeout)
            .await;
        let _ = pool
            .get_or_create_connection_with_timeout("test-server", &config, None, short_timeout)
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
        if !is_lua_ls_available() {
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
                .get(&ConnectionKey::for_server("lua-ls"))
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
        let handle = create_handle_with_key(
            ConnectionState::Initializing,
            ConnectionKey::for_server("test-server"),
        )
        .await;
        let handle_clone = Arc::clone(&handle);
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("test-server"), handle);

        // Spawn a task that will transition to Ready after a delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle_clone.set_state(ConnectionState::Ready);
        });

        // Call get_or_create_connection_wait_ready - should wait and succeed
        let result = pool
            .get_or_create_connection_wait_ready(
                "test-server",
                &config,
                None,
                Duration::from_secs(1),
            )
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
        let handle = create_handle_with_key(
            ConnectionState::Initializing,
            ConnectionKey::for_server("test-server"),
        )
        .await;
        let handle_clone = Arc::clone(&handle);
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("test-server"), handle);

        // Spawn a task that will transition to Failed after a delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            handle_clone.set_state(ConnectionState::Failed);
        });

        // Call get_or_create_connection_wait_ready - should fail
        let result = pool
            .get_or_create_connection_wait_ready(
                "test-server",
                &config,
                None,
                Duration::from_secs(1),
            )
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
        let handle = create_handle_with_key(
            ConnectionState::Initializing,
            ConnectionKey::for_server("test-server"),
        )
        .await;
        pool.connections
            .lock()
            .await
            .insert(ConnectionKey::for_server("test-server"), handle);

        // Call with short timeout - should timeout
        let result = pool
            .get_or_create_connection_wait_ready(
                "test-server",
                &config,
                None,
                Duration::from_millis(50),
            )
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
        if !is_lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();
        let config = lua_ls_config();

        // First call establishes Ready connection
        let handle1 = pool
            .get_or_create_connection("lua", &config, None)
            .await
            .expect("should establish connection");
        assert_eq!(handle1.state(), ConnectionState::Ready);

        // Second call via wait_ready should return immediately
        let start = std::time::Instant::now();
        let handle2 = pool
            .get_or_create_connection_wait_ready("lua", &config, None, Duration::from_secs(1))
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

        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua-ls"));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("pyright"));

        let registry = pool.upstream_request_registry.lock().unwrap();
        let servers = registry.get(&upstream_id).expect("should have entry");

        assert!(servers.contains_key(&ConnectionKey::for_server("lua-ls")));
        assert!(servers.contains_key(&ConnectionKey::for_server("pyright")));
        assert_eq!(servers.len(), 2);
    }

    /// Test that unregistering removes only the specified server from the set.
    #[test]
    fn unregister_upstream_request_removes_single_server() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua-ls"));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("pyright"));
        pool.unregister_upstream_request(&upstream_id, &ConnectionKey::for_server("lua-ls"));

        let registry = pool.upstream_request_registry.lock().unwrap();
        let servers = registry.get(&upstream_id).expect("should still have entry");

        assert!(!servers.contains_key(&ConnectionKey::for_server("lua-ls")));
        assert!(servers.contains_key(&ConnectionKey::for_server("pyright")));
        assert_eq!(servers.len(), 1);
    }

    /// Test that unregistering the last server cleans up the entire entry.
    #[test]
    fn unregister_upstream_request_cleans_up_empty_set() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua-ls"));
        pool.unregister_upstream_request(&upstream_id, &ConnectionKey::for_server("lua-ls"));

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
        let handle_lua =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua-ls"))
                .await;
        let (downstream_id_lua, _rx_lua) = handle_lua
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register lua request");
        assert!(handle_lua.router().claim_for_write(downstream_id_lua));
        handle_lua.router().mark_sent(downstream_id_lua);

        let handle_py =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("pyright"))
                .await;
        let (downstream_id_py, _rx_py) = handle_py
            .register_request_with_upstream(Some(upstream_id.clone()))
            .expect("should register py request");
        assert!(handle_py.router().claim_for_write(downstream_id_py));
        handle_py.router().mark_sent(downstream_id_py);

        // Insert handles
        {
            let mut connections = pool.connections.lock().await;
            connections.insert(ConnectionKey::for_server("lua-ls"), Arc::clone(&handle_lua));
            connections.insert(ConnectionKey::for_server("pyright"), Arc::clone(&handle_py));
        }

        // Register both servers for the same upstream ID
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua-ls"));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("pyright"));

        // Forward cancel - should succeed for both servers
        let result = pool
            .forward_cancel_by_upstream_id_with_notify(upstream_id.clone(), || {})
            .await;
        assert!(result.is_ok());

        // Verify metrics show 2 successful cancels
        let (successful, _, _, _, _) = pool.cancel_metrics().snapshot();
        assert_eq!(
            successful, 2,
            "should have forwarded cancel to both servers"
        );
    }

    /// Test that unregister_all_for_upstream_id removes the entire entry at once.
    #[test]
    fn unregister_all_for_upstream_id_removes_entire_entry() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(42);

        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("lua-ls"));
        pool.register_upstream_request(upstream_id.clone(), &ConnectionKey::for_server("pyright"));
        pool.unregister_all_for_upstream_id(Some(&upstream_id));

        let registry = pool.upstream_request_registry.lock().unwrap();
        assert!(
            registry.get(&upstream_id).is_none(),
            "entire entry should be removed"
        );
    }

    /// Test that unregister_all_for_upstream_id is idempotent (no-op on missing entry).
    #[test]
    fn unregister_all_for_upstream_id_is_idempotent() {
        let pool = LanguageServerPool::new();
        let upstream_id = UpstreamId::Number(99);

        // Should not panic when called on non-existent entry
        pool.unregister_all_for_upstream_id(Some(&upstream_id));

        let registry = pool.upstream_request_registry.lock().unwrap();
        assert!(registry.get(&upstream_id).is_none());
    }

    // ============================================================
    // forward_didchange multi-server tests
    // ============================================================

    #[tokio::test]
    async fn forward_didchange_waits_for_pending_didopen_then_sends() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/pending-open.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);
        let connection_key = ConnectionKey::for_server("lua_ls");
        let handle = create_handle_with_key(ConnectionState::Ready, connection_key.clone()).await;
        pool.connections
            .lock()
            .await
            .insert(connection_key.clone(), handle);

        let transition = pool.open_transition_lock(&virtual_uri, &connection_key);
        let transition_guard = transition.lock().await;
        assert!(
            pool.document_tracker
                .try_claim_for_open(&virtual_uri, &connection_key)
                .await
        );
        let claim = pool
            .document_tracker
            .open_claim_waiter(&virtual_uri, &connection_key)
            .unwrap();
        pool.document_tracker
            .register_pending_document(&host_uri, &virtual_uri, &connection_key)
            .await;

        let forwarding = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            tokio::spawn(async move {
                pool.forward_didchange_to_opened_docs(
                    &host_uri,
                    1,
                    &[crate::lsp::bridge::coordinator::BridgeInjection {
                        language: "lua".to_string(),
                        region_id: TEST_ULID_LUA_0.to_string(),
                        content: "print('new')".to_string(),
                    }],
                )
                .await;
            })
        };
        tokio::task::yield_now().await;
        assert!(
            !forwarding.is_finished(),
            "didChange must serialize behind the pending didOpen"
        );

        assert!(
            pool.document_tracker
                .mark_open_sent(&virtual_uri, &connection_key, &claim)
        );
        drop(transition_guard);
        forwarding.await.unwrap();
        assert_eq!(
            pool.increment_document_version(&virtual_uri, &connection_key)
                .await,
            Some(3),
            "didChange should advance version after didOpen promotion"
        );
    }

    /// When the same virtual doc is opened on two servers (e.g., emmylua and lua_ls),
    /// didChange must be forwarded to both.
    #[tokio::test]
    async fn forward_didchange_sends_to_all_servers() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Register the same virtual doc for two servers
        pool.register_opened_document(
            &host_uri,
            &virtual_uri,
            &ConnectionKey::for_server("emmylua"),
        )
        .await;
        pool.register_opened_document(
            &host_uri,
            &virtual_uri,
            &ConnectionKey::for_server("lua_ls"),
        )
        .await;

        // Insert Ready connections for both servers
        {
            let handle_emmylua = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("emmylua"),
            )
            .await;
            let handle_lua_ls =
                create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua_ls"))
                    .await;
            let mut connections = pool.connections.lock().await;
            connections.insert(ConnectionKey::for_server("emmylua"), handle_emmylua);
            connections.insert(ConnectionKey::for_server("lua_ls"), handle_lua_ls);
        }

        // Forward didChange
        let injections = vec![crate::lsp::bridge::coordinator::BridgeInjection {
            language: "lua".to_string(),
            region_id: TEST_ULID_LUA_0.to_string(),
            content: "print('hello')".to_string(),
        }];
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.forward_didchange_to_opened_docs(&host_uri, 1, &injections)
            .await;

        // Verify both servers got their versions incremented (1 -> 2)
        let version_emmylua = pool
            .increment_document_version(&virtual_uri, &ConnectionKey::for_server("emmylua"))
            .await;
        let version_lua_ls = pool
            .increment_document_version(&virtual_uri, &ConnectionKey::for_server("lua_ls"))
            .await;

        // After forward_didchange incremented once (1->2), our manual increment makes it 2->3
        assert_eq!(
            version_emmylua,
            Some(3),
            "emmylua should have version 3 (opened=1, didChange=2, test-increment=3)"
        );
        assert_eq!(
            version_lua_ls,
            Some(3),
            "lua_ls should have version 3 (opened=1, didChange=2, test-increment=3)"
        );
    }

    /// Test that forward_didchange skips servers in Initializing state.
    ///
    /// Only Ready servers should receive didChange notifications.
    /// Initializing servers haven't completed handshake yet.
    #[tokio::test]
    async fn forward_didchange_skips_initializing_server() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "lua", TEST_ULID_LUA_0);

        // Register the same virtual doc for two servers
        pool.register_opened_document(
            &host_uri,
            &virtual_uri,
            &ConnectionKey::for_server("ready_server"),
        )
        .await;
        pool.register_opened_document(
            &host_uri,
            &virtual_uri,
            &ConnectionKey::for_server("init_server"),
        )
        .await;

        // One Ready, one Initializing
        {
            let handle_ready = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("ready_server"),
            )
            .await;
            let handle_init = create_handle_with_key(
                ConnectionState::Initializing,
                ConnectionKey::for_server("init_server"),
            )
            .await;
            let mut connections = pool.connections.lock().await;
            connections.insert(ConnectionKey::for_server("ready_server"), handle_ready);
            connections.insert(ConnectionKey::for_server("init_server"), handle_init);
        }

        // Forward didChange
        let injections = vec![crate::lsp::bridge::coordinator::BridgeInjection {
            language: "lua".to_string(),
            region_id: TEST_ULID_LUA_0.to_string(),
            content: "print('hello')".to_string(),
        }];
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.forward_didchange_to_opened_docs(&host_uri, 1, &injections)
            .await;

        // ready_server should have been incremented (1->2)
        let version_ready = pool
            .increment_document_version(&virtual_uri, &ConnectionKey::for_server("ready_server"))
            .await;
        assert_eq!(
            version_ready,
            Some(3),
            "ready_server: opened=1, didChange=2, test-increment=3"
        );

        // init_server should NOT have been incremented (still at 1)
        let version_init = pool
            .increment_document_version(&virtual_uri, &ConnectionKey::for_server("init_server"))
            .await;
        assert_eq!(
            version_init,
            Some(2),
            "init_server: opened=1, no didChange, test-increment=2"
        );
    }

    /// `pull_driven_servers` classifies among `candidates` exactly the servers
    /// with a live connection advertising `textDocument/diagnostic` (#425):
    /// a pull-capable server is returned, a push-only one is not, and a name
    /// with no live connection is dropped — all without creating a connection.
    #[tokio::test]
    async fn pull_driven_servers_classifies_by_live_capability() {
        use std::collections::HashSet;
        use tower_lsp_server::ls_types::{
            DiagnosticOptions, DiagnosticServerCapabilities, ServerCapabilities,
        };

        let pool = LanguageServerPool::new();

        // "ra" advertises pull diagnostics (static capability) → pull-driven.
        let ra =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("ra")).await;
        ra.set_server_capabilities(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                DiagnosticOptions::default(),
            )),
            ..Default::default()
        });
        pool.insert_connection(ra).await;

        // "linter" has no diagnostic capability → push-driven.
        let linter =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("linter"))
                .await;
        linter.set_server_capabilities(ServerCapabilities::default());
        pool.insert_connection(linter).await;

        let candidates = HashSet::from([
            "ra", "linter", "ghost", // no live connection
        ]);
        let pull_driven = pool.pull_driven_servers(&candidates).await;

        assert_eq!(
            pull_driven,
            HashSet::from(["ra".to_string()]),
            "only the pull-capable server with a live connection is pull-driven"
        );
    }

    /// An empty candidate set short-circuits to empty without touching the pool.
    #[tokio::test]
    async fn pull_driven_servers_empty_candidates_returns_empty() {
        use std::collections::HashSet;
        let pool = LanguageServerPool::new();
        assert!(
            pool.pull_driven_servers(&HashSet::<&str>::new())
                .await
                .is_empty()
        );
    }

    /// `servers_known_incapable` returns exactly the candidates whose capability
    /// is *definitively* known-absent: a `Ready` connection lacking `method`.
    /// A capable server, a still-`Initializing` one, and a name with no live
    /// connection are all kept out of the result (capability-prefilter-fanout).
    #[tokio::test]
    async fn servers_known_incapable_returns_only_ready_and_lacking() {
        use std::collections::HashSet;
        use tower_lsp_server::ls_types::{HoverProviderCapability, ServerCapabilities};

        let pool = LanguageServerPool::new();

        // "capable" is Ready and advertises hover → NOT incapable.
        let capable =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("capable"))
                .await;
        capable.set_server_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
        pool.insert_connection(capable).await;

        // "lacking" is Ready with no hover capability → incapable (the one hit).
        let lacking =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lacking"))
                .await;
        lacking.set_server_capabilities(ServerCapabilities::default());
        pool.insert_connection(lacking).await;

        // "warming" is still Initializing → capability unknown, must be kept.
        let warming = create_handle_with_key(
            ConnectionState::Initializing,
            ConnectionKey::for_server("warming"),
        )
        .await;
        pool.insert_connection(warming).await;

        let candidates = HashSet::from([
            "capable", "lacking", "warming", "ghost", // no live connection
        ]);
        let incapable = pool
            .servers_known_incapable(&candidates, "textDocument/hover")
            .await;

        assert_eq!(
            incapable,
            HashSet::from(["lacking".to_string()]),
            "only a Ready connection that lacks the capability is known-incapable"
        );
    }

    /// A server counts as capable if ANY of its `(server, root)` connections
    /// advertises `method`, so a Ready-but-lacking instance alongside a capable
    /// one does not make the server incapable (across-roots caveat).
    #[tokio::test]
    async fn servers_known_incapable_capable_on_any_instance_wins() {
        use std::collections::HashSet;
        use tower_lsp_server::ls_types::{HoverProviderCapability, ServerCapabilities};

        let pool = LanguageServerPool::new();

        let root_a = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::new("srv", Some("/a".to_string())),
        )
        .await;
        root_a.set_server_capabilities(ServerCapabilities::default());
        pool.insert_connection(root_a).await;

        let root_b = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::new("srv", Some("/b".to_string())),
        )
        .await;
        root_b.set_server_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
        pool.insert_connection(root_b).await;

        let candidates = HashSet::from(["srv"]);
        let incapable = pool
            .servers_known_incapable(&candidates, "textDocument/hover")
            .await;

        assert!(
            incapable.is_empty(),
            "a capable instance makes the whole server capable across roots"
        );
    }

    /// A candidate whose only live connection is `Failed` or `Closing` — not
    /// past a successful init — is NOT returned: its capability is not
    /// known-absent (a `Failed` connection is evicted and respawned on the next
    /// request, which may then advertise the method). Pins the strict
    /// `state() == Ready` gate against a future widening (e.g. `!= Closed`) that
    /// would silently over-drop such a server (capability-prefilter-fanout).
    #[tokio::test]
    async fn servers_known_incapable_keeps_failed_and_closing_only() {
        use std::collections::HashSet;
        use tower_lsp_server::ls_types::ServerCapabilities;

        let pool = LanguageServerPool::new();

        let failed =
            create_handle_with_key(ConnectionState::Failed, ConnectionKey::for_server("failed"))
                .await;
        failed.set_server_capabilities(ServerCapabilities::default());
        pool.insert_connection(failed).await;

        let closing = create_handle_with_key(
            ConnectionState::Closing,
            ConnectionKey::for_server("closing"),
        )
        .await;
        closing.set_server_capabilities(ServerCapabilities::default());
        pool.insert_connection(closing).await;

        let candidates = HashSet::from(["failed", "closing"]);
        let incapable = pool
            .servers_known_incapable(&candidates, "textDocument/hover")
            .await;

        assert!(
            incapable.is_empty(),
            "only a Ready connection is known-incapable; Failed/Closing are kept"
        );
    }

    /// An empty candidate set short-circuits to empty without touching the pool.
    #[tokio::test]
    async fn servers_known_incapable_empty_candidates_returns_empty() {
        use std::collections::HashSet;
        let pool = LanguageServerPool::new();
        assert!(
            pool.servers_known_incapable(&HashSet::<&str>::new(), "textDocument/hover")
                .await
                .is_empty()
        );
    }

    /// Path c: `propagate_settings` re-stores each live connection's cell from the
    /// resolver, so a later `workspace/configuration` re-pull reflects the change.
    /// A connection whose resolved value is unchanged keeps its cell as-is.
    #[tokio::test]
    async fn propagate_settings_updates_changed_connections() {
        use serde_json::json;

        let pool = LanguageServerPool::new();

        // Two live connections: "rust-analyzer" gets new settings; "lua" already
        // holds its value and the resolver returns the same thing (unchanged).
        let ra = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("rust-analyzer"),
        )
        .await;
        let lua_value = Arc::new(json!({ "Lua": { "telemetry": { "enable": false } } }));
        let lua =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("lua")).await;
        ra.record_launch_config(&crate::config::settings::BridgeServerConfig {
            settings: Some(json!({ "not-retained": true })),
            ..Default::default()
        });
        lua.record_launch_config(&crate::config::settings::BridgeServerConfig::default());
        assert!(ra.launch_config().unwrap().settings.is_none());
        lua.store_settings(Some(Arc::clone(&lua_value)));
        {
            let mut conns = pool.connections().await;
            conns.insert(ConnectionKey::for_server("rust-analyzer"), Arc::clone(&ra));
            conns.insert(ConnectionKey::for_server("lua"), Arc::clone(&lua));
        }

        let ra_value = json!({ "rust-analyzer": { "cargo": { "features": "all" } } });
        let ra_value_for_resolve = ra_value.clone();
        let lua_value_for_resolve = (*lua_value).clone();
        let pushed = pool
            .propagate_settings(move |name| match name {
                "rust-analyzer" => Some(crate::config::settings::BridgeServerConfig {
                    settings: Some(ra_value_for_resolve.clone()),
                    ..Default::default()
                }),
                "lua" => Some(crate::config::settings::BridgeServerConfig {
                    settings: Some(lua_value_for_resolve.clone()),
                    ..Default::default()
                }),
                _ => None,
            })
            .await;

        assert_eq!(
            pushed, 1,
            "only the changed connection is pushed — the unchanged one does not storm"
        );
        assert_eq!(
            ra.current_settings().as_deref(),
            Some(&ra_value),
            "changed connection's cell is updated to the resolved value"
        );
        assert_eq!(
            lua.current_settings().as_deref(),
            Some(lua_value.as_ref()),
            "unchanged connection keeps its settings"
        );

        // Re-running with the same resolver now pushes nothing: both cells match.
        let lua_again = (*lua_value).clone();
        let ra_again = ra_value.clone();
        let pushed_again = pool
            .propagate_settings(move |name| match name {
                "rust-analyzer" => Some(crate::config::settings::BridgeServerConfig {
                    settings: Some(ra_again.clone()),
                    ..Default::default()
                }),
                "lua" => Some(crate::config::settings::BridgeServerConfig {
                    settings: Some(lua_again.clone()),
                    ..Default::default()
                }),
                _ => None,
            })
            .await;
        assert_eq!(pushed_again, 0, "an unchanged reload pushes nothing");
    }

    /// Path c does NOT notify a still-initializing connection (that would
    /// precede its `initialized` and break LSP ordering), but it DOES advance the
    /// cell so the post-`initialized` push (path a) carries the latest value.
    #[tokio::test]
    async fn propagate_settings_updates_but_does_not_notify_initializing_connection() {
        use serde_json::json;

        let pool = LanguageServerPool::new();
        let initializing = create_handle_with_key(
            ConnectionState::Initializing,
            ConnectionKey::for_server("rust-analyzer"),
        )
        .await;
        pool.connections().await.insert(
            ConnectionKey::for_server("rust-analyzer"),
            Arc::clone(&initializing),
        );

        let value = json!({ "rust-analyzer": { "cargo": { "features": "all" } } });
        let value_for_resolve = value.clone();
        let pushed = pool
            .propagate_settings(move |_| {
                Some(crate::config::settings::BridgeServerConfig {
                    settings: Some(value_for_resolve.clone()),
                    ..Default::default()
                })
            })
            .await;

        assert_eq!(pushed, 0, "an initializing connection is not notified");
        assert_eq!(
            initializing.current_settings().as_deref(),
            Some(&value),
            "but its cell is advanced so path (a) pushes the latest value",
        );
    }

    /// Path c with no live connections (e.g. initialize-time apply) is a clean
    /// no-op, not a panic.
    #[tokio::test]
    async fn propagate_settings_no_connections_is_noop() {
        let pool = LanguageServerPool::new();
        let pushed = pool
            .propagate_settings(|_| {
                Some(crate::config::settings::BridgeServerConfig {
                    settings: Some(serde_json::json!({ "x": 1 })),
                    ..Default::default()
                })
            })
            .await;
        assert_eq!(pushed, 0, "no live connections → nothing pushed");
    }

    #[test]
    fn launch_config_compares_boolean_defaults_by_effective_value() {
        let inherited = crate::config::settings::BridgeServerConfig::default();
        let explicit = crate::config::settings::BridgeServerConfig {
            prefer_shared_instance: Some(false),
            enabled: Some(true),
            ..Default::default()
        };
        assert!(same_launch_config(&inherited, &explicit));
    }

    #[tokio::test]
    async fn propagate_settings_invalidates_changed_and_removed_launch_configs() {
        use crate::config::settings::BridgeServerConfig;

        let pool = LanguageServerPool::new();
        let changed_key = ConnectionKey::for_server("changed");
        let removed_key = ConnectionKey::for_server("removed");
        let unchanged_key = ConnectionKey::for_server("unchanged");
        let changed =
            create_handle_with_key(ConnectionState::Initializing, changed_key.clone()).await;
        let removed = create_handle_with_key(ConnectionState::Ready, removed_key.clone()).await;
        let unchanged = create_handle_with_key(ConnectionState::Ready, unchanged_key.clone()).await;

        let old_changed = BridgeServerConfig {
            cmd: vec!["old-server".into()],
            ..Default::default()
        };
        let old_removed = BridgeServerConfig {
            cmd: vec!["removed-server".into()],
            ..Default::default()
        };
        let stable = BridgeServerConfig {
            cmd: vec!["stable-server".into()],
            ..Default::default()
        };
        changed.record_launch_config(&old_changed);
        removed.record_launch_config(&old_removed);
        unchanged.record_launch_config(&stable);
        {
            let mut connections = pool.connections().await;
            connections.insert(changed_key.clone(), Arc::clone(&changed));
            connections.insert(removed_key.clone(), Arc::clone(&removed));
            connections.insert(unchanged_key.clone(), Arc::clone(&unchanged));
        }

        let stable_for_resolve = stable.clone();
        pool.propagate_settings(move |name| match name {
            "changed" => Some(BridgeServerConfig {
                cmd: vec!["new-server".into()],
                ..Default::default()
            }),
            "removed" => None,
            "unchanged" => Some(stable_for_resolve.clone()),
            _ => unreachable!(),
        })
        .await;

        let connections = pool.connections().await;
        assert!(!connections.contains_key(&changed_key));
        assert!(!connections.contains_key(&removed_key));
        assert!(connections.contains_key(&unchanged_key));
        assert!(matches!(
            changed.state(),
            ConnectionState::Closing | ConnectionState::Closed
        ));
        assert!(
            !changed.transition_initializing_to_ready(),
            "a completing handshake must not resurrect an invalidated connection"
        );
        assert!(matches!(
            removed.state(),
            ConnectionState::Closing | ConnectionState::Closed
        ));
        assert_eq!(unchanged.state(), ConnectionState::Ready);
    }
}
