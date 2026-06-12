//! Query file downloading from nvim-treesitter repository.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::http::agent_with_timeout;

/// Base URL for nvim-treesitter query files on GitHub (main branch).
/// Note: In the main branch, queries are under runtime/queries instead of queries.
pub(crate) const NVIM_TREESITTER_QUERIES_URL: &str =
    "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/main/runtime/queries";

/// Query file types to download.
const QUERY_FILES: &[&str] = &["highlights.scm", "locals.scm", "injections.scm"];

/// HTTP timeout for query file downloads; keeps installs bounded when a
/// response stalls (query files are small text files, so 60s is generous).
const QUERY_HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Error types for query installation.
#[derive(Debug)]
pub enum QueryInstallError {
    /// The language is not supported (queries don't exist in nvim-treesitter).
    LanguageNotSupported(String),
    /// HTTP request failed.
    HttpError(String),
    /// File system operation failed.
    IoError(std::io::Error),
    /// Queries already exist and --force not specified.
    AlreadyExists(PathBuf),
}

impl std::fmt::Display for QueryInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LanguageNotSupported(lang) => {
                write!(
                    f,
                    "Language '{}' is not supported or queries not found in nvim-treesitter",
                    lang
                )
            }
            Self::HttpError(msg) => write!(f, "HTTP error: {}", msg),
            Self::IoError(e) => write!(f, "IO error: {}", e),
            Self::AlreadyExists(path) => {
                write!(
                    f,
                    "Queries already exist at {}. Use --force to overwrite.",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for QueryInstallError {}

impl From<std::io::Error> for QueryInstallError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError(e)
    }
}

/// Result of installing queries for a language.
pub struct QueryInstallResult {
    /// The language that was installed.
    pub language: String,
    /// Path where queries were installed.
    pub install_path: PathBuf,
    /// List of files that were downloaded.
    pub files_downloaded: Vec<String>,
}

