use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use crate::text::terminal::escape_terminal_controls;
use url::Url;

use crate::config::WorkspaceSettings;
use crate::lsp::auto_install::{InstallEvent, InstallResult};
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use crate::lsp::lsp_impl::{
    Kakehashi, ReloadLanguageState, ReloadTrigger, SettingsReloadInput,
    apply_shared_settings_locked, build_notifier, lock_settings_reload,
};
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;

use super::ParseCoordinator;
use super::parse::ParseCoordinatorDeps;

/// A query-dependency check that is due: the generation it was recorded in,
/// and the repair-failure revision its answer is judged against.
#[derive(Clone, Copy)]
struct QueryRepairCheck {
    generation: u64,
    failure_revision: u64,
}

fn query_dependency_paths(settings: &WorkspaceSettings, language: &str) -> Vec<std::path::PathBuf> {
    // The loader returns after loading an explicit list (including an empty
    // one), so runtime files for this root language are not dependency inputs.
    if settings
        .languages
        .get(language)
        .is_some_and(|config| config.queries.is_some())
    {
        return Vec::new();
    }
    settings
        .search_paths
        .iter()
        .map(std::path::PathBuf::from)
        .collect()
}

/// What a lifecycle probe found for a managed language's query chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryChainState {
    /// Complete, or not a query-repair target at all.
    Settled,
    /// The managed parser's query chain is missing a language.
    NeedsRepair,
    /// A language in the chain is locked, so the chain cannot be judged now.
    Busy,
}

fn managed_query_chain_state(
    settings: &WorkspaceSettings,
    language: &str,
    data_dir: &std::path::Path,
) -> QueryChainState {
    if !settings.auto_install_for(language) {
        return QueryChainState::Settled;
    }
    let paths = query_dependency_paths(settings, language);
    if paths.is_empty() {
        return QueryChainState::Settled;
    }
    let parser_config = settings
        .languages
        .get(language)
        .and_then(|config| config.parser.as_deref());
    let selected = crate::language::query_loader::QueryLoader::resolve_library_path(
        parser_config,
        language,
        &paths,
    );
    let managed = crate::install::parser_file_exists(language, data_dir);
    // A stale managed copy must not turn a custom parser into an install target.
    let same_parser = selected
        .and_then(|path| path.canonicalize().ok())
        .zip(managed.and_then(|path| path.canonicalize().ok()))
        .is_some_and(|(selected, managed)| selected == managed);
    if !same_parser {
        return QueryChainState::Settled;
    }
    match crate::install::queries::probe_chain(data_dir, language, &paths) {
        crate::install::queries::ChainProbe::Complete(_) => QueryChainState::Settled,
        crate::install::queries::ChainProbe::Incomplete => QueryChainState::NeedsRepair,
        crate::install::queries::ChainProbe::Busy => QueryChainState::Busy,
    }
}

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

        // Raw settings are re-expanded by everything that reads them — a
        // `didChangeConfiguration` merges onto this snapshot and converts it
        // again — so the directory has to be spelled so that expanding it gives
        // it back. A literal `$` takes the documented escape; a leading `~`
        // (reachable with a quoted `--data-dir '~/data'`, naming a directory
        // actually called `~`) is put out of reach of tilde expansion by a `./`
        // prefix, since the escape does not apply to it. The expanded copy above
        // keeps the real name either way.
        let mut raw_data_dir = data_dir_str.replace('$', "$$");
        if raw_data_dir.starts_with('~') {
            raw_data_dir.insert_str(0, "./");
        }
        let raw_search_paths = updated_raw_settings
            .search_paths
            .get_or_insert_with(Default::default);
        if !raw_search_paths.contains(&raw_data_dir) {
            raw_search_paths.push(raw_data_dir);
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
    pub(super) shutdown: tokio_util::sync::CancellationToken,
}

/// Preserve the caller's query-repair decision across publication races.
#[derive(Clone, Copy)]
pub(crate) struct InstallRequest {
    repair_queries: bool,
    /// The requester already had a usable parser, so its own parse (or the
    /// host's existing tree) publishes this document's first snapshot.
    parser_loaded: bool,
    allow_recovery: bool,
}

impl InstallRequest {
    pub(crate) fn new(repair_queries: bool) -> Self {
        Self {
            repair_queries,
            parser_loaded: repair_queries,
            allow_recovery: true,
        }
    }
}

/// Installation/lifetime eligibility is distinct from ownership of a parse.
/// A sibling may supply the tree while this install is waiting.
#[derive(Default)]
pub(crate) struct InstallCompletion {
    pub(crate) same_lifetime: bool,
    pub(crate) parsed: Option<super::parse::ParseLineage>,
    queries_reloaded: bool,
}

impl InstallCompletion {
    /// Reloading queries authorizes a fresh downstream pass even when the
    /// current tree was retained. A plain parser recovery still owns no pass.
    pub(crate) fn downstream_lineage(
        &self,
        documents: &DocumentStore,
        uri: &Url,
        incarnation: u64,
    ) -> Option<super::parse::ParseLineage> {
        if let Some(parsed) = self.parsed {
            return Some(parsed);
        }
        if !self.queries_reloaded {
            return None;
        }
        let document = documents.get(uri)?;
        (document.incarnation() == incarnation && document.has_current_tree()).then(|| {
            super::parse::ParseLineage {
                incarnation,
                content_version: document.content_version(),
            }
        })
    }
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
    shutdown: tokio_util::sync::CancellationToken,
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
            shutdown: server.shutdown_token.clone(),
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
            shutdown: deps.shutdown,
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

