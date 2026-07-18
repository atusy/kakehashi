//! Installation module for Tree-sitter parsers and queries.
//!
//! This module provides functionality to download and install Tree-sitter
//! query files and compile parser shared libraries.

pub(crate) mod cache;
pub(crate) mod http;
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
    // In `cfg(test)`, redirect install assets, search-path defaults, and
    // other persistent data callers to a project-local dir under `deps/`
    // so the developer's real
    // `~/.local/share/kakehashi/` is never read or written. The dir is
    // shared across test processes to keep parser/query installs cached
    // between runs. Crash-recovery state is stored separately in the
    // process-local `test_state_dir`.
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
/// across runs to avoid re-downloading. Returns the same path every call
/// within a process via `OnceLock`.
#[cfg(test)]
pub(crate) fn test_data_dir() -> PathBuf {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = test_support::test_data_dir_path();
        let _ = std::fs::create_dir_all(&dir);
        // Auto-install required parsers + queries on first call per
        // process. Cached on disk via the `.installed` marker so
        // subsequent test processes are fast.
        let _ = test_support::ensure_test_languages_installed(&dir);
        dir
    })
    .clone()
}

/// Process-local crash-recovery directory used by unit-test builds.
///
/// Parser/query assets remain shared in [`test_data_dir`], but marker files
/// must not: deleting stale-looking markers from that shared directory can
/// unlink a concurrently running test process's live, locked marker. A temp
/// directory owned for the lifetime of this process gives recovery tests a
/// stable location without environment-variable mutation or peer cleanup.
#[cfg(test)]
pub(crate) fn test_state_dir() -> PathBuf {
    use std::sync::OnceLock;
    // Rust does not drop statics, so this small directory intentionally remains
    // in the OS temp tree after the test process exits. Keeping the `TempDir`
    // guard (rather than only its path) prevents premature cleanup while tests
    // are still using the shared process-local registry state; this matches the
    // E2E helpers' accepted lifetime policy.
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("create unit-test crash state directory"))
        .path()
        .to_path_buf()
}

/// Test-support helpers exposed to integration tests under `tests/`.
///
/// Gated to test-only builds: lib unit tests reach it via `cfg(test)`, and
/// the E2E integration crate reaches it via `feature = "e2e"`. Plain
/// `cfg(test)` alone would not work — integration tests link against the
/// non-test lib build and can't see `cfg(test)`-gated items — so the gate
/// also opts in the `e2e` feature. The production library/binary build (no
/// `cfg(test)`, no `e2e`) compiles without this module.
///
/// Not part of the public LSP-server API — intended only for the
/// in-tree test harness.
#[cfg(any(test, feature = "e2e"))]
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
    /// the `.installed` marker is missing, writing the marker only when
    /// every required install succeeded.
    ///
    /// Mirrors the install loop in `make deps/tree-sitter/.installed`.
    /// Both the parser and queries installs short-circuit when a
    /// language is up-to-date; any genuine failure is logged so tests
    /// depending on that language fail with a clearer error rather than
    /// the whole suite panicking in setup.
    ///
    /// A transient failure (network, file lock, compile error) must not
    /// leave the marker behind: a marker over a half-populated data dir
    /// would make every later test process skip setup, turning a one-off
    /// failure into a persistent one until the marker is deleted by hand.
    /// So the marker is written only after all installs succeed.
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
            // Test setup runs from a test-harness binary, whose `current_exe()` has
            // no `__compile-parser` subcommand — compile in-process.
            compile: parser::ParserCompile::InProcess,
        };
        let mut all_ok = true;
        for lang in TEST_LANGUAGES {
            // `AlreadyExists` means the artifact is present from an earlier
            // run — success for setup purposes, not a failure. Treating it as
            // failure would make a partial prior run unrecoverable: if parsers
            // installed but a query failed (so the marker was never written),
            // every later run would see `AlreadyExists` for the parser and
            // could never write the marker without deleting files by hand.
            match parser::install_parser(lang, &parser_options) {
                Ok(_) | Err(parser::ParserInstallError::AlreadyExists(_)) => {}
                Err(e) => {
                    eprintln!("[test setup] install_parser({}) failed: {}", lang, e);
                    all_ok = false;
                }
            }
            match queries::install_queries_with_dependencies(lang, data_dir, false) {
                Ok(_) | Err(queries::QueryInstallError::AlreadyExists(_)) => {}
                Err(e) => {
                    eprintln!(
                        "[test setup] install_queries_with_dependencies({}) failed: {}",
                        lang, e
                    );
                    all_ok = false;
                }
            }
        }
        if all_ok {
            std::fs::write(&marker, "")?;
        }
        Ok(())
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

