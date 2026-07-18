//! Parser auto-install coordinator: dedupes concurrent installs
//! (`InstallingLanguages`), tracks crashed parsers (`FailedParserRegistry`),
//! and runs the install.
//!
//! Returns `InstallResult` with events instead of calling `ClientNotifier`
//! directly — keeps this module free of LSP infrastructure so it can be
//! unit-tested in isolation. Kakehashi gates on `is_auto_install_enabled()`,
//! calls `try_install()`, dispatches the events, and then handles
//! post-install coordination (settings update, language reload).

use std::{
    collections::HashMap,
    future::Future,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tower_lsp_server::ls_types::MessageType;

use crate::error::LockResultExt;
use crate::install::support_check::{
    TrackedSupportCheck, should_skip_unsupported_language_tracked,
};
use crate::language::FailedParserRegistry;

use super::{InstallingLanguages, InstallingLanguagesExt};

/// Result of an installation attempt with all events for Kakehashi to dispatch.
pub(crate) struct InstallResult {
    /// What happened during the installation attempt
    pub outcome: InstallOutcome,
    /// Events to be dispatched to ClientNotifier by Kakehashi
    pub events: Vec<InstallEvent>,
    /// Exact in-flight claim observed by an `AlreadyInstalling` caller.
    pub completion: Option<InstallCompletion>,
    claim: Option<InstallMarkerGuard>,
}

#[derive(Clone)]
pub(crate) struct InstallCompletion {
    pub(crate) receiver: tokio::sync::watch::Receiver<Option<InstallOutcome>>,
}

struct ClaimState {
    completion: tokio::sync::watch::Sender<Option<InstallOutcome>>,
}

#[cfg(test)]
pub(crate) struct TestInstallClaim {
    guard: Option<InstallMarkerGuard>,
}

#[cfg(test)]
impl TestInstallClaim {
    pub(crate) fn complete(mut self, outcome: InstallOutcome) {
        self.guard
            .take()
            .expect("test claim is present")
            .complete(outcome);
    }
}

impl Drop for InstallResult {
    fn drop(&mut self) {
        // An uncompleted claim fails closed through InstallMarkerGuard::drop.
        let _ = self.claim.take();
    }
}

impl InstallResult {
    fn with_claim(
        outcome: InstallOutcome,
        events: Vec<InstallEvent>,
        mut claim: InstallMarkerGuard,
    ) -> Self {
        claim.preserve_terminal(outcome.clone());
        Self {
            outcome,
            events,
            completion: None,
            claim: Some(claim),
        }
    }

    pub(crate) fn complete_claim(&mut self) {
        if let Some(claim) = self.claim.take() {
            claim.complete(self.outcome.clone());
        }
    }
}

/// Outcome of an installation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallOutcome {
    /// Installation succeeded, parser is ready to use
    Success {
        /// Directory where parser and queries were installed
        data_dir: PathBuf,
    },
    /// Parser compiled but queries had warnings (still usable)
    SuccessWithWarnings {
        /// Directory where parser and queries were installed
        data_dir: PathBuf,
    },
    /// Parser already exists, just needs reload (no installation performed)
    AlreadyExists {
        /// Directory where parser already exists
        data_dir: PathBuf,
    },
    /// Installation already in progress for this language
    AlreadyInstalling,
    /// The owner disappeared before producing a terminal install result
    Abandoned,
    /// Parser previously crashed, skipping to protect system
    ParserFailed,
    /// Language not supported by nvim-treesitter
    Unsupported,
    /// Installation failed
    Failed,
    /// Could not determine data directory
    NoDataDir,
}

impl InstallOutcome {
    /// Get the data directory if installation was successful.
    pub(crate) fn data_dir(&self) -> Option<&PathBuf> {
        match self {
            InstallOutcome::Success { data_dir }
            | InstallOutcome::SuccessWithWarnings { data_dir }
            | InstallOutcome::AlreadyExists { data_dir } => Some(data_dir),
            _ => None,
        }
    }
}

/// Events generated during installation for Kakehashi to dispatch.
#[derive(Debug, Clone)]
pub(crate) enum InstallEvent {
    /// Log message to send to client
    Log { level: MessageType, message: String },
    /// Progress begin notification
    ProgressBegin,
    /// Progress end notification
    ProgressEnd { success: bool },
}

/// Isolated coordinator for parser auto-installation.
///
/// Handles installation state and execution without depending on other
/// coordinators, returning events that Kakehashi dispatches to `ClientNotifier`.
/// Thread-safe and cheaply cloneable (all `Arc`-based) for sharing across async
/// tasks: `InstallingLanguages` is `Arc<Mutex<HashSet>>` and
/// `FailedParserRegistry` uses sharded-lock `DashSet`/`DashMap`.
#[derive(Clone)]
pub(crate) struct AutoInstallManager {
    /// Tracks languages currently being installed to prevent duplicates
    installing_languages: InstallingLanguages,
    /// Tracks parsers that have crashed to prevent repeated failures
    failed_parsers: FailedParserRegistry,
    claims: Arc<Mutex<HashMap<String, ClaimState>>>,
}

