use super::config_store::ConfigStore;
use super::events::{LanguageEvent, LanguageLoadResult, LanguageLoadSummary, LanguageLogLevel};
use super::filetypes::FiletypeResolver;
use super::loader::ParserLoader;
use super::parser_pool::{DocumentParserPool, ParserFactory};
use super::query_loader::{ParseFailure, QueryLoader, format_search_paths};
use super::query_store::QueryStore;
use super::registry::LanguageRegistry;
use crate::config::settings::{LanguageSettings, QueryKind, infer_query_kind};
use crate::config::{CaptureMappings, WorkspaceSettings};
use crate::error::LockResultExt;
use log::debug;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tree_sitter::Language;

/// Maximum length (in characters) for pattern previews in log messages.
const MAX_PREVIEW_LEN: usize = 60;

/// Context for loading a query, including metadata for log messages.
struct QueryLoadContext<'a> {
    language_id: &'a str,
    query_type: &'a str,
    /// Describes where the query was loaded from (e.g., "Dynamically loaded").
    /// Only used by `load_query_with_inheritance`, not by `load_query_from_paths`.
    context: Option<&'a str>,
}

/// Coordinates language runtime components (registry, queries, configs).
pub struct LanguageCoordinator {
    query_store: QueryStore,
    config_store: ConfigStore,
    filetype_resolver: FiletypeResolver,
    language_registry: LanguageRegistry,
    parser_loader: RwLock<ParserLoader>,
    /// Maps alias languageId → canonical language name.
    /// Built from `languages.<name>.aliases` in configuration.
    /// Example: "rmd" → "markdown", "qmd" → "markdown"
    alias_map: RwLock<HashMap<String, String>>,
}

impl Default for LanguageCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageCoordinator {
    pub fn new() -> Self {
        Self {
            query_store: QueryStore::new(),
            config_store: ConfigStore::new(),
            filetype_resolver: FiletypeResolver::new(),
            language_registry: LanguageRegistry::new(),
            parser_loader: RwLock::new(ParserLoader::new()),
            alias_map: RwLock::new(HashMap::new()),
        }
    }

    /// Ensure a language parser is loaded, attempting dynamic load if needed.
    ///
    /// Visibility: Public - called by LSP layer (semantic_tokens, selection_range)
    /// and analysis modules to ensure parsers are available before use.
    pub fn ensure_language_loaded(&self, language_id: &str) -> LanguageLoadResult {
        if self.language_registry.contains(language_id) {
            LanguageLoadResult::success_with(Vec::new())
        } else {
            self.try_load_language_by_id(language_id)
        }
    }

    /// Initialize from workspace-level settings and return coordination events.
    ///
    /// Visibility: Public - called by LSP layer during initialization and
    /// settings updates to configure language support.
    pub fn load_settings(&self, settings: WorkspaceSettings) -> LanguageLoadSummary {
        self.config_store.update_from_settings(&settings);

        // Build alias map from language configs
        self.build_alias_map(&settings.languages);

        let mut summary = LanguageLoadSummary::default();
        let search_paths = self.config_store.get_search_paths();
        for (lang_name, config) in &settings.languages {
            let result = self.load_single_language(lang_name, config, &search_paths);
            summary.record(lang_name, result);
        }
        summary
    }

    /// Build the alias → canonical language map from configuration.
    ///
    /// For each language with `aliases = ["a", "b"]`, maps "a" → language_name
    /// and "b" → language_name. This enables editors sending languageId "rmd"
    /// to use the "markdown" parser configuration.
    fn build_alias_map(&self, languages: &HashMap<String, LanguageSettings>) {
        let mut alias_map = self
            .alias_map
            .write()
            .recover_poison("LanguageCoordinator::build_alias_map");

        alias_map.clear();

        for (lang_name, config) in languages {
            if let Some(aliases) = &config.aliases {
                for alias in aliases {
                    if let Some(previous) = alias_map.insert(alias.clone(), lang_name.clone()) {
                        log::warn!(
                            target: "kakehashi::language_detection",
                            "Alias '{}' collision: was '{}', now '{}' (last-wins)",
                            alias,
                            previous,
                            lang_name
                        );
                    } else {
                        log::debug!(
                            target: "kakehashi::language_detection",
                            "Registered alias '{}' → '{}'",
                            alias,
                            lang_name
                        );
                    }
                }
            }
        }
    }

    /// Resolve a languageId to its canonical language name using the alias map.
    ///
    /// Returns the canonical name if the input is an alias, otherwise returns None.
    /// Example: "rmd" → Some("markdown") if markdown has aliases = ["rmd"]
    fn resolve_alias(&self, language_id: &str) -> Option<String> {
        let alias_map = self
            .alias_map
            .read()
            .recover_poison("LanguageCoordinator::resolve_alias");

        alias_map.get(language_id).cloned()
    }

    /// ADR-0005: Try candidate directly, then with config-based alias.
    ///
    /// Returns the language name if a parser is available (either directly or via alias).
    /// This is applied as a sub-step after each detection method.
    fn try_with_alias_fallback(&self, candidate: &str) -> Option<String> {
        // Direct match
        if self.has_parser_available(candidate) {
            return Some(candidate.to_string());
        }
        // Config-based alias
        if let Some(canonical) = self.resolve_alias(candidate)
            && self.has_parser_available(&canonical)
        {
            return Some(canonical);
        }
        None
    }

