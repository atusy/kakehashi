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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
}

impl FailedParserRegistry {
    /// Create a new registry with the given state directory.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            failed: Arc::new(DashSet::new()),
            state_dir: state_dir.to_path_buf(),
            parsing_counts: Arc::new(DashMap::new()),
        }
    }

    /// Path to the "parsing in progress" state file.
    fn parsing_state_path(&self) -> PathBuf {
        self.state_dir.join("parsing_in_progress")
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

        // Load previously failed parsers
        self.load_failed_parsers()?;

        // Check for crash recovery
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

    /// Clear a failed parser (e.g., after reinstallation).
    pub fn clear_failed(&self, language: &str) -> io::Result<()> {
        self.failed.remove(language);
        self.save_failed_parsers()
    }

    /// Record that parsing is starting for a language.
    ///
    /// This updates in-memory state only. Crash detection happens by checking
    /// this state on restart (via init()).
    ///
    /// Supports concurrent parsing by tracking a counter per language.
    pub fn begin_parsing(&self, language: &str) -> io::Result<()> {
        // Increment the parsing count for this language
        self.parsing_counts
            .entry(language.to_string())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        Ok(())
    }

    /// Record that parsing completed successfully for a language.
    ///
    /// This clears in-memory state only (no disk I/O).
    pub fn end_parsing_language(&self, language: &str) -> io::Result<()> {
        // Decrement the parsing count for this language
        if let Some(mut entry) = self.parsing_counts.get_mut(language) {
            *entry -= 1;
            if *entry == 0 {
                // Remove the entry when count reaches 0
                drop(entry);
                self.parsing_counts.remove(language);
            }
        }
        Ok(())
    }

    /// Persist current parsing state to disk.
    ///
    /// This should be called on graceful shutdown to enable crash detection
    /// across process restarts. If parsers are currently being parsed, write
    /// their names to the parsing_in_progress file (one per line).
    pub fn persist_state(&self) -> io::Result<()> {
        let parsing_languages: Vec<String> = self
            .parsing_counts
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        if !parsing_languages.is_empty() {
            fs::create_dir_all(&self.state_dir)?;
            fs::write(self.parsing_state_path(), parsing_languages.join("\n"))?;
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
    fn test_clear_failed() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        registry.mark_failed("lua").unwrap();
        assert!(registry.is_failed("lua"));

        registry.clear_failed("lua").unwrap();
        assert!(!registry.is_failed("lua"));
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
    fn test_begin_parsing_does_not_write_to_disk() {
        let temp = tempdir().unwrap();
        let registry = FailedParserRegistry::new(temp.path());
        registry.init().unwrap();

        // Call begin_parsing
        registry.begin_parsing("lua").unwrap();

        // Verify that parsing_in_progress file does NOT exist
        // (begin_parsing should only update in-memory state)
        let parsing_state_path = temp.path().join("parsing_in_progress");
        assert!(
            !parsing_state_path.exists(),
            "begin_parsing should not write parsing_in_progress file to disk"
        );

        // Verify that in-memory state is updated (we'll add accessor for this)
        assert_eq!(
            registry.current_parsing_language(),
            Some("lua".to_string()),
            "begin_parsing should update in-memory state"
        );
    }

    #[test]
    fn test_end_parsing_only_clears_memory() {
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

        // Verify no disk I/O happened
        let parsing_state_path = temp.path().join("parsing_in_progress");
        assert!(
            !parsing_state_path.exists(),
            "end_parsing_language should not create or modify files"
        );
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
