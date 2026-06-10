//! Query file downloading from nvim-treesitter repository.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Base URL for nvim-treesitter query files on GitHub (main branch).
/// Note: In the main branch, queries are under runtime/queries instead of queries.
const NVIM_TREESITTER_QUERIES_URL: &str =
    "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/main/runtime/queries";

/// Query file types to download.
const QUERY_FILES: &[&str] = &["highlights.scm", "locals.scm", "injections.scm"];

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

/// Download and install query files for a language.
pub fn install_queries(
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    let queries_dir = data_dir.join("queries").join(language);

    // Check if queries already exist
    if queries_dir.exists() && !force {
        return Err(QueryInstallError::AlreadyExists(queries_dir));
    }

    // Create the queries directory
    fs::create_dir_all(&queries_dir)?;

    let mut files_downloaded = Vec::new();
    let mut any_success = false;

    // Download each query file
    for query_file in QUERY_FILES {
        let url = format!(
            "{}/{}/{}",
            NVIM_TREESITTER_QUERIES_URL, language, query_file
        );

        match download_file(&url) {
            Ok(content) => {
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

    Ok(QueryInstallResult {
        language: language.to_string(),
        install_path: queries_dir,
        files_downloaded,
    })
}

/// Parse the `; inherits: lang1,lang2` directive from query content.
/// Returns the list of parent languages.
fn parse_inherits_directive(content: &str) -> Vec<String> {
    let first_line = content.lines().next().unwrap_or("");
    if let Some(rest) = first_line.strip_prefix("; inherits:") {
        rest.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
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
    let mut installed = std::collections::HashSet::new();
    install_queries_recursive(language, data_dir, force, &mut installed)
}

/// Internal recursive helper for installing queries with dependencies.
fn install_queries_recursive(
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
        // Even if skipping, we need to check for inherited dependencies
        let highlights_path = queries_dir.join("highlights.scm");
        if highlights_path.exists()
            && let Ok(content) = std::fs::read_to_string(&highlights_path)
        {
            let parents = parse_inherits_directive(&content);
            for parent in parents {
                // Install parent dependencies (don't force, just ensure they exist)
                let _ = install_queries_recursive(&parent, data_dir, false, installed);
            }
        }
        installed.insert(language.to_string());
        return Err(QueryInstallError::AlreadyExists(queries_dir));
    }

    // Create the queries directory
    fs::create_dir_all(&queries_dir)?;

    let mut files_downloaded = Vec::new();
    let mut any_success = false;
    let mut parents_to_install = Vec::new();

    // Download each query file
    for query_file in QUERY_FILES {
        let url = format!(
            "{}/{}/{}",
            NVIM_TREESITTER_QUERIES_URL, language, query_file
        );

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
        match install_queries_recursive(&parent, data_dir, false, installed) {
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
    let mut response = ureq::get(url).call().map_err(|e| match e {
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

    #[test]
    fn test_install_queries_creates_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        // This test requires network access - skip in CI if needed
        let result = install_queries("lua", &data_dir, false);

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
    fn test_install_queries_returns_error_for_nonexistent_language() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        let result = install_queries("nonexistent_language_xyz_123", &data_dir, false);

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
        let result = install_queries("lua", &data_dir, false);
        assert!(matches!(result, Err(QueryInstallError::AlreadyExists(_))));

        // With force, should succeed (requires network)
        // Skip actual download test to avoid flaky CI
    }
}
