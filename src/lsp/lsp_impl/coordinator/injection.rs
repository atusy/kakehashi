use std::collections::HashSet;

use url::Url;

use crate::document::DocumentStore;
use crate::language::injection::{InjectionResolver, collect_all_injections};
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::bridge::ResolvedServerConfig;
use crate::lsp::bridge::coordinator::BridgeInjection;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;

use crate::lsp::lsp_impl::detect_document_language;

use super::InstallCoordinator;
use super::install::InstallCoordinatorDeps;

pub(crate) struct InjectionCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
    documents: std::sync::Arc<DocumentStore>,
    cache: std::sync::Arc<CacheCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    auto_install: AutoInstallManager,
    bridge: std::sync::Arc<BridgeCoordinator>,
}

impl InjectionCoordinator {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            parser_pool: std::sync::Arc::clone(&server.parser_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            auto_install: server.auto_install.clone(),
            bridge: std::sync::Arc::clone(&server.bridge),
        }
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
    pub(crate) async fn close_invalidated_virtual_docs(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) {
        if invalidated_ulids.is_empty() {
            return;
        }

        self.cache
            .remove_injection_tokens_for_ulids(host_uri, invalidated_ulids);

        self.bridge
            .close_invalidated_docs(host_uri, invalidated_ulids)
            .await;
    }

    /// Get all bridge server configs for a given injection language from settings.
    ///
    /// Returns **all** servers configured for the injection language,
    /// supporting multiple servers per language (e.g., pyright + ruff both handling Python).
    pub(crate) fn get_all_bridge_configs_for_language(
        &self,
        host_language: &str,
        injection_language: &str,
    ) -> Vec<ResolvedServerConfig> {
        let settings = self.settings_manager.load_settings();
        self.bridge
            .get_all_configs_for_language(&settings, host_language, injection_language)
    }

    /// Resolve all injection regions for a document.
    ///
    /// This method:
    /// 1. Gets the injection query for `host_language`
    /// 2. Extracts the parse tree (minimal lock duration on document store)
    /// 3. Collects all injection regions via `collect_all_injections`
    /// 4. Calculates stable region IDs via `RegionIdTracker` (ADR-0019)
    ///
    /// Returns an empty Vec if no injections are found (no query, no tree,
    /// or no injection regions).
    ///
    /// # Lock Safety
    /// The document store lock is held only to clone the tree and text, then
    /// released before the tree traversal. No DashMap deadlock risk.
    pub(crate) fn resolve_injection_data(
        &self,
        uri: &Url,
        host_language: &str,
    ) -> Vec<BridgeInjection> {
        let Some(injection_query) = self.language.injection_query(host_language) else {
            return Vec::new();
        };

        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(tree) = doc.tree().cloned() else {
            return Vec::new();
        };
        let text = doc.text().to_string();
        drop(doc);

        let Some(regions) =
            collect_all_injections(&tree.root_node(), &text, Some(injection_query.as_ref()))
        else {
            return Vec::new();
        };

        if regions.is_empty() {
            return Vec::new();
        }

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
    pub(crate) async fn process_injections(&self, uri: &Url, forward_did_change: bool) {
        let Some(host_language) = self.get_language_for_document(uri) else {
            self.bridge.cancel_eager_open(uri);
            return;
        };
        let injections = self.resolve_injection_data(uri, &host_language);
        if injections.is_empty() {
            self.bridge.cancel_eager_open(uri);
            return;
        }

        if forward_did_change {
            self.bridge
                .forward_didchange_to_opened_docs(uri, &injections)
                .await;
        }

        let languages: HashSet<String> =
            injections.iter().map(|inj| inj.language.clone()).collect();

        self.check_injected_languages_auto_install(uri, &languages)
            .await;

        self.eager_spawn_bridge_servers(uri, &host_language, injections);
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
    pub(crate) async fn check_injected_languages_auto_install(
        &self,
        uri: &Url,
        languages: &HashSet<String>,
    ) {
        let install = self.install_coordinator();
        let auto_install_enabled = self.settings_manager.is_auto_install_enabled();

        let (text, reason) = if auto_install_enabled {
            (
                self.documents.get(uri).map(|doc| doc.text().to_string()),
                String::new(),
            )
        } else {
            (None, install.auto_install_disabled_reason())
        };

        for lang in languages {
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
                install.notify_parser_missing(&resolved_lang, &reason).await;
                continue;
            }

            if let Some(ref text) = text {
                let _ = install
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
    pub(crate) fn eager_spawn_bridge_servers(
        &self,
        uri: &Url,
        host_language: &str,
        injections: Vec<BridgeInjection>,
    ) {
        let settings = self.settings_manager.load_settings();
        self.bridge
            .eager_spawn_and_open_documents(&settings, host_language, uri, injections);
    }

    fn get_language_for_document(&self, uri: &Url) -> Option<String> {
        detect_document_language(&self.language, &self.documents, uri)
    }

    fn install_coordinator(&self) -> InstallCoordinator {
        InstallCoordinator::from_parts(InstallCoordinatorDeps {
            client: self.client.clone(),
            language: std::sync::Arc::clone(&self.language),
            parser_pool: std::sync::Arc::clone(&self.parser_pool),
            documents: std::sync::Arc::clone(&self.documents),
            cache: std::sync::Arc::clone(&self.cache),
            settings_manager: std::sync::Arc::clone(&self.settings_manager),
            auto_install: self.auto_install.clone(),
            bridge: std::sync::Arc::clone(&self.bridge),
        })
    }
}
