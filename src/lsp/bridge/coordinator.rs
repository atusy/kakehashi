//! Bridge coordinator unifying the language server pool and node tracker
//! into a single coherent API.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;
use url::Url;

use crate::config::{
    WorkspaceSettings, merge_bridge_server_configs, resolve_with_wildcard,
    settings::BridgeServerConfig,
};
use crate::language::node_tracker::{EditInfo, NodeTracker};
use crate::lsp::request_id::CancelForwarder;

use super::LanguageServerPool;
use super::pool::ConnectionKey;
use super::pool::INIT_TIMEOUT_SECS;
use super::protocol::{RoutingAnswer, RoutingLanguageServer, RoutingParams, RoutingTextDocument};

/// A resolved bridge virtual-document payload from a host document.
///
/// Usually represents one injected region; an `injection.combined` payload can
/// span multiple captures and uses the first capture's stable region ID.
#[derive(Debug, Clone)]
pub(crate) struct BridgeInjection {
    /// The injection language (e.g., "lua", "python", "rust")
    pub(crate) language: String,
    /// Stable ULID-based region ID (lazy-node-identity-tracking)
    pub(crate) region_id: String,
    /// The text content of the bridge virtual document
    pub(crate) content: String,
}

/// One server's share of an eager-open batch: its spawn config plus the
/// injections to open on it.
type ServerGroup = (Arc<BridgeServerConfig>, Vec<BridgeInjection>);

/// Resolved server configuration with server name.
///
/// Carries both the server name (for connection lookup) and the config (for
/// spawning) so multiple languages can share one server process (e.g., ts and
/// tsx using tsgo).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedServerConfig {
    /// The server name from the languageServers config key (e.g., "tsgo", "rust-analyzer")
    pub(crate) server_name: String,
    /// The server configuration (cmd, languages, initialization_options, etc.).
    ///
    /// Wrapped in `Arc` to avoid cloning large configs during fan-out dispatch.
    /// Each spawned task gets an `Arc::clone` (atomic increment) instead of a
    /// deep clone of `Vec<String>` fields. `send_*_request` takes `&BridgeServerConfig`,
    /// so the `Arc` auto-derefs transparently.
    pub(crate) config: Arc<BridgeServerConfig>,
}

/// Whether an acquire error means the warm-up was overtaken rather than that
/// the server failed to start.
///
/// Three shapes qualify, and none of them is worth telling a user about: an
/// acquire for the same key already shaking hands, a slot on its way down, and
/// the pool refusing new spawns — which covers both shutdown and a warm-up
/// this pass superseded. What is left is a server that genuinely could not
/// start, which is exactly what the user needs to hear.
fn is_concurrent_acquire(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::Interrupted {
        return true;
    }
    use crate::lsp::bridge::pool::BridgeError;
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<BridgeError>())
        .is_some_and(|inner| inner.is_initializing() || inner.is_closing())
}

fn resolve_reload_server_config(
    settings: &WorkspaceSettings,
    server_name: &str,
) -> Option<BridgeServerConfig> {
    // A wildcard supplies defaults to concrete entries; it must not keep a
    // deleted concrete server alive merely because its old name is known by a
    // live connection.
    if !settings.language_servers.contains_key(server_name) {
        return None;
    }
    resolve_with_wildcard(
        &settings.language_servers,
        server_name,
        merge_bridge_server_configs,
    )
}

/// A batch of eager-open task handles with a generation counter.
///
/// The generation counter enables detection of stale pushes: when a concurrent
/// `supersede` replaces the batch, handles from the previous generation are
/// aborted instead of being accidentally adopted.
///
/// The `cancel` token closes a second race window the abort-handle scheme can't
/// reach (#435): on a multi-thread runtime a spawned task's *body* can start
/// running (reaching `get_or_create_connection_wait_ready` + didOpen) BEFORE its
/// `AbortHandle` is registered, so a concurrent cancel/supersede/abort lands in
/// the spawn→register window with nothing to abort. Each task `select!`s on a
/// clone of this token before its first side effect; cancelling the token bails
/// the body even when its handle isn't registered yet. The batch is inserted into
/// the DashMap (in `supersede_*`) before any task spawns, so the token is always
/// reachable by a concurrent cancel during that window.
struct EagerOpenBatch {
    generation: u64,
    handles: Vec<tokio::task::AbortHandle>,
    cancel: CancellationToken,
}

#[cfg(test)]
pub(crate) struct ForceStartTestControl {
    pub(crate) before_admission: Arc<tokio::sync::Notify>,
    pub(crate) release_admission: Arc<tokio::sync::Notify>,
    pub(crate) admission_finished: Arc<tokio::sync::Notify>,
    pub(crate) pause_propagation: std::sync::atomic::AtomicBool,
    pub(crate) after_propagation: Arc<tokio::sync::Notify>,
    pub(crate) release_propagation: Arc<tokio::sync::Notify>,
}

/// Bundles `LanguageServerPool` and `NodeTracker` so LSP handlers see one field.
/// The pool is `Arc`'d so the cancel-forwarding middleware can share it.
///
/// Prefer `self.bridge.pool().*` directly in new code; only add a delegating method
/// here when the operation has 3+ callers, combines pool with node_tracker, or
/// genuinely benefits from a semantic name (e.g., document lifecycle, shutdown).
pub(crate) struct BridgeCoordinator {
    pool: Arc<LanguageServerPool>,
    node_tracker: Arc<NodeTracker>,
    /// Cancel forwarder for upstream cancel notification and downstream forwarding.
    ///
    /// This is shared with the `RequestIdCapture` middleware via `cancel_forwarder()`.
    /// Handlers can subscribe to cancel notifications using `cancel_forwarder().subscribe()`.
    cancel_forwarder: CancelForwarder,
    /// Monotonic generation counter for eager-open batches.
    ///
    /// Incremented by each `supersede_eager_open_tasks` call. Handles pushed
    /// with a stale generation are aborted, preventing accidental adoption
    /// by a concurrent supersede's batch.
    ///
    /// Uses `Ordering::Relaxed` — monotonicity is the only requirement;
    /// DashMap's internal locks provide memory synchronization for the
    /// stored generation values.
    eager_open_generation: std::sync::atomic::AtomicU64,
    /// Monotonic generation counter for `forceStart` warm-up passes.
    ///
    /// Each pass claims the next value; its detached acquires re-read this
    /// before touching the pool and stand down if a newer pass has claimed
    /// one, so a task carrying a superseded configuration cannot spawn — or
    /// worse, replace a correctly-configured connection with a stale launch
    /// config, which the pool would do on seeing what it reads as a config
    /// change. The newer pass re-asserts every flag anyway, so standing down
    /// loses nothing.
    ///
    /// `Arc` because the check happens inside the detached task. Ordering is
    /// `Relaxed` for the same reason the counter above is: monotonicity is
    /// the whole requirement, and the pool's own lock orders the effects.
    force_start_generation: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)]
    force_start_test_control: arc_swap::ArcSwapOption<ForceStartTestControl>,
    /// Eager-open task batches, keyed by host document URI.
    ///
    /// Each batch contains a generation counter and abort handles. When a new
    /// batch is registered for the same URI, the previous batch is aborted.
    ///
    /// This prevents orphaned virtual documents when:
    /// - Host document is closed while tasks wait for server readiness
    /// - Rapid did_change events spawn many overlapping batches
    eager_open_tasks: DashMap<Url, EagerOpenBatch>,
    /// Generation counter for host-layer eager-open batches (#429); separate from
    /// `eager_open_generation` so the two paths never alias.
    host_eager_open_generation: std::sync::atomic::AtomicU64,
    /// Host-layer eager-open tasks, keyed by host document URI (#429). Separate
    /// from `eager_open_tasks` because the host path fires on `didOpen` for the
    /// real host doc (no injections). Uses the same generation/placeholder shape
    /// as the virt path: `supersede` resets to an empty placeholder before
    /// spawning, so a handle *registered* after a concurrent
    /// `cancel_host_eager_open` (didClose) / `abort_all_eager_open` (shutdown) is
    /// aborted on the spot (the registration leak is closed). The
    /// body-started-before-registration window is closed too (#435): the batch's
    /// `CancellationToken` is in the map before any task spawns, each task `select!`s
    /// on it before its first side effect, and cancel/supersede/abort cancel it.
    host_eager_open_tasks: DashMap<Url, EagerOpenBatch>,
    /// Resolved-config memo for the current settings snapshot.
    ///
    /// `get_all_configs_for_language` / `get_host_configs_for_language`
    /// re-merge every configured server (deep-cloning each server's
    /// `settings` JSON blob) on every call. Whole-document handlers call
    /// them once **per injection region**, so on a fence-heavy document with
    /// a large user config that is seconds of clone/drop CPU inside a single
    /// handler poll — enough to wedge the transport's shared dispatch task
    /// and stall every other response. The memo keys results by the settings
    /// snapshot's `Arc` identity (settings are hot-swapped whole via
    /// `ArcSwap`, so pointer identity IS snapshot identity) and by language
    /// pair, making repeat resolutions a shallow `Vec` clone (one `String` +
    /// one `Arc` bump per configured server — the settings blobs themselves
    /// stay behind their `Arc`s).
    config_memo: ArcSwap<ConfigMemo>,
}

/// One settings snapshot's worth of resolved-config lookups (see
/// [`BridgeCoordinator::config_memo`]). Replaced wholesale when a lookup
/// arrives for a different settings snapshot.
/// One `(injection_language, configs)` pair in [`ConfigMemo::virt`]'s
/// per-host list.
type VirtMemoEntry = (String, Arc<Vec<ResolvedServerConfig>>);

struct ConfigMemo {
    /// Identity anchor: results below are valid only for this snapshot.
    /// `None` for the initial placeholder, which never matches.
    settings: Option<Arc<WorkspaceSettings>>,
    /// `host_language` → `(injection_language, configs)` pairs. A nested Vec
    /// rather than a `(String, String)` key so the per-region hit path looks
    /// up with a borrowed `&str` and scans a handful of pairs — zero
    /// allocations per hit (a tuple key cannot be borrowed field-wise).
    /// Inserts re-check under the entry lock, so racing same-pair computes
    /// cannot append duplicates.
    virt: DashMap<String, Vec<VirtMemoEntry>>,
    /// `host_language` → `_self` host-bridge configs.
    host: DashMap<String, Arc<Vec<ResolvedServerConfig>>>,
}

impl ConfigMemo {
    fn empty(settings: Option<Arc<WorkspaceSettings>>) -> Self {
        Self {
            settings,
            virt: DashMap::new(),
            host: DashMap::new(),
        }
    }
}

impl BridgeCoordinator {
    pub(crate) fn begin_virtual_routing_for_injections(
        &self,
        host_uri: &Url,
        injections: &[BridgeInjection],
    ) {
        let Ok(host_uri_lsp) = crate::lsp::lsp_impl::url_to_uri(host_uri) else {
            return;
        };
        for injection in injections {
            let virtual_uri = super::protocol::VirtualDocumentUri::new(
                &host_uri_lsp,
                &injection.language,
                &injection.region_id,
            );
            let Ok(virtual_uri) = Url::parse(&virtual_uri.to_uri_string()) else {
                continue;
            };
            self.pool.begin_virtual_routing(host_uri, &virtual_uri);
        }
    }

    /// Create a new bridge coordinator with fresh pool and tracker.
    pub(crate) fn new() -> Self {
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
        Self {
            pool,
            node_tracker: Arc::new(NodeTracker::new()),
            cancel_forwarder,
            eager_open_generation: std::sync::atomic::AtomicU64::new(0),
            force_start_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            force_start_test_control: arc_swap::ArcSwapOption::empty(),
            eager_open_tasks: DashMap::new(),
            host_eager_open_generation: std::sync::atomic::AtomicU64::new(0),
            host_eager_open_tasks: DashMap::new(),
            config_memo: ArcSwap::new(Arc::new(ConfigMemo::empty(None))),
        }
    }

    /// Create a bridge coordinator with an existing pool and cancel forwarder.
    ///
    /// This is used when the pool/forwarder needs to be shared with external components
    /// like the cancel forwarding middleware.
    ///
    /// The `cancel_forwarder` MUST be created from the same `pool` to ensure cancel
    /// notifications are properly routed.
    pub(crate) fn with_cancel_forwarder(
        pool: Arc<LanguageServerPool>,
        cancel_forwarder: CancelForwarder,
    ) -> Self {
        Self {
            pool,
            node_tracker: Arc::new(NodeTracker::new()),
            cancel_forwarder,
            eager_open_generation: std::sync::atomic::AtomicU64::new(0),
            force_start_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            force_start_test_control: arc_swap::ArcSwapOption::empty(),
            eager_open_tasks: DashMap::new(),
            host_eager_open_generation: std::sync::atomic::AtomicU64::new(0),
            host_eager_open_tasks: DashMap::new(),
            config_memo: ArcSwap::new(Arc::new(ConfigMemo::empty(None))),
        }
    }

    // ========================================
    // Accessor methods (leaky but pragmatic)
    // ========================================

    /// Resolve a virtual-document URI string to its `(host_url, region_id)`
    /// (used by `window/showDocument` translation). Delegates to the pool.
    pub(crate) async fn resolve_virtual_uri(&self, virtual_uri: &str) -> Option<(Url, String)> {
        self.pool.resolve_virtual_uri(virtual_uri).await
    }

    /// The version currently tracked for a virtual document on one connection
    /// (didOpen = 1, each content-changing didChange bumps it; the bridge's
    /// own revision counter, not a delivery receipt — see
    /// `DocumentTracker::document_version`). `None` when the document is not
    /// tracked for this connection. Used by the inbound `workspace/applyEdit`
    /// translation to validate downstream-supplied `TextDocumentEdit.version`s.
    /// Delegates to the pool.
    pub(crate) async fn virtual_document_version(
        &self,
        virtual_uri: &str,
        connection_key: &crate::lsp::bridge::pool::ConnectionKey,
    ) -> Option<i32> {
        self.pool
            .virtual_document_version(virtual_uri, connection_key)
            .await
    }

    /// Access the underlying node tracker.
    ///
    /// Used by handlers for `InjectionResolver::resolve_at_byte_offset()`.
    pub(crate) fn node_tracker(&self) -> &NodeTracker {
        &self.node_tracker
    }

    /// Share the node tracker for use on the blocking semantic-token pool.
    ///
    /// Returns an owned `Arc` so the tracker can be moved into `spawn_blocking`
    /// (injection-token-cache region-id resolution, #529) without borrowing `self`.
    pub(crate) fn node_tracker_arc(&self) -> Arc<NodeTracker> {
        Arc::clone(&self.node_tracker)
    }

