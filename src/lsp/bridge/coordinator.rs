//! Bridge coordinator for consolidating bridge pool and region ID tracking.
//!
//! This module provides the `BridgeCoordinator` which unifies the language server
//! pool and region ID tracker into a single coherent API.
//!
//! # Responsibilities
//!
//! - Manages downstream language server connections via `LanguageServerPool`
//! - Tracks stable ULID-based region IDs via `RegionIdTracker`
//! - Provides bridge config lookup with wildcard resolution
//! - Provides cancel notification support via `CancelForwarder`

use std::sync::Arc;

use dashmap::DashMap;
use ulid::Ulid;
use url::Url;

use crate::config::{
    WorkspaceSettings, resolve_language_server_with_wildcard,
    resolve_language_settings_with_wildcard, settings::BridgeServerConfig,
};
use crate::language::region_id_tracker::{EditInfo, RegionIdTracker};
use crate::lsp::request_id::CancelForwarder;

use super::LanguageServerPool;

/// Resolved server configuration with server name.
///
/// Wraps `BridgeServerConfig` with the server name from the config key.
/// This enables server-name-based pooling where multiple languages can
/// share the same language server process (e.g., ts and tsx using tsgo).
///
/// # Design Rationale
///
/// Created during the language-to-server-name pooling migration to carry
/// both the server name (for connection lookup) and the config (for server
/// spawning) through the system.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedServerConfig {
    /// The server name from the languageServers config key (e.g., "tsgo", "rust-analyzer")
    pub(crate) server_name: String,
    /// The server configuration (cmd, languages, initialization_options, etc.)
    pub(crate) config: BridgeServerConfig,
}

/// Coordinator for bridge connections and region ID tracking.
///
/// Consolidates the `LanguageServerPool` and `RegionIdTracker` into a single
/// struct, reducing Kakehashi's field count from 9 to 8.
///
/// # Design Notes
///
/// The coordinator exposes internals via accessor methods (`pool()`, `region_id_tracker()`)
/// for handlers that need direct access. This is a pragmatic trade-off for Phase 5:
/// - Keeps handler changes minimal (just path changes, not signature changes)
/// - Allows future phases to encapsulate these internals further
///
/// The pool is wrapped in `Arc` to enable sharing with the cancel forwarding middleware.
///
/// # API Design Pattern (Sprint 15 Retrospective)
///
/// Two access patterns coexist:
///
/// 1. **Direct pool access** (preferred for NEW code): Use `self.bridge.pool().*` to call
///    pool methods directly. This is the primary pattern for LSP request handlers.
///    - Pros: No coordinator changes needed, smaller API surface
///    - Use for: send_*_request(), forward_cancel(), new pool operations
///
/// 2. **Delegating methods** (for common lifecycle operations): Methods like
///    `close_host_document()`, `shutdown_all()`, etc. delegate to pool methods.
///    - Pros: Semantic naming, hides pool implementation details
///    - Use for: Document lifecycle (open/close), shutdown, region ID management
///
/// **Decision Guide**: Use direct pool access by default. Only add a delegating method
/// if the operation is (a) used in 3+ places, (b) involves coordinator-level logic
/// (e.g., combining pool + region_id_tracker), or (c) needs semantic naming for clarity.
pub(crate) struct BridgeCoordinator {
    pool: Arc<LanguageServerPool>,
    region_id_tracker: RegionIdTracker,
    /// Cancel forwarder for upstream cancel notification and downstream forwarding.
    ///
    /// This is shared with the `RequestIdCapture` middleware via `cancel_forwarder()`.
    /// Handlers can subscribe to cancel notifications using `cancel_forwarder().subscribe()`.
    cancel_forwarder: CancelForwarder,
    /// Abort handles for eager-open tasks, keyed by host document URI.
    ///
    /// Each host URI maps to a Vec of AbortHandles (one per server group spawned
    /// by eager_spawn_and_open_documents). When a new batch is registered for
    /// the same URI, the previous batch is aborted (superseding behavior).
    ///
    /// This prevents orphaned virtual documents when:
    /// - Host document is closed while tasks wait for server readiness
    /// - Rapid did_change events spawn many overlapping batches
    eager_open_tasks: DashMap<Url, Vec<tokio::task::AbortHandle>>,
}

