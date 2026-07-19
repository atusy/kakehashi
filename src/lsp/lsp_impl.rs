mod apply_edit_translation;
pub(crate) mod bridge_context;
mod cli;
mod coordinator;
pub(crate) use coordinator::DiagnosticPublisher;
pub(crate) mod kakehashi;
mod lifecycle;
mod parse_scheduler;
mod region_offset;
mod show_document_translation;
mod snapshot_read;
pub(crate) mod text_document;
mod tree_worker_shadow;
mod whole_document;
mod workspace;

use crate::error::LockResultExt;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoImplementationParams,
    GotoImplementationResponse, GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionParams, CodeActionResponse, CodeLens, CodeLensParams, CompletionItem,
    CompletionParams, CompletionResponse, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightParams, DocumentLink,
    DocumentLinkParams, DocumentOnTypeFormattingParams, DocumentRangeFormattingParams,
    DocumentSymbolParams, DocumentSymbolResponse, ExecuteCommandParams, FoldingRange,
    FoldingRangeParams, GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams,
    InitializeParams, InitializeResult, InitializedParams, InlayHint, InlayHintParams,
    LinkedEditingRangeParams, LinkedEditingRanges, Location, Moniker, MonikerParams,
    PrepareRenameResponse, ReferenceParams, RenameParams, SelectionRange, SelectionRangeParams,
    SemanticTokensDeltaParams, SemanticTokensFullDeltaResult, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult, SignatureHelp,
    SignatureHelpParams, TextDocumentPositionParams, TextEdit, Uri, WillSaveTextDocumentParams,
    WorkspaceEdit,
};
use tower_lsp_server::ls_types::{
    ColorInformation, ColorPresentation, ColorPresentationParams, DocumentColorParams,
};
use tower_lsp_server::{Client, LanguageServer};
use url::Url;

use crate::config::{RawWorkspaceSettings, WorkspaceSettings};
use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::bridge::ResolvedServerConfig;
use crate::lsp::client::ClientNotifier;
use crate::lsp::settings_manager::SettingsManager;

use super::auto_install::{AutoInstallManager, InstallingLanguages};
use super::cache::CacheCoordinator;
use super::debounced_diagnostics::DebouncedDiagnosticsManager;
use super::synthetic_diagnostics::SyntheticDiagnosticsManager;

pub(super) fn uri_to_url(uri: &Uri) -> std::result::Result<Url, url::ParseError> {
    Url::parse(uri.as_str())
}

pub(super) fn build_notifier<'a>(
    client: &'a Client,
    settings_manager: &'a SettingsManager,
) -> ClientNotifier<'a> {
    ClientNotifier::new(
        client.clone(),
        settings_manager,
        settings_manager.client_capabilities_lock(),
    )
}

/// Detect the canonical language for a document using the full language-detection-fallback-chain.
///
/// This uses the stored document text and optional language_id so alias resolution
/// still works even if the document is accessed before didOpen fully completes.
pub(super) fn detect_document_language(
    language: &std::sync::Arc<LanguageCoordinator>,
    documents: &DocumentStore,
    uri: &Url,
) -> Option<String> {
    let path = uri.path();

    if let Some(doc) = documents.get(uri) {
        language.detect_language(path, doc.text(), None, doc.language_id())
    } else {
        language.detect_language(path, "", None, None)
    }
}

pub(super) fn bridge_configs_for_injection_language(
    bridge: &BridgeCoordinator,
    settings_manager: &SettingsManager,
    host_language: &str,
    injection_language: &str,
) -> Vec<ResolvedServerConfig> {
    let settings = settings_manager.load_settings();
    bridge.cached_configs_for_injection_language(&settings, host_language, injection_language)
}

pub(super) struct ReloadLanguageState<'a> {
    language: &'a LanguageCoordinator,
    parser_pool: &'a std::sync::Mutex<DocumentParserPool>,
    documents: &'a DocumentStore,
    invalidate_documents: bool,
    request_semantic_refresh: bool,
}

pub(super) struct SettingsReloadInput {
    raw_settings: Option<RawWorkspaceSettings>,
    settings: WorkspaceSettings,
}

/// One server process owns one effective workspace settings snapshot. Keep the
/// complete async reload transaction ordered across didChangeConfiguration and
/// post-install reloads, including downstream propagation and SettingsManager
/// publication after the synchronous language/query swap.
static SETTINGS_RELOAD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) async fn lock_settings_reload() -> tokio::sync::MutexGuard<'static, ()> {
    SETTINGS_RELOAD_LOCK.lock().await
}

struct ParserReloadGuard<'a> {
    parser_pool: &'a std::sync::Mutex<DocumentParserPool>,
    language: &'a LanguageCoordinator,
}

impl<'a> ParserReloadGuard<'a> {
    fn begin(
        parser_pool: &'a std::sync::Mutex<DocumentParserPool>,
        language: &'a LanguageCoordinator,
    ) -> Self {
        parser_pool
            .lock()
            .recover_poison("ParserReloadGuard::begin")
            .begin_reload();
        Self {
            parser_pool,
            language,
        }
    }
}

impl Drop for ParserReloadGuard<'_> {
    fn drop(&mut self) {
        self.parser_pool
            .lock()
            .recover_poison("ParserReloadGuard::drop")
            .finish_reload_at(self.language.configuration_generation());
    }
}

