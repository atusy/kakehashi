use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use url::Url;

use crate::config::WorkspaceSettings;
use crate::lsp::auto_install::{InstallEvent, InstallResult};
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use crate::lsp::lsp_impl::{
    Kakehashi, ReloadLanguageState, SettingsReloadInput, apply_shared_settings_locked,
    build_notifier, lock_settings_reload,
};
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;

use super::ParseCoordinator;
use super::parse::ParseCoordinatorDeps;

fn updated_settings_after_install(
    raw_settings: &crate::config::RawWorkspaceSettings,
    settings: &WorkspaceSettings,
    data_dir: &std::path::Path,
) -> (crate::config::RawWorkspaceSettings, WorkspaceSettings) {
    let mut updated_settings = settings.clone();
    let mut updated_raw_settings = raw_settings.clone();
    let data_dir_str = data_dir.to_string_lossy().to_string();
    if !updated_settings.search_paths.contains(&data_dir_str) {
        updated_settings.search_paths.push(data_dir_str.clone());

        let raw_search_paths = updated_raw_settings
            .search_paths
            .get_or_insert_with(Default::default);
        if !raw_search_paths.contains(&data_dir_str) {
            raw_search_paths.push(data_dir_str);
        }
    }

    (updated_raw_settings, updated_settings)
}

pub(super) struct InstallCoordinatorDeps {
    pub(super) client: Client,
    pub(super) language: std::sync::Arc<LanguageCoordinator>,
    pub(super) parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    pub(super) compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    pub(super) documents: std::sync::Arc<DocumentStore>,
    pub(super) cache: std::sync::Arc<CacheCoordinator>,
    pub(super) settings_manager: std::sync::Arc<SettingsManager>,
    pub(super) auto_install: AutoInstallManager,
    pub(super) bridge: std::sync::Arc<BridgeCoordinator>,
}

