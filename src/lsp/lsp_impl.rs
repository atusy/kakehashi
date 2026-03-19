pub(crate) mod bridge_context;
mod kakehashi;
mod lifecycle;
pub(crate) mod text_document;
mod workspace;

use std::collections::HashSet;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoImplementationParams,
    GotoImplementationResponse, GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};
#[cfg(feature = "experimental")]
use tower_lsp_server::ls_types::{
    ColorInformation, ColorPresentation, ColorPresentationParams, DocumentColorParams,
};
use tower_lsp_server::ls_types::{
    CompletionItem, CompletionParams, CompletionResponse, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentHighlight, DocumentHighlightParams, DocumentLink, DocumentLinkParams,
    DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverParams, InitializeParams, InitializeResult, InitializedParams, InlayHint,
    InlayHintParams, Location, Moniker, MonikerParams, PrepareRenameResponse, ReferenceParams,
    RenameParams, SelectionRange, SelectionRangeParams, SemanticTokensDeltaParams,
    SemanticTokensFullDeltaResult, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, SignatureHelp, SignatureHelpParams,
    TextDocumentPositionParams, Uri, WorkspaceEdit,
};
use tower_lsp_server::{Client, LanguageServer};
use tree_sitter::InputEdit;
use url::Url;

use crate::config::WorkspaceSettings;
use crate::document::DocumentStore;
use crate::language::LanguageEvent;
use crate::language::injection::{InjectionResolver, collect_all_injections};
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::bridge::coordinator::BridgeInjection;
use crate::lsp::client::ClientNotifier;
use crate::lsp::settings_manager::SettingsManager;
use tokio::sync::Mutex;

use super::auto_install::{AutoInstallManager, InstallEvent, InstallingLanguages};
use super::cache::CacheCoordinator;
use super::debounced_diagnostics::DebouncedDiagnosticsManager;
use super::synthetic_diagnostics::SyntheticDiagnosticsManager;

/// Timeout for spawn_blocking parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) fn uri_to_url(uri: &Uri) -> std::result::Result<Url, url::ParseError> {
    Url::parse(uri.as_str())
}