    /// Access the underlying language server pool.
    ///
    /// Used by handlers for `send_*_request()` methods.
    pub(crate) fn pool(&self) -> &LanguageServerPool {
        &self.pool
    }

    /// Get a cloneable reference to the pool for use in spawned tasks.
    ///
    /// Used when handlers need to spawn parallel tasks that each need
    /// their own reference to the pool (e.g., diagnostic fan-out).
    pub(crate) fn pool_arc(&self) -> Arc<LanguageServerPool> {
        Arc::clone(&self.pool)
    }

    /// Apply merged server-config changes to live downstream connections.
    /// Runtime `settings` changes are pushed in place; removed servers and
    /// spawn-time config changes evict and shut down their connections so the
    /// next use spawns from the new config. Returns the number of settings
    /// notifications pushed (evictions are not counted).
    pub(crate) async fn propagate_settings(&self, settings: &WorkspaceSettings) -> usize {
        let pushed = self
            .pool
            .propagate_settings(|server_name| resolve_reload_server_config(settings, server_name))
            .await;
        #[cfg(test)]
        if let Some(control) = self.force_start_test_control.load_full()
            && control
                .pause_propagation
                .load(std::sync::atomic::Ordering::Acquire)
        {
            control.after_propagation.notify_one();
            control.release_propagation.notified().await;
        }
        pushed
    }

    #[cfg(test)]
    pub(crate) fn set_force_start_test_control(&self, control: Arc<ForceStartTestControl>) {
        self.force_start_test_control.store(Some(control));
    }

    /// Retire every in-flight warm-up acquire, without launching new ones.
    ///
    /// Call this at the *start* of a settings application, before anything
    /// that walks the connection map. An acquire is admitted for as long as
    /// its generation is current, so retiring them only when the new warm-ups
    /// launch would leave a window — after propagation, before the pass — in
    /// which a stale acquire can still install a connection propagation has
    /// already walked past.
    pub(crate) fn supersede_force_start(&self) {
        self.force_start_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Spawn every configured server that asks to start without waiting for a
    /// document (`forceStart`), returning how many warm-up acquires were
    /// launched — attempts, not connections: each one is detached, so none of
    /// them has resolved a key or started a process by the time this returns.
    ///
    /// Runs at each settings application, so a reload that adds the flag —  or
    /// adds the server — starts it then, and one that removes the flag simply
    /// stops re-asserting it: within a session the flag is one-way, since
    /// nothing here stops a running server (bridge-routing-protocol).
    ///
    /// Ordinary get-or-create, so a server already running under the key it
    /// resolves to is reused rather than double-spawned, and one racing a
    /// document's lazy acquire collides with it on the same key instead of
    /// forking a second process. With no document there is no marker walk, so
    /// the key is whatever a document-less acquire produces — the shared key
    /// for a `preferSharedInstance` server, the client-fallback root
    /// otherwise. Documents under marker roots resolve *marker* keys and so
    /// will not reuse this connection; that limit is recorded on the config
    /// field itself.
    ///
    /// Each acquire is **detached**, and that is the whole reason this
    /// function does not await: an acquire runs the LSP handshake to
    /// completion before it returns, up to the initialization timeout, and a
    /// warm-up must never hold settings publication — or the reload lock —
    /// for a heavy server's whole startup, let alone for a fleet of them.
    /// Failures are logged there and never propagated here.
    ///
    /// The acquires are also **untracked**, unlike the eager-open tasks this
    /// coordinator registers abort handles for. They need no abort path: the
    /// pool refuses new spawns once shutdown begins, and it checks that inside
    /// the same `connections` lock `shutdown_all` takes to snapshot, so a
    /// racing warm-up is either rejected or already in the snapshot.
    ///
    /// bridge-routing-protocol
    /// additionally asks that a `forceStart` slot be *registered* before the
    /// configuration mandating it becomes observable to `didOpen`, so that a
    /// racing first open cannot enumerate an empty provider set. Nothing here
    /// provides that fence: the insertion happens inside the detached
    /// acquire. It is deferred deliberately — the guarantee is only
    /// observable once routing decisions and their bindings exist, and buying
    /// it now would mean either blocking publication on process startup or a
    /// third acquire variant with no caller to justify it.
    pub(crate) fn force_start_servers(&self, settings: &WorkspaceSettings) -> usize {
        use std::sync::atomic::Ordering;

        let servers = &settings.language_servers;
        let wildcard = servers.get(crate::config::WILDCARD_KEY);
        // Claim this pass's generation before launching anything, so every
        // task it spawns can tell whether it still speaks for the current
        // configuration by the time it reaches the pool. A caller that
        // superseded earlier in the same transaction claims again here, which
        // is harmless: only the newest value admits anything.
        let generation = self.force_start_generation.fetch_add(1, Ordering::Relaxed) + 1;
        // Deterministic order, so the log reads the same way every session
        // where the config map's iteration order would not. It orders the
        // launches only — the acquires themselves are detached and race.
        let mut names: Vec<&String> = servers
            .keys()
            .filter(|name| name.as_str() != crate::config::WILDCARD_KEY)
            .collect();
        names.sort();

        let mut launched = 0;
        for name in names {
            // Gate on the two fields alone before merging anything. Resolving
            // a whole config clones every field and deep-merges the wildcard's
            // `settings` and `initializationOptions` JSON, and this runs for
            // every configured server on every settings application — so the
            // merge belongs on the servers that actually force-start, not on
            // the fleet. (The same reason `is_spawnable_with_wildcard` exists.)
            let Some(config) = servers.get(name) else {
                continue;
            };
            // Spawnability outranks the flag: `forceStart` says when a
            // configured server starts, not whether a disabled or
            // command-less one may.
            if !config.forces_start_with_wildcard(wildcard)
                || !config.is_spawnable_with_wildcard(wildcard)
            {
                continue;
            }
            let Some(config) = resolve_reload_server_config(settings, name) else {
                continue;
            };

            let pool = Arc::clone(&self.pool);
            let name = name.clone();
            let current_generation = Arc::clone(&self.force_start_generation);
            #[cfg(test)]
            let test_control = self.force_start_test_control.load_full();
            tokio::spawn(async move {
                #[cfg(test)]
                if let Some(control) = &test_control {
                    control.before_admission.notify_one();
                    control.release_admission.notified().await;
                }
                // The config in hand was resolved from the settings snapshot
                // of this pass. If a later application has already run, that
                // snapshot is history: spawning from it would start a server
                // configuration no longer names, and — worse — the pool reads
                // a differing launch config as a change, so a stale task would
                // tear down the correctly-configured connection and replace it
                // with the old command. The newer pass re-asserts every flag,
                // so standing down costs nothing.
                //
                // The check is handed to the pool rather than made here,
                // because here is too early: the acquire takes the pool lock
                // afterwards, and whoever holds it may be the very pass that
                // supersedes this one. Evaluated inside that lock, it lands
                // ahead of the launch-config comparison it exists to prevent.
                let admit = || current_generation.load(Ordering::Relaxed) == generation;
                let result = pool
                    .get_or_create_connection_admitted(&name, &config, None, &admit)
                    .await;
                #[cfg(test)]
                if let Some(control) = &test_control {
                    control.admission_finished.notify_one();
                }
                let Err(error) = result else {
                    return;
                };
                // Two errors mean "someone else is already doing this", not
                // failure: a previous application's acquire is still shaking
                // hands, or the pool is shutting down. Both are ordinary — a
                // client that pushes configuration right after `initialized`
                // hits the first one every session — and reporting them as
                // failures would train the user to ignore the message.
                if is_concurrent_acquire(&error) {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "forceStart for '{name}' deferred to an acquire already in flight: {error}"
                    );
                    return;
                }
                log::warn!(
                    target: "kakehashi::bridge",
                    "forceStart could not start language server '{name}': {error}"
                );
                // The editor has to hear this one. A warm-up failure is
                // otherwise invisible by construction: with an unset
                // `RUST_LOG` the line above is filtered out, and a server
                // nothing routes to has no request whose failure would
                // surface it either.
                pool.warn_to_editor(format!(
                    "forceStart could not start language server '{name}': {error}"
                ));
            });
            launched += 1;
        }
        launched
    }

    /// Access the cancel forwarder.
    ///
    /// Used by:
    /// - `RequestIdCapture` middleware to receive the forwarder for the service layer
    /// - Handlers that want to subscribe to cancel notifications via `subscribe()`
    pub(crate) fn cancel_forwarder(&self) -> &CancelForwarder {
        &self.cancel_forwarder
    }

