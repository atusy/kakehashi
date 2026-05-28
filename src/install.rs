//! Installation module for Tree-sitter parsers and queries.
//!
//! This module provides functionality to download and install Tree-sitter
//! query files and compile parser shared libraries.

pub(crate) mod cache;
pub mod metadata;
pub mod parser;
pub mod queries;
pub(crate) mod support_check;

/// Test helper module for setting up mock metadata cache.
#[cfg(test)]
pub mod test_helpers {
    use super::cache::MetadataCache;
    use std::path::Path;

    /// Set up a mock metadata cache with the given content.
    ///
    /// This writes the provided content to the cache file in the given directory,
    /// allowing tests to use a controlled set of parsers.lua data without HTTP requests.
    pub fn setup_mock_metadata_cache(data_dir: &Path, content: &str) {
        let cache = MetadataCache::with_default_ttl(data_dir);
        cache.write(content).expect("Failed to write mock cache");
    }
}

pub(crate) use parser::parser_file_exists;

use std::path::PathBuf;

/// Get the default data directory for kakehashi.
///
/// Platform-specific paths:
/// - Linux: ~/.local/share/kakehashi/
/// - macOS: ~/Library/Application Support/kakehashi/
/// - Windows: %APPDATA%/kakehashi/
pub fn default_data_dir() -> Option<PathBuf> {
    // In `cfg(test)`, redirect every caller (Kakehashi::new -> failed
    // parser registry, search-path defaulting, etc.) to a project-local
    // persistent dir under `deps/` so the developer's real
    // `~/.local/share/kakehashi/` is never read or written. The dir is
    // shared across the test process to keep parser/query installs
    // cached between runs; transient crash-recovery files
    // (`parsing_in_progress`, `failed_parsers`) are cleared once per
    // process at first call so a prior E2E shutdown can't taint this run.
    #[cfg(test)]
    {
        return Some(test_data_dir());
    }

    #[cfg_attr(test, allow(unreachable_code))]
    {
        // --data-dir CLI flag override (highest priority)
        if let Some(dir) = crate::config::expand::data_dir_override() {
            return Some(dir.to_path_buf());
        }
        resolve_data_dir(|var| std::env::var(var).ok())
    }
}

/// Project-local persistent data directory used by unit tests.
///
/// Lives under `deps/` (already gitignored). Parser/query installs persist
/// across runs to avoid re-downloading, while crash-recovery state files
/// are cleared once per test process so a previous test's leftovers
/// (typically from an E2E binary that exited mid-parse) don't poison this
/// run. Returns the same path every call within a process via `OnceLock`.
#[cfg(test)]
pub(crate) fn test_data_dir() -> PathBuf {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = test_support::test_data_dir_path();
        let _ = std::fs::create_dir_all(&dir);
        // Crash-recovery state can be left over from a previous E2E
        // shutdown; clear it once per test process so init() doesn't
        // pre-mark languages as failed.
        let _ = std::fs::remove_file(dir.join("parsing_in_progress"));
        let _ = std::fs::remove_file(dir.join("failed_parsers"));
        // Auto-install required parsers + queries on first call per
        // process. Cached on disk via the `.installed` marker so
        // subsequent test processes are fast.
        let _ = test_support::ensure_test_languages_installed(&dir);
        dir
    })
    .clone()
}

/// Test-support helpers exposed to integration tests under `tests/`.
///
/// Integration tests can't see `cfg(test)`-gated items in the lib (they
/// link against the non-test build), so the shared test-data-dir wiring
/// lives in a regular `pub` module and is just routed through
/// `test_data_dir()` for lib unit tests.
///
/// Not part of the public LSP-server API — intended only for the
/// in-tree test harness.
pub mod test_support {
    use super::{parser, queries};
    use std::path::{Path, PathBuf};

    /// Languages whose parsers and queries every kakehashi test relies on.
    ///
    /// Mirrors the `make deps/tree-sitter/.installed` Makefile target.
    /// Tests that open lua / markdown / rust / yaml documents expect
    /// parsers and highlight queries to already be present; without
    /// them, semantic-tokens and similar end-to-end tests come back
    /// empty.
    pub const TEST_LANGUAGES: &[&str] = &[
        "lua",
        "markdown",
        "markdown_inline",
        "python",
        "rust",
        "yaml",
    ];

    /// Project-local persistent test data directory under `deps/test/`.
    ///
    /// `/deps` is already gitignored, so this dir doesn't pollute git.
    /// Leaf is `kakehashi` so search-path defaulting (which asserts the
    /// dir name) still matches production semantics.
    pub fn test_data_dir_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("deps")
            .join("test")
            .join("kakehashi")
    }

    /// Install every language in [`TEST_LANGUAGES`] into `data_dir` if
    /// the `.installed` marker is missing, then write the marker.
    ///
    /// Mirrors the install loop in `make deps/tree-sitter/.installed`.
    /// Both `install_parser` and `install_queries` short-circuit when a
    /// language is up-to-date; any genuine failure is logged so tests
    /// depending on that language fail with a clearer error rather than
    /// the whole suite panicking in setup.
    pub fn ensure_test_languages_installed(data_dir: &Path) -> std::io::Result<()> {
        let marker = data_dir.join(".installed");
        if marker.exists() {
            return Ok(());
        }
        let parser_options = parser::InstallOptions {
            data_dir: data_dir.to_path_buf(),
            force: false,
            verbose: false,
            no_cache: false,
        };
        for lang in TEST_LANGUAGES {
            if let Err(e) = parser::install_parser(lang, &parser_options) {
                eprintln!("[test setup] install_parser({}) failed: {}", lang, e);
            }
            if let Err(e) = queries::install_queries(lang, data_dir, false) {
                let msg = e.to_string();
                if !msg.contains("already exists") {
                    eprintln!("[test setup] install_queries({}) failed: {}", lang, e);
                }
            }
        }
        std::fs::write(&marker, "")
    }
}

