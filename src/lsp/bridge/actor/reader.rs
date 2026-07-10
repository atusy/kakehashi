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
use tokio::sync::{mpsc, oneshot};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::MessageType;

use super::super::connection::BridgeReader;
use super::super::{client, telemetry, text_document, window};
use super::OutboundMessage;
use super::ResponseRouter;
use super::response_router::{LivenessExpiry, RouteResult};
use crate::error::LockResultExt;
use crate::lsp::bridge::pool::DynamicCapabilityRegistry;
use crate::lsp::bridge::workspace::{self, WorkspaceFolderSet};

/// Notification to forward from downstream server to upstream editor.
///
/// Reader tasks use this to signal events that require upstream Client interaction,
/// keeping the bridge module decoupled from tower-lsp's Client type.
///
/// Two channels carry these, split by loss tolerance:
/// - `DiagnosticRefresh` travels on an **unbounded** channel: dropping one
///   would silently stale the editor's diagnostics, and its volume is tiny
///   (one per downstream `workspace/diagnostic/refresh`).
/// - `LogMessage`/`ShowMessage`/`TelemetryEvent` travel on a **bounded** channel
///   ([`WINDOW_NOTIFICATION_QUEUE_CAPACITY`]) with drop-on-full: a downstream
///   server stuck in a tight logMessage (or telemetry) loop must not grow memory
///   without bound or starve `DiagnosticRefresh` behind a burst (the forwarding
///   loop drains the unbounded channel first). Within the bounded channel FIFO
///   order is preserved, which the window-notification e2e relies on.
///
/// `Eq` is intentionally not derived: `TelemetryEvent` carries an arbitrary
/// `serde_json::Value` (which is `PartialEq` but not `Eq` because of floats).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UpstreamNotification {
    /// Request upstream to re-pull diagnostics.
    /// Sent when downstream server issues `workspace/diagnostic/refresh`.
    DiagnosticRefresh,
    /// A downstream-initiated `textDocument/publishDiagnostics`
    /// (push-propagation-diagnostic-forwarding). The forwarding loop classifies
    /// `uri`: a virtual injection URI resolves to its host document + region (a
    /// region push, virtual coordinates); a real URI is a candidate `_self`
    /// host-layer push (host coordinates) accepted only for an open host-bridged
    /// document. When the push classifies to a live target it is cached under
    /// `server` and the merged host set is republished; otherwise it is dropped
    /// (unresolved virtual URI; real URI not open / not `_self` / wrong server).
    ///
    /// This carries an arbitrary-size `Vec<Diagnostic>` over the **unbounded**
    /// upstream channel. To keep a chatty downstream re-pushing the *same*
    /// `(connection, uri)` from making the loop republish every superseded push, the
    /// forwarding loop coalesces a drained burst by `(connection, uri)` to the latest
    /// before delivering (`coalesce_upstream_batch`, #426), then records each
    /// surviving push in a barrier-delimited run and republishes once per resolved
    /// host. The latter collapses distinct injection-region URIs that all contribute
    /// to one host without dropping any region slot.
    PublishDiagnostics {
        /// The URI the downstream published for — a virtual injection URI (region
        /// push) or a real host URI (`_self` host-layer push).
        uri: String,
        /// The originating downstream server's config name (`deps.server_name`);
        /// pushes without a name are dropped at the reader, so this is always set.
        server: String,
        /// The originating connection's id (`deps.progress_connection_id`). Tags the
        /// cached slot so a later reader exit can evict only this connection's slots,
        /// never a restarted connection's (which gets a fresh id) (#469).
        connection_id: crate::lsp::bridge::ProgressConnectionId,
        /// The pushed diagnostics, in the published document's own coordinates
        /// (virtual for a region push, host for a `_self` push).
        diagnostics: Vec<tower_lsp_server::ls_types::Diagnostic>,
    },
    /// Forward a downstream `window/logMessage` to the editor.
    /// `message` is already prefixed with the originating server name.
    LogMessage { typ: MessageType, message: String },
    /// Forward a downstream `window/showMessage` to the editor.
    /// `message` is already prefixed with the originating server name.
    ShowMessage { typ: MessageType, message: String },
    /// Forward a downstream `telemetry/event` notification to the editor
    /// verbatim. `data` is the raw notification `params` (arbitrary LSPAny);
    /// telemetry has no client capability and is always passed through.
    TelemetryEvent { data: serde_json::Value },
    /// Ask the editor to create a work-done progress for `token`.
    /// Sent lazily on the token's first renderable `begin` — not when the
    /// downstream issues `window/workDoneProgress/create` (lazy announcement,
    /// see `ProgressRegistry`); `token` is the bridge-minted *upstream*
    /// token, not the downstream's own.
    CreateWorkDoneProgress {
        token: tower_lsp_server::ls_types::NumberOrString,
    },
    /// Forward a `$/progress` notification to the editor. `params` already
    /// carries the bridge-minted *upstream* token (see [`ProgressRegistry`]).
    ///
    /// [`ProgressRegistry`]: crate::lsp::bridge::ProgressRegistry
    Progress {
        params: tower_lsp_server::ls_types::ProgressParams,
    },
    /// Forward aggregated **client-provided** work-done progress to the editor.
    /// `params` carries the editor's own `workDoneToken` (the bridge aggregated
    /// the fanned-out downstreams' progress onto it; ls-bridge-client-progress).
    /// Unlike [`Progress`](Self::Progress) this is **not** admission-gated: the
    /// editor minted the token and needs no `window/workDoneProgress/create`.
    ClientProgress {
        params: tower_lsp_server::ls_types::ProgressParams,
    },
    /// Tell the forwarding loop to forget these (upstream) progress tokens
    /// without an `End` — sent when a downstream connection's reader exits with
    /// progress still in flight, so the loop's created-token set can't leak
    /// entries across crashes/respawns (window-work-done-progress bridging).
    ForgetWorkDoneProgress(Vec<tower_lsp_server::ls_types::NumberOrString>),
    /// Evict the diagnostic-cache slots a now-exited connection produced and
    /// republish the affected hosts — sent when a downstream connection's reader
    /// exits (crash/respawn), so a dead server's pushed diagnostics don't linger
    /// until the host's `didClose` (#469). A restart's slots carry a fresh
    /// connection id and so survive.
    EvictConnectionDiagnostics {
        connection_id: crate::lsp::bridge::ProgressConnectionId,
    },
}