    /// Insert a ready test connection into the pool.
    ///
    /// Used by higher-level LSP tests that need eager-open behavior without
    /// depending on a real downstream language server.
    #[cfg(test)]
    pub(crate) async fn insert_ready_test_connection(&self, server_name: &str) {
        use crate::lsp::bridge::pool::ConnectionKey;
        use crate::lsp::bridge::pool::ConnectionState;
        use crate::lsp::bridge::pool::test_helpers::create_handle_with_key;

        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server(server_name),
        )
        .await;
        self.pool.insert_connection(handle).await;
    }

    /// Register a virtual document as opened, so [`Self::resolve_virtual_uri`]
    /// can recover its host and region for a test-driven region push, without
    /// a real downstream connection.
    #[cfg(test)]
    pub(crate) async fn register_opened_document_for_test(
        &self,
        host_uri: &Url,
        virtual_uri: &crate::lsp::bridge::protocol::VirtualDocumentUri,
        connection_key: &crate::lsp::bridge::pool::ConnectionKey,
    ) {
        self.pool
            .register_opened_document(host_uri, virtual_uri, connection_key)
            .await
    }

    /// Bump a virtual document's tracked version, as a content-changing
    /// `didChange` forward would (test helper for the applyEdit version
    /// validation).
    #[cfg(test)]
    pub(crate) async fn increment_document_version_for_test(
        &self,
        virtual_uri: &crate::lsp::bridge::protocol::VirtualDocumentUri,
        connection_key: &crate::lsp::bridge::pool::ConnectionKey,
    ) -> Option<i32> {
        self.pool
            .increment_document_version(virtual_uri, connection_key)
            .await
    }

    // ========================================
    // Config lookup (moved from Kakehashi)
    // ========================================

    /// Await eager-opening ONLY `server_name`'s virtual documents for `host_uri`,
    /// so a bridged `workspace/executeCommand` routed to a respawned downstream
    /// (whose doc tracker was purged) doesn't compute against missing document
    /// state. Unlike the request path, executeCommand has no
    /// `ensure_document_opened` step; unlike [`Self::eager_spawn_and_open_documents`]
    /// (fire-and-forget), this is AWAITED so that, WHEN a `didOpen` is enqueued,
    /// it lands on the shared single-writer connection before the caller sends
    /// the command (FIFO → didOpen first). The open is best-effort:
    /// `eager_open_virtual_documents` may skip or return early (downstream not
    /// ready, outbound queue full), in which case no `didOpen` is queued and the
    /// command simply proceeds without it (handled fail-soft by dispatch). A
    /// no-op when the docs are already open (idempotent claim), and when no
    /// injection maps to `server_name` (e.g. a host-layer command — host-layer
    /// sync is a separate follow-up).
    ///
    /// This heals MISSING document state (a purged tracker), not stale content —
    /// it never sends `didChange` (that is the edit path's job). And it is
    /// best-effort against a *concurrent* respawn: if the downstream is replaced
    /// in the narrow gap between this open and the caller's own connection
    /// acquisition, the command can still execute against missing state (the
    /// fresh server may error, no-op, or act on incomplete state); any failure
    /// is handled fail-soft by the existing dispatch path. Still a far smaller
    /// window than the codeAction↔executeCommand gap this closes.
    pub(crate) async fn ensure_server_documents_open(
        &self,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
        host_uri: &Url,
        expect: super::text_document::OpenExpectation<'_>,
        injections: Vec<BridgeInjection>,
        server_name: &str,
    ) -> super::text_document::OpenOutcome {
        use super::text_document::OpenOutcome;
        let Ok(host_uri_lsp) = crate::lsp::lsp_impl::url_to_uri(host_uri) else {
            return OpenOutcome::NotOpened;
        };
        let routed = self
            .route_virtual_injections(
                settings,
                host_language,
                host_uri,
                &host_uri_lsp,
                injections,
                Some(server_name),
            )
            .await;
        let mut config = None;
        let for_server = routed
            .into_iter()
            .filter_map(|(injection, configs)| {
                let resolved = configs
                    .into_iter()
                    .find(|resolved| resolved.server_name == server_name)?;
                config.get_or_insert(Arc::clone(&resolved.config));
                Some(injection)
            })
            .collect::<Vec<_>>();
        let Some(config) = config else {
            // No injected region on this host bridges to `server_name`, so this
            // host supplies nothing for that server on any connection. Pure
            // config — resolved from the memo, before any pool lookup or marker
            // walk, so the hosts that bridge nowhere near this server cost
            // nothing to reject.
            return OpenOutcome::NotApplicable;
        };
        // A repair is for one concrete connection. The same host can have
        // injections routed to several keys, so do not pass the whole server
        // batch to `eager_open_virtual_documents` and let its first injection
        // represent the rest. The normal eager path is partitioned earlier;
        // this filters the respawn path to the key being repaired.
        let for_server = if let Some(expected_key) = expect.connection {
            let mut matching = Vec::new();
            for injection in for_server {
                let virtual_uri = super::protocol::VirtualDocumentUri::new(
                    &host_uri_lsp,
                    &injection.language,
                    &injection.region_id,
                );
                let Ok(routing_uri) = Url::parse(&virtual_uri.to_uri_string()) else {
                    continue;
                };
                let routed_key = self
                    .pool
                    .resolved_connection_key(server_name, &config, &routing_uri)
                    .await;
                if &routed_key == expected_key {
                    matching.push(injection);
                }
            }
            matching
        } else {
            for_server
        };
        if for_server.is_empty() {
            return OpenOutcome::NotApplicable;
        }
        self.pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                host_uri,
                &host_uri_lsp,
                expect,
                for_server,
            )
            .await
    }

    fn injection_open_on_connection(
        &self,
        host_uri_lsp: &tower_lsp_server::ls_types::Uri,
        connection_key: &super::pool::ConnectionKey,
        injection: &BridgeInjection,
    ) -> bool {
        let virtual_uri = super::protocol::VirtualDocumentUri::new(
            host_uri_lsp,
            &injection.language,
            &injection.region_id,
        );
        self.pool
            .get_all_connections_for_virtual_uri(&virtual_uri)
            .contains(connection_key)
    }

    /// The injections whose language bridges to `server_name`, plus that server's
    /// resolved config. A codeAction fans out to ALL servers bridging an
    /// injection language, so the command's origin may be ANY of them — match by
    /// name against the full set ([`Self::get_all_configs_for_language`]). A
    /// single first pick would miss the origin when e.g. both ruff and pyright
    /// bridge python and the command came from ruff. Pure; the async open is
    /// separate so it is unit-testable.
    #[cfg(test)]
    fn injections_for_server(
        &self,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
        injections: Vec<BridgeInjection>,
        server_name: &str,
    ) -> (Vec<BridgeInjection>, Option<Arc<BridgeServerConfig>>) {
        let mut config: Option<Arc<BridgeServerConfig>> = None;
        // Resolve via the per-settings-snapshot memo
        // ([`Self::cached_configs_for_injection_language`]): several injections
        // commonly share an injection language, and the cache also spans repeated
        // executeCommands on the same snapshot, so the scan/merge/sort runs once
        // per (host, injection) language rather than once per injection — this is
        // on the user-facing executeCommand path.
        let for_server = injections
            .into_iter()
            .filter(|inj| {
                match self
                    .cached_configs_for_injection_language(settings, host_language, &inj.language)
                    .into_iter()
                    .find(|r| r.server_name == server_name)
                {
                    Some(resolved) => {
                        config.get_or_insert(resolved.config);
                        true
                    }
                    None => false,
                }
            })
            .collect();
        (for_server, config)
    }

    /// Memo-resolving front for [`Self::get_all_configs_for_language`] /
    /// [`Self::get_host_configs_for_language`]: returns the memoized result
    /// for the current settings snapshot, computing (and caching) it on
    /// first use. Callers on request paths — especially per-region loops —
    /// must use this instead of the raw resolvers (see `config_memo`).
    fn cached_configs(
        &self,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
        injection_language: Option<&str>,
    ) -> Vec<ResolvedServerConfig> {
        let memo = self.config_memo.load();
        let memo = if memo
            .settings
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, settings))
        {
            arc_swap::Guard::into_inner(memo)
        } else {
            // New settings snapshot: swap in a fresh generation and keep
            // USING the locally-built Arc rather than re-loading. A re-load
            // could return a memo a racing caller anchored to a DIFFERENT
            // (newer) snapshot — inserting this caller's configs (computed
            // from ITS settings) there would poison every later hit for that
            // snapshot until the next reload. Inserting into our own anchor
            // is always self-consistent: if a newer anchor replaced it in
            // the cell, our inserts are simply invisible to its callers (one
            // wasted compute, no wrong serve).
            let fresh = Arc::new(ConfigMemo::empty(Some(Arc::clone(settings))));
            self.config_memo.store(Arc::clone(&fresh));
            fresh
        };
        match injection_language {
            Some(injection_language) => {
                if let Some(hit) = memo.virt.get(host_language)
                    && let Some((_, configs)) = hit
                        .value()
                        .iter()
                        .find(|(lang, _)| lang == injection_language)
                {
                    return configs.as_ref().clone();
                }
                let configs =
                    self.get_all_configs_for_language(settings, host_language, injection_language);
                let entry = (injection_language.to_string(), Arc::new(configs.clone()));
                // `get_mut` first (borrowed key; `entry()` would clone the
                // host Url-sized String even on a present host), and skip the
                // push when a racing compute already recorded this pair.
                if let Some(mut pairs) = memo.virt.get_mut(host_language) {
                    if !pairs.iter().any(|(lang, _)| lang == injection_language) {
                        pairs.push(entry);
                    }
                } else {
                    let mut pairs = memo.virt.entry(host_language.to_string()).or_default();
                    // Re-check under the entry lock: two racing misses for
                    // the same host both fall into this branch; the loser
                    // must not append a duplicate pair.
                    if !pairs.iter().any(|(lang, _)| lang == injection_language) {
                        pairs.push(entry);
                    }
                }
                configs
            }
            None => {
                if let Some(hit) = memo.host.get(host_language) {
                    return hit.value().as_ref().clone();
                }
                let configs = self.get_host_configs_for_language(settings, host_language);
                memo.host
                    .insert(host_language.to_string(), Arc::new(configs.clone()));
                configs
            }
        }
    }

    /// Memoized [`Self::get_all_configs_for_language`] for the current
    /// settings snapshot.
    pub(crate) fn cached_configs_for_injection_language(
        &self,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
        injection_language: &str,
    ) -> Vec<ResolvedServerConfig> {
        self.cached_configs(settings, host_language, Some(injection_language))
    }

    /// Whether a document whose HOST language is `host_language` could bridge
    /// any injection to `server_name` at all, under current settings.
    ///
    /// Pure configuration, answered from the same per-snapshot memo the open
    /// path uses: no pool lookup, no marker walk, no tree. It exists so the
    /// respawn re-open can reject a candidate host before paying for one —
    /// deriving the target set means asking about EVERY open document, and the
    /// answer is "no" for most of them. Without this the per-host parse wait
    /// and injection resolution run first, so the barrier's fixed budget is
    /// spent in proportion to workspace size rather than to the work that
    /// belongs to the connection (respawn-reopen-derives-its-targets).
    ///
    /// Conservative in the safe direction: a server declaring the `*` wildcard
    /// could serve any injection language, so it is never pre-rejected.
    pub(crate) fn host_language_can_reach_server(
        &self,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
        server_name: &str,
    ) -> bool {
        let Some(config) = settings.language_servers.get(server_name) else {
            return false;
        };
        // Resolve `_` inheritance: `languages` is `#[serde(default)]`, so a
        // server that omits it reads as declaring NOTHING until the wildcard
        // template is merged in — and the authoritative resolver merges before
        // matching. Reading the raw list here would answer "reaches nothing"
        // for a server that reaches everything, and because this screen only
        // ever SKIPS, that false negative is silent: no didOpen, no failure
        // reported, and the barrier releases commands onto an empty connection.
        let effective_languages = config.effective_languages_with_wildcard(
            settings.language_servers.get(crate::config::WILDCARD_KEY),
        );
        effective_languages.iter().any(|injection_language| {
            injection_language == crate::config::settings::LANGUAGES_WILDCARD
                || self
                    .cached_configs_for_injection_language(
                        settings,
                        host_language,
                        injection_language,
                    )
                    .iter()
                    .any(|resolved| resolved.server_name == server_name)
        })
    }

    /// Memoized [`Self::get_host_configs_for_language`] for the current
    /// settings snapshot.
    pub(crate) fn cached_host_configs_for_language(
        &self,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
    ) -> Vec<ResolvedServerConfig> {
        self.cached_configs(settings, host_language, None)
    }

    /// Get all bridge server configs for a given injection language from settings.
    ///
    /// **All** servers configured for the injection language, not a preferred
    /// one: every consumer needs the full set — diagnostic fan-out to e.g.
    /// pyright + ruff, codeAction command routing back to whichever server
    /// produced it, and the eager open, where a push-only server that is
    /// skipped never receives the region at all.
    ///
    /// Results are sorted by server name for deterministic ordering.
    ///
    /// Returns an empty Vec if:
    /// - No servers are configured for this injection language, OR
    /// - The host language has a bridge filter that excludes this injection language
    pub(crate) fn get_all_configs_for_language(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        injection_language: &str,
    ) -> Vec<ResolvedServerConfig> {
        // Check the host's bridge filter before considering any server
        if let Some(host_settings) = settings.resolve_host_language_settings(host_language)
            && !host_settings.is_language_bridgeable(injection_language)
        {
            log::debug!(
                target: "kakehashi::bridge",
                "Bridge filter for {} blocks injection language {}",
                host_language,
                injection_language
            );
            return Vec::new();
        }

        let servers = &settings.language_servers;

        let mut results: Vec<ResolvedServerConfig> = servers
            .keys()
            .filter(|name| *name != "_")
            .filter_map(|server_name| {
                resolve_with_wildcard(servers, server_name, merge_bridge_server_configs)
                    .filter(|c| c.is_spawnable())
                    .filter(|c| c.handles_language(injection_language))
                    .map(|config| ResolvedServerConfig {
                        server_name: server_name.clone(),
                        config: Arc::new(config),
                    })
            })
            .collect();

        // Sort by server name for deterministic ordering
        results.sort_by(|a, b| a.server_name.cmp(&b.server_name));
        results
    }

    /// Get every server config that can act as a **host** bridge for the
    /// given host language (host-document-bridge).
    ///
    /// Selection mirrors [`Self::get_all_configs_for_language`] with the
    /// host-path matching rule: servers whose `languages` matches the *host*
    /// language itself, via `handles_language` — so a `"*"` server qualifies
    /// here too (any-language-server-wildcard). Gated on the explicit `bridge._self.enabled =
    /// true` opt-in ([`LanguageSettings::is_host_bridging_enabled`]) — a
    /// candidate server alone is not consent to use it.
    pub(crate) fn get_host_configs_for_language(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
    ) -> Vec<ResolvedServerConfig> {
        let enabled = settings
            .resolve_host_language_settings(host_language)
            .is_some_and(|host_settings| host_settings.is_host_bridging_enabled());
        if !enabled {
            return Vec::new();
        }

        let servers = &settings.language_servers;

        let mut results: Vec<ResolvedServerConfig> = servers
            .keys()
            .filter(|name| *name != "_")
            .filter_map(|server_name| {
                resolve_with_wildcard(servers, server_name, merge_bridge_server_configs)
                    .filter(|c| c.is_spawnable())
                    .filter(|c| c.handles_language(host_language))
                    .map(|config| ResolvedServerConfig {
                        server_name: server_name.clone(),
                        config: Arc::new(config),
                    })
            })
            .collect();

        // Sort by server name for deterministic ordering
        results.sort_by(|a, b| a.server_name.cmp(&b.server_name));
        results
    }

    // ========================================
    // Node tracker management (delegate to tracker)
    // ========================================

    /// Apply input edits to update region positions using START-priority invalidation.
    ///
    /// Returns ULIDs that were invalidated by this edit (for cleanup).
    pub(crate) fn apply_input_edits(&self, uri: &Url, edits: &[EditInfo]) -> Vec<Ulid> {
        self.node_tracker.apply_input_edits(uri, edits)
    }

    /// Apply text diff to update region positions.
    ///
    /// Used when InputEdits are not available (full document sync).
    /// Returns ULIDs that were invalidated.
    pub(crate) fn apply_text_diff(&self, uri: &Url, old_text: &str, new_text: &str) -> Vec<Ulid> {
        self.node_tracker.apply_text_diff(uri, old_text, new_text)
    }

    /// Remove all tracked regions for a document.
    ///
    /// Called on didClose to remove the closing incarnation while preserving
    /// entries a raced reopen already minted.
    pub(crate) fn cleanup(&self, uri: &Url, closing_incarnation: u64) {
        self.node_tracker.cleanup(uri, closing_incarnation)
    }

    pub(crate) fn open_tracker_incarnation(&self, uri: &Url, incarnation: u64) {
        self.node_tracker.open_incarnation(uri, incarnation);
    }

    // ========================================
    // Lifecycle (delegate to pool)
    // ========================================

    /// Close all virtual documents associated with a host document.
    ///
    /// Returns the list of closed virtual document URIs (useful for logging).
    pub(crate) async fn close_host_document(&self, uri: &Url) -> Vec<String> {
        self.pool
            .close_host_document(uri)
            .await
            .into_iter()
            .map(|doc| doc.virtual_uri.to_uri_string())
            .collect()
    }

    /// Close invalidated virtual documents.
    ///
    /// When region IDs are invalidated by edits, their corresponding virtual
    /// documents become orphaned in downstream LSs. This method sends didClose
    /// notifications.
    pub(crate) async fn close_invalidated_docs(&self, uri: &Url, ulids: &[Ulid]) {
        self.pool.close_invalidated_docs(uri, ulids).await;
    }

    pub(crate) async fn close_replaced_docs(
        &self,
        uri: &Url,
        injections: &[BridgeInjection],
    ) -> std::collections::HashSet<String> {
        self.pool.close_replaced_docs(uri, injections).await
    }

    /// Take the upstream notification receiver for forwarding to the editor.
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// Delegates to the underlying pool.
    pub(crate) fn take_upstream_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<super::actor::UpstreamNotification>> {
        self.pool.take_upstream_rx()
    }

    /// Take the bounded `window/*` notification receiver (#378).
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// Delegates to the underlying pool.
    pub(crate) fn take_window_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<super::actor::UpstreamNotification>> {
        self.pool.take_window_rx()
    }

    /// Take the upstream request receiver for forwarding to the editor.
    ///
    /// Returns `Some(receiver)` on first call, `None` on subsequent calls.
    /// Delegates to the underlying pool.
    pub(crate) fn take_upstream_request_rx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<super::actor::UpstreamRequest>> {
        self.pool.take_upstream_request_rx()
    }

    /// Graceful shutdown of all downstream language server connections.
    pub(crate) async fn shutdown_all(&self) {
        self.pool.shutdown_all().await;
    }

    /// Forward didChange notifications to opened virtual documents.
    ///
    /// Delegates to the pool's forward_didchange_to_opened_docs method.
    pub(crate) async fn forward_didchange_to_opened_docs(
        &self,
        uri: &Url,
        incarnation: u64,
        injections: &[BridgeInjection],
    ) {
        self.pool
            .forward_didchange_to_opened_docs(uri, incarnation, injections)
            .await;
    }

    pub(crate) async fn open_host_incarnation(&self, uri: &Url, incarnation: u64) {
        self.pool.open_host_incarnation(uri, incarnation).await;
    }

    pub(crate) async fn close_host_incarnation(&self, uri: &Url, incarnation: u64) {
        self.pool.close_host_incarnation(uri, incarnation).await;
    }

    // ========================================
    // Eager spawn + open (warmup with document content)
    // ========================================

    async fn route_virtual_injections(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        host_uri: &Url,
        host_uri_lsp: &tower_lsp_server::ls_types::Uri,
        injections: Vec<BridgeInjection>,
        target_server: Option<&str>,
    ) -> Vec<(BridgeInjection, Vec<ResolvedServerConfig>)> {
        let mut configs_by_lang: HashMap<String, Vec<ResolvedServerConfig>> = HashMap::new();
        let mut routed = Vec::with_capacity(injections.len());
        for injection in injections {
            let virtual_uri = super::protocol::VirtualDocumentUri::new(
                host_uri_lsp,
                &injection.language,
                &injection.region_id,
            );
            let Ok(document_uri) = Url::parse(&virtual_uri.to_uri_string()) else {
                continue;
            };
            let configs = configs_by_lang
                .entry(injection.language.clone())
                .or_insert_with(|| {
                    self.get_all_configs_for_language(settings, host_language, &injection.language)
                })
                .clone();
            let configs = match target_server {
                Some(target) => configs
                    .into_iter()
                    .filter(|config| config.server_name == target)
                    .collect(),
                None => configs,
            };
            if configs.is_empty() {
                self.pool.finish_virtual_routing(host_uri, &document_uri);
                continue;
            }
            let selected = Self::resolve_document_routing(
                &self.pool,
                &document_uri,
                &injection.language,
                Some(super::protocol::RoutingHostDocument {
                    uri: host_uri.to_string(),
                    language_id: host_language.to_string(),
                }),
                configs,
            )
            .await;
            self.pool.finish_virtual_routing(host_uri, &document_uri);
            routed.push((injection, selected));
        }
        routed
    }

    fn eager_open_groups_for_configs(
        routed: Vec<(BridgeInjection, Vec<ResolvedServerConfig>)>,
    ) -> BTreeMap<String, ServerGroup> {
        let mut groups: BTreeMap<String, ServerGroup> = BTreeMap::new();
        for (injection, configs) in routed {
            let Some((last, rest)) = configs.split_last() else {
                continue;
            };
            for config in rest {
                groups
                    .entry(config.server_name.clone())
                    .or_insert_with(|| (config.config.clone(), Vec::new()))
                    .1
                    .push(injection.clone());
            }
            groups
                .entry(last.server_name.clone())
                .or_insert_with(|| (last.config.clone(), Vec::new()))
                .1
                .push(injection);
        }
        groups
    }

    /// Which servers should receive an eager `didOpen`, and for which
    /// injections. Grouped by server name, since several languages can share
    /// one server (e.g. ts/tsx → tsgo).
    ///
    /// **Every** matching server, not a preferred one: the eager open is the
    /// only way a push-only server ever receives a region. It issues no
    /// request, so nothing opens the document lazily, and the pull path
    /// returns on its capability check before `ensure_document_opened`. Any
    /// ranking here would silently starve such a server whenever another
    /// server also matched the language — which, for an any-language server
    /// (any-language-server-wildcard), is every language that has a real
    /// server of its own.
    ///
    /// Resolves configs once per DISTINCT language rather than per region:
    /// resolution walks the wildcard+merge chain, and a fence-heavy document
    /// has hundreds of regions across a handful of languages — per-region
    /// resolution was a measured tokio-side hotspot (the caller runs on the
    /// runtime, and starving it delays every handler).
    ///
    /// Pure, so the selection is unit-testable; resolving connection keys and
    /// skipping already-open regions needs `await` and stays in the caller.
    #[cfg(test)]
    fn eager_open_groups(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        injections: Vec<BridgeInjection>,
    ) -> BTreeMap<String, ServerGroup> {
        let mut configs_by_lang: HashMap<String, Vec<ResolvedServerConfig>> = HashMap::new();
        let mut groups: BTreeMap<String, ServerGroup> = BTreeMap::new();

        for injection in injections {
            let resolved = configs_by_lang
                .entry(injection.language.clone())
                .or_insert_with(|| {
                    self.get_all_configs_for_language(settings, host_language, &injection.language)
                });

            // Hand the injection itself to the last server and clone only for
            // the others, so the overwhelmingly common one-server case stays a
            // move: `BridgeInjection` owns the region text, and this loop runs
            // on the tokio runtime over every region in the document.
            let Some((last, rest)) = resolved.split_last() else {
                continue;
            };
            for config in rest {
                groups
                    .entry(config.server_name.clone())
                    .or_insert_with(|| (config.config.clone(), Vec::new()))
                    .1
                    .push(injection.clone());
            }
            groups
                .entry(last.server_name.clone())
                .or_insert_with(|| (last.config.clone(), Vec::new()))
                .1
                .push(injection);
        }

        groups
    }

    /// Eagerly spawn language servers and open virtual documents for detected injections.
    ///
    /// Sending `didOpen` up front (not just a handshake) lets downstream servers
    /// start analyzing immediately, yielding faster diagnostics.
    pub(crate) async fn eager_spawn_and_open_documents(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        host_uri: &Url,
        incarnation: u64,
        injections: Vec<BridgeInjection>,
    ) {
        // Convert host_uri to ls_types::Uri for VirtualDocumentUri construction
        let host_uri_lsp = match crate::lsp::lsp_impl::url_to_uri(host_uri) {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Failed to convert host URI for eager open, skipping: {}",
                    e
                );
                return;
            }
        };

        // Empty means current settings resolve no server for any injection —
        // the batch belongs to removed configuration and must stop.
        let routed = self
            .route_virtual_injections(
                settings,
                host_language,
                host_uri,
                &host_uri_lsp,
                injections,
                None,
            )
            .await;
        let resolved_groups = Self::eager_open_groups_for_configs(routed);
        if resolved_groups.is_empty() {
            self.cancel_eager_open(host_uri);
            return;
        }

        // Drop the regions already open on each server's connection. A single
        // server can receive different routing answers for different virtual
        // documents, so group by the complete resolved connection key rather
        // than only by server name or rootless-ness.
        let mut server_groups: HashMap<ConnectionKey, (String, ServerGroup)> = HashMap::new();
        for (server_name, (config, group_injections)) in resolved_groups {
            for injection in group_injections {
                let routing_uri = super::protocol::VirtualDocumentUri::new(
                    &host_uri_lsp,
                    &injection.language,
                    &injection.region_id,
                );
                let Ok(routing_uri) = Url::parse(&routing_uri.to_uri_string()) else {
                    continue;
                };
                let connection_key = self
                    .pool
                    .resolved_connection_key(&server_name, &config, &routing_uri)
                    .await;
                let entry = server_groups
                    .entry(connection_key)
                    .or_insert_with(|| (server_name.clone(), (Arc::clone(&config), Vec::new())));
                entry.1.1.push(injection);
            }
        }

        let mut pending_groups: HashMap<ConnectionKey, (String, ServerGroup)> = HashMap::new();
        for (connection_key, (server_name, (config, group_injections))) in server_groups {
            let pending: Vec<BridgeInjection> = group_injections
                .into_iter()
                .filter(|injection| {
                    !self.injection_open_on_connection(&host_uri_lsp, &connection_key, injection)
                })
                .collect();
            if !pending.is_empty() {
                pending_groups.insert(connection_key, (server_name, (config, pending)));
            }
        }
        let server_groups = pending_groups;

        // Every resolved injection is already sent/open — preserve the batch.
        if server_groups.is_empty() {
            return;
        }

        // Supersede previous batch: abort + insert empty placeholder BEFORE spawning.
        // This closes the race window between spawn and registration. The returned
        // token (stored in the batch, already in the map) is `select!`ed on by each
        // task body to close the spawn→register window the abort handle can't reach (#435).
        let (generation, cancel) = self.supersede_eager_open_tasks(host_uri);

        // Spawn one task per server group, registering each handle immediately
        for (connection_key, (server_name, (config, group_injections))) in server_groups {
            log::debug!(
                target: "kakehashi::bridge",
                "Eager open: spawning {} on {} with {} injections",
                server_name,
                connection_key,
                group_injections.len()
            );

            let pool = self.pool_arc();
            let host_uri_owned = host_uri.clone();
            let host_uri_lsp = host_uri_lsp.clone();
            let cancel = cancel.clone();

            let task = tokio::spawn(async move {
                tokio::select! {
                    biased;
                    // Cancelled during the spawn→register window (or later) —
                    // bail before the side effect.
                    _ = cancel.cancelled() => {}
                    _ = pool.eager_open_virtual_documents(
                        &server_name,
                        &config,
                        &host_uri_owned,
                        &host_uri_lsp,
                        super::text_document::OpenExpectation {
                            incarnation,
                            // The eager batch opens wherever the host routes now.
                            connection: None,
                        },
                        group_injections,
                    ) => {}
                }
            });

            // Register immediately — if concurrent cancel removed the entry
            // or the generation is stale, the handle is aborted instead of leaked.
            self.push_or_abort_eager_open_handle(host_uri, task.abort_handle(), generation);
        }
    }

    /// Eagerly open the real host document on every `_self` host-bridge server for
    /// `host_language` (host-document-bridge, #429), so a push-only host server
    /// starts analyzing and pushing diagnostics on `didOpen` instead of only after
    /// the first host-bridged request. No-op (and cancels any prior batch) when
    /// host bridging is off for the language.
    pub(crate) fn eager_open_host_document_on_servers(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        host_uri: &Url,
        text: &str,
    ) {
        let configs = self.get_host_configs_for_language(settings, host_language);
        // Initial open: the snapshot text is current and there is no concurrent
        // re-sync to race, so no live reader is needed.
        self.eager_sync_host_document_on_servers(
            host_uri,
            host_language,
            Arc::from(text),
            configs,
            None,
        );
    }

    /// Ask one advertising host bridge for a routing decision over the full
    /// candidate set, then mark every candidate connection with that decision
    /// before any host `didOpen` is sent. This is deliberately one orchestration
    /// step: asking each server about a projection containing only itself cannot
    /// let a provider such as tsudoi suppress a sibling provider such as tsgo.
    async fn resolve_document_routing(
        pool: &LanguageServerPool,
        document_uri: &Url,
        language_id: &str,
        host: Option<super::protocol::RoutingHostDocument>,
        configs: Vec<ResolvedServerConfig>,
    ) -> Vec<ResolvedServerConfig> {
        if configs.iter().all(|config| {
            pool.host_routing_by_server(document_uri, &config.server_name)
                .is_some()
        }) {
            return configs
                .into_iter()
                .filter(|config| {
                    pool.host_routing_by_server(document_uri, &config.server_name)
                        .unwrap_or(true)
                })
                .collect();
        }
        let language_servers = configs
            .iter()
            .map(|config| {
                let workspace_markers = config
                    .config
                    .workspace_markers
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|marker| serde_json::to_value(marker).expect("RootMarker is serializable"))
                    .collect();
                (
                    config.server_name.clone(),
                    RoutingLanguageServer {
                        languages: config.config.languages.clone().unwrap_or_default(),
                        workspace_markers,
                        prefer_shared_instance: config
                            .config
                            .prefer_shared_instance
                            .unwrap_or(false),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let params = RoutingParams {
            text_document: RoutingTextDocument {
                uri: document_uri.to_string(),
                language_id: language_id.to_string(),
                host,
            },
            language_servers,
        };

        let mut candidates = futures::stream::FuturesUnordered::new();
        for config in &configs {
            let server_name = config.server_name.clone();
            let server_config = Arc::clone(&config.config);
            let document_uri = document_uri.clone();
            candidates.push(async move {
                let result = pool
                    .get_or_create_connection_wait_ready(
                        &server_name,
                        &server_config,
                        Some(&document_uri),
                        std::time::Duration::from_secs(INIT_TIMEOUT_SECS),
                    )
                    .await;
                (server_name, result)
            });
        }

        let mut handles = Vec::with_capacity(configs.len());
        let mut answer: Option<RoutingAnswer> = None;
        while let Some((server_name, result)) = candidates.next().await {
            let handle = match result {
                Ok(handle) => handle,
                Err(error) => {
                    log::debug!(
                        target: "kakehashi::bridge::routing",
                        "Routing candidate {} was not ready for {}: {}",
                        server_name,
                        document_uri,
                        error
                    );
                    continue;
                }
            };
            if answer.is_none() && handle.supports_bridge_routing() {
                match handle.request_routing(params.clone()).await {
                    Ok(Some(candidate_answer)) => answer = Some(candidate_answer),
                    Ok(None) => {}
                    Err(error) => log::debug!(
                        target: "kakehashi::bridge::routing",
                        "Routing provider {} failed for {}: {}",
                        server_name,
                        document_uri,
                        error
                    ),
                }
            }
            handles.push((server_name, handle));
        }

        let mut selected = Vec::new();
        for config in configs {
            let enabled = answer
                .as_ref()
                .and_then(|answer| answer.routing.get(&config.server_name))
                .and_then(|entry| entry.enabled)
                != Some(false);
            pool.set_host_routing_by_server(document_uri, &config.server_name, enabled);
            pool.set_host_routing_workspace_folders(
                document_uri,
                &config.server_name,
                answer
                    .as_ref()
                    .and_then(|answer| answer.routing.get(&config.server_name))
                    .and_then(|entry| entry.workspace_folders.clone()),
            );
            let rootless = answer
                .as_ref()
                .and_then(|answer| answer.routing.get(&config.server_name))
                .and_then(|entry| entry.workspace_folders.as_ref())
                .is_some_and(|folders| folders.as_ref().is_some_and(Vec::is_empty));
            pool.set_host_routing_rootless(document_uri, &config.server_name, rootless);
            let Some((_, handle)) = handles.iter().find(|(name, _)| name == &config.server_name)
            else {
                if enabled {
                    selected.push(config);
                }
                continue;
            };
            pool.set_host_routing_decided(document_uri, handle.key());
            if !enabled {
                pool.set_host_routing_suppressed(document_uri, handle.key());
                continue;
            }
            selected.push(config);
        }
        selected
    }

    /// Sync the real host document to a resolved set of `_self` host servers
    /// (host-document-bridge). `sync_host_document` sends `didOpen` the first time
    /// and a versioned `didChange` when the text changed, so this is used both for
    /// the eager open on `didOpen` (#429) and the eager **re-sync on edit** at the
    /// debounced diagnostic cadence (#431) — the latter is what keeps a push-only
    /// host server (skipped by the capability-gated pull) analyzing current text
    /// rather than stale text. Spawns one fire-and-forget task per server; no-op
    /// (and cancels any prior batch) when `configs` is empty.
    ///
    /// `language_id` is the downstream `languageId` — for a `_self` bridge that is
    /// the host language itself (consistent with `HostRequestContext.language_id`).
    /// `text` is taken as `Arc<str>` so the debounced re-sync path can hand over its
    /// existing `HostRequestContext.text` allocation (a cheap clone) rather than
    /// copying the full document on every fire.
    pub(crate) fn eager_sync_host_document_on_servers(
        &self,
        host_uri: &Url,
        language_id: &str,
        text: Arc<str>,
        configs: Vec<ResolvedServerConfig>,
        live_text_reader: Option<crate::lsp::bridge::HostTextReader>,
    ) {
        if configs.is_empty() {
            // Host bridging off / no host server for this language — drop any
            // prior batch so a stale sync can't fire.
            self.cancel_host_eager_open(host_uri);
            return;
        }

        // Supersede the previous batch (abort + reset to an empty placeholder)
        // BEFORE spawning, then register each handle against this generation. This
        // closes the *registration* leak: if a concurrent `cancel_host_eager_open`
        // (didClose) or `abort_all_eager_open` (shutdown) removed the entry, a
        // handle registered afterwards is aborted on the spot. The
        // body-started-before-registration window is also closed (#435): the batch's
        // `CancellationToken` is in the map before any task spawns, each task
        // `select!`s on it before its first side effect, and cancel/supersede/abort
        // cancel it.
        //
        // On-edit re-sync carries *different* text per fire, so a superseded task
        // emitting after a newer one could otherwise roll the host server back to
        // older text. The supersede above aborts a task still parked at
        // `get_or_create_connection_wait_ready`, closing the common case; the µs
        // residual — an older task unblocking from wait-ready at the instant a newer
        // task reaches the (await-free) sync — is closed by `live_text_reader`:
        // `sync_host_document` reads the document's *current* text under the
        // `host_documents` lock, so whichever task syncs last sends the latest text,
        // not the snapshot it was spawned with (#422). The `text` snapshot remains
        // the fallback when no reader is supplied (initial open) or it yields `None`.
        let (generation, cancel) = self.supersede_host_eager_open(host_uri);

        // Share the text + languageId across per-server tasks via `Arc<str>` rather
        // than cloning the (potentially large) document text once per host server.
        // `text` already arrives as `Arc<str>` (the debounce path hands over its
        // `HostRequestContext.text` without copying).
        let language_id: Arc<str> = Arc::from(language_id);
        let pool = self.pool_arc();
        let host_uri_owned = host_uri.clone();
        let configs_for_routing = configs;
        let routing_sender = pool.begin_host_routing(host_uri);
        let task = tokio::spawn(async move {
            let configs = tokio::select! {
                _ = cancel.cancelled() => {
                    pool.finish_host_routing(&host_uri_owned, &routing_sender);
                    return;
                }
                configs = async {
                    let lifecycle = pool.host_lifecycle_lock(&host_uri_owned);
                    let _guard = lifecycle.lock().await;
                    Self::resolve_document_routing(
                        &pool,
                        &host_uri_owned,
                        &language_id,
                        None,
                        configs_for_routing,
                    ).await
                } => configs,
            };
            pool.finish_host_routing(&host_uri_owned, &routing_sender);
            let mut opens = Vec::new();
            for config in configs {
                let pool = Arc::clone(&pool);
                let host_uri = host_uri_owned.clone();
                let language_id = Arc::clone(&language_id);
                let text = Arc::clone(&text);
                let cancel = cancel.clone();
                let live_text_reader = live_text_reader.clone();
                opens.push(tokio::spawn(async move {
                    let server_name = config.server_name;
                    let server_config = config.config;
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = pool.eager_open_host_document(
                            &server_name,
                            &server_config,
                            &host_uri,
                            &language_id,
                            &text,
                            live_text_reader.as_deref(),
                        ) => {}
                    }
                }));
            }
            for open in opens {
                let _ = open.await;
            }
        });
        self.push_or_abort_host_eager_open_handle(host_uri, task.abort_handle(), generation);
    }

    /// Supersede the host eager-open batch for `uri` (abort old handles + cancel the
    /// old token + reset to an empty placeholder under one shard lock), returning the
    /// new generation **and the batch's fresh `CancellationToken`** to hand to each
    /// task it spawns (#435). Mirrors `supersede_eager_open_tasks` for the host path.
    fn supersede_host_eager_open(&self, uri: &Url) -> (u64, CancellationToken) {
        let generation = self
            .host_eager_open_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        use dashmap::mapref::entry::Entry;
        match self.host_eager_open_tasks.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                // Cancel the OLD batch's token first — this bails any task whose
                // body already started before its handle registered (#435) — then
                // install a fresh token for the new generation.
                let batch = entry.get_mut();
                batch.cancel.cancel();
                batch.cancel = CancellationToken::new();
                let prev = std::mem::take(&mut batch.handles);
                batch.generation = generation;
                for handle in prev {
                    if !handle.is_finished() {
                        handle.abort();
                    }
                }
                (generation, batch.cancel.clone())
            }
            Entry::Vacant(entry) => {
                let batch = entry.insert(EagerOpenBatch {
                    generation,
                    handles: Vec::new(),
                    cancel: CancellationToken::new(),
                });
                (generation, batch.cancel.clone())
            }
        }
    }

    /// Push a host eager-open abort handle into its batch, or abort it if the entry
    /// was removed (cancel/shutdown) or its generation is stale (a newer batch
    /// superseded it). Mirrors `push_or_abort_eager_open_handle`.
    fn push_or_abort_host_eager_open_handle(
        &self,
        uri: &Url,
        handle: tokio::task::AbortHandle,
        expected_generation: u64,
    ) {
        match self.host_eager_open_tasks.get_mut(uri) {
            Some(mut entry) if entry.value().generation == expected_generation => {
                entry.value_mut().handles.push(handle);
            }
            _ => handle.abort(),
        }
    }

    /// Abort and forget the host-layer eager-open tasks for `host_uri` (host
    /// `didClose`). MUST run before the host document is closed so an in-flight
    /// task still waiting for server readiness can't open a doc whose `didClose`
    /// already ran.
    pub(crate) fn cancel_host_eager_open(&self, host_uri: &Url) {
        if let Some((_, batch)) = self.host_eager_open_tasks.remove(host_uri) {
            // Cancel the token so a body that already started (before its handle
            // registered) bails before its side effect (#435).
            batch.cancel.cancel();
            for handle in batch.handles {
                if !handle.is_finished() {
                    handle.abort();
                }
            }
        }
    }

    // ========================================
    // Eager-open task cancellation
    // ========================================

    /// Supersede previous eager-open tasks for a URI, returning the new batch's
    /// generation counter (passed to `push_or_abort_eager_open_handle` to detect
    /// stale pushes) **and the batch's fresh `CancellationToken`** to hand to each
    /// task it spawns — cancelled by a concurrent cancel/supersede/abort so a task
    /// whose body started before its handle registered still bails (#435).
    ///
    /// Uses `DashMap::entry()` so the abort-and-reset happens under a single shard
    /// lock. Must be called BEFORE spawning new tasks to close the race window
    /// between spawn and handle registration.
    fn supersede_eager_open_tasks(&self, uri: &Url) -> (u64, CancellationToken) {
        let generation = self
            .eager_open_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        use dashmap::mapref::entry::Entry;
        match self.eager_open_tasks.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                // Cancel the OLD batch's token first — this bails any task whose
                // body already started before its handle registered (#435) — then
                // install a fresh token for the new generation.
                let batch = entry.get_mut();
                batch.cancel.cancel();
                batch.cancel = CancellationToken::new();
                let prev_handles = std::mem::take(&mut batch.handles);
                batch.generation = generation;
                let mut aborted = 0;
                for handle in prev_handles {
                    if !handle.is_finished() {
                        handle.abort();
                        aborted += 1;
                    }
                }
                if aborted > 0 {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "Aborted {} previous eager-open tasks for {} (superseded by new batch, gen={})",
                        aborted,
                        uri,
                        generation
                    );
                }
                (generation, batch.cancel.clone())
            }
            Entry::Vacant(entry) => {
                let batch = entry.insert(EagerOpenBatch {
                    generation,
                    handles: Vec::new(),
                    cancel: CancellationToken::new(),
                });
                (generation, batch.cancel.clone())
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn begin_test_eager_open_batch(&self, uri: &Url) -> CancellationToken {
        self.supersede_eager_open_tasks(uri).1
    }

    /// Whether every eager-open task registered for `uri` has finished
    /// (no batch counts as finished).
    ///
    /// Used by CLI-mode formatting to serialize with eager opens: an
    /// eager-open task claims each virtual document BEFORE its `didOpen`
    /// reaches the writer queue (`is_document_opened` is true pre-send), so
    /// a formatting request issued in that window skips its own `didOpen`
    /// and overtakes the eager one on the wire. Once the tasks are finished,
    /// every `didOpen` is enqueued and the single-writer FIFO
    /// (ls-bridge-message-ordering) keeps later requests behind them.
    ///
    /// Precondition: the `didOpen` that triggered the eager spawn has been
    /// **awaited to completion** (CLI mode awaits `did_open_impl`, which
    /// registers every handle before returning). `supersede` inserts an
    /// empty placeholder batch before the handles are pushed, so a caller
    /// polling *concurrently with registration* could observe a zero-handle
    /// batch as "finished"; conversely an empty batch must stay "finished"
    /// here, because documents with no bridge-capable injections keep zero
    /// handles forever and treating that as pending would stall them for
    /// the caller's whole timeout.
    pub(crate) fn eager_open_tasks_finished(&self, uri: &Url) -> bool {
        self.eager_open_tasks
            .get(uri)
            .is_none_or(|batch| batch.handles.iter().all(|h| h.is_finished()))
    }

    /// Push an abort handle into an existing entry, or abort it if stale/removed.
    ///
    /// Called immediately after each `tokio::spawn`. The handle is aborted (not
    /// registered) if the entry was removed by a concurrent `cancel_eager_open`,
    /// or its generation doesn't match (a concurrent `supersede` replaced it).
    fn push_or_abort_eager_open_handle(
        &self,
        uri: &Url,
        handle: tokio::task::AbortHandle,
        expected_generation: u64,
    ) {
        match self.eager_open_tasks.get_mut(uri) {
            Some(mut entry) => {
                if entry.value().generation == expected_generation {
                    entry.value_mut().handles.push(handle);
                } else {
                    // Generation mismatch — a concurrent supersede replaced the batch
                    log::debug!(
                        target: "kakehashi::bridge",
                        "Aborting eager-open handle for {} (stale generation {} != current {})",
                        uri,
                        expected_generation,
                        entry.value().generation
                    );
                    handle.abort();
                }
            }
            None => {
                // Entry was removed by concurrent cancel — abort this task
                log::debug!(
                    target: "kakehashi::bridge",
                    "Aborting eager-open handle for {} (entry removed by concurrent cancel)",
                    uri
                );
                handle.abort();
            }
        }
    }

    /// Cancel all eager-open tasks for a document.
    ///
    /// Called on didClose to prevent orphaned virtual documents when tasks
    /// are still waiting for server readiness.
    pub(crate) fn cancel_eager_open(&self, uri: &Url) {
        if let Some((_, batch)) = self.eager_open_tasks.remove(uri) {
            log::debug!(
                target: "kakehashi::bridge",
                "Cancelling {} eager-open tasks for {} (gen={})",
                batch.handles.len(),
                uri,
                batch.generation
            );
            // Cancel the token so a body that already started (before its handle
            // registered) bails before its side effect (#435).
            batch.cancel.cancel();
            for handle in batch.handles {
                handle.abort();
            }
        }
    }

    /// Abort all eager-open tasks (called during shutdown).
    ///
    /// Ensures clean shutdown by cancelling all background tasks that may
    /// still be waiting for server readiness.
    ///
    /// Uses `DashMap::retain` to abort handles and remove entries under the
    /// same per-shard write lock, so no task can be inserted-then-cleared
    /// without being aborted — even if called outside a strict shutdown window.
    pub(crate) fn abort_all_eager_open(&self) {
        let mut count: usize = 0;
        self.eager_open_tasks.retain(|_uri, batch| {
            // Cancel the token too (#435): a body that started before its handle
            // registered isn't reachable via the handles below.
            batch.cancel.cancel();
            for handle in batch.handles.iter() {
                handle.abort();
                count += 1;
            }
            false // remove entry
        });
        // Drain the host-layer eager-open batch too (#429): otherwise a host
        // eager-open still waiting for server readiness could spawn a connection
        // or queue a didOpen during shutdown. `retain` aborts + removes under the
        // shard lock, so a handle being registered concurrently (spawn→register
        // window) either lands before this drain and is aborted here, or finds the
        // entry already gone and is aborted by `push_or_abort_host_eager_open_handle`.
        self.host_eager_open_tasks.retain(|_uri, batch| {
            // Cancel the token too (#435): a body that started before its handle
            // registered isn't reachable via the handles below.
            batch.cancel.cancel();
            for handle in batch.handles.iter() {
                handle.abort();
                count += 1;
            }
            false // remove entry
        });
        if count > 0 {
            log::debug!(
                target: "kakehashi::bridge",
                "Aborted {} eager-open tasks during shutdown",
                count
            );
        }
    }
}