impl BridgeCoordinator {
    /// Create a new bridge coordinator with fresh pool and tracker.
    pub(crate) fn new() -> Self {
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
        Self {
            pool,
            region_id_tracker: RegionIdTracker::new(),
            cancel_forwarder,
            eager_open_tasks: DashMap::new(),
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
            region_id_tracker: RegionIdTracker::new(),
            cancel_forwarder,
            eager_open_tasks: DashMap::new(),
        }
    }

    // ========================================
    // Accessor methods (leaky but pragmatic)
    // ========================================

    /// Access the underlying region ID tracker.
    ///
    /// Used by handlers for `InjectionResolver::resolve_at_byte_offset()`.
    pub(crate) fn region_id_tracker(&self) -> &RegionIdTracker {
        &self.region_id_tracker
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

    /// Access the cancel forwarder.
    ///
    /// Used by:
    /// - `RequestIdCapture` middleware to receive the forwarder for the service layer
    /// - Handlers that want to subscribe to cancel notifications via `subscribe()`
    pub(crate) fn cancel_forwarder(&self) -> &CancelForwarder {
        &self.cancel_forwarder
    }

    // ========================================
    // Config lookup (moved from Kakehashi)
    // ========================================

    /// Get bridge server config for a given injection language from settings.
    ///
    /// Looks up the bridge.servers configuration and finds a server that handles
    /// the specified language. Returns `ResolvedServerConfig` which includes both
    /// the server name (for connection pooling) and the config (for spawning).
    ///
    /// Returns None if:
    /// - No server is configured for this injection language, OR
    /// - The host language has a bridge filter that excludes this injection language
    ///
    /// Uses wildcard resolution (ADR-0011) for host language lookup:
    /// - If host language is not defined, inherits from languages._ if present
    /// - This allows setting default bridge filters for all hosts via languages._
    ///
    /// # Arguments
    /// * `settings` - The current workspace settings
    /// * `host_language` - The language of the host document (e.g., "markdown")
    /// * `injection_language` - The injection language to bridge (e.g., "rust", "python")
    pub(crate) fn get_config_for_language(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        injection_language: &str,
    ) -> Option<ResolvedServerConfig> {
        // Use wildcard resolution for host language lookup (ADR-0011)
        // This allows languages._ to define default bridge filters
        if let Some(host_settings) =
            resolve_language_settings_with_wildcard(&settings.languages, host_language)
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

        // Check if language servers exist
        if let Some(ref servers) = settings.language_servers {
            // Look for a server that handles this language
            // ADR-0011: Resolve each server with wildcard BEFORE checking languages,
            // because languages list may be inherited from languageServers._
            for server_name in servers.keys() {
                // Skip wildcard entry - we use it for inheritance, not direct lookup
                if server_name == "_" {
                    continue;
                }

                if let Some(resolved_config) =
                    resolve_language_server_with_wildcard(servers, server_name)
                        .filter(|c| c.languages.iter().any(|l| l == injection_language))
                {
                    return Some(ResolvedServerConfig {
                        server_name: server_name.clone(),
                        config: resolved_config,
                    });
                }
            }
        }

        None
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
        if let Some(host_settings) =
            resolve_language_settings_with_wildcard(&settings.languages, host_language)
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

        let Some(ref servers) = settings.language_servers else {
            return Vec::new();
        };

        let mut results: Vec<ResolvedServerConfig> = servers
            .keys()
            .filter(|name| *name != "_")
            .filter_map(|server_name| {
                resolve_language_server_with_wildcard(servers, server_name)
                    .filter(|c| c.languages.iter().any(|l| l == injection_language))
                    .map(|config| ResolvedServerConfig {
                        server_name: server_name.clone(),
                        config,
                    })
            })
            .collect();

        // Sort by server name for deterministic ordering
        results.sort_by(|a, b| a.server_name.cmp(&b.server_name));
        results
    }

    // ========================================
    // Region ID management (delegate to tracker)
    // ========================================

    /// Apply input edits to update region positions using START-priority invalidation.
    ///
    /// Returns ULIDs that were invalidated by this edit (for cleanup).
    pub(crate) fn apply_input_edits(&self, uri: &Url, edits: &[EditInfo]) -> Vec<Ulid> {
        self.region_id_tracker.apply_input_edits(uri, edits)
    }

    /// Apply text diff to update region positions.
    ///
    /// Used when InputEdits are not available (full document sync).
    /// Returns ULIDs that were invalidated.
    pub(crate) fn apply_text_diff(&self, uri: &Url, old_text: &str, new_text: &str) -> Vec<Ulid> {
        self.region_id_tracker
            .apply_text_diff(uri, old_text, new_text)
    }

    /// Remove all tracked regions for a document.
    ///
    /// Called on didClose to prevent memory leaks.
    pub(crate) fn cleanup(&self, uri: &Url) {
        self.region_id_tracker.cleanup(uri)
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
        injections: &[(String, String, String)],
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
    /// This method:
    /// 1. Groups injections by server name using `get_config_for_language`
    /// 2. Spawns one background task per server group
    /// 3. Each task waits for server ready, then sends `didOpen` for all injections
    ///
    /// This replaces the old `eager_spawn_servers` which only did handshakes.
    /// By also sending `didOpen`, downstream servers can start analyzing immediately,
    /// resulting in faster diagnostic responses.
    ///
    /// # Arguments
    /// * `settings` - Current workspace settings
    /// * `host_language` - Language of the host document (e.g., "markdown")
    /// * `host_uri` - URI of the host document
    /// * `injections` - List of (language, region_id, content) tuples for all injection regions
    pub(crate) fn eager_spawn_and_open_documents(
        &self,
        settings: &WorkspaceSettings,
        host_language: &str,
        host_uri: &Url,
        injections: Vec<(String, String, String)>,
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
        type InjectionTuple = (String, String, String);
        type ServerGroup = (BridgeServerConfig, Vec<InjectionTuple>);
        let mut server_groups: std::collections::HashMap<String, ServerGroup> =
            std::collections::HashMap::new();

        for injection in injections {
            let (language, _, _) = &injection;

            if let Some(resolved) = self.get_config_for_language(settings, host_language, language)
            {
                server_groups
                    .entry(resolved.server_name.clone())
                    .or_insert_with(|| (resolved.config.clone(), Vec::new()))
                    .1
                    .push(injection);
            }
        }

        // If no servers match, cancel any previous batch to prevent stale didOpen
        if server_groups.is_empty() {
            self.cancel_eager_open(host_uri);
            return;
        }

        // Supersede previous batch: abort + insert empty placeholder BEFORE spawning.
        // This closes the race window between spawn and registration.
        self.supersede_eager_open_tasks(host_uri);

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

            let task = tokio::spawn(async move {
                pool.eager_open_virtual_documents(
                    &server_name,
                    &config,
                    &host_uri_owned,
                    &host_uri_lsp,
                    group_injections,
                )
                .await;
            });

            // Register immediately — if concurrent cancel removed the entry,
            // the handle is aborted instead of leaked.
            self.push_or_abort_eager_open_handle(host_uri, task.abort_handle());
        }
    }

    // ========================================
    // Eager-open task cancellation
    // ========================================

    /// Supersede previous eager-open tasks for a URI.
    ///
    /// Performs cleanup, aborts any previous batch, and inserts an empty placeholder
    /// entry. Must be called BEFORE spawning new tasks to close the race window
    /// between spawn and handle registration.
    ///
    /// # Arguments
    /// * `uri` - The host document URI
    fn supersede_eager_open_tasks(&self, uri: &Url) {
        // Opportunistic cleanup: remove finished tasks from all documents
        self.cleanup_finished_eager_open_tasks();

        // Abort previous batch if it exists
        if let Some((_, prev_handles)) = self.eager_open_tasks.remove(uri) {
            log::debug!(
                target: "kakehashi::bridge",
                "Aborting {} previous eager-open tasks for {} (superseded by new batch)",
                prev_handles.len(),
                uri
            );
            for handle in prev_handles {
                handle.abort();
            }
        }

        // Insert empty placeholder — concurrent cancel_eager_open will remove this,
        // causing subsequent push_or_abort calls to abort their handles.
        self.eager_open_tasks.insert(uri.clone(), Vec::new());
    }

    /// Push an abort handle into an existing entry, or abort it if the entry was removed.
    ///
    /// Called immediately after each `tokio::spawn` to register the handle. If a
    /// concurrent `cancel_eager_open` removed the entry between `supersede` and this
    /// call, the handle is aborted to prevent stale `didOpen` from being sent.
    ///
    /// # Arguments
    /// * `uri` - The host document URI
    /// * `handle` - The AbortHandle to register
    fn push_or_abort_eager_open_handle(&self, uri: &Url, handle: tokio::task::AbortHandle) {
        match self.eager_open_tasks.get_mut(uri) {
            Some(mut entry) => {
                entry.value_mut().push(handle);
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

    /// Clean up finished eager-open tasks to prevent memory leaks.
    ///
    /// When tasks complete naturally (server becomes ready, didOpen succeeds),
    /// their AbortHandles stay in the map. This method removes finished handles
    /// and deletes entries where all handles are finished.
    ///
    /// Called opportunistically by supersede_eager_open_tasks() on each batch.
    fn cleanup_finished_eager_open_tasks(&self) {
        let mut removed_handles = 0;
        let mut to_remove = Vec::new();

        // Single pass: retain only running handles, collect empty entries for removal
        for mut entry in self.eager_open_tasks.iter_mut() {
            let before = entry.value().len();
            entry.value_mut().retain(|h| !h.is_finished());
            let after = entry.value().len();
            removed_handles += before - after;

            if after == 0 {
                to_remove.push(entry.key().clone());
            }
        }

        // Remove entries that became empty (can't remove during iter_mut).
        //
        // Race window: a concurrent supersede_eager_open_tasks could insert a new
        // empty placeholder for the same URI between iter_mut completing and this
        // remove call, causing us to delete the fresh placeholder. This is benign:
        // push_or_abort_eager_open_handle handles the missing entry by aborting
        // the task, and the next didChange re-triggers eager open (self-healing).
        let cleaned_docs = to_remove.len();
        for uri in &to_remove {
            self.eager_open_tasks.remove(uri);
        }

        if removed_handles > 0 {
            log::trace!(
                target: "kakehashi::bridge",
                "Cleaned up {} finished eager-open handles across {} documents ({} entries removed)",
                removed_handles,
                cleaned_docs,
                cleaned_docs
            );
        }
    }

    /// Cancel all eager-open tasks for a document.
    ///
    /// Called on didClose to prevent orphaned virtual documents when tasks
    /// are still waiting for server readiness.
    ///
    /// # Arguments
    /// * `uri` - The host document URI
    pub(crate) fn cancel_eager_open(&self, uri: &Url) {
        if let Some((_, handles)) = self.eager_open_tasks.remove(uri) {
            log::debug!(
                target: "kakehashi::bridge",
                "Cancelling {} eager-open tasks for {}",
                handles.len(),
                uri
            );
            for handle in handles {
                handle.abort();
            }
        }
    }

    /// Abort all eager-open tasks (called on shutdown).
    ///
    /// Ensures clean shutdown by cancelling all background tasks that may
    /// still be waiting for server readiness.
    pub(crate) fn abort_all_eager_open(&self) {
        let count: usize = self.eager_open_tasks.iter().map(|e| e.value().len()).sum();
        if count > 0 {
            log::debug!(
                target: "kakehashi::bridge",
                "Aborting {} eager-open tasks across {} documents",
                count,
                self.eager_open_tasks.len()
            );
        }

        for entry in self.eager_open_tasks.iter() {
            for handle in entry.value().iter() {
                handle.abort();
            }
        }
        self.eager_open_tasks.clear();
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
            .field("region_id_tracker", &"RegionIdTracker")
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
    use super::*;
    use crate::config::LanguageSettings;
    use crate::config::settings::BridgeLanguageConfig;
    use std::collections::HashMap;

    #[test]
    fn test_get_config_respects_bridge_filter() {
        let coordinator = BridgeCoordinator::new();

        // Create settings with a markdown host that only allows python bridging
        let mut languages = HashMap::new();
        let mut bridge_filter = HashMap::new();
        bridge_filter.insert("python".to_string(), BridgeLanguageConfig { enabled: true });
        languages.insert(
            "markdown".to_string(),
            LanguageSettings::with_bridge(None, None, Some(bridge_filter)),
        );

        // Create language server config for rust
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        let settings = WorkspaceSettings::with_language_servers(
            vec![],
            languages,
            HashMap::new(),
            false,
            Some(servers),
        );

        // rust should be blocked by markdown's bridge filter
        let result = coordinator.get_config_for_language(&settings, "markdown", "rust");
        assert!(
            result.is_none(),
            "rust should be blocked by markdown's bridge filter"
        );
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
                workspace_type: None,
            },
        );

        let settings = WorkspaceSettings::with_language_servers(
            vec![],
            languages,
            HashMap::new(),
            false,
            Some(servers),
        );

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
                workspace_type: None,
            },
        );
        servers.insert(
            "ruff".to_string(),
            BridgeServerConfig {
                cmd: vec!["ruff".to_string(), "server".to_string()],
                languages: vec!["python".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        let settings = WorkspaceSettings::with_language_servers(
            vec![],
            languages,
            HashMap::new(),
            false,
            Some(servers),
        );

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
        bridge_filter.insert("python".to_string(), BridgeLanguageConfig { enabled: true });
        languages.insert(
            "markdown".to_string(),
            LanguageSettings::with_bridge(None, None, Some(bridge_filter)),
        );

        // Create language server config for rust (which is NOT in the filter)
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        let settings = WorkspaceSettings::with_language_servers(
            vec![],
            languages,
            HashMap::new(),
            false,
            Some(servers),
        );

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
                workspace_type: None,
            },
        );

        let settings = WorkspaceSettings::with_language_servers(
            vec![],
            languages,
            HashMap::new(),
            false,
            Some(servers),
        );

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
        let handles = vec![task1.abort_handle(), task2.abort_handle()];

        // Insert handles directly for this URI
        coordinator.eager_open_tasks.insert(uri.clone(), handles);

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
        coordinator
            .eager_open_tasks
            .insert(uri.clone(), vec![task1.abort_handle()]);

        // Second batch — supersede should abort the first batch and insert placeholder
        coordinator.supersede_eager_open_tasks(&uri);

        // Push a new task into the placeholder
        let task2 = tokio::spawn(futures::future::pending::<()>());
        coordinator.push_or_abort_eager_open_handle(&uri, task2.abort_handle());

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
    fn test_get_config_uses_wildcard_for_undefined_host() {
        let coordinator = BridgeCoordinator::new();

        // Create settings with wildcard that blocks all bridging
        let mut languages = HashMap::new();
        languages.insert(
            "_".to_string(),
            LanguageSettings::with_bridge(None, None, Some(HashMap::new())), // empty = block all
        );

        // Create language server config for rust
        let mut servers = HashMap::new();
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        let settings = WorkspaceSettings::with_language_servers(
            vec![],
            languages,
            HashMap::new(),
            false,
            Some(servers),
        );

        // "quarto" is not defined, so it inherits from wildcard which blocks all
        let result = coordinator.get_config_for_language(&settings, "quarto", "rust");
        assert!(
            result.is_none(),
            "quarto should inherit wildcard's empty filter"
        );
    }

    #[tokio::test]
    async fn test_cleanup_handles_all_cases() {
        let coordinator = BridgeCoordinator::new();

        let uri_a = Url::parse("file:///a.md").unwrap();
        let uri_b = Url::parse("file:///b.md").unwrap();
        let uri_c = Url::parse("file:///c.md").unwrap();

        // URI_A: 2 tasks that complete immediately (all finished → should be removed)
        let finished_a1 = tokio::spawn(async {});
        let finished_a2 = tokio::spawn(async {});
        coordinator.eager_open_tasks.insert(
            uri_a.clone(),
            vec![finished_a1.abort_handle(), finished_a2.abort_handle()],
        );

        // URI_B: 1 finished task + 1 running task (mixed → should filter out finished)
        let finished_b = tokio::spawn(async {});
        let running_b = tokio::spawn(futures::future::pending::<()>());
        coordinator.eager_open_tasks.insert(
            uri_b.clone(),
            vec![finished_b.abort_handle(), running_b.abort_handle()],
        );

        // URI_C: 2 running tasks (all running → no cleanup needed)
        let running_c1 = tokio::spawn(futures::future::pending::<()>());
        let running_c2 = tokio::spawn(futures::future::pending::<()>());
        coordinator.eager_open_tasks.insert(
            uri_c.clone(),
            vec![running_c1.abort_handle(), running_c2.abort_handle()],
        );

        // Let finished tasks complete
        tokio::task::yield_now().await;

        // Single pass cleans all entries
        coordinator.cleanup_finished_eager_open_tasks();

        // URI_A should be removed entirely (all finished)
        assert!(
            coordinator.eager_open_tasks.get(&uri_a).is_none(),
            "URI_A should be removed (all handles finished)"
        );

        // URI_B should have only 1 handle remaining (the running one)
        let uri_b_handles = coordinator.eager_open_tasks.get(&uri_b).unwrap();
        assert_eq!(
            uri_b_handles.value().len(),
            1,
            "URI_B should have 1 handle after cleanup filters out the finished one"
        );

        // URI_C: should still have 2 handles (all running)
        let uri_c_handles = coordinator.eager_open_tasks.get(&uri_c).unwrap();
        assert_eq!(
            uri_c_handles.value().len(),
            2,
            "URI_C should still have 2 handles (all running)"
        );
    }

    #[tokio::test]
    async fn test_cleanup_all_finished_entries_removed() {
        // Regression: old two-pass code skipped all-finished entries in pass 2.
        let coordinator = BridgeCoordinator::new();

        let uri_a = Url::parse("file:///a.md").unwrap();
        let uri_b = Url::parse("file:///b.md").unwrap();

        // Both URIs have all-finished tasks
        let finished_a = tokio::spawn(async {});
        let finished_b = tokio::spawn(async {});
        coordinator
            .eager_open_tasks
            .insert(uri_a.clone(), vec![finished_a.abort_handle()]);
        coordinator
            .eager_open_tasks
            .insert(uri_b.clone(), vec![finished_b.abort_handle()]);

        // Let tasks complete
        tokio::task::yield_now().await;

        // Single pass should clean up all finished entries
        coordinator.cleanup_finished_eager_open_tasks();

        assert!(
            coordinator.eager_open_tasks.get(&uri_a).is_none()
                && coordinator.eager_open_tasks.get(&uri_b).is_none(),
            "Both all-finished entries should be removed, but got: a={}, b={}",
            coordinator.eager_open_tasks.get(&uri_a).is_some(),
            coordinator.eager_open_tasks.get(&uri_b).is_some(),
        );
    }

    #[tokio::test]
    async fn test_push_or_abort_adds_handle_when_entry_exists() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Pre-insert placeholder (simulates supersede_eager_open_tasks)
        coordinator.eager_open_tasks.insert(uri.clone(), vec![]);

        // Spawn a task and push its handle
        let task = tokio::spawn(futures::future::pending::<()>());
        let handle = task.abort_handle();
        coordinator.push_or_abort_eager_open_handle(&uri, handle);

        // Entry should now have 1 handle
        let entry = coordinator.eager_open_tasks.get(&uri).unwrap();
        assert_eq!(entry.value().len(), 1, "should have 1 handle after push");
        assert!(!task.is_finished(), "task should still be running");
    }

    #[tokio::test]
    async fn test_push_or_abort_aborts_when_entry_removed() {
        let coordinator = BridgeCoordinator::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Do NOT insert a placeholder — simulates concurrent cancel removing the entry

        // Spawn a task and try to push its handle
        let task = tokio::spawn(futures::future::pending::<()>());
        let handle = task.abort_handle();
        coordinator.push_or_abort_eager_open_handle(&uri, handle);

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
        coordinator
            .eager_open_tasks
            .insert(uri.clone(), vec![previous_task.abort_handle()]);

        // Supersede — should abort previous and insert empty placeholder
        coordinator.supersede_eager_open_tasks(&uri);

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
            entry.value().len(),
            0,
            "supersede should insert empty placeholder"
        );
    }
}
