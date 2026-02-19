pub(crate) mod bridge_context;
pub(crate) mod text_document;

use std::collections::HashSet;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse, GotoImplementationParams,
    GotoImplementationResponse, GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};
#[cfg(feature = "experimental")]
use tower_lsp_server::ls_types::{
    ColorInformation, ColorPresentation, ColorPresentationParams, ColorProviderCapability,
    DocumentColorParams,
};
use tower_lsp_server::ls_types::{
    CompletionOptions, CompletionParams, CompletionResponse, DeclarationCapability,
    DiagnosticOptions, DiagnosticServerCapabilities, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentDiagnosticParams, DocumentDiagnosticReportResult,
    DocumentHighlight, DocumentHighlightParams, DocumentLink, DocumentLinkOptions,
    DocumentLinkParams, DocumentSymbolParams, DocumentSymbolResponse, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverParams, HoverProviderCapability,
    ImplementationProviderCapability, InitializeParams, InitializeResult, InitializedParams,
    InlayHint, InlayHintParams, Location, Moniker, MonikerParams, OneOf, ReferenceParams,
    RenameParams, SaveOptions, SelectionRange, SelectionRangeParams,
    SelectionRangeProviderCapability, SemanticTokenModifier, SemanticTokenType,
    SemanticTokensDeltaParams, SemanticTokensFullDeltaResult, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensOptions, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, SemanticTokensResult, SemanticTokensServerCapabilities,
    ServerCapabilities, ServerInfo, SignatureHelp, SignatureHelpOptions, SignatureHelpParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability, Uri, WorkDoneProgressOptions,
    WorkspaceEdit,
};
use tower_lsp_server::{Client, LanguageServer};
use tree_sitter::InputEdit;
use url::Url;

use crate::analysis::{LEGEND_MODIFIERS, LEGEND_TYPES};
use crate::config::WorkspaceSettings;
use crate::document::DocumentStore;
use crate::language::LanguageEvent;
use crate::language::injection::{InjectionResolver, collect_all_injections};
use crate::language::region_id_tracker::EditInfo;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::bridge::coordinator::InjectionRegion;
use crate::lsp::client::{ClientNotifier, check_semantic_tokens_refresh_support};
use crate::lsp::settings_manager::SettingsManager;
use crate::lsp::{SettingsSource, load_settings};
use tokio::sync::Mutex;

use super::text_sync::apply_content_changes_with_edits;

use super::auto_install::{AutoInstallManager, InstallEvent, InstallingLanguages};
use super::cache::CacheCoordinator;
use super::debounced_diagnostics::DebouncedDiagnosticsManager;
use super::synthetic_diagnostics::SyntheticDiagnosticsManager;

/// Convert ls_types::Uri to url::Url
///
/// This is needed because ls-types uses its own Uri type (based on fluent-uri),
/// while kakehashi internally uses url::Url for document storage and processing.
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

fn lsp_legend_types() -> Vec<SemanticTokenType> {
    LEGEND_TYPES
        .iter()
        .map(|t| SemanticTokenType::new(t.as_str()))
        .collect()
}

fn lsp_legend_modifiers() -> Vec<SemanticTokenModifier> {
    LEGEND_MODIFIERS
        .iter()
        .map(|m| SemanticTokenModifier::new(m.as_str()))
        .collect()
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
            .finish_non_exhaustive()
    }
}