impl Default for BridgeCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BridgeCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeCoordinator")
            .field("pool", &"LanguageServerPool")
            .field("node_tracker", &"NodeTracker")
            .field("cancel_forwarder", &"CancelForwarder")
            .field(
                "eager_open_tasks",
                &format!("{} entries", self.eager_open_tasks.len()),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::LanguageSettings;
    use crate::config::settings::{BridgeLanguageConfig, LANGUAGES_WILDCARD};
    use crate::lsp::bridge::ConnectionKey;
    use crate::lsp::bridge::pool::ConnectionState;

    #[test]
    fn reload_resolution_does_not_resurrect_deleted_server_from_wildcard() {
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            crate::config::WILDCARD_KEY.to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["shared-server".into()]),
                ..Default::default()
            },
        );

        assert!(resolve_reload_server_config(&settings, "deleted").is_none());
    }

    /// A server config whose command runs harmlessly forever: enough to
    /// occupy a connection slot without answering an LSP handshake.
    fn force_start_settings(entries: &[(&str, BridgeServerConfig)]) -> WorkspaceSettings {
        let mut settings = WorkspaceSettings::default();
        for (name, config) in entries {
            settings
                .language_servers
                .insert((*name).to_string(), config.clone());
        }
        settings
    }

    fn idle_server(force_start: bool, languages: Vec<String>) -> BridgeServerConfig {
        BridgeServerConfig {
            cmd: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat > /dev/null".to_string(),
            ]),
            languages: Some(languages),
            force_start: force_start.then_some(true),
            ..Default::default()
        }
    }

    /// The connection keys once `expected` acquires have reached the pool, and
    /// nothing more has since.
    ///
    /// `force_start_servers` detaches each acquire, and an acquire inserts its
    /// `Initializing` handle before running the handshake — which these idle
    /// servers never answer. So the assertion point is the insertion, reached
    /// by polling rather than by awaiting a readiness that will not come.
    ///
    /// The settle after the count is reached is what lets a caller assert that
    /// a server did *not* start: returning the instant `expected` appears
    /// would never observe a spurious connection landing a moment later.
    /// Timing out panics rather than returning a short list, so a loaded
    /// machine reports a timeout instead of an inscrutable wrong-value diff.
    async fn started_servers(coordinator: &BridgeCoordinator, expected: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let reached = coordinator.pool().connections().await.len() >= expected;
            assert!(
                reached || std::time::Instant::now() <= deadline,
                "timed out waiting for {expected} warm-up connection(s)"
            );
            if reached {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let connections = coordinator.pool().connections().await;
        let mut names: Vec<String> = connections
            .keys()
            .map(|key| key.server().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names.len(),
            expected,
            "a server outside the expected set started: {names:?}"
        );
        names
    }

    /// `forceStart` exists for servers nothing else would ever start: with
    /// `languages = []` the server is in no document's candidate set, so no
    /// lazy acquire can fire for it (bridge-routing-protocol's policy-server
    /// pattern). Without the flag the same entry stays unspawned.
    #[tokio::test]
    async fn force_start_spawns_a_server_no_document_would() {
        let coordinator = BridgeCoordinator::new();
        let settings = force_start_settings(&[
            ("policy-server", idle_server(true, vec![])),
            ("lazy-server", idle_server(false, vec!["lua".to_string()])),
        ]);

        assert_eq!(coordinator.force_start_servers(&settings), 1);

        assert_eq!(started_servers(&coordinator, 1).await, ["policy-server"]);
    }

    /// A reload re-runs the pass, and a server whose first acquire has not
    /// finished is left alone rather than double-spawned.
    ///
    /// This pins the *concurrent* case specifically: these idle servers never
    /// answer `initialize`, so the connection is still `Initializing` when the
    /// second pass arrives and the pool refuses the acquire outright. That is
    /// the branch a client which pushes configuration right after
    /// `initialized` takes every session, and the one whose error must not be
    /// reported to the user as a failure.
    #[tokio::test]
    async fn force_start_is_idempotent_across_reloads() {
        let coordinator = BridgeCoordinator::new();
        let settings = force_start_settings(&[("policy-server", idle_server(true, vec![]))]);

        assert_eq!(coordinator.force_start_servers(&settings), 1);
        started_servers(&coordinator, 1).await;
        let first = coordinator
            .pool()
            .connections()
            .await
            .values()
            .next()
            .map(Arc::clone)
            .expect("the first pass spawned a connection");

        assert_eq!(
            first.state(),
            ConnectionState::Initializing,
            "the case under test is a second pass arriving mid-handshake"
        );

        assert_eq!(coordinator.force_start_servers(&settings), 1);
        // Two acquires for one key serialize on the pool lock; the second must
        // leave the first's handle alone rather than spawn a second process.
        // Give it room to do the wrong thing before asserting it didn't.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let connections = coordinator.pool().connections().await;
        assert_eq!(connections.len(), 1, "no second process for the same key");
        assert!(
            Arc::ptr_eq(&first, connections.values().next().unwrap()),
            "the running connection survives, rather than being replaced"
        );
        drop(connections);
    }

    /// The refusal a second pass gets while the first is still shaking hands
    /// is not a failure, and must not be reported to the user as one — a
    /// client that pushes configuration right after `initialized` would
    /// otherwise produce a spurious warning every session.
    #[test]
    fn a_concurrent_acquire_is_not_a_forced_start_failure() {
        use crate::lsp::bridge::pool::BridgeError;

        assert!(is_concurrent_acquire(&BridgeError::Initializing.into()));
        assert!(
            is_concurrent_acquire(&BridgeError::Closing.into()),
            "a slot on its way down is a lifecycle transition, not a start failure"
        );
        assert!(is_concurrent_acquire(&std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "bridge pool is shutting down; rejecting new connection spawn",
        )));
        assert!(
            !is_concurrent_acquire(&std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory (os error 2)",
            )),
            "a missing command is exactly what the user has to be told about"
        );
        assert!(
            !is_concurrent_acquire(&BridgeError::Disabled.into()),
            "a server disabled after repeated handshake failures is a failure"
        );
    }

    /// A task carrying a superseded configuration must not reach the pool.
    ///
    /// The acquires are detached, so one launched by application N can arrive
    /// after N+1 has been applied. Spawning from a snapshot configuration no
    /// longer names is bad enough; worse, the pool reads a differing launch
    /// config as a change, so the stale task would tear down N+1's correctly
    /// configured connection and replace it with the old command.
    #[tokio::test]
    async fn force_start_stands_down_when_a_newer_configuration_arrives() {
        let evidence = tempfile::tempdir().expect("a temp dir for the marker file");
        let stale_ran = evidence.path().join("stale-command-ran");

        // The two passes differ in the one way that matters: only the stale
        // one's command leaves a trace. Counting connections cannot tell these
        // apart — both target the same key, so a stale task that DID reach the
        // pool would produce one connection either way, just the wrong one.
        let staged = |marker: Option<&std::path::Path>| {
            let script = match marker {
                Some(path) => format!("touch {}; cat > /dev/null", path.display()),
                None => "cat > /dev/null".to_string(),
            };
            force_start_settings(&[(
                "policy-server",
                BridgeServerConfig {
                    cmd: Some(vec!["sh".to_string(), "-c".to_string(), script]),
                    ..idle_server(true, vec![])
                },
            )])
        };

        let coordinator = BridgeCoordinator::new();
        // The generation is claimed synchronously, so by the time the first
        // pass's task body runs, the second pass has already superseded it —
        // which is exactly the ordering a rapid second settings application
        // produces.
        assert_eq!(
            coordinator.force_start_servers(&staged(Some(&stale_ran))),
            1
        );
        assert_eq!(coordinator.force_start_servers(&staged(None)), 1);

        assert_eq!(started_servers(&coordinator, 1).await, ["policy-server"]);
        assert!(
            !stale_ran.exists(),
            "the superseded task reached the pool and spawned its command"
        );
    }

    /// Spawnability outranks the flag: `forceStart` asks *when* a configured
    /// server starts, never whether a disabled or command-less one may.
    #[tokio::test]
    async fn force_start_never_starts_an_unspawnable_server() {
        let coordinator = BridgeCoordinator::new();
        let disabled = BridgeServerConfig {
            enabled: Some(false),
            ..idle_server(true, vec![])
        };
        let no_cmd = BridgeServerConfig {
            cmd: None,
            ..idle_server(true, vec![])
        };
        let settings = force_start_settings(&[("disabled", disabled), ("no-cmd", no_cmd)]);

        assert_eq!(coordinator.force_start_servers(&settings), 0);
        assert!(coordinator.pool().connections().await.is_empty());
    }

    /// The wildcard supplies the flag like any other field, and — as with
    /// `preferSharedInstance` — a concrete server can opt out of a blanket
    /// opt-in. The wildcard entry itself is a template, never a server.
    #[tokio::test]
    async fn force_start_inherits_the_wildcard_and_can_be_opted_out_of() {
        let coordinator = BridgeCoordinator::new();
        let mut settings = force_start_settings(&[
            ("inheritor", idle_server(false, vec!["lua".to_string()])),
            (
                "opted-out",
                BridgeServerConfig {
                    force_start: Some(false),
                    ..idle_server(false, vec!["lua".to_string()])
                },
            ),
        ]);
        settings.language_servers.insert(
            crate::config::WILDCARD_KEY.to_string(),
            BridgeServerConfig {
                force_start: Some(true),
                ..Default::default()
            },
        );

        assert_eq!(coordinator.force_start_servers(&settings), 1);
        assert_eq!(started_servers(&coordinator, 1).await, ["inheritor"]);
    }

    /// With no document there is no marker to walk, so the connection lands on
    /// the same key any document-less acquire produces: the shared key for a
    /// `preferSharedInstance` server, the client-fallback root otherwise. That
    /// is the honest scope of the warm-up, and the key is what a later
    /// document has to match to reuse the process.
    #[tokio::test]
    async fn force_start_lands_on_the_document_less_key() {
        let coordinator = BridgeCoordinator::new();
        let settings = force_start_settings(&[
            ("per-root", idle_server(true, vec![])),
            (
                "shared",
                BridgeServerConfig {
                    prefer_shared_instance: Some(true),
                    ..idle_server(true, vec![])
                },
            ),
        ]);

        assert_eq!(coordinator.force_start_servers(&settings), 2);
        started_servers(&coordinator, 2).await;

        let connections = coordinator.pool().connections().await;
        let mut keys: Vec<&ConnectionKey> = connections.keys().collect();
        assert_eq!(keys.len(), 2, "both warm-ups must have reached the pool");
        keys.sort_by_key(|key| key.server().to_string());
        assert!(
            keys[0].is_client_fallback(),
            "a per-root server with no document has no marker root to key on"
        );
        assert_eq!(keys[1], &ConnectionKey::shared("shared"));
        drop(connections);
    }

    /// Shutdown's `abort_all_eager_open` must drain the host-layer eager-open
    /// batch too (#429), not only the virt `eager_open_tasks`.
    #[tokio::test]
    async fn abort_all_eager_open_drains_host_batch() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///host_shutdown.lua").unwrap();
        // A never-completing task stands in for a host eager-open still waiting on
        // server readiness at shutdown.
        let task = tokio::spawn(std::future::pending::<()>());
        coordinator.host_eager_open_tasks.insert(
            uri.clone(),
            EagerOpenBatch {
                generation: 0,
                handles: vec![task.abort_handle()],
                cancel: CancellationToken::new(),
            },
        );
        assert!(coordinator.host_eager_open_tasks.contains_key(&uri));

        coordinator.abort_all_eager_open();

        assert!(
            coordinator.host_eager_open_tasks.is_empty(),
            "host eager-open batch must be drained on shutdown"
        );
        tokio::task::yield_now().await;
        assert!(task.is_finished(), "the host eager-open task was aborted");
    }

    /// #435: the per-batch `CancellationToken` returned by `supersede_*` is the
    /// only handle that reaches a task whose body started before its `AbortHandle`
    /// registered (the spawn→register window). A concurrent `cancel_eager_open` /
    /// `cancel_host_eager_open` must cancel that token so such a body bails before
    /// its side effect. Exercises the token+map mechanism directly (no spawn race).
    #[test]
    fn eager_open_token_cancels_body_started_before_handle_registers() {
        let coordinator = BridgeCoordinator::new();

        // Region path.
        let region_uri = Url::parse("file:///region.md").unwrap();
        let (_gen, region_token) = coordinator.supersede_eager_open_tasks(&region_uri);
        assert!(
            !region_token.is_cancelled(),
            "freshly superseded region token must not be cancelled"
        );
        assert!(
            coordinator.eager_open_tasks.contains_key(&region_uri),
            "supersede must leave the batch in the map for a concurrent cancel to reach"
        );
        // A task body that started before its handle registered holds a clone of
        // this token; cancelling it (didClose) must bail that body.
        coordinator.cancel_eager_open(&region_uri);
        assert!(
            region_token.is_cancelled(),
            "cancel_eager_open must cancel the batch token so an early-started body bails"
        );

        // Host path (mirrors the region path).
        let host_uri = Url::parse("file:///host.lua").unwrap();
        let (_gen, host_token) = coordinator.supersede_host_eager_open(&host_uri);
        assert!(
            !host_token.is_cancelled(),
            "freshly superseded host token must not be cancelled"
        );
        assert!(
            coordinator.host_eager_open_tasks.contains_key(&host_uri),
            "supersede must leave the host batch in the map for a concurrent cancel to reach"
        );
        coordinator.cancel_host_eager_open(&host_uri);
        assert!(
            host_token.is_cancelled(),
            "cancel_host_eager_open must cancel the batch token so an early-started body bails"
        );
    }

    #[test]
    fn test_get_config_respects_bridge_filter() {
        let coordinator = BridgeCoordinator::new();

        // Create settings with a markdown host that only allows python bridging
        let mut languages = HashMap::new();
        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                bridge: Some(bridge_filter),
                ..Default::default()
            },
        );

        // Create language server config for rust
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        // rust should be blocked by markdown's bridge filter
        let result = coordinator.get_all_configs_for_language(&settings, "markdown", "rust");
        assert!(
            result.is_empty(),
            "rust should be blocked by markdown's bridge filter"
        );
    }

    /// The screen the respawn re-open applies to every open document before
    /// paying for a parse wait or an injection resolution.
    #[test]
    fn host_language_can_reach_server_screens_on_configuration_alone() {
        let coordinator = BridgeCoordinator::new();
        let server = |language: &str| BridgeServerConfig {
            cmd: Some(vec!["x".to_string()]),
            languages: Some(vec![language.to_string()]),
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            force_start: None,
            enabled: None,
            settings: None,
        };
        let mut servers = HashMap::new();
        servers.insert("ruff".to_string(), server("python"));
        servers.insert("anything".to_string(), server("*"));
        let settings = Arc::new(WorkspaceSettings {
            languages: HashMap::new(),
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        });

        assert!(
            coordinator.host_language_can_reach_server(&settings, "markdown", "ruff"),
            "markdown can host a python injection, so ruff is reachable"
        );
        assert!(
            !coordinator.host_language_can_reach_server(&settings, "markdown", "gone"),
            "a server that is not configured at all is unreachable"
        );
        assert!(
            coordinator.host_language_can_reach_server(&settings, "markdown", "anything"),
            "a wildcard server could serve any injection language, so it must \
             never be pre-rejected"
        );
    }

    /// A server that INHERITS its `languages` from the `_` template must not be
    /// screened out.
    ///
    /// `languages` is `#[serde(default)]`, so omitting the key leaves the raw
    /// entry's list empty and the authoritative resolver merges the template in
    /// before matching. A screen that read the raw list would answer "reaches
    /// nothing" for a server that reaches everything — and because this screen
    /// only ever SKIPS, the failure is silent: no didOpen is sent, nothing is
    /// reported as failed, and the barrier releases commands onto a connection
    /// that holds no documents.
    #[test]
    fn host_language_can_reach_server_resolves_inherited_languages() {
        let coordinator = BridgeCoordinator::new();
        let mut servers = HashMap::new();
        servers.insert(
            crate::config::WILDCARD_KEY.to_string(),
            BridgeServerConfig {
                cmd: None,
                languages: Some(vec!["*".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );
        servers.insert(
            "harper-ls".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["harper-ls".to_string()]),
                // Deliberately omitted in TOML → empty here, inherited from `_`.
                languages: None,
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );
        let settings = Arc::new(WorkspaceSettings {
            languages: HashMap::new(),
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        });

        // The authority says this server bridges the language...
        assert!(
            coordinator
                .get_all_configs_for_language(&settings, "markdown", "python")
                .iter()
                .any(|r| r.server_name == "harper-ls"),
            "precondition: the merged config makes harper-ls a candidate"
        );
        // ...so the screen must not disagree.
        assert!(
            coordinator.host_language_can_reach_server(&settings, "markdown", "harper-ls"),
            "a server inheriting `languages` from `_` must not be pre-rejected"
        );
    }

    /// A host language whose bridge filter blocks the server's only language
    /// must be screened out — otherwise the re-open pays full price for a
    /// document that can supply nothing.
    #[test]
    fn host_language_can_reach_server_respects_the_hosts_bridge_filter() {
        let coordinator = BridgeCoordinator::new();
        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                bridge: Some(bridge_filter),
                ..Default::default()
            },
        );
        let mut servers = HashMap::new();
        servers.insert(
            "ruff".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["ruff".to_string()]),
                languages: Some(vec!["python".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );
        let settings = Arc::new(WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        });

        assert!(
            !coordinator.host_language_can_reach_server(&settings, "markdown", "ruff"),
            "markdown blocks python, so ruff can receive nothing from it"
        );
    }

    #[test]
    fn injections_for_server_matches_any_fan_out_server_not_just_the_first() {
        // python bridges to BOTH ruff and pyright (codeAction fans out to all).
        // A command routed to either must still select the python injection —
        // a single first pick would miss whichever the command did NOT come
        // from.
        let coordinator = BridgeCoordinator::new();

        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                bridge: Some(bridge_filter),
                ..Default::default()
            },
        );
        let server = |cmd: &str| BridgeServerConfig {
            cmd: Some(vec![cmd.to_string()]),
            languages: Some(vec!["python".to_string()]),
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            force_start: None,
            enabled: None,
            settings: None,
        };
        let mut servers = HashMap::new();
        servers.insert("ruff".to_string(), server("ruff"));
        servers.insert("pyright".to_string(), server("pyright"));
        let settings = Arc::new(WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        });

        let injections = vec![BridgeInjection {
            language: "python".to_string(),
            region_id: "region-0".to_string(),
            content: "import os\n".to_string(),
        }];

        for (name, cmd) in [("ruff", "ruff"), ("pyright", "pyright")] {
            let (kept, config) =
                coordinator.injections_for_server(&settings, "markdown", injections.clone(), name);
            assert_eq!(kept.len(), 1, "{name} must self-heal its python injection");
            assert_eq!(
                config.expect("config for matched server").cmd,
                Some(vec![cmd.to_string()])
            );
        }

        // A server that does not bridge python selects nothing.
        let (kept, config) =
            coordinator.injections_for_server(&settings, "markdown", injections, "gopls");
        assert!(kept.is_empty());
        assert!(config.is_none());
    }

    #[tokio::test]
    async fn ensure_server_documents_open_short_circuits_for_a_non_matching_server() {
        // A command whose host injections bridge to another server must not
        // trigger a connect/spawn for `server_name`: the filter drops every
        // injection, so the method returns before touching the pool. Asserted
        // via a timeout — a spawn attempt would block on the init handshake.
        let coordinator = BridgeCoordinator::new();

        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                bridge: Some(bridge_filter),
                ..Default::default()
            },
        );
        // Use a blocking devnull command (not a real binary like `ruff`): if the
        // filter regressed and DID attempt to spawn, the handshake would hang and
        // the 2s timeout below would fire — a real binary could instead fail-fast
        // on a missing runner and let the test pass without proving no-spawn.
        let mut servers = HashMap::new();
        servers.insert(
            "ruff".to_string(),
            crate::lsp::bridge::pool::test_helpers::devnull_config_for_language("python"),
        );
        let settings = Arc::new(WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        });

        let host_uri = Url::parse("file:///doc.md").unwrap();
        let injections = vec![BridgeInjection {
            language: "python".to_string(),
            region_id: "region-0".to_string(),
            content: "import os\n".to_string(),
        }];

        // `python` bridges to "ruff", not "other-server" → no match → no-op.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            coordinator.ensure_server_documents_open(
                &settings,
                "markdown",
                &host_uri,
                crate::lsp::bridge::OpenExpectation {
                    incarnation: 1,
                    connection: None,
                },
                injections,
                "other-server",
            ),
        )
        .await
        .expect("a non-matching server must short-circuit, not attempt a spawn");
        assert_eq!(
            outcome,
            crate::lsp::bridge::OpenOutcome::NotApplicable,
            "a host that bridges to no such server supplies nothing for it — \
             the answer the respawn re-open gets for most open documents"
        );
    }

    #[tokio::test]
    async fn injection_open_requires_exact_server_and_root() {
        let coordinator = BridgeCoordinator::new();
        let host_uri = Url::parse("file:///doc.md").unwrap();
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap();
        let injection = BridgeInjection {
            language: "lua".to_string(),
            region_id: "region-0".to_string(),
            content: "print('hello')".to_string(),
        };
        let virtual_uri = crate::lsp::bridge::protocol::VirtualDocumentUri::new(
            &host_uri_lsp,
            &injection.language,
            &injection.region_id,
        );
        let root_a = crate::lsp::bridge::pool::ConnectionKey::new(
            "lua_ls",
            Some("file:///workspace-a".to_string()),
        );
        coordinator
            .register_opened_document_for_test(&host_uri, &virtual_uri, &root_a)
            .await;

        assert!(coordinator.injection_open_on_connection(&host_uri_lsp, &root_a, &injection));
        assert!(!coordinator.injection_open_on_connection(
            &host_uri_lsp,
            &crate::lsp::bridge::pool::ConnectionKey::new(
                "lua_ls",
                Some("file:///workspace-b".to_string()),
            ),
            &injection,
        ));
        assert!(!coordinator.injection_open_on_connection(
            &host_uri_lsp,
            &crate::lsp::bridge::pool::ConnectionKey::for_server("ruff"),
            &injection,
        ));
    }

    #[test]
    fn test_get_config_returns_server_for_allowed_language() {
        let coordinator = BridgeCoordinator::new();

        // Create settings with no bridge filter (all languages allowed)
        let languages = HashMap::new();

        // Create language server config for rust
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        // rust should be allowed (no filter)
        let result = coordinator.get_all_configs_for_language(&settings, "markdown", "rust");
        assert_eq!(
            result.len(),
            1,
            "rust should be allowed when no filter is set"
        );
        assert_eq!(result[0].server_name, "rust-analyzer");
        assert_eq!(result[0].config.cmd(), ["rust-analyzer".to_string()]);
    }

    #[test]
    fn test_get_config_skips_server_with_empty_resolved_cmd() {
        // A concrete entry can inherit everything except cmd from the `_`
        // wildcard (e.g. the user listed a server name but forgot cmd).
        // Such a server is unspawnable and must not be selected.
        let coordinator = BridgeCoordinator::new();

        let mut servers = HashMap::new();
        servers.insert(
            "_".to_string(),
            BridgeServerConfig {
                cmd: None,
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: Some(vec![crate::config::settings::RootMarker::Single(
                    ".git".to_string(),
                )]),
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );
        servers.insert(
            "broken".to_string(),
            BridgeServerConfig {
                cmd: None,
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        // Host bridging opted in for rust, so the host lookup would select
        // the server if the empty-cmd filter were missing there.
        let mut languages = HashMap::new();
        languages.insert(
            "rust".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    "_self".to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        assert!(
            coordinator
                .get_all_configs_for_language(&settings, "markdown", "rust")
                .is_empty(),
            "a server whose resolved cmd is empty must be skipped"
        );
        assert!(
            coordinator
                .get_host_configs_for_language(&settings, "rust")
                .is_empty(),
            "the host lookup must also skip servers with empty resolved cmd"
        );
    }

    #[test]
    fn test_get_config_skips_disabled_server() {
        // A server explicitly disabled (or disabled via the `_` wildcard)
        // must never be selected, even when it is otherwise fully
        // configured (non-empty cmd, matching languages).
        let coordinator = BridgeCoordinator::new();

        let mut servers = HashMap::new();
        servers.insert(
            "_".to_string(),
            BridgeServerConfig {
                cmd: None,
                languages: None,
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: Some(false),
                settings: None,
            },
        );
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let mut languages = HashMap::new();
        languages.insert(
            "rust".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    "_self".to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        assert!(
            coordinator
                .get_all_configs_for_language(&settings, "markdown", "rust")
                .is_empty(),
            "a server disabled via the wildcard must be skipped"
        );
        assert!(
            coordinator
                .get_host_configs_for_language(&settings, "rust")
                .is_empty(),
            "the host lookup must also skip servers disabled via the wildcard"
        );
    }

    #[test]
    fn test_get_config_reenables_server_over_disabled_wildcard() {
        // A concrete server can opt back in with `enabled: true` even when
        // the `_` wildcard disables everything by default.
        let coordinator = BridgeCoordinator::new();

        let mut servers = HashMap::new();
        servers.insert(
            "_".to_string(),
            BridgeServerConfig {
                cmd: None,
                languages: None,
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: Some(false),
                settings: None,
            },
        );
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: Some(true),
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        assert!(
            !coordinator
                .get_all_configs_for_language(&settings, "markdown", "rust")
                .is_empty(),
            "a server with an explicit enabled: true must override a disabled wildcard"
        );
    }

    #[test]
    fn test_get_all_configs_returns_multiple_servers_for_same_language() {
        let coordinator = BridgeCoordinator::new();

        // No bridge filter (all languages allowed)
        let languages = HashMap::new();

        // Configure two servers that both handle python
        let mut servers = HashMap::new();
        servers.insert(
            "pyright".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["pyright-langserver".to_string()]),
                languages: Some(vec!["python".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );
        servers.insert(
            "ruff".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["ruff".to_string(), "server".to_string()]),
                languages: Some(vec!["python".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        let result = coordinator.get_all_configs_for_language(&settings, "markdown", "python");
        assert_eq!(result.len(), 2, "should return both pyright and ruff");

        // Use HashSet for order-independent comparison (HashMap iteration is non-deterministic)
        let names: std::collections::HashSet<&str> =
            result.iter().map(|r| r.server_name.as_str()).collect();
        assert!(names.contains("pyright"), "should contain pyright");
        assert!(names.contains("ruff"), "should contain ruff");
    }

    /// A server with `languages = ["*"]` plus one concrete-language server.
    fn settings_with_any_language_server() -> WorkspaceSettings {
        let servers = HashMap::from([
            (
                "harper-ls".to_string(),
                BridgeServerConfig {
                    cmd: Some(vec!["harper-ls".to_string()]),
                    languages: Some(vec![LANGUAGES_WILDCARD.to_string()]),
                    ..Default::default()
                },
            ),
            (
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: Some(vec!["rust-analyzer".to_string()]),
                    languages: Some(vec!["rust".to_string()]),
                    ..Default::default()
                },
            ),
        ]);

        WorkspaceSettings {
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        }
    }

    #[test]
    fn any_language_server_is_a_candidate_for_every_injection_language() {
        let coordinator = BridgeCoordinator::new();
        let settings = settings_with_any_language_server();

        // Named-language server + wildcard server both match rust...
        let names: std::collections::HashSet<String> = coordinator
            .get_all_configs_for_language(&settings, "markdown", "rust")
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert!(names.contains("harper-ls"));
        assert!(names.contains("rust-analyzer"));

        // ...and the wildcard server alone matches a language no server names.
        let names: Vec<String> = coordinator
            .get_all_configs_for_language(&settings, "markdown", "toml")
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert_eq!(names, vec!["harper-ls".to_string()]);
    }

    fn injection(language: &str, region_id: &str) -> BridgeInjection {
        BridgeInjection {
            language: language.to_string(),
            region_id: region_id.to_string(),
            content: String::new(),
        }
    }

    #[test]
    fn eager_open_reaches_every_server_matching_the_language() {
        // The eager open is the ONLY way a push-only server (one that
        // publishes diagnostics rather than answering pulls) ever receives a
        // region: it issues no request, so nothing opens the document lazily,
        // and the pull path returns on its capability check before
        // `ensure_document_opened`. Resolving one server per language starves
        // it whenever another server also matches — and for a `"*"` server
        // that is every language that has a real server of its own.
        let coordinator = BridgeCoordinator::new();
        let settings = settings_with_any_language_server();

        let groups = coordinator.eager_open_groups(
            &settings,
            "markdown",
            vec![injection("rust", "r1"), injection("toml", "r2")],
        );

        let names: Vec<&str> = groups.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["harper-ls", "rust-analyzer"]);

        // The wildcard server takes both regions; the rust server takes only
        // the one it handles.
        let harper: Vec<&str> = groups["harper-ls"]
            .1
            .iter()
            .map(|i| i.region_id.as_str())
            .collect();
        assert_eq!(harper, vec!["r1", "r2"]);
        let ra: Vec<&str> = groups["rust-analyzer"]
            .1
            .iter()
            .map(|i| i.region_id.as_str())
            .collect();
        assert_eq!(ra, vec!["r1"]);
    }

    #[tokio::test]
    async fn eager_open_spawns_a_task_for_every_group_not_just_the_first() {
        // `eager_open_groups` deciding on two servers is worthless if the
        // dispatch loop below it only acts on one — the push-only server would
        // still get no `didOpen`. Pin the loop itself: one registered task per
        // group. The commands are unspawnable on purpose; the assertion is
        // about dispatch, and the batch is cancelled before anything runs.
        let coordinator = BridgeCoordinator::new();
        let mut settings = settings_with_any_language_server();
        for config in settings.language_servers.values_mut() {
            config.cmd = Some(vec!["/nonexistent/kakehashi-test-server".to_string()]);
        }
        let host_uri = Url::parse("file:///test.md").unwrap();
        let injections = vec![injection("rust", "r1")];
        coordinator.begin_virtual_routing_for_injections(&host_uri, &injections);

        coordinator
            .eager_spawn_and_open_documents(&settings, "markdown", &host_uri, 1, injections)
            .await;

        let handles = coordinator
            .eager_open_tasks
            .get(&host_uri)
            .map(|batch| batch.handles.len());
        coordinator.cancel_eager_open(&host_uri);

        assert_eq!(
            handles,
            Some(2),
            "both the rust server and the wildcard server must get a task"
        );
    }

    #[test]
    fn eager_open_groups_are_empty_when_nothing_bridges() {
        // Distinguishes "resolved nothing" from "everything already open" for
        // the caller, which cancels a stale batch only in the former case.
        let coordinator = BridgeCoordinator::new();
        let settings = WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        };

        assert!(
            coordinator
                .eager_open_groups(&settings, "markdown", vec![injection("rust", "r1")])
                .is_empty()
        );
    }

    #[test]
    fn any_language_server_works_under_the_real_shipped_defaults() {
        // The hand-built settings above leave `languages` empty, which makes
        // `resolve_host_language_settings` return None and skips the bridge
        // filter entirely. Real configs always carry the shipped `languages._`
        // entry, so the filter branch *is* live — and it is the branch that
        // decides whether a `"*"` server reaches a language with no
        // `languages.<lang>` entry of its own. Pin the zero-config path.
        let coordinator = BridgeCoordinator::new();
        let mut settings = settings_with_any_language_server();
        settings.languages = crate::config::defaults::default_settings().languages;
        assert!(
            settings.languages.contains_key("_"),
            "the shipped defaults must carry the wildcard language entry"
        );

        let names: Vec<String> = coordinator
            .get_all_configs_for_language(&settings, "markdown", "toml")
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert_eq!(
            names,
            vec!["harper-ls".to_string()],
            "a `\"*\"` server must reach an injection language that no \
             `languages.<lang>` entry mentions"
        );
    }

    #[test]
    fn any_language_server_still_obeys_the_host_bridge_filter() {
        // `"*"` widens the *server* axis (which servers can answer), not the
        // *language* axis (which injections the host bridges at all). A host
        // that only enables python must not gain rust bridging for free.
        let coordinator = BridgeCoordinator::new();
        let mut settings = settings_with_any_language_server();
        settings.languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    "python".to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        );

        assert!(
            coordinator
                .get_all_configs_for_language(&settings, "markdown", "rust")
                .is_empty(),
            "the host's bridge filter must still block rust"
        );
        assert!(
            !coordinator
                .get_all_configs_for_language(&settings, "markdown", "python")
                .is_empty(),
            "the wildcard server must answer the language the host does enable"
        );
    }

    #[test]
    fn any_language_server_reaches_the_host_axis_only_when_opted_in() {
        // `handles_language` is shared by both axes, so a `"*"` server is a
        // host candidate for every language — but candidacy is not consent:
        // `bridge._self.enabled = true` still gates the host path.
        let coordinator = BridgeCoordinator::new();
        let mut settings = settings_with_any_language_server();
        // The shipped `languages._` carries `bridge._` but no `_self`, so the
        // gate is genuinely evaluated and genuinely says no. Leaving
        // `languages` empty would instead make `resolve_host_language_settings`
        // return None and short-circuit `is_some_and` before the gate runs —
        // the assertion would then hold even if the gate were deleted.
        settings.languages = crate::config::defaults::default_settings().languages;

        assert!(
            coordinator
                .get_host_configs_for_language(&settings, "lua")
                .is_empty(),
            "without the _self opt-in the host axis stays empty"
        );

        settings.languages.insert(
            "lua".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    "_self".to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        );

        let names: Vec<String> = coordinator
            .get_host_configs_for_language(&settings, "lua")
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert_eq!(names, vec!["harper-ls".to_string()]);
    }

    #[test]
    fn any_language_is_inheritable_from_the_wildcard_server_entry() {
        // wildcard-config-inheritance: `languageServers._.languages = ["*"]`
        // reaches a concrete server that omits `languages` entirely.
        let coordinator = BridgeCoordinator::new();
        let servers = HashMap::from([
            (
                "_".to_string(),
                BridgeServerConfig {
                    languages: Some(vec![LANGUAGES_WILDCARD.to_string()]),
                    ..Default::default()
                },
            ),
            (
                "harper-ls".to_string(),
                BridgeServerConfig {
                    cmd: Some(vec!["harper-ls".to_string()]),
                    ..Default::default()
                },
            ),
            // ...but a concrete server that DOES declare `languages` keeps its
            // own narrower list. This is the containment guarantee for the
            // documented `_.languages = ["*"]` footgun: the wildcard reaches
            // only the servers that stayed silent, so listing real languages
            // is a working opt-out.
            (
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: Some(vec!["rust-analyzer".to_string()]),
                    languages: Some(vec!["rust".to_string()]),
                    ..Default::default()
                },
            ),
        ]);
        let settings = WorkspaceSettings {
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        let names: Vec<String> = coordinator
            .get_all_configs_for_language(&settings, "markdown", "toml")
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert_eq!(
            names,
            vec!["harper-ls".to_string()],
            "the wildcard must not leak into a server with its own list"
        );

        let names: Vec<String> = coordinator
            .get_all_configs_for_language(&settings, "markdown", "rust")
            .into_iter()
            .map(|r| r.server_name)
            .collect();
        assert_eq!(
            names,
            vec!["harper-ls".to_string(), "rust-analyzer".to_string()],
            "both answer for rust, in the documented name-sorted order"
        );
    }

    #[test]
    fn test_get_all_configs_returns_empty_when_blocked_by_filter() {
        let coordinator = BridgeCoordinator::new();

        // Create settings with a markdown host that only allows python bridging
        let mut languages = HashMap::new();
        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                bridge: Some(bridge_filter),
                ..Default::default()
            },
        );

        // Create language server config for rust (which is NOT in the filter)
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        // rust should be blocked by markdown's bridge filter
        let result = coordinator.get_all_configs_for_language(&settings, "markdown", "rust");
        assert!(
            result.is_empty(),
            "rust should be blocked by markdown's bridge filter"
        );
    }

    #[test]
    fn test_get_all_configs_returns_single_server_when_only_one_matches() {
        let coordinator = BridgeCoordinator::new();

        // No bridge filter
        let languages = HashMap::new();

        // Single server for rust
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        let result = coordinator.get_all_configs_for_language(&settings, "markdown", "rust");
        assert_eq!(result.len(), 1, "should return exactly one server");
        assert_eq!(result[0].server_name, "rust-analyzer");
    }

    #[tokio::test]
    async fn test_cancel_eager_open_aborts_tracked_tasks() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Spawn tasks that will never complete on their own
        let task1 = tokio::spawn(futures::future::pending::<()>());
        let task2 = tokio::spawn(futures::future::pending::<()>());

        // Insert handles directly for this URI
        coordinator.eager_open_tasks.insert(
            uri.clone(),
            EagerOpenBatch {
                generation: 0,
                handles: vec![task1.abort_handle(), task2.abort_handle()],
                cancel: CancellationToken::new(),
            },
        );

        // Cancel all tasks for this URI
        coordinator.cancel_eager_open(&uri);

        // Give tokio a chance to process the abort
        tokio::task::yield_now().await;

        // Verify tasks are finished (aborted)
        assert!(task1.is_finished(), "task1 should be aborted");
        assert!(task2.is_finished(), "task2 should be aborted");
    }

    #[test]
    fn test_cancel_eager_open_noop_for_unknown_uri() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///unknown.md").unwrap();

        // Should not panic or error when cancelling for an unknown URI
        coordinator.cancel_eager_open(&uri);
    }

    #[tokio::test]
    async fn test_register_supersedes_previous_tasks() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // First batch: insert a running task directly
        let task1 = tokio::spawn(futures::future::pending::<()>());
        coordinator.eager_open_tasks.insert(
            uri.clone(),
            EagerOpenBatch {
                generation: 0,
                handles: vec![task1.abort_handle()],
                cancel: CancellationToken::new(),
            },
        );

        // Second batch — supersede should abort the first batch and insert placeholder
        let (generation, _cancel) = coordinator.supersede_eager_open_tasks(&uri);

        // Push a new task into the placeholder
        let task2 = tokio::spawn(futures::future::pending::<()>());
        coordinator.push_or_abort_eager_open_handle(&uri, task2.abort_handle(), generation);

        // Give tokio a chance to process the abort
        tokio::task::yield_now().await;

        // First batch should be aborted
        assert!(
            task1.is_finished(),
            "first batch should be aborted on supersede"
        );
        // Second batch should still be running
        assert!(!task2.is_finished(), "second batch should still be running");
    }

    #[test]
    fn test_get_config_blocks_configured_host_with_empty_bridge() {
        // After Phase 2 (resolve_base_configs), each configured language has "_"'s
        // bridge config merged in. If "quarto" is explicitly configured with an
        // empty bridge map (which it would inherit from "_"), it should be blocked.
        let coordinator = BridgeCoordinator::new();

        // "quarto" is explicitly in the map with empty bridge (as Phase 2 would produce)
        let mut languages = HashMap::new();
        languages.insert(
            "quarto".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::new()), // empty = block all (inherited from "_")
                ..Default::default()
            },
        );

        // Create language server config for rust
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["rust-analyzer".to_string()]),
                languages: Some(vec!["rust".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        // "quarto" is configured with an empty bridge — should be blocked
        let result = coordinator.get_all_configs_for_language(&settings, "quarto", "rust");
        assert!(
            result.is_empty(),
            "quarto with empty bridge map should block all bridging"
        );
    }

    #[tokio::test]
    async fn test_push_or_abort_adds_handle_when_entry_exists() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Pre-insert placeholder via supersede (gets a generation)
        let (generation, _cancel) = coordinator.supersede_eager_open_tasks(&uri);

        // Spawn a task and push its handle with matching generation
        let task = tokio::spawn(futures::future::pending::<()>());
        let handle = task.abort_handle();
        coordinator.push_or_abort_eager_open_handle(&uri, handle, generation);

        // Entry should now have 1 handle
        let entry = coordinator.eager_open_tasks.get(&uri).unwrap();
        assert_eq!(
            entry.value().handles.len(),
            1,
            "should have 1 handle after push"
        );
        assert!(!task.is_finished(), "task should still be running");
    }

    #[tokio::test]
    async fn test_push_or_abort_aborts_when_entry_removed() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Do NOT insert a placeholder — simulates concurrent cancel removing the entry

        // Spawn a task and try to push its handle (generation doesn't matter — no entry)
        let task = tokio::spawn(futures::future::pending::<()>());
        let handle = task.abort_handle();
        coordinator.push_or_abort_eager_open_handle(&uri, handle, 0);

        // Give tokio a chance to process the abort
        tokio::task::yield_now().await;

        // Task should be aborted since there's no entry to push into
        assert!(
            task.is_finished(),
            "task should be aborted when entry is missing (concurrent cancel)"
        );
        // No entry should have been created
        assert!(
            coordinator.eager_open_tasks.get(&uri).is_none(),
            "no entry should be created for a cancelled URI"
        );
    }

    #[tokio::test]
    async fn test_supersede_aborts_previous_and_inserts_placeholder() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Register a running task (simulates previous batch)
        let previous_task = tokio::spawn(futures::future::pending::<()>());
        coordinator.eager_open_tasks.insert(
            uri.clone(),
            EagerOpenBatch {
                generation: 0,
                handles: vec![previous_task.abort_handle()],
                cancel: CancellationToken::new(),
            },
        );

        // Supersede — should abort previous and insert empty placeholder
        let _ = coordinator.supersede_eager_open_tasks(&uri);

        // Give tokio a chance to process the abort
        tokio::task::yield_now().await;

        // Previous task should be aborted
        assert!(
            previous_task.is_finished(),
            "previous task should be aborted on supersede"
        );

        // Entry should exist with empty handles (placeholder)
        let entry = coordinator.eager_open_tasks.get(&uri).unwrap();
        assert_eq!(
            entry.value().handles.len(),
            0,
            "supersede should insert empty placeholder"
        );
    }

    /// Test that push_or_abort with a stale generation aborts the handle.
    ///
    /// When two supersede calls happen concurrently, the first caller's
    /// generation becomes stale. Handles pushed with the stale generation
    /// should be aborted instead of adopted by the newer batch.
    #[tokio::test]
    async fn test_push_or_abort_with_stale_generation_aborts_handle() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // First supersede — get gen1
        let (gen1, _) = coordinator.supersede_eager_open_tasks(&uri);

        // Second supersede — get gen2 (gen1 is now stale)
        let (gen2, _) = coordinator.supersede_eager_open_tasks(&uri);
        assert!(gen2 > gen1, "gen2 should be greater than gen1");

        // Push with stale gen1 — should be aborted
        let stale_task = tokio::spawn(futures::future::pending::<()>());
        coordinator.push_or_abort_eager_open_handle(&uri, stale_task.abort_handle(), gen1);

        // Push with current gen2 — should be kept
        let current_task = tokio::spawn(futures::future::pending::<()>());
        coordinator.push_or_abort_eager_open_handle(&uri, current_task.abort_handle(), gen2);

        // Give tokio a chance to process the abort
        tokio::task::yield_now().await;

        // Stale generation handle should be aborted
        assert!(
            stale_task.is_finished(),
            "Handle from stale generation should be aborted"
        );

        // Current generation handle should still be running
        assert!(
            !current_task.is_finished(),
            "Handle from current generation should still be running"
        );
    }

    #[tokio::test]
    async fn test_abort_all_eager_open_aborts_all_tasks() {
        let coordinator = BridgeCoordinator::new();
        let uri1 = Url::parse("file:///a.md").unwrap();
        let uri2 = Url::parse("file:///b.md").unwrap();

        // Spawn tasks for two different URIs
        let task1 = tokio::spawn(futures::future::pending::<()>());
        let task2 = tokio::spawn(futures::future::pending::<()>());

        coordinator.eager_open_tasks.insert(
            uri1,
            EagerOpenBatch {
                generation: 0,
                handles: vec![task1.abort_handle()],
                cancel: CancellationToken::new(),
            },
        );
        coordinator.eager_open_tasks.insert(
            uri2,
            EagerOpenBatch {
                generation: 0,
                handles: vec![task2.abort_handle()],
                cancel: CancellationToken::new(),
            },
        );

        coordinator.abort_all_eager_open();
        tokio::task::yield_now().await;

        assert!(task1.is_finished(), "task1 should be aborted");
        assert!(task2.is_finished(), "task2 should be aborted");
        assert!(
            coordinator.eager_open_tasks.is_empty(),
            "All entries should be removed"
        );
    }

    #[test]
    fn test_abort_all_eager_open_noop_when_empty() {
        let coordinator = BridgeCoordinator::new();
        // Should not panic
        coordinator.abort_all_eager_open();
        assert!(coordinator.eager_open_tasks.is_empty());
    }

    #[test]
    fn test_get_config_unconfigured_host_inherits_wildcard_bridge_filter() {
        // Auto-discovered languages (not in config) should still inherit "_"'s bridge
        // filter. This tests the scenario where [languages.markdown] is absent but
        // [languages._] has bridge = { lua = { enabled = false } }.
        let coordinator = BridgeCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "_".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    "lua".to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(false),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        );

        let mut servers = HashMap::new();
        servers.insert(
            "lua-language-server".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["lua-language-server".to_string()]),
                languages: Some(vec!["lua".to_string()]),
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            },
        );

        let settings = WorkspaceSettings {
            languages,
            auto_install: false,
            language_servers: servers,
            ..Default::default()
        };

        // "markdown" is not in settings.languages (auto-discovered at runtime).
        // It should still inherit "_"'s bridge filter that blocks lua.
        let result = coordinator.get_all_configs_for_language(&settings, "markdown", "lua");
        assert!(
            result.is_empty(),
            "unconfigured host should inherit '_'s bridge filter — lua should be blocked"
        );
    }
}