/// Install a language synchronously (both parser and queries).
///
/// Production callers pass [`queries::NVIM_TREESITTER_QUERIES_URL`]. Tests
/// can inject other HTTPS endpoints; local HTTP fixtures use a test-only
/// wrapper below so production downloads stay HTTPS-only.
fn install_language_blocking(
    language: &str,
    data_dir: &std::path::Path,
    force: bool,
    queries_base_url: &str,
    compile: parser::ParserCompile,
) -> InstallResult {
    install_language_blocking_with_query_installer(
        language,
        data_dir,
        force,
        queries_base_url,
        compile,
        queries::install_queries_with_dependencies_from,
    )
}

fn install_language_blocking_with_query_installer(
    language: &str,
    data_dir: &std::path::Path,
    force: bool,
    queries_base_url: &str,
    compile: parser::ParserCompile,
    install_queries: fn(
        &str,
        &str,
        &std::path::Path,
        bool,
    ) -> Result<queries::QueryInstallResult, queries::QueryInstallError>,
) -> InstallResult {
    let mut result = InstallResult {
        parser_path: None,
        queries_path: None,
        parser_error: None,
        queries_error: None,
    };

    // The queries installer re-checks names itself, but the parser
    // installer also builds paths from the name (`parser/<name>.<ext>`),
    // so reject traversal-capable names before touching either.
    if !queries::is_safe_language_name(language) {
        let reason = format!("Language name '{}' is unsafe", language.escape_default());
        result.parser_error = Some(reason.clone());
        result.queries_error = Some(reason);
        return result;
    }

    if let Err(e) = queries::clear_uninstall_tombstone_for_install(data_dir, language) {
        let reason = e.to_string();
        result.queries_error = Some(reason);
    }

    // Install parser
    // For async/auto-install, always use cache (background operation)
    let parser_options = parser::InstallOptions {
        data_dir: data_dir.to_path_buf(),
        force,
        verbose: false,
        no_cache: false,
        // Caller-chosen: the killable subprocess re-execs this binary's
        // `__compile-parser`, so it is valid only when `current_exe()` is the
        // kakehashi binary. The production caller (LSP auto-install) is, and asks
        // for it; test/embedder callers pass InProcess.
        compile,
    };

    // AlreadyExists means the artifact is present and usable — success,
    // not failure; treating it as an error made the auto-install manager
    // degrade a fully-installed language to "installed but with warnings".
    match parser::install_parser(language, &parser_options) {
        Ok(parser_result) => {
            result.parser_path = Some(parser_result.install_path);
        }
        Err(parser::ParserInstallError::AlreadyExists(path)) => {
            result.parser_path = Some(path);
        }
        Err(e) => {
            result.parser_error = Some(e.to_string());
        }
    }

    // Install queries, following `; inherits:` so languages like html
    // (which keeps its @comment capture in html_tags) highlight correctly.
    match install_queries(queries_base_url, language, data_dir, force) {
        Ok(query_result) => {
            result.queries_path = Some(query_result.install_path);
        }
        Err(queries::QueryInstallError::AlreadyExists(path)) => {
            result.queries_path = Some(path);
        }
        Err(e) => {
            result.queries_error = Some(e.to_string());
        }
    }

    result
}