    /// Try to dynamically load a language by ID from configured search paths
    ///
    /// Visibility: Internal only - called by ensure_language_loaded.
    /// Not exposed as public API to keep interface minimal (YAGNI).
    fn try_load_language_by_id(&self, language_id: &str) -> LanguageLoadResult {
        if self.language_registry.contains(language_id) {
            return LanguageLoadResult::success_with(Vec::new());
        }

        let search_paths = self.config_store.get_search_paths();
        if search_paths.is_empty() {
            return LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Warning,
                format!("No search paths configured, cannot load language '{language_id}'"),
            ));
        }

        let library_path = QueryLoader::resolve_library_path(None, language_id, &search_paths);
        let Some(lib_path) = library_path else {
            return LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Warning,
                format!(
                    "Could not find parser for language '{language_id}' in search paths: {}",
                    format_search_paths(&search_paths),
                ),
            ));
        };

        let language = {
            let result = self
                .parser_loader
                .write()
                .recover_poison("LanguageCoordinator::try_load_language_by_id")
                .load_language(&lib_path, language_id);
            match result {
                Ok(lang) => lang,
                Err(err) => {
                    return LanguageLoadResult::failure_with(LanguageEvent::log(
                        LanguageLogLevel::Error,
                        format!(
                            "Failed to load language {language_id} from {}: {err}",
                            lib_path.display()
                        ),
                    ));
                }
            }
        };

        self.language_registry
            .register(language_id.to_string(), language.clone());

        let mut events = Vec::new();

        // Use fault-tolerant loading for all query types
        // This handles languages like TypeScript that inherit from ecma,
        // and gracefully skips invalid patterns while preserving valid ones
        self.load_query(
            &language,
            &search_paths,
            QueryLoadContext {
                language_id,
                query_type: "highlights",
                context: Some("Dynamically loaded"),
            },
            &mut events,
            |store, query| store.insert_highlight_query(language_id.to_string(), query),
        );
        self.load_query(
            &language,
            &search_paths,
            QueryLoadContext {
                language_id,
                query_type: "locals",
                context: Some("Dynamically loaded"),
            },
            &mut events,
            |store, query| store.insert_locals_query(language_id.to_string(), query),
        );
        self.load_query(
            &language,
            &search_paths,
            QueryLoadContext {
                language_id,
                query_type: "injections",
                context: Some("Dynamically loaded"),
            },
            &mut events,
            |store, query| store.insert_injection_query(language_id.to_string(), query),
        );

        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!(
                "Dynamically loaded language {language_id} from {}",
                lib_path.display()
            ),
        ));
        if self.has_queries(language_id) {
            events.push(LanguageEvent::semantic_tokens_refresh(
                language_id.to_string(),
            ));
        }

        LanguageLoadResult::success_with(events)
    }

    /// Load a query file with inheritance resolution.
    fn load_query(
        &self,
        language: &Language,
        paths: &[PathBuf],
        ctx: QueryLoadContext<'_>,
        events: &mut Vec<LanguageEvent>,
        insert_fn: impl FnOnce(&QueryStore, Arc<tree_sitter::Query>),
    ) {
        let filename = format!("{}.scm", ctx.query_type);
        let result = match QueryLoader::load_query_with_inheritance(
            language,
            paths,
            ctx.language_id,
            &filename,
        ) {
            Ok(r) => r,
            Err(_) => {
                debug!(
                    "Query file {}/{} not found in search paths (this is normal if not provided)",
                    ctx.language_id, filename
                );
                return;
            }
        };

        let query_label = format!("{}/{}", ctx.language_id, filename);
        let context = ctx.context.unwrap_or(ctx.query_type);
        let success_prefix = format!("{} {} for {}", context, ctx.query_type, ctx.language_id);
        self.process_query_result(result, &query_label, &success_prefix, events, insert_fn);
    }

    /// Load a query from explicit paths (unified queries configuration).
    fn load_query_from_paths<P: AsRef<Path>>(
        &self,
        language: &Language,
        paths: &[P],
        ctx: QueryLoadContext<'_>,
        events: &mut Vec<LanguageEvent>,
        insert_fn: impl FnOnce(&QueryStore, Arc<tree_sitter::Query>),
    ) {
        let result = match QueryLoader::load_query_from_paths(language, paths) {
            Ok(r) => r,
            Err(err) => {
                events.push(LanguageEvent::log(
                    LanguageLogLevel::Error,
                    format!(
                        "Failed to load {} query for {}: {err}",
                        ctx.query_type, ctx.language_id
                    ),
                ));
                return;
            }
        };

        let query_label = format!("{} {} query", ctx.language_id, ctx.query_type);
        let success_prefix = format!("{} query loaded for {}", ctx.query_type, ctx.language_id);
        self.process_query_result(result, &query_label, &success_prefix, events, insert_fn);
    }

    /// Process a ParseResult: log skipped patterns, insert query, log outcome.
    fn process_query_result(
        &self,
        result: super::query_loader::ParseResult,
        query_label: &str,
        success_prefix: &str,
        events: &mut Vec<LanguageEvent>,
        insert_fn: impl FnOnce(&QueryStore, Arc<tree_sitter::Query>),
    ) {
        // Log warnings for skipped patterns
        for skipped in &result.skipped {
            let preview = truncate_preview(&skipped.text, MAX_PREVIEW_LEN);
            // When inheritance is used, line numbers refer to the combined query,
            // not the original source file
            let line_note = if result.used_inheritance {
                " (in combined query)"
            } else {
                ""
            };
            events.push(LanguageEvent::log(
                LanguageLogLevel::Warning,
                format!(
                    "Skipped invalid pattern in {query_label} (lines {}-{}{}): {} | pattern: {}",
                    skipped.start_line, skipped.end_line, line_note, skipped.error, preview
                ),
            ));
        }

        match result.query {
            Some(query) => {
                insert_fn(&self.query_store, Arc::new(query));
                let skipped_count = result.skipped.len();
                let msg = if skipped_count > 0 {
                    format!("{success_prefix} ({skipped_count} pattern(s) skipped)")
                } else {
                    success_prefix.to_string()
                };
                events.push(LanguageEvent::log(LanguageLogLevel::Info, msg));
            }
            None => {
                if let Some(reason) = &result.failure_reason {
                    let msg = match reason {
                        ParseFailure::PatternSplitFailed(err) => {
                            format!(
                                "Failed to parse {query_label}: could not split patterns ({err})"
                            )
                        }
                        ParseFailure::AllPatternsInvalid => {
                            format!(
                                "Failed to load {query_label}: all {} pattern(s) were invalid",
                                result.skipped.len()
                            )
                        }
                        ParseFailure::CombinationFailed(err) => {
                            format!("Failed to combine patterns in {query_label}: {err}")
                        }
                    };
                    events.push(LanguageEvent::log(LanguageLogLevel::Warning, msg));
                }
            }
        }
    }

    /// Get language for a document path.
    ///
    /// Visibility: Public - called by LSP layer (auto_install, lsp_impl)
    /// for document language detection.
    pub fn get_language_for_path(&self, path: &str) -> Option<String> {
        self.filetype_resolver.get_language_for_path(path)
    }

    /// Check if a parser is available for a given language name.
    ///
    /// Used by the detection fallback chain (ADR-0005) to determine whether
    /// to accept a detection result or continue to the next method.
    ///
    /// Visibility: Public - called by LSP layer (lsp_impl) to check parser
    /// availability before attempting language operations.
    pub fn has_parser_available(&self, language_name: &str) -> bool {
        self.language_registry.contains(language_name)
    }

    /// ADR-0005: Unified detection fallback chain for both host documents and injections.
    ///
    /// Returns the first language for which a parser is available.
    ///
    /// Priority order (ADR-0005), two stages:
    /// Each stage follows: detect → alias resolution → availability check
    /// 1. LSP languageId (if not "plaintext")
    /// 2. Heuristic detection:
    ///    - Explicit token (for injections, e.g., "py", "js")
    ///    - Path-derived token (extension/basename via `extract_token_from_path`)
    ///    - First-line content (shebang, mode line, Emacs markers)
    ///
    /// Usage:
    /// - Host document: `detect_language(path, content, None, language_id)`
    /// - Injection: `detect_language(token, content, Some(token), Some(token))`
    ///
    /// Visibility: Public - called by LSP layer and injection resolution.
    pub fn detect_language(
        &self,
        path: &str,
        content: &str,
        token: Option<&str>,
        language_id: Option<&str>,
    ) -> Option<String> {
        let (result, method, candidate) =
            self.detect_language_with_method(path, content, token, language_id);

        match result {
            Some(ref lang) => {
                log::debug!(
                    target: "kakehashi::language_detection",
                    "Detected '{}' via {} for path='{}'",
                    lang,
                    method,
                    path
                );
            }
            None => {
                if let Some(detected) = candidate {
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Detected '{}' but no parser available for path='{}'",
                        detected,
                        path
                    );
                } else {
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "No language detected for path='{}'",
                        path
                    );
                }
            }
        }

        result
    }

    /// Internal detection returning (result, method, last_candidate_without_parser).
    ///
    /// - `result`: The detected language with an available parser, if found
    /// - `method`: Description of the detection method that succeeded
    /// - `last_candidate`: The last detected candidate without a parser (for logging)
    fn detect_language_with_method(
        &self,
        path: &str,
        content: &str,
        token: Option<&str>,
        language_id: Option<&str>,
    ) -> (Option<String>, &'static str, Option<String>) {
        // Track last detected candidate without parser for logging
        let mut last_candidate: Option<String> = None;

        // 1. Try languageId (with alias fallback)
        if let Some(lang_id) = language_id
            && lang_id != "plaintext"
        {
            if let Some(result) = self.try_with_alias_fallback(lang_id) {
                return (Some(result), "languageId", None);
            }
            last_candidate = Some(lang_id.to_string());
        }

        // 2. Try heuristic detection: token, path-derived token, first line (shebang)
        //    Short-circuits on first successful match.
        //
        // Token priority (ADR-0005):
        // - Explicit token (e.g., code fence identifier) takes precedence
        // - Path-derived token (extension or basename) is used as fallback
        // - First line (shebang/mode line) for extensionless files
        let effective_token = token.or_else(|| super::heuristic::extract_token_from_path(path));

        if let Some(tok) = effective_token {
            // First try syntect-based detection (normalizes "py" → "python", etc.)
            if let Some(candidate) = super::heuristic::detect_from_token(tok) {
                if let Some(result) = self.try_with_alias_fallback(&candidate) {
                    let method = if token.is_some() {
                        "token"
                    } else {
                        "path-token"
                    };
                    return (Some(result), method, None);
                }
                // Syntect recognized but no parser available - record for logging
                last_candidate = Some(candidate);
            } else if let Some(result) = self.try_with_alias_fallback(tok) {
                // Syntect doesn't recognize the token, try it directly as alias
                // This handles extensions like "jsx", "tsx" that syntect doesn't know
                // but may be configured as aliases (e.g., "jsx" → "javascript")
                let method = if token.is_some() {
                    "token"
                } else {
                    "path-token"
                };
                return (Some(result), method, None);
            } else {
                // Neither syntect nor alias resolved - record raw token for logging
                last_candidate = Some(tok.to_string());
            }
        }

        if let Some(candidate) = super::heuristic::detect_from_first_line(content) {
            if let Some(result) = self.try_with_alias_fallback(&candidate) {
                return (Some(result), "first-line", None);
            }
            last_candidate = Some(candidate);
        }

        (None, "none", last_candidate)
    }

    /// ADR-0005: Resolve injection language using unified detection heuristics.
    ///
    /// Unlike `detect_language` which gates on parser availability, this function
    /// uses the same detection heuristics but attempts to LOAD the detected language.
    /// This is needed because injection discovery happens before we know which parsers
    /// are needed.
    ///
    /// Detection chain (same order as `detect_language`):
    /// 1. Direct identifier (try to load as-is)
    /// 2. Syntect token normalization (py -> python, js -> javascript)
    /// 3. First-line detection (shebang, mode line)
    ///
    /// Config-based alias resolution (rmd -> markdown) is applied as a sub-step
    /// after each detection method via `try_with_alias_fallback`.
    ///
    /// Visibility: Public - called by analysis layer (semantic.rs) for
    /// nested language injection support.
    pub fn resolve_injection_language(
        &self,
        identifier: &str,
        content: &str,
    ) -> Option<(String, LanguageLoadResult)> {
        log::debug!(
            target: "kakehashi::language_detection",
            "Resolving injection language for identifier='{}', content_len={}",
            identifier,
            content.len()
        );

        // 1. Try direct identifier first (skip "plaintext")
        if identifier != "plaintext" {
            // Try direct match
            let direct_result = self.ensure_language_loaded(identifier);
            if direct_result.success {
                log::debug!(
                    target: "kakehashi::language_detection",
                    "Resolved injection '{}' via direct identifier",
                    identifier
                );
                return Some((identifier.to_string(), direct_result));
            }

            // Try config-based alias for direct identifier
            if let Some(canonical) = self.resolve_alias(identifier) {
                let alias_result = self.ensure_language_loaded(&canonical);
                if alias_result.success {
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Resolved injection '{}' -> '{}' via config alias",
                        identifier,
                        canonical
                    );
                    return Some((canonical, alias_result));
                }
            }
        }

        // 2. Try syntect token normalization (handles py -> python, js -> javascript, etc.)
        if let Some(normalized) = super::heuristic::detect_from_token(identifier) {
            let token_result = self.ensure_language_loaded(&normalized);
            if token_result.success {
                log::debug!(
                    target: "kakehashi::language_detection",
                    "Resolved injection '{}' -> '{}' via syntect token",
                    identifier,
                    normalized
                );
                return Some((normalized, token_result));
            }

            // Try config alias for the normalized name too
            if let Some(canonical) = self.resolve_alias(&normalized) {
                let alias_result = self.ensure_language_loaded(&canonical);
                if alias_result.success {
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Resolved injection '{}' -> '{}' -> '{}' via syntect + config alias",
                        identifier,
                        normalized,
                        canonical
                    );
                    return Some((canonical, alias_result));
                }
            }
        }

        // 3. Try first-line detection (shebang, mode line)
        if let Some(first_line_lang) = super::heuristic::detect_from_first_line(content) {
            let first_line_result = self.ensure_language_loaded(&first_line_lang);
            if first_line_result.success {
                log::debug!(
                    target: "kakehashi::language_detection",
                    "Resolved injection '{}' -> '{}' via first-line detection",
                    identifier,
                    first_line_lang
                );
                return Some((first_line_lang, first_line_result));
            }

            // Try config alias for the first-line detected language
            if let Some(canonical) = self.resolve_alias(&first_line_lang) {
                let alias_result = self.ensure_language_loaded(&canonical);
                if alias_result.success {
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Resolved injection '{}' -> '{}' -> '{}' via first-line + config alias",
                        identifier,
                        first_line_lang,
                        canonical
                    );
                    return Some((canonical, alias_result));
                }
            }
        }

        log::debug!(
            target: "kakehashi::language_detection",
            "Failed to resolve injection language for identifier='{}'",
            identifier
        );
        None
    }

    /// Create a document parser pool.
    ///
    /// Visibility: Public - called by LSP layer (lsp_impl) and analysis modules
    /// to obtain parser instances for document processing.
    pub fn create_document_parser_pool(&self) -> DocumentParserPool {
        let parser_factory = ParserFactory::new(self.language_registry.clone());
        DocumentParserPool::new(parser_factory)
    }

    /// Check if queries exist for a language.
    ///
    /// Visibility: Public - called by LSP layer (lsp_impl) to determine if
    /// semantic tokens should be refreshed after language load.
    pub fn has_queries(&self, lang_name: &str) -> bool {
        self.query_store.has_highlight_query(lang_name)
    }

    /// Get highlight query for a language.
    ///
    /// Visibility: Public - called by LSP layer (semantic_tokens) and analysis
    /// layer (refactor, semantic) for syntax highlighting and token analysis.
    pub fn get_highlight_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store.get_highlight_query(lang_name)
    }

    /// Get injection query for a language.
    ///
    /// Visibility: Public - called by LSP layer (multiple handlers) and analysis
    /// layer (refactor, semantic, selection) for nested language support.
    pub fn get_injection_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store.get_injection_query(lang_name)
    }

    /// Get capture mappings.
    ///
    /// Visibility: Public - called by LSP layer (semantic_tokens) and analysis
    /// layer (refactor) for custom capture-to-token-type mapping.
    pub fn get_capture_mappings(&self) -> CaptureMappings {
        self.config_store.get_capture_mappings()
    }

    fn load_single_language(
        &self,
        lang_name: &str,
        config: &LanguageSettings,
        search_paths: &[PathBuf],
    ) -> LanguageLoadResult {
        let library_path =
            QueryLoader::resolve_library_path(config.parser.as_deref(), lang_name, search_paths);
        let Some(lib_path) = library_path else {
            return LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Error,
                format!(
                    "No parser path found for language {lang_name} in search paths: {}",
                    format_search_paths(search_paths),
                ),
            ));
        };

        let language = {
            let result = self
                .parser_loader
                .write()
                .recover_poison("LanguageCoordinator::load_single_language")
                .load_language(&lib_path, lang_name);
            match result {
                Ok(lang) => lang,
                Err(err) => {
                    return LanguageLoadResult::failure_with(LanguageEvent::log(
                        LanguageLogLevel::Error,
                        format!(
                            "Failed to load language {lang_name} from {}: {err}",
                            lib_path.display()
                        ),
                    ));
                }
            }
        };

        self.language_registry
            .register(lang_name.to_string(), language.clone());

        let mut events = self.load_queries_for_language(lang_name, config, search_paths, &language);

        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!("Language {lang_name} loaded."),
        ));
        LanguageLoadResult::success_with(events)
    }

    fn load_queries_for_language(
        &self,
        lang_name: &str,
        config: &LanguageSettings,
        search_paths: &[PathBuf],
        language: &Language,
    ) -> Vec<LanguageEvent> {
        let mut events = Vec::new();

        // Process unified queries field if present
        if let Some(queries) = &config.queries {
            events.extend(self.load_unified_queries(lang_name, queries, language));
            return events;
        }

        // Fall back to search paths when queries field is not specified
        if !search_paths.is_empty() {
            self.load_query(
                language,
                search_paths,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type: "highlights",
                    context: Some("Loaded from search paths"),
                },
                &mut events,
                |store, q| store.insert_highlight_query(lang_name.to_string(), q),
            );

            self.load_query(
                language,
                search_paths,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type: "locals",
                    context: Some("Loaded from search paths"),
                },
                &mut events,
                |store, q| store.insert_locals_query(lang_name.to_string(), q),
            );

            self.load_query(
                language,
                search_paths,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type: "injections",
                    context: Some("Loaded from search paths"),
                },
                &mut events,
                |store, q| store.insert_injection_query(lang_name.to_string(), q),
            );
        }

        events
    }

    /// Load queries from the unified queries field (new format).
    ///
    /// Processes each QueryItem, using explicit kind or inferring from filename.
    /// Unknown patterns (where kind is None and cannot be inferred) are skipped.
    fn load_unified_queries(
        &self,
        lang_name: &str,
        queries: &[crate::config::settings::QueryItem],
        language: &Language,
    ) -> Vec<LanguageEvent> {
        let mut events = Vec::new();

        // Group query paths by their effective kind
        let mut highlights: Vec<String> = Vec::new();
        let mut locals: Vec<String> = Vec::new();
        let mut injections: Vec<String> = Vec::new();

        for query in queries {
            let effective_kind = query.kind.or_else(|| infer_query_kind(&query.path));
            match effective_kind {
                Some(QueryKind::Highlights) => highlights.push(query.path.clone()),
                Some(QueryKind::Locals) => locals.push(query.path.clone()),
                Some(QueryKind::Injections) => injections.push(query.path.clone()),
                None => {
                    // Skip unrecognized patterns silently
                }
            }
        }

        // Load highlights
        if !highlights.is_empty() {
            self.load_query_from_paths(
                language,
                &highlights,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type: "highlights",
                    context: None,
                },
                &mut events,
                |store, q| store.insert_highlight_query(lang_name.to_string(), q),
            );
        }

        // Load locals
        if !locals.is_empty() {
            self.load_query_from_paths(
                language,
                &locals,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type: "locals",
                    context: None,
                },
                &mut events,
                |store, q| store.insert_locals_query(lang_name.to_string(), q),
            );
        }

        // Load injections
        if !injections.is_empty() {
            self.load_query_from_paths(
                language,
                &injections,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type: "injections",
                    context: None,
                },
                &mut events,
                |store, q| store.insert_injection_query(lang_name.to_string(), q),
            );
        }

        events
    }

    /// Register a language directly for testing purposes.
    ///
    /// This bypasses the normal loading process and directly registers
    /// a tree-sitter Language in the registry. Useful for unit tests
    /// that need to test with specific language parsers.
    #[cfg(test)]
    pub(crate) fn register_language_for_test(
        &self,
        language_id: &str,
        language: tree_sitter::Language,
    ) {
        self.language_registry
            .register(language_id.to_string(), language);
    }

    /// Register an injection query directly for testing purposes.
    ///
    /// This bypasses the normal loading process and directly registers
    /// an injection query in the query store. Useful for unit tests
    /// that need to test nested injection scenarios.
    #[cfg(test)]
    pub(crate) fn register_injection_query_for_test(
        &self,
        language_id: &str,
        query: tree_sitter::Query,
    ) {
        self.query_store
            .insert_injection_query(language_id.to_string(), Arc::new(query));
    }

    /// Get a clone of the language registry for testing purposes.
    ///
    /// This allows test code to access the registry directly for creating
    /// parser factories without going through the coordinator's normal
    /// document parser pool creation.
    #[cfg(test)]
    pub(crate) fn language_registry_for_testing(&self) -> LanguageRegistry {
        self.language_registry.clone()
    }

    /// Get a clone of the language registry for parallel processing.
    ///
    /// This allows the parallel injection processor to create thread-local
    /// parser factories without going through the coordinator's document
    /// parser pool. The registry is safely clonable (uses Arc internally).
    pub(crate) fn language_registry_for_parallel(&self) -> LanguageRegistry {
        self.language_registry.clone()
    }
}