pub(super) async fn apply_shared_settings(
    client: &Client,
    language_state: ReloadLanguageState<'_>,
    settings_manager: &SettingsManager,
    cache: &CacheCoordinator,
    bridge: &BridgeCoordinator,
    raw_settings: Option<RawWorkspaceSettings>,
    settings: WorkspaceSettings,
) -> Vec<Url> {
    let reload = lock_settings_reload().await;
    apply_shared_settings_locked(
        &reload,
        client,
        language_state,
        settings_manager,
        cache,
        bridge,
        SettingsReloadInput {
            raw_settings,
            settings,
        },
    )
    .await
}

pub(super) async fn apply_shared_settings_locked(
    _reload: &tokio::sync::MutexGuard<'static, ()>,
    client: &Client,
    language_state: ReloadLanguageState<'_>,
    settings_manager: &SettingsManager,
    cache: &CacheCoordinator,
    bridge: &BridgeCoordinator,
    input: SettingsReloadInput,
) -> Vec<Url> {
    let SettingsReloadInput {
        raw_settings,
        settings,
    } = input;
    // TRANSITIONAL generation bump BEFORE any query/config mutation: from
    // this instant, every generation-stamped product built from the OLD
    // queries (snapshot-riding discovery/bridge/resolved regions, layer
    // trees, kind queries, walk memos) stops matching and consumers fall
    // back inline — without it, `load_settings` swaps the queries first and
    // the (possibly long: `propagate_settings` awaits the network) window
    // until the post-reload bump kept serving old-query products as current.
    // The second bump below then also invalidates anything a racing request
    // built MID-swap against a half-updated query set.
    cache.bump_semantic_token_generation();
    crate::analysis::semantic::invalidate_thread_local_parser_caches();
    let parser_reload =
        ParserReloadGuard::begin(language_state.parser_pool, language_state.language);
    let mut summary = language_state.language.load_settings(&settings);
    let reparse_uris = if language_state.invalidate_documents {
        language_state.documents.invalidate_all_parses()
    } else {
        Vec::new()
    };
    // Invalidate again after the synchronous swap: the first bump rejects
    // pre-reload checkouts, while this one rejects parsers acquired while the
    // registry and query stores were being replaced.
    drop(parser_reload);
    crate::analysis::semantic::invalidate_thread_local_parser_caches();
    // Second bump IMMEDIATELY after the query swap, before any await: a
    // request that started after the transitional bump above but before the
    // swap computed against the OLD queries yet stamped the new generation —
    // without this bump those products would be accepted as current for the
    // whole awaited propagate below. (The final bump after apply_settings
    // covers the settings-side inputs the apply swaps.)
    cache.bump_semantic_token_generation();
    // Publish the settings snapshot before invalidating downstream connections:
    // once propagation exposes a pool miss, a concurrent request must resolve
    // the NEW launch config rather than respawn from the old snapshot (#587).
    match raw_settings {
        Some(raw_settings) => settings_manager.apply_settings_with_raw(raw_settings, settings),
        None => settings_manager.apply_settings(settings),
    }
    let settings = settings_manager.load_settings();
    // Update the reader-side copy before propagating downstream settings so a
    // newly suppressed log cannot occupy the bounded window queue after this
    // configuration application completes.
    bridge
        .pool()
        .set_log_message_level(settings.features.window_log_message)
        .await;
    // Path c: apply downstream config at this single reload choke point
    // (initialize, didChangeConfiguration, auto-install reload): push runtime
    // settings in place and recycle connections whose launch config changed.
    // At initialize time there are no connections yet, so it is a clean no-op
    // (downstream-settings-propagation).
    let pushed = bridge.propagate_settings(&settings).await;
    if pushed > 0 {
        log::debug!(
            target: "kakehashi::bridge",
            "Propagated settings change to {} downstream connection(s)", pushed
        );
    }
    // A settings/query reload can change tokenization for unchanged text, so
    // bump the semantic-token cache generation once more: the ladder is one
    // bump per mutation boundary — before the query swap (kills old-query
    // stamps), after it (kills mid-swap stamps), and here after the settings
    // apply (kills stamps that read the new queries but the OLD
    // settings-side inputs, e.g. capture mappings, during the awaited
    // propagate).
    // Done at this single choke point so every reload path (initialize,
    // didChangeConfiguration, and the auto-install reload) is covered, and
    // *before* the refresh below so the editor's re-request recomputes. The
    // generation bump (not a bare clear) also defeats a request that was
    // mid-tokenization across the reload and stores afterwards: it captured
    // an old generation, so its entry can't be served post-reload.
    // `didChange` deliberately does NOT invalidate (delta needs the previous
    // tokens); only a query/config reload does.
    cache.bump_semantic_token_generation();
    // Query removal/replacement affects unchanged documents too. Request one
    // workspace refresh for every reload; ClientNotifier capability-gates and
    // coalesces it with any language-specific refresh events in this batch.
    if language_state.request_semantic_refresh {
        summary
            .events
            .push(crate::language::LanguageEvent::semantic_tokens_refresh(
                "workspace settings reload",
            ));
    } else {
        // Initialization may produce language-specific refresh events (for
        // example while registering a derived language). No refresh request is
        // valid before InitializeResult/initialized, and no document has yet
        // consumed tokens, so keep the logs while stripping every refresh.
        summary.events.retain(|event| {
            !matches!(
                event,
                crate::language::LanguageEvent::SemanticTokensRefresh { .. }
            )
        });
    }
    build_notifier(client, settings_manager)
        .log_language_events(&summary.events)
        .await;
    reparse_uris
}