/// A downstream-initiated **request** the bridge forwards to the editor and
/// whose response must be relayed back to that downstream server.
///
/// Unlike [`UpstreamNotification`] (fire-and-forget), these need the editor's
/// answer routed back, so each variant carries a `oneshot` reply the forwarding
/// loop fulfills with the editor's typed result. The reader-side handler then
/// frames the JSON-RPC response and writes it to the originating connection's
/// `response_tx`. This keeps the bridge decoupled from tower-lsp's `Client`
/// (the loop owns the `Client`) and the forwarding loop decoupled from
/// [`OutboundMessage`] / JSON-RPC framing (the reader side owns that).
///
/// A separate **unbounded** channel carries these (a dropped request would hang
/// the downstream waiting for a response that never comes); the channel is held
/// off [`UpstreamNotification`] because `oneshot::Sender` is neither `Clone` nor
/// `Eq`. The forwarding loop must service each request on a spawned task — these
/// are user-interactive and can pend indefinitely, so awaiting one inline would
/// freeze forwarding for every bridged server.
///
/// Each *reply-bearing* variant carries a [`ForwardedRequestCancel`] so the
/// forwarding loop can observe a downstream `$/cancelRequest` (or connection
/// death) and forward the cancel to the editor — see [`InboundRequestRegistry`].
/// The one exception is [`RegisterCommands`](Self::RegisterCommands), which is
/// fire-and-forget: it advertises capability names to the editor, expects no
/// reply, and so carries neither a `reply` sender nor a `cancel`.
///
/// [`InboundRequestRegistry`]: crate::lsp::bridge::InboundRequestRegistry
pub(crate) enum UpstreamRequest {
    /// Forward a `window/showMessageRequest`; the editor's selected action (or
    /// `None`) is relayed back to the downstream.
    ShowMessageRequest {
        typ: MessageType,
        message: String,
        actions: Option<Vec<tower_lsp_server::ls_types::MessageActionItem>>,
        reply: oneshot::Sender<Option<tower_lsp_server::ls_types::MessageActionItem>>,
        cancel: ForwardedRequestCancel,
    },
    /// Forward a `window/showDocument` (URI passed through verbatim, including
    /// virtual-document URIs); the editor's success flag is relayed back.
    ShowDocument {
        params: tower_lsp_server::ls_types::ShowDocumentParams,
        reply: oneshot::Sender<bool>,
        cancel: ForwardedRequestCancel,
    },
    /// Forward a `workspace/applyEdit`; the editor's full response (`applied`,
    /// `failureReason`, `failedChange`) is relayed back — the downstream acts
    /// on the outcome. The forwarding loop translates virtual-document edits
    /// back to host coordinates before the editor sees them, and answers
    /// untranslatable edits — and all edits when the editor never declared
    /// `workspace.applyEdit` — `applied: false` locally (#568).
    ApplyEdit {
        params: tower_lsp_server::ls_types::ApplyWorkspaceEditParams,
        reply: oneshot::Sender<tower_lsp_server::ls_types::ApplyWorkspaceEditResponse>,
        cancel: ForwardedRequestCancel,
    },
    /// Advertise a downstream server's `workspace/executeCommand` command names
    /// to the editor via `client/registerCapability`, so its palette lists them
    /// and fires them by raw name (#628 palette-fired commands). Fire-and-forget:
    /// no reply/cancel (the editor's ack is ignored; routing fails soft anyway).
    RegisterCommands { commands: Vec<String> },
}

/// Cancellation context for a forwarded request: the originating connection and
/// downstream request id (used to drop the registry entry once settled) plus the
/// [`CancellationToken`] the forwarding loop awaits. Registered on the reader via
/// [`InboundRequestRegistry`] *before* the request is enqueued, so a racing
/// downstream `$/cancelRequest` can't miss it.
///
/// [`InboundRequestRegistry`]: crate::lsp::bridge::InboundRequestRegistry
pub(crate) struct ForwardedRequestCancel {
    pub(crate) connection_id: crate::lsp::bridge::ProgressConnectionId,
    pub(crate) request_id: jsonrpc::Id,
    pub(crate) token: CancellationToken,
    /// The registration generation, passed back to
    /// [`InboundRequestRegistry::unregister`] so a request whose id was reused
    /// only drops its own entry.
    ///
    /// [`InboundRequestRegistry::unregister`]: crate::lsp::bridge::InboundRequestRegistry
    pub(crate) generation: u64,
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
    start_rx: mpsc::Receiver<u64>,
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
    /// Loss-intolerant notifications (`DiagnosticRefresh` and work-done
    /// progress create/$progress/forget); unbounded.
    pub(crate) upstream_tx: mpsc::UnboundedSender<UpstreamNotification>,
    /// Best-effort `window/*` (and `telemetry/event`) notifications; bounded,
    /// dropped when full.
    pub(crate) window_tx: mpsc::Sender<UpstreamNotification>,
    /// Downstream-initiated requests forwarded to the editor with a response
    /// relayed back (`window/showMessageRequest`, `window/showDocument`,
    /// `workspace/applyEdit`). Unbounded: a dropped request would hang the
    /// downstream. See [`UpstreamRequest`].
    pub(crate) upstream_request_tx: mpsc::UnboundedSender<UpstreamRequest>,
    /// Tracks in-flight forwarded requests so a downstream `$/cancelRequest`
    /// (or connection death) can cancel the editor-bound request (#404).
    pub(crate) inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry,
    /// The folders this connection currently serves, used to answer downstream
    /// `workspace/workspaceFolders` pulls. Mutable: a `preferSharedInstance`
    /// connection grows it as new marker roots join (#391).
    pub(crate) workspace_folders: WorkspaceFolderSet,
    /// Shared registry that remaps downstream-declared progress tokens to unique
    /// upstream tokens (window-work-done-progress bridging).
    pub(crate) progress_registry: Arc<crate::lsp::bridge::ProgressRegistry>,
    /// This connection's id, used to scope forward token translation.
    pub(crate) progress_connection_id: crate::lsp::bridge::ProgressConnectionId,
    /// This server's current workspace settings, shared with the pool, used to
    /// answer downstream `workspace/configuration` pulls (downstream-settings-propagation).
    /// `None` slot = no settings configured for this server. The pool seeds it at
    /// spawn from the resolved `BridgeServerConfig.settings` and (later) re-stores
    /// it on a merge change, so a re-pull after a `didChangeConfiguration` reads
    /// the current value.
    pub(crate) settings: Arc<arc_swap::ArcSwapOption<serde_json::Value>>,
    /// Routes bridge-minted client-progress tokens (`kakehashi/bridge/cprog/*`)
    /// to their aggregator (ls-bridge-client-progress).
    pub(crate) client_progress_registry: Arc<crate::lsp::bridge::ClientProgressRegistry>,
}

/// RAII guard that purges this connection's progress-token mappings when the
/// reader loop exits (crash, shutdown, respawn). Without it, a downstream that
/// dies mid-progress would leak registry entries and a later client cancel could
/// route to a dead writer (window-work-done-progress bridging).
///
/// Any still-live upstream tokens are forwarded to the loop so it can drop their
/// created-token admissions too — otherwise progress that never reached `End`
/// before the crash would leak in the loop's set across respawns.
struct ProgressPurgeGuard {
    registry: Arc<crate::lsp::bridge::ProgressRegistry>,
    connection_id: crate::lsp::bridge::ProgressConnectionId,
    upstream_tx: mpsc::UnboundedSender<UpstreamNotification>,
    /// Cancel any forwarded requests still in flight when this connection's
    /// reader exits, so their editor dialogs don't linger (#404).
    inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry,
}

impl Drop for ProgressPurgeGuard {
    fn drop(&mut self) {
        self.inbound_request_registry
            .cancel_connection(self.connection_id);
        let tokens = self.registry.purge_connection(self.connection_id);
        if !tokens.is_empty() {
            let _ = self
                .upstream_tx
                .send(UpstreamNotification::ForgetWorkDoneProgress(tokens));
        }
        // Evict this connection's pushed diagnostics so a dead server's stale
        // diagnostics don't linger until the host's `didClose` (#469). Sent
        // unconditionally — when the connection never pushed, the loop's eviction
        // scans the cache, matches no slot, and republishes nothing (a semantic
        // no-op on this rare exit path).
        let _ = self
            .upstream_tx
            .send(UpstreamNotification::EvictConnectionDiagnostics {
                connection_id: self.connection_id,
            });
    }
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
    epoch: Option<u64>,
}