    /// Build a human-readable reason why auto-install is disabled for
    /// `language`.
    ///
    /// Points at the config that decided it: exactly for the wildcard and
    /// top-level cases, hedged for a per-language entry, where the base-chain
    /// fold makes the original spelling unknowable. See
    /// [`crate::config::WorkspaceSettings::auto_install_disabled_reason`].
    pub(crate) fn auto_install_disabled_reason(&self, language: &str) -> String {
        let settings = self.settings_manager.load_settings();
        if let Some(reason) = settings.auto_install_disabled_reason(language) {
            return reason;
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
                "Parser for '{}' is unavailable. Auto-install is disabled because {}. \
                 Please install the parser manually using: kakehashi language install {}",
                escape_terminal_controls(language),
                escape_terminal_controls(reason),
                escape_terminal_controls(language)
            ))
            .await;
    }

    /// Whether an already-loaded managed parser's query chain should be
    /// repaired now. `initial_pass` marks lifecycle passes (open, install
    /// completion) that check regardless of this generation's earlier checks.
    ///
    /// The probe resolves the parser path, canonicalizes, takes lock files and
    /// reads modelines across every search path, so it runs on the blocking
    /// pool rather than on an async worker.
    pub(crate) async fn query_repair_needed(&self, language: &str, initial_pass: bool) -> bool {
        let Some(check) = self.begin_query_repair_check(language, initial_pass) else {
            return false;
        };
        let settings = self.settings_manager.load_settings();
        let probe_language = language.to_string();
        let state = tokio::task::spawn_blocking(move || {
            crate::install::default_data_dir().map_or(QueryChainState::Settled, |data_dir| {
                managed_query_chain_state(&settings, &probe_language, &data_dir)
            })
        })
        .await
        .unwrap_or_else(|error| {
            // A probe that panics would panic again on the next pass; reading
            // it as busy would re-arm the check and respawn it every edit.
            log::warn!(
                target: "kakehashi::install",
                "Query dependency check for {language:?} did not finish: {error}"
            );
            QueryChainState::Settled
        });
        self.finish_query_repair_check(language, check, state)
    }

    #[cfg(test)]
    fn decide_query_repair(
        &self,
        language: &str,
        initial_pass: bool,
        probe: impl FnOnce() -> QueryChainState,
    ) -> bool {
        self.begin_query_repair_check(language, initial_pass)
            .is_some_and(|check| self.finish_query_repair_check(language, check, probe()))
    }

    /// The generation to probe in, or `None` when no probe is due.
    fn begin_query_repair_check(
        &self,
        language: &str,
        initial_pass: bool,
    ) -> Option<QueryRepairCheck> {
        // Discovery may already have loaded the parser before this task runs.
        // Track checks independently of load events; reload generations reset
        // eligibility while steady-state edits do no dependency filesystem
        // work. Opening another file does not change why the last repair
        // failed; a reload (settings change or any successful install) retries
        // it. The generation returned is the one the check was recorded in, so
        // a busy answer undoes this very mark even when a reload lands between.
        //
        // The memo goes first: on an edit pass it answers without the settings
        // and data-directory work the auto-install check does. A mark taken
        // while auto-install is off is harmless; enabling it is a settings
        // change, which starts a new generation.
        let generation = self.cache.semantic_token_generation();
        // Read before the check is recorded: a failure landing after this is
        // newer than whatever the probe answers.
        let failure_revision = self.auto_install.query_repair_revision();
        (self
            .auto_install
            .begin_query_dependency_check(language, generation, initial_pass)
            && self.settings_manager.is_auto_install_enabled(language))
        .then_some(QueryRepairCheck {
            generation,
            failure_revision,
        })
    }

    /// A repair a pass decided on but dropped before installing, because its
    /// document's lifetime ended, answered nothing for the language: let a
    /// later pass of another document check it again this generation.
    fn release_dropped_repair(&self, language: &str, generation: u64) {
        self.auto_install
            .forget_query_dependency_check(language, generation);
        self.auto_install.defer_query_repair_retry(language);
    }

    fn finish_query_repair_check(
        &self,
        language: &str,
        check: QueryRepairCheck,
        state: QueryChainState,
    ) -> bool {
        // Probes run concurrently; a repair that failed while this one read
        // the chain has already answered. Judged in the current generation: a
        // reload during the probe retires failures from the probe's own, and a
        // failure after it is the one that counts. An answer that stands also
        // ends the failed repair's wait for a retry.
        let failed_meanwhile = || {
            self.auto_install
                .query_repair_failed(language, self.cache.semantic_token_generation())
        };
        match state {
            QueryChainState::NeedsRepair => {
                let admitted = !failed_meanwhile();
                if admitted {
                    self.auto_install
                        .resolve_query_repair_retry(language, check.failure_revision);
                }
                admitted
            }
            QueryChainState::Settled => {
                if !failed_meanwhile() {
                    self.auto_install
                        .resolve_query_repair_retry(language, check.failure_revision);
                }
                false
            }
            // A lock held exclusively by an install (staging or publishing)
            // or an uninstall is not evidence of a missing language. Leave the answer
            // to a later pass instead of spawning an install that would find
            // nothing to do and still reload every document's queries.
            QueryChainState::Busy => {
                self.auto_install
                    .forget_query_dependency_check(language, check.generation);
                // Nothing answered: whatever the lock was hiding still waits
                // for a check, which a configuration push must not skip.
                self.auto_install.defer_query_repair_retry(language);
                false
            }
        }
    }

    /// Try to auto-install a language if not already being installed.
    ///
    /// Delegates to `AutoInstallManager::try_install()`, dispatches its events, and
    /// reloads on success. An `AlreadyInstalling` caller waits for the shared
    /// install claim, then reloads its own document when the parser artifact exists.
    /// `parsed` names only a parse published by this call; successful recovery
    /// through a sibling's tree does not authorize open downstream work.
    pub(crate) async fn maybe_auto_install_language(
        &self,
        language: &str,
        uri: Url,
        is_injection: bool,
        expected_incarnation: Option<u64>,
        request: InstallRequest,
    ) -> InstallCompletion {
        let mut parsed = None;
        // A failure is remembered for the generation it was attempted in, so
        // a reload that lands meanwhile already counts as the retry trigger.
        let generation = self.cache.semantic_token_generation();
        if !self.same_document_incarnation(&uri, expected_incarnation) {
            if request.repair_queries {
                self.release_dropped_repair(language, generation);
            }
            return InstallCompletion::default();
        }

        let parser_available = self.language.has_parser_available(language);
        let query_repair = request.repair_queries
            || (parser_available && self.query_repair_needed(language, true).await);
        let request = InstallRequest {
            repair_queries: query_repair,
            ..request
        };
        // A usable parser parses now. A repair requested with a loaded parser
        // leaves that to the caller's own parse; one that started as a parser
        // install had its caller skip parsing, and a failed repair must not
        // leave the document tree-less.
        if parser_available && (!query_repair || !request.parser_loaded) {
            if !is_injection && self.same_document_incarnation(&uri, expected_incarnation) {
                parsed = self
                    .parse_coordinator()
                    .reparse_installed_document(uri.clone(), language, expected_incarnation)
                    .await;
            }
            if !self.same_document_incarnation(&uri, expected_incarnation) {
                if query_repair {
                    self.release_dropped_repair(language, generation);
                }
                return InstallCompletion::default();
            }
        }
        if parser_available && !query_repair {
            let recovered =
                self.install_reparse_recovered(language, &uri, is_injection, expected_incarnation);
            if recovered {
                return InstallCompletion {
                    same_lifetime: true,
                    parsed,
                    queries_reloaded: false,
                };
            }
        }
        let search_paths = query_dependency_paths(&self.settings_manager.load_settings(), language);
        let mut result = self.auto_install.try_install(language, search_paths).await;

        self.dispatch_install_events(language, &result.events).await;

        if let Some(data_dir) = result.outcome.data_dir().cloned() {
            parsed = self
                .reload_language_after_install(
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
                return InstallCompletion {
                    same_lifetime: true,
                    parsed,
                    queries_reloaded: true,
                };
            }
            drop(result);
            return InstallCompletion::default();
        }

        // Every no-reparse outcome lands here — Failed/Unsupported/NoDataDir as
        // well as AlreadyInstalling. (`Abandoned` never reaches this point: it
        // is produced by `InstallMarkerGuard::drop` into the watch channel, so
        // it surfaces only as the `terminal` read below.) None of them publishes
        // a snapshot anywhere below, so release a parked first-parse waiter with
        // a tree-less snapshot (bootstrap-gated inside) instead of letting
        // every request
        // burn the full first-parse backstop. Harmless for AlreadyInstalling:
        // its eventual reload-reparse lands the same-version tree through the
        // snapshot cell's tree-upgrade clause.
        // A repair requested with a usable parser is the exception: the
        // caller's own parse publishes the tree, and a give-up landing first
        // would hand its parked readers a tree-less snapshot. A parser install
        // that became a repair still owns the release: its caller skipped
        // the inline parse.
        if let Some(expected_incarnation) = expected_incarnation
            && !request.parser_loaded
        {
            self.documents
                .publish_giveup_snapshot(&uri, expected_incarnation);
        }
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
            if let Some(data_dir) = terminal.data_dir()
                && self.same_document_incarnation(&uri, expected_incarnation)
            {
                if query_repair && !completion_token.owner_settled() {
                    // A cancelled owner may publish its successful install
                    // outcome before reloading. Refresh explicitly rather than
                    // treating a terminal artifact outcome as a query-store ack.
                    // An owner that completed the claim itself already
                    // reloaded; repeating it would refresh every document again.
                    parsed = self
                        .reload_language_after_install(
                            language,
                            data_dir,
                            uri.clone(),
                            is_injection,
                            expected_incarnation,
                            None,
                        )
                        .await;
                } else if !is_injection {
                    parsed = self
                        .parse_coordinator()
                        .reparse_installed_document(uri.clone(), language, expected_incarnation)
                        .await;
                }
                if self.install_reparse_recovered(
                    language,
                    &uri,
                    is_injection,
                    expected_incarnation,
                ) {
                    return InstallCompletion {
                        same_lifetime: true,
                        parsed,
                        queries_reloaded: query_repair,
                    };
                }
                if request.allow_recovery {
                    // The queries were reloaded (by the owner or above); the
                    // retry only needs a tree. Letting it re-decide keeps a
                    // repair request from owning another install and a second
                    // workspace-wide reload. The reload it would skip already
                    // happened, so its completion still authorizes downstream
                    // work for the repaired queries.
                    let mut retried = Box::pin(self.maybe_auto_install_language(
                        language,
                        uri,
                        is_injection,
                        expected_incarnation,
                        InstallRequest {
                            repair_queries: false,
                            allow_recovery: false,
                            ..request
                        },
                    ))
                    .await;
                    retried.queries_reloaded |= query_repair;
                    return retried;
                }
                return InstallCompletion::default();
            }
            // A shared failure is the owner's to record, in the generation
            // its attempt started in: a waiter joining after a reload must
            // not spend that reload's retry on an attempt from before it.
            if terminal == crate::lsp::auto_install::InstallOutcome::Abandoned
                && self.same_document_incarnation(&uri, expected_incarnation)
                && request.allow_recovery
            {
                return Box::pin(self.maybe_auto_install_language(
                    language,
                    uri,
                    is_injection,
                    expected_incarnation,
                    InstallRequest {
                        allow_recovery: false,
                        ..request
                    },
                ))
                .await;
            }
        } else {
            // Recorded for any owned failure, not only a repair's: waiters
            // repairing through this claim rely on it, and the memo only
            // gates query-repair decisions.
            if result.outcome.is_failure() {
                self.auto_install
                    .record_query_repair_failure(language, generation);
            }
            result.complete_claim();
        }
        InstallCompletion {
            same_lifetime: self.same_document_incarnation(&uri, expected_incarnation),
            parsed,
            queries_reloaded: false,
        }
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
    ) -> Option<super::parse::ParseLineage> {
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
        if !global_loaded && let Some(expected_incarnation) = expected_incarnation {
            self.documents
                .publish_giveup_snapshot(&uri, expected_incarnation);
        }
        if let Some(claim) = claim {
            if !global_loaded {
                claim.outcome = crate::lsp::auto_install::InstallOutcome::Failed;
            }
            claim.complete_claim();
        }
        drop(reload);

        self.notifier()
            .log_language_events(&load_result.events)
            .await;

        if global_loaded && !is_injection {
            // Resurrection-safe, off-ingress reparse: re-detects the language from
            // the current document lifetime and persists through a non-inserting
            // `install_parse`, so a didClose/reopen during the install can't receive a tree for
            // the old language. A host waiter whose install claim was won by an
            // injection reaches this same per-URI path after the claim completes.
            self.parse_coordinator()
                .reparse_installed_document(uri, language, expected_incarnation)
                .await
        } else {
            None
        }
    }

    async fn apply_raw_settings_locked(
        &self,
        reload: &tokio::sync::MutexGuard<'static, ()>,
        raw_settings: crate::config::RawWorkspaceSettings,
        settings: WorkspaceSettings,
    ) {
        let _outcome = apply_shared_settings_locked(
            reload,
            &self.client,
            ReloadLanguageState {
                language: &self.language,
                parser_pool: &self.parser_pool,
                documents: &self.documents,
                trigger: ReloadTrigger::Install,
                reload_required: false,
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
                    .is_some_and(|document| document.has_current_tree()))
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
            bridge: std::sync::Arc::clone(&self.bridge),
            shutdown: self.shutdown.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LanguageSettings, RawWorkspaceSettings, WILDCARD_KEY, WorkspaceSettings};
    use futures::StreamExt;
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::Path;
    use std::task::Poll;
    use tower_lsp_server::LspService;

    #[tokio::test]
    async fn parser_loaded_by_discovery_still_gets_a_first_dependency_check() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        let loaded = server.language.ensure_language_loaded_async("rust").await;
        assert!(loaded.success && loaded.events.is_empty());
        let install = server.install_coordinator();
        let due = |initial_pass| {
            install.decide_query_repair("rust", initial_pass, || QueryChainState::NeedsRepair)
        };
        assert!(due(false), "no load event, yet the first pass checks");
        assert!(!due(false), "an edit pass checks once per generation");
        assert!(due(true), "an initial pass always checks");
        assert!(!due(false));
        server.cache.bump_semantic_token_generation();
        assert!(due(false), "a reload re-arms the check");
    }

    fn auto_install_settings() -> WorkspaceSettings {
        WorkspaceSettings {
            auto_install: true,
            search_paths: vec![
                crate::install::default_data_dir()
                    .expect("test data directory")
                    .to_string_lossy()
                    .into_owned(),
            ],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_busy_chain_is_not_repaired_and_stays_eligible_for_a_recheck() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        let install = server.install_coordinator();
        assert!(server.settings_manager.is_auto_install_enabled("rust"));
        assert!(
            !install.decide_query_repair("rust", false, || QueryChainState::Busy),
            "a held lock is an install at work, not a missing language"
        );
        assert!(
            server.auto_install.has_query_repairs_awaiting_retry(),
            "a busy answer answered nothing, so a configuration push must not skip its retry"
        );
        assert!(
            install.decide_query_repair("rust", false, || QueryChainState::NeedsRepair),
            "a busy answer must not consume this generation's check"
        );
        assert!(
            !install.decide_query_repair("rust", false, || QueryChainState::NeedsRepair),
            "a definitive answer does consume it"
        );
    }

    #[tokio::test]
    async fn a_waiter_leaves_a_shared_failure_to_the_owners_generation() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        let uri = Url::parse("file:///failed-repair.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        // An attempt started before a reload is still running afterwards...
        let claim = server.auto_install.begin_test_claim(
            "rust",
            query_dependency_paths(&server.settings_manager.load_settings(), "rust"),
        );
        server.cache.bump_semantic_token_generation();
        // ...and a repair requested after the reload joins it.
        let install = server.install_coordinator();
        let mut repair = Box::pin(install.maybe_auto_install_language(
            "rust",
            uri.clone(),
            false,
            Some(incarnation),
            InstallRequest::new(true),
        ));
        std::future::poll_fn(|cx| {
            assert!(repair.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        claim.complete(crate::lsp::auto_install::InstallOutcome::Failed);
        repair.await;

        assert!(
            install.decide_query_repair("rust", true, || QueryChainState::NeedsRepair),
            "the reload's retry must not be spent on an attempt from before it"
        );
    }

    #[tokio::test]
    async fn an_owned_failed_query_repair_is_not_retried_until_the_next_reload() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        let uri = Url::parse("file:///owned-failed-repair.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        server
            .auto_install
            .script_next_install("rust", crate::lsp::auto_install::InstallOutcome::Failed);
        let install = server.install_coordinator();
        install
            .maybe_auto_install_language(
                "rust",
                uri.clone(),
                false,
                Some(incarnation),
                InstallRequest::new(true),
            )
            .await;
        assert!(
            !install.decide_query_repair("rust", true, || QueryChainState::NeedsRepair),
            "reopening must not repeat a repair that just failed"
        );
        server.cache.bump_semantic_token_generation();
        assert!(
            install.decide_query_repair("rust", true, || QueryChainState::NeedsRepair),
            "a reload (settings change or a successful install) retries it"
        );
    }

    #[tokio::test]
    async fn a_parser_install_turned_repair_still_parses_with_the_loaded_parser() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        // didOpen found no parser and skipped its inline parse; by the time
        // its task runs the parser is back and the chain needs repair.
        let uri = Url::parse("file:///turned-repair.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        server
            .auto_install
            .script_next_install("rust", crate::lsp::auto_install::InstallOutcome::Failed);
        server
            .install_coordinator()
            .maybe_auto_install_language(
                "rust",
                uri.clone(),
                false,
                Some(incarnation),
                InstallRequest {
                    repair_queries: true,
                    parser_loaded: false,
                    allow_recovery: true,
                },
            )
            .await;
        assert!(
            server.documents.get(&uri).unwrap().tree().is_some(),
            "a failed repair must not leave a document tree-less under a usable parser"
        );
    }

    #[tokio::test]
    async fn a_repair_dropped_for_a_closed_document_rearms_the_check() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        let install = server.install_coordinator();
        // An edit pass found the chain in need of repair...
        assert!(install.decide_query_repair("rust", false, || QueryChainState::NeedsRepair));
        // ...but its document closed before the install could start.
        let closed = Url::parse("file:///closed-before-repair.rs").unwrap();
        install
            .maybe_auto_install_language("rust", closed, true, Some(1), InstallRequest::new(true))
            .await;
        assert!(
            install.decide_query_repair("rust", false, || QueryChainState::NeedsRepair),
            "another document's edit pass must still be able to repair"
        );
    }

    #[tokio::test]
    async fn a_failure_recorded_during_a_probe_declines_its_repair() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        let install = server.install_coordinator();
        let generation = server.cache.semantic_token_generation();
        // A concurrent open's repair fails while this probe is still reading.
        assert!(!install.decide_query_repair("rust", true, || {
            server
                .auto_install
                .record_query_repair_failure("rust", generation);
            QueryChainState::NeedsRepair
        }));
    }

    #[tokio::test]
    async fn a_probe_outlived_by_a_reload_sees_the_newer_failure() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .apply_settings(auto_install_settings());
        let install = server.install_coordinator();
        // A reload lands while the probe reads, and a repair then fails in
        // the new generation.
        assert!(!install.decide_query_repair("rust", true, || {
            server.cache.bump_semantic_token_generation();
            server
                .auto_install
                .record_query_repair_failure("rust", server.cache.semantic_token_generation());
            QueryChainState::NeedsRepair
        }));
    }

    #[tokio::test]
    async fn query_repair_leaves_the_first_snapshot_to_the_inline_parse() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        // didOpen spawns the repair beside the inline parse, which has not
        // published yet.
        let uri = Url::parse("file:///repair-before-parse.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        let claim = server.auto_install.begin_test_claim(
            "rust",
            query_dependency_paths(&server.settings_manager.load_settings(), "rust"),
        );
        let install = server.install_coordinator();
        let mut repair = Box::pin(install.maybe_auto_install_language(
            "rust",
            uri.clone(),
            false,
            Some(incarnation),
            InstallRequest::new(true),
        ));
        std::future::poll_fn(|cx| {
            assert!(repair.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        claim.complete(crate::lsp::auto_install::InstallOutcome::Failed);
        repair.await;
        assert!(
            server
                .documents
                .latest_snapshot(&uri)
                .and_then(|view| view.slot.snapshot)
                .is_none(),
            "a tree-less give-up would wake first-parse readers before the tree lands"
        );
    }

    #[tokio::test]
    async fn a_reloaded_owner_spares_query_repair_waiters_a_second_reload() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///settled-owner.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        let original = server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", Some(incarnation))
            .await
            .expect("initial parse");
        let claim = server.auto_install.begin_test_claim(
            "rust",
            query_dependency_paths(&server.settings_manager.load_settings(), "rust"),
        );
        let install = server.install_coordinator();
        let mut waiter = Box::pin(install.maybe_auto_install_language(
            "rust",
            uri.clone(),
            false,
            Some(incarnation),
            InstallRequest::new(true),
        ));
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let generation = server.cache.semantic_token_generation();
        // An owner completes its claim only after its own reload.
        claim.complete(crate::lsp::auto_install::InstallOutcome::Success {
            data_dir: "/installed".into(),
        });
        let completion = waiter.await;
        assert_eq!(
            server.cache.semantic_token_generation(),
            generation,
            "the owner already reloaded every document's queries"
        );
        assert_eq!(
            completion.downstream_lineage(&server.documents, &uri, incarnation),
            Some(original),
            "the waiter still refreshes its own document's downstream"
        );
    }

    #[tokio::test]
    async fn query_repair_request_survives_a_siblings_publication() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///query-waiter.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        let original = server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", Some(incarnation))
            .await
            .expect("initial parse");
        // This document requested repair before the sibling published. By the
        // time its task runs, the current parser fastpath is otherwise usable.
        let claim = server.auto_install.begin_test_claim(
            "rust",
            query_dependency_paths(&server.settings_manager.load_settings(), "rust"),
        );
        let install = server.install_coordinator();
        let mut waiter = Box::pin(install.maybe_auto_install_language(
            "rust",
            uri.clone(),
            false,
            Some(incarnation),
            InstallRequest::new(true),
        ));
        std::future::poll_fn(|cx| {
            assert!(
                waiter.as_mut().poll(cx).is_pending(),
                "repair intent must bypass the ordinary parser shortcut"
            );
            Poll::Ready(())
        })
        .await;
        // Artifact success is intentionally published without an owner reload.
        let generation = server.cache.semantic_token_generation();
        claim.publish_without_reload(crate::lsp::auto_install::InstallOutcome::Success {
            data_dir: "/installed".into(),
        });
        let completion = waiter.await;
        assert!(
            server.cache.semantic_token_generation() > generation,
            "a cancelled owner's reload falls to the waiter"
        );
        assert!(completion.same_lifetime);
        assert!(
            completion.parsed.is_none(),
            "existing tree should be retained"
        );
        assert_eq!(
            completion.downstream_lineage(&server.documents, &uri, incarnation),
            Some(original)
        );
    }

    #[tokio::test]
    async fn query_reload_authorizes_retained_tree_downstream_only_for_current_lifetime() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///query-repair.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        let original = server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", Some(incarnation))
            .await
            .expect("initial parse");
        let parsed = server
            .install_coordinator()
            .reload_language_after_install(
                "rust",
                Path::new("/installed"),
                uri.clone(),
                false,
                Some(incarnation),
                None,
            )
            .await;
        assert!(parsed.is_none(), "reload retains the current tree");
        let mut completion = InstallCompletion {
            same_lifetime: true,
            parsed,
            queries_reloaded: true,
        };
        assert_eq!(
            completion.downstream_lineage(&server.documents, &uri, incarnation),
            Some(original)
        );
        completion.queries_reloaded = false;
        assert_eq!(
            completion.downstream_lineage(&server.documents, &uri, incarnation),
            None,
            "ordinary shared-parser recovery does not own downstream work"
        );
        completion.queries_reloaded = true;
        server.documents.remove(&uri);
        let reopened = server.documents.insert(
            uri.clone(),
            "fn next() {}".into(),
            Some("rust".into()),
            None,
        );
        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", Some(reopened))
            .await
            .expect("reopened parse");
        assert_eq!(
            completion.downstream_lineage(&server.documents, &uri, incarnation),
            None,
            "a reload must not authorize work for a reopened lifetime"
        );
    }

    #[test]
    fn loaded_managed_parser_still_needs_missing_overlay_parent() {
        let temp = tempfile::TempDir::new().unwrap();
        let data = temp.path().join("data");
        let runtime = temp.path().join("runtime");
        std::fs::create_dir_all(data.join("parser")).unwrap();
        std::fs::create_dir_all(data.join("queries/lua")).unwrap();
        std::fs::create_dir_all(runtime.join("queries/lua")).unwrap();
        let parser = data.join(format!("parser/lua.{}", std::env::consts::DLL_EXTENSION));
        std::fs::write(&parser, "fixture").unwrap();
        std::fs::write(data.join("queries/lua/highlights.scm"), "base").unwrap();
        std::fs::write(
            runtime.join("queries/lua/highlights.scm"),
            ";; extends\n;; inherits: parent\n",
        )
        .unwrap();
        let mut settings = WorkspaceSettings {
            search_paths: vec![
                runtime.to_string_lossy().into(),
                data.to_string_lossy().into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            managed_query_chain_state(&settings, "lua", &data),
            QueryChainState::NeedsRepair
        );
        settings.languages.insert(
            "lua".into(),
            LanguageSettings {
                auto_install: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(
            managed_query_chain_state(&settings, "lua", &data),
            QueryChainState::Settled
        );
        settings.languages.insert(
            "lua".into(),
            LanguageSettings {
                queries: Some(Vec::new()),
                ..Default::default()
            },
        );
        assert_eq!(
            managed_query_chain_state(&settings, "lua", &data),
            QueryChainState::Settled
        );
        settings.languages.clear();
        std::fs::create_dir_all(runtime.join("parser")).unwrap();
        let custom = runtime.join("parser/lua.so");
        std::fs::write(&custom, "custom").unwrap();
        assert_eq!(
            managed_query_chain_state(&settings, "lua", &data),
            QueryChainState::Settled
        );
        std::fs::remove_file(&custom).unwrap();
        settings.languages.insert(
            "lua".into(),
            LanguageSettings {
                parser: Some(custom.to_string_lossy().into()),
                ..Default::default()
            },
        );
        assert_eq!(
            managed_query_chain_state(&settings, "lua", &data),
            QueryChainState::Settled
        );
        settings.languages.clear();
        std::fs::create_dir_all(data.join("queries/parent")).unwrap();
        std::fs::write(data.join("queries/parent/highlights.scm"), "parent").unwrap();
        assert_eq!(
            managed_query_chain_state(&settings, "lua", &data),
            QueryChainState::Settled
        );
    }

    #[test]
    fn explicit_queries_exclude_unused_runtime_dependency_paths() {
        let mut settings = WorkspaceSettings {
            search_paths: vec!["/runtime".into()],
            ..Default::default()
        };
        assert_eq!(
            query_dependency_paths(&settings, "lua"),
            vec![std::path::PathBuf::from("/runtime")]
        );
        settings.languages.insert(
            "lua".into(),
            LanguageSettings {
                queries: Some(Vec::new()),
                ..Default::default()
            },
        );
        assert!(query_dependency_paths(&settings, "lua").is_empty());
        assert!(!query_dependency_paths(&settings, "rust").is_empty());
    }

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

    /// A data directory may contain a `$`. The raw copy is re-expanded by
    /// everything that reads it — a `didChangeConfiguration` merges onto this
    /// snapshot and converts it again — so the literal has to carry the escape
    /// there, while the already-expanded copy keeps the directory's real name.
    #[test]
    fn reload_after_install_escapes_a_dollar_in_the_data_dir_for_the_raw_copy() {
        let raw_settings = RawWorkspaceSettings::default();
        let settings = WorkspaceSettings::default();

        let (updated_raw, updated_settings) =
            updated_settings_after_install(&raw_settings, &settings, Path::new("/data/a$b"));

        assert_eq!(
            updated_raw.search_paths,
            Some(vec!["/data/a$$b".to_string()])
        );
        assert_eq!(updated_settings.search_paths, vec!["/data/a$b".to_string()]);

        // The escape must round-trip: converting the raw copy again gives the
        // real directory back rather than failing on an undefined `$b`.
        let reconverted = WorkspaceSettings::try_from_settings(&updated_raw, None, |_| None)
            .expect("the escaped raw copy must still convert");
        assert_eq!(reconverted.search_paths, vec!["/data/a$b".to_string()]);
    }

    /// `--data-dir '~/data'` names a directory actually called `~`. The escape
    /// for `$` does not apply to a tilde, so the raw copy has to put it out of
    /// expansion's reach another way, or the next configuration update would
    /// silently relocate discovery to the home directory.
    #[test]
    fn reload_after_install_protects_a_literal_tilde_in_the_data_dir() {
        let raw_settings = RawWorkspaceSettings::default();
        let settings = WorkspaceSettings::default();

        let (updated_raw, updated_settings) =
            updated_settings_after_install(&raw_settings, &settings, Path::new("~/data"));

        assert_eq!(updated_settings.search_paths, vec!["~/data".to_string()]);

        let reconverted =
            WorkspaceSettings::try_from_settings(&updated_raw, Some("/home/someone"), |_| None)
                .expect("the raw copy must still convert");
        assert_eq!(
            reconverted.search_paths,
            vec!["./~/data".to_string()],
            "the literal directory survives; it must not become /home/someone/data"
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
    async fn reload_failure_completes_claim_as_failed() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let language = "missing-after-install";
        let uri = Url::parse("file:///workspace/missing.txt").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "missing parser".to_string(),
            Some(language.to_string()),
            None,
        );
        let mut claim = server.auto_install.begin_test_result(
            language,
            crate::lsp::auto_install::InstallOutcome::Success {
                data_dir: std::path::PathBuf::from("/installed"),
            },
        );
        let duplicate = server.auto_install.try_install(language, Vec::new()).await;
        assert_eq!(
            duplicate.outcome,
            crate::lsp::auto_install::InstallOutcome::AlreadyInstalling
        );
        let mut completion = duplicate
            .completion
            .clone()
            .expect("exact waiter observes the reload claim")
            .receiver;

        server
            .install_coordinator()
            .reload_language_after_install(
                language,
                Path::new("/installed"),
                uri.clone(),
                true,
                Some(incarnation),
                Some(&mut claim),
            )
            .await;

        assert_eq!(
            claim.outcome,
            crate::lsp::auto_install::InstallOutcome::Failed
        );
        assert_eq!(
            completion.borrow_and_update().clone(),
            Some(crate::lsp::auto_install::InstallOutcome::Failed),
            "the exact waiter must observe the failed reload"
        );
        let snapshot = server
            .documents
            .latest_snapshot(&uri)
            .and_then(|view| view.slot.snapshot)
            .expect("failed reload must release the owner's first-parse waiters");
        assert!(snapshot.tree.is_none());
    }

    #[tokio::test]
    async fn successful_claim_reparses_owner_and_waiter_documents() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let first = Url::parse("file:///workspace/first.rs").unwrap();
        let second = Url::parse("file:///workspace/second.rs").unwrap();
        let first_incarnation = server.documents.insert(
            first.clone(),
            "fn first() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let second_incarnation = server.documents.insert(
            second.clone(),
            "fn second() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let claim = server.auto_install.begin_test_claim(
            "rust",
            query_dependency_paths(&server.settings_manager.load_settings(), "rust"),
        );
        let install = server.install_coordinator();
        let mut waiter = Box::pin(install.maybe_auto_install_language(
            "rust",
            second.clone(),
            false,
            Some(second_incarnation),
            InstallRequest::new(false),
        ));
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let owner_parse = server
            .install_coordinator()
            .reload_language_after_install(
                "rust",
                Path::new("/installed"),
                first.clone(),
                false,
                Some(first_incarnation),
                None,
            )
            .await;
        claim.complete(crate::lsp::auto_install::InstallOutcome::Success {
            data_dir: std::path::PathBuf::from("/installed"),
        });

        let completion = waiter.await;
        assert!(completion.same_lifetime);
        assert!(
            owner_parse
                .expect("owner produced a parse")
                .matches(&server.documents.get(&first).unwrap())
        );
        assert!(
            completion
                .parsed
                .expect("waiter produced its own parse")
                .matches(&server.documents.get(&second).unwrap())
        );
        assert!(server.documents.get(&first).unwrap().tree().is_some());
        assert!(server.documents.get(&second).unwrap().tree().is_some());
    }

    #[tokio::test]
    async fn abandoned_claim_waiter_takes_over_reparse() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/waiter.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn waiter() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let claim = server.auto_install.begin_test_claim(
            "rust",
            query_dependency_paths(&server.settings_manager.load_settings(), "rust"),
        );
        let install = server.install_coordinator();
        let mut waiter = Box::pin(install.maybe_auto_install_language(
            "rust",
            uri.clone(),
            false,
            Some(incarnation),
            InstallRequest::new(false),
        ));
        std::future::poll_fn(|cx| {
            assert!(waiter.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        drop(claim);

        let completion = waiter.await;
        assert!(completion.same_lifetime);
        assert!(
            completion
                .parsed
                .expect("takeover produced a parse")
                .matches(&server.documents.get(&uri).unwrap())
        );
        assert!(server.documents.get(&uri).unwrap().tree().is_some());
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
    async fn reload_does_not_publish_installed_language_tree_for_relabelled_document() {
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

    #[rstest::rstest]
    #[case(None)]
    #[case(Some("text"))]
    #[case(Some("rust"))]
    #[tokio::test]
    async fn available_parser_reports_only_its_own_parse(#[case] initial_label: Option<&str>) {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///own-install.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            initial_label.map(str::to_string),
            None,
        );
        let install = server.install_coordinator();
        let first = install
            .maybe_auto_install_language(
                "rust",
                uri.clone(),
                false,
                Some(incarnation),
                InstallRequest::new(false),
            )
            .await;
        assert!(first.same_lifetime);
        let parsed = first.parsed.expect("this install published the parse");
        assert!(parsed.matches(&server.documents.get(&uri).unwrap()));
        assert_eq!(
            server.documents.get(&uri).unwrap().language_id(),
            Some("rust")
        );
        let second = install
            .maybe_auto_install_language(
                "rust",
                uri.clone(),
                false,
                Some(incarnation),
                InstallRequest::new(false),
            )
            .await;
        assert!(
            second.same_lifetime,
            "already parsed is still successful recovery"
        );
        assert!(
            second.parsed.is_none(),
            "a current tree alone does not grant downstream work"
        );
    }

    #[tokio::test]
    async fn install_completion_cannot_cancel_a_newer_edits_eager_batch() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///delayed-install.rs").unwrap();
        let incarnation =
            server
                .documents
                .insert(uri.clone(), "fn old() {}".into(), Some("rust".into()), None);
        let completion = server
            .install_coordinator()
            .maybe_auto_install_language(
                "rust",
                uri.clone(),
                false,
                Some(incarnation),
                InstallRequest::new(false),
            )
            .await;
        let parsed = completion.parsed.expect("install's own parse");
        server
            .documents
            .update_document(uri.clone(), "fn edited() {}".into(), None);
        server.parse_coordinator().reparse_latest(&uri, None).await;
        assert!(server.documents.get(&uri).unwrap().has_current_tree());
        let token = server.bridge.begin_test_eager_open_batch(&uri);
        assert!(
            !server
                .injection_coordinator()
                .process_injections_for_parse(&uri, parsed)
                .await
        );
        assert!(
            !token.is_cancelled(),
            "a delayed install must leave the edit's batch intact"
        );
    }

    // `start_paused`: the healthy path returns without awaiting, so the bound
    // below needs no wall-clock time; a regressed guard instead parks on the
    // completion wait, the runtime goes idle, and the deadline fires at once.
    // Without it the bound is real time and a 5s scheduling stall on a loaded
    // CI box reads as a regression.
    #[tokio::test(start_paused = true)]
    async fn stale_install_task_stops_before_install_events() {
        let (service, mut socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let language = "stale-install-language";
        // Hold an install claim so a regressed staleness guard falls through to
        // `try_install`'s `AlreadyInstalling` branch, which parks on a
        // completion this test never resolves. That makes the regression
        // deterministic and network-free: without the claim the fall-through
        // reaches the metadata-backed support lookup, whose timing depends on
        // the cache and the network.
        let _claim = server.auto_install.begin_test_claim(
            language,
            query_dependency_paths(&server.settings_manager.load_settings(), language),
        );
        let uri = Url::parse("file:///workspace/stale-install.txt").unwrap();
        let old_incarnation = server.documents.insert(
            uri.clone(),
            "old".to_string(),
            Some(language.to_string()),
            None,
        );
        server.documents.remove(&uri);
        let new_incarnation = server.documents.insert(
            uri.clone(),
            "new".to_string(),
            Some(language.to_string()),
            None,
        );
        assert_ne!(new_incarnation, old_incarnation);

        // Bounded: the held claim makes a regressed guard park on the
        // `AlreadyInstalling` completion wait instead of returning.
        let stale = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server.install_coordinator().maybe_auto_install_language(
                language,
                uri,
                false,
                Some(old_incarnation),
                InstallRequest::new(false),
            ),
        )
        .await
        .expect("a stale task must return without awaiting an install");
        assert!(!stale.same_lifetime);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), socket.next())
                .await
                .is_err(),
            "a stale task must not emit install progress events"
        );
    }
}