impl std::fmt::Debug for AutoInstallManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoInstallManager")
            .field("installing_languages", &"InstallingLanguages")
            .field("failed_parsers", &"FailedParserRegistry")
            .finish()
    }
}

/// RAII marker for an in-flight install: clears the `InstallingLanguages`
/// entry on drop. Manual `finish_install` calls would leak the marker if the
/// calling future were dropped at an await, leaving the language stuck
/// `AlreadyInstalling` (and therefore never parsed) for the
/// server's lifetime. The guard first moves into the support-check task, then
/// into the install task on success, so it tracks detached work's true
/// lifetime rather than the caller's.
struct InstallMarkerGuard {
    installing: InstallingLanguages,
    claims: Arc<Mutex<HashMap<String, ClaimState>>>,
    completion: tokio::sync::watch::Sender<Option<InstallOutcome>>,
    language: String,
    terminal: Option<InstallOutcome>,
}

impl InstallMarkerGuard {
    fn preserve_terminal(&mut self, outcome: InstallOutcome) {
        self.terminal = Some(outcome);
    }

    fn complete(mut self, outcome: InstallOutcome) {
        self.terminal = Some(outcome);
    }
}

impl Drop for InstallMarkerGuard {
    fn drop(&mut self) {
        let terminal = self.terminal.take().unwrap_or(InstallOutcome::Abandoned);
        self.installing.finish_install(&self.language);
        self.claims
            .lock()
            .recover_poison("AutoInstallManager::finish_claim")
            .remove(&self.language);
        self.completion.send_replace(Some(terminal));
    }
}

#[cfg(test)]
fn failed_parser_state_dir() -> PathBuf {
    crate::install::test_state_dir()
}

#[cfg(not(test))]
fn failed_parser_state_dir() -> PathBuf {
    std::env::var_os("KAKEHASHI_STATE_DIR")
        // An empty value resolves to the process cwd (writing crash files
        // wherever it was started), so treat it as unset — matching how
        // `resolve_data_dir` handles an empty `KAKEHASHI_DATA_DIR`.
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(crate::install::default_data_dir)
        // Platform-aware last resort (not a hard-coded `/tmp`, which doesn't
        // exist on Windows); only reached if the data dir can't resolve.
        .unwrap_or_else(|| std::env::temp_dir().join("kakehashi"))
}