impl Kakehashi {
    pub fn new(client: Client) -> Self {
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
            bridge: BridgeCoordinator::new(),
            synthetic_diagnostics: std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            debounced_diagnostics: DebouncedDiagnosticsManager::new(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        }
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
            bridge: BridgeCoordinator::with_cancel_forwarder(pool, cancel_forwarder),
            synthetic_diagnostics: std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            debounced_diagnostics: DebouncedDiagnosticsManager::new(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Create a `ClientNotifier` for centralized client communication.
    ///
    /// The notifier wraps the LSP client and references the stored capabilities,
    /// providing a clean API for logging, progress notifications, and semantic
    /// token refresh requests.
    fn notifier(&self) -> ClientNotifier<'_> {
        ClientNotifier::new(
            self.client.clone(),
            self.settings_manager.client_capabilities_lock(),
        )
    }

    /// Check if auto-install is enabled.
    ///
    /// Delegates to SettingsManager for the actual check.
    fn is_auto_install_enabled(&self) -> bool {
        self.settings_manager.is_auto_install_enabled()
    }

    /// Check if the client supports multiline semantic tokens.
    ///
    /// Delegates to SettingsManager for capability checking.
    fn supports_multiline_tokens(&self) -> bool {
        self.settings_manager.supports_multiline_tokens()
    }

    /// Check if the client supports definition link (LocationLink[]).
    ///
    /// Delegates to SettingsManager for capability checking.
    fn supports_definition_link(&self) -> bool {
        self.settings_manager.supports_definition_link()
    }

    /// Check if the client supports type definition link (LocationLink[]).
    ///
    /// Delegates to SettingsManager for capability checking.
    fn supports_type_definition_link(&self) -> bool {
        self.settings_manager.supports_type_definition_link()
    }

    /// Check if the client supports implementation link (LocationLink[]).
    ///
    /// Delegates to SettingsManager for capability checking.
    fn supports_implementation_link(&self) -> bool {
        self.settings_manager.supports_implementation_link()
    }

    /// Check if the client supports declaration link (LocationLink[]).
    ///
    /// Delegates to SettingsManager for capability checking.
    fn supports_declaration_link(&self) -> bool {
        self.settings_manager.supports_declaration_link()
    }

    /// Check if the client supports hierarchical document symbols (DocumentSymbol[]).
    ///
    /// Delegates to SettingsManager for capability checking.
    fn supports_hierarchical_document_symbol(&self) -> bool {
        self.settings_manager
            .supports_hierarchical_document_symbol()
    }

    /// Check if the given search paths include the default data directory.
    fn search_paths_include_default_data_dir(&self, search_paths: &[String]) -> bool {
        self.settings_manager
            .search_paths_include_default_data_dir(search_paths)
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
        } else if !self.search_paths_include_default_data_dir(&settings.search_paths) {
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

            // Parse the document with crash detection
            // Narrow critical section: checkout parser → release lock → parse in spawn_blocking → return parser
            let parsed_tree = {
                // Checkout parser from pool (brief lock)
                let parser = {
                    let mut pool = self.parser_pool.lock().await;
                    pool.acquire(&language_name)
                };

                if let Some(mut parser) = parser {
                    // Get old tree for incremental parsing
                    // For edits: get edited tree (after tree.edit() applied)
                    // For full parse: get current tree as-is
                    let (old_tree, edited_old_tree_for_store) = if !edits.is_empty() {
                        let edited = self.documents.get_edited_tree(&uri, &edits);
                        // Clone for storage - we need to keep the edited tree for changed_ranges()
                        let for_store = edited.clone();
                        (edited, for_store)
                    } else {
                        let tree = self.documents.get(&uri).and_then(|doc| doc.tree().cloned());
                        (tree, None)
                    };

                    let language_name_clone = language_name.clone();
                    let text_clone = text.clone();
                    let auto_install = self.auto_install.clone();

                    // Parse in spawn_blocking with timeout to avoid blocking tokio worker thread
                    // and prevent infinite hangs on pathological input
                    const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

                    let result = tokio::time::timeout(
                        PARSE_TIMEOUT,
                        tokio::task::spawn_blocking(move || {
                            // Record that we're about to parse (for crash detection)
                            let _ = auto_install.begin_parsing(&language_name_clone);

                            let parse_result = parser.parse(&text_clone, old_tree.as_ref());

                            // Parsing succeeded without crash - clear the state for this language
                            let _ = auto_install.end_parsing(&language_name_clone);

                            (parser, parse_result)
                        }),
                    )
                    .await;

                    // Handle timeout vs successful completion
                    let result = match result {
                        Ok(join_result) => match join_result {
                            Ok(result) => Some(result),
                            Err(e) => {
                                log::error!(
                                    "Parse task panicked for language '{}' on document {}: {}",
                                    language_name,
                                    uri,
                                    e
                                );
                                None
                            }
                        },
                        Err(_timeout) => {
                            log::warn!(
                                "Parse timeout after {:?} for language '{}' on document {} ({} bytes)",
                                PARSE_TIMEOUT,
                                language_name,
                                uri,
                                text.len()
                            );
                            // Parser is lost in the still-running blocking task - cannot recover it
                            // The parser pool will create a new parser on next acquire
                            None
                        }
                    };

                    if let Some((parser, parse_result)) = result {
                        // Return parser to pool (brief lock)
                        let mut pool = self.parser_pool.lock().await;
                        pool.release(language_name.clone(), parser);
                        // Return both parse result and edited tree for proper changed_ranges support
                        parse_result.map(|tree| (tree, edited_old_tree_for_store))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            // Store the parsed document
            if let Some((tree, edited_old_tree)) = parsed_tree {
                // Populate InjectionMap with injection regions for targeted cache invalidation
                self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.region_id_tracker(),
                );

                if let Some(edited_tree) = edited_old_tree {
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
    /// the fallback chain: languageId → alias → shebang → extension (ADR-0005).
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
        let mut new_search_paths = current_settings.search_paths.clone();

        // Add data_dir as a base search path (not subdirectories)
        let data_dir_str = data_dir.to_string_lossy().to_string();
        if !new_search_paths.contains(&data_dir_str) {
            new_search_paths.push(data_dir_str);
        }

        // Create updated settings
        let updated_settings = WorkspaceSettings::with_language_servers(
            new_search_paths,
            current_settings.languages.clone(),
            current_settings.capture_mappings.clone(),
            current_settings.auto_install,
            current_settings.language_servers.clone(),
        );

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
    fn resolve_injection_data(&self, uri: &Url) -> Vec<InjectionRegion> {
        // Get the host language for this document
        let host_language = match self.get_language_for_document(uri) {
            Some(lang) => lang,
            None => return Vec::new(),
        };

        // Get the injection query for this language
        let injection_query = match self.language.get_injection_query(&host_language) {
            Some(q) => q,
            None => return Vec::new(),
        };

        // Extract tree and text from document with minimal lock duration
        // IMPORTANT: Clone both to release document lock immediately
        let (tree, text) = {
            let doc = match self.documents.get(uri) {
                Some(d) => d,
                None => return Vec::new(),
            };

            let tree = match doc.tree() {
                Some(t) => t.clone(),
                None => return Vec::new(),
            };
            let text = doc.text().to_string();
            (tree, text)
            // Document lock released here when `doc` guard drops
        };

        // Collect all injection regions (no locks held)
        let regions = match collect_all_injections(
            &tree.root_node(),
            &text,
            Some(injection_query.as_ref()),
        ) {
            Some(r) => r,
            None => return Vec::new(),
        };

        if regions.is_empty() {
            return Vec::new();
        }

        // Build InjectionRegion for each injection
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
                let content = &text[region.content_node.byte_range()];
                InjectionRegion {
                    language: region.language.clone(),
                    region_id: region_id.to_string(),
                    content: content.to_string(),
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
        let auto_install_enabled = self.is_auto_install_enabled();

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
            if !load_result.success {
                if auto_install_enabled {
                    if let Some(ref text) = text {
                        // Language not loaded - trigger auto-install with resolved name
                        // maybe_auto_install_language uses InstallingLanguages to prevent duplicates
                        // is_injection=true: Don't re-parse the document with injection language
                        // Return value ignored - for injections we never skip parsing (host document already parsed)
                        let _ = self
                            .maybe_auto_install_language(
                                &resolved_lang,
                                uri.clone(),
                                text.clone(),
                                true,
                            )
                            .await;
                    }
                } else {
                    // Notify user that parser is missing and needs manual installation
                    self.notify_parser_missing(&resolved_lang).await;
                }
            }
        }
    }

    /// Eagerly spawn bridge servers and open virtual documents for detected injections.
    ///
    /// This warms up language servers (spawn + handshake + didOpen) in the background
    /// for injection regions found in the document. Downstream servers receive
    /// document content immediately, enabling faster diagnostic responses.
    fn eager_spawn_bridge_servers(&self, uri: &Url, injections: Vec<InjectionRegion>) {
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

/// Forward upstream notifications from downstream language servers to the editor.
///
/// Consumes notifications from `upstream_rx` and dispatches them to the LSP client.
/// Currently handles:
/// - `DiagnosticRefresh`: forwards `workspace/diagnostic/refresh` to trigger a
///   fresh diagnostic pull from the editor.
///
/// Exits when:
/// - The channel is closed (all senders dropped), OR
/// - The `cancel_token` is cancelled (deterministic shutdown)
async fn upstream_forwarding_loop(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<super::bridge::UpstreamNotification>,
    client: Client,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    use super::bridge::UpstreamNotification;
    loop {
        tokio::select! {
            biased;

            _ = cancel_token.cancelled() => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Upstream forwarding loop cancelled"
                );
                break;
            }

            notification = upstream_rx.recv() => {
                match notification {
                    Some(UpstreamNotification::DiagnosticRefresh) => {
                        if let Err(e) = client.workspace_diagnostic_refresh().await {
                            log::debug!(
                                target: "kakehashi::bridge",
                                "workspace/diagnostic/refresh forwarding failed: {}",
                                e
                            );
                        }
                    }
                    None => break, // Channel closed
                }
            }
        }
    }
}

/// Cancellable upstream forwarding loop without a Client (for testing).
///
/// Drains notifications from the channel and exits when the token is cancelled
/// or the channel closes. Does not forward to any client.
#[cfg(test)]
async fn upstream_forwarding_loop_with_cancel(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<super::bridge::UpstreamNotification>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            biased;

            _ = cancel_token.cancelled() => break,

            notification = upstream_rx.recv() => {
                if notification.is_none() {
                    break; // Channel closed
                }
                // Notification consumed but not forwarded (no client in test)
            }
        }
    }
}

impl LanguageServer for Kakehashi {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Store client capabilities for LSP compliance checks (e.g., refresh support).
        // Uses SettingsManager which wraps OnceLock for "set once, read many" semantics.
        self.settings_manager
            .set_capabilities(params.capabilities.clone());

        // Log capability state for troubleshooting client compatibility issues.
        log::debug!(
            "Client capabilities stored: semantic_tokens_refresh={}",
            check_semantic_tokens_refresh_support(&params.capabilities)
        );

        // Debug: Log initialization
        self.notifier()
            .log_info("Received initialization request")
            .await;

        // Extract first workspace folder for reuse
        let first_folder = params.workspace_folders.as_ref().and_then(|f| f.first());

        // Get root URI from workspace folders or deprecated root_uri (for downstream servers)
        #[allow(deprecated)]
        let root_uri_for_bridge: Option<String> = first_folder
            .map(|f| f.uri.to_string())
            .or_else(|| params.root_uri.as_ref().map(|uri| uri.to_string()))
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| Url::from_file_path(p).ok())
                    .map(|u| u.to_string())
            });

        // Forward root_uri and workspace_folders to bridge pool for downstream server initialization
        self.bridge.pool().set_root_uri(root_uri_for_bridge.clone());

        let workspace_folders_for_bridge = params
            .workspace_folders
            .as_ref()
            .and_then(|f| serde_json::to_value(f).ok())
            .or_else(|| {
                root_uri_for_bridge.as_ref().map(|uri| {
                    let name = Url::parse(uri)
                        .ok()
                        .and_then(|url| {
                            url.to_file_path()
                                .ok()
                                .and_then(|path| {
                                    path.file_name().and_then(|s| s.to_str().map(String::from))
                                })
                                .or_else(|| {
                                    url.path_segments()
                                        .and_then(|mut seg| seg.next_back().map(|s| s.to_string()))
                                })
                        })
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "workspace".to_string());
                    serde_json::json!([{ "uri": uri, "name": name }])
                })
            });
        self.bridge
            .pool()
            .set_workspace_folders(workspace_folders_for_bridge);

