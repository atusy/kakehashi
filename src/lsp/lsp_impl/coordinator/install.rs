use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use url::Url;

use crate::config::WorkspaceSettings;
use crate::lsp::auto_install::InstallEvent;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use crate::lsp::lsp_impl::{
    Kakehashi, apply_shared_settings, build_notifier, detect_document_language,
};
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;

use super::{ParseCoordinator, ParseCoordinatorDeps};

pub(crate) struct InstallCoordinatorDeps {
    pub(crate) client: Client,
    pub(crate) language: std::sync::Arc<LanguageCoordinator>,
    pub(crate) parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
    pub(crate) documents: std::sync::Arc<DocumentStore>,
    pub(crate) cache: std::sync::Arc<CacheCoordinator>,
    pub(crate) settings_manager: std::sync::Arc<SettingsManager>,
    pub(crate) auto_install: AutoInstallManager,
    pub(crate) bridge: std::sync::Arc<BridgeCoordinator>,
}

pub(crate) struct InstallCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
    documents: std::sync::Arc<DocumentStore>,
    cache: std::sync::Arc<CacheCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    auto_install: AutoInstallManager,
    bridge: std::sync::Arc<BridgeCoordinator>,
}

impl InstallCoordinator {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self::from_parts(InstallCoordinatorDeps {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            parser_pool: std::sync::Arc::clone(&server.parser_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            auto_install: server.auto_install.clone(),
            bridge: std::sync::Arc::clone(&server.bridge),
        })
    }

    pub(crate) fn from_parts(deps: InstallCoordinatorDeps) -> Self {
        Self {
            client: deps.client,
            language: deps.language,
            parser_pool: deps.parser_pool,
            documents: deps.documents,
            cache: deps.cache,
            settings_manager: deps.settings_manager,
            auto_install: deps.auto_install,
            bridge: deps.bridge,
        }
    }

    /// Dispatch install events to ClientNotifier.
    ///
    /// This method bridges AutoInstallManager (isolated) with ClientNotifier.
    /// AutoInstallManager returns events, Kakehashi dispatches them.
    pub(crate) async fn dispatch_install_events(&self, language: &str, events: &[InstallEvent]) {
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

    /// Build a human-readable reason why auto-install is disabled.
    pub(crate) fn auto_install_disabled_reason(&self) -> String {
        let settings = self.settings_manager.load_settings();
        if !settings.auto_install {
            return "autoInstall is disabled".to_string();
        }
        if !self
            .settings_manager
            .search_paths_include_default_data_dir(&settings.search_paths)
        {
            let default_dir = crate::install::default_data_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            return format!(
                "searchPaths does not include the default data directory ({})",
                default_dir
            );
        }
        "unknown reason".to_string()
    }

    /// Notify user that parser is missing and needs manual installation.
    ///
    /// Called when a parser fails to load and auto-install is disabled
    /// (either explicitly or because searchPaths doesn't include the default data dir).
    pub(crate) async fn notify_parser_missing(&self, language: &str, reason: &str) {
        self.notifier()
            .log_warning(format!(
                "Parser for '{}' not found. Auto-install is disabled because {}. \
                 Please install the parser manually using: kakehashi language install {}",
                language, reason, language
            ))
            .await;
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
    pub(crate) async fn maybe_auto_install_language(
        &self,
        language: &str,
        uri: Url,
        text: String,
        is_injection: bool,
    ) -> bool {
        let result = self.auto_install.try_install(language).await;

        self.dispatch_install_events(language, &result.events).await;

        if let Some(data_dir) = result.outcome.data_dir() {
            self.reload_language_after_install(language, data_dir, uri, text, is_injection)
                .await;
            return true;
        }

        result.outcome.should_skip_parse()
    }

    /// Reload a language after installation and optionally re-parse the document.
    pub(crate) async fn reload_language_after_install(
        &self,
        language: &str,
        data_dir: &std::path::Path,
        uri: Url,
        text: String,
        is_injection: bool,
    ) {
        let current_settings = self.settings_manager.load_settings();
        let mut updated_settings = (*current_settings).clone();

        let data_dir_str = data_dir.to_string_lossy().to_string();
        if !updated_settings.search_paths.contains(&data_dir_str) {
            updated_settings.search_paths.push(data_dir_str);
        }

        self.apply_settings(updated_settings).await;

        let load_result = self.language.ensure_language_loaded(language);
        self.notifier()
            .log_language_events(&load_result.events)
            .await;

        if !is_injection {
            let host_language = self.get_language_for_document(&uri);
            let lang_for_parse = host_language.as_deref();
            let parse_coordinator = self.parse_coordinator();
            parse_coordinator
                .parse_document(uri.clone(), text, lang_for_parse, vec![])
                .await;
        }
    }

    async fn apply_settings(&self, settings: WorkspaceSettings) {
        apply_shared_settings(
            &self.client,
            &self.language,
            &self.settings_manager,
            settings,
        )
        .await;
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }

    fn get_language_for_document(&self, uri: &Url) -> Option<String> {
        detect_document_language(&self.language, &self.documents, uri)
    }

    fn parse_coordinator(&self) -> ParseCoordinator {
        ParseCoordinator::from_parts(ParseCoordinatorDeps {
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