impl AutoInstallManager {
    /// Create a new `AutoInstallManager`.
    pub fn new(
        installing_languages: InstallingLanguages,
        failed_parsers: FailedParserRegistry,
    ) -> Self {
        Self {
            installing_languages,
            failed_parsers,
            claims: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(crate) fn begin_test_claim(&self, language: &str) -> TestInstallClaim {
        let mut claims = self
            .claims
            .lock()
            .recover_poison("AutoInstallManager::begin_test_claim");
        assert!(!claims.contains_key(language));
        let (completion, _) = tokio::sync::watch::channel(None);
        claims.insert(
            language.to_string(),
            ClaimState {
                completion: completion.clone(),
            },
        );
        assert!(self.installing_languages.try_start_install(language));
        TestInstallClaim {
            guard: Some(InstallMarkerGuard {
                installing: self.installing_languages.clone(),
                claims: Arc::clone(&self.claims),
                completion,
                language: language.to_string(),
                terminal: None,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn begin_test_result(
        &self,
        language: &str,
        outcome: InstallOutcome,
    ) -> InstallResult {
        let TestInstallClaim { guard } = self.begin_test_claim(language);
        InstallResult::with_claim(
            outcome,
            Vec::new(),
            guard.expect("test result claim is present"),
        )
    }

    /// Initialize the failed parser registry with crash detection.
    ///
    /// State storage location: `KAKEHASHI_STATE_DIR` if set, else the default
    /// data directory, falling back to a `kakehashi` dir under the OS temp dir
    /// if neither resolves. Unit-test builds instead use a stable process-owned
    /// temp directory, independent of the shared parser assets. Crash-recovery
    /// state (`parsing_in_progress`, `failed_parsers`) is ephemeral and
    /// conceptually distinct from the
    /// persistent parser/query install assets in the data dir; the override
    /// lets it live elsewhere — e.g. so concurrent test processes that share one
    /// read-only install dir can isolate their crash state and not poison each
    /// other (an unlocked marker from another process is otherwise read as a
    /// crash and marks that parser failed).
    /// If initialization fails, returns an uninitialized registry whose
    /// `begin_parsing` calls fail closed before entering native parser code.
    pub fn init_failed_parser_registry() -> FailedParserRegistry {
        let state_dir = failed_parser_state_dir();

        let registry = FailedParserRegistry::new(&state_dir);

        // Initialize and detect any previous crashes
        if let Err(e) = registry.init() {
            log::warn!(
                target: "kakehashi::crash_recovery",
                "Failed to initialize crash recovery state: {}",
                e
            );
        }

        registry
    }

    /// Check if a parser has previously crashed and should be skipped.
    pub fn is_parser_failed(&self, language: &str) -> bool {
        self.failed_parsers.is_failed(language)
    }

    /// Record that parsing is starting for crash detection.
    ///
    /// Should be called before parsing a document.
    pub fn begin_parsing(&self, language: &str) -> std::io::Result<()> {
        self.failed_parsers.begin_parsing(language)
    }

    /// Record that parsing completed successfully.
    ///
    /// Should be called after parsing completes without crashing.
    pub fn end_parsing(&self, language: &str) -> std::io::Result<()> {
        self.failed_parsers.end_parsing_language(language)
    }

    /// Persist crash detection state on shutdown.
    ///
    /// Should be called during graceful shutdown.
    pub fn persist_state(&self) -> std::io::Result<()> {
        self.failed_parsers.persist_state()
    }

    /// Clone-handle to the crash-detection registry (`Arc`-backed), for tasks
    /// that outlive `&self` — the signal-reap task persists crash-detection
    /// state before exiting, exactly like the graceful shutdown path.
    pub fn failed_parsers_handle(&self) -> FailedParserRegistry {
        self.failed_parsers.clone()
    }

    /// Attempt to install a language parser.
    ///
    /// Intentionally isolated to enable unit testing without LSP infrastructure:
    /// it does NOT call `ClientNotifier` (returns events instead), access
    /// `SettingsManager` (Kakehashi checks settings first), or reload the
    /// language (Kakehashi handles post-install).
    pub async fn try_install(&self, language: &str) -> InstallResult {
        self.try_install_with_support_check(language, |language, default_data_dir| async move {
            let fetch_options =
                default_data_dir
                    .as_ref()
                    .map(|dir| crate::install::metadata::FetchOptions {
                        data_dir: Some(dir.as_path()),
                        use_cache: true,
                    });
            should_skip_unsupported_language_tracked(&language, fetch_options.as_ref()).await
        })
        .await
    }

    async fn try_install_with_support_check<F, Fut>(
        &self,
        language: &str,
        support_check: F,
    ) -> InstallResult
    where
        F: FnOnce(String, Option<PathBuf>) -> Fut + Send + 'static,
        Fut: Future<Output = TrackedSupportCheck> + Send + 'static,
    {
        self.try_install_with_support_check_and_executor(
            language,
            support_check,
            |language, data_dir| async move {
                crate::install::install_language_async(
                    language,
                    data_dir,
                    false,
                    crate::install::parser::ParserCompile::KillableSubprocess,
                )
                .await
            },
        )
        .await
    }

    async fn try_install_with_support_check_and_executor<F, Fut, I, IFut>(
        &self,
        language: &str,
        support_check: F,
        install_executor: I,
    ) -> InstallResult
    where
        F: FnOnce(String, Option<PathBuf>) -> Fut + Send + 'static,
        Fut: Future<Output = TrackedSupportCheck> + Send + 'static,
        I: FnOnce(String, PathBuf) -> IFut + Send + 'static,
        IFut: Future<Output = crate::install::InstallResult> + Send + 'static,
    {
        let mut events = Vec::new();

        // Check if parser previously failed (crash protection)
        if self.failed_parsers.is_failed(language) {
            events.push(InstallEvent::Log {
                level: MessageType::WARNING,
                message: format!(
                    "Parser '{}' previously crashed. Skipping auto-install. \
                     Clear with: kakehashi language clear-failed {}",
                    language, language
                ),
            });
            return InstallResult {
                outcome: InstallOutcome::ParserFailed,
                events,
                completion: None,
                claim: None,
            };
        }

        // Claim before the network-backed support lookup so concurrent calls
        // for the same language do not all fetch metadata. Ordinary early
        // returns release the RAII claim; a timeout transfers it to a detached
        // keeper until the still-running blocking lookup exits.
        let install_marker = {
            let mut claims = self
                .claims
                .lock()
                .recover_poison("AutoInstallManager::claim_install");
            if let Some(claim) = claims.get(language) {
                events.push(InstallEvent::Log {
                    level: MessageType::INFO,
                    message: format!(
                        "Language '{}' support is already being checked or installed",
                        language
                    ),
                });
                return InstallResult {
                    outcome: InstallOutcome::AlreadyInstalling,
                    events,
                    completion: Some(InstallCompletion {
                        receiver: claim.completion.subscribe(),
                    }),
                    claim: None,
                };
            }
            let (completion, _) = tokio::sync::watch::channel(None);
            claims.insert(
                language.to_string(),
                ClaimState {
                    completion: completion.clone(),
                },
            );
            if !self.installing_languages.try_start_install(language) {
                let _ = writeln!(
                    std::io::stderr(),
                    "Auto-install state mismatch for '{}': repairing a stale installing marker",
                    language
                );
                events.push(InstallEvent::Log {
                    level: MessageType::WARNING,
                    message: format!(
                        "Auto-install state for '{}' was inconsistent; retrying with repaired state",
                        language
                    ),
                });
                self.installing_languages.finish_install(language);
                if !self.installing_languages.try_start_install(language) {
                    claims.remove(language);
                    return InstallResult {
                        outcome: InstallOutcome::Failed,
                        events,
                        completion: None,
                        claim: None,
                    };
                }
            }
            InstallMarkerGuard {
                installing: self.installing_languages.clone(),
                claims: Arc::clone(&self.claims),
                completion,
                language: language.to_string(),
                terminal: None,
            }
        };

        // Check if language is supported by nvim-treesitter
        let default_data_dir = crate::install::default_data_dir();
        // The task owns the claim so cancellation of this caller cannot release
        // it while the lookup (which contains spawn_blocking work) continues.
        // On normal completion the claim returns here and is either dropped by
        // an early return or transferred to the parser-install task below.
        let lookup_language = language.to_string();
        let lookup_data_dir = default_data_dir.clone();
        let support_task = tokio::spawn(async move {
            let mut result = support_check(lookup_language, lookup_data_dir).await;
            if let Some(completion) = result.completion.take() {
                let terminal = if result.should_skip {
                    InstallOutcome::Unsupported
                } else {
                    InstallOutcome::Failed
                };
                // Start the keeper inside this detached task. If the caller
                // dropped our JoinHandle while the check was running, task
                // output would be discarded and could not safely carry the
                // marker/completion pair back to it.
                tokio::spawn(async move {
                    if let Err(join_error) = completion.await {
                        log::error!(
                            target: "kakehashi::auto_install",
                            "Metadata support-check completion task failed: {}",
                            join_error
                        );
                    } else {
                        install_marker.complete(terminal);
                    }
                });
                (result, None)
            } else {
                (result, Some(install_marker))
            }
        });
        let (support_result, install_marker) = match support_task.await {
            Ok(result) => result,
            Err(join_error) => {
                events.push(InstallEvent::Log {
                    level: MessageType::ERROR,
                    message: format!(
                        "Support check task for '{}' failed: {}",
                        language, join_error
                    ),
                });
                return InstallResult {
                    outcome: InstallOutcome::Failed,
                    events,
                    completion: None,
                    claim: None,
                };
            }
        };
        let TrackedSupportCheck {
            should_skip,
            reason,
            completion: _,
        } = support_result;

        if let Some(reason) = &reason {
            events.push(InstallEvent::Log {
                level: reason.message_type(),
                message: reason.message(),
            });
        }

        if should_skip {
            return match install_marker {
                Some(claim) => {
                    InstallResult::with_claim(InstallOutcome::Unsupported, events, claim)
                }
                None => InstallResult {
                    outcome: InstallOutcome::Unsupported,
                    events,
                    completion: None,
                    claim: None,
                },
            };
        }
        let Some(install_marker) = install_marker else {
            events.push(InstallEvent::Log {
                level: MessageType::ERROR,
                message: format!(
                    "Support check for '{}' completed without its install claim",
                    language
                ),
            });
            return InstallResult {
                outcome: InstallOutcome::Failed,
                events,
                completion: None,
                claim: None,
            };
        };

        // Progress begin
        events.push(InstallEvent::ProgressBegin);

        // Get data directory
        let data_dir = match default_data_dir {
            Some(dir) => dir,
            None => {
                events.push(InstallEvent::Log {
                    level: MessageType::ERROR,
                    message: "Could not determine data directory for auto-install".to_string(),
                });
                events.push(InstallEvent::ProgressEnd { success: false });
                return InstallResult::with_claim(
                    InstallOutcome::NoDataDir,
                    events,
                    install_marker,
                );
            }
        };

        // Check if parser already exists - skip installation and just signal reload
        if crate::install::parser_file_exists(language, &data_dir).is_some() {
            events.push(InstallEvent::Log {
                level: MessageType::INFO,
                message: format!(
                    "Parser for '{}' already exists. Loading without reinstall...",
                    language
                ),
            });
            events.push(InstallEvent::ProgressEnd { success: true });
            let outcome = InstallOutcome::AlreadyExists {
                data_dir: data_dir.clone(),
            };
            return InstallResult::with_claim(outcome, events, install_marker);
        }

        // Log installation start
        events.push(InstallEvent::Log {
            level: MessageType::INFO,
            message: format!("Auto-installing language '{}' in background...", language),
        });

        // Run the actual installation in its own task that owns the marker:
        // the blocking install keeps running even if this caller is cancelled
        // at the await, so clearing the marker on caller-drop would let a
        // retry race the still-running install on the same parser/query
        // paths. Owned by the task, the marker is held until the install
        // truly finishes (the task itself is never cancelled).
        let lang = language.to_string();
        let task_lang = lang.clone();
        let task_data_dir = data_dir.clone();
        let install_task = tokio::spawn(async move {
            // Auto-install runs inside the LSP server, whose `current_exe()` is the
            // kakehashi binary — so the killable subprocess (re-exec
            // `__compile-parser`) is valid and bounds a hung `cc`.
            let result = install_executor(task_lang.clone(), task_data_dir.clone()).await;
            let parser_exists =
                crate::install::parser_file_exists(&task_lang, &task_data_dir).is_some();
            let terminal = if result.is_success() {
                InstallOutcome::Success {
                    data_dir: task_data_dir.clone(),
                }
            } else if parser_exists {
                InstallOutcome::SuccessWithWarnings {
                    data_dir: task_data_dir.clone(),
                }
            } else {
                InstallOutcome::Failed
            };
            let mut install_marker = install_marker;
            // If the caller awaiting this task is cancelled, the task output is
            // dropped. Keep the real install result on the guard so waiters see
            // success rather than the guard's fail-closed fallback.
            install_marker.preserve_terminal(terminal);
            (result, parser_exists, install_marker)
        });
        let (result, parser_exists, install_marker) = match install_task.await {
            Ok(result) => result,
            Err(join_error) => {
                events.push(InstallEvent::ProgressEnd { success: false });
                events.push(InstallEvent::Log {
                    level: MessageType::ERROR,
                    message: format!("Install task for '{}' failed: {}", lang, join_error),
                });
                return InstallResult {
                    outcome: InstallOutcome::Failed,
                    events,
                    completion: None,
                    claim: None,
                };
            }
        };

        if result.is_success() {
            events.push(InstallEvent::ProgressEnd { success: true });
            events.push(InstallEvent::Log {
                level: MessageType::INFO,
                message: format!("Successfully installed language '{}'. Reloading...", lang),
            });
            let outcome = InstallOutcome::Success {
                data_dir: data_dir.clone(),
            };
            InstallResult::with_claim(outcome, events, install_marker)
        } else if parser_exists {
            // Parser compiled but queries had issues - still usable
            events.push(InstallEvent::ProgressEnd { success: true });

            let mut warnings = Vec::new();
            if let Some(e) = &result.queries_error {
                warnings.push(format!("queries: {}", e));
            }
            events.push(InstallEvent::Log {
                level: MessageType::WARNING,
                message: format!(
                    "Language '{}' parser installed but with warnings: {}. Reloading...",
                    lang,
                    warnings.join("; ")
                ),
            });

            let outcome = InstallOutcome::SuccessWithWarnings {
                data_dir: data_dir.clone(),
            };
            InstallResult::with_claim(outcome, events, install_marker)
        } else {
            // Installation failed
            events.push(InstallEvent::ProgressEnd { success: false });

            let mut errors = Vec::new();
            if let Some(e) = result.parser_error {
                errors.push(format!("parser: {}", e));
            }
            if let Some(e) = result.queries_error {
                errors.push(format!("queries: {}", e));
            }
            events.push(InstallEvent::Log {
                level: MessageType::ERROR,
                message: format!(
                    "Failed to install language '{}': {}",
                    lang,
                    errors.join("; ")
                ),
            });

            let outcome = InstallOutcome::Failed;
            InstallResult::with_claim(outcome, events, install_marker)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PARENT_STATE_DIR: &str = "KAKEHASHI_TEST_PARENT_CRASH_STATE_DIR";
    const CHILD_HANDSHAKE_PATH: &str = "KAKEHASHI_TEST_CRASH_STATE_HANDSHAKE_PATH";

    #[test]
    fn unit_test_crash_state_is_separate_from_shared_install_assets() {
        const CHILD_TEST: &str =
            "lsp::auto_install::manager::tests::child_process_uses_distinct_crash_state";

        let shared_install_dir = crate::install::test_support::test_data_dir_path();
        let state_dir = failed_parser_state_dir();
        let _registry = AutoInstallManager::init_failed_parser_registry();

        assert_ne!(state_dir, shared_install_dir);
        assert_eq!(state_dir, failed_parser_state_dir());
        assert!(state_dir.join("crash_recovery.lock").is_file());

        let handshake_dir = tempdir().expect("create crash-state handshake directory");
        let handshake_path = handshake_dir.path().join("child-ran");
        let child_status = std::process::Command::new(
            std::env::current_exe().expect("resolve current test executable"),
        )
        .args(["--ignored", "--exact", CHILD_TEST])
        .env(PARENT_STATE_DIR, &state_dir)
        .env(CHILD_HANDSHAKE_PATH, &handshake_path)
        .status()
        .expect("run crash-state isolation test in child process");

        assert!(
            child_status.success(),
            "child test process must use a different crash-state directory"
        );
        assert!(
            handshake_path.is_file(),
            "child test process must execute the isolation assertion"
        );
    }

    #[test]
    #[ignore = "run only as a child of the crash-state isolation test"]
    fn child_process_uses_distinct_crash_state() {
        let (Some(parent_state_dir), Some(handshake_path)) = (
            std::env::var_os(PARENT_STATE_DIR),
            std::env::var_os(CHILD_HANDSHAKE_PATH),
        ) else {
            return;
        };

        assert_ne!(failed_parser_state_dir(), PathBuf::from(parent_state_dir));
        std::fs::write(handshake_path, b"child assertion passed")
            .expect("write crash-state child handshake");
    }

    fn create_test_manager() -> (AutoInstallManager, tempfile::TempDir) {
        let temp = tempdir().expect("Failed to create temp dir");
        let installing = InstallingLanguages::new();
        let failed = FailedParserRegistry::new(temp.path());
        failed.init().expect("Failed to init registry");
        (AutoInstallManager::new(installing, failed), temp)
    }

    async fn wait_for_terminal(
        receiver: &mut tokio::sync::watch::Receiver<Option<InstallOutcome>>,
    ) -> InstallOutcome {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(outcome) = receiver.borrow().clone() {
                    break outcome;
                }
                receiver.changed().await.unwrap();
            }
        })
        .await
        .expect("install claim must publish a terminal outcome")
    }

    #[test]
    fn test_is_parser_failed_returns_false_for_new_parser() {
        let (manager, _temp) = create_test_manager();
        assert!(!manager.is_parser_failed("lua"));
    }

    #[test]
    fn test_crash_tracking_workflow() {
        let (manager, _temp) = create_test_manager();

        // Start parsing
        manager.begin_parsing("lua").expect("begin_parsing failed");

        // End parsing successfully
        manager.end_parsing("lua").expect("end_parsing failed");

        // Parser should not be marked as failed
        assert!(!manager.is_parser_failed("lua"));
    }

    #[test]
    fn install_marker_guard_releases_on_drop() {
        let installing = InstallingLanguages::new();
        assert!(installing.try_start_install("lua"));
        let claims = Arc::new(Mutex::new(HashMap::new()));
        let (completion, _) = tokio::sync::watch::channel(None);
        claims.lock().unwrap().insert(
            "lua".to_string(),
            ClaimState {
                completion: completion.clone(),
            },
        );

        let guard = InstallMarkerGuard {
            installing: installing.clone(),
            claims,
            completion,
            language: "lua".to_string(),
            terminal: None,
        };
        drop(guard);

        assert!(
            installing.try_start_install("lua"),
            "marker must be released when the guard drops, even if try_install \
             is cancelled at its install await"
        );
    }

    #[tokio::test]
    async fn cancelled_owner_preserves_detached_install_success_for_exact_waiter() {
        let (manager, _temp) = create_test_manager();
        let owner_manager = manager.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            owner_manager
                .try_install_with_support_check_and_executor(
                    "controlled-detached-success",
                    |_, _| async { TrackedSupportCheck::completed(false, None) },
                    |_, _| async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                        crate::install::InstallResult {
                            parser_path: Some(PathBuf::from("/installed/parser")),
                            queries_path: Some(PathBuf::from("/installed/queries")),
                            parser_error: None,
                            queries_error: None,
                        }
                    },
                )
                .await
        });
        started_rx.await.expect("install executor must start");

        let duplicate = manager
            .try_install_with_support_check("controlled-detached-success", |_, _| async {
                panic!("the exact waiter must not start another support check")
            })
            .await;
        assert_eq!(duplicate.outcome, InstallOutcome::AlreadyInstalling);
        let mut completion = duplicate
            .completion
            .clone()
            .expect("waiter observes the owner's exact claim")
            .receiver;

        owner.abort();
        let _ = owner.await;
        let _ = release_tx.send(());
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(outcome) = completion.borrow().clone() {
                    break outcome;
                }
                completion.changed().await.unwrap();
            }
        })
        .await
        .expect("detached install must publish its actual result");

