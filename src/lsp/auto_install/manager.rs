//! Parser auto-install coordinator: dedupes concurrent installs
//! (`InstallingLanguages`), tracks crashed parsers (`FailedParserRegistry`),
//! and runs the install.
//!
//! Returns `InstallResult` with events instead of calling `ClientNotifier`
//! directly — keeps this module free of LSP infrastructure so it can be
//! unit-tested in isolation. Kakehashi gates on `is_auto_install_enabled()`,
//! calls `try_install()`, dispatches the events, and then handles
//! post-install coordination (settings update, language reload).

use std::{future::Future, path::PathBuf};
use tower_lsp_server::ls_types::MessageType;

use crate::install::support_check::{SkipReason, should_skip_unsupported_language};
use crate::language::FailedParserRegistry;

use super::{InstallingLanguages, InstallingLanguagesExt};

/// Result of an installation attempt with all events for Kakehashi to dispatch.
#[derive(Debug)]
pub(crate) struct InstallResult {
    /// What happened during the installation attempt
    pub outcome: InstallOutcome,
    /// Events to be dispatched to ClientNotifier by Kakehashi
    pub events: Vec<InstallEvent>,
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
    /// Check if the caller should skip parsing after this outcome.
    ///
    /// True when a reload will handle parsing (`Success`, `SuccessWithWarnings`,
    /// `AlreadyExists`) or another task is mid-install (`AlreadyInstalling`);
    /// false when no install happened or will (`ParserFailed`, `Unsupported`,
    /// `Failed`, `NoDataDir`).
    pub(crate) fn should_skip_parse(&self) -> bool {
        matches!(
            self,
            InstallOutcome::Success { .. }
                | InstallOutcome::SuccessWithWarnings { .. }
                | InstallOutcome::AlreadyExists { .. }
                | InstallOutcome::AlreadyInstalling
        )
    }

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
/// `AlreadyInstalling` (and, via `should_skip_parse`, never parsed) for the
/// server's lifetime. For the install itself the guard moves into the
/// spawned install task, so it tracks the blocking work's true lifetime
/// rather than the caller's.
struct InstallMarkerGuard {
    installing: InstallingLanguages,
    language: String,
}

impl Drop for InstallMarkerGuard {
    fn drop(&mut self) {
        self.installing.finish_install(&self.language);
    }
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
        }
    }

    /// Initialize the failed parser registry with crash detection.
    ///
    /// State storage location: `KAKEHASHI_STATE_DIR` if set, else the default
    /// data directory, falling back to a `kakehashi` dir under the OS temp dir
    /// if neither resolves. Crash-recovery state (`parsing_in_progress`,
    /// `failed_parsers`) is ephemeral and conceptually distinct from the
    /// persistent parser/query install assets in the data dir; the override
    /// lets it live elsewhere — e.g. so concurrent test processes that share one
    /// read-only install dir can isolate their crash state and not poison each
    /// other (a leftover `parsing_in_progress` from another process is otherwise
    /// read as a crash and marks that parser failed).
    /// If initialization fails, returns an empty registry.
    pub fn init_failed_parser_registry() -> FailedParserRegistry {
        let state_dir = std::env::var_os("KAKEHASHI_STATE_DIR")
            // An empty value resolves to the process cwd (writing crash files
            // wherever it was started), so treat it as unset — matching how
            // `resolve_data_dir` handles an empty `KAKEHASHI_DATA_DIR`.
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(crate::install::default_data_dir)
            // Platform-aware last resort (not a hard-coded `/tmp`, which doesn't
            // exist on Windows); only reached if the data dir can't resolve.
            .unwrap_or_else(|| std::env::temp_dir().join("kakehashi"));

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
            should_skip_unsupported_language(&language, fetch_options.as_ref()).await
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
        Fut: Future<Output = (bool, Option<SkipReason>)> + Send + 'static,
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
            };
        }

        // Claim before the network-backed support lookup so concurrent calls
        // for the same language do not all fetch metadata. The RAII guard
        // releases the claim on every lookup/error early return.
        if !self.installing_languages.try_start_install(language) {
            events.push(InstallEvent::Log {
                level: MessageType::INFO,
                message: format!("Language '{}' is already being installed", language),
            });
            return InstallResult {
                outcome: InstallOutcome::AlreadyInstalling,
                events,
            };
        }
        let install_marker = InstallMarkerGuard {
            installing: self.installing_languages.clone(),
            language: language.to_string(),
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
            let result = support_check(lookup_language, lookup_data_dir).await;
            (result, install_marker)
        });
        let ((should_skip, reason), install_marker) = match support_task.await {
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
                };
            }
        };

        if let Some(reason) = &reason {
            events.push(InstallEvent::Log {
                level: reason.message_type(),
                message: reason.message(),
            });
        }

        if should_skip {
            return InstallResult {
                outcome: InstallOutcome::Unsupported,
                events,
            };
        }

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
                return InstallResult {
                    outcome: InstallOutcome::NoDataDir,
                    events,
                };
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
            return InstallResult {
                outcome: InstallOutcome::AlreadyExists {
                    data_dir: data_dir.clone(),
                },
                events,
            };
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
            let _marker = install_marker;
            // Auto-install runs inside the LSP server, whose `current_exe()` is the
            // kakehashi binary — so the killable subprocess (re-exec
            // `__compile-parser`) is valid and bounds a hung `cc`.
            crate::install::install_language_async(
                task_lang,
                task_data_dir,
                false,
                crate::install::parser::ParserCompile::KillableSubprocess,
            )
            .await
        });
        let result = match install_task.await {
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
                };
            }
        };

        // Check if parser file exists after install attempt (even if queries failed)
        let parser_exists = crate::install::parser_file_exists(&lang, &data_dir).is_some();

        if result.is_success() {
            events.push(InstallEvent::ProgressEnd { success: true });
            events.push(InstallEvent::Log {
                level: MessageType::INFO,
                message: format!("Successfully installed language '{}'. Reloading...", lang),
            });
            InstallResult {
                outcome: InstallOutcome::Success {
                    data_dir: data_dir.clone(),
                },
                events,
            }
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

            InstallResult {
                outcome: InstallOutcome::SuccessWithWarnings {
                    data_dir: data_dir.clone(),
                },
                events,
            }
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

            InstallResult {
                outcome: InstallOutcome::Failed,
                events,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn create_test_manager() -> (AutoInstallManager, tempfile::TempDir) {
        let temp = tempdir().expect("Failed to create temp dir");
        let installing = InstallingLanguages::new();
        let failed = FailedParserRegistry::new(temp.path());
        failed.init().expect("Failed to init registry");
        (AutoInstallManager::new(installing, failed), temp)
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

        let guard = InstallMarkerGuard {
            installing: installing.clone(),
            language: "lua".to_string(),
        };
        drop(guard);

        assert!(
            installing.try_start_install("lua"),
            "marker must be released when the guard drops, even if try_install \
             is cancelled at its install await"
        );
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
                    (true, None)
                })
                .await
        });
        started_rx.await.expect("first support lookup must start");

        let duplicate_lookup_count = Arc::clone(&lookup_count);
        let result = manager
            .try_install_with_support_check("lua", move |_, _| {
                duplicate_lookup_count.fetch_add(1, Ordering::SeqCst);
                async { (false, None) }
            })
            .await;

        assert_eq!(result.outcome, InstallOutcome::AlreadyInstalling);
        assert_eq!(lookup_count.load(Ordering::SeqCst), 1);
        assert!(result.events.iter().any(|e| matches!(
            e,
            InstallEvent::Log { level: MessageType::INFO, message } if message.contains("already being installed")
        )));

        let _ = release_tx.send(());
        assert_eq!(
            first.await.expect("first install task must finish").outcome,
            InstallOutcome::Unsupported
        );
    }

    #[tokio::test]
    async fn skipped_support_lookup_releases_install_claim() {
        let (manager, _temp) = create_test_manager();

        let result = manager
            .try_install_with_support_check("unsupported", |_, _| async { (true, None) })
            .await;

        assert_eq!(result.outcome, InstallOutcome::Unsupported);
        assert!(
            manager
                .installing_languages
                .try_start_install("unsupported"),
            "unsupported and metadata-error exits share this skip path and must release the claim"
        );
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
                    (true, None)
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
                async { (false, None) }
            })
            .await;
        assert_eq!(duplicate.outcome, InstallOutcome::AlreadyInstalling);
        assert_eq!(duplicate_lookups.load(Ordering::SeqCst), 0);

        let _ = release_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !manager.installing_languages.try_start_install("lua") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the detached lookup task must release the claim after its support check finishes");
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
    fn test_install_outcome_should_skip_parse() {
        // Outcomes that should skip parse (installation handled or in progress)
        assert!(
            InstallOutcome::Success {
                data_dir: PathBuf::from("/tmp")
            }
            .should_skip_parse()
        );
        assert!(
            InstallOutcome::SuccessWithWarnings {
                data_dir: PathBuf::from("/tmp")
            }
            .should_skip_parse()
        );
        assert!(
            InstallOutcome::AlreadyExists {
                data_dir: PathBuf::from("/tmp")
            }
            .should_skip_parse()
        );
        // AlreadyInstalling: another task is installing, caller should wait
        assert!(InstallOutcome::AlreadyInstalling.should_skip_parse());

        // Outcomes that should NOT skip parse (no installation will happen)
        assert!(!InstallOutcome::ParserFailed.should_skip_parse());
        assert!(!InstallOutcome::Unsupported.should_skip_parse());
        assert!(!InstallOutcome::Failed.should_skip_parse());
        assert!(!InstallOutcome::NoDataDir.should_skip_parse());
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