/// Truncate a pattern string for display in log messages.
///
/// Collapses whitespace and truncates to max_len characters, adding "..." if truncated.
fn truncate_preview(pattern: &str, max_len: usize) -> String {
    // Collapse all whitespace (including newlines) to single spaces
    let collapsed: String = pattern.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = collapsed.chars().count();
    if char_count <= max_len {
        collapsed
    } else {
        // Truncate to max_len characters (including the "...")
        let truncate_at = max_len.saturating_sub(3);
        let truncated: String = collapsed.chars().take(truncate_at).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_injection_direct_identifier_first() {
        let coordinator = LanguageCoordinator::new();
        // Register "python" parser
        coordinator.register_language_for_test("python", tree_sitter_rust::LANGUAGE.into());

        // Direct identifier "python" should work
        let result = coordinator.resolve_injection_language("python", "print('hello')");
        assert!(result.is_some());
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_uses_syntect_token() {
        let coordinator = LanguageCoordinator::new();
        // Register "python" parser (not "py")
        coordinator.register_language_for_test("python", tree_sitter_rust::LANGUAGE.into());

        // Token "py" should resolve to "python" via syntect's detect_from_token
        let result = coordinator.resolve_injection_language("py", "print('hello')");
        assert!(result.is_some());
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_unknown_alias_returns_none() {
        let coordinator = LanguageCoordinator::new();
        // No parsers registered

        // Unknown alias with no parser should return None
        let result = coordinator.resolve_injection_language("unknown_lang", "");
        assert!(result.is_none());

        // Known token but no parser should also return None
        let result = coordinator.resolve_injection_language("py", "");
        assert!(result.is_none());
    }

    #[test]
    fn test_injection_uses_config_alias() {
        // When config has "rmd" → "markdown" alias, injection resolution should use it
        let coordinator = LanguageCoordinator::new();

        // Register "markdown" parser (not "rmd")
        coordinator.register_language_for_test("markdown", tree_sitter_rust::LANGUAGE.into());

        // Build config-based alias map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages);

        // Injection "rmd" should resolve to "markdown" via config alias
        let result = coordinator.resolve_injection_language("rmd", "");
        assert!(result.is_some(), "rmd should resolve via config alias");
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "markdown");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_prefers_direct_over_alias() {
        let coordinator = LanguageCoordinator::new();
        // Register both "js" and "javascript" as separate parsers
        coordinator.register_language_for_test("js", tree_sitter_rust::LANGUAGE.into());
        coordinator.register_language_for_test("javascript", tree_sitter_rust::LANGUAGE.into());

        // "js" should resolve to "js" (direct), not "javascript" (alias)
        let result = coordinator.resolve_injection_language("js", "");
        assert!(result.is_some());
        let (resolved, _) = result.unwrap();
        assert_eq!(
            resolved, "js",
            "Direct identifier should be preferred over alias"
        );
    }

    #[test]
    fn test_injection_uses_first_line_detection() {
        // When identifier doesn't match and token normalization fails,
        // fall back to first-line (shebang/mode line) detection
        let coordinator = LanguageCoordinator::new();

        // Register "python" parser
        coordinator.register_language_for_test("python", tree_sitter_rust::LANGUAGE.into());

        // Injection with unknown identifier but Python shebang in content
        let content = "#!/usr/bin/env python\nprint('hello')";
        let result = coordinator.resolve_injection_language("script", content);

        assert!(
            result.is_some(),
            "Should detect python via shebang when identifier is unknown"
        );
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_first_line_with_alias() {
        // First-line detection should also try config alias resolution
        let coordinator = LanguageCoordinator::new();

        // Register "bash" parser
        coordinator.register_language_for_test("bash", tree_sitter_rust::LANGUAGE.into());

        // Content with bash shebang - syntect detects "bash" for #!/bin/bash
        let content = "#!/bin/bash\necho hello";
        let result = coordinator.resolve_injection_language("unknown", content);

        assert!(result.is_some(), "Should detect bash via shebang");
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "bash");
        assert!(load_result.success);
    }

    #[test]
    fn test_load_settings_does_not_make_parser_available() {
        // Documents that load_settings alone does NOT make parsers available.
        // ensure_language_loaded must be called to actually load the parser.
        // This is important for reload_language_after_install to work correctly.
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();

        // Initially, parser is not available
        assert!(
            !coordinator.has_parser_available("rust"),
            "Parser should not be available before load_settings"
        );

        // Load settings (simulating apply_settings behavior)
        let settings = WorkspaceSettings::default();
        let _summary = coordinator.load_settings(settings);

        // After load_settings, parser is STILL not available
        assert!(
            !coordinator.has_parser_available("rust"),
            "Parser should not be available after load_settings alone - ensure_language_loaded must be called"
        );
    }

    // Smoke tests for coordinator API (moved from integration tests)
    // These verify the API surface exists and basic functionality works

    #[test]
    fn test_coordinator_has_parser_available() {
        let coordinator = LanguageCoordinator::new();

        // No languages loaded initially - should return false
        assert!(!coordinator.has_parser_available("rust"));

        // This test verifies the API is exposed on LanguageCoordinator.
        // The full behavior (true when loaded) is tested in unit tests
        // via register_language_for_test which is only available there.
    }

    #[test]
    fn test_heuristic_used_when_language_id_plaintext() {
        let coordinator = LanguageCoordinator::new();

        // When languageId is "plaintext", fallback to heuristic detection
        // Note: No parser loaded, so will return None (graceful degradation)
        // But the heuristic detection path is still exercised
        let content = "#!/usr/bin/env python\nprint('hello')";
        let result = coordinator.detect_language("/script", content, None, Some("plaintext"));

        // No python parser loaded, so result is None
        // The important thing is that "plaintext" didn't short-circuit
        assert_eq!(result, None);
    }

    #[test]
    fn test_heuristic_skipped_when_language_id_has_parser() {
        let coordinator = LanguageCoordinator::new();

        // When languageId has an available parser, don't run heuristic detection
        // This tests lazy evaluation - heuristic parsing is skipped entirely

        // Scenario: languageId is "rust" but no rust parser loaded
        // So it falls through to heuristic, but no python parser either
        let content = "#!/usr/bin/env python\nprint('hello')";
        let result = coordinator.detect_language("/script", content, None, Some("rust"));

        // Neither rust nor python parser loaded
        assert_eq!(result, None);

        // Full behavior with loaded parser is tested in unit tests
    }

    #[test]
    fn test_extension_fallback_after_heuristic() {
        let coordinator = LanguageCoordinator::new();

        // When heuristic detection fails (no parser), extension fallback runs
        // File has .rs extension but content has python shebang
        let content = "#!/usr/bin/env python\nprint('hello')";
        let result = coordinator.detect_language("/path/to/file.rs", content, None, None);

        // No parsers loaded, so result is None
        // But the chain tried: languageId (None) -> heuristic (python, no parser) -> extension (rs, no parser)
        assert_eq!(result, None);
    }

    #[test]
    fn test_full_detection_chain() {
        let coordinator = LanguageCoordinator::new();

        // Full chain test: languageId -> heuristic -> extension
        // All methods tried, none have parsers available

        // languageId = "plaintext" (skipped), heuristic = python (no parser), extension = rs (no parser)
        let content = "#!/usr/bin/env python\nprint('hello')";
        let result =
            coordinator.detect_language("/path/to/file.rs", content, None, Some("plaintext"));

        assert_eq!(result, None);
    }

    #[test]
    fn test_detection_chain_returns_none_when_all_fail() {
        let coordinator = LanguageCoordinator::new();

        // No languageId, no heuristic match, no extension -> None
        let result = coordinator.detect_language(
            "/random_file",
            "random content without shebang",
            None,
            None,
        );

        assert_eq!(result, None);
    }

    #[test]
    fn test_heuristic_detects_makefile_by_filename() {
        let coordinator = LanguageCoordinator::new();

        // Makefile has no extension but should be detected by filename pattern
        // No parser loaded, so returns None (but heuristic path is exercised)
        let result = coordinator.detect_language("/path/to/Makefile", "all: build", None, None);

        // No make parser loaded
        assert_eq!(result, None);
    }

    // Tests for load_unified_queries

    #[test]
    fn test_load_unified_queries_with_explicit_kind() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator.register_language_for_test("rust", tree_sitter_rust::LANGUAGE.into());

        // Create a temporary query file with valid highlights content
        let temp_dir = TempDir::new().unwrap();
        let query_path = temp_dir.path().join("my_highlights.scm");
        let mut file = fs::File::create(&query_path).unwrap();
        writeln!(file, "(identifier) @variable").unwrap();

        // Create QueryItem with explicit kind
        let queries = vec![QueryItem {
            path: query_path.to_str().unwrap().to_string(),
            kind: Some(QueryKind::Highlights),
        }];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have one info event for successful load
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            LanguageEvent::Log {
                level: LanguageLogLevel::Info,
                ..
            }
        ));

        // Verify the query was actually loaded
        assert!(
            coordinator.get_highlight_query("rust").is_some(),
            "Highlight query should be loaded"
        );
    }

    #[test]
    fn test_load_unified_queries_with_filename_inference() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator.register_language_for_test("rust", tree_sitter_rust::LANGUAGE.into());

        // Create a temporary query file with the exact filename "highlights.scm"
        let temp_dir = TempDir::new().unwrap();
        let query_path = temp_dir.path().join("highlights.scm");
        let mut file = fs::File::create(&query_path).unwrap();
        writeln!(file, "(identifier) @variable").unwrap();

        // Create QueryItem WITHOUT explicit kind - should infer from filename
        let queries = vec![QueryItem {
            path: query_path.to_str().unwrap().to_string(),
            kind: None, // No explicit kind - will be inferred
        }];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have one info event for successful load
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            LanguageEvent::Log {
                level: LanguageLogLevel::Info,
                ..
            }
        ));

        // Verify the query was actually loaded via inference
        assert!(
            coordinator.get_highlight_query("rust").is_some(),
            "Highlight query should be loaded via filename inference"
        );
    }

    #[test]
    fn test_load_unified_queries_unknown_patterns_skipped() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator.register_language_for_test("rust", tree_sitter_rust::LANGUAGE.into());

        // Create a temporary query file with a non-standard filename
        let temp_dir = TempDir::new().unwrap();
        let query_path = temp_dir.path().join("custom.scm");
        let mut file = fs::File::create(&query_path).unwrap();
        writeln!(file, "(identifier) @variable").unwrap();

        // Create QueryItem with unknown pattern (no explicit kind, non-standard filename)
        let queries = vec![QueryItem {
            path: query_path.to_str().unwrap().to_string(),
            kind: None, // No explicit kind and filename won't match inference patterns
        }];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries - should silently skip the unknown pattern
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have NO events because the unknown pattern was silently skipped
        assert_eq!(
            events.len(),
            0,
            "Unknown patterns should be silently skipped with no events"
        );

        // Verify no queries were loaded
        assert!(
            coordinator.get_highlight_query("rust").is_none(),
            "No highlight query should be loaded for unknown pattern"
        );
    }

    #[test]
    fn test_load_unified_queries_grouped_by_type() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator.register_language_for_test("rust", tree_sitter_rust::LANGUAGE.into());

        // Create temporary query files for different types
        let temp_dir = TempDir::new().unwrap();

        // Highlights query
        let highlights_path = temp_dir.path().join("highlights.scm");
        let mut highlights_file = fs::File::create(&highlights_path).unwrap();
        writeln!(highlights_file, "(identifier) @variable").unwrap();

        // Locals query
        let locals_path = temp_dir.path().join("locals.scm");
        let mut locals_file = fs::File::create(&locals_path).unwrap();
        writeln!(locals_file, "(identifier) @local.definition").unwrap();

        // Injections query
        let injections_path = temp_dir.path().join("injections.scm");
        let mut injections_file = fs::File::create(&injections_path).unwrap();
        writeln!(injections_file, "; empty injection query").unwrap();

        // Create mixed QueryItems - some with explicit kind, some with inference
        let queries = vec![
            QueryItem {
                path: highlights_path.to_str().unwrap().to_string(),
                kind: None, // Will be inferred as Highlights
            },
            QueryItem {
                path: locals_path.to_str().unwrap().to_string(),
                kind: Some(QueryKind::Locals), // Explicit kind
            },
            QueryItem {
                path: injections_path.to_str().unwrap().to_string(),
                kind: None, // Will be inferred as Injections
            },
        ];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have 3 info events (one for each query type)
        assert_eq!(events.len(), 3, "Should have 3 events for 3 query types");
        for event in &events {
            assert!(
                matches!(
                    event,
                    LanguageEvent::Log {
                        level: LanguageLogLevel::Info,
                        ..
                    }
                ),
                "All events should be Info level"
            );
        }

        // Verify highlight query was loaded (locals and injections confirmed by events)
        assert!(
            coordinator.get_highlight_query("rust").is_some(),
            "Highlight query should be loaded"
        );
    }

    // Tests for alias resolution

    #[test]
    fn test_alias_resolution_detects_canonical_language() {
        // When languageId "rmd" is aliased to "markdown" and markdown parser exists,
        // detect_language should return "markdown"
        let coordinator = LanguageCoordinator::new();

        // Register "markdown" parser (using rust as a stand-in)
        coordinator.register_language_for_test("markdown", tree_sitter_rust::LANGUAGE.into());

        // Build alias map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string(), "qmd".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages);

        // Detection with languageId "rmd" should resolve to "markdown"
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result,
            Some("markdown".to_string()),
            "rmd should resolve to markdown via alias"
        );

        // Also test qmd
        let result = coordinator.detect_language("/path/to/file.qmd", "", None, Some("qmd"));
        assert_eq!(
            result,
            Some("markdown".to_string()),
            "qmd should also resolve to markdown via alias"
        );
    }

    #[test]
    fn test_alias_resolution_prefers_direct_language() {
        // When languageId directly has a parser, use it (don't check alias)
        let coordinator = LanguageCoordinator::new();

        // Register both "rmd" and "markdown" as separate parsers
        coordinator.register_language_for_test("rmd", tree_sitter_rust::LANGUAGE.into());
        coordinator.register_language_for_test("markdown", tree_sitter_rust::LANGUAGE.into());

        // Build alias map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages);

        // Detection with languageId "rmd" should use "rmd" directly (not alias)
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result,
            Some("rmd".to_string()),
            "Direct languageId should be preferred over alias"
        );
    }

    #[test]
    fn test_alias_resolution_skipped_when_no_parser_for_canonical() {
        // When alias points to a language without a parser, continue fallback
        let coordinator = LanguageCoordinator::new();

        // Don't register any parser - only the alias mapping

        // Build alias map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages);

        // Detection should return None (alias found but no parser for "markdown")
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result, None,
            "Should return None when alias target has no parser"
        );
    }

    #[test]
    fn test_alias_map_cleared_on_reload() {
        // Verify that alias map is cleared and rebuilt when settings change
        let coordinator = LanguageCoordinator::new();

        // First config: "rmd" → "markdown"
        let mut languages1 = HashMap::new();
        languages1.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages1);

        // Verify first mapping
        assert_eq!(
            coordinator.resolve_alias("rmd"),
            Some("markdown".to_string())
        );

        // Second config: no aliases for markdown, "jsx" → "javascript"
        let mut languages2 = HashMap::new();
        languages2.insert(
            "javascript".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["jsx".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages2);

        // Old alias should be gone
        assert_eq!(
            coordinator.resolve_alias("rmd"),
            None,
            "Old alias should be cleared after rebuild"
        );
        // New alias should work
        assert_eq!(
            coordinator.resolve_alias("jsx"),
            Some("javascript".to_string())
        );
    }

    // ADR-0005: Alias resolution as sub-step for extension detection

    #[test]
    fn test_extension_detection_with_alias_fallback() {
        // When extension is "jsx" and alias maps "jsx" → "javascript",
        // detect_language should return "javascript"
        let coordinator = LanguageCoordinator::new();

        // Register "javascript" parser (using rust as a stand-in)
        coordinator.register_language_for_test("javascript", tree_sitter_rust::LANGUAGE.into());

        // Build alias map: "jsx" → "javascript"
        let mut languages = HashMap::new();
        languages.insert(
            "javascript".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["jsx".to_string(), "mjs".to_string()]),
                ..Default::default()
            },
        );
        coordinator.build_alias_map(&languages);

        // No languageId, no shebang, extension = jsx
        let result = coordinator.detect_language("/path/to/component.jsx", "", None, None);

        assert_eq!(
            result,
            Some("javascript".to_string()),
            "jsx extension should resolve to javascript via alias"
        );
    }

    // Tests for truncate_preview

    #[test]
    fn test_truncate_preview_short_string() {
        let result = truncate_preview("(identifier) @variable", 60);
        assert_eq!(result, "(identifier) @variable");
    }

    #[test]
    fn test_truncate_preview_long_string() {
        let long_pattern = "a".repeat(100);
        let result = truncate_preview(&long_pattern, 20);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 20); // 17 'a's + "..."
    }

    #[test]
    fn test_truncate_preview_collapses_whitespace() {
        let result = truncate_preview("(identifier)\n    @variable", 60);
        assert_eq!(result, "(identifier) @variable");
    }

    #[test]
    fn test_truncate_preview_multibyte_characters() {
        // Pattern with multi-byte UTF-8 characters (Japanese)
        // "こんにちは世界" = 7 characters
        let pattern = "こんにちは世界";
        // Truncate to 5 characters - should include 2 chars + "..."
        let result = truncate_preview(pattern, 5);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 5); // 2 Japanese chars + "..."
        assert_eq!(result, "こん...");
    }

    #[test]
    fn test_truncate_preview_mixed_ascii_multibyte() {
        // Mix of ASCII and multi-byte characters
        // "abc日本語def" = 10 characters
        let pattern = "abc日本語def";
        let result = truncate_preview(pattern, 8);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 8); // 5 chars + "..."
        assert_eq!(result, "abc日本...");
    }

    #[test]
    fn ensure_language_loaded_fails_with_empty_search_paths() {
        let coordinator = LanguageCoordinator::new();
        // No settings loaded → search_paths is empty Vec
        let result = coordinator.ensure_language_loaded("lua");
        assert!(!result.success, "Should fail when search paths are empty");
        match &result.events[0] {
            LanguageEvent::Log { level, message } => {
                assert!(matches!(level, LanguageLogLevel::Warning));
                assert!(
                    message.contains("No search paths configured"),
                    "Expected 'No search paths configured' warning, got: {message}"
                );
            }
            other => panic!("Expected Log event, got: {other:?}"),
        }
    }

    #[test]
    fn load_single_language_with_empty_search_paths_skips_queries() {
        use crate::config::settings::LanguageSettings;

        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let parser_path = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        if !parser_path.exists() {
            eprintln!(
                "skipping load_single_language_with_empty_search_paths_skips_queries: parser '{}' does not exist",
                parser_path.display()
            );
            return;
        }

        let config = LanguageSettings {
            parser: Some(parser_path.to_string_lossy().into_owned()),
            queries: None,
            ..Default::default()
        };
        const NO_SEARCH_PATHS: &[PathBuf] = &[];
        let result = coordinator.load_single_language("lua", &config, NO_SEARCH_PATHS);
        assert!(result.success, "Language should load with explicit parser");
        assert!(
            coordinator.has_parser_available("lua"),
            "Parser should be registered"
        );
        assert!(
            !coordinator.has_queries("lua"),
            "No queries should be loaded when search_paths is empty and no queries field"
        );
    }

    #[test]
    fn dynamic_lua_load_from_search_paths() {
        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let search_path = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        if !std::path::Path::new(&search_path).exists() {
            eprintln!(
                "skipping dynamic_lua_load_from_search_paths: grammar path '{}' does not exist",
                search_path
            );
            return;
        }

        let settings = WorkspaceSettings {
            search_paths: vec![search_path.clone()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(settings);

        assert!(
            !coordinator.config_store.get_search_paths().is_empty(),
            "Search paths should be set after load_settings"
        );

        let result = coordinator.ensure_language_loaded("lua");

        assert!(
            result.success,
            "Lua should load successfully from {}",
            search_path
        );

        assert!(
            coordinator.has_parser_available("lua"),
            "Lua should be registered in language registry"
        );
        assert!(
            coordinator.has_queries("lua"),
            "Lua should have highlight queries"
        );
    }
}