/// Convert url::Url to ls_types::Uri
///
/// This is the reverse conversion, needed when calling bridge protocol functions
/// that expect ls_types::Uri but we have url::Url from internal storage.
///
/// # Errors
/// Returns `LspError::Internal` if conversion fails. Both `url::Url` and
/// `fluent_uri::Uri` implement RFC 3986, so failure indicates an edge case
/// difference between the URI parsers (should be extremely rare in practice).
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
    parser_pool: Mutex<DocumentParserPool>,
    documents: DocumentStore,
    /// Unified cache coordinator for semantic tokens, injections, and request tracking
    cache: CacheCoordinator,
    /// Consolidated settings, capabilities, and workspace root management
    settings_manager: SettingsManager,
    /// Isolated coordinator for parser auto-installation
    auto_install: AutoInstallManager,
    /// Bridge coordinator for downstream LS pool and region ID tracking
    bridge: BridgeCoordinator,
    /// Manager for synthetic (background) diagnostic push tasks (ADR-0020 Phase 2).
    /// Wrapped in Arc for sharing with debounced diagnostics (Phase 3).
    synthetic_diagnostics: std::sync::Arc<SyntheticDiagnosticsManager>,
    /// Manager for debounced didChange diagnostic triggers (ADR-0020 Phase 3)
    debounced_diagnostics: DebouncedDiagnosticsManager,
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
            parser_pool: Mutex::new(parser_pool),
            documents: DocumentStore::new(),
            cache: CacheCoordinator::new(),
            settings_manager: SettingsManager::new(),
            auto_install,
            bridge,
            synthetic_diagnostics: std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            debounced_diagnostics: DebouncedDiagnosticsManager::new(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            home_dir: dirs::home_dir().map(|p| p.to_string_lossy().into_owned()),
        }
    }

    /// Create a `ClientNotifier` for centralized client communication.
    ///
    /// The notifier wraps the LSP client and references the stored capabilities,
    /// providing a clean API for logging, progress notifications, and semantic
    /// token refresh requests.
    pub(super) fn notifier(&self) -> ClientNotifier<'_> {
        ClientNotifier::new(
            self.client.clone(),
            self.settings_manager.client_capabilities_lock(),
        )
    }

    /// Dispatch install events to ClientNotifier.
    ///
    /// This method bridges AutoInstallManager (isolated) with ClientNotifier.
    /// AutoInstallManager returns events, Kakehashi dispatches them.
    async fn dispatch_install_events(&self, language: &str, events: &[InstallEvent]) {
        let notifier = self.notifier();
        for event in events {
            match event {
                InstallEvent::Log { level, message } => {
                    notifier.log(*level, message.clone()).await;
                }
                InstallEvent::ProgressBegin => {
                    notifier.progress_begin(language).await;
                }
                InstallEvent::ProgressEnd { success } => {
                    notifier.progress_end(language, *success).await;
                }
            }
        }
    }

    /// Notify user that parser is missing and needs manual installation.
    ///
    /// Called when a parser fails to load and auto-install is disabled
    /// (either explicitly or because searchPaths doesn't include the default data dir).
    async fn notify_parser_missing(&self, language: &str) {
        let settings = self.settings_manager.load_settings();

        // Check why auto-install is disabled
        let reason = if !settings.auto_install {
            "autoInstall is disabled".to_string()
        } else if !self
            .settings_manager
            .search_paths_include_default_data_dir(&settings.search_paths)
        {
            let default_dir = crate::install::default_data_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "searchPaths does not include the default data directory ({})",
                default_dir
            )
        } else {
            "unknown reason".to_string()
        };

        self.notifier()
            .log_warning(format!(
                "Parser for '{}' not found. Auto-install is disabled because {}. \
                 Please install the parser manually using: kakehashi language install {}",
                language, reason, language
            ))
            .await;
    }

    /// Send didClose for invalidated virtual documents.
    ///
    /// When region IDs are invalidated (e.g., due to edits touching their START),
    /// the corresponding virtual documents become orphaned in downstream LSs.
    /// This method cleans them up by:
    ///
    /// 1. Clearing injection token cache for invalidated ULIDs
    /// 2. Delegating to BridgeCoordinator for tracking cleanup and didClose
    ///
    /// Documents that were never opened (not in host_to_virtual) are automatically
    /// skipped - they don't need didClose since didOpen was never sent.
    async fn close_invalidated_virtual_docs(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) {
        if invalidated_ulids.is_empty() {
            return;
        }

        // Clear injection token cache for invalidated ULIDs via cache coordinator
        self.cache
            .remove_injection_tokens_for_ulids(host_uri, invalidated_ulids);

        // Delegate to bridge coordinator for tracking cleanup and didClose notifications
        self.bridge
            .close_invalidated_docs(host_uri, invalidated_ulids)
            .await;
    }

    /// Shared parsing orchestration: acquire parser from pool, run parse logic in
    /// `spawn_blocking` with timeout, release parser back to pool.
    ///
    /// The caller provides the actual parse logic via `parse_fn`, which receives a
    /// `tree_sitter::Parser` and must return it along with an optional result.
    /// On normal completion, this ensures the parser is returned to the pool.
    /// The parser is not returned if the blocking task times out (it keeps
    /// running) or if the task fails or is cancelled and yields a `JoinError`.
    ///
    /// Returns `None` if:
    /// - No parser is available for the language
    /// - The parse task panicked or was cancelled (JoinError; parser not returned)
    /// - The parse timed out after `PARSE_TIMEOUT` (parser not returned)
    /// - The closure returned `None`
    async fn parse_with_pool<T, F>(
        &self,
        language_name: &str,
        uri: &Url,
        text_len: usize,
        parse_fn: F,
    ) -> Option<T>
    where
        F: FnOnce(tree_sitter::Parser) -> (tree_sitter::Parser, Option<T>) + Send + 'static,
        T: Send + 'static,
    {
        // Checkout parser from pool (brief lock)
        let parser = {
            let mut pool = self.parser_pool.lock().await;
            pool.acquire(language_name)
        };

        let parser = parser?;

        // Parse in spawn_blocking with timeout to avoid blocking tokio worker thread
        // and prevent infinite hangs on pathological input
        let result = tokio::time::timeout(
            PARSE_TIMEOUT,
            tokio::task::spawn_blocking(move || parse_fn(parser)),
        )
        .await;

        // Handle timeout vs successful completion
        match result {
            Ok(Ok((parser, value))) => {
                // Return parser to pool (brief lock)
                let mut pool = self.parser_pool.lock().await;
                pool.release(language_name.to_string(), parser);
                value
            }
            Ok(Err(join_error)) => {
                if join_error.is_panic() {
                    log::error!(
                        "Parse task panicked for language '{}' on document {}: {}",
                        language_name,
                        uri,
                        join_error
                    );
                } else {
                    log::warn!(
                        "Parse task was cancelled for language '{}' on document {}: {}",
                        language_name,
                        uri,
                        join_error
                    );
                }
                // Parser is lost in the task (panicked or cancelled)
                None
            }
            Err(_timeout) => {
                log::warn!(
                    "Parse timeout after {:?} for language '{}' on document {} ({} bytes)",
                    PARSE_TIMEOUT,
                    language_name,
                    uri,
                    text_len
                );
                // Parser is lost in the still-running blocking task
                None
            }
        }
    }

    async fn parse_document(
        &self,
        uri: Url,
        text: String,
        language_id: Option<&str>,
        edits: Vec<InputEdit>,
    ) {
        let parse_generation = self.documents.mark_parse_started(&uri);
        let mut events = Vec::new();

        // ADR-0005: Detection fallback chain via LanguageCoordinator
        // Host document: token is None (no code fence identifier)
        let language_name = self
            .language
            .detect_language(uri.path(), &text, None, language_id);

        if let Some(language_name) = language_name {
            // Check if this parser has previously crashed
            if self.auto_install.is_parser_failed(&language_name) {
                log::warn!(
                    target: "kakehashi::crash_recovery",
                    "Skipping parsing for '{}' - parser previously crashed",
                    language_name
                );
                // Store document without parsing
                self.documents
                    .insert(uri.clone(), text, Some(language_name), None);
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
                self.handle_language_events(&events).await;
                return;
            }

            // Ensure language is loaded
            let load_result = self.language.ensure_language_loaded(&language_name);
            events.extend(load_result.events.clone());

            // Parse the document with crash detection via parse_with_pool
            // Get old tree for incremental parsing before entering the closure
            // For edits: get edited tree (after tree.edit() applied)
            // For full parse: get current tree as-is
            let (base_tree, pre_edit_tree) = if !edits.is_empty() {
                let edited = self.documents.get_edited_tree(&uri, &edits);
                // Clone for storage - we need to keep the edited tree for changed_ranges()
                let for_store = edited.clone();
                (edited, for_store)
            } else {
                let tree = self.documents.get(&uri).and_then(|doc| doc.tree().cloned());
                (tree, None)
            };

            let text_clone = text.clone();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();

            let parsed_tree = self
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    // Record that we're about to parse (for crash detection)
                    let _ = auto_install.begin_parsing(&language_name_clone);

                    let parse_result = parser.parse(&text_clone, base_tree.as_ref());

                    // Parsing succeeded without crash - clear the state for this language
                    let _ = auto_install.end_parsing(&language_name_clone);

                    // Return both parse result and edited tree for proper changed_ranges support
                    (parser, parse_result.map(|tree| (tree, pre_edit_tree)))
                })
                .await;

            // Store the parsed document
            if let Some((tree, pre_edit_tree)) = parsed_tree {
                // Populate InjectionMap with injection regions for targeted cache invalidation
                self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.region_id_tracker(),
                );

                if let Some(edited_tree) = pre_edit_tree {
                    // Use the new method that preserves the edited tree for changed_ranges()
                    self.documents.update_document_with_edited_tree(
                        uri.clone(),
                        text,
                        tree,
                        edited_tree,
                    );
                } else {
                    self.documents.insert(
                        uri.clone(),
                        text,
                        Some(language_name.clone()),
                        Some(tree),
                    );
                }

                self.documents
                    .mark_parse_finished(&uri, parse_generation, true);
                self.handle_language_events(&events).await;
                return;
            }
        }

        // Store unparsed document
        self.documents.insert(uri.clone(), text, None, None);
        self.documents
            .mark_parse_finished(&uri, parse_generation, false);
        self.handle_language_events(&events).await;
    }

    /// Get the language for a document using the full detection chain.
    ///
    /// Uses LanguageCoordinator::detect_language() which implements
    /// the fallback chain (ADR-0005): languageId → heuristics
    /// (token, path-derived token, first-line content) with alias
    /// resolution at each step.
    ///
    /// This ensures aliases are resolved (e.g., "rmd" → "markdown") even when
    /// the document is accessed before didOpen fully completes (race condition).
    fn get_language_for_document(&self, uri: &Url) -> Option<String> {
        let path = uri.path();

        // Get the document's language_id and content if available
        let (language_id, content) = self
            .documents
            .get(uri)
            .map(|doc| {
                (
                    doc.language_id().map(|s| s.to_string()),
                    doc.text().to_string(),
                )
            })
            .unwrap_or((None, String::new()));

        // ADR-0005: Unified detection chain with alias resolution at each step
        // Priority: languageId → heuristics (first-line, filename) → extension
        // Host document: token=None (no code fence identifier)
        self.language
            .detect_language(path, &content, None, language_id.as_deref())
    }

    /// Get all bridge server configs for a given injection language from settings.
    ///
    /// Returns **all** servers configured for the injection language,
    /// supporting multiple servers per language (e.g., pyright + ruff both handling Python).
    fn get_all_bridge_configs_for_language(
        &self,
        host_language: &str,
        injection_language: &str,
    ) -> Vec<crate::lsp::bridge::ResolvedServerConfig> {
        let settings = self.settings_manager.load_settings();
        self.bridge
            .get_all_configs_for_language(&settings, host_language, injection_language)
    }

    async fn apply_settings(&self, settings: WorkspaceSettings) {
        // Store settings via SettingsManager for auto_install check
        self.settings_manager.apply_settings(settings.clone());
        let summary = self.language.load_settings(settings);
        self.notifier().log_language_events(&summary.events).await;
    }

    async fn report_settings_events(&self, events: &[crate::lsp::SettingsEvent]) {
        self.notifier().log_settings_events(events).await;
    }

    async fn handle_language_events(&self, events: &[LanguageEvent]) {
        self.notifier().log_language_events(events).await;
    }

    /// Try to auto-install a language if not already being installed.
    ///
    /// Delegates to `AutoInstallManager::try_install()` and handles coordination:
    /// 1. Dispatches install events to ClientNotifier
    /// 2. Triggers `reload_language_after_install()` on success
    ///
    /// # Arguments
    /// * `language` - The language to install
    /// * `uri` - The document URI that triggered the install
    /// * `text` - The document text
    /// * `is_injection` - True if this is an injection language (not the document's main language)
    ///
    /// # Returns
    /// `true` if installation was triggered (caller should skip parse_document),
    /// `false` if installation was not triggered (caller should proceed with parse_document)
    async fn maybe_auto_install_language(
        &self,
        language: &str,
        uri: Url,
        text: String,
        is_injection: bool,
    ) -> bool {
        // Delegate to AutoInstallManager (isolated, returns events)
        let result = self.auto_install.try_install(language).await;

        // Dispatch events to ClientNotifier
        self.dispatch_install_events(language, &result.events).await;

        // Handle post-install coordination if successful
        if let Some(data_dir) = result.outcome.data_dir() {
            self.reload_language_after_install(language, data_dir, uri, text, is_injection)
                .await;
            return true; // Reload triggered, caller should skip parse
        }

        // Return based on outcome
        result.outcome.should_skip_parse()
    }

    /// Reload a language after installation and optionally re-parse the document.
    ///
    /// # Arguments
    /// * `language` - The language that was installed
    /// * `data_dir` - The data directory where parsers/queries are stored
    /// * `uri` - The document URI that triggered the install
    /// * `text` - The document text (used for re-parsing document languages)
    /// * `is_injection` - If true, this is an injection language and we should NOT
    ///   re-parse the document (which would use the wrong language).
    ///   Instead, we just refresh semantic tokens so the injection
    ///   gets highlighted on next request.
    async fn reload_language_after_install(
        &self,
        language: &str,
        data_dir: &std::path::Path,
        uri: Url,
        text: String,
        is_injection: bool,
    ) {
        // The installed files are at:
        // - Parser: {data_dir}/parser/{language}.{so|dylib}
        // - Queries: {data_dir}/queries/{language}/
        //
        // Both resolve_library_path and find_query_file expect the BASE directory
        // and append "parser/" or "queries/" internally. So we add data_dir itself,
        // not the subdirectories.

        // Update settings to include the new paths
        let current_settings = self.settings_manager.load_settings();
        let mut updated_settings = (*current_settings).clone();

        // Add data_dir as a base search path (not subdirectories)
        let data_dir_str = data_dir.to_string_lossy().to_string();
        if !updated_settings.search_paths.contains(&data_dir_str) {
            updated_settings.search_paths.push(data_dir_str);
        }

        // Apply the updated settings
        self.apply_settings(updated_settings).await;

        // Ensure the language is loaded and process its events.
        // apply_settings only stores configuration but doesn't load the parser.
        // The load result contains SemanticTokensRefresh event that will trigger
        // a non-blocking refresh request to the client via handle_language_events.
        let load_result = self.language.ensure_language_loaded(language);
        self.handle_language_events(&load_result.events).await;

        // For document languages, re-parse the document that triggered the install.
        // For injection languages, DON'T re-parse - the host document is already parsed
        // with the correct language. Re-parsing with the injection language would break
        // all highlighting. The SemanticTokensRefresh event above will notify the client.
        if !is_injection {
            // Get the host language for this document (not the installed language)
            let host_language = self.get_language_for_document(&uri);
            let lang_for_parse = host_language.as_deref();
            self.parse_document(uri.clone(), text, lang_for_parse, vec![])
                .await;
        }
    }

    /// Resolve all injection regions for a document.
    ///
    /// This method:
    /// 1. Gets the host language and its injection query
    /// 2. Extracts the parse tree (minimal lock duration on document store)
    /// 3. Collects all injection regions via `collect_all_injections`
    /// 4. Calculates stable region IDs via `RegionIdTracker` (ADR-0019)
    ///
    /// Returns an empty Vec if no injections are found (no language, no query,
    /// no tree, or no injection regions).
    ///
    /// # Lock Safety
    /// The document store lock is held only to clone the tree and text, then
    /// released before the tree traversal. No DashMap deadlock risk.
    fn resolve_injection_data(&self, uri: &Url) -> Vec<BridgeInjection> {
        // Get the host language for this document
        let Some(host_language) = self.get_language_for_document(uri) else {
            return Vec::new();
        };

        // Get the injection query for this language
        let Some(injection_query) = self.language.get_injection_query(&host_language) else {
            return Vec::new();
        };

        // Extract tree and text from document with minimal lock duration
        // IMPORTANT: Clone both to release document lock immediately
        let (tree, text) = match self.documents.get(uri) {
            Some(doc) => match doc.tree().cloned() {
                Some(tree) => (tree, doc.text().to_string()),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };

        // Collect all injection regions (no locks held)
        let Some(regions) =
            collect_all_injections(&tree.root_node(), &text, Some(injection_query.as_ref()))
        else {
            return Vec::new();
        };

        if regions.is_empty() {
            return Vec::new();
        }

        // Build BridgeInjection for each injection
        // ADR-0019: Use RegionIdTracker with position-based keys
        // No document lock held here - safe to access region_id_tracker
        regions
            .iter()
            .map(|region| {
                let region_id = InjectionResolver::calculate_region_id(
                    self.bridge.region_id_tracker(),
                    uri,
                    region,
                );
                let included_ranges = crate::language::injection::compute_included_ranges(
                    &region.content_node,
                    region.include_children,
                );
                let content = crate::language::injection::extract_clean_content(
                    &text,
                    region.content_node.byte_range(),
                    included_ranges.as_deref(),
                );
                BridgeInjection {
                    language: region.language.clone(),
                    region_id: region_id.to_string(),
                    content,
                }
            })
            .collect()
    }

    /// Process injected languages: resolve injection data, optionally forward didChange,
    /// auto-install missing parsers, and eagerly open virtual documents.
    ///
    /// This resolves injection data **once** and uses it for:
    /// 1. Forwarding didChange to already-opened virtual documents (when `forward_did_change` is true)
    /// 2. Auto-install check for missing parsers
    /// 3. Eager server spawn + didOpen for virtual documents
    ///
    /// Must be called AFTER parse_document so we have access to the AST.
    async fn process_injections(&self, uri: &Url, forward_did_change: bool) {
        let injections = self.resolve_injection_data(uri);
        if injections.is_empty() {
            // Cancel any previously spawned eager-open tasks for this URI.
            // Without this, stale tasks could send didOpen for removed injection regions.
            self.bridge.cancel_eager_open(uri);
            return;
        }

        if forward_did_change {
            // Forward didChange to opened virtual documents
            self.bridge
                .forward_didchange_to_opened_docs(uri, &injections)
                .await;
        }

        // Derive unique language set for auto-install
        let languages: HashSet<String> =
            injections.iter().map(|inj| inj.language.clone()).collect();

        // Check for missing parsers and trigger auto-install
        self.check_injected_languages_auto_install(uri, &languages)
            .await;

        // Eagerly spawn bridge servers and open virtual documents
        self.eager_spawn_bridge_servers(uri, injections);
    }

    /// Check injected languages and handle missing parsers.
    ///
    /// This function:
    /// 1. For each language, checks if it's already loaded
    /// 2. If not loaded and auto-install is enabled, triggers maybe_auto_install_language()
    /// 3. If not loaded and auto-install is disabled, notifies user
    ///
    /// The InstallingLanguages tracker in maybe_auto_install_language prevents
    /// duplicate install attempts.
    async fn check_injected_languages_auto_install(&self, uri: &Url, languages: &HashSet<String>) {
        let auto_install_enabled = self.settings_manager.is_auto_install_enabled();

        // Get document text for auto-install (needed by maybe_auto_install_language)
        let text = if auto_install_enabled {
            self.documents.get(uri).map(|doc| doc.text().to_string())
        } else {
            None
        };

        // Check each injected language and trigger auto-install if not loaded
        for lang in languages {
            // ADR-0005: Try direct identifier first, then syntect token normalization
            // This ensures "py" -> "python" before auto-install
            let resolved_lang = if self.language.has_parser_available(lang) {
                lang.clone()
            } else if let Some(normalized) = crate::language::heuristic::detect_from_token(lang) {
                normalized
            } else {
                lang.clone()
            };

            let load_result = self.language.ensure_language_loaded(&resolved_lang);
            if load_result.success {
                continue;
            }

            if !auto_install_enabled {
                self.notify_parser_missing(&resolved_lang).await;
                continue;
            }

            if let Some(ref text) = text {
                // Language not loaded - trigger auto-install with resolved name
                // maybe_auto_install_language uses InstallingLanguages to prevent duplicates
                // is_injection=true: Don't re-parse the document with injection language
                // Return value ignored - for injections we never skip parsing (host document already parsed)
                let _ = self
                    .maybe_auto_install_language(&resolved_lang, uri.clone(), text.clone(), true)
                    .await;
            }
        }
    }

    /// Eagerly spawn bridge servers and open virtual documents for detected injections.
    ///
    /// This warms up language servers (spawn + handshake + didOpen) in the background
    /// for injection regions found in the document. Downstream servers receive
    /// document content immediately, enabling faster diagnostic responses.
    fn eager_spawn_bridge_servers(&self, uri: &Url, injections: Vec<BridgeInjection>) {
        // Get the host language for this document
        let Some(host_language) = self.get_language_for_document(uri) else {
            return;
        };

        // Get current settings for server config lookup
        let settings = self.settings_manager.load_settings();

        // Spawn servers and open virtual documents for each detected injection
        self.bridge
            .eager_spawn_and_open_documents(&settings, &host_language, uri, injections);
    }

    /// Schedule a debounced diagnostic for a document (ADR-0020 Phase 3).
    ///
    /// This schedules a diagnostic collection to run after a debounce delay.
    /// If another change arrives before the delay expires, the previous timer
    /// is cancelled and a new one is started.
    ///
    /// The diagnostic snapshot is captured immediately (at schedule time) to
    /// ensure consistency with the document state that triggered the change.
    fn schedule_debounced_diagnostic(&self, uri: Url, lsp_uri: Uri) {
        // Capture snapshot data synchronously (same as spawn_synthetic_diagnostic_task)
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);

        // Schedule the debounced diagnostic
        self.debounced_diagnostics.schedule(
            uri,
            lsp_uri,
            self.client.clone(),
            snapshot_data,
            self.bridge.pool_arc(),
            std::sync::Arc::clone(&self.synthetic_diagnostics),
        );
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

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.document_symbol_impl(params).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.rename_impl(params).await
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

    #[cfg(feature = "experimental")]
    async fn document_color(&self, params: DocumentColorParams) -> Result<Vec<ColorInformation>> {
        self.document_color_impl(params).await
    }

    #[cfg(feature = "experimental")]
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