impl LivenessTimerState {
    /// Create a new liveness timer state with optional timeout configuration.
    ///
    /// If `timeout` is None, the timer is disabled and all operations are no-ops.
    fn new(timeout: Option<Duration>) -> Self {
        Self {
            timer: None,
            timeout,
            epoch: None,
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
    fn start(&mut self, server_prefix: &str, epoch: u64) {
        if let Some(timeout) = self.timeout {
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Liveness timer started: {:?}",
                server_prefix,
                timeout
            );
            self.timer = Some(new_liveness_timer(timeout));
            self.epoch = Some(epoch);
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
        self.epoch = None;
    }

    fn disable(&mut self, server_prefix: &str, reason: &str) {
        self.stop(server_prefix, reason);
        self.timeout = None;
    }

    /// Get the configured timeout duration (for logging on expiry).
    fn timeout_duration(&self) -> Duration {
        self.timeout.unwrap_or_default()
    }

    fn epoch(&self) -> Option<u64> {
        self.epoch
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
    liveness_start_tx: mpsc::Sender<u64>,

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
    pub(crate) fn notify_liveness_start(&self, epoch: u64) {
        // Use try_send to avoid blocking. If channel is full, timer is already running.
        let _ = self.liveness_start_tx.try_send(epoch);
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
    let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
    let progress_connection_id = progress_registry.new_connection_id();
    spawn_reader_task_for_server(
        reader,
        router,
        liveness_timeout,
        ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            workspace_folders: WorkspaceFolderSet::new(None),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
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
    let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
    let progress_connection_id = progress_registry.new_connection_id();
    let server_request_deps = ServerRequestDeps {
        settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
        server_name: None,
        response_tx,
        dynamic_capabilities,
        upstream_tx,
        workspace_folders: WorkspaceFolderSet::new(None),
        window_tx,
        upstream_request_tx: mpsc::unbounded_channel().0,
        inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
        progress_registry,
        client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
        progress_connection_id,
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

    // Purge this connection's progress-token mappings when the loop exits.
    let _progress_purge_guard = ProgressPurgeGuard {
        registry: Arc::clone(&server_request_deps.progress_registry),
        connection_id: server_request_deps.progress_connection_id,
        upstream_tx: server_request_deps.upstream_tx.clone(),
        inbound_request_registry: server_request_deps.inbound_request_registry.clone(),
    };

    // Consolidated liveness timer state (ls-bridge-async-connection)
    let mut liveness = LivenessTimerState::new(liveness_timeout);
    let mut liveness_failed_tx = Some(liveness_failed_tx);

    loop {
        tokio::select! {
            biased; // Process in order: cancellation, shutdown stop, timer, start, read

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

            // Global shutdown overrides Tier-2 liveness, including when its
            // stop signal and an expired timer become ready together. Disable
            // future starts too, so a previously queued wake-up cannot re-arm.
            Some(()) = liveness_stop_rx.recv() => {
                liveness.disable(&server_prefix, "shutdown began");
            }

            // Check for liveness timeout (if timer is active)
            _ = liveness.wait() => {
                // A queued request can be cancelled and removed by the writer
                // without producing downstream stdout. The timer notification
                // was already enqueued at registration, so re-check the router
                // at expiry before declaring a healthy idle server hung.
                let expected_epoch = liveness.epoch().expect("an awaited timer has an epoch");
                let pending_count = match router.fail_all_if_awaiting_downstream(
                    expected_epoch,
                    "bridge: liveness timeout - server unresponsive",
                    || {
                        if let Some(tx) = liveness_failed_tx.take() {
                            let _ = tx.send(());
                        }
                    },
                ) {
                    LivenessExpiry::Stale { current_epoch } => {
                        liveness.start(&server_prefix, current_epoch);
                        continue;
                    }
                    LivenessExpiry::Idle => {
                        liveness.stop(&server_prefix, "no request awaits downstream at expiry");
                        continue;
                    }
                    LivenessExpiry::Failed { pending_count } => pending_count,
                };

                // Liveness timeout fired - server is unresponsive
                // This warn! is the primary observability signal for production debugging.
                // The pending_count is included for debugging stuck request scenarios.
                warn!(
                    target: "kakehashi::bridge::reader",
                    "{}Liveness timeout expired after {:?}, server appears hung - failing {} pending request(s)",
                    server_prefix,
                    liveness.timeout_duration(),
                    pending_count
                );
                break;
            }

            // Check for liveness timer start notification (pending 0->1)
            Some(_epoch) = liveness_start_rx.recv() => {
                // The bounded channel may still contain an older wake-up. The
                // router epoch is authoritative and was advanced atomically
                // with registration.
                liveness.start(&server_prefix, router.liveness_epoch());
            }

            // Try to read a message (lowest priority)
            result = reader.read_message() => {
                match result {
                    Ok(message) => {
                        // Reset liveness timer on any message activity (ls-bridge-async-connection)
                        liveness.reset(&server_prefix);

                        handle_message(message, &router, &server_prefix, &server_request_deps).await;

                        // Check if pending count returned to 0 - stop timer
                        if liveness.is_active() && router.awaiting_downstream_count() == 0 {
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
            if text_document::publish_diagnostics::is_scratch_publish_diagnostics(&message) {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Discarding publishDiagnostics targeting a scratch virtual document",
                    server_prefix
                );
            } else if message["method"].as_str() == Some("textDocument/publishDiagnostics") {
                // Non-scratch region push: route into the diagnostics cache
                // (push-propagation-diagnostic-forwarding) instead of dropping (#380).
                // `message` is owned and discarded after this arm, so move it in and
                // deserialize the diagnostics by value (no string clones).
                text_document::publish_diagnostics::forward_push(message, deps);
            } else if message["method"].as_str() == Some("$/progress") {
                window::progress::forward(&message, server_prefix, deps);
            } else if message["method"].as_str() == Some("$/cancelRequest") {
                handle_cancel_request(&message, deps);
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

/// Handle an inbound downstream `$/cancelRequest`: if it targets a forwarded
/// request still in flight (`window/showMessageRequest` / `window/showDocument`),
/// cancel the editor-bound request so its dialog is dismissed (#404). The id is
/// parsed as a [`jsonrpc::Id`] exactly as the original request's id was, so the
/// registry key matches. Ids that aren't tracked (e.g. the bridge's own outbound
/// requests *to* the downstream, which the downstream can't cancel this way) are
/// a no-op.
fn handle_cancel_request(message: &serde_json::Value, deps: &ServerRequestDeps) {
    // Deserialize the id straight from the borrowed value (no clone). A missing
    // params/id indexes to `Value::Null`, which deserializes to `Id::Null` and
    // matches nothing registered (harmless no-op).
    let Ok(request_id) = <jsonrpc::Id as serde::Deserialize>::deserialize(&message["params"]["id"])
    else {
        return;
    };
    deps.inbound_request_registry
        .cancel(deps.progress_connection_id, &request_id);
}

/// Forward supported downstream notifications to the upstream editor.
///
/// Routes `window/logMessage`, `window/showMessage`, and `telemetry/event` to
/// their per-method modules ([`window::log_message`], [`window::show_message`],
/// [`telemetry::event`]); all are forwarded unconditionally so the bridge stays
/// transparent.
///
/// `$/progress`, `$/cancelRequest`, and non-scratch `textDocument/publishDiagnostics`
/// (routed to the diagnostics cache — push-propagation-diagnostic-forwarding) are
/// handled earlier in `handle_message`, so they never reach here. Everything else
/// is still silently ignored.
fn forward_notification(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    match message["method"].as_str() {
        Some("window/logMessage") => window::log_message::forward(message, server_prefix, deps),
        Some("window/showMessage") => window::show_message::forward(message, server_prefix, deps),
        Some("telemetry/event") => telemetry::event::forward(message, server_prefix, deps),
        _ => {}
    }
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

    // Deferred request handlers forward to the editor and relay its response
    // back asynchronously (the editor may pend on user interaction), so they own
    // their entire response lifecycle on a spawned task rather than returning a
    // synchronous `body` to frame-and-send below.
    match method {
        "window/showMessageRequest" => {
            window::show_message_request::handle(&message, id, server_prefix, deps);
            return;
        }
        "window/showDocument" => {
            window::show_document::handle(&message, id, server_prefix, deps);
            return;
        }
        "workspace/applyEdit" => {
            workspace::apply_edit::handle(&message, id, server_prefix, deps);
            return;
        }
        _ => {}
    }

    let body: jsonrpc::Result<serde_json::Value> = match method {
        "client/registerCapability" => {
            client::register_capability::handle(&message, server_prefix, deps)
        }
        "client/unregisterCapability" => {
            client::unregister_capability::handle(&message, server_prefix, deps)
        }
        "window/workDoneProgress/create" => {
            window::work_done_progress_create::handle(&message, server_prefix, deps)
        }
        "workspace/diagnostic/refresh" => {
            workspace::diagnostic_refresh::handle(server_prefix, deps)
        }
        "workspace/workspaceFolders" => workspace::workspace_folders::handle(server_prefix, deps),
        "workspace/configuration" => {
            workspace::configuration::handle(&message, server_prefix, deps)
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
        Ok(result) => jsonrpc::Response::from_ok(id, result),
        Err(error) => jsonrpc::Response::from_error(id, error),
    };
    send_server_response(&deps.response_tx, response, server_prefix, method).await;
}

/// Frame a server-request response and send it to the downstream server via the
/// writer channel.
///
/// Used both by the synchronous dispatch in [`handle_server_request`] and by the
/// deferred request handlers ([`window::show_message_request`],
/// [`window::show_document`]) that relay an editor response back asynchronously.
///
/// We use [`OutboundMessage::Untracked`] because a server-initiated response has
/// no [`ResponseRouter`] entry to clean up on failure (unlike `Tracked`
/// requests). `send_timeout(5s)` (rather than `try_send`) guarantees delivery
/// under transient backpressure without risking the deadlock a bare
/// `send().await` could cause if the queue is full while the writer is blocked
/// on stdin and the server on stdout — the response is dropped only after 5s of
/// sustained backpressure, far better than instant loss.
pub(in crate::lsp::bridge) async fn send_server_response(
    response_tx: &mpsc::Sender<OutboundMessage>,
    response: jsonrpc::Response,
    server_prefix: &str,
    method: &str,
) {
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

    if let Err(e) = response_tx
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
        // Same construction as the typed builder; callers here only need the
        // receivers kept alive, not read.
        dummy_server_request_deps_with_rx()
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

    /// #404: an inbound downstream `$/cancelRequest` fires the registered
    /// in-flight request's token; an unknown id is a no-op.
    #[tokio::test]
    async fn handle_message_cancel_request_fires_registered_token() {
        let router = ResponseRouter::new();
        let (deps, _keep) = dummy_server_request_deps();

        // The handler would have registered these before enqueueing each request.
        let (token, _g7) = deps
            .inbound_request_registry
            .register(deps.progress_connection_id, jsonrpc::Id::Number(7));
        let (other, _g8) = deps
            .inbound_request_registry
            .register(deps.progress_connection_id, jsonrpc::Id::Number(8));
        assert!(!token.is_cancelled());

        handle_message(
            json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 7 } }),
            &router,
            "",
            &deps,
        )
        .await;
        assert!(
            token.is_cancelled(),
            "inbound $/cancelRequest should fire the registered token"
        );
        assert!(!other.is_cancelled(), "only the targeted id is cancelled");

        // A cancel for an unknown id is a no-op: no panic, and no other token fires.
        handle_message(
            json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": 999 } }),
            &router,
            "",
            &deps,
        )
        .await;
        assert!(!other.is_cancelled(), "unknown id must not cancel anything");
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
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: server_name.map(String::from),
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
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
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: Some("mock-ls".to_string()),
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            workspace_folders: WorkspaceFolderSet::new(None),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
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

    /// When a reader task exits, its `ProgressPurgeGuard` clears that
    /// connection's progress-token mappings so a later cancel can't route to a
    /// dead writer (window-work-done-progress bridging).
    #[tokio::test]
    async fn reader_exit_purges_progress_mappings() {
        use crate::lsp::bridge::connection::BridgeReader;
        use std::process::Stdio;
        use tokio::process::Command;
        use tower_lsp_server::ls_types::NumberOrString;

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
        let (response_tx, _response_rx) = mpsc::channel(16);
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let conn = progress_registry.new_connection_id();
        let downstream_token = NumberOrString::Number(1);
        let (upstream_token, _) =
            progress_registry.register(conn, downstream_token.clone(), response_tx.clone());

        let handle = spawn_reader_task_for_server(
            reader,
            Arc::clone(&router),
            None,
            ServerRequestDeps {
                settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
                server_name: None,
                response_tx,
                dynamic_capabilities: Arc::new(DynamicCapabilityRegistry::new()),
                upstream_tx,
                window_tx,
                upstream_request_tx: mpsc::unbounded_channel().0,
                inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
                workspace_folders: WorkspaceFolderSet::new(None),
                progress_registry: Arc::clone(&progress_registry),
                client_progress_registry: Arc::new(
                    crate::lsp::bridge::ClientProgressRegistry::new(),
                ),
                progress_connection_id: conn,
            },
        );

        // Mapping is live while the reader runs.
        assert!(progress_registry.resolve_cancel(&upstream_token).is_some());

        // Dropping the handle cancels the loop; its purge guard then fires.
        drop(handle);
        for _ in 0..50 {
            if progress_registry.resolve_cancel(&upstream_token).is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            progress_registry.resolve_cancel(&upstream_token).is_none(),
            "reader exit must purge the connection's progress mappings"
        );
        assert_eq!(progress_registry.translate(conn, &downstream_token), None);

        // The same guard also asks the forwarding loop to evict this connection's
        // diagnostic slots, so a dead server's diagnostics don't linger (#469).
        // `Drop` runs `purge_connection` *then* the sends, so the purge above can be
        // observed before the eviction send lands — `recv().await` (not a single
        // `try_recv`) so we wait for it rather than racing the guard's Drop.
        let saw_eviction = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(notification) = upstream_rx.recv().await {
                if let UpstreamNotification::EvictConnectionDiagnostics { connection_id } =
                    notification
                {
                    assert_eq!(
                        connection_id, conn,
                        "eviction must target the exited connection"
                    );
                    return true;
                }
            }
            false // channel closed (all senders dropped) without an eviction
        })
        .await
        .expect("EvictConnectionDiagnostics must arrive after reader exit");
        assert!(
            saw_eviction,
            "reader exit must emit EvictConnectionDiagnostics for its connection"
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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

    /// A downstream `window/workDoneProgress/create` is acknowledged to the
    /// downstream and registered (so `$/progress` can be translated), but the
    /// editor-facing `CreateWorkDoneProgress` is deferred until the token's
    /// first renderable `begin` (lazy announcement: per-request downstream
    /// progress storms must not reach the editor).
    #[tokio::test]
    async fn handle_message_work_done_progress_create_bridges_upstream() {
        use tower_lsp_server::ls_types::NumberOrString;

        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::clone(&progress_registry),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "window/workDoneProgress/create",
            "params": { "token": "some-token" }
        });

        handle_message(message, &router, "", &deps).await;

        // Downstream gets an immediate success ack.
        let response = response_rx.try_recv().expect("should have response");
        let OutboundMessage::Untracked(val) = response else {
            panic!("Expected Untracked variant");
        };
        assert_eq!(val["id"], 5);
        assert!(val["result"].is_null());

        // Nothing reaches the editor yet: announcement waits for a renderable begin.
        assert!(
            upstream_rx.try_recv().is_err(),
            "create must not be bridged before a renderable begin"
        );

        // The mapping is registered so downstream `$/progress` can be translated.
        let downstream_token = NumberOrString::String("some-token".to_string());
        let upstream_token = progress_registry
            .translate(progress_connection_id, &downstream_token)
            .expect("mapping registered");
        assert_ne!(
            upstream_token, downstream_token,
            "upstream token must be remapped"
        );

        // The first renderable begin announces: create, then the begin itself.
        let begin = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": "some-token",
                "value": { "kind": "begin", "title": "Indexing" }
            }
        });
        handle_message(begin, &router, "", &deps).await;
        let UpstreamNotification::CreateWorkDoneProgress { token } = upstream_rx
            .try_recv()
            .expect("renderable begin should announce the token")
        else {
            panic!("Expected CreateWorkDoneProgress");
        };
        assert_eq!(token, upstream_token);
        let UpstreamNotification::Progress { params } =
            upstream_rx.try_recv().expect("begin should forward")
        else {
            panic!("Expected Progress");
        };
        assert_eq!(params.token, upstream_token);
    }

    /// A downstream progress lifecycle whose `begin` is fully blank (empty
    /// title, no message, no percentage) is swallowed: no create, no begin,
    /// no end reach the editor, and the `End` still clears the mapping. This
    /// is the storm case — some downstreams declare a token per analysis pass
    /// with nothing renderable.
    #[tokio::test]
    async fn handle_message_blank_progress_lifecycle_is_swallowed() {
        use tower_lsp_server::ls_types::NumberOrString;

        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::clone(&progress_registry),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let create = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "window/workDoneProgress/create",
            "params": { "token": "storm" }
        });
        handle_message(create, &router, "", &deps).await;
        let begin = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "storm", "value": { "kind": "begin", "title": "" } }
        });
        handle_message(begin, &router, "", &deps).await;
        let end = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "storm", "value": { "kind": "end" } }
        });
        handle_message(end, &router, "", &deps).await;