/// Convert url::Url to ls_types::Uri, the reverse conversion for bridge protocol
/// functions that need ls_types::Uri while internal storage holds url::Url.
///
/// Both `url::Url` and `fluent_uri::Uri` implement RFC 3986, so an `Err`
/// (`LspError::Internal`) indicates an edge-case parser difference and should be
/// extremely rare.
pub(crate) fn url_to_uri(url: &Url) -> std::result::Result<Uri, crate::error::LspError> {
    use std::str::FromStr;
    Uri::from_str(url.as_str()).map_err(|e| {
        log::error!(
            target: "kakehashi::protocol",
            "URI conversion failed (potential library incompatibility): url={}, error={}",
            url.as_str(),
            e
        );
        crate::error::LspError::internal(format!(
            "Failed to convert URL to URI: {}. Please report this as a bug.",
            url.as_str()
        ))
    })
}

pub struct Kakehashi {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    /// Bounded compute pool for all synchronous tree-CPU (parse-snapshot ADR §4):
    /// keeps tree work off the tokio workers so timers and unrelated documents'
    /// handlers stay pollable.
    compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    documents: std::sync::Arc<DocumentStore>,
    /// Unified cache coordinator for semantic tokens, injections, and request tracking
    cache: std::sync::Arc<CacheCoordinator>,
    /// Consolidated settings, capabilities, and workspace root management
    settings_manager: std::sync::Arc<SettingsManager>,
    /// Isolated coordinator for parser auto-installation
    auto_install: AutoInstallManager,
    /// Bridge coordinator for downstream LS pool and node tracking
    bridge: std::sync::Arc<BridgeCoordinator>,
    /// Manager for synthetic (background) diagnostic push tasks (pull-first-diagnostic-forwarding Phase 2).
    /// Wrapped in Arc for sharing with debounced diagnostics (Phase 3).
    synthetic_diagnostics: std::sync::Arc<SyntheticDiagnosticsManager>,
    /// Manager for debounced didChange diagnostic triggers (pull-first-diagnostic-forwarding Phase 3)
    debounced_diagnostics: std::sync::Arc<DebouncedDiagnosticsManager>,
    /// Per-host proactive diagnostic cache — the single source of truth for
    /// `textDocument/publishDiagnostics` (push-propagation-diagnostic-forwarding).
    /// Both the host-event pull feed and downstream pushes write slots here; one
    /// publisher merges and emits per host.
    diagnostics: std::sync::Arc<crate::lsp::diagnostic_cache::DiagnosticAggregator>,
    /// Token for cancelling the upstream forwarding task on shutdown.
    ///
    /// Without this, the forwarding task only exits when all channel senders are
    /// dropped. Cancelling the token gives deterministic shutdown: `shutdown()`
    /// cancels → task exits immediately → no waiting for channel drainage.
    shutdown_token: tokio_util::sync::CancellationToken,
    /// Pre-computed home directory for tilde expansion in config paths.
    /// Computed once at construction — `dirs::home_dir()` is stable for the
    /// process lifetime.
    home_dir: Option<String>,
    /// Previous full results per `(uri, kind)` for `kakehashi/captures/full/delta`
    /// (captures-protocol §"Delta semantics"). One lineage slot **per
    /// injection mode** — index `0` host-only, index `1` injection — each
    /// holding `(resultId, matches as wire JSON)`. Deltas carry no `injection`
    /// parameter: `previousResultId` selects its mode by matching a slot, so a
    /// host-only client and an injection client sharing a document never
    /// clobber each other's mode. Entries are dropped on `didClose` (the
    /// insert sites serialize with close via the per-URI edit lock); a stale
    /// `resultId` falls back to a full response only when a single mode slot
    /// is live (unambiguous), otherwise `null`.
    #[allow(clippy::type_complexity)]
    captures_cache: dashmap::DashMap<
        (Url, String),
        [Option<(String, std::sync::Arc<Vec<serde_json::Value>>)>; 2],
    >,
    /// Memoized captures walk results per `(uri, kind, injection-mode)`,
    /// tagged with the exact inputs they were computed from — snapshot
    /// `(parsed_version, incarnation)` and the settings generation. A typing
    /// client sends `captures/full` AND `captures/full/delta` per keystroke;
    /// both walk the same snapshot, so the second (and any repeat on an
    /// unchanged document) serves the memo instead of re-executing the kind
    /// query over every layer. One entry per key (insert-overwrites), dropped
    /// with the lineage cache on `didClose`.
    #[allow(clippy::type_complexity)]
    captures_walk_cache:
        dashmap::DashMap<(Url, String, bool), kakehashi::captures::CachedCapturesWalk>,
    /// Single-flight state for in-progress captures walks, keyed like the
    /// walk memo above. Concurrent identical requests (a client's `full` +
    /// `delta`s for one kind and snapshot) park on the winner; a fresher
    /// `(incarnation, generation, parsed_version)` atomically replaces and
    /// cancels obsolete work. Entries are removed by a pointer-checked winner
    /// guard, so an old winner cannot erase its successor's marker.
    #[allow(clippy::type_complexity)]
    captures_walk_inflight: dashmap::DashMap<
        (Url, String, bool),
        std::sync::Arc<kakehashi::captures::CapturesWalkFlight>,
    >,
    /// Cross-snapshot, content-addressed cache of per-layer captures query
    /// matches: a layer whose content merely SHIFTED between snapshots (the
    /// per-keystroke common case — an edit above a fence) reuses its cached
    /// ID-free match data instead of re-executing the kind query. `Arc`'d
    /// because the compute-pool walk runs off `self`. Swept per completed
    /// current-at-entry full walk, dropped on `didClose`; see the
    /// `captures_match_cache` module docs.
    captures_match_cache: std::sync::Arc<kakehashi::captures_match_cache::CapturesMatchCache>,
    /// True when the process runs as a one-shot CLI (`kakehashi diagnose`/`format`)
    /// rather than a long-lived LSP server. Set once by `cli_initialize`. No editor
    /// consumes a proactive `publishDiagnostics` in CLI mode (the stub client pump
    /// discards it), so `did_open_impl` skips the synthetic diagnostic task — that
    /// avoids both the wasted downstream pull and the abort-vs-`didClose` race (#489).
    cli_mode: std::sync::atomic::AtomicBool,
    /// Runtime opt-in for experimental features (`KAKEHASHI_EXPERIMENTAL=true`,
    /// see [`crate::experimental`]) — currently the native lexical-resolution
    /// layer and documentColor / colorPresentation (capability advertisement
    /// and handlers). Copied from the environment once at construction; atomic
    /// only so tests can toggle it per instance.
    experimental: std::sync::atomic::AtomicBool,
    /// Per-document coalescing scheduler for the off-ingress parse
    /// (per-document-parse-scheduler ADR): `did_change` applies the edit and clears the
    /// tree synchronously, then schedules the (re)parse here so the parse runs off
    /// the ingress writer ticket, coalescing bursts to one reparse over the latest
    /// text. Shared (`Arc`) with the spawned parse loops.
    parse_scheduler: std::sync::Arc<parse_scheduler::ParseScheduler>,
    /// Non-authoritative, opt-in mirror of document parser state in one
    /// process-isolated worker. Queue failure disables only this shadow path.
    tree_worker_shadow: std::sync::Arc<tree_worker_shadow::TreeWorkerShadow>,
}

