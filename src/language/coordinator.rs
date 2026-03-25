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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tree_sitter::Language;

/// Maximum length (in characters) for pattern previews in log messages.
const MAX_PREVIEW_LEN: usize = 60;

/// Context for loading a query, including metadata for log messages.
struct QueryLoadContext<'a> {
    language_id: &'a str,
    query_type: &'a str,
}

/// Coordinates language runtime components (registry, queries, configs).
pub(crate) struct LanguageCoordinator {
    query_store: QueryStore,
    config_store: ConfigStore,
    filetype_resolver: FiletypeResolver,
    language_registry: LanguageRegistry,
    parser_loader: RwLock<ParserLoader>,
    /// Maps derived languageId → base language name.
    /// Built from `languages.<name>.base` in configuration.
    /// Example: "rmd" → "markdown" (when rmd has `base = "markdown"`)
    base_map: RwLock<HashMap<String, String>>,
    derived_languages: RwLock<HashSet<String>>,
    config_warnings: RwLock<Vec<String>>,
}

impl Default for LanguageCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            query_store: QueryStore::new(),
            config_store: ConfigStore::new(),
            filetype_resolver: FiletypeResolver::new(),
            language_registry: LanguageRegistry::new(),
            parser_loader: RwLock::new(ParserLoader::new()),
            base_map: RwLock::new(HashMap::new()),
            derived_languages: RwLock::new(HashSet::new()),
            config_warnings: RwLock::new(Vec::new()),
        }
    }

    /// Ensure a language parser is loaded, attempting dynamic load if needed.
    ///
    /// Visibility: pub(crate) - called by LSP layer (semantic_tokens, selection_range)
    /// and analysis modules to ensure parsers are available before use.
    pub(crate) fn ensure_language_loaded(&self, language_id: &str) -> LanguageLoadResult {
        if self.language_registry.contains(language_id) {
            LanguageLoadResult::success_with(Vec::new())
        } else {
            self.try_load_language_by_id(language_id)
        }
    }

    /// Initialize from workspace-level settings and return coordination events.
    ///
    /// Visibility: pub(crate) - called by LSP layer during initialization and
    /// settings updates to configure language support.
    pub(crate) fn load_settings(&self, settings: &WorkspaceSettings) -> LanguageLoadSummary {
        self.config_store.update_from_settings(settings);
        self.clear_derived_languages();

        // Build base map from language configs
        self.build_base_map(&settings.languages);

        // Partition languages into (no-base, has-base) groups.
        // HashMap iteration order is arbitrary, so explicit partitioning
        // ensures base languages are loaded before derived ones.
        let (base_languages, derived_languages): (Vec<_>, Vec<_>) = settings
            .languages
            .iter()
            .partition(|(_, config)| config.base.is_none());
        let derived_languages = Self::sort_derived_languages(&settings.languages, derived_languages);

        let mut summary = LanguageLoadSummary::default();
        summary.events.extend(self.config_warning_events());
        let search_paths = self.config_store.search_paths();

        // Pass 1: Load all languages WITHOUT base (normal path)
        for (lang_name, config) in &base_languages {
            let result = self.load_single_language(lang_name, config, &search_paths);
            summary.record(lang_name, result);
        }

        // Pass 2: For each language WITH base, register base's parser and queries
        // under the derived name
        for (derived_name, config) in &derived_languages {
            let base_config = config
                .base
                .as_ref()
                .and_then(|base_name| settings.languages.get(base_name));
            let result =
                self.load_derived_language(derived_name, config, base_config, &search_paths);
            summary.record(derived_name, result);
        }

        summary
    }

    /// Load a derived language by copying parser and queries from its base.
    ///
    /// The base's parser is loaded first (if not already available), then
    /// registered under the derived name. Queries are similarly copied.
    fn load_derived_language(
        &self,
        derived_name: &str,
        config: &LanguageSettings,
        base_config: Option<&LanguageSettings>,
        search_paths: &[PathBuf],
    ) -> LanguageLoadResult {
        let base_name = match &config.base {
            Some(name) if name != derived_name => name.as_str(),
            Some(_) => {
                // Self-reference: load as a normal language using its own config
                return self.load_single_language(derived_name, config, search_paths);
            }
            None => {
                return LanguageLoadResult::failure_with(LanguageEvent::log(
                    LanguageLogLevel::Error,
                    format!("load_derived_language called for '{derived_name}' without base"),
                ));
            }
        };

        if base_config.is_none() && config.parser.is_some() {
            return self.load_single_language(derived_name, config, search_paths);
        }

        // Ensure base language is loaded (may need dynamic load from search paths)
        if !self.language_registry.contains(base_name) {
            let base_result = self.try_load_language_by_id(base_name);
            if !base_result.success {
                return LanguageLoadResult::failure_with(LanguageEvent::log(
                    LanguageLogLevel::Error,
                    format!(
                        "Cannot load derived language '{derived_name}': \
                         base language '{base_name}' not found"
                    ),
                ));
            }
        }

        // Get the base's Language object and register under derived name
        let language = match self.language_registry.get(base_name) {
            Some(lang) => lang,
            None => {
                return LanguageLoadResult::failure_with(LanguageEvent::log(
                    LanguageLogLevel::Error,
                    format!("Base language '{base_name}' was loaded but not found in registry"),
                ));
            }
        };

        if let Some(base_config) = base_config
            && config.parser != base_config.parser
        {
            return self.load_single_language(derived_name, config, search_paths);
        }

        self.language_registry
            .register(derived_name.to_string(), language.clone());
        self.derived_languages
            .write()
            .recover_poison("LanguageCoordinator::load_derived_language(derived_languages)")
            .insert(derived_name.to_string());

        let mut events = Vec::new();
        if let Some(base_config) = base_config
            && config.queries != base_config.queries
        {
            events.extend(self.load_queries_for_language(
                derived_name,
                config,
                search_paths,
                &language,
            ));
        } else {
            if let Some(query) = self.query_store.highlight_query(base_name) {
                self.query_store
                    .insert_highlight_query(derived_name.to_string(), query);
            }
            if let Some(query) = self.query_store.locals_query(base_name) {
                self.query_store
                    .insert_locals_query(derived_name.to_string(), query);
            }
            if let Some(query) = self.query_store.injection_query(base_name) {
                self.query_store
                    .insert_injection_query(derived_name.to_string(), query);
            }
        }

        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!("Derived language '{derived_name}' loaded from base '{base_name}'"),
        ));
        if self.has_queries(derived_name) {
            events.push(LanguageEvent::semantic_tokens_refresh(
                derived_name.to_string(),
            ));
        }

        LanguageLoadResult::success_with(events)
    }

    fn clear_derived_languages(&self) {
        let mut derived_languages = self
            .derived_languages
            .write()
            .recover_poison("LanguageCoordinator::clear_derived_languages");

        for language_id in derived_languages.drain() {
            self.language_registry.unregister(&language_id);
            self.query_store.remove_queries(&language_id);
        }
    }

    fn sort_derived_languages<'a>(
        languages: &'a HashMap<String, LanguageSettings>,
        mut derived_languages: Vec<(&'a String, &'a LanguageSettings)>,
    ) -> Vec<(&'a String, &'a LanguageSettings)> {
        derived_languages.sort_by(|(left_name, _), (right_name, _)| {
            Self::derived_load_depth(left_name, languages)
                .cmp(&Self::derived_load_depth(right_name, languages))
                .then_with(|| left_name.cmp(right_name))
        });
        derived_languages
    }

    fn derived_load_depth(
        language_id: &str,
        languages: &HashMap<String, LanguageSettings>,
    ) -> usize {
        Self::derived_load_depth_with_seen(language_id, languages, &mut HashSet::new())
    }

    fn derived_load_depth_with_seen(
        language_id: &str,
        languages: &HashMap<String, LanguageSettings>,
        seen: &mut HashSet<String>,
    ) -> usize {
        if !seen.insert(language_id.to_string()) {
            return 0;
        }

        let depth = languages
            .get(language_id)
            .and_then(|config| config.base.as_deref())
            .filter(|base_name| *base_name != language_id)
            .and_then(|base_name| {
                languages.get(base_name).map(|_| {
                    1 + Self::derived_load_depth_with_seen(base_name, languages, seen)
                })
            })
            .unwrap_or(0);

        seen.remove(language_id);
        depth
    }

    /// Build the derived → base language map from configuration.
    ///
    /// For each language with `base = "markdown"`, maps derived_name → "markdown".
    /// This enables editors sending languageId "rmd" to use the "markdown" parser.
    ///
    /// Also warns if the deprecated `aliases` field is still in use.
    fn build_base_map(&self, languages: &HashMap<String, LanguageSettings>) {
        let mut base_map = self
            .base_map
            .write()
            .recover_poison("LanguageCoordinator::build_base_map");
        let mut config_warnings = self
            .config_warnings
            .write()
            .recover_poison("LanguageCoordinator::build_base_map(config_warnings)");

        base_map.clear();
        config_warnings.clear();

        for (lang_name, config) in languages {
            if let Some(base) = &config.base {
                if lang_name == base {
                    let message = format!(
                        "Language '{}' has base='{}' (self-reference). \
                         The base field will be ignored.",
                        lang_name, base
                    );
                    log::warn!(target: "kakehashi::config", "{message}");
                    config_warnings.push(message);
                } else {
                    base_map.insert(lang_name.clone(), base.clone());
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Registered base '{}' → '{}'",
                        lang_name,
                        base
                    );
                }
            }

            if let Some(aliases) = &config.aliases {
                let example_alias = aliases
                    .iter()
                    .next()
                    .map(|a| a.as_str())
                    .unwrap_or("<derived>");
                let message = format!(
                    "Language '{}' uses deprecated 'aliases' field. \
                     Use 'base' on derived languages instead. \
                     Example: [languages.{}] base = \"{}\"",
                    lang_name, example_alias, lang_name
                );
                log::warn!(target: "kakehashi::config", "{message}");
                config_warnings.push(message);
            }
        }
    }

    /// Resolve a derived languageId to its base language name.
    ///
    /// Returns the base name if the input has a base declaration, otherwise None.
    /// Example: "rmd" → Some("markdown") if rmd has `base = "markdown"`
    fn resolve_base(&self, language_id: &str) -> Option<String> {
        let base_map = self
            .base_map
            .read()
            .recover_poison("LanguageCoordinator::resolve_base");

        base_map.get(language_id).cloned()
    }

    fn config_warning_events(&self) -> Vec<LanguageEvent> {
        let warnings = self
            .config_warnings
            .read()
            .recover_poison("LanguageCoordinator::config_warning_events");

        warnings
            .iter()
            .cloned()
            .map(|message| LanguageEvent::show_message(LanguageLogLevel::Warning, message))
            .collect()
    }

    /// ADR-0005: Try candidate directly, then with config-based base fallback.
    ///
    /// Returns the language name if a parser is available (either directly or via base).
    /// This is applied as a sub-step after each detection method.
    fn try_with_base_fallback(&self, candidate: &str) -> Option<String> {
        // Direct match
        if self.has_parser_available(candidate) {
            return Some(candidate.to_string());
        }
        // Config-based base resolution
        if let Some(base) = self.resolve_base(candidate)
            && self.has_parser_available(&base)
        {
            return Some(base);
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

        let search_paths = self.config_store.search_paths();
        if search_paths.is_empty() {
            return LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Warning,
                format!("No search paths configured, cannot load language '{language_id}'"),
            ));
        }

        // Warning: parser may not exist yet (dynamic discovery)
        let language = match self.load_and_register_parser(
            language_id,
            None,
            &search_paths,
            LanguageLogLevel::Warning,
        ) {
            Ok(lang) => lang,
            Err(result) => return result,
        };

        let mut events = Vec::new();

        // Use fault-tolerant loading for all query types
        // This handles languages like TypeScript that inherit from ecma,
        // and gracefully skips invalid patterns while preserving valid ones
        self.load_all_queries(
            &language,
            &search_paths,
            language_id,
            "Dynamically loaded",
            &mut events,
        );

        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!("Dynamically loaded language {language_id} from search paths",),
        ));
        if self.has_queries(language_id) {
            events.push(LanguageEvent::semantic_tokens_refresh(
                language_id.to_string(),
            ));
        }

        LanguageLoadResult::success_with(events)
    }

    /// Resolve, load, and register a parser for the given language.
    ///
    /// Returns `Ok(Language)` on success, or `Err(LanguageLoadResult)` with
    /// an appropriate failure event on error.
    ///
    /// `missing_parser_level` controls how a missing parser is reported:
    /// - `Warning` for dynamic loading — the parser may not exist yet (normal)
    /// - `Error` for config-driven loading — the parser was explicitly configured
    ///   but cannot be found (configuration problem)
    fn load_and_register_parser(
        &self,
        lang_name: &str,
        parser_config: Option<&str>,
        search_paths: &[PathBuf],
        missing_parser_level: LanguageLogLevel,
    ) -> Result<Language, LanguageLoadResult> {
        let library_path =
            QueryLoader::resolve_library_path(parser_config, lang_name, search_paths);
        let Some(lib_path) = library_path else {
            return Err(LanguageLoadResult::failure_with(LanguageEvent::log(
                missing_parser_level,
                format!(
                    "No parser path found for language '{lang_name}' in search paths: {}",
                    format_search_paths(search_paths),
                ),
            )));
        };

        let language = {
            let result = self
                .parser_loader
                .write()
                .recover_poison("LanguageCoordinator::load_and_register_parser")
                .load_language(&lib_path, lang_name);
            match result {
                Ok(lang) => lang,
                Err(err) => {
                    return Err(LanguageLoadResult::failure_with(LanguageEvent::log(
                        LanguageLogLevel::Error,
                        format!(
                            "Failed to load language {lang_name} from {}: {err}",
                            lib_path.display()
                        ),
                    )));
                }
            }
        };

        self.language_registry
            .register(lang_name.to_string(), language.clone());

        Ok(language)
    }

    /// Load all three query types (highlights, locals, injections) for a language.
    fn load_all_queries(
        &self,
        language: &Language,
        search_paths: &[PathBuf],
        lang_name: &str,
        context: &str,
        events: &mut Vec<LanguageEvent>,
    ) {
        for query_type in ["highlights", "locals", "injections"] {
            self.load_query(
                language,
                search_paths,
                QueryLoadContext {
                    language_id: lang_name,
                    query_type,
                },
                context,
                events,
                |store, query| match query_type {
                    "highlights" => store.insert_highlight_query(lang_name.to_string(), query),
                    "locals" => store.insert_locals_query(lang_name.to_string(), query),
                    "injections" => store.insert_injection_query(lang_name.to_string(), query),
                    _ => unreachable!(),
                },
            );
        }
    }

    /// Load a query file with inheritance resolution.
    fn load_query(
        &self,
        language: &Language,
        paths: &[PathBuf],
        ctx: QueryLoadContext<'_>,
        context: &str,
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
    /// Visibility: pub(crate) - called by LSP layer (auto_install, lsp_impl)
    /// for document language detection.
    pub(crate) fn language_for_path(&self, path: &str) -> Option<String> {
        self.filetype_resolver.language_for_path(path)
    }

    /// Check if a parser is available for a given language name.
    ///
    /// Used by the detection fallback chain (ADR-0005) to determine whether
    /// to accept a detection result or continue to the next method.
    ///
    /// Visibility: pub(crate) - called by LSP layer (lsp_impl) to check parser
    /// availability before attempting language operations.
    pub(crate) fn has_parser_available(&self, language_name: &str) -> bool {
        self.language_registry.contains(language_name)
    }

    /// ADR-0005: Unified detection fallback chain for both host documents and injections.
    ///
    /// Returns the first language for which a parser is available.
    ///
    /// Priority order (ADR-0005), two stages:
    /// Each stage follows: detect → base resolution → availability check
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
    /// Visibility: pub(crate) - called by LSP layer and injection resolution.
    pub(crate) fn detect_language(
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

        // 1. Try languageId (with base fallback)
        if let Some(lang_id) = language_id
            && lang_id != "plaintext"
        {
            if let Some(result) = self.try_with_base_fallback(lang_id) {
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
                if let Some(result) = self.try_with_base_fallback(&candidate) {
                    let method = if token.is_some() {
                        "token"
                    } else {
                        "path-token"
                    };
                    return (Some(result), method, None);
                }
                // Syntect recognized but no parser available - record for logging
                last_candidate = Some(candidate);
            } else if let Some(result) = self.try_with_base_fallback(tok) {
                // Syntect doesn't recognize the token, try it directly as base
                // This handles extensions like "jsx", "tsx" that syntect doesn't know
                // but may be configured with base (e.g., jsx has base = "javascript")
                let method = if token.is_some() {
                    "token"
                } else {
                    "path-token"
                };
                return (Some(result), method, None);
            } else {
                // Neither syntect nor base resolved - record raw token for logging
                last_candidate = Some(tok.to_string());
            }
        }

        if let Some(candidate) = super::heuristic::detect_from_first_line(content) {
            if let Some(result) = self.try_with_base_fallback(&candidate) {
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
    /// Config-based base resolution (rmd -> markdown) is applied as a sub-step
    /// after each detection method via `try_load_with_base`.
    ///
    /// Visibility: pub(crate) - called by analysis layer (semantic.rs) for
    /// nested language injection support.
    pub(crate) fn resolve_injection_language(
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
        if identifier != "plaintext"
            && let Some(found) = self.try_load_with_base(identifier)
        {
            log::debug!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via identifier (direct or base)",
                identifier, found.0
            );
            return Some(found);
        }

        // 2. Try syntect token normalization (handles py -> python, js -> javascript, etc.)
        if let Some(normalized) = super::heuristic::detect_from_token(identifier)
            && let Some(found) = self.try_load_with_base(&normalized)
        {
            log::debug!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via syntect token (direct or base)",
                identifier, found.0
            );
            return Some(found);
        }

        // 3. Try first-line detection (shebang, mode line)
        if let Some(first_line_lang) = super::heuristic::detect_from_first_line(content)
            && let Some(found) = self.try_load_with_base(&first_line_lang)
        {
            log::debug!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via first-line detection (direct or base)",
                identifier, found.0
            );
            return Some(found);
        }

        log::debug!(
            target: "kakehashi::language_detection",
            "Failed to resolve injection language for identifier='{}'",
            identifier
        );
        None
    }

    /// Try to load a language directly, then via config base resolution.
    ///
    /// Returns `(resolved_name, load_result)` on success, or `None` if
    /// neither direct load nor base resolution succeeded.
    fn try_load_with_base(&self, candidate: &str) -> Option<(String, LanguageLoadResult)> {
        let result = self.ensure_language_loaded(candidate);
        if result.success {
            return Some((candidate.to_string(), result));
        }
        if let Some(base) = self.resolve_base(candidate) {
            let result = self.ensure_language_loaded(&base);
            if result.success {
                return Some((base, result));
            }
        }
        None
    }

    /// Create a document parser pool.
    ///
    /// Visibility: pub(crate) - called by LSP layer (lsp_impl) and analysis modules
    /// to obtain parser instances for document processing.
    pub(crate) fn create_document_parser_pool(&self) -> DocumentParserPool {
        let parser_factory = ParserFactory::new(self.language_registry.clone());
        DocumentParserPool::new(parser_factory)
    }

    /// Access the query store.
    ///
    /// Visibility: pub(crate) - used internally by coordinator methods and
    /// by test code that needs to register queries directly.
    pub(crate) fn query_store(&self) -> &QueryStore {
        &self.query_store
    }

    /// Check if queries exist for a language.
    ///
    /// Visibility: pub(crate) - called by LSP layer (lsp_impl) to determine if
    /// semantic tokens should be refreshed after language load.
    pub(crate) fn has_queries(&self, lang_name: &str) -> bool {
        self.query_store().has_highlight_query(lang_name)
    }

    /// Get highlight query for a language.
    ///
    /// Visibility: pub(crate) - called by LSP layer (semantic_tokens) and analysis
    /// layer (refactor, semantic) for syntax highlighting and token analysis.
    pub(crate) fn highlight_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store().highlight_query(lang_name)
    }

    /// Get injection query for a language.
    ///
    /// Visibility: pub(crate) - called by LSP layer (multiple handlers) and analysis
    /// layer (refactor, semantic, selection) for nested language support.
    pub(crate) fn injection_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store().injection_query(lang_name)
    }

    /// Get capture mappings.
    ///
    /// Visibility: pub(crate) - called by LSP layer (semantic_tokens) and analysis
    /// layer (refactor) for custom capture-to-token-type mapping.
    pub(crate) fn capture_mappings(&self) -> CaptureMappings {
        self.config_store.capture_mappings()
    }

    fn load_single_language(
        &self,
        lang_name: &str,
        config: &LanguageSettings,
        search_paths: &[PathBuf],
    ) -> LanguageLoadResult {
        // Error: parser was explicitly configured but can't be found
        let language = match self.load_and_register_parser(
            lang_name,
            config.parser.as_deref(),
            search_paths,
            LanguageLogLevel::Error,
        ) {
            Ok(lang) => lang,
            Err(result) => return result,
        };

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
            self.load_all_queries(
                language,
                search_paths,
                lang_name,
                "Loaded from search paths",
                &mut events,
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

        for (query_type, paths) in [
            ("highlights", &highlights),
            ("locals", &locals),
            ("injections", &injections),
        ] {
            if !paths.is_empty() {
                self.load_query_from_paths(
                    language,
                    paths,
                    QueryLoadContext {
                        language_id: lang_name,
                        query_type,
                    },
                    &mut events,
                    |store, q| match query_type {
                        "highlights" => store.insert_highlight_query(lang_name.to_string(), q),
                        "locals" => store.insert_locals_query(lang_name.to_string(), q),
                        "injections" => store.insert_injection_query(lang_name.to_string(), q),
                        _ => unreachable!(),
                    },
                );
            }
        }

        events
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
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_injection_direct_identifier_first() {
        let coordinator = LanguageCoordinator::new();
        // Register "python" parser
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_rust::LANGUAGE.into());

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
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Token "py" should resolve to "python" via syntect's detect_from_token
        let result = coordinator.resolve_injection_language("py", "print('hello')");
        assert!(result.is_some());
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_unknown_base_returns_none() {
        let coordinator = LanguageCoordinator::new();
        // No parsers registered

        // Unknown language with no parser should return None
        let result = coordinator.resolve_injection_language("unknown_lang", "");
        assert!(result.is_none());

        // Known token but no parser should also return None
        let result = coordinator.resolve_injection_language("py", "");
        assert!(result.is_none());
    }

    #[test]
    fn test_injection_uses_config_base() {
        // When config has rmd.base = "markdown", injection resolution should use it
        let coordinator = LanguageCoordinator::new();

        // Register "markdown" parser (not "rmd")
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Injection "rmd" should resolve to "markdown" via base
        let result = coordinator.resolve_injection_language("rmd", "");
        assert!(result.is_some(), "rmd should resolve via config base");
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "markdown");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_prefers_direct_over_base() {
        let coordinator = LanguageCoordinator::new();
        // Register both "js" and "javascript" as separate parsers
        let registry = coordinator.language_registry_for_parallel();
        registry.register("js".to_string(), tree_sitter_rust::LANGUAGE.into());
        registry.register("javascript".to_string(), tree_sitter_rust::LANGUAGE.into());

        // "js" should resolve to "js" (direct), not "javascript" (base)
        let result = coordinator.resolve_injection_language("js", "");
        assert!(result.is_some());
        let (resolved, _) = result.unwrap();
        assert_eq!(
            resolved, "js",
            "Direct identifier should be preferred over base"
        );
    }

    #[test]
    fn test_injection_uses_first_line_detection() {
        // When identifier doesn't match and token normalization fails,
        // fall back to first-line (shebang/mode line) detection
        let coordinator = LanguageCoordinator::new();

        // Register "python" parser
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_rust::LANGUAGE.into());

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
    fn test_injection_first_line_with_base() {
        // First-line detection should also try config base resolution
        let coordinator = LanguageCoordinator::new();

        // Register "bash" parser
        coordinator
            .language_registry_for_parallel()
            .register("bash".to_string(), tree_sitter_rust::LANGUAGE.into());

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
        let _summary = coordinator.load_settings(&settings);

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

        // Full behavior (true when loaded) is tested in other unit tests.
    }

    #[test]
    fn test_heuristic_used_when_language_id_plaintext() {
        let coordinator = LanguageCoordinator::new();

        // When languageId is "plaintext", skip it and fall through to heuristic.
        // Python shebang detected but no parser loaded.
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/script", content, None, Some("plaintext"));

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_falls_back_to_heuristic_when_language_id_missing_parser() {
        let coordinator = LanguageCoordinator::new();

        // languageId "rust" has no parser, falls through to heuristic.
        // Python shebang overrides "rust" as last_candidate.
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/script", content, None, Some("rust"));

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_extension_fallback_after_heuristic() {
        let coordinator = LanguageCoordinator::new();

        // No languageId. Path extension "rs" → syntect detects "rust" (no parser).
        // Then first-line shebang detects "python" (no parser) → last candidate.
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/path/to/file.rs", content, None, None);

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_full_detection_chain() {
        let coordinator = LanguageCoordinator::new();

        // Full chain: "plaintext" skipped, "rs" extension → "rust" (no parser),
        // first-line shebang → "python" (no parser).
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) = coordinator.detect_language_with_method(
            "/path/to/file.rs",
            content,
            None,
            Some("plaintext"),
        );

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_detection_chain_returns_none_when_all_fail() {
        let coordinator = LanguageCoordinator::new();

        // No languageId, syntect doesn't recognize "random_file", no shebang.
        let (result, method, last_candidate) = coordinator.detect_language_with_method(
            "/random_file",
            "random content without shebang",
            None,
            None,
        );

        assert_eq!(result, None);
        assert_eq!(method, "none");
        // "random_file" basename is tried as token but syntect doesn't recognize it
        assert_eq!(last_candidate, Some("random_file".to_string()));
    }

    #[test]
    fn test_heuristic_detects_makefile_by_filename() {
        let coordinator = LanguageCoordinator::new();

        // Basename "Makefile" → syntect maps to "make" (no parser loaded).
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/path/to/Makefile", "all: build", None, None);

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("make".to_string()));
    }

    // Tests for load_unified_queries

    #[test]
    fn test_load_unified_queries_with_explicit_kind() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

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
            coordinator.highlight_query("rust").is_some(),
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
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

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
            coordinator.highlight_query("rust").is_some(),
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
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

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
            coordinator.highlight_query("rust").is_none(),
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
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

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
            coordinator.highlight_query("rust").is_some(),
            "Highlight query should be loaded"
        );
    }

    // Tests for base resolution

    #[test]
    fn test_base_resolution_detects_canonical_language() {
        // When languageId "rmd" has base = "markdown" and markdown parser exists,
        // detect_language should return "markdown"
        let coordinator = LanguageCoordinator::new();

        // Register "markdown" parser (using rust as a stand-in)
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "rmd" → "markdown", "qmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        languages.insert(
            "qmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Detection with languageId "rmd" should resolve to "markdown"
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result,
            Some("markdown".to_string()),
            "rmd should resolve to markdown via base"
        );

        // Also test qmd
        let result = coordinator.detect_language("/path/to/file.qmd", "", None, Some("qmd"));
        assert_eq!(
            result,
            Some("markdown".to_string()),
            "qmd should also resolve to markdown via base"
        );
    }

    #[test]
    fn test_load_settings_surfaces_deprecated_aliases_to_client() {
        let coordinator = LanguageCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string(), "qmd".to_string()]),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            summary.events.iter().any(|event| matches!(
                event,
                LanguageEvent::ShowMessage { level, message }
                    if *level == LanguageLogLevel::Warning
                        && message.contains("deprecated 'aliases' field")
                        && message.contains("Use 'base' on derived languages instead")
            )),
            "load_settings should emit a client-visible migration warning for deprecated aliases"
        );
    }

    #[test]
    fn test_load_settings_self_ref_base_surfaces_warning_and_loads_normally() {
        let coordinator = LanguageCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("rmd".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        // Should surface a user-visible warning about self-reference
        assert!(
            summary.events.iter().any(|event| matches!(
                event,
                LanguageEvent::ShowMessage { level, message }
                    if *level == LanguageLogLevel::Warning
                        && message.contains("self-reference")
                        && message.contains("rmd")
            )),
            "load_settings should emit a client-visible warning for self-referencing base. Events: {:?}",
            summary.events
        );
    }

    #[test]
    fn test_base_resolution_prefers_direct_language() {
        // When languageId directly has a parser, use it (don't check base)
        let coordinator = LanguageCoordinator::new();

        // Register both "rmd" and "markdown" as separate parsers
        let registry = coordinator.language_registry_for_parallel();
        registry.register("rmd".to_string(), tree_sitter_rust::LANGUAGE.into());
        registry.register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Detection with languageId "rmd" should use "rmd" directly (not base)
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result,
            Some("rmd".to_string()),
            "Direct languageId should be preferred over base"
        );
    }

    #[test]
    fn test_base_resolution_skipped_when_no_parser_for_base() {
        // When base points to a language without a parser, continue fallback
        let coordinator = LanguageCoordinator::new();

        // Don't register any parser - only the base mapping

        // Build base map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Detection should return None (base found but no parser for "markdown")
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result, None,
            "Should return None when base target has no parser"
        );
    }

    #[test]
    fn test_base_map_cleared_on_reload() {
        // Verify that base map is cleared and rebuilt when settings change
        let coordinator = LanguageCoordinator::new();

        // First config: "rmd" → "markdown"
        let mut languages1 = HashMap::new();
        languages1.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages1);

        // Verify first mapping
        assert_eq!(
            coordinator.resolve_base("rmd"),
            Some("markdown".to_string())
        );

        // Second config: no base for rmd, "jsx" → "javascript"
        let mut languages2 = HashMap::new();
        languages2.insert(
            "jsx".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("javascript".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages2);

        // Old base should be gone
        assert_eq!(
            coordinator.resolve_base("rmd"),
            None,
            "Old base should be cleared after rebuild"
        );
        // New base should work
        assert_eq!(
            coordinator.resolve_base("jsx"),
            Some("javascript".to_string())
        );
    }

    // ADR-0005: Base resolution as sub-step for extension detection

    #[test]
    fn test_extension_detection_with_base_fallback() {
        // When extension is "jsx" and base maps "jsx" → "javascript",
        // detect_language should return "javascript"
        let coordinator = LanguageCoordinator::new();

        // Register "javascript" parser (using rust as a stand-in)
        coordinator
            .language_registry_for_parallel()
            .register("javascript".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "jsx" → "javascript"
        let mut languages = HashMap::new();
        languages.insert(
            "jsx".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("javascript".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // No languageId, no shebang, extension = jsx
        let result = coordinator.detect_language("/path/to/component.jsx", "", None, None);

        assert_eq!(
            result,
            Some("javascript".to_string()),
            "jsx extension should resolve to javascript via base"
        );
    }

    // Tests for two-pass loading of base languages

    #[test]
    fn test_load_settings_registers_derived_language_from_base() {
        // When rmd has base = "markdown" and markdown parser is loadable,
        // load_settings should register "rmd" with markdown's parser
        let coordinator = LanguageCoordinator::new();

        // Manually register "markdown" parser (simulating it being loaded)
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query =
            std::sync::Arc::new(tree_sitter::Query::new(&lang, "(identifier) @variable").unwrap());
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), lang);
        coordinator
            .query_store()
            .insert_highlight_query("markdown".to_string(), query.clone());

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        // rmd should now have a parser available
        assert!(
            coordinator.has_parser_available("rmd"),
            "rmd should have parser from base markdown"
        );

        // rmd should also have the highlight query
        assert!(
            coordinator.has_queries("rmd"),
            "rmd should have queries from base markdown"
        );

        // rmd should be recorded as loaded
        assert!(
            summary.loaded.contains(&"rmd".to_string()),
            "rmd should be recorded as loaded"
        );
    }

    #[test]
    fn test_load_settings_derived_language_uses_effective_queries() {
        let coordinator = LanguageCoordinator::new();

        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let base_query =
            std::sync::Arc::new(tree_sitter::Query::new(&language, "(identifier) @variable").unwrap());
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), language);
        coordinator
            .query_store()
            .insert_highlight_query("markdown".to_string(), base_query);

        let dir = tempdir().unwrap();
        let custom_query = dir.path().join("rmd-highlights.scm");
        fs::write(&custom_query, "(identifier) @function").unwrap();

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                queries: Some(vec![crate::config::settings::QueryItem {
                    path: custom_query.display().to_string(),
                    kind: Some(crate::config::settings::QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            summary.loaded.contains(&"rmd".to_string()),
            "rmd should be recorded as loaded"
        );

        let rmd_query = coordinator
            .highlight_query("rmd")
            .expect("rmd should have a highlight query");
        assert_eq!(
            rmd_query.capture_names(),
            &["function".to_string()],
            "derived language should use its effective queries instead of copying base queries"
        );
    }

    #[test]
    fn test_load_settings_clears_removed_derived_language_registrations() {
        let coordinator = LanguageCoordinator::new();

        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query =
            std::sync::Arc::new(tree_sitter::Query::new(&lang, "(identifier) @variable").unwrap());
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), lang);
        coordinator
            .query_store()
            .insert_highlight_query("markdown".to_string(), query);

        let mut initial_languages = HashMap::new();
        initial_languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        initial_languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.load_settings(&WorkspaceSettings {
            languages: initial_languages,
            ..Default::default()
        });
        assert!(coordinator.has_parser_available("rmd"), "precondition");
        assert!(coordinator.has_queries("rmd"), "precondition");

        let mut reloaded_languages = HashMap::new();
        reloaded_languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        coordinator.load_settings(&WorkspaceSettings {
            languages: reloaded_languages,
            ..Default::default()
        });

        assert!(
            !coordinator.has_parser_available("rmd"),
            "removed derived language should be unregistered on reload"
        );
        assert!(
            !coordinator.has_queries("rmd"),
            "removed derived language queries should be cleared on reload"
        );
        assert_eq!(
            coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd")),
            None,
            "removed derived language should no longer resolve"
        );
    }

    #[test]
    fn test_sort_derived_languages_orders_multi_level_base_chains() {
        let languages = HashMap::from([
            (
                "markdown".to_string(),
                crate::config::settings::LanguageSettings::default(),
            ),
            (
                "markdown_custom".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("markdown".to_string()),
                    ..Default::default()
                },
            ),
            (
                "rmd".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("markdown_custom".to_string()),
                    ..Default::default()
                },
            ),
        ]);

        let derived_languages = vec![
            (
                languages.get_key_value("rmd").unwrap().0,
                languages.get_key_value("rmd").unwrap().1,
            ),
            (
                languages.get_key_value("markdown_custom").unwrap().0,
                languages.get_key_value("markdown_custom").unwrap().1,
            ),
        ];

        let sorted = LanguageCoordinator::sort_derived_languages(&languages, derived_languages);
        let sorted_names: Vec<_> = sorted
            .into_iter()
            .map(|(language_id, _)| language_id.as_str())
            .collect();

        assert_eq!(sorted_names, vec!["markdown_custom", "rmd"]);
    }

    #[test]
    fn test_load_settings_derived_with_missing_base_fails() {
        // When rmd has base = "nonexistent" and no parser for it exists,
        // it should fail gracefully
        let coordinator = LanguageCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("nonexistent".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            !coordinator.has_parser_available("rmd"),
            "rmd should not have parser when base is missing"
        );
        assert!(
            !summary.loaded.contains(&"rmd".to_string()),
            "rmd should not be recorded as loaded"
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
        let _summary = coordinator.load_settings(&settings);

        assert!(
            !coordinator.config_store.search_paths().is_empty(),
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
