//! Registry for tracking parsers that have crashed.
//!
//! This module provides crash resilience by:
//! 1. Tracking which parsers are currently being used (parsing-in-progress state)
//! 2. Marking parsers as failed when crashes are detected
//! 3. Preventing failed parsers from being loaded again
//!
//! The design handles C assertion failures (SIGABRT) that cannot be caught:
//! - Before parsing, we record the parser being used to a state file
//! - If the process crashes, on restart we detect the crash and mark that parser as failed
//! - Failed parsers are skipped, allowing other languages to continue working
//!
//! Supports concurrent parsing by tracking per-language parsing counts.

use dashmap::{DashMap, DashSet};
use fs4::fs_std::FileExt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const PARSING_MARKER_PREFIX: &str = "parsing_in_progress.";

fn is_lock_contended(error: &io::Error) -> bool {
    error.raw_os_error() == fs4::lock_contended_error().raw_os_error()
}

struct SessionMarker {
    state: std::sync::Mutex<MarkerState>,
}

struct MarkerState {
    path: PathBuf,
    file: fs::File,
    /// Old markers whose cleanup failed stay locked so live peers never
    /// misclassify their stale contents as a crash.
    retired: Vec<(PathBuf, fs::File)>,
}

/// Registry for tracking failed parsers.
///
/// Thread-safe registry that persists failed parser state to disk
/// to survive process restarts.
#[derive(Clone)]
pub(crate) struct FailedParserRegistry {
    /// In-memory set of failed parsers for fast lookup
    failed: Arc<DashSet<String>>,
    /// Directory where state files are stored
    state_dir: PathBuf,
    /// Per-language parsing counts for concurrent crash detection
    /// Key: language name, Value: number of concurrent parses
    parsing_counts: Arc<DashMap<String, usize>>,
    /// Serializes count transitions with their durable marker update.
    persistence_lock: Arc<std::sync::Mutex<()>>,
    /// This session's exclusively locked marker. A peer can distinguish this
    /// live owner from an unlocked marker left by a crashed process.
    session_marker: Arc<OnceLock<Arc<SessionMarker>>>,
}