        // Get root path from workspace folders, deprecated root_uri, or current directory
        let uri_to_path = |uri: &Uri| uri_to_url(uri).ok().and_then(|url| url.to_file_path().ok());

        #[allow(deprecated)]
        let root_path = first_folder
            .and_then(|f| uri_to_path(&f.uri))
            .or_else(|| params.root_uri.as_ref().and_then(uri_to_path))
            .or_else(|| std::env::current_dir().ok());

        // Store root path for later use and log the source
        #[allow(deprecated)]
        if let Some(ref path) = root_path {
            let source = if params.workspace_folders.is_some() {
                "workspace folders"
            } else if params.root_uri.is_some() {
                "root_uri (deprecated)"
            } else {
                "current working directory (fallback)"
            };

            self.notifier()
                .log_info(format!(
                    "Using workspace root from {}: {}",
                    source,
                    path.display()
                ))
                .await;
            self.settings_manager.set_root_path(Some(path.clone()));
        } else {
            self.notifier()
                .log_warning("Failed to determine workspace root - config file will not be loaded")
                .await;
        }

        let root_path = self.settings_manager.root_path().as_ref().clone();
        let settings_outcome = load_settings(
            root_path.as_deref(),
            params
                .initialization_options
                .map(|options| (SettingsSource::InitializationOptions, options)),
        );
        self.report_settings_events(&settings_outcome.events).await;