/// Parse the `; inherits: lang1,lang2` directive from query content.
/// Returns the list of parent languages.
///
/// Names are used as path segments (`queries/<name>/`) and URL segments,
/// so anything outside nvim-treesitter's `[a-z0-9_]+` naming is dropped:
/// a `; inherits: ../../x` from a compromised or custom query source must
/// not escape the data dir.
fn parse_inherits_directive(content: &str) -> Vec<String> {
    let first_line = content.lines().next().unwrap_or("");
    if let Some(rest) = first_line.strip_prefix("; inherits:") {
        rest.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|s| {
                let safe = s
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
                if !safe {
                    eprintln!("Warning: ignoring unsafe inherited language name '{}'", s);
                }
                safe
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Download and install query files for a language, including inherited dependencies.
///
/// This recursively downloads parent queries (e.g., ecma, jsx for TypeScript).
pub fn install_queries_with_dependencies(
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    install_queries_with_dependencies_from(NVIM_TREESITTER_QUERIES_URL, language, data_dir, force)
}

/// Like [`install_queries_with_dependencies`] but downloading from `base_url`,
/// so tests can serve query files from a local HTTP server.
pub(crate) fn install_queries_with_dependencies_from(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    let mut installed = std::collections::HashSet::new();
    install_queries_recursive(base_url, language, data_dir, force, &mut installed)
}

/// Internal recursive helper for installing queries with dependencies.
fn install_queries_recursive(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    installed: &mut std::collections::HashSet<String>,
) -> Result<QueryInstallResult, QueryInstallError> {
    // Skip if already installed in this session
    if installed.contains(language) {
        return Ok(QueryInstallResult {
            language: language.to_string(),
            install_path: data_dir.join("queries").join(language),
            files_downloaded: vec![],
        });
    }

    let queries_dir = data_dir.join("queries").join(language);

    // Check if queries already exist
    if queries_dir.exists() && !force {
        // Mark as installed BEFORE recursing into parents: an inheritance
        // cycle among on-disk query files (self-inherit typo, A↔B) would
        // otherwise recurse forever and overflow the stack. The download
        // branch below already inserts before its parent loop.
        installed.insert(language.to_string());

        // Even if skipping, we need to check for inherited dependencies
        let highlights_path = queries_dir.join("highlights.scm");
        if highlights_path.exists()
            && let Ok(content) = std::fs::read_to_string(&highlights_path)
        {
            let parents = parse_inherits_directive(&content);
            for parent in parents {
                // Install parent dependencies (don't force, just ensure they exist)
                match install_queries_recursive(base_url, &parent, data_dir, false, installed) {
                    Ok(_) | Err(QueryInstallError::AlreadyExists(_)) => {}
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to install inherited queries '{}': {}",
                            parent, e
                        );
                    }
                }
            }
        }
        return Err(QueryInstallError::AlreadyExists(queries_dir));
    }

    // Create the queries directory
    fs::create_dir_all(&queries_dir)?;

    let mut files_downloaded = Vec::new();
    let mut any_success = false;
    let mut parents_to_install = Vec::new();

    // Download each query file
    for query_file in QUERY_FILES {
        let url = format!("{}/{}/{}", base_url, language, query_file);

        match download_file(&url) {
            Ok(content) => {
                // Check for inherits directive in highlights.scm
                if *query_file == "highlights.scm" {
                    parents_to_install = parse_inherits_directive(&content);
                }

                let file_path = queries_dir.join(query_file);
                let mut file = fs::File::create(&file_path)?;
                file.write_all(content.as_bytes())?;
                files_downloaded.push(query_file.to_string());
                any_success = true;
            }
            Err(e) => {
                // highlights.scm is required, others are optional
                if *query_file == "highlights.scm" {
                    // Clean up the directory we created
                    let _ = fs::remove_dir_all(&queries_dir);
                    return Err(QueryInstallError::LanguageNotSupported(
                        language.to_string(),
                    ));
                }
                // Log but continue for optional files
                eprintln!(
                    "Note: {} not available for {} ({})",
                    query_file, language, e
                );
            }
        }
    }

    if !any_success {
        let _ = fs::remove_dir_all(&queries_dir);
        return Err(QueryInstallError::LanguageNotSupported(
            language.to_string(),
        ));
    }

    installed.insert(language.to_string());

    // Install parent dependencies
    for parent in parents_to_install {
        eprintln!("Installing inherited queries: {}", parent);
        // Don't fail if parent already exists
        match install_queries_recursive(base_url, &parent, data_dir, false, installed) {
            Ok(_) | Err(QueryInstallError::AlreadyExists(_)) => {}
            Err(e) => {
                eprintln!(
                    "Warning: Failed to install inherited queries '{}': {}",
                    parent, e
                );
            }
        }
    }

    Ok(QueryInstallResult {
        language: language.to_string(),
        install_path: queries_dir,
        files_downloaded,
    })
}

/// Download a file from a URL.
fn download_file(url: &str) -> Result<String, QueryInstallError> {
    let mut response = agent_with_timeout(QUERY_HTTP_TIMEOUT)
        .get(url)
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => {
                QueryInstallError::HttpError(format!("HTTP {} for {}", code, url))
            }
            e => QueryInstallError::HttpError(e.to_string()),
        })?;

    response
        .body_mut()
        .read_to_string()
        .map_err(|e| QueryInstallError::HttpError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Inherited language names become path segments (`queries/<name>/`) and
    /// URL segments, so anything outside nvim-treesitter's `[a-z0-9_]+`
    /// naming must be dropped — `; inherits: ../../x` from a compromised or
    /// custom query source must not escape the data dir.
    #[test]
    fn parse_inherits_directive_drops_unsafe_language_names() {
        let parents = parse_inherits_directive(
            "; inherits: ../../evil, html_tags, UPPER, with-dash, c3\n(comment) @comment\n",
        );
        assert_eq!(
            parents,
            vec!["html_tags".to_string(), "c3".to_string()],
            "only lowercase/digit/underscore names may survive"
        );
    }

    #[test]
    fn test_install_queries_creates_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        // This test requires network access - skip in CI if needed
        let result = install_queries_with_dependencies("lua", &data_dir, false);

        // The test may fail due to network issues, but structure should be correct
        if let Ok(result) = result {
            assert_eq!(result.language, "lua");
            assert!(result.install_path.exists());
            assert!(
                result
                    .files_downloaded
                    .contains(&"highlights.scm".to_string())
            );
        }
    }

    #[test]
    fn install_with_dependencies_survives_inheritance_cycles_on_disk() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        // Self-cycle: a query file inheriting its own language (a one-word
        // typo in a real highlights.scm). No network: both branches hit the
        // already-exists path.
        let a_dir = data_dir.join("queries").join("cyclic_a");
        fs::create_dir_all(&a_dir).unwrap();
        std::fs::write(a_dir.join("highlights.scm"), "; inherits: cyclic_a\n").unwrap();

        let result = install_queries_with_dependencies("cyclic_a", &data_dir, false);
        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(_))),
            "self-inheriting installed queries must terminate with AlreadyExists"
        );

        // Mutual cycle between two installed languages.
        let b_dir = data_dir.join("queries").join("cyclic_b");
        let c_dir = data_dir.join("queries").join("cyclic_c");
        fs::create_dir_all(&b_dir).unwrap();
        fs::create_dir_all(&c_dir).unwrap();
        std::fs::write(b_dir.join("highlights.scm"), "; inherits: cyclic_c\n").unwrap();
        std::fs::write(c_dir.join("highlights.scm"), "; inherits: cyclic_b\n").unwrap();

        let result = install_queries_with_dependencies("cyclic_b", &data_dir, false);
        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(_))),
            "mutually-inheriting installed queries must terminate with AlreadyExists"
        );
    }

    #[test]
    fn test_install_queries_returns_error_for_nonexistent_language() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        let result =
            install_queries_with_dependencies("nonexistent_language_xyz_123", &data_dir, false);

        assert!(result.is_err());
        if let Err(QueryInstallError::LanguageNotSupported(lang)) = result {
            assert_eq!(lang, "nonexistent_language_xyz_123");
        }
    }

    #[test]
    fn test_install_queries_respects_force_flag() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let queries_dir = data_dir.join("queries").join("lua");

        // Create existing directory
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "existing content").unwrap();

        // Without force, should error
        let result = install_queries_with_dependencies("lua", &data_dir, false);
        assert!(matches!(result, Err(QueryInstallError::AlreadyExists(_))));

        // With force, should succeed (requires network)
        // Skip actual download test to avoid flaky CI
    }
}