impl std::fmt::Debug for Kakehashi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kakehashi")
            .field("client", &self.client)
            .field("language", &"LanguageCoordinator")
            .field("parser_pool", &"Mutex<DocumentParserPool>")
            .field("documents", &"DocumentStore")
            .field("cache", &"CacheCoordinator")
            .field("settings_manager", &"SettingsManager")
            .field("auto_install", &"AutoInstallManager")
            .field("bridge", &"BridgeCoordinator")
            .field("synthetic_diagnostics", &"SyntheticDiagnosticsManager")
            .field("debounced_diagnostics", &"DebouncedDiagnosticsManager")
            .field("shutdown_token", &"CancellationToken")
            .field("home_dir", &self.home_dir)
            .finish_non_exhaustive()
    }
}

impl Kakehashi {
    pub fn new(client: Client) -> Self {
        Self::build(client, BridgeCoordinator::new())
    }

    /// Create a Kakehashi instance with an externally-provided pool and cancel forwarder.
    ///
    /// This is used when the pool/forwarder needs to be shared with other components,
    /// such as the cancel forwarding middleware.
    ///
    /// The `cancel_forwarder` MUST be created from the same `pool` to ensure cancel
    /// notifications are properly routed between the middleware and handlers.
    pub fn with_cancel_forwarder(
        client: Client,
        pool: std::sync::Arc<super::bridge::LanguageServerPool>,
        cancel_forwarder: super::request_id::CancelForwarder,
    ) -> Self {
        Self::build(
            client,
            BridgeCoordinator::with_cancel_forwarder(pool, cancel_forwarder),
        )
    }

    fn build(client: Client, bridge: BridgeCoordinator) -> Self {
        let language = std::sync::Arc::new(LanguageCoordinator::new());
        let parser_pool = language.create_document_parser_pool();

        // Initialize auto-install manager with crash detection
        let failed_parsers = AutoInstallManager::init_failed_parser_registry();
        let auto_install = AutoInstallManager::new(InstallingLanguages::new(), failed_parsers);

        Self {
            client,
            language,
            parser_pool: std::sync::Arc::new(std::sync::Mutex::new(parser_pool)),
            compute_pool: std::sync::Arc::new(crate::compute_pool::ComputePool::new()),
            documents: std::sync::Arc::new(DocumentStore::new()),
            cache: std::sync::Arc::new(CacheCoordinator::new()),
            settings_manager: std::sync::Arc::new(SettingsManager::new()),
            auto_install,
            bridge: std::sync::Arc::new(bridge),
            synthetic_diagnostics: std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            debounced_diagnostics: std::sync::Arc::new(DebouncedDiagnosticsManager::new()),
            diagnostics: std::sync::Arc::new(
                crate::lsp::diagnostic_cache::DiagnosticAggregator::new(),
            ),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            home_dir: dirs::home_dir().map(|p| p.to_string_lossy().into_owned()),
            captures_cache: dashmap::DashMap::new(),
            captures_walk_cache: dashmap::DashMap::new(),
            captures_walk_inflight: dashmap::DashMap::new(),
            captures_match_cache: std::sync::Arc::new(
                kakehashi::captures_match_cache::CapturesMatchCache::new(),
            ),
            cli_mode: std::sync::atomic::AtomicBool::new(false),
            experimental: std::sync::atomic::AtomicBool::new(crate::experimental::enabled()),
            parse_scheduler: std::sync::Arc::new(parse_scheduler::ParseScheduler::default()),
            tree_worker_shadow: std::sync::Arc::new(
                tree_worker_shadow::TreeWorkerShadow::from_environment(),
            ),
        }
    }