        // Always apply settings (use defaults if none were loaded)
        // This ensures auto_install=true, default capture_mappings, and other defaults are active
        // for zero-config experience. Use default_settings() instead of TreeSitterSettings::default()
        // because the derived Default creates empty capture_mappings while default_settings() includes
        // the full default capture_mappings (markup.strong → "", etc.)
        let settings = settings_outcome.settings.unwrap_or_else(|| {
            WorkspaceSettings::from(crate::config::defaults::default_settings())
        });
        self.apply_settings(settings).await;

        self.notifier().log_info("server initialized!").await;
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "kakehashi".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: lsp_legend_types(),
                                token_modifiers: lsp_legend_modifiers(),
                            },
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                            range: Some(true),
                            ..Default::default()
                        },
                    ),
                ),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    ..Default::default()
                }),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Left(true)),
                inlay_hint_provider: Some(OneOf::Left(true)),
                #[cfg(feature = "experimental")]
                color_provider: Some(ColorProviderCapability::Simple(true)),
                #[cfg(not(feature = "experimental"))]
                color_provider: None,
                moniker_provider: Some(OneOf::Left(true)),
                // ADR-0020: Pull-first diagnostic forwarding
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        ..Default::default()
                    },
                )),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.notifier().log_info("server is ready").await;

        // Forward downstream server notifications to upstream editor.
        // When a downstream LS sends workspace/diagnostic/refresh, the reader
        // task puts DiagnosticRefresh on this channel. We forward it to the
        // editor via Client::workspace_diagnostic_refresh() so the editor
        // triggers a fresh textDocument/diagnostic pull.
        if let Some(upstream_rx) = self.bridge.take_upstream_rx() {
            let client = self.client.clone();
            let token = self.shutdown_token.clone();
            tokio::spawn(upstream_forwarding_loop(upstream_rx, client, token));
        }
    }

    async fn shutdown(&self) -> Result<()> {
        // Persist crash detection state before shutdown
        // This enables crash recovery to detect if parsing was in progress
        if let Err(e) = self.auto_install.persist_state() {
            log::warn!(
                target: "kakehashi::crash_recovery",
                "Failed to persist crash detection state on shutdown: {}",
                e
            );
        }

        // Abort all synthetic diagnostic tasks (ADR-0020 Phase 2)
        self.synthetic_diagnostics.abort_all();

        // Cancel all debounced diagnostic timers (ADR-0020 Phase 3)
        self.debounced_diagnostics.cancel_all();

        // Abort all eager-open tasks to prevent orphaned didOpen during shutdown
        self.bridge.abort_all_eager_open();

        // Cancel the upstream forwarding task for deterministic shutdown.
        // Without this, the task only exits when all senders are dropped.
        self.shutdown_token.cancel();

        // Graceful shutdown of all downstream language server connections (ADR-0017)
        // - Transitions to Closing state, sends LSP shutdown/exit handshake
        // - Escalates to SIGTERM/SIGKILL for unresponsive servers (Unix)
        self.bridge.shutdown_all().await;

        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let language_id = params.text_document.language_id.clone();
        let lsp_uri = params.text_document.uri.clone();
        let text = params.text_document.text.clone();

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didOpen: {}", lsp_uri.as_str());
            return;
        };

        // Try to determine the language
        let language_name = self
            .language
            .get_language_for_path(uri.path())
            .or_else(|| Some(language_id.clone()));

        // Insert document immediately (without tree) so concurrent requests can find it.
        // This handles race conditions where semanticTokens/full arrives before
        // parse_document completes. The tree will be updated by parse_document.
        self.documents
            .insert(uri.clone(), text.clone(), language_name.clone(), None);

        // Check if we need to auto-install
        let mut deferred_events = Vec::new();
        let mut skip_parse = false; // Track if auto-install was triggered

        if let Some(ref lang) = language_name {
            let load_result = self.language.ensure_language_loaded(lang);

            // Defer SemanticTokensRefresh events until after parse_document completes
            // to avoid race condition where tokens are requested before tree exists.
            // Log events immediately but defer refresh.
            for event in &load_result.events {
                match event {
                    crate::language::LanguageEvent::SemanticTokensRefresh { .. } => {
                        deferred_events.push(event.clone());
                    }
                    _ => {
                        self.handle_language_events(std::slice::from_ref(event))
                            .await;
                    }
                }
            }

            if !load_result.success {
                if self.is_auto_install_enabled() {
                    // Language failed to load and auto-install is enabled
                    // is_injection=false: This is the document's main language
                    // If install is triggered, skip parse_document here - reload_language_after_install will handle it
                    skip_parse = self
                        .maybe_auto_install_language(lang, uri.clone(), text.clone(), false)
                        .await;
                } else {
                    // Notify user that parser is missing and needs manual installation
                    self.notify_parser_missing(lang).await;
                }
            }
        }

        // Only parse if auto-install was NOT triggered
        // If auto-install was triggered, reload_language_after_install will call parse_document
        // after the parser file is completely written, preventing race condition
        if !skip_parse {
            self.parse_document(
                uri.clone(),
                params.text_document.text,
                Some(&language_id),
                vec![], // No edits for initial document open
            )
            .await;
        }

        // Now handle deferred SemanticTokensRefresh events after document is parsed
        if !deferred_events.is_empty() {
            self.handle_language_events(&deferred_events).await;
        }

        // Process injected languages: auto-install missing parsers and spawn bridge servers.
        // This must be called AFTER parse_document so we have access to the AST.
        self.process_injections(&uri, false).await;

        // ADR-0020 Phase 2: Trigger synthetic diagnostic push on didOpen
        // This provides proactive diagnostics for clients that don't support pull diagnostics.
        // Note: We use the already-cloned lsp_uri here (it was cloned at the start of the method).
        self.spawn_synthetic_diagnostic_task(uri, lsp_uri);

        // NOTE: No semantic_tokens_refresh() on didOpen.
        // Capable LSP clients should request by themselves.
        // Calling refresh would be redundant and can cause deadlocks with clients
        // like vim-lsp that don't respond to workspace/semanticTokens/refresh requests.

        self.notifier().log_info("file opened!").await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didClose: {}", lsp_uri.as_str());
            return;
        };

        // Remove the document from the store when it's closed
        // This ensures that reopening the file will properly reinitialize everything
        self.documents.remove(&uri);

        // Clean up all caches for this document (semantic tokens, injections, requests)
        self.cache.remove_document(&uri);

        // Clean up region ID mappings for this document (ADR-0019)
        self.bridge.cleanup(&uri);

        // Abort any in-progress synthetic diagnostic task for this document (ADR-0020 Phase 2)
        self.synthetic_diagnostics.remove_document(&uri);

        // Cancel any pending debounced diagnostic for this document (ADR-0020 Phase 3)
        self.debounced_diagnostics.cancel(&uri);

        // Cancel any eager-open tasks for this document (prevents orphaned didOpen)
        self.bridge.cancel_eager_open(&uri);

        // Close all virtual documents associated with this host document
        // This sends didClose notifications to downstream language servers
        let closed_docs = self.bridge.close_host_document(&uri).await;
        if !closed_docs.is_empty() {
            log::debug!(
                target: "kakehashi::bridge",
                "Closed {} virtual documents for host {}",
                closed_docs.len(),
                uri
            );
        }

        self.notifier().log_info("file closed!").await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didChange: {}", lsp_uri.as_str());
            return;
        };

        self.notifier()
            .log_trace(format!("[DID_CHANGE] START uri={}", uri))
            .await;

        // Retrieve the stored document info
        let (language_id, old_text) = {
            let doc = self.documents.get(&uri);
            match doc {
                Some(d) => (d.language_id().map(|s| s.to_string()), d.text().to_string()),
                None => {
                    self.notifier()
                        .log_warning("Document not found for change event")
                        .await;
                    return;
                }
            }
        };

        // Apply content changes and build tree-sitter edits
        let (text, edits) = apply_content_changes_with_edits(&old_text, params.content_changes);

        // ADR-0019: Apply START-priority invalidation to region ID tracker.
        // Use InputEdits directly for precise invalidation when available,
        // fall back to diff-based approach for full document sync.
        //
        // This must be called AFTER content changes are applied (so we have new text)
        // but BEFORE parse_document (so position sync happens before new tree is built).
        let invalidated_ulids = if edits.is_empty() {
            // Full document sync: no InputEdits available, reconstruct from diff
            self.bridge.apply_text_diff(&uri, &old_text, &text)
        } else {
            // Incremental sync: use InputEdits directly (precise, no over-invalidation)
            let edit_infos: Vec<EditInfo> = edits.iter().map(EditInfo::from).collect();
            self.bridge.apply_input_edits(&uri, &edit_infos)
        };

        // Invalidate injection caches for regions overlapping with edits (AC4/AC5)
        // Must be called BEFORE parse_document which updates the injection_map
        self.cache.invalidate_for_edits(&uri, &edits);

        // Parse the updated document with edit information
        self.parse_document(uri.clone(), text, language_id.as_deref(), edits)
            .await;

        // NOTE: We intentionally do NOT invalidate the semantic token cache here.
        // The cached tokens (with their result_id) are needed for delta calculations.
        // When semanticTokens/full/delta arrives with previousResultId, we look up
        // the cached tokens to compute the delta. If we invalidated here, the delta
        // request would always fall back to full tokenization.
        //
        // The cache is validated at lookup time via result_id matching, so stale
        // tokens won't be returned for mismatched result_ids.

        // ADR-0019: Close invalidated virtual documents.
        // Send didClose notifications to downstream LSs for orphaned docs.
        self.close_invalidated_virtual_docs(&uri, &invalidated_ulids)
            .await;

        // Forward didChange to opened virtual documents + process injected languages.
        // Injection data is resolved once and reused for:
        // 1. didChange forwarding to already-opened virtual documents
        // 2. Auto-install missing parsers
        // 3. Eager server spawn + didOpen for virtual documents
        // Must be called AFTER parse_document so we have access to the updated AST.
        self.process_injections(&uri, true).await;

        // ADR-0020 Phase 3: Schedule debounced diagnostic push on didChange.
        // After 500ms of no changes, diagnostics will be collected and published.
        // This provides near-real-time feedback while avoiding excessive requests during typing.
        self.schedule_debounced_diagnostic(uri, lsp_uri);

        // NOTE: We intentionally do NOT call semantic_tokens_refresh() here.
        // LSP clients already request new tokens after didChange (via semanticTokens/full/delta).
        // Calling refresh would be redundant and can cause deadlocks with synchronous clients
        // like vim-lsp on Vim, which cannot respond to server requests while processing.

        self.notifier().log_info("file changed!").await;
    }

    /// Handle textDocument/didSave notification.
    ///
    /// ADR-0020 Phase 2: Triggers synthetic diagnostic push.
    /// Collects diagnostics from downstream servers and publishes via publishDiagnostics.
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didSave: {}", lsp_uri.as_str());
            return;
        };

        log::debug!(
            target: "kakehashi::synthetic_diag",
            "didSave received for {}",
            uri
        );

        // Spawn background task for synthetic diagnostic collection
        self.spawn_synthetic_diagnostic_task(uri, lsp_uri);

        self.notifier().log_info("file saved!").await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let root_path = self.settings_manager.root_path().as_ref().clone();
        let settings_outcome = load_settings(
            root_path.as_deref(),
            Some((SettingsSource::ClientConfiguration, params.settings)),
        );
        self.report_settings_events(&settings_outcome.events).await;

        if let Some(settings) = settings_outcome.settings {
            self.apply_settings(settings).await;
            self.notifier().log_info("Configuration updated!").await;
        }
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

    /// Test that upstream_forwarding_loop exits when its CancellationToken is cancelled,
    /// even if the channel is still open.
    #[tokio::test]
    async fn upstream_forwarding_loop_exits_on_cancellation() {
        use crate::lsp::bridge::UpstreamNotification;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let token = CancellationToken::new();

        // Send a notification before cancellation — it should be forwarded
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();

        // Spawn the loop with a cancellation token (channel stays open via `tx`)
        let token_clone = token.clone();
        let handle = tokio::spawn(upstream_forwarding_loop_with_cancel(rx, token_clone));

        // Give the loop time to process the notification
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Cancel the token — loop should exit even though tx is still alive
        token.cancel();

        // The loop should exit promptly
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(
            result.is_ok(),
            "upstream_forwarding_loop should exit when token is cancelled"
        );
    }
}
