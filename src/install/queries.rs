//! Query file downloading from nvim-treesitter repository.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::http::agent_with_timeout;
#[cfg(test)]
use super::http::agent_with_timeout_allowing_http;

/// Base URL for nvim-treesitter query files on GitHub (main branch).
/// Note: In the main branch, queries are under runtime/queries instead of queries.
pub(crate) const NVIM_TREESITTER_QUERIES_URL: &str =
    "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/main/runtime/queries";

/// Query file types to download.
const QUERY_FILES: &[&str] = &["highlights.scm", "injections.scm"];

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
    /// HTTP response returned a structured non-success status code.
    HttpStatus { code: u16, url: String },
    /// Plain HTTP was rejected by the production HTTPS-only policy.
    HttpsOnly { url: String },
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
            Self::HttpStatus { code, url } => write!(f, "HTTP {} for {}", code, url),
            Self::HttpsOnly { url } => write!(f, "HTTPS-only policy rejected {}", url),
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

/// Whether a language name is safe to use as a path and URL segment.
///
/// Language names are used as path segments (`queries/<name>/`) and URL
/// segments, so anything outside nvim-treesitter's `[a-z0-9_]+` naming is
/// rejected: a name like `../../x` (from a caller or a `; inherits:` line in
/// a compromised or custom query source) must not escape the data dir.
pub(crate) fn is_safe_language_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Parse the `; inherits: lang1,lang2` directive from query content.
/// Returns the list of parent languages, dropping unsafe names
/// (see [`is_safe_language_name`]).
fn parse_inherits_directive(content: &str) -> Vec<String> {
    let first_line = content.lines().next().unwrap_or("");
    if let Some(rest) = first_line.strip_prefix("; inherits:") {
        rest.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|s| {
                let safe = is_safe_language_name(s);
                if !safe {
                    // Debug-format: the name is untrusted input and could
                    // smuggle ANSI escapes into the terminal if printed raw.
                    eprintln!("Warning: ignoring unsafe inherited language name {:?}", s);
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
    install_queries_with_dependencies_from_with_http_policy(
        NVIM_TREESITTER_QUERIES_URL,
        language,
        data_dir,
        force,
        QueryHttpPolicy::HttpsOnly,
    )
}

/// Like [`install_queries_with_dependencies`] but downloading from `base_url`.
pub(crate) fn install_queries_with_dependencies_from(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    install_queries_with_dependencies_from_with_http_policy(
        base_url,
        language,
        data_dir,
        force,
        QueryHttpPolicy::HttpsOnly,
    )
}

/// Like [`install_queries_with_dependencies_from`] but disables the HTTPS-only
/// policy for tests that serve fixture query files over local plain HTTP.
#[cfg(test)]
pub(crate) fn install_queries_with_dependencies_from_allowing_http_for_tests(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    install_queries_with_dependencies_from_with_http_policy(
        base_url,
        language,
        data_dir,
        force,
        QueryHttpPolicy::AllowHttpForTests,
    )
}

#[derive(Clone, Copy)]
enum QueryHttpPolicy {
    HttpsOnly,
    #[cfg(test)]
    AllowHttpForTests,
}

fn install_queries_with_dependencies_from_with_http_policy(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    http_policy: QueryHttpPolicy,
) -> Result<QueryInstallResult, QueryInstallError> {
    if !is_safe_language_name(language) {
        return Err(QueryInstallError::LanguageNotSupported(
            language.escape_default().to_string(),
        ));
    }
    let mut installed = std::collections::HashSet::new();
    install_queries_recursive(
        base_url,
        language,
        data_dir,
        force,
        &mut installed,
        http_policy,
    )
}

fn validate_base_url_http_policy(
    base_url: &str,
    http_policy: QueryHttpPolicy,
) -> Result<(), QueryInstallError> {
    match http_policy {
        QueryHttpPolicy::HttpsOnly if base_url.starts_with("http://") => {
            Err(QueryInstallError::HttpsOnly {
                url: base_url.to_string(),
            })
        }
        _ => Ok(()),
    }
}

/// Internal recursive helper for installing queries with dependencies.
fn install_queries_recursive(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    installed: &mut std::collections::HashSet<String>,
    http_policy: QueryHttpPolicy,
) -> Result<QueryInstallResult, QueryInstallError> {
    // The name becomes a path and URL segment below; reject anything that
    // could escape the data dir (e.g. a caller-provided `../../x`).
    // Escape the untrusted name: the error's Display is printed raw by
    // the CLI, so control characters must not reach the terminal.
    if !is_safe_language_name(language) {
        return Err(QueryInstallError::LanguageNotSupported(
            language.escape_default().to_string(),
        ));
    }

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
                match install_queries_recursive(
                    base_url,
                    &parent,
                    data_dir,
                    false,
                    installed,
                    http_policy,
                ) {
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

        match download_file(&url, http_policy) {
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
                    return match e {
                        QueryInstallError::HttpStatus { code: 404, .. } => Err(
                            QueryInstallError::LanguageNotSupported(language.to_string()),
                        ),
                        other => Err(other),
                    };
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
        match install_queries_recursive(base_url, &parent, data_dir, false, installed, http_policy)
        {
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
fn download_file(url: &str, http_policy: QueryHttpPolicy) -> Result<String, QueryInstallError> {
    validate_base_url_http_policy(url, http_policy)?;
    let agent = match http_policy {
        QueryHttpPolicy::HttpsOnly => agent_with_timeout(QUERY_HTTP_TIMEOUT),
        #[cfg(test)]
        QueryHttpPolicy::AllowHttpForTests => agent_with_timeout_allowing_http(QUERY_HTTP_TIMEOUT),
    };

    let mut response = agent.get(url).call().map_err(|e| match e {
        ureq::Error::StatusCode(code) => QueryInstallError::HttpStatus {
            code,
            url: url.to_string(),
        },
        ureq::Error::RequireHttpsOnly(_) => QueryInstallError::HttpsOnly {
            url: url.to_string(),
        },
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

    fn spawn_404_server() -> String {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => {}
                    }
                }
                let response =
                    "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });
        base_url
    }

    /// The rejected name flows into the error's `Display` (printed raw by
    /// the CLI), so control characters in an untrusted name must be escaped
    /// before they reach terminal output.
    #[test]
    fn unsafe_language_name_error_escapes_control_characters() {
        let temp = TempDir::new().unwrap();
        let result = install_queries_with_dependencies_from(
            "http://127.0.0.1:1",
            "evil\u{1b}[31m",
            temp.path(),
            false,
        );
        match result {
            Err(QueryInstallError::LanguageNotSupported(name)) => {
                assert!(
                    !name.contains('\u{1b}'),
                    "stored name must not carry raw escape bytes: {:?}",
                    name
                );
            }
            other => panic!("expected LanguageNotSupported, got {:?}", other.err()),
        }
    }

    #[test]
    fn production_query_install_rejects_plain_http_base_url() {
        let temp = TempDir::new().unwrap();
        let result =
            install_queries_with_dependencies_from("http://127.0.0.1:1", "lua", temp.path(), false);

        assert!(
            matches!(result, Err(QueryInstallError::HttpsOnly { url }) if url == "http://127.0.0.1:1/lua/highlights.scm"),
            "plain HTTP downloads should fail before being reported as missing queries"
        );
    }

    #[test]
    fn installed_queries_skip_plain_http_sentinel_base_url() {
        let temp = TempDir::new().unwrap();
        let queries_dir = temp.path().join("queries").join("lua");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "(comment) @comment\n").unwrap();

        let result =
            install_queries_with_dependencies_from("http://127.0.0.1:1", "lua", temp.path(), false);

        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(path)) if path == queries_dir),
            "already-installed queries must not validate an unused HTTP sentinel URL"
        );
    }

    #[test]
    fn missing_required_highlights_remains_language_not_supported() {
        let temp = TempDir::new().unwrap();
        let base_url = spawn_404_server();

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "missing_lang",
            temp.path(),
            false,
        );

        assert!(
            matches!(result, Err(QueryInstallError::LanguageNotSupported(lang)) if lang == "missing_lang"),
            "404 for required highlights.scm still means the language has no query support"
        );
    }

    #[test]
    fn download_file_preserves_http_status_code() {
        let base_url = spawn_404_server();
        let result = download_file(
            &format!("{base_url}/missing_lang/highlights.scm"),
            QueryHttpPolicy::AllowHttpForTests,
        );

        assert!(
            matches!(result, Err(QueryInstallError::HttpStatus { code: 404, .. })),
            "download errors should preserve structured status codes"
        );
    }

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
