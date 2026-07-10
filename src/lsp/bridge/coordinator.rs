//! Bridge coordinator unifying the language server pool and node tracker
//! into a single coherent API.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
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

/// An injection region resolved from a host document.
///
/// Represents a single code block embedded in a host document (e.g., a Lua
/// code fence in a markdown file) along with its stable region ID (lazy-node-identity-tracking).
#[derive(Debug, Clone)]
pub(crate) struct BridgeInjection {
    /// The injection language (e.g., "lua", "python", "rust")
    pub(crate) language: String,
    /// Stable ULID-based region ID (lazy-node-identity-tracking)
    pub(crate) region_id: String,
    /// The text content of the injection region
    pub(crate) content: String,
}

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
    /// Create a new bridge coordinator with fresh pool and tracker.
    pub(crate) fn new() -> Self {
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
        Self {
            pool,
            node_tracker: Arc::new(NodeTracker::new()),
            cancel_forwarder,
            eager_open_generation: std::sync::atomic::AtomicU64::new(0),
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

    /// Propagate a merged-settings change to every live downstream connection
    /// (downstream-settings-propagation, path c). Each server's settings are
    /// re-resolved through the same wildcard merge used at spawn, so an
    /// unchanged config yields identical values and pushes nothing. Returns the
    /// number of connections pushed.
    pub(crate) async fn propagate_settings(&self, settings: &WorkspaceSettings) -> usize {
        self.pool
            .propagate_settings(|server_name| {
                resolve_with_wildcard(
                    &settings.language_servers,
                    server_name,
                    merge_bridge_server_configs,
                )
                .and_then(|config| config.settings)
            })
            .await
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
        injections: Vec<BridgeInjection>,
        server_name: &str,
    ) {
        let (for_server, config) =
            self.injections_for_server(settings, host_language, injections, server_name);
        let Some(config) = config else {
            return; // no injected region on this host bridges to `server_name`
        };
        let Ok(host_uri_lsp) = crate::lsp::lsp_impl::url_to_uri(host_uri) else {
            return;
        };
        self.pool
            .eager_open_virtual_documents(server_name, &config, host_uri, &host_uri_lsp, for_server)
            .await;
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
    /// name against the full set ([`Self::get_all_configs_for_language`]), not
    /// [`Self::get_config_for_language`]'s single first pick (which would miss
    /// the origin when e.g. both ruff and pyright bridge python and the command
    /// came from ruff). Pure; the async open is separate so it is unit-testable.
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

    /// Resolve `bridge.servers` for `injection_language`, returning the
    /// `ResolvedServerConfig` (server name for pooling + spawn config) or
    /// `None` when no server matches, or the host's bridge filter excludes
    /// this injection. Host lookup uses wildcard resolution (wildcard-config-inheritance):
    /// undefined hosts inherit `languages._`, letting one default filter apply
    /// to every host.
    pub(crate) fn get_config_for_language(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        injection_language: &str,
    ) -> Option<ResolvedServerConfig> {
        // Host-language bridge filters are checked first, with an optional
        // fallback to the wildcard ("_") entry when the host has no explicit
        // configuration. This allows using languages._ to define shared
        // defaults, but does not guarantee that every language inherits them.
        if let Some(host_settings) = settings.resolve_host_language_settings(host_language)
            && !host_settings.is_language_bridgeable(injection_language)
        {
            log::debug!(
                target: "kakehashi::bridge",
                "Bridge filter for {} blocks injection language {}",
                host_language,
                injection_language
            );
            return None;
        }

        // Look for a server that handles this language
        // wildcard-config-inheritance: Resolve each server with wildcard BEFORE checking languages,
        // because languages list may be inherited from languageServers._
        let servers = &settings.language_servers;
        for server_name in servers.keys() {
            // Skip wildcard entry - we use it for inheritance, not direct lookup
            if server_name == "_" {
                continue;
            }

            if let Some(resolved_config) =
                resolve_with_wildcard(servers, server_name, merge_bridge_server_configs)
                    .filter(|c| c.is_spawnable())
                    .filter(|c| c.languages.iter().any(|l| l == injection_language))
            {
                return Some(ResolvedServerConfig {
                    server_name: server_name.clone(),
                    config: Arc::new(resolved_config),
                });
            }
        }

        None
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
    /// Unlike `get_config_for_language()` which returns the first matching server,
    /// this method returns **all** servers configured for the injection language.
    /// This enables diagnostic fan-out to multiple servers (e.g., pyright + ruff
    /// both handling Python).
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
        // Check bridge filter (same logic as get_config_for_language)
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
                    .filter(|c| c.languages.iter().any(|l| l == injection_language))
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
    /// host-path matching rule: servers whose `languages` contains the
    /// *host* language itself. Gated on the explicit `bridge._self.enabled =
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
                    .filter(|c| c.languages.iter().any(|l| l == host_language))
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
    /// Called on didClose to prevent memory leaks. `reopened` is the
    /// raced-reopen probe forwarded to [`NodeTracker::cleanup`] — see the
    /// removal guard there.
    pub(crate) fn cleanup(&self, uri: &Url, reopened: impl FnOnce() -> bool) {
        self.node_tracker.cleanup(uri, reopened)
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
        injections: &[BridgeInjection],
    ) {
        self.pool
            .forward_didchange_to_opened_docs(uri, injections)
            .await;
    }

    // ========================================
    // Eager spawn + open (warmup with document content)
    // ========================================

    /// Eagerly spawn language servers and open virtual documents for detected injections.
    ///
    /// Sending `didOpen` up front (not just a handshake) lets downstream servers
    /// start analyzing immediately, yielding faster diagnostics.
    pub(crate) async fn eager_spawn_and_open_documents(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        host_uri: &Url,
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

        // Group injections by server name
        // Multiple injection languages may map to the same server (e.g., ts/tsx → tsgo)
        type ServerGroup = (Arc<BridgeServerConfig>, Vec<BridgeInjection>);
        let mut server_groups: BTreeMap<String, ServerGroup> = BTreeMap::new();

        // Resolve the server config once per DISTINCT language, not per region:
        // config resolution walks the wildcard+merge chain, and a fence-heavy
        // document has hundreds of regions across a handful of languages —
        // per-region resolution was a measured tokio-side hotspot (this loop
        // runs on the runtime, and starving it delays every handler).
        let mut config_by_lang: HashMap<String, Option<ResolvedServerConfig>> = HashMap::new();
        let mut connection_key_by_lang = HashMap::new();
        for injection in injections {
            let resolved = config_by_lang
                .entry(injection.language.clone())
                .or_insert_with(|| {
                    self.get_config_for_language(settings, host_language, &injection.language)
                });
            if let Some(resolved) = resolved {
                let connection_key = match connection_key_by_lang.get(&injection.language) {
                    Some(key) => key,
                    None => {
                        let key = self
                            .pool
                            .resolved_connection_key(
                                &resolved.server_name,
                                &resolved.config,
                                host_uri,
                            )
                            .await;
                        connection_key_by_lang.insert(injection.language.clone(), key);
                        connection_key_by_lang
                            .get(&injection.language)
                            .expect("just inserted")
                    }
                };
                if self.injection_open_on_connection(&host_uri_lsp, connection_key, &injection) {
                    continue;
                }
                server_groups
                    .entry(resolved.server_name.clone())
                    .or_insert_with(|| (resolved.config.clone(), Vec::new()))
                    .1
                    .push(injection);
            }
        }

        // Every injection is already claimed/open on its exact target
        // connection. Keep any active batch: a pre-send claim is visible in the
        // reverse index, and aborting its owner here would strand the claim
        // before didOpen reaches the FIFO writer.
        if server_groups.is_empty() {
            return;
        }

        // Supersede previous batch: abort + insert empty placeholder BEFORE spawning.
        // This closes the race window between spawn and registration. The returned
        // token (stored in the batch, already in the map) is `select!`ed on by each
        // task body to close the spawn→register window the abort handle can't reach (#435).
        let (generation, cancel) = self.supersede_eager_open_tasks(host_uri);

        // Spawn one task per server group, registering each handle immediately
        for (server_name, (config, group_injections)) in server_groups {
            log::debug!(
                target: "kakehashi::bridge",
                "Eager open: spawning {} with {} injections",
                server_name,
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
        for config in configs {
            let pool = self.pool_arc();
            let host_uri_owned = host_uri.clone();
            let language_id = Arc::clone(&language_id);
            let text = Arc::clone(&text);
            let server_name = config.server_name.clone();
            let server_config = config.config.clone();
            let cancel = cancel.clone();
            // Each task shares the same live reader (cheap `Arc` clone) — it is
            // evaluated inside `sync_host_document`'s lock, so the last task to sync
            // sends the latest text regardless of which snapshot it was spawned with.
            let live_text_reader = live_text_reader.clone();
            let task = tokio::spawn(async move {
                tokio::select! {
                    biased;
                    // Cancelled during the spawn→register window (or later) —
                    // bail before the side effect (#435).
                    _ = cancel.cancelled() => {}
                    _ = pool.eager_open_host_document(
                        &server_name,
                        &server_config,
                        &host_uri_owned,
                        &language_id,
                        &text,
                        live_text_reader.as_deref(),
                    ) => {}
                }
            });
            self.push_or_abort_host_eager_open_handle(host_uri, task.abort_handle(), generation);
        }
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
    use crate::config::settings::BridgeLanguageConfig;

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
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
        let result = coordinator.get_config_for_language(&settings, "markdown", "rust");
        assert!(
            result.is_none(),
            "rust should be blocked by markdown's bridge filter"
        );
    }

    #[test]
    fn injections_for_server_matches_any_fan_out_server_not_just_the_first() {
        // python bridges to BOTH ruff and pyright (codeAction fans out to all).
        // A command routed to either must still select the python injection —
        // get_config_for_language's single first pick would miss whichever the
        // command did NOT come from.
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
            cmd: vec![cmd.to_string()],
            languages: vec!["python".to_string()],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
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
                vec![cmd.to_string()]
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
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            coordinator.ensure_server_documents_open(
                &settings,
                "markdown",
                &host_uri,
                injections,
                "other-server",
            ),
        )
        .await
        .expect("a non-matching server must short-circuit, not attempt a spawn");
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
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
        let result = coordinator.get_config_for_language(&settings, "markdown", "rust");
        assert!(
            result.is_some(),
            "rust should be allowed when no filter is set"
        );
        let resolved = result.unwrap();
        assert_eq!(resolved.server_name, "rust-analyzer");
        assert_eq!(resolved.config.cmd, vec!["rust-analyzer".to_string()]);
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
                cmd: vec![],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: Some(vec![crate::config::settings::RootMarker::Single(
                    ".git".to_string(),
                )]),
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
            },
        );
        servers.insert(
            "broken".to_string(),
            BridgeServerConfig {
                cmd: vec![],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
                .get_config_for_language(&settings, "markdown", "rust")
                .is_none(),
            "a server whose resolved cmd is empty must be skipped"
        );
        assert!(
            coordinator
                .get_all_configs_for_language(&settings, "markdown", "rust")
                .is_empty(),
            "fan-out must also skip servers with empty resolved cmd"
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
                cmd: vec![],
                languages: vec![],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: Some(false),
                settings: None,
            },
        );
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
                .get_config_for_language(&settings, "markdown", "rust")
                .is_none(),
            "a server disabled via the wildcard must be skipped"
        );
        assert!(
            coordinator
                .get_all_configs_for_language(&settings, "markdown", "rust")
                .is_empty(),
            "fan-out must also skip servers disabled via the wildcard"
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
                cmd: vec![],
                languages: vec![],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: Some(false),
                settings: None,
            },
        );
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
            coordinator
                .get_config_for_language(&settings, "markdown", "rust")
                .is_some(),
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
                cmd: vec!["pyright-langserver".to_string()],
                languages: vec!["python".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
            },
        );
        servers.insert(
            "ruff".to_string(),
            BridgeServerConfig {
                cmd: vec!["ruff".to_string(), "server".to_string()],
                languages: vec!["python".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
        let result = coordinator.get_config_for_language(&settings, "quarto", "rust");
        assert!(
            result.is_none(),
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
                cmd: vec!["lua-language-server".to_string()],
                languages: vec!["lua".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
        let result = coordinator.get_config_for_language(&settings, "markdown", "lua");
        assert!(
            result.is_none(),
            "unconfigured host should inherit '_'s bridge filter — lua should be blocked"
        );
    }
}
