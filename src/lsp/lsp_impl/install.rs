//! Parser install and post-install reload orchestration.

use url::Url;

use super::Kakehashi;
use crate::lsp::auto_install::InstallEvent;

impl Kakehashi {
    /// Dispatch install events to ClientNotifier.
    ///
    /// This method bridges AutoInstallManager (isolated) with ClientNotifier.
    /// AutoInstallManager returns events, Kakehashi dispatches them.
    pub(super) async fn dispatch_install_events(&self, language: &str, events: &[InstallEvent]) {
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
    pub(super) fn auto_install_disabled_reason(&self) -> String {
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
    pub(super) async fn notify_parser_missing(&self, language: &str, reason: &str) {
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
    pub(super) async fn maybe_auto_install_language(
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
    pub(super) async fn reload_language_after_install(
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
        self.notifier()
            .log_language_events(&load_result.events)
            .await;

        // For document languages, re-parse the document that triggered the install.
        // For injection languages, DON'T re-parse - the host document is already parsed
        // with the correct language. Re-parsing with the injection language would break
        // all highlighting. The SemanticTokensRefresh event above will notify the client.
        if !is_injection {
            // Get the host language for this document (not the installed language)
            let host_language = self.parse_coordinator().get_language_for_document(&uri);
            let lang_for_parse = host_language.as_deref();
            self.parse_coordinator()
                .parse_document(uri.clone(), text, lang_for_parse, vec![])
                .await;
        }
    }
}
