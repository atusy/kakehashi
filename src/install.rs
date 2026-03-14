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
    // --data-dir CLI flag override (highest priority)
    if let Some(dir) = crate::config::expand::data_dir_override() {
        return Some(dir.to_path_buf());
    }
    resolve_data_dir(|var| std::env::var(var).ok())
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