    /// Mark this instance as running in one-shot CLI mode. Called by
    /// `cli_initialize`; see the `cli_mode` field.
    pub(crate) fn mark_cli_mode(&self) {
        self.cli_mode
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this instance runs as a one-shot CLI rather than an LSP server.
    pub(crate) fn is_cli_mode(&self) -> bool {
        self.cli_mode.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether experimental features are enabled for this instance; see the
    /// `experimental` field.
    pub(crate) fn experimental_enabled(&self) -> bool {
        self.experimental.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Toggle experimental features for this instance — tests only; the
    /// production value comes from the environment at construction.
    #[cfg(test)]
    pub(crate) fn set_experimental(&self, enabled: bool) {
        self.experimental
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// Create a `ClientNotifier` for centralized client communication.
    ///
    /// The notifier wraps the LSP client and references the stored capabilities,
    /// providing a clean API for logging, progress notifications, and semantic
    /// token refresh requests.
    pub(super) fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }

    #[cfg(test)]
    async fn apply_raw_settings(
        &self,
        raw_settings: RawWorkspaceSettings,
        settings: WorkspaceSettings,
    ) {
        let reload = lock_settings_reload().await;
        self.apply_raw_settings_locked(&reload, raw_settings, settings)
            .await;
    }

    async fn apply_raw_settings_locked(
        &self,
        reload: &tokio::sync::MutexGuard<'static, ()>,
        raw_settings: RawWorkspaceSettings,
        settings: WorkspaceSettings,
    ) {
        let reparse_uris = apply_shared_settings_locked(
            reload,
            &self.client,
            ReloadLanguageState {
                language: &self.language,
                parser_pool: &self.parser_pool,
                documents: &self.documents,
                invalidate_documents: true,
                request_semantic_refresh: true,
            },
            &self.settings_manager,
            &self.cache,
            &self.bridge,
            SettingsReloadInput {
                raw_settings: Some(raw_settings),
                settings,
            },
        )
        .await;
        self.refresh_tree_worker_documents(&self.documents.open_uris());
        self.tree_worker_shadow
            .configuration_changed(self.language.configuration_generation());
        reparse_uris
            .into_iter()
            .for_each(|uri| self.schedule_reparse(uri, None));
    }

    fn refresh_tree_worker_documents(&self, uris: &[Url]) {
        if !self.tree_worker_shadow.is_enabled() {
            return;
        }
        for uri in uris {
            let Some((incarnation, content_version, language, text)) =
                self.documents.get(uri).and_then(|document| {
                    Some((
                        document.incarnation(),
                        document.content_version(),
                        document.language_id()?.to_owned(),
                        document.text().to_owned(),
                    ))
                })
            else {
                continue;
            };
            let Some(grammar) = self.language.worker_grammar_descriptor(&language) else {
                self.tree_worker_shadow.mirror_close(
                    uri,
                    incarnation,
                    content_version,
                    self.language.configuration_generation(),
                );
                continue;
            };
            self.tree_worker_shadow.mirror_full(
                uri,
                incarnation,
                content_version,
                language,
                grammar,
                text,
            );
        }
    }

    async fn apply_initial_settings(
        &self,
        raw_settings: RawWorkspaceSettings,
        settings: WorkspaceSettings,
    ) {
        let warnings = Self::misconfigured_settings_warnings(&settings);
        apply_shared_settings(
            &self.client,
            ReloadLanguageState {
                language: &self.language,
                parser_pool: &self.parser_pool,
                documents: &self.documents,
                invalidate_documents: false,
                request_semantic_refresh: false,
            },
            &self.settings_manager,
            &self.cache,
            &self.bridge,
            Some(raw_settings),
            settings,
        )
        .await
        .into_iter()
        .for_each(|uri| self.schedule_reparse(uri, None));
        self.warn_on_misconfigured_settings(&warnings).await;
    }

    /// Build client-visible warnings for settings misconfigurations.
    ///
    /// One warning summarizes all (host, injection) pairs whose configured
    /// `textDocument/formatting` aggregation is the misconfigured
    /// `Concatenated`-with-empty-`priorities` combination. The concatenated
    /// formatting pipeline requires a non-empty `priorities` list (it defines
    /// pipeline membership and order — ADR concatenated-formatting-pipeline
    /// Decision point 2); without one the region falls back to `preferred`.
    /// Another warning summarizes all language servers that cannot be spawned.
    /// Surfacing these mismatches at settings-apply time avoids silent
    /// misbehavior on every affected request.
    ///
    /// Previously this emitted one notification per pair, which floods the
    /// editor log on `didChangeConfiguration` reloads for workspaces with
    /// many bridge entries.
    fn misconfigured_settings_warnings(settings: &WorkspaceSettings) -> Vec<String> {
        let mut warnings = Vec::new();
        let pairs = bridge_context::concatenated_formatting_pairs(settings);
        if let Some(msg) = bridge_context::format_concatenated_formatting_warning(&pairs) {
            warnings.push(msg);
        }
        let unspawnable = bridge_context::unspawnable_language_servers(settings);
        if let Some(msg) = bridge_context::format_unspawnable_servers_warning(&unspawnable) {
            warnings.push(msg);
        }
        warnings
    }

    async fn warn_on_misconfigured_settings(&self, warnings: &[String]) {
        for warning in warnings {
            self.notifier().log_warning(warning).await;
        }
    }

    pub(super) fn parse_coordinator(&self) -> coordinator::ParseCoordinator {
        coordinator::ParseCoordinator::new(self)
    }

    pub(super) fn document_language(&self, uri: &Url) -> Option<String> {
        detect_document_language(&self.language, &self.documents, uri)
    }

    pub(super) fn bridge_configs_for_injection_language(
        &self,
        host_language: &str,
        injection_language: &str,
    ) -> Vec<ResolvedServerConfig> {
        bridge_configs_for_injection_language(
            &self.bridge,
            &self.settings_manager,
            host_language,
            injection_language,
        )
    }

    pub(super) fn install_coordinator(&self) -> coordinator::InstallCoordinator {
        coordinator::InstallCoordinator::new(self)
    }

    pub(super) fn injection_coordinator(&self) -> coordinator::InjectionCoordinator {
        coordinator::InjectionCoordinator::new(self)
    }

    pub(super) fn diagnostic_scheduler(&self) -> coordinator::DiagnosticScheduler {
        coordinator::DiagnosticScheduler::new(self)
    }

    /// Schedule the **off-ingress** (re)parse of `uri` after a `did_change` applied
    /// the edit and cleared the tree (per-document-parse-scheduler ADR). The parse runs
    /// in a spawned loop off the writer ticket, so `did_change` returns without
    /// waiting on it; the [`ParseScheduler`](parse_scheduler::ParseScheduler)
    /// coalesces a burst of edits into a single follow-up reparse over the latest
    /// text. The loop reparses, then runs the tree-dependent downstream work
    /// (injected-language processing + bridge `didChange` forwarding, then the
    /// diagnostic geometry re-merge), then loops if another edit arrived.
    ///
    /// Resurrection-safe with no teardown: the parse's non-inserting CAS no-ops
    /// once a `didClose` removed the document, so a `Close` needs no actor
    /// coordination.
    pub(crate) fn schedule_reparse(&self, uri: Url, ticket: Option<u64>) {
        if !self.parse_scheduler.schedule(&uri, ticket) {
            // A parse loop is already running for this document; it has been
            // marked dirty and will reparse the latest text when it finishes.
            return;
        }

        let parse = self.parse_coordinator();
        let injection = self.injection_coordinator();
        let publisher = DiagnosticPublisher::new(self);
        let diagnostic_scheduler = self.diagnostic_scheduler();
        let diagnostics = std::sync::Arc::clone(&self.diagnostics);
        let documents = std::sync::Arc::clone(&self.documents);
        let scheduler = std::sync::Arc::clone(&self.parse_scheduler);
        let shutdown = self.shutdown_token.clone();

        tokio::spawn(async move {
            // If the loop panics in its glue (the parse work-unit is already
            // panic-isolated by the compute pool), this guard clears the stuck
            // `parsing` entry on unwind so the next edit re-spawns rather than the
            // document wedging tree-less forever.
            let mut guard = parse_scheduler::ParseLoopGuard::new(
                std::sync::Arc::clone(&scheduler),
                uri.clone(),
            );

            // Stop the loop and clear the scheduler entry so a later edit re-spawns
            // (leaving the entry at `parsing: true` would wedge the URI — the same
            // failure the panic guard prevents). Used by the shutdown and
            // closed-document early exits, which bypass the normal `finish()`.
            let stop = |scheduler: &parse_scheduler::ParseScheduler,
                        guard: &mut parse_scheduler::ParseLoopGuard| {
                scheduler.clear(&uri);
                guard.disarm();
            };

            loop {
                // Stop promptly on server shutdown rather than running another
                // parse + injection/bridge round into a tearing-down bridge.
                if shutdown.is_cancelled() {
                    stop(&scheduler, &mut guard);
                    break;
                }

                // Start the pass: clear `dirty` and take the latest ticket, so an
                // edit folded into this text doesn't trigger a redundant reparse and
                // a later edit still loops. (`flatten`: outer = entry present, inner
                // = the ticket.)
                let ticket = scheduler.start_pass(&uri).flatten();
                parse.reparse_latest(&uri, ticket).await;

                // If the document was closed during the parse, skip ALL downstream
                // work: process_injections / republish on a gone URI could re-publish
                // diagnostics that `didClose` just cleared (resurrecting them in the
                // editor) and act on removed state.
                if documents.get(&uri).is_none() {
                    stop(&scheduler, &mut guard);
                    break;
                }

                // Re-check shutdown after the (awaited) parse and BEFORE the
                // downstream work: shutdown could have been requested while parsing,
                // and process_injections can spawn fresh eager bridge connections
                // that would escape `shutdown_all`'s snapshot.
                if shutdown.is_cancelled() {
                    stop(&scheduler, &mut guard);
                    break;
                }

                // Tree-dependent downstream, in the order did_change ran it inline:
                // injected-language processing + forwarding, then geometry re-merge
                // (which re-anchors region push slots against the now-current
                // injection geometry).
                injection.process_injections(&uri, true).await;
                // This gate is the geometry-deferral's retry backstop —
                // `republish`'s `needs_geometry` is the same shared predicate
                // (`has_live_region_slots`), so a deferred publish always has
                // this retry (diagnostic_publisher.rs).
                if diagnostics.has_region_slots(&uri) {
                    publisher.republish(&uri).await;
                }
                // Degraded-pull recovery: a pull answered while THIS parse was
                // pending couldn't fold the region slots and skipped
                // `mark_served` — and no later event re-nudges a pull-first
                // client until the next push. The per-host debt (recorded by
                // that pull, consumed here exactly once) keys the recovery, so
                // an ordinary edit cycle — or an unrelated host being dirty —
                // never turns a parse pass into a refresh trigger. FORCED past
                // the coverage gate (still single-flighted): the debt proves
                // the client holds a non-covering answer, which the
                // version-based gate cannot see — an edit-race degradation
                // leaves served == current, so a gated request would be
                // suppressed while the client displays the region-less set.
                if diagnostics.take_degraded_pull(&uri) {
                    publisher.request_pull_diagnostic_refresh(true);
                }

                // Schedule the debounced diagnostic HERE, after the reparse restored
                // the tree — NOT in the did_change handler, where the tree has just
                // been cleared. `prepare_diagnostic_snapshot` returns `None` without
                // a tree (`Document::snapshot()` requires one), and a `None` snapshot
                // makes the debounce a no-op — skipping the on-edit host re-sync
                // (#431) that keeps a push-only `_self` host server's diagnostics
                // following edits. Running it post-parse captures a snapshot with the
                // fresh tree; the debounce coalesces across loop iterations.
                diagnostic_scheduler.schedule_debounced_diagnostic(uri.clone());

                if !scheduler.finish(&uri) {
                    // Normal exit: finish() removed the entry.
                    guard.disarm();
                    break;
                }
            }
        });
    }
}

impl LanguageServer for Kakehashi {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.initialize_impl(params).await
    }

    async fn initialized(&self, params: InitializedParams) {
        self.initialized_impl(params).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.shutdown_impl().await
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.did_open_impl(params).await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.did_close_impl(params).await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.did_change_impl(params).await
    }

    async fn will_save(&self, params: WillSaveTextDocumentParams) {
        self.will_save_impl(params).await
    }

    async fn will_save_wait_until(
        &self,
        params: WillSaveTextDocumentParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.will_save_wait_until_impl(params).await
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.did_save_impl(params).await
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.did_change_configuration_impl(params).await
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        self.semantic_tokens_full_impl(params).await
    }

    async fn semantic_tokens_full_delta(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        self.semantic_tokens_full_delta_impl(params).await
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        self.semantic_tokens_range_impl(params).await
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        self.selection_range_impl(params).await
    }

    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> Result<Option<GotoDeclarationResponse>> {
        self.goto_declaration_impl(params).await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_definition_impl(params).await
    }

    async fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Result<Option<GotoTypeDefinitionResponse>> {
        self.goto_type_definition_impl(params).await
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        self.goto_implementation_impl(params).await
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.hover_impl(params).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.completion_impl(params).await
    }

    async fn completion_resolve(&self, params: CompletionItem) -> Result<CompletionItem> {
        self.completion_resolve_impl(params).await
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        self.signature_help_impl(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.references_impl(params).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        self.document_highlight_impl(params).await
    }

    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        self.document_link_impl(params).await
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        self.code_lens_impl(params).await
    }

    async fn code_lens_resolve(&self, params: CodeLens) -> Result<CodeLens> {
        self.code_lens_resolve_impl(params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.document_symbol_impl(params).await
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        self.folding_range_impl(params).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.rename_impl(params).await
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        self.code_action_impl(params).await
    }

    async fn code_action_resolve(&self, params: CodeAction) -> Result<CodeAction> {
        self.code_action_resolve_impl(params).await
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        self.execute_command_impl(params).await
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        self.formatting_impl(params).await
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.range_formatting_impl(params).await
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.on_type_formatting_impl(params).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.prepare_rename_impl(params).await
    }

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        self.inlay_hint_impl(params).await
    }

    async fn linked_editing_range(
        &self,
        params: LinkedEditingRangeParams,
    ) -> Result<Option<LinkedEditingRanges>> {
        self.linked_editing_range_impl(params).await
    }

    async fn document_color(&self, params: DocumentColorParams) -> Result<Vec<ColorInformation>> {
        self.document_color_impl(params).await
    }

    async fn color_presentation(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        self.color_presentation_impl(params).await
    }

    async fn moniker(&self, params: MonikerParams) -> Result<Option<Vec<Moniker>>> {
        self.moniker_impl(params).await
    }

    async fn diagnostic(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        self.diagnostic_impl(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::auto_install::InstallingLanguagesExt;

    // Note: Wildcard config resolution tests are in src/config.rs
    // Note: apply_content_changes_with_edits tests are in src/lsp/text_sync.rs

    #[tokio::test]
    async fn settings_reload_discards_available_document_parsers() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        service
            .inner()
            .parser_pool
            .lock()
            .unwrap()
            .release("stale".to_string(), tree_sitter::Parser::new());

        service
            .inner()
            .apply_raw_settings(
                RawWorkspaceSettings::default(),
                WorkspaceSettings::default(),
            )
            .await;

        assert!(
            service
                .inner()
                .parser_pool
                .lock()
                .unwrap()
                .acquire("stale")
                .is_none(),
            "settings reload must not reuse a parser configured before the reload"
        );
    }

    #[tokio::test]
    async fn parser_released_after_settings_reload_is_discarded() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let (parser, generation) = {
            let mut pool = service.inner().parser_pool.lock().unwrap();
            pool.release("stale".to_string(), tree_sitter::Parser::new());
            match pool.acquire_versioned("stale") {
                crate::language::parser_pool::ParserCheckout::Acquired(parser, generation, _) => {
                    (parser, generation)
                }
                _ => panic!("precondition: stale parser is available"),
            }
        };

        service
            .inner()
            .apply_raw_settings(
                RawWorkspaceSettings::default(),
                WorkspaceSettings::default(),
            )
            .await;
        let parser_is_current = service
            .inner()
            .parser_pool
            .lock()
            .unwrap()
            .release_versioned("stale".to_string(), parser, generation)
            .is_ok();

        assert!(
            !parser_is_current,
            "a parse from the previous pool generation must not publish its result"
        );

        assert!(
            service
                .inner()
                .parser_pool
                .lock()
                .unwrap()
                .acquire("stale")
                .is_none(),
            "a parser checked out before reload must not re-enter the new pool generation"
        );
    }

    #[tokio::test]
    async fn parse_retries_after_parser_generation_changes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        service
            .inner()
            .parser_pool
            .lock()
            .unwrap()
            .release("test".to_string(), tree_sitter::Parser::new());
        let parser_pool = std::sync::Arc::clone(&service.inner().parser_pool);
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let calls_for_parse = std::sync::Arc::clone(&calls);

        let parsed = service
            .inner()
            .parse_coordinator()
            .parse_with_pool_versioned(
                "test",
                &url::Url::parse("file:///reload-race.test").unwrap(),
                0,
                move |parser, _deadline, generation_retry| {
                    let call = calls_for_parse.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(generation_retry, call != 0);
                    if call == 0 {
                        let mut pool = parser_pool.lock().unwrap();
                        pool.begin_reload();
                        pool.finish_reload_at(1);
                        pool.release("test".to_string(), tree_sitter::Parser::new());
                    }
                    (parser, Some(call + 1))
                },
            )
            .await;

        assert_eq!(
            parsed,
            Some((2, 1)),
            "only the current-generation parse lands"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn parser_checkout_is_rejected_throughout_reload() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let mut pool = service.inner().parser_pool.lock().unwrap();
        pool.release("test".to_string(), tree_sitter::Parser::new());

        pool.begin_reload();
        pool.begin_reload();
        assert!(
            matches!(
                pool.acquire_versioned("test"),
                crate::language::parser_pool::ParserCheckout::Reloading
            ),
            "no parser checkout may start inside the reload window"
        );
        pool.finish_reload_at(1);
        assert!(
            matches!(
                pool.acquire_versioned("test"),
                crate::language::parser_pool::ParserCheckout::Reloading
            ),
            "one reload finishing must not close an overlapping reload window"
        );
        pool.finish_reload_at(1);
        pool.release("test".to_string(), tree_sitter::Parser::new());
        assert!(matches!(
            pool.acquire_versioned("test"),
            crate::language::parser_pool::ParserCheckout::Acquired(_, _, _)
        ));
    }

    #[test]
    fn parser_reload_guard_finishes_reload_during_unwind() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let parser_pool = &service.inner().parser_pool;
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _reload = ParserReloadGuard::begin(parser_pool, &service.inner().language);
            panic!("simulate reload failure");
        }));

        assert!(unwind.is_err());
        assert!(!matches!(
            parser_pool.lock().unwrap().acquire_versioned("test"),
            crate::language::parser_pool::ParserCheckout::Reloading
        ));
    }

    #[tokio::test]
    async fn parse_gives_up_when_reload_never_finishes() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        service.inner().parser_pool.lock().unwrap().begin_reload();

        let parsed = service
            .inner()
            .parse_coordinator()
            .parse_with_pool_versioned(
                "test",
                &url::Url::parse("file:///stuck-reload.test").unwrap(),
                0,
                |parser, _deadline, _generation_retry| (parser, Some(1)),
            )
            .await;

        service
            .inner()
            .parser_pool
            .lock()
            .unwrap()
            .finish_reload_at(1);
        assert_eq!(parsed, None);
    }

    #[test]
    fn test_check_injected_languages_identifies_missing_parsers() {
        // Test that check_injected_languages_auto_install correctly identifies
        // which injected languages need auto-installation (parsers not loaded).
        //
        // The function should:
        // 1. Get injected languages from resolve_injection_data()
        // 2. For each language, call ensure_language_loaded() to check if parser exists
        // 3. If parser is NOT loaded AND autoInstall is enabled, trigger maybe_auto_install_language()
        // 4. Skip languages that are already loaded or already being installed
        //
        // This test verifies the logic by checking what languages would be identified
        // as needing installation based on ensure_language_loaded() results.

        use crate::language::LanguageCoordinator;

        // Create a LanguageCoordinator to test ensure_language_loaded behavior
        let coordinator = LanguageCoordinator::new();

        // Test that ensure_language_loaded returns false for unknown languages
        // These are the languages that should trigger auto-install
        let unknown_langs = vec!["lua", "python", "rust"];
        for lang in &unknown_langs {
            let result = coordinator.ensure_language_loaded(lang);
            // Without any language configured, ensure_language_loaded should fail
            assert!(
                !result.success,
                "Expected ensure_language_loaded to fail for unconfigured language '{}'",
                lang
            );
        }

        // This verifies the core logic: if ensure_language_loaded().success is false,
        // the language should be a candidate for auto-installation.

        // The check_injected_languages_auto_install method uses this pattern:
        // 1. let injections = self.resolve_injection_data(uri);
        // 2. let languages = derive unique set from injections;
        // 3. for lang in languages {
        //        let load_result = self.language.ensure_language_loaded(&lang);
        //        if !load_result.success {
        //            self.maybe_auto_install_language(&lang, uri, text).await;
        //        }
        //    }

        // Verify that InstallingLanguages tracker would prevent duplicate installs
        let tracker = InstallingLanguages::new();
        assert!(tracker.try_start_install("lua"));
        assert!(!tracker.try_start_install("lua")); // Second attempt fails
        tracker.finish_install("lua");
        assert!(tracker.try_start_install("lua")); // After finish, can start again
    }

    // Note: Large integration tests for auto-install are in tests/test_auto_install_integration.rs
}