        assert!(matches!(terminal, InstallOutcome::Success { .. }));
    }

    #[tokio::test]
    async fn panicked_support_task_abandons_exact_claim_and_allows_retry() {
        let (manager, _temp) = create_test_manager();
        let owner_manager = manager.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            owner_manager
                .try_install_with_support_check("panic-support", |_, _| async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    panic!("controlled support panic");
                })
                .await
        });
        started_rx.await.unwrap();
        let duplicate = manager
            .try_install_with_support_check("panic-support", |_, _| async {
                panic!("exact waiter must not run support check")
            })
            .await;
        let mut completion = duplicate.completion.clone().unwrap().receiver;

        let _ = release_tx.send(());
        assert_eq!(owner.await.unwrap().outcome, InstallOutcome::Failed);
        assert_eq!(
            wait_for_terminal(&mut completion).await,
            InstallOutcome::Abandoned
        );
        let retry = manager
            .try_install_with_support_check("panic-support", |_, _| async {
                TrackedSupportCheck::completed(true, None)
            })
            .await;
        assert_eq!(retry.outcome, InstallOutcome::Unsupported);
    }

    #[tokio::test]
    async fn panicked_install_task_abandons_exact_claim_and_allows_retry() {
        let (manager, _temp) = create_test_manager();
        let owner_manager = manager.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            owner_manager
                .try_install_with_support_check_and_executor(
                    "panic-install",
                    |_, _| async { TrackedSupportCheck::completed(false, None) },
                    |_, _| async move {
                        let _ = started_tx.send(());
                        let _ = release_rx.await;
                        panic!("controlled install panic");
                    },
                )
                .await
        });
        started_rx.await.unwrap();
        let duplicate = manager
            .try_install_with_support_check("panic-install", |_, _| async {
                panic!("exact waiter must not run support check")
            })
            .await;
        let mut completion = duplicate.completion.clone().unwrap().receiver;

        let _ = release_tx.send(());
        assert_eq!(owner.await.unwrap().outcome, InstallOutcome::Failed);
        assert_eq!(
            wait_for_terminal(&mut completion).await,
            InstallOutcome::Abandoned
        );
        let retry = manager
            .try_install_with_support_check("panic-install", |_, _| async {
                TrackedSupportCheck::completed(true, None)
            })
            .await;
        assert_eq!(retry.outcome, InstallOutcome::Unsupported);
    }

    #[tokio::test]
    async fn concurrent_duplicate_install_skips_support_lookup() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let (manager, _temp) = create_test_manager();
        let lookup_count = Arc::new(AtomicUsize::new(0));
        let first_manager = manager.clone();
        let first_lookup_count = Arc::clone(&lookup_count);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let first = tokio::spawn(async move {
            first_manager
                .try_install_with_support_check("lua", |_, _| async move {
                    first_lookup_count.fetch_add(1, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    TrackedSupportCheck::completed(true, None)
                })
                .await
        });
        started_rx.await.expect("first support lookup must start");

        let duplicate_lookup_count = Arc::clone(&lookup_count);
        let result = manager
            .try_install_with_support_check("lua", move |_, _| {
                duplicate_lookup_count.fetch_add(1, Ordering::SeqCst);
                async { TrackedSupportCheck::completed(false, None) }
            })
            .await;

        assert_eq!(result.outcome, InstallOutcome::AlreadyInstalling);
        assert_eq!(lookup_count.load(Ordering::SeqCst), 1);
        assert!(result.events.iter().any(|e| matches!(
            e,
            InstallEvent::Log { level: MessageType::INFO, message }
                if message.contains("already being checked or installed")
        )));

        let mut completion = result
            .completion
            .clone()
            .expect("duplicate receives exact claim")
            .receiver;
        let waiter = tokio::spawn(async move {
            loop {
                if let Some(outcome) = completion.borrow().clone() {
                    return outcome;
                }
                completion.changed().await.unwrap();
            }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        let _ = release_tx.send(());
        let mut first_result = first.await.expect("first install task must finish");
        assert_eq!(first_result.outcome, InstallOutcome::Unsupported);
        first_result.complete_claim();
        drop(first_result);

        let second_manager = manager.clone();
        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
        let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
        let second = tokio::spawn(async move {
            second_manager
                .try_install_with_support_check("lua", |_, _| async move {
                    let _ = second_started_tx.send(());
                    let _ = second_release_rx.await;
                    TrackedSupportCheck::completed(true, None)
                })
                .await
        });
        second_started_rx.await.expect("second claim must start");

        assert_eq!(
            waiter
                .await
                .expect("a failed winner must still release install waiters"),
            InstallOutcome::Unsupported
        );
        let _ = second_release_tx.send(());
        assert_eq!(
            second.await.expect("second claim must finish").outcome,
            InstallOutcome::Unsupported
        );
    }

    #[tokio::test]
    async fn skipped_support_lookup_releases_install_claim() {
        let (manager, _temp) = create_test_manager();

        let result = manager
            .try_install_with_support_check("unsupported", |_, _| async {
                TrackedSupportCheck::completed(true, None)
            })
            .await;

        assert_eq!(result.outcome, InstallOutcome::Unsupported);
        drop(result);
        assert!(
            manager
                .installing_languages
                .try_start_install("unsupported"),
            "unsupported and metadata-error exits share this skip path and must release the claim"
        );
    }

    #[tokio::test]
    async fn stale_installing_marker_is_repaired_without_panicking() {
        let temp = tempdir().unwrap();
        let installing = InstallingLanguages::new();
        assert!(installing.try_start_install("stale-marker"));
        let failed = FailedParserRegistry::new(temp.path());
        failed.init().unwrap();
        let manager = AutoInstallManager::new(installing.clone(), failed);

        let result = manager
            .try_install_with_support_check("stale-marker", |_, _| async {
                TrackedSupportCheck::completed(true, None)
            })
            .await;

        assert_eq!(result.outcome, InstallOutcome::Unsupported);
        drop(result);
        assert!(installing.try_start_install("stale-marker"));
    }

    #[tokio::test]
    async fn timed_out_support_work_keeps_claim_until_completion() {
        let (manager, _temp) = create_test_manager();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let result = manager
            .try_install_with_support_check("lua", |_, _| async move {
                TrackedSupportCheck {
                    should_skip: true,
                    reason: None,
                    completion: Some(tokio::spawn(async move {
                        let _ = release_rx.await;
                    })),
                }
            })
            .await;
        assert_eq!(result.outcome, InstallOutcome::Unsupported);

        let duplicate = manager
            .try_install_with_support_check("lua", |_, _| async {
                TrackedSupportCheck::completed(false, None)
            })
            .await;
        assert_eq!(duplicate.outcome, InstallOutcome::AlreadyInstalling);
        let mut completion = duplicate
            .completion
            .clone()
            .expect("duplicate observes timed-out claim")
            .receiver;

        let _ = release_tx.send(());
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(outcome) = completion.borrow().clone() {
                    return outcome;
                }
                completion.changed().await.unwrap();
            }
        })
        .await
        .expect("timed-out claim must publish its terminal outcome");
        assert_eq!(terminal, InstallOutcome::Unsupported);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !manager.installing_languages.try_start_install("lua") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("claim keeper must release after timed-out work finishes");
    }

    #[tokio::test]
    async fn supported_result_without_returned_claim_fails_closed() {
        let (manager, _temp) = create_test_manager();

        let result = manager
            .try_install_with_support_check("lua", |_, _| async {
                TrackedSupportCheck {
                    should_skip: false,
                    reason: None,
                    completion: Some(tokio::spawn(async {})),
                }
            })
            .await;

        assert_eq!(result.outcome, InstallOutcome::Failed);
        assert!(result.events.iter().any(|event| matches!(
            event,
            InstallEvent::Log { level: MessageType::ERROR, message }
                if message.contains("completed without its install claim")
        )));
    }

    #[tokio::test]
    async fn cancelled_caller_keeps_claim_until_support_lookup_finishes() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let (manager, _temp) = create_test_manager();
        let task_manager = manager.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(async move {
            task_manager
                .try_install_with_support_check("lua", |_, _| async move {
                    let _ = started_tx.send(());
                    let _ = release_rx.await;
                    TrackedSupportCheck::completed(true, None)
                })
                .await
        });
        started_rx.await.expect("support lookup must start");
        task.abort();
        let _ = task.await;

        let duplicate_lookups = Arc::new(AtomicUsize::new(0));
        let duplicate_count = Arc::clone(&duplicate_lookups);
        let duplicate = manager
            .try_install_with_support_check("lua", move |_, _| {
                duplicate_count.fetch_add(1, Ordering::SeqCst);
                async { TrackedSupportCheck::completed(false, None) }
            })
            .await;
        assert_eq!(duplicate.outcome, InstallOutcome::AlreadyInstalling);
        assert_eq!(duplicate_lookups.load(Ordering::SeqCst), 0);
        let mut completion = duplicate
            .completion
            .clone()
            .expect("waiter observes the abandoned owner's exact claim")
            .receiver;

        let _ = release_tx.send(());
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(outcome) = completion.borrow().clone() {
                    break outcome;
                }
                completion.changed().await.unwrap();
            }
        })
        .await
        .expect("abandoned support owner must publish a retryable outcome");
        assert_eq!(terminal, InstallOutcome::Abandoned);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !manager.installing_languages.try_start_install("lua") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached lookup task must release the claim after its support check finishes");
    }

    #[tokio::test]
    async fn cancelled_caller_keeps_claim_after_lookup_timeout() {
        let (manager, _temp) = create_test_manager();
        let task_manager = manager.clone();
        let (lookup_started_tx, lookup_started_rx) = tokio::sync::oneshot::channel();
        let (timeout_tx, timeout_rx) = tokio::sync::oneshot::channel();
        let (timed_out_tx, timed_out_rx) = tokio::sync::oneshot::channel();
        let (work_release_tx, work_release_rx) = tokio::sync::oneshot::channel();

        let caller = tokio::spawn(async move {
            task_manager
                .try_install_with_support_check("lua", |_, _| async move {
                    let _ = lookup_started_tx.send(());
                    let _ = timeout_rx.await;
                    let completion = tokio::spawn(async move {
                        let _ = work_release_rx.await;
                    });
                    let _ = timed_out_tx.send(());
                    TrackedSupportCheck {
                        should_skip: true,
                        reason: None,
                        completion: Some(completion),
                    }
                })
                .await
        });
        lookup_started_rx.await.expect("support lookup must start");
        caller.abort();
        let _ = caller.await;

        let _ = timeout_tx.send(());
        timed_out_rx
            .await
            .expect("detached support lookup must report timeout");
        tokio::task::yield_now().await;
        assert!(
            !manager.installing_languages.try_start_install("lua"),
            "timeout completion must retain the claim after caller cancellation"
        );

        let _ = work_release_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !manager.installing_languages.try_start_install("lua") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("claim keeper must release after timed-out blocking work finishes");
    }

    #[tokio::test]
    async fn test_try_install_returns_parser_failed_for_crashed_parser() {
        let (manager, _temp) = create_test_manager();

        // Mark parser as failed
        manager
            .failed_parsers
            .mark_failed("bad_parser")
            .expect("mark_failed failed");

        // Try to install
        let result = manager.try_install("bad_parser").await;

        assert_eq!(result.outcome, InstallOutcome::ParserFailed);
        assert!(result.events.iter().any(|e| matches!(
            e,
            InstallEvent::Log { level: MessageType::WARNING, message } if message.contains("previously crashed")
        )));
    }

    #[test]
    fn test_install_outcome_data_dir() {
        let path = PathBuf::from("/test/path");

        assert_eq!(
            InstallOutcome::Success {
                data_dir: path.clone()
            }
            .data_dir(),
            Some(&path)
        );
        assert_eq!(
            InstallOutcome::SuccessWithWarnings {
                data_dir: path.clone()
            }
            .data_dir(),
            Some(&path)
        );
        assert_eq!(
            InstallOutcome::AlreadyExists {
                data_dir: path.clone()
            }
            .data_dir(),
            Some(&path)
        );

        assert_eq!(InstallOutcome::AlreadyInstalling.data_dir(), None);
        assert_eq!(InstallOutcome::Failed.data_dir(), None);
    }
}