pub(crate) struct InstallCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
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
            compute_pool: std::sync::Arc::clone(&server.compute_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            auto_install: server.auto_install.clone(),
            bridge: std::sync::Arc::clone(&server.bridge),
        })
    }

    pub(super) fn from_parts(deps: InstallCoordinatorDeps) -> Self {
        Self {
            client: deps.client,
            language: deps.language,
            parser_pool: deps.parser_pool,
            compute_pool: deps.compute_pool,
            documents: deps.documents,
            cache: deps.cache,
            settings_manager: deps.settings_manager,
            auto_install: deps.auto_install,
            bridge: deps.bridge,
        }
    }

    /// Dispatch install events to ClientNotifier.
    ///
    /// Bridges the isolated `AutoInstallManager` (which only returns events) to the
    /// `ClientNotifier` that performs the actual client-facing side effects.
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
    /// Delegates to `AutoInstallManager::try_install()`, dispatches its events, and
    /// reloads on success. An `AlreadyInstalling` caller waits for the shared
    /// install claim, then reloads its own document when the parser artifact exists.
    pub(crate) async fn maybe_auto_install_language(
        &self,
        language: &str,
        uri: Url,
        is_injection: bool,
        expected_incarnation: Option<u64>,
        allow_recovery: bool,
    ) -> bool {
        if self.language.has_parser_available(language) {
            if !is_injection && self.same_document_incarnation(&uri, expected_incarnation) {
                self.parse_coordinator()
                    .reparse_installed_document(uri.clone(), language, expected_incarnation)
                    .await;
            }
            if !self.same_document_incarnation(&uri, expected_incarnation) {
                return false;
            }
            let recovered =
                self.install_reparse_recovered(language, &uri, is_injection, expected_incarnation);
            if recovered {
                return true;
            }
        }
        let mut result = self.auto_install.try_install(language).await;

        self.dispatch_install_events(language, &result.events).await;

        if let Some(data_dir) = result.outcome.data_dir().cloned() {
            self.reload_language_after_install(
                language,
                &data_dir,
                uri.clone(),
                is_injection,
                expected_incarnation,
                Some(&mut result),
            )
            .await;
            let recovered =
                self.install_reparse_recovered(language, &uri, is_injection, expected_incarnation);
            if recovered {
                return true;
            }
            drop(result);
            return false;
        }

        // Every no-reparse outcome lands here — Failed/Unsupported/NoDataDir/
        // ParserFailed (the did_open auto-install branch never runs
        // parse_document regardless) as well as AlreadyInstalling. None of
        // them publishes a snapshot anywhere
        // below, so release a parked first-parse waiter with a tree-less
        // snapshot (bootstrap-gated inside) instead of letting every request
        // burn the full first-parse backstop. Harmless for AlreadyInstalling:
        // its eventual reload-reparse lands the same-version tree through the
        // snapshot cell's tree-upgrade clause.
        self.documents.publish_giveup_snapshot(&uri);
        if result.outcome == crate::lsp::auto_install::InstallOutcome::AlreadyInstalling {
            let completion_token = result
                .completion
                .clone()
                .expect("duplicate install has claim token");
            let mut completion = completion_token.receiver.clone();
            let terminal = loop {
                if let Some(outcome) = completion.borrow().clone() {
                    break outcome;
                }
                if completion.changed().await.is_err() {
                    break crate::lsp::auto_install::InstallOutcome::Failed;
                }
            };
            if terminal.data_dir().is_some()
                && self.same_document_incarnation(&uri, expected_incarnation)
            {
                if !is_injection {
                    self.parse_coordinator()
                        .reparse_installed_document(uri.clone(), language, expected_incarnation)
                        .await;
                }
                if self.install_reparse_recovered(
                    language,
                    &uri,
                    is_injection,
                    expected_incarnation,
                ) {
                    return true;
                }
                if allow_recovery {
                    return Box::pin(self.maybe_auto_install_language(
                        language,
                        uri,
                        is_injection,
                        expected_incarnation,
                        false,
                    ))
                    .await;
                }
                return false;
            }
        } else {
            result.complete_claim();
        }
        self.same_document_incarnation(&uri, expected_incarnation)
    }

    /// Reload a language after installation and optionally re-parse the document.
    ///
    /// The re-parse re-reads the latest store text itself
    /// ([`reparse_installed_document`](ParseCoordinator::reparse_installed_document)),
    /// so no open-time text is threaded here.
    pub(crate) async fn reload_language_after_install(
        &self,
        language: &str,
        data_dir: &std::path::Path,
        uri: Url,
        is_injection: bool,
        expected_incarnation: Option<u64>,
        claim: Option<&mut InstallResult>,
    ) {
        let reload = lock_settings_reload().await;
        let settings_snapshot = self.settings_manager.load_settings_pair();
        let (updated_raw_settings, updated_settings) = updated_settings_after_install(
            &settings_snapshot.raw_settings,
            &settings_snapshot.settings,
            data_dir,
        );

        self.apply_raw_settings_locked(&reload, updated_raw_settings, updated_settings)
            .await;

        let load_result = self.language.ensure_language_loaded_async(language).await;
        let global_loaded = self.language.has_parser_available(language);
        if global_loaded && let Some(claim) = claim {
            claim.complete_claim();
        }
        drop(reload);

        self.notifier()
            .log_language_events(&load_result.events)
            .await;

        if global_loaded && !is_injection {
            // Resurrection-safe, off-ingress reparse: re-detects the language from
            // the current document lifetime and persists through a non-inserting
            // CAS, so a didClose/reopen during the install can't receive a tree for
            // the old language. A host waiter whose install claim was won by an
            // injection reaches this same per-URI path after the claim completes.
            self.parse_coordinator()
                .reparse_installed_document(uri, language, expected_incarnation)
                .await;
        }
    }

    async fn apply_raw_settings_locked(
        &self,
        reload: &tokio::sync::MutexGuard<'static, ()>,
        raw_settings: crate::config::RawWorkspaceSettings,
        settings: WorkspaceSettings,
    ) {
        let _reparse_uris = apply_shared_settings_locked(
            reload,
            &self.client,
            ReloadLanguageState {
                language: &self.language,
                parser_pool: &self.parser_pool,
                documents: &self.documents,
                invalidate_documents: false,
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
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }

    fn same_document_incarnation(&self, uri: &Url, expected: Option<u64>) -> bool {
        expected.is_some() && self.documents.get(uri).map(|doc| doc.incarnation()) == expected
    }

    fn install_reparse_recovered(
        &self,
        language: &str,
        uri: &Url,
        is_injection: bool,
        expected_incarnation: Option<u64>,
    ) -> bool {
        self.same_document_incarnation(uri, expected_incarnation)
            && self.language.has_parser_available(language)
            && (is_injection
                || self
                    .documents
                    .get(uri)
                    .is_some_and(|document| document.tree().is_some()))
    }

    fn parse_coordinator(&self) -> ParseCoordinator {
        ParseCoordinator::from_parts(ParseCoordinatorDeps {
            client: self.client.clone(),
            language: std::sync::Arc::clone(&self.language),
            parser_pool: std::sync::Arc::clone(&self.parser_pool),
            compute_pool: std::sync::Arc::clone(&self.compute_pool),
            documents: std::sync::Arc::clone(&self.documents),
            cache: std::sync::Arc::clone(&self.cache),
            settings_manager: std::sync::Arc::clone(&self.settings_manager),
            auto_install: self.auto_install.clone(),
            bridge: std::sync::Arc::clone(&self.bridge),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LanguageSettings, RawWorkspaceSettings, WILDCARD_KEY, WorkspaceSettings};
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::Path;
    use std::task::Poll;
    use tower_lsp_server::LspService;

    #[test]
    fn reload_after_install_preserves_explicit_matching_override_in_raw_settings() {
        let raw_settings = RawWorkspaceSettings {
            search_paths: Some(vec!["/existing".to_string()]),
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            crate::config::settings::BridgeLanguageConfig {
                                enabled: Some(true),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
                (
                    "r".to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            crate::config::settings::BridgeLanguageConfig {
                                enabled: Some(true),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let settings = WorkspaceSettings::try_from_settings(&raw_settings, None, |_| None).unwrap();

        let (updated_raw, updated_settings) =
            updated_settings_after_install(&raw_settings, &settings, Path::new("/installed"));

        assert_eq!(
            updated_raw.languages["r"].bridge.as_ref().unwrap()["python"].enabled,
            Some(true)
        );
        assert_eq!(
            updated_raw.search_paths,
            Some(vec!["/existing".to_string(), "/installed".to_string()])
        );
        assert_eq!(
            updated_settings.search_paths,
            vec!["/existing".to_string(), "/installed".to_string()]
        );
    }

    #[test]
    fn reload_after_install_preserves_raw_search_path_template_when_not_modified() {
        let raw_settings = RawWorkspaceSettings {
            search_paths: Some(vec!["${KAKEHASHI_DATA_DIR}".to_string()]),
            ..Default::default()
        };
        let settings = WorkspaceSettings {
            search_paths: vec!["/installed".to_string()],
            ..Default::default()
        };

        let (updated_raw, updated_settings) =
            updated_settings_after_install(&raw_settings, &settings, Path::new("/installed"));

        assert_eq!(
            updated_raw.search_paths,
            Some(vec!["${KAKEHASHI_DATA_DIR}".to_string()])
        );
        assert_eq!(
            updated_settings.search_paths,
            vec!["/installed".to_string()]
        );
    }

    #[test]
    fn reload_after_install_appends_data_dir_to_raw_search_paths_without_expanding_templates() {
        let raw_settings = RawWorkspaceSettings {
            search_paths: Some(vec![
                "${KAKEHASHI_DATA_DIR}".to_string(),
                "/custom".to_string(),
            ]),
            ..Default::default()
        };
        let settings = WorkspaceSettings {
            search_paths: vec!["/expanded".to_string(), "/custom".to_string()],
            ..Default::default()
        };

        let (updated_raw, updated_settings) =
            updated_settings_after_install(&raw_settings, &settings, Path::new("/installed"));

        assert_eq!(
            updated_raw.search_paths,
            Some(vec![
                "${KAKEHASHI_DATA_DIR}".to_string(),
                "/custom".to_string(),
                "/installed".to_string(),
            ])
        );
        assert_eq!(
            updated_settings.search_paths,
            vec![
                "/expanded".to_string(),
                "/custom".to_string(),
                "/installed".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn reload_after_install_reparses_the_requesting_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let first = Url::parse("file:///workspace/first.rs").unwrap();
        server.documents.insert(
            first.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        server
            .install_coordinator()
            .reload_language_after_install(
                "rust",
                Path::new("/installed"),
                first.clone(),
                false,
                server.documents.get(&first).map(|doc| doc.incarnation()),
                None,
            )
            .await;

        assert!(
            server.documents.get(&first).unwrap().tree().is_some(),
            "the document that won the install should be reparsed"
        );
    }

    #[tokio::test]
    async fn reload_after_install_merges_into_settings_published_while_waiting() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let initial_raw = RawWorkspaceSettings {
            search_paths: Some(vec!["/initial".to_string()]),
            ..Default::default()
        };
        let initial = WorkspaceSettings {
            search_paths: vec!["/initial".to_string()],
            ..Default::default()
        };
        server
            .settings_manager
            .apply_settings_with_raw(initial_raw, initial);

        let reload_guard = lock_settings_reload().await;
        let install = server.install_coordinator();
        let mut reload = Box::pin(install.reload_language_after_install(
            "test-language",
            Path::new("/installed"),
            Url::parse("file:///workspace/test.txt").unwrap(),
            true,
            None,
            None,
        ));
        std::future::poll_fn(|cx| {
            assert!(reload.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        let newer_raw = RawWorkspaceSettings {
            search_paths: Some(vec!["/newer".to_string()]),
            ..Default::default()
        };
        let newer = WorkspaceSettings {
            search_paths: vec!["/newer".to_string()],
            ..Default::default()
        };
        server
            .settings_manager
            .apply_settings_with_raw(newer_raw, newer);
        drop(reload_guard);
        reload.await;

        let snapshot = server.settings_manager.load_settings_pair();
        assert_eq!(
            snapshot.raw_settings.search_paths,
            Some(vec!["/newer".to_string(), "/installed".to_string()])
        );
        assert_eq!(
            snapshot.settings.search_paths,
            vec!["/newer".to_string(), "/installed".to_string()]
        );
    }

    #[tokio::test]
    async fn reload_does_not_attach_installed_language_tree_to_relabelled_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        server
            .language
            .language_registry_for_parallel()
            .register("go".to_string(), tree_sitter_go::LANGUAGE.into());
        let uri = Url::parse("file:///workspace/reopened.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "package main".to_string(),
            Some("go".to_string()),
            None,
        );

        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", None)
            .await;

        assert!(server.documents.get(&uri).unwrap().tree().is_none());
    }

    #[tokio::test]
    async fn old_install_task_rejects_close_reopen_with_the_same_language() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///workspace/reopened.rs").unwrap();
        let original = server.documents.insert(
            uri.clone(),
            "fn old() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        server.documents.remove(&uri);
        server.documents.insert(
            uri.clone(),
            "fn new() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", Some(original))
            .await;

        assert!(server.documents.get(&uri).unwrap().tree().is_none());
    }
}