/// Install a language asynchronously (both parser and queries).
///
/// Wraps the blocking install functions in `spawn_blocking` so it is safe to
/// call from async contexts like the LSP server.
pub(crate) async fn install_language_async(
    language: String,
    data_dir: PathBuf,
    force: bool,
    compile: parser::ParserCompile,
) -> InstallResult {
    // Run blocking install operations in a separate thread pool
    tokio::task::spawn_blocking(move || {
        install_language_blocking(
            &language,
            &data_dir,
            force,
            queries::NVIM_TREESITTER_QUERIES_URL,
            compile,
        )
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
fn install_language_blocking_allowing_http_queries_for_tests(
    language: &str,
    data_dir: &std::path::Path,
    force: bool,
    queries_base_url: &str,
    compile: parser::ParserCompile,
) -> InstallResult {
    install_language_blocking_with_query_installer(
        language,
        data_dir,
        force,
        queries_base_url,
        compile,
        queries::install_queries_with_dependencies_from_allowing_http_for_tests,
    )
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

    /// Serve canned query files over HTTP from an OS-assigned local port.
    ///
    /// `routes` maps request paths (e.g. "/lang/highlights.scm") to bodies;
    /// unknown paths get a 404, mirroring how raw.githubusercontent.com
    /// responds for query files a language doesn't provide. Returns the
    /// base URL. The serving thread lives until the test process exits.
    fn spawn_query_file_server(routes: Vec<(&str, &str)>) -> String {
        use std::io::{BufRead, BufReader, Write};

        let routes: Vec<(String, String)> = routes
            .into_iter()
            .map(|(p, b)| (p.to_string(), b.to_string()))
            .collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(&mut stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                // Drain headers so the client sees a clean request/response
                // cycle; bail on EOF (Ok(0)) so a half-sent request can't
                // spin this loop forever.
                let mut header = String::new();
                loop {
                    header.clear();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" || header == "\n" => break,
                        Ok(_) => {}
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("");
                let response = match routes.iter().find(|(p, _)| p == path) {
                    Some((_, body)) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        base_url
    }

    /// Auto-install must follow `; inherits:` in downloaded highlights.scm.
    ///
    /// Regression test: the LSP auto-install path installed only the
    /// requested language's queries, so `html` arrived without `html_tags`
    /// and its highlights query failed to load at runtime (the `@comment`
    /// capture for `<!-- -->` lives in the inherited file).
    #[test]
    fn auto_install_installs_inherited_query_dependencies() {
        let temp = tempfile::TempDir::new().unwrap();
        let data_dir = temp.path();

        // Pre-create the parser artifact so install_parser short-circuits
        // with AlreadyExists and the test never touches the network for it.
        let parser_dir = data_dir.join("parser");
        std::fs::create_dir_all(&parser_dir).unwrap();
        std::fs::write(
            parser_dir.join(format!("child_lang.{}", std::env::consts::DLL_EXTENSION)),
            "",
        )
        .unwrap();

        let base_url = spawn_query_file_server(vec![
            (
                "/child_lang/highlights.scm",
                "; inherits: parent_lang\n(identifier) @variable\n",
            ),
            ("/parent_lang/highlights.scm", "(comment) @comment\n"),
        ]);

        let result = install_language_blocking_allowing_http_queries_for_tests(
            "child_lang",
            data_dir,
            false,
            &base_url,
            parser::ParserCompile::InProcess,
        );

        assert!(
            result.queries_error.is_none(),
            "queries install should succeed: {:?}",
            result.queries_error
        );
        assert!(
            data_dir
                .join("queries")
                .join("child_lang")
                .join("highlights.scm")
                .exists(),
            "requested language queries should be installed"
        );
        assert!(
            data_dir
                .join("queries")
                .join("parent_lang")
                .join("highlights.scm")
                .exists(),
            "inherited parent queries should be installed alongside the child"
        );
    }

    /// The top-level language name becomes a path segment
    /// (`queries/<name>/`) and a URL segment, exactly like inherited names —
    /// it must obey the same `[a-z0-9_]+` rule or installing `../../evil`
    /// writes outside the data dir.
    #[test]
    fn install_rejects_unsafe_top_level_language_name() {
        let temp = tempfile::TempDir::new().unwrap();
        // Nest the data dir so an escape stays inside the TempDir.
        let data_dir = temp.path().join("nest").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        // Serve highlights.scm under both the raw and the dot-segment-
        // normalized path, so the download succeeds (and the escape would
        // persist) no matter how the HTTP client treats `..` in URLs.
        let body = "(comment) @comment\n";
        let base_url = spawn_query_file_server(vec![
            ("/evil/highlights.scm", body),
            ("/../../evil/highlights.scm", body),
        ]);

        let result = queries::install_queries_with_dependencies_from(
            &base_url,
            "../../evil",
            &data_dir,
            false,
        );

        assert!(
            matches!(
                result,
                Err(queries::QueryInstallError::InvalidLanguageName(_))
            ),
            "unsafe top-level language name must be rejected"
        );
        assert!(
            !temp.path().join("nest").join("evil").exists(),
            "install must not write outside the queries dir"
        );
    }

    /// The queries installer validates names itself, but the parser
    /// installer also builds paths from the language name
    /// (`parser/<name>.<ext>`), so `install_language_blocking` must reject
    /// unsafe names before touching either installer.
    #[test]
    fn install_language_blocking_rejects_unsafe_language_name() {
        let temp = tempfile::TempDir::new().unwrap();
        let data_dir = temp.path().join("nest").join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let result = install_language_blocking(
            "../../evil",
            &data_dir,
            false,
            "http://127.0.0.1:1",
            parser::ParserCompile::InProcess,
        );

        assert!(!result.is_success(), "unsafe name must not install");
        assert!(
            result
                .parser_error
                .as_deref()
                .is_some_and(|e| e.contains("unsafe")),
            "parser side must be blocked by the name guard, got {:?}",
            result.parser_error
        );
        assert!(
            result
                .queries_error
                .as_deref()
                .is_some_and(|e| e.contains("unsafe")),
            "queries side must be blocked by the name guard, got {:?}",
            result.queries_error
        );
        assert!(
            !temp.path().join("nest").join("evil").exists(),
            "nothing may be written outside the data dir"
        );
    }

    /// A language whose parser and queries are already on disk is a
    /// successful install, not a failure: reporting AlreadyExists as an
    /// error made the auto-install manager degrade a fully-usable language
    /// to "installed but with warnings".
    #[test]
    fn install_language_blocking_treats_already_installed_as_success() {
        let temp = tempfile::TempDir::new().unwrap();
        let data_dir = temp.path();

        let parser_dir = data_dir.join("parser");
        std::fs::create_dir_all(&parser_dir).unwrap();
        std::fs::write(
            parser_dir.join(format!("exists_lang.{}", std::env::consts::DLL_EXTENSION)),
            "",
        )
        .unwrap();
        let queries_dir = data_dir.join("queries").join("exists_lang");
        std::fs::create_dir_all(&queries_dir).unwrap();
        std::fs::write(queries_dir.join("highlights.scm"), "(comment) @comment\n").unwrap();
        queries::write_install_marker_for_tests(&queries_dir).unwrap();

        // Everything exists, so no download may happen — point the base URL
        // at a closed port to fail loudly if one is attempted anyway.
        let result = install_language_blocking(
            "exists_lang",
            data_dir,
            false,
            "http://127.0.0.1:1",
            parser::ParserCompile::InProcess,
        );

        assert!(
            result.is_success(),
            "already-installed language must count as success: parser_error={:?} queries_error={:?}",
            result.parser_error,
            result.queries_error
        );
        assert_eq!(
            result.parser_path,
            Some(parser_dir.join(format!("exists_lang.{}", std::env::consts::DLL_EXTENSION)))
        );
        assert_eq!(result.queries_path, Some(queries_dir));
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

    #[test]
    fn install_language_reports_tombstone_cleanup_as_query_error_only() {
        let temp = tempfile::TempDir::new().unwrap();
        let data_dir = temp.path();

        // Keep the parser side successful without fetching metadata or source:
        // this test isolates how a query tombstone failure is classified.
        let parser_dir = data_dir.join("parser");
        std::fs::create_dir_all(&parser_dir).unwrap();
        std::fs::write(
            parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION)),
            "",
        )
        .unwrap();
        std::fs::write(data_dir.join("queries"), "not a directory").unwrap();
        let expected_queries_error =
            queries::clear_uninstall_tombstone_for_install(data_dir, "lua")
                .unwrap_err()
                .to_string();

        fn successful_query_install(
            _base_url: &str,
            language: &str,
            data_dir: &std::path::Path,
            _force: bool,
        ) -> Result<queries::QueryInstallResult, queries::QueryInstallError> {
            Ok(queries::QueryInstallResult {
                language: language.to_string(),
                install_path: data_dir.join("queries").join(language),
                files_downloaded: Vec::new(),
            })
        }

        let result = install_language_blocking_with_query_installer(
            "lua",
            data_dir,
            false,
            "https://example.invalid",
            parser::ParserCompile::InProcess,
            successful_query_install,
        );

        assert!(
            result.parser_error.is_none(),
            "tombstone cleanup must not be reported as a parser error"
        );
        assert!(
            result.parser_path.is_some(),
            "query tombstone cleanup failures must preserve the available parser result"
        );
        assert_eq!(
            result.queries_error.as_deref(),
            Some(expected_queries_error.as_str())
        );
    }
}