/// Resolve the data directory from an env lookup and platform default.
///
/// Separated from `default_data_dir()` so tests can inject a mock env
/// without `unsafe` env var mutation.
fn resolve_data_dir(env_fn: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(val) = env_fn("KAKEHASHI_DATA_DIR")
        && !val.is_empty()
    {
        return Some(PathBuf::from(val));
    }
    dirs::data_dir().map(|p| p.join("kakehashi"))
}

/// Result of installing a language (both parser and queries).
#[derive(Debug)]
pub(crate) struct InstallResult {
    /// Path where the parser was installed, if successful.
    pub(crate) parser_path: Option<PathBuf>,
    /// Path where queries were installed, if successful.
    pub(crate) queries_path: Option<PathBuf>,
    /// Error message if parser install failed.
    pub(crate) parser_error: Option<String>,
    /// Error message if queries install failed.
    pub(crate) queries_error: Option<String>,
}

impl InstallResult {
    /// Check if the installation was fully successful.
    pub(crate) fn is_success(&self) -> bool {
        self.parser_error.is_none() && self.queries_error.is_none()
    }
}

/// Install a language asynchronously (both parser and queries).
///
/// This wraps the blocking install functions in `spawn_blocking` for use
/// in async contexts like the LSP server.
///
/// # Arguments
/// * `language` - The language to install (e.g., "lua", "rust")
/// * `data_dir` - The base data directory for kakehashi
/// * `force` - Whether to overwrite existing files
pub(crate) async fn install_language_async(
    language: String,
    data_dir: PathBuf,
    force: bool,
) -> InstallResult {
    let lang = language.clone();
    let dir = data_dir.clone();

    // Run blocking install operations in a separate thread pool
    tokio::task::spawn_blocking(move || {
        let mut result = InstallResult {
            parser_path: None,
            queries_path: None,
            parser_error: None,
            queries_error: None,
        };

        // Install parser
        // For async/auto-install, always use cache (background operation)
        let parser_options = parser::InstallOptions {
            data_dir: dir.clone(),
            force,
            verbose: false,
            no_cache: false,
        };

        match parser::install_parser(&lang, &parser_options) {
            Ok(parser_result) => {
                result.parser_path = Some(parser_result.install_path);
            }
            Err(e) => {
                result.parser_error = Some(e.to_string());
            }
        }

        // Install queries
        match queries::install_queries(&lang, &dir, force) {
            Ok(query_result) => {
                result.queries_path = Some(query_result.install_path);
            }
            Err(e) => {
                result.queries_error = Some(e.to_string());
            }
        }

        result
    })
    .await
    .unwrap_or_else(|e| InstallResult {
        parser_path: None,
        queries_path: None,
        parser_error: Some(format!("Task panicked: {}", e)),
        queries_error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_data_dir_returns_some() {
        let dir = default_data_dir();
        assert!(dir.is_some());
        let path = dir.unwrap();
        assert!(path.to_string_lossy().contains("kakehashi"));
    }

    #[test]
    fn test_resolve_data_dir_uses_env_var_when_set() {
        let env = |var: &str| match var {
            "KAKEHASHI_DATA_DIR" => Some("/custom/data/dir".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_data_dir(env),
            Some(PathBuf::from("/custom/data/dir"))
        );
    }

    #[test]
    fn test_resolve_data_dir_ignores_empty_env_var() {
        let env = |var: &str| match var {
            "KAKEHASHI_DATA_DIR" => Some(String::new()),
            _ => None,
        };
        let dir = resolve_data_dir(env);
        // Should fall back to platform default, not return empty path
        assert!(dir.is_some());
        assert!(dir.unwrap().to_string_lossy().contains("kakehashi"));
    }

    #[test]
    fn test_resolve_data_dir_falls_back_when_env_unset() {
        let env = |_: &str| None;
        let dir = resolve_data_dir(env);
        // Should use platform default (dirs::data_dir() + "kakehashi")
        assert!(dir.is_some());
        assert!(dir.unwrap().to_string_lossy().contains("kakehashi"));
    }

    #[test]
    fn test_install_result_success_check() {
        let success = InstallResult {
            parser_path: Some(PathBuf::from("/tmp/parser")),
            queries_path: Some(PathBuf::from("/tmp/queries")),
            parser_error: None,
            queries_error: None,
        };
        assert!(success.is_success());

        let failure = InstallResult {
            parser_path: None,
            queries_path: None,
            parser_error: Some("Parser failed".to_string()),
            queries_error: Some("Queries failed".to_string()),
        };
        assert!(!failure.is_success());
    }
}