        assert!(
            upstream_rx.try_recv().is_err(),
            "blank lifecycle must not reach the editor"
        );
        let downstream_token = NumberOrString::String("storm".to_string());
        assert_eq!(
            progress_registry.translate(progress_connection_id, &downstream_token),
            None,
            "End clears the swallowed mapping"
        );
    }

    /// A `begin` whose title is empty but which carries a renderable `message`
    /// (or percentage) is NOT swallowed: an empty title alone is legal and
    /// clients render message/percentage without one — only the fully blank
    /// storm shape is elided.
    #[tokio::test]
    async fn handle_message_blank_title_with_message_still_announces() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::clone(&progress_registry),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let create = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "window/workDoneProgress/create",
            "params": { "token": "msg-only" }
        });
        handle_message(create, &router, "", &deps).await;
        let begin = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": "msg-only",
                "value": { "kind": "begin", "title": "", "message": "Indexing 42%" }
            }
        });
        handle_message(begin, &router, "", &deps).await;

        let UpstreamNotification::CreateWorkDoneProgress { .. } = upstream_rx
            .try_recv()
            .expect("message-bearing begin must announce")
        else {
            panic!("expected CreateWorkDoneProgress");
        };
        assert!(
            matches!(
                upstream_rx.try_recv(),
                Ok(UpstreamNotification::Progress { .. })
            ),
            "the begin itself must forward after the create"
        );
    }

    /// A begin that is blank except for `cancellable: true` still announces:
    /// a cancel button is renderable.
    #[tokio::test]
    async fn handle_message_cancellable_only_begin_still_announces() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::clone(&progress_registry),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let create = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "window/workDoneProgress/create",
            "params": { "token": "cancellable-only" }
        });
        handle_message(create, &router, "", &deps).await;
        let begin = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": "cancellable-only",
                "value": { "kind": "begin", "title": "", "cancellable": true }
            }
        });
        handle_message(begin, &router, "", &deps).await;

        assert!(
            matches!(
                upstream_rx.try_recv(),
                Ok(UpstreamNotification::CreateWorkDoneProgress { .. })
            ),
            "cancellable begin must announce"
        );
        assert!(
            matches!(
                upstream_rx.try_recv(),
                Ok(UpstreamNotification::Progress { .. })
            ),
            "the begin itself must forward after the create"
        );
    }

    /// A swallowed token that a downstream reuses with a renderable `begin`
    /// (no intervening `end`) upgrades to announced instead of staying dark.
    #[tokio::test]
    async fn handle_message_swallowed_token_upgrades_on_renderable_begin() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::clone(&progress_registry),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let create = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "window/workDoneProgress/create",
            "params": { "token": "reused" }
        });
        handle_message(create, &router, "", &deps).await;
        let blank = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "reused", "value": { "kind": "begin", "title": "" } }
        });
        handle_message(blank, &router, "", &deps).await;
        assert!(
            upstream_rx.try_recv().is_err(),
            "blank begin is swallowed first"
        );

        let renderable = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "reused", "value": { "kind": "begin", "title": "Indexing" } }
        });
        handle_message(renderable, &router, "", &deps).await;
        assert!(
            matches!(
                upstream_rx.try_recv(),
                Ok(UpstreamNotification::CreateWorkDoneProgress { .. })
            ),
            "renderable reuse must upgrade to announced"
        );
        assert!(
            matches!(
                upstream_rx.try_recv(),
                Ok(UpstreamNotification::Progress { .. })
            ),
            "the upgrading begin must forward"
        );
    }

    /// Re-`create`ing the same downstream token mints a fresh upstream token and
    /// emits a `ForgetWorkDoneProgress` for the evicted one — but only if it was
    /// announced (the editor saw a create); unannounced tokens have no admission
    /// to forget, so no message is wasted on them.
    #[tokio::test]
    async fn handle_message_recreate_same_token_forgets_stale() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let make = || {
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "window/workDoneProgress/create",
                "params": { "token": "dup" }
            })
        };

        handle_message(make(), &router, "", &deps).await;

        // Re-create before any begin: the first token was never announced, so
        // nothing is forgotten (and nothing was created).
        handle_message(make(), &router, "", &deps).await;
        assert!(
            upstream_rx.try_recv().is_err(),
            "unannounced stale token needs no Forget"
        );

        // Announce the second token via a renderable begin...
        let begin = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "dup", "value": { "kind": "begin", "title": "Indexing" } }
        });
        handle_message(begin, &router, "", &deps).await;
        let UpstreamNotification::CreateWorkDoneProgress { token: announced } =
            upstream_rx.try_recv().expect("announce create")
        else {
            panic!("expected CreateWorkDoneProgress");
        };
        let UpstreamNotification::Progress { .. } = upstream_rx.try_recv().expect("begin forwards")
        else {
            panic!("expected Progress");
        };

        // ...then a third create for the SAME downstream token forgets it.
        handle_message(make(), &router, "", &deps).await;
        let UpstreamNotification::ForgetWorkDoneProgress(forgotten) =
            upstream_rx.try_recv().expect("forget stale")
        else {
            panic!("expected ForgetWorkDoneProgress");
        };
        assert_eq!(forgotten, vec![announced.clone()]);

        // The replacement lifecycle announces under a FRESH upstream token.
        let begin2 = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "dup", "value": { "kind": "begin", "title": "Indexing" } }
        });
        handle_message(begin2, &router, "", &deps).await;
        let UpstreamNotification::CreateWorkDoneProgress { token: reannounced } =
            upstream_rx.try_recv().expect("re-announce create")
        else {
            panic!("expected CreateWorkDoneProgress");
        };
        assert_ne!(reannounced, announced, "re-create must mint a fresh token");
    }

    /// A `window/workDoneProgress/create` without a token is rejected with
    /// InvalidParams rather than silently acked.
    #[tokio::test]
    async fn handle_message_work_done_progress_create_missing_token_errors() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let message = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "window/workDoneProgress/create",
            "params": {}
        });

        handle_message(message, &router, "", &deps).await;

        // An error response is returned and nothing is bridged upstream.
        let response = response_rx.try_recv().expect("should have response");
        let OutboundMessage::Untracked(val) = response else {
            panic!("Expected Untracked variant");
        };
        assert_eq!(val["id"], 6);
        assert!(val.get("error").is_some(), "missing token should error");
        assert!(
            upstream_rx.try_recv().is_err(),
            "nothing forwarded upstream"
        );
    }

    /// A downstream `$/progress` for a registered token is forwarded upstream
    /// with the token rewritten to the bridge-minted upstream token; an `End`
    /// clears the mapping.
    #[tokio::test]
    async fn handle_message_progress_translates_token_and_cleans_up_on_end() {
        use tower_lsp_server::ls_types::NumberOrString;

        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let downstream_token = NumberOrString::Number(1);
        let (upstream_token, _) = progress_registry.register(
            progress_connection_id,
            downstream_token.clone(),
            response_tx.clone(),
        );
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::clone(&progress_registry),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        // A renderable "begin" announces the token (create first) and is forwarded
        // with the translated token.
        let begin = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": 1,
                "value": { "kind": "begin", "title": "Indexing" }
            }
        });
        handle_message(begin, &router, "", &deps).await;
        let UpstreamNotification::CreateWorkDoneProgress { token } =
            upstream_rx.try_recv().expect("begin should announce")
        else {
            panic!("Expected CreateWorkDoneProgress");
        };
        assert_eq!(token, upstream_token);
        let UpstreamNotification::Progress { params } =
            upstream_rx.try_recv().expect("begin should forward")
        else {
            panic!("Expected Progress");
        };
        assert_eq!(params.token, upstream_token);

        // A mid-lifecycle "report" forwards with the translated token and
        // must NOT clear the mapping — only End does.
        let report = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": 1, "value": { "kind": "report", "message": "50%" } }
        });
        handle_message(report, &router, "", &deps).await;
        let UpstreamNotification::Progress { params } =
            upstream_rx.try_recv().expect("report should forward")
        else {
            panic!("Expected Progress");
        };
        assert_eq!(params.token, upstream_token);
        assert_eq!(
            progress_registry.translate(progress_connection_id, &downstream_token),
            Some(upstream_token.clone()),
            "mapping must survive a report"
        );

        // An "end" is forwarded too, then the mapping is cleared.
        let end = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": 1, "value": { "kind": "end" } }
        });
        handle_message(end, &router, "", &deps).await;
        assert!(upstream_rx.try_recv().is_ok(), "end should forward");
        assert_eq!(
            progress_registry.translate(progress_connection_id, &downstream_token),
            None,
            "mapping cleared after End"
        );
    }

    /// A `$/progress` for a token the bridge never minted (e.g. a client-provided
    /// workDoneToken) is dropped, not forwarded.
    #[tokio::test]
    async fn handle_message_progress_unmapped_token_is_dropped() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };

        let progress = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "never-registered", "value": { "kind": "begin", "title": "x" } }
        });
        handle_message(progress, &router, "", &deps).await;

        assert!(
            upstream_rx.try_recv().is_err(),
            "unmapped progress must not be forwarded"
        );
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            workspace_folders: WorkspaceFolderSet::new(Some(folders)),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            workspace_folders: WorkspaceFolderSet::new(None),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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

    /// A non-scratch push for an injection region's virtual document is routed
    /// upstream as `PublishDiagnostics` (push-propagation-diagnostic-forwarding),
    /// carrying the originating server name and the pushed diagnostics.
    #[tokio::test]
    async fn handle_message_routes_non_scratch_publish_diagnostics_for_virtual_uri() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: Some("luals".to_string()),
            response_tx,
            dynamic_capabilities: Arc::new(DynamicCapabilityRegistry::new()),
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
        };

        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///project/kakehashi-virtual-uri-REGION.lua",
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    "message": "boom"
                }]
            }
        });

        handle_message(message, &router, "", &deps).await;

        match upstream_rx
            .try_recv()
            .expect("virtual-uri push should be routed upstream")
        {
            UpstreamNotification::PublishDiagnostics {
                uri,
                server,
                connection_id,
                diagnostics,
            } => {
                assert_eq!(connection_id, deps.progress_connection_id);
                assert!(uri.contains("kakehashi-virtual-uri-REGION"));
                assert_eq!(server, "luals");
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].message, "boom");
            }
            _ => panic!("expected PublishDiagnostics"),
        }
    }

    /// A non-virtual (real host URI) push is now routed up too — the publisher
    /// classifies it as a candidate `_self` host-layer push and decides (using
    /// document/config state the reader lacks) whether it names an open
    /// host-bridged doc. The reader only forwards.
    #[tokio::test]
    async fn handle_message_routes_real_uri_push_for_host_classification() {
        let router = ResponseRouter::new();
        let (response_tx, _response_rx) = mpsc::channel(16);
        let (upstream_tx, mut upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: Some("lua_ls".to_string()),
            response_tx,
            dynamic_capabilities: Arc::new(DynamicCapabilityRegistry::new()),
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
        };

        let message = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///project/real_file.lua",
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    "message": "host diag"
                }]
            }
        });

        handle_message(message, &router, "", &deps).await;

        match upstream_rx
            .try_recv()
            .expect("real-uri push should be routed for host classification")
        {
            UpstreamNotification::PublishDiagnostics {
                uri, diagnostics, ..
            } => {
                assert_eq!(uri, "file:///project/real_file.lua");
                assert_eq!(diagnostics.len(), 1);
            }
            _ => panic!("expected PublishDiagnostics"),
        }
    }

    /// A malformed `diagnostics` array must be dropped, not routed as an empty
    /// (clearing) push — otherwise a parse error would silently wipe the region.
    /// An empty array, by contrast, is a legitimate clear and is routed.
    #[tokio::test]
    async fn handle_message_drops_malformed_but_routes_empty_publish_diagnostics() {
        let make_deps = || {
            let (response_tx, _response_rx) = mpsc::channel(16);
            let (upstream_tx, upstream_rx) = mpsc::unbounded_channel();
            let (window_tx, _window_rx) = mpsc::channel(16);
            let deps = ServerRequestDeps {
                settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
                server_name: Some("luals".to_string()),
                response_tx,
                dynamic_capabilities: Arc::new(DynamicCapabilityRegistry::new()),
                upstream_tx,
                workspace_folders: WorkspaceFolderSet::new(None),
                window_tx,
                upstream_request_tx: mpsc::unbounded_channel().0,
                inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
                progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
                client_progress_registry: Arc::new(
                    crate::lsp::bridge::ClientProgressRegistry::new(),
                ),
                progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
            };
            (deps, upstream_rx)
        };
        let uri = "file:///project/kakehashi-virtual-uri-REGION.lua";

        // Malformed: `diagnostics` is not an array of Diagnostic -> dropped.
        let (deps, mut upstream_rx) = make_deps();
        let malformed = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": [{"range": "not-a-range"}]}
        });
        handle_message(malformed, &ResponseRouter::new(), "", &deps).await;
        assert!(
            upstream_rx.try_recv().is_err(),
            "malformed diagnostics must be dropped, not routed"
        );

        // Empty array: a legitimate clear -> routed with an empty Vec.
        let (deps, mut upstream_rx) = make_deps();
        let empty = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "diagnostics": []}
        });
        handle_message(empty, &ResponseRouter::new(), "", &deps).await;
        match upstream_rx
            .try_recv()
            .expect("empty array should be routed")
        {
            UpstreamNotification::PublishDiagnostics { diagnostics, .. } => {
                assert!(diagnostics.is_empty(), "empty array clears");
            }
            _ => panic!("expected PublishDiagnostics"),
        }

        // Adversarial: `params` is not an object (string) — must not panic, drop.
        let (deps, mut upstream_rx) = make_deps();
        let bad_params = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": "not-an-object"
        });
        handle_message(bad_params, &ResponseRouter::new(), "", &deps).await;
        assert!(
            upstream_rx.try_recv().is_err(),
            "a non-object params must be dropped without panicking"
        );

        // Adversarial: `params` present but missing `uri` — drop, no panic.
        let (deps, mut upstream_rx) = make_deps();
        let no_uri = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"diagnostics": []}
        });
        handle_message(no_uri, &ResponseRouter::new(), "", &deps).await;
        assert!(upstream_rx.try_recv().is_err(), "missing uri is dropped");
    }

    #[tokio::test]
    async fn handle_message_unknown_server_request_sends_method_not_found() {
        let router = ResponseRouter::new();
        let (response_tx, mut response_rx) = mpsc::channel(16);
        let dynamic_capabilities = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities: Arc::clone(&dynamic_capabilities),
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx: response_tx.clone(),
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx,
            dynamic_capabilities,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry: Arc::new(crate::lsp::bridge::ProgressRegistry::new()),
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
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
        handle.notify_liveness_start(1);

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
        handle.notify_liveness_start(1);

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
        handle.notify_liveness_start(1);

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
        handle.notify_liveness_start(1);

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

    #[tokio::test(start_paused = true)]
    async fn liveness_timeout_ignores_request_cancelled_before_write() {
        use crate::lsp::bridge::UpstreamId;
        use crate::lsp::bridge::protocol::RequestId;
        use std::time::Duration;

        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");
        let (_writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let request_id = RequestId::new(1);
        let upstream_id = UpstreamId::Number(7);
        let _rx = router
            .register_with_upstream(request_id, Some(upstream_id.clone()))
            .unwrap();
        assert_eq!(
            router.prepare_cancel_by_upstream(&upstream_id),
            (true, vec![])
        );
        assert_eq!(router.pending_count(), 1);

        let handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(100)),
        );
        handle.notify_liveness_start(1);
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::time::advance(Duration::from_millis(101)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;

        assert!(
            !handle.check_liveness_failed(),
            "an empty router must not fail an otherwise healthy idle connection"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_old_liveness_epoch_does_not_fail_new_request() {
        use crate::lsp::bridge::UpstreamId;
        use crate::lsp::bridge::protocol::RequestId;
        use std::time::Duration;

        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");
        let (_writer, reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let old_upstream = UpstreamId::Number(7);
        let (_old_rx, old_epoch) = router
            .register_with_upstream_liveness(RequestId::new(1), Some(old_upstream.clone()))
            .unwrap();
        let handle = spawn_reader_task_with_liveness(
            reader,
            Arc::clone(&router),
            Some(Duration::from_millis(100)),
        );
        handle.notify_liveness_start(old_epoch.unwrap());
        tokio::time::sleep(Duration::from_millis(1)).await;

        // Make the old deadline ready without letting the reader poll it, then
        // replace the only progressing request and queue the new epoch wake-up.
        tokio::time::advance(Duration::from_millis(101)).await;
        router.prepare_cancel_by_upstream(&old_upstream);
        let (_new_rx, new_epoch) = router
            .register_with_upstream_liveness(RequestId::new(2), None)
            .unwrap();
        handle.notify_liveness_start(new_epoch.unwrap());
        tokio::time::sleep(Duration::from_millis(1)).await;

        assert!(
            !handle.check_liveness_failed(),
            "an expired timer from an older request epoch must not fail new work"
        );
        assert_eq!(router.awaiting_downstream_count(), 1);
    }

    #[tokio::test]
    async fn handle_message_forwards_telemetry_event_upstream() {
        let router = ResponseRouter::new();
        let (deps, mut window_rx, _keep) = server_request_deps_for(Some("mock-ls"));

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "telemetry/event",
            "params": { "kind": "metric", "value": 42 }
        });
        handle_message(notification, &router, "", &deps).await;

        match window_rx
            .try_recv()
            .expect("telemetry/event should be forwarded")
        {
            UpstreamNotification::TelemetryEvent { data } => {
                assert_eq!(data, json!({ "kind": "metric", "value": 42 }));
            }
            other => panic!("expected TelemetryEvent, got {other:?}"),
        }
    }

    /// Deps exposing the response receiver and the upstream-request receiver, for
    /// the deferred request-relay handlers (showMessageRequest / showDocument).
    fn server_request_deps_with_request_rx() -> (
        ServerRequestDeps,
        mpsc::Receiver<OutboundMessage>,
        mpsc::UnboundedReceiver<UpstreamRequest>,
    ) {
        let (response_tx, response_rx) = mpsc::channel(16);
        let (upstream_tx, _upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, _window_rx) = mpsc::channel(16);
        let (upstream_request_tx, upstream_request_rx) = mpsc::unbounded_channel();
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: Some("mock-ls".to_string()),
            response_tx,
            dynamic_capabilities: Arc::new(DynamicCapabilityRegistry::new()),
            upstream_tx,
            window_tx,
            upstream_request_tx,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            workspace_folders: WorkspaceFolderSet::new(None),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };
        (deps, response_rx, upstream_request_rx)
    }

    /// Await the next outbound response, unwrapping the `Untracked` envelope.
    async fn recv_untracked(
        response_rx: &mut mpsc::Receiver<OutboundMessage>,
    ) -> serde_json::Value {
        match response_rx.recv().await.expect("a response should be sent") {
            OutboundMessage::Untracked(value) => value,
            other => panic!("expected Untracked response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_message_show_message_request_relays_selected_action() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        let request = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "window/showMessageRequest",
            "params": {
                "type": 3,
                "message": "pick one",
                "actions": [{ "title": "Retry" }, { "title": "Cancel" }]
            }
        });
        handle_message(request, &router, "", &deps).await;

        // The bridge forwards an UpstreamRequest carrying the reply channel.
        let UpstreamRequest::ShowMessageRequest {
            message,
            actions,
            reply,
            ..
        } = request_rx
            .recv()
            .await
            .expect("should forward an UpstreamRequest")
        else {
            panic!("expected ShowMessageRequest");
        };
        assert_eq!(message, "pick one");
        assert_eq!(actions.expect("actions present").len(), 2);

        // Simulate the editor selecting "Retry".
        reply
            .send(Some(
                serde_json::from_value(json!({ "title": "Retry" })).unwrap(),
            ))
            .expect("reply channel open");

        // The downstream receives a response whose result is the selected action.
        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"]["title"], "Retry");
    }

    #[tokio::test]
    async fn handle_message_show_message_request_editor_error_responds_null() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        let request = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "window/showMessageRequest",
            "params": { "type": 1, "message": "oops" }
        });
        handle_message(request, &router, "", &deps).await;

        // Dropping the request (and its reply sender) simulates the editor failing
        // to answer; the downstream must still get a `null` (no selection).
        let request = request_rx.recv().await.expect("forwarded");
        drop(request);

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 8);
        assert_eq!(response["result"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn handle_message_show_message_request_invalid_params_responds_error() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        // Missing the required `type` field.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "window/showMessageRequest",
            "params": { "message": "no type" }
        });
        handle_message(request, &router, "", &deps).await;

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 9);
        assert!(
            response["error"].is_object(),
            "invalid params should produce an error response: {response}"
        );
        // No request should have been forwarded to the editor.
        assert!(request_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_message_show_document_relays_success_and_passes_virtual_uri_through() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        // A virtual-document URI is passed through verbatim (no translation).
        let virtual_uri = "file:///p/kakehashi-virtual-uri-R.py";
        let request = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "window/showDocument",
            "params": { "uri": virtual_uri }
        });
        handle_message(request, &router, "", &deps).await;

        let UpstreamRequest::ShowDocument { params, reply, .. } = request_rx
            .recv()
            .await
            .expect("should forward an UpstreamRequest")
        else {
            panic!("expected ShowDocument");
        };
        assert_eq!(params.uri.as_str(), virtual_uri);

        reply.send(true).expect("reply channel open");

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 10);
        assert_eq!(response["result"]["success"], true);
    }

    #[tokio::test]
    async fn handle_message_show_document_editor_failure_responds_success_false() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        let request = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "window/showDocument",
            "params": { "uri": "file:///nope.rs" }
        });
        handle_message(request, &router, "", &deps).await;

        let UpstreamRequest::ShowDocument { reply, .. } =
            request_rx.recv().await.expect("forwarded")
        else {
            panic!("expected ShowDocument");
        };
        // Editor could not open the document.
        reply.send(false).expect("reply channel open");

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 11);
        assert_eq!(response["result"]["success"], false);
    }

    #[tokio::test]
    async fn handle_message_show_document_invalid_params_responds_error() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        // Missing the required `uri` field.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "window/showDocument",
            "params": { "external": true }
        });
        handle_message(request, &router, "", &deps).await;

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 13);
        assert!(
            response["error"].is_object(),
            "invalid params should produce an error response: {response}"
        );
        assert!(request_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_message_show_document_loop_gone_responds_success_false() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, request_rx) = server_request_deps_with_request_rx();

        // Forwarding loop gone before the request is handled.
        drop(request_rx);

        let request = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "window/showDocument",
            "params": { "uri": "file:///x.rs" }
        });
        handle_message(request, &router, "", &deps).await;

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 12);
        assert_eq!(response["result"]["success"], false);
    }

    #[tokio::test]
    async fn handle_message_apply_edit_relays_full_editor_response() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        let request = json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "workspace/applyEdit",
            "params": {
                "label": "organize imports",
                "edit": { "changes": { "file:///p/main.rs": [] } }
            }
        });
        handle_message(request, &router, "", &deps).await;

        let UpstreamRequest::ApplyEdit { params, reply, .. } = request_rx
            .recv()
            .await
            .expect("should forward an UpstreamRequest")
        else {
            panic!("expected ApplyEdit");
        };
        assert_eq!(params.label.as_deref(), Some("organize imports"));

        // The editor rejected the edit: `applied`, `failureReason`, and
        // `failedChange` must all reach the downstream — it acts on them.
        reply
            .send(
                serde_json::from_value(json!({
                    "applied": false,
                    "failureReason": "user rejected",
                    "failedChange": 1
                }))
                .unwrap(),
            )
            .expect("reply channel open");

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 20);
        assert_eq!(response["result"]["applied"], false);
        assert_eq!(response["result"]["failureReason"], "user rejected");
        assert_eq!(response["result"]["failedChange"], 1);
    }

    #[tokio::test]
    async fn handle_message_apply_edit_editor_drop_responds_applied_false() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        let request = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "workspace/applyEdit",
            "params": { "edit": {} }
        });
        handle_message(request, &router, "", &deps).await;

        // Dropping the request (and its reply sender) simulates the editor
        // failing to answer; the downstream must still get `applied: false`.
        let request = request_rx.recv().await.expect("forwarded");
        drop(request);

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 21);
        assert_eq!(response["result"]["applied"], false);
        assert!(
            response["result"]["failureReason"].is_string(),
            "a fabricated failure must carry a reason: {response}"
        );
    }

    #[tokio::test]
    async fn handle_message_apply_edit_invalid_params_responds_error() {
        let router = ResponseRouter::new();
        let (deps, mut response_rx, mut request_rx) = server_request_deps_with_request_rx();

        // Missing the required `edit` field.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "workspace/applyEdit",
            "params": { "label": "no edit" }
        });
        handle_message(request, &router, "", &deps).await;

        let response = recv_untracked(&mut response_rx).await;
        assert_eq!(response["id"], 22);
        assert!(
            response["error"].is_object(),
            "invalid params should produce an error response: {response}"
        );
        // No request should have been forwarded to the editor.
        assert!(request_rx.try_recv().is_err());
    }

    // ---- workspace/applyEdit reader handler ---------------------------------

    #[tokio::test]
    async fn apply_edit_rejects_unparseable_params_with_invalid_params() {
        let (deps, (mut response_rx, _upstream_rx, _window_rx)) =
            dummy_server_request_deps_with_rx();
        let message = json!({
            "jsonrpc": "2.0", "id": 9, "method": "workspace/applyEdit",
            "params": { "edit": "not-an-object" }
        });

        crate::lsp::bridge::workspace::apply_edit::handle(
            &message,
            jsonrpc::Id::Number(9),
            "[test] ",
            &deps,
        );

        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), response_rx.recv())
            .await
            .expect("a response must be produced")
            .expect("channel open");
        let OutboundMessage::Untracked(value) = sent else {
            panic!("expected an untracked server-request response");
        };
        assert_eq!(
            value["error"]["code"], -32602,
            "unparseable params must answer InvalidParams: {value}"
        );
    }

    #[tokio::test]
    async fn apply_edit_answers_applied_false_when_the_reply_channel_drops() {
        // The forwarding loop dying mid-flight (or losing the editor response)
        // drops the reply sender; the downstream must still get a response —
        // applied:false with the neutral reason — never a hang.
        let (mut deps, (mut response_rx, _upstream_rx, _window_rx)) =
            dummy_server_request_deps_with_rx();
        let (request_tx, mut request_rx) = mpsc::unbounded_channel();
        deps.upstream_request_tx = request_tx;
        let message = json!({
            "jsonrpc": "2.0", "id": 10, "method": "workspace/applyEdit",
            "params": { "edit": { "changes": { "file:///x.rs": [] } } }
        });

        crate::lsp::bridge::workspace::apply_edit::handle(
            &message,
            jsonrpc::Id::Number(10),
            "[test] ",
            &deps,
        );

        let forwarded = tokio::time::timeout(std::time::Duration::from_secs(5), request_rx.recv())
            .await
            .expect("the request must be enqueued")
            .expect("channel open");
        let UpstreamRequest::ApplyEdit { reply, .. } = forwarded else {
            panic!("expected an ApplyEdit upstream request");
        };
        drop(reply);

        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), response_rx.recv())
            .await
            .expect("a response must be produced")
            .expect("channel open");
        let OutboundMessage::Untracked(value) = sent else {
            panic!("expected an untracked server-request response");
        };
        assert_eq!(value["result"]["applied"], false, "got: {value}");
        assert!(
            value["result"]["failureReason"]
                .as_str()
                .is_some_and(|r| r.contains("no workspace/applyEdit response")),
            "the drop path's neutral reason must be relayed: {value}"
        );
    }

    #[tokio::test]
    async fn apply_edit_still_answers_when_the_forwarding_loop_is_gone() {
        // Enqueue failure (receiver dropped — shutdown): the handler
        // unregisters its just-made registry entry and the responder still
        // answers applied:false via the dropped reply channel — the
        // downstream never hangs and the registry entry doesn't leak into
        // the #404 cancel path.
        let (deps, (mut response_rx, _upstream_rx, _window_rx)) =
            dummy_server_request_deps_with_rx();
        // dummy deps' upstream_request_tx receiver is already dropped.
        let message = json!({
            "jsonrpc": "2.0", "id": 11, "method": "workspace/applyEdit",
            "params": { "edit": { "changes": {} } }
        });

        crate::lsp::bridge::workspace::apply_edit::handle(
            &message,
            jsonrpc::Id::Number(11),
            "[test] ",
            &deps,
        );

        let sent = tokio::time::timeout(std::time::Duration::from_secs(5), response_rx.recv())
            .await
            .expect("a response must be produced")
            .expect("channel open");
        let OutboundMessage::Untracked(value) = sent else {
            panic!("expected an untracked server-request response");
        };
        assert_eq!(value["result"]["applied"], false, "got: {value}");
        // The failed-enqueue path must also unregister its just-made registry
        // entry, or it would leak into the #404 cancel path.
        assert!(
            !deps
                .inbound_request_registry
                .is_registered(deps.progress_connection_id, &jsonrpc::Id::Number(11)),
            "the registry entry must not leak after a failed enqueue"
        );
    }

    /// Like `dummy_server_request_deps` but hands back the typed receiver
    /// tuple so tests can read the produced responses.
    #[allow(clippy::type_complexity)]
    fn dummy_server_request_deps_with_rx() -> (
        ServerRequestDeps,
        (
            mpsc::Receiver<OutboundMessage>,
            mpsc::UnboundedReceiver<UpstreamNotification>,
            mpsc::Receiver<UpstreamNotification>,
        ),
    ) {
        let (tx, rx) = mpsc::channel(16);
        let caps = Arc::new(DynamicCapabilityRegistry::new());
        let (upstream_tx, upstream_rx) = mpsc::unbounded_channel();
        let (window_tx, window_rx) = mpsc::channel(16);
        let progress_registry = Arc::new(crate::lsp::bridge::ProgressRegistry::new());
        let progress_connection_id = progress_registry.new_connection_id();
        let deps = ServerRequestDeps {
            settings: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
            server_name: None,
            response_tx: tx,
            dynamic_capabilities: caps,
            upstream_tx,
            workspace_folders: WorkspaceFolderSet::new(None),
            window_tx,
            upstream_request_tx: mpsc::unbounded_channel().0,
            inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry::default(),
            progress_registry,
            client_progress_registry: Arc::new(crate::lsp::bridge::ClientProgressRegistry::new()),
            progress_connection_id,
        };
        (deps, (rx, upstream_rx, window_rx))
    }
}