impl FailedParserRegistry {
    /// Create a new registry with the given state directory.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            failed: Arc::new(DashSet::new()),
            state_dir: state_dir.to_path_buf(),
            parsing_counts: Arc::new(DashMap::new()),
            persistence_lock: Arc::new(std::sync::Mutex::new(())),
            session_marker: Arc::new(OnceLock::new()),
        }
    }

    /// Path to the legacy single-session marker used by older versions.
    fn parsing_state_path(&self) -> PathBuf {
        self.state_dir.join("parsing_in_progress")
    }

    fn session_marker_path(&self) -> PathBuf {
        self.state_dir
            .join(format!("{PARSING_MARKER_PREFIX}{}", ulid::Ulid::new()))
    }

    /// Path to the "failed parsers" list file.
    fn failed_parsers_path(&self) -> PathBuf {
        self.state_dir.join("failed_parsers")
    }

    /// Initialize the registry by checking for crash recovery.
    ///
    /// This should be called on server startup. If a previous parsing
    /// operation was in progress (crash detected), mark those parsers as failed.
    pub fn init(&self) -> io::Result<()> {
        // Ensure state directory exists
        fs::create_dir_all(&self.state_dir)?;

        // Serialize recovery scanning with marker creation. Otherwise a peer
        // could observe a newly-created marker before its owner locks it and
        // misclassify the live session as crashed.
        let init_lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.state_dir.join("crash_recovery.lock"))?;
        init_lock.lock_exclusive()?;

        // Load only after acquiring the cross-process lock. A peer may have
        // recovered another marker while this process waited; loading earlier
        // would let our later save overwrite and forget that quarantine.
        self.load_failed_parsers()?;

        // Recover the legacy single marker written by older versions.
        let parsing_state = self.parsing_state_path();
        if parsing_state.exists() {
            // Previous parsing was interrupted - crash detected!
            if let Ok(content) = fs::read_to_string(&parsing_state) {
                for line in content.lines() {
                    let language = line.trim();
                    if !language.is_empty() {
                        log::error!(
                            target: "kakehashi::crash_recovery",
                            "Detected crash during parsing of '{}'. Marking as failed.",
                            language
                        );
                        self.mark_failed(language)?;
                    }
                }
            }
            // Clean up state file
            let _ = fs::remove_file(&parsing_state);
        }

        // Recover only unlocked per-session markers. A locked marker belongs
        // to another live kakehashi process sharing this state directory.
        for entry in fs::read_dir(&self.state_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with(PARSING_MARKER_PREFIX) {
                continue;
            }
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())?;
            match file.try_lock_exclusive() {
                Ok(()) => {
                    let mut content = String::new();
                    file.read_to_string(&mut content)?;
                    self.mark_languages_failed(&content)?;
                    drop(file);
                    let _ = fs::remove_file(entry.path());
                }
                Err(error) if is_lock_contended(&error) => {}
                Err(error) => return Err(error),
            }
        }

        if self.session_marker.get().is_none() {
            let path = self.session_marker_path();
            let file = fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)?;
            file.lock_exclusive()?;
            let marker = Arc::new(SessionMarker {
                state: std::sync::Mutex::new(MarkerState {
                    path,
                    file,
                    retired: Vec::new(),
                }),
            });
            let _ = self.session_marker.set(marker);
        }

        Ok(())
    }

    fn mark_languages_failed(&self, content: &str) -> io::Result<()> {
        for line in content.lines() {
            let language = line.trim();
            if !language.is_empty() {
                log::error!(
                    target: "kakehashi::crash_recovery",
                    "Detected crash during parsing of '{}'. Marking as failed.",
                    language
                );
                self.mark_failed(language)?;
            }
        }
        Ok(())
    }

    /// Load the list of failed parsers from disk.
    fn load_failed_parsers(&self) -> io::Result<()> {
        let path = self.failed_parsers_path();
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            for line in content.lines() {
                let lang = line.trim();
                if !lang.is_empty() {
                    self.failed.insert(lang.to_string());
                }
            }
        }
        Ok(())
    }

    /// Save the list of failed parsers to disk.
    fn save_failed_parsers(&self) -> io::Result<()> {
        let path = self.failed_parsers_path();
        let languages: Vec<String> = self.failed.iter().map(|r| r.clone()).collect();
        fs::write(&path, languages.join("\n"))
    }

    /// Check if a parser has failed previously.
    pub fn is_failed(&self, language: &str) -> bool {
        self.failed.contains(language)
    }

    /// Mark a parser as failed.
    pub fn mark_failed(&self, language: &str) -> io::Result<()> {
        self.failed.insert(language.to_string());
        self.save_failed_parsers()
    }

    /// Record that parsing is starting for a language.
    ///
    /// The durable marker is updated before returning, so an uncatchable crash
    /// in the native parser still leaves recovery evidence for the next run.
    ///
    /// Supports concurrent parsing by tracking a counter per language.
    pub fn begin_parsing(&self, language: &str) -> io::Result<()> {
        let _guard = self
            .persistence_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Increment the parsing count for this language
        self.parsing_counts
            .entry(language.to_string())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        if let Err(error) = self.persist_current_state() {
            self.decrement_parsing_count(language);
            return Err(error);
        }
        Ok(())
    }

    /// Record that parsing completed successfully for a language.
    ///
    /// This updates or clears the durable marker after the in-memory count.
    pub fn end_parsing_language(&self, language: &str) -> io::Result<()> {
        let _guard = self
            .persistence_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.decrement_parsing_count(language);
        self.persist_current_state()
    }

    fn decrement_parsing_count(&self, language: &str) {
        if let Some(mut entry) = self.parsing_counts.get_mut(language) {
            *entry -= 1;
            if *entry == 0 {
                // Remove the entry when count reaches 0
                drop(entry);
                self.parsing_counts.remove(language);
            }
        }
    }

    /// Force the current active-parser set into this session's durable marker.
    ///
    /// Begin/end transitions already persist this state synchronously. This
    /// explicit flush remains available to lifecycle paths such as shutdown.
    pub fn persist_state(&self) -> io::Result<()> {
        let _guard = self
            .persistence_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.persist_current_state()
    }

    fn persist_current_state(&self) -> io::Result<()> {
        let parsing_languages: Vec<String> = self
            .parsing_counts
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        let marker = self
            .session_marker
            .get()
            .ok_or_else(|| io::Error::other("crash recovery registry is not initialized"))?;
        let contents = parsing_languages.join("\n");
        let mut current = marker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Build and durably flush a replacement inode without touching the
        // currently locked marker. Any create/write/sync failure therefore
        // leaves the prior active-language evidence intact.
        let next_path = self.session_marker_path();
        let temp_path = self
            .state_dir
            .join(format!(".parsing_marker_next.{}", ulid::Ulid::new()));
        let replacement = (|| -> io::Result<fs::File> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&temp_path)?;
            file.lock_exclusive()?;
            file.write_all(contents.as_bytes())?;
            file.sync_data()?;
            fs::rename(&temp_path, &next_path)?;
            Ok(file)
        })();
        let replacement = match replacement {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
        };

        // The replacement is now visible, durable, and already locked. Swap
        // ownership before releasing the old lock, so startup scanning can
        // never observe a gap with no live marker.
        let old_path = std::mem::replace(&mut current.path, next_path);
        let old_file = std::mem::replace(&mut current.file, replacement);
        if let Err(remove_error) = fs::remove_file(&old_path) {
            // Windows normally cannot unlink an open locked file. Clear its
            // stale contents while it is still locked, then retry after close.
            // If either operation fails, retain the lock for this session;
            // safety prefers a later false quarantine over a live peer reading
            // stale evidence now.
            let cleared = old_file.set_len(0).and_then(|()| old_file.sync_data());
            if cleared.is_ok() {
                drop(old_file);
                if let Err(error) = fs::remove_file(&old_path) {
                    log::warn!(
                        target: "kakehashi::crash_recovery",
                        "Failed to remove retired parser marker after clearing it: {}",
                        error
                    );
                }
            } else {
                log::warn!(
                    target: "kakehashi::crash_recovery",
                    "Failed to retire parser marker (unlink: {}; clear: {:?}); retaining lock",
                    remove_error,
                    cleared.err()
                );
                current.retired.push((old_path, old_file));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl FailedParserRegistry {
    fn failed_parsers(&self) -> Vec<String> {
        self.failed.iter().map(|r| r.clone()).collect()
    }

    fn clear_all(&self) -> io::Result<()> {
        self.failed.clear();
        let path = self.failed_parsers_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn session_marker_contents(state_dir: &Path) -> Vec<String> {
        fs::read_dir(state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(PARSING_MARKER_PREFIX)
            })
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect()
    }

    impl FailedParserRegistry {
        /// Get the currently parsing languages (test helper).
        fn current_parsing_language(&self) -> Option<String> {
            // For backward compatibility with single-language tests, return first language
            self.parsing_counts
                .iter()
                .next()
                .map(|entry| entry.key().clone())
        }
    }

    #[test]
    fn test_new_registry_has_no_failed_parsers() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        assert!(!registry.is_failed("lua"));
        assert!(!registry.is_failed("rust"));
        assert!(registry.failed_parsers().is_empty());
    }

    #[test]
    fn test_mark_and_check_failed() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        registry.mark_failed("lua").unwrap();

        assert!(registry.is_failed("lua"));
        assert!(!registry.is_failed("rust"));
        assert_eq!(registry.failed_parsers(), vec!["lua"]);
    }

    #[test]
    fn test_failed_parsers_persist_across_restarts() {
        let temp = tempdir().unwrap();

        // First "session"
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            registry.mark_failed("yaml").unwrap();
        }

        // Second "session" - should load persisted state
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            assert!(registry.is_failed("yaml"));
        }
    }

    #[test]
    fn test_crash_detection_marks_parser_failed() {
        let temp = tempdir().unwrap();

        // Simulate a crash: begin_parsing but never end_parsing
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            registry.begin_parsing("yaml").unwrap();
            // Simulated crash - persist state shows parsing was in progress
            registry.persist_state().unwrap();
            // No end_parsing() called - simulates crash during parsing
        }

        // Restart and init should detect the crash
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            assert!(registry.is_failed("yaml"));
        }
    }

    #[test]
    fn test_crash_detection_does_not_require_shutdown_persistence() {
        let temp = tempdir().unwrap();

        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            registry.begin_parsing("lua").unwrap();
            // Simulate an uncatchable parser crash: neither normal parse cleanup
            // nor the process lifecycle's graceful-shutdown hook can run.
        }

        let restarted = FailedParserRegistry::new(temp.path());
        restarted.init().unwrap();

        assert!(
            restarted.is_failed("lua"),
            "an active parser must be recoverable without graceful shutdown"
        );
    }

    #[test]
    fn test_live_peer_marker_is_not_treated_as_a_crash() {
        let temp = tempdir().unwrap();
        let first = FailedParserRegistry::new(temp.path());
        first.init().unwrap();
        first.begin_parsing("lua").unwrap();

        let second = FailedParserRegistry::new(temp.path());
        second.init().unwrap();

        assert!(
            !second.is_failed("lua"),
            "a live peer's active parser must not be quarantined"
        );
        first.end_parsing_language("lua").unwrap();
    }

    #[test]
    fn test_recovery_preserves_failures_recorded_by_a_peer() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("failed_parsers"), "lua").unwrap();
        fs::write(
            temp.path().join(format!("{PARSING_MARKER_PREFIX}stale")),
            "rust",
        )
        .unwrap();

        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        assert!(registry.is_failed("lua"));
        assert!(registry.is_failed("rust"));
        let persisted = fs::read_to_string(temp.path().join("failed_parsers")).unwrap();
        assert!(persisted.lines().any(|language| language == "lua"));
        assert!(persisted.lines().any(|language| language == "rust"));
    }

    #[cfg(unix)]
    #[test]
    fn test_failed_replacement_preserves_existing_active_evidence() {
        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let moved_dir = temp.path().join("moved-state");
        let registry = FailedParserRegistry::new(&state_dir);
        registry.init().unwrap();
        registry.begin_parsing("rust").unwrap();

        // Make creation of the transactional replacement fail without touching
        // the already-open marker inode.
        fs::rename(&state_dir, &moved_dir).unwrap();
        assert!(registry.begin_parsing("lua").is_err());

        assert_eq!(session_marker_contents(&moved_dir), vec!["rust"]);
        assert_eq!(registry.current_parsing_language().as_deref(), Some("rust"));
    }

    #[test]
    fn test_successful_parsing_does_not_mark_failed() {
        let temp = tempdir().unwrap();

        // Normal parsing flow
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            registry.begin_parsing("lua").unwrap();
            registry.end_parsing_language("lua").unwrap();
        }

        // Restart should not see lua as failed
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            assert!(!registry.is_failed("lua"));
        }
    }

    #[test]
    fn test_clear_all() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        registry.mark_failed("lua").unwrap();
        registry.mark_failed("rust").unwrap();

        registry.clear_all().unwrap();

        assert!(!registry.is_failed("lua"));
        assert!(!registry.is_failed("rust"));
        assert!(registry.failed_parsers().is_empty());
    }

    #[test]
    fn test_init_detects_crash_and_marks_failed() {
        let temp = tempdir().unwrap();

        // Simulate a crash: begin_parsing but never end_parsing
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            registry.begin_parsing("zsh").unwrap();
            // Simulated crash - persist state before process terminates
            registry.persist_state().unwrap();
            // No end_parsing() called - simulates crash during parsing
        }

        // Restart and init should detect the crash
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();
            // The crashed parser should be marked as failed
            assert!(registry.is_failed("zsh"));
        }
    }

    #[test]
    fn test_init_no_crash_no_failed_parsers() {
        let temp = tempdir().unwrap();

        // Normal startup - no crash
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();
        // No parsers should be marked as failed
        assert!(registry.failed_parsers().is_empty());
    }

    #[test]
    fn test_begin_parsing_writes_crash_marker() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        // Call begin_parsing
        registry.begin_parsing("lua").unwrap();

        // Verify that crash evidence is durable before native parsing begins.
        assert_eq!(session_marker_contents(temp.path()), vec!["lua"]);

        // Verify that in-memory state is updated (we'll add accessor for this)
        assert_eq!(
            registry.current_parsing_language(),
            Some("lua".to_string()),
            "begin_parsing should update in-memory state"
        );
    }

    #[test]
    fn test_end_parsing_clears_crash_marker() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        // Start parsing
        registry.begin_parsing("rust").unwrap();
        assert_eq!(
            registry.current_parsing_language(),
            Some("rust".to_string())
        );

        // End parsing
        registry.end_parsing_language("rust").unwrap();

        // Verify in-memory state is cleared
        assert_eq!(
            registry.current_parsing_language(),
            None,
            "end_parsing_language should clear in-memory state"
        );

        // Successful completion removes the recovery evidence.
        assert_eq!(session_marker_contents(temp.path()), vec![""]);
    }

    #[test]
    fn test_concurrent_parsing_crash_recovery_identifies_correct_parser() {
        let temp = tempdir().unwrap();

        // Simulate concurrent parsing: start lua, then start rust, then crash rust
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();

            // Start parsing lua
            registry.begin_parsing("lua").unwrap();

            // Start parsing rust (concurrent with lua)
            registry.begin_parsing("rust").unwrap();

            // Lua finishes successfully
            registry.end_parsing_language("lua").unwrap();

            // Crash happens during rust parsing (rust never calls end_parsing)
            registry.persist_state().unwrap();
            // No end_parsing("rust") called - simulates crash during rust parsing
        }

        // Restart and init should detect the crash
        {
            let registry = FailedParserRegistry::new(temp.path());
            registry.init().unwrap();

            // Only rust should be marked as failed (it was still parsing when crash happened)
            assert!(
                registry.is_failed("rust"),
                "rust should be marked as failed - it was parsing when crash occurred"
            );
            assert!(
                !registry.is_failed("lua"),
                "lua should NOT be marked as failed - it completed successfully before crash"
            );
        }
    }
}
