use crate::error::{LspError, LspResult};
use crate::language::query_modeline::parse_modeline;
use log::{debug, warn};
use path_clean::PathClean;
use std::fmt::Write;
use std::fs;
use std::path::{Component, Path, PathBuf};
use tree_sitter::{Language, Query};

/// Parser library file extensions for different platforms
const PARSER_EXTENSIONS: &[&str] = &["so", "dylib", "dll"];

/// Information about a pattern that was skipped during tolerant parsing.
///
/// Known limitation: when a query is combined from several files (`inherits`
/// parents, `extends` overlays), line numbers refer to the **combined** query
/// string, not the original source file (a pattern on line 5 of a child whose
/// parent has 100 lines reports about 105).
#[derive(Debug, Clone)]
pub(crate) struct SkippedPattern {
    /// The pattern text that failed to compile
    pub text: String,
    /// Starting line number (1-indexed for display).
    ///
    /// **Note**: When the query is combined from several files, this refers
    /// to the line in the combined query string, not the original source file.
    pub start_line: usize,
    /// Ending line number (1-indexed for display).
    ///
    /// **Note**: When the query is combined from several files, this refers
    /// to the line in the combined query string, not the original source file.
    pub end_line: usize,
    /// The error message from tree-sitter
    pub error: String,
}

/// Reason why tolerant parsing produced no query (i.e. why `ParseResult.query` is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParseFailure {
    /// The query file couldn't be split into patterns (malformed query syntax).
    /// The `skipped` vec will be empty since individual patterns couldn't be identified.
    PatternSplitFailed(String),
    /// All patterns were identified but none compiled successfully.
    /// The `skipped` vec will contain all the invalid patterns.
    AllPatternsInvalid,
    /// All patterns validated individually but combining them failed.
    ///
    /// Defensive: tree-sitter's pattern validation is consistent, so patterns that
    /// compile individually typically combine successfully. This variant exists to
    /// degrade gracefully (rather than panic) on theoretical edge cases such as
    /// capture-name conflicts across patterns.
    CombinationFailed(String),
}

/// Result of tolerant query parsing.
#[derive(Debug)]
pub(crate) struct ParseResult {
    /// The successfully compiled query (None if all patterns failed)
    pub query: Option<Query>,
    /// Patterns that were skipped due to errors
    pub skipped: Vec<SkippedPattern>,
    /// If `query` is `None`, this indicates why parsing failed.
    /// When `query` is `Some`, this will be `None`.
    pub failure_reason: Option<ParseFailure>,
    /// Whether more than one file contributed (`inherits` parents, `extends`
    /// overlays). When true, line numbers in `skipped` refer to the combined
    /// query, not any one source file.
    pub multi_file: bool,
}

/// The text of a query assembled from every file that contributes to it.
struct ResolvedQuery {
    content: String,
    /// How many files were concatenated, across parents and overlays.
    file_count: usize,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum QueryLoadError {
    #[error("query file not found")]
    NotFound,
    #[error(transparent)]
    Other(#[from] LspError),
}

/// Format search paths for display in error messages.
pub(crate) fn format_search_paths<P: AsRef<Path>>(paths: &[P]) -> String {
    if paths.is_empty() {
        "(no search paths configured)".to_string()
    } else {
        let mut buf = String::from("[");
        for (i, p) in paths.iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            let _ = write!(buf, "{}", p.as_ref().display());
        }
        buf.push(']');
        buf
    }
}

/// Loads Tree-sitter queries from files and configuration
pub(crate) struct QueryLoader;

impl QueryLoader {
    /// Build `<base>/queries/<lang_name>/<file_name>`.
    ///
    /// Callers must reject a `lang_name` that is not a single normal path
    /// component before calling this; `file_name` is likewise caller-validated
    /// (see `is_single_path_component`).
    fn query_file_path(base: &Path, lang_name: &str, file_name: &str) -> PathBuf {
        base.join("queries").join(lang_name).join(file_name).clean()
    }

    /// Build `<base>/parser/<language>.<ext>`.
    ///
    /// Callers must reject a `language` that is not a single normal path
    /// component before calling this.
    fn parser_library_path(base: &Path, language: &str, ext: &str) -> PathBuf {
        base.join("parser")
            .join(format!("{language}.{ext}"))
            .clean()
    }

    /// Resolve every query file that makes up `lang_name`'s `file_name`.
    ///
    /// Mirrors Neovim's `vim.treesitter.query.get_files`: across the search
    /// paths, the first file without an `extends` modeline is the base query
    /// (a later plain file is shadowed), every file marked `extends` is an
    /// overlay, and the `inherits` parents named by any of them resolve first
    /// through this same function. The combined text is `parents, base,
    /// overlays`. Modelines stay in place: tree-sitter reads them as comments,
    /// and keeping them keeps each file's line numbers intact.
    ///
    /// `is_included` says the language is being resolved as a parent of
    /// another rather than for itself; Neovim's `get_files(lang, name,
    /// is_included)` then skips parents written in parentheses, `(cpp)`, and
    /// so does this.
    ///
    /// `visited` holds the languages on the current inheritance path so a
    /// cycle is an error; a language leaves it on the way back out so a
    /// diamond (two parents sharing an ancestor) resolves, as in Neovim. No
    /// error path removes anything: every error propagates straight to the
    /// top-level call, which owns the set and drops it.
    fn resolve_query_recursive<P: AsRef<Path>>(
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
        is_included: bool,
        visited: &mut std::collections::HashSet<String>,
    ) -> Result<ResolvedQuery, QueryLoadError> {
        if visited.contains(lang_name) {
            return Err(LspError::query(format!(
                "Circular inheritance detected for language '{}'",
                lang_name
            ))
            .into());
        }
        visited.insert(lang_name.to_string());

        let paths = Self::find_query_files(runtime_bases, lang_name, file_name);
        if paths.is_empty() {
            return Err(QueryLoadError::NotFound);
        }

        // Each parent with the file that first named it, so a parent that
        // cannot be found is reported against the modeline to fix.
        let mut parents: Vec<(String, &PathBuf)> = Vec::new();
        let mut base: Option<String> = None;
        let mut overlays: Vec<String> = Vec::new();
        for path in &paths {
            let content = fs::read_to_string(path).map_err(|e| {
                LspError::query(format!(
                    "Failed to read query file {}: {}",
                    path.display(),
                    e
                ))
            })?;
            let modeline = parse_modeline(&content);
            // Neovim's `get_files` reads a file naming its own language as an
            // extension (`add_included_lang` returns true for the
            // self-reference instead of adding a parent), so the file is an
            // overlay, not a parent of itself.
            let mut extends = modeline.extends;
            for parent in modeline.inherits {
                if parent.name == lang_name {
                    extends = true;
                } else if parent.optional && is_included {
                    // `(cpp)`: inherited when this file is loaded for its own
                    // language, not when the file is itself being inherited.
                } else if !parents.iter().any(|(name, _)| *name == parent.name) {
                    parents.push((parent.name, path));
                }
            }
            if extends {
                overlays.push(content);
            } else if base.is_none() {
                base = Some(content);
            } else {
                debug!(
                    "Query file {} is shadowed by an earlier search path (mark it `;; extends` to merge it)",
                    path.display()
                );
            }
        }

        // A shadowed plain file contributed no text, so it does not count.
        let mut file_count = usize::from(base.is_some()) + overlays.len();
        let mut combined = String::new();
        for (parent, declared_in) in &parents {
            let resolved = match Self::resolve_query_recursive(
                runtime_bases,
                parent,
                file_name,
                true,
                visited,
            ) {
                Ok(resolved) => resolved,
                Err(QueryLoadError::NotFound) => {
                    return Err(LspError::query(format!(
                        "Query file {} not found for language {} (inherited by {} in {}) in search paths: {}",
                        file_name,
                        parent,
                        lang_name,
                        declared_in.display(),
                        format_search_paths(runtime_bases)
                    ))
                    .into());
                }
                Err(other) => return Err(other),
            };
            combined.push_str(&resolved.content);
            combined.push('\n');
            file_count += resolved.file_count;
        }
        for content in base.into_iter().chain(overlays) {
            combined.push_str(&content);
            combined.push('\n');
        }

        visited.remove(lang_name);
        Ok(ResolvedQuery {
            content: combined,
            file_count,
        })
    }

    /// Load query content from paths (without parsing).
    fn load_content_from_paths<P: AsRef<Path>>(paths: &[P]) -> LspResult<String> {
        let mut combined_query = String::new();

        for path in paths {
            let normalized_path = path.as_ref().clean();
            match fs::read_to_string(&normalized_path) {
                Ok(content) => {
                    combined_query.push_str(&content);
                    combined_query.push('\n');
                }
                Err(e) => {
                    return Err(LspError::query(format!(
                        "Failed to read query file {}: {e}",
                        normalized_path.display()
                    )));
                }
            }
        }

        Ok(combined_query)
    }

    /// Every `<base>/queries/<lang_name>/<file_name>` across the search paths,
    /// in search-path order, each path once: Neovim dedupes its runtime hits
    /// the same way, so a directory listed twice in `searchPaths` does not
    /// append its overlays twice.
    ///
    /// Returns nothing without touching the filesystem when `lang_name` is not
    /// a single normal path component, so a document-controlled injection
    /// language cannot escape `<base>/queries/`.
    ///
    /// An entry that exists but cannot be probed (a dangling symlink, an
    /// unreadable directory) is listed too: presence probing must not
    /// downgrade a broken asset to an ordinary absence, so the real read
    /// reports its concrete I/O error.
    fn find_query_files<P: AsRef<Path>>(
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
    ) -> Vec<PathBuf> {
        // Name-only, so it cannot vary per base: reject once, before the search.
        if !is_single_path_component(lang_name) {
            return Vec::new();
        }
        let mut found: Vec<PathBuf> = Vec::new();
        for base in runtime_bases {
            let candidate = Self::query_file_path(base.as_ref(), lang_name, file_name);
            if found.contains(&candidate) {
                continue;
            }
            match fs::symlink_metadata(&candidate) {
                Ok(_) => found.push(candidate),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => found.push(candidate),
            }
        }
        found
    }

    /// Parse a query string with fault tolerance.
    ///
    /// First attempts full compilation (fast path). If that fails, splits the
    /// query into individual patterns, validates each separately, and combines
    /// only the valid ones.
    pub(crate) fn parse_query(
        language: &Language,
        query_str: &str,
        multi_file: bool,
    ) -> ParseResult {
        use crate::language::query_pattern_splitter::split_patterns;

        // Fast path: try full compilation first.
        // If this succeeds, we know all patterns are valid for this language's grammar.
        // Tree-sitter's Query::new is all-or-nothing: it either compiles the entire
        // query successfully or fails with an error. There are no "silently ignored"
        // patterns that would be valid individually but skipped in full compilation.
        if let Ok(query) = Query::new(language, query_str) {
            return ParseResult {
                query: Some(query),
                skipped: Vec::new(),
                failure_reason: None,
                multi_file,
            };
        }

        // Slow path: split into patterns and compile individually
        let patterns = match split_patterns(query_str) {
            Ok(p) => p,
            Err(e) => {
                // Pattern splitting failed - return result with failure reason
                let reason = e.to_string();
                warn!("Failed to split query patterns: {}", reason);
                return ParseResult {
                    query: None,
                    skipped: Vec::new(),
                    failure_reason: Some(ParseFailure::PatternSplitFailed(reason)),
                    multi_file,
                };
            }
        };

        let mut valid_patterns = Vec::new();
        let mut skipped = Vec::new();

        for pattern in patterns {
            match Query::new(language, &pattern.text) {
                Ok(_) => {
                    valid_patterns.push(pattern.text);
                }
                Err(e) => {
                    skipped.push(SkippedPattern {
                        text: pattern.text,
                        start_line: pattern.start_line + 1, // Convert to 1-indexed
                        end_line: pattern.end_line + 1,
                        error: e.message,
                    });
                }
            }
        }

        // Combine valid patterns and compile
        if valid_patterns.is_empty() {
            return ParseResult {
                query: None,
                skipped,
                failure_reason: Some(ParseFailure::AllPatternsInvalid),
                multi_file,
            };
        }

        let valid_query = valid_patterns.join("\n");
        match Query::new(language, &valid_query) {
            Ok(q) => ParseResult {
                query: Some(q),
                skipped,
                failure_reason: None,
                multi_file,
            },
            Err(e) => {
                // Defensive: handle the rare case where individually-valid patterns
                // fail when combined. See ParseFailure::CombinationFailed docs.
                warn!(
                    "All {} patterns validated individually but combination failed: {}",
                    valid_patterns.len(),
                    e.message
                );
                ParseResult {
                    query: None,
                    skipped,
                    failure_reason: Some(ParseFailure::CombinationFailed(e.message)),
                    multi_file,
                }
            }
        }
    }

    /// Load and parse a query from explicit paths with fault tolerance.
    ///
    /// Tolerant parsing skips invalid patterns instead of failing the whole query;
    /// errors only on a missing/unreadable file. Several paths concatenate in
    /// the order given, and the result then says so through `multi_file`.
    pub(crate) fn load_query_from_paths<P: AsRef<Path>>(
        language: &Language,
        paths: &[P],
    ) -> LspResult<ParseResult> {
        let query_str = Self::load_content_from_paths(paths)?;
        Ok(Self::parse_query(language, &query_str, paths.len() > 1))
    }

    /// Load and parse a query with inheritance resolution and fault tolerance.
    ///
    /// Resolves `inherits` parents and `extends` overlays across the search
    /// paths (see [`Self::resolve_query_recursive`]) and skips invalid patterns
    /// instead of failing the whole query. When more than one file contributes,
    /// line numbers in [`SkippedPattern`] refer to the combined query string,
    /// not any one source file.
    pub(crate) fn load_query_with_inheritance<P: AsRef<Path>>(
        language: &Language,
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
    ) -> Result<ParseResult, QueryLoadError> {
        let mut visited = std::collections::HashSet::new();
        let resolved = Self::resolve_query_recursive(
            runtime_bases,
            lang_name,
            file_name,
            false,
            &mut visited,
        )?;
        Ok(Self::parse_query(
            language,
            &resolved.content,
            resolved.file_count > 1,
        ))
    }

    /// Resolve library path for a language.
    ///
    /// An explicit `library` is normalized and returned as-is; otherwise searches
    /// `search_paths` for `<base>/parser/<language>.<ext>`. `library` stays `&str`
    /// because it comes from `LanguageSettings.parser: Option<String>`; `search_paths`
    /// is generic over `AsRef<Path>` to accept both `PathBuf` (from `ConfigStore`)
    /// and `String` (from `WorkspaceSettings`).
    ///
    /// The implicit search is skipped entirely when `language` is not a single
    /// normal path component. An explicit `library` is exempt: it comes from
    /// config rather than from document text.
    pub(crate) fn resolve_library_path<P: AsRef<Path>>(
        library: Option<&str>,
        language: &str,
        search_paths: &[P],
    ) -> Option<PathBuf> {
        // If explicit library path is provided, normalize and use it.
        // Stays ahead of the name gate: this is the documented escape hatch for
        // assets that cannot follow the implicit layout.
        if let Some(lib) = library {
            let normalized = PathBuf::from(lib).clean();
            return Some(normalized);
        }

        // Name-only, so it cannot vary per base or extension: reject once here
        // rather than inside the search below.
        if !is_single_path_component(language) {
            return None;
        }

        // Otherwise, search in searchPaths: <base>/parser/
        for path in search_paths {
            for ext in PARSER_EXTENSIONS {
                let parser_path = Self::parser_library_path(path.as_ref(), language, ext);
                if parser_path.exists() {
                    return Some(parser_path);
                }
            }
        }

        None
    }
}

/// Whether `value` names exactly one ordinary path component.
///
/// Gates the language half of implicit asset lookup so a document-controlled
/// injection language cannot leave `<base>/queries` or `<base>/parser`. The
/// query-kind half is gated separately, by `is_valid_kind` in the captures
/// handler; every other `file_name` is a `QueryKind::filename()` constant.
///
/// The `name == value` comparison is load-bearing, not a formality:
/// `Components` silently normalizes away trailing separators and interior `.`,
/// so `"rust/"` and `"rust/."` both yield a lone `Normal("rust")`. Requiring
/// the component to span the whole input rejects them. Every normalization
/// `Components` performs shortens the rendered form, so equality can only hold
/// when none occurred — which is why this cannot be reduced to a
/// `components().count() == 1` check.
///
/// Deliberately laxer than `is_safe_language_name` (`[a-z0-9_]+`), which gates
/// installs: names like `typescript-react` are legal to place by hand and must
/// stay readable, whereas the write side may be stricter because the name also
/// becomes a URL segment. Rejection here is a path-shape decision only; it is
/// not a charset filter and must not be relied on as one.
fn is_single_path_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(name)) if name == value)
        && components.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    const NO_SEARCH_PATHS: &[PathBuf] = &[];

    /// Test helper: the combined query text for a language.
    fn resolve_query<P: AsRef<Path>>(
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
    ) -> LspResult<String> {
        let mut visited = std::collections::HashSet::new();
        QueryLoader::resolve_query_recursive(
            runtime_bases,
            lang_name,
            file_name,
            false,
            &mut visited,
        )
        .map(|resolved| resolved.content)
        .map_err(|e| match e {
            QueryLoadError::Other(e) => e,
            not_found @ QueryLoadError::NotFound => LspError::query(not_found.to_string()),
        })
    }

    #[test]
    fn test_load_content_from_paths() {
        // Create temp files with query content
        let dir = tempdir().unwrap();
        let file1 = dir.path().join("query1.scm");
        let file2 = dir.path().join("query2.scm");
        fs::write(&file1, "(identifier) @variable").unwrap();
        fs::write(&file2, "(string) @string").unwrap();

        let paths = vec![
            file1.to_string_lossy().to_string(),
            file2.to_string_lossy().to_string(),
        ];

        let result = QueryLoader::load_content_from_paths(&paths).unwrap();
        assert!(result.contains("(identifier) @variable"));
        assert!(result.contains("(string) @string"));
    }

    #[test]
    fn test_find_query_files() {
        let dir = tempdir().unwrap();
        let base_path = dir.path().to_str().unwrap().to_string();

        // Create directory structure
        let query_dir = dir.path().join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();

        // Create a query file
        let query_file = query_dir.join("highlights.scm");
        fs::write(&query_file, "(identifier) @variable").unwrap();

        // Test finding the file
        let result = QueryLoader::find_query_files(&[base_path], "rust", "highlights.scm");
        assert_eq!(result, vec![query_file]);

        // Test not finding a non-existent file
        let result = QueryLoader::find_query_files(NO_SEARCH_PATHS, "rust", "highlights.scm");
        assert!(result.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn find_query_files_treats_dangling_symlink_as_present_asset() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let query_dir = dir.path().join("queries/rust");
        fs::create_dir_all(&query_dir).unwrap();
        let query_file = query_dir.join("highlights.scm");
        symlink("missing-target.scm", &query_file).unwrap();

        assert_eq!(
            QueryLoader::find_query_files(&[dir.path()], "rust", "highlights.scm"),
            vec![query_file],
            "a broken configured asset must not be classified as ordinary absence"
        );

        let error = resolve_query(&[dir.path()], "rust", "highlights.scm")
            .expect_err("the present-but-broken asset must surface its read failure");
        let message = error.to_string();
        assert!(message.contains("Failed to read query file"), "{message}");
        assert!(message.contains("highlights.scm"), "{message}");
    }

    #[test]
    fn find_query_files_rejects_language_path_traversal() {
        let dir = tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("highlights.scm"), "(identifier) @variable").unwrap();

        // `runtime/queries` sits two components below the tempdir root, so the
        // pre-fix join cleaned to exactly the file planted above.
        let result = QueryLoader::find_query_files(
            std::slice::from_ref(&runtime),
            "../../outside",
            "highlights.scm",
        );

        assert!(result.is_empty());

        // An absolute name is the worse variant: `join` discards the base
        // outright, so no `..` arithmetic is needed to escape.
        let result = QueryLoader::find_query_files(
            std::slice::from_ref(&runtime),
            outside.to_str().unwrap(),
            "highlights.scm",
        );

        assert!(result.is_empty());

        // Positive control on the same base: an ordinary name still resolves,
        // so the assertions above pin the gate rather than a broken fixture.
        let inside = runtime.join("queries").join("rust");
        fs::create_dir_all(&inside).unwrap();
        fs::write(inside.join("highlights.scm"), "(identifier) @variable").unwrap();

        let result = QueryLoader::find_query_files(&[runtime], "rust", "highlights.scm");

        assert_eq!(result, vec![inside.join("highlights.scm")]);
    }

    #[test]
    fn resolve_query_rejects_traversing_inherits_parent() {
        // A parent language name comes from a modeline in an on-disk query
        // file, so it is the content-driven route into the search paths. The
        // modeline grammar admits only `[a-z0-9_]+` names, and the lookup
        // itself rejects anything but a single path component; either alone
        // must keep the smuggled content out of the combined query.
        let dir = tempdir().unwrap();
        let runtime = dir.path().join("runtime");

        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("highlights.scm"), "(identifier) @smuggled").unwrap();

        let child = runtime.join("queries").join("child");
        fs::create_dir_all(&child).unwrap();
        fs::write(
            child.join("highlights.scm"),
            "; inherits: ../../outside\n(string_literal) @string\n",
        )
        .unwrap();

        let content = resolve_query(&[runtime], "child", "highlights.scm")
            .expect("a traversal-shaped modeline is a comment, and the child loads");
        assert!(content.contains("@string"));
        assert!(
            !content.contains("@smuggled"),
            "content outside queries/ must never reach the combined query:\n{content}"
        );
        assert!(
            QueryLoader::find_query_files(&[dir.path()], "../outside", "highlights.scm").is_empty(),
            "and the lookup gate holds on its own, without the grammar"
        );
    }

    #[test]
    fn implicit_asset_language_must_be_one_normal_component() {
        assert!(is_single_path_component("typescript-react"));
        assert!(!is_single_path_component(""));
        assert!(!is_single_path_component("."));
        assert!(!is_single_path_component(".."));
        assert!(!is_single_path_component("../rust"));
        assert!(!is_single_path_component("rust/query"));
        assert!(!is_single_path_component("rust/"));
        assert!(!is_single_path_component("rust/."));
        assert!(!is_single_path_component("/rust"));
        assert!(!is_single_path_component(&format!(
            "rust{}query",
            std::path::MAIN_SEPARATOR
        )));
        #[cfg(windows)]
        {
            assert!(!is_single_path_component(r"C:\rust"));
            assert!(!is_single_path_component(r"\\server\share\rust"));
            // Drive-relative: a prefix needs no following separator, so this
            // parses as Prefix(Disk) + Normal("rust").
            assert!(!is_single_path_component(r"C:rust"));
            // Windows accepts `/` as a separator too, so the unix-looking case
            // above is not redundant here.
            assert!(!is_single_path_component("rust/query"));
        }
    }

    #[test]
    fn resolve_library_path_rejects_language_path_traversal() {
        let dir = tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();
        for ext in PARSER_EXTENSIONS {
            fs::write(dir.path().join(format!("outside.{ext}")), "not a parser").unwrap();
        }

        let result = QueryLoader::resolve_library_path(None, "../../outside", &[runtime]);

        assert_eq!(result, None);
    }

    #[test]
    fn resolve_library_path_honors_explicit_path_for_rejected_language() {
        // The name gate guards implicit `<base>/parser/<language>.<ext>` lookup only.
        // An explicit `languages[*].parser` comes from config, not from a document,
        // and is the documented escape hatch for assets that cannot follow the
        // implicit layout -- so it must resolve even for a name the gate rejects.
        let dir = tempdir().unwrap();
        let runtime = dir.path().join("runtime");

        let result = QueryLoader::resolve_library_path(
            Some("explicit/path.so"),
            "../../outside",
            &[runtime],
        );

        assert_eq!(result, Some(PathBuf::from("explicit/path.so")));
    }

    #[test]
    fn test_resolve_library_path() {
        // Test explicit library path
        let explicit = Some("explicit/path.so");
        let no_paths: &[String] = &[];
        let result = QueryLoader::resolve_library_path(explicit, "rust", no_paths);
        assert_eq!(result, Some(PathBuf::from("explicit/path.so")));

        // Test search paths
        let dir = tempdir().unwrap();
        let base_path = dir.path().to_str().unwrap().to_string();
        // Create parser directory
        let parser_dir = dir.path().join("parser");
        fs::create_dir_all(&parser_dir).unwrap();

        // Create a .so file
        let so_file = parser_dir.join("rust.so");
        fs::write(&so_file, "").unwrap();

        let search_paths = vec![base_path];
        let result = QueryLoader::resolve_library_path(None, "rust", &search_paths);
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .to_string_lossy()
                .ends_with("parser/rust.so")
        );

        // Empty search paths and no explicit library → None
        assert!(QueryLoader::resolve_library_path(None, "rust", no_paths).is_none());
    }

    // ============================================================
    // Tests for AsRef<Path> generic with PathBuf inputs
    // ============================================================

    #[test]
    fn test_resolve_library_path_with_pathbuf_search_paths() {
        let dir = tempdir().unwrap();
        let parser_dir = dir.path().join("parser");
        fs::create_dir_all(&parser_dir).unwrap();
        fs::write(parser_dir.join("rust.so"), "").unwrap();

        let search_paths = vec![dir.path().to_path_buf()];
        let result = QueryLoader::resolve_library_path(None, "rust", &search_paths);
        assert!(result.is_some());
        assert!(
            result
                .unwrap()
                .to_string_lossy()
                .ends_with("parser/rust.so")
        );
    }

    #[test]
    fn explicit_paths_report_multi_file_only_when_several_are_given() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let dir = tempdir().unwrap();
        let one = dir.path().join("one.scm");
        let two = dir.path().join("two.scm");
        fs::write(&one, "(identifier) @variable\n").unwrap();
        fs::write(&two, "(string_literal) @string\n").unwrap();

        let single = QueryLoader::load_query_from_paths(&language, &[&one]).unwrap();
        assert!(!single.multi_file, "one file, line numbers are its own");
        let layered = QueryLoader::load_query_from_paths(&language, &[&one, &two]).unwrap();
        assert!(
            layered.multi_file,
            "a second explicit path shifts line numbers like an overlay does"
        );
    }

    #[test]
    fn test_load_content_from_paths_with_pathbuf() {
        let dir = tempdir().unwrap();
        let file1 = dir.path().join("query1.scm");
        let file2 = dir.path().join("query2.scm");
        fs::write(&file1, "(identifier) @variable").unwrap();
        fs::write(&file2, "(string) @string").unwrap();

        let paths: Vec<PathBuf> = vec![file1, file2];
        let result = QueryLoader::load_content_from_paths(&paths).unwrap();
        assert!(result.contains("(identifier) @variable"));
        assert!(result.contains("(string) @string"));
    }

    /// Create a directory with non-UTF-8 bytes in its name under the given temp dir.
    #[cfg(unix)]
    fn create_non_utf8_base(dir: &tempfile::TempDir) -> PathBuf {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let non_utf8_name = OsStr::from_bytes(b"base_\xff\xfe");
        dir.path().join(non_utf8_name)
    }

    // macOS APFS enforces UTF-8 filenames at the kernel level, so non-UTF-8
    // directory names cannot be created. This test only works on Linux.
    #[cfg(unix)]
    #[cfg_attr(target_os = "macos", ignore)]
    #[test]
    fn test_find_query_files_with_non_utf8_search_path() {
        let dir = tempdir().unwrap();
        let non_utf8_base = create_non_utf8_base(&dir);
        let query_dir = non_utf8_base.join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();
        fs::write(query_dir.join("highlights.scm"), "(identifier) @variable").unwrap();

        let search_paths = vec![non_utf8_base];
        let result = QueryLoader::find_query_files(&search_paths, "rust", "highlights.scm");
        assert!(
            !result.is_empty(),
            "Should find query file under non-UTF-8 path"
        );
    }

    // macOS APFS enforces UTF-8 filenames at the kernel level, so non-UTF-8
    // directory names cannot be created. This test only works on Linux.
    #[cfg(unix)]
    #[cfg_attr(target_os = "macos", ignore)]
    #[test]
    fn test_resolve_library_path_with_non_utf8_search_path() {
        let dir = tempdir().unwrap();
        let non_utf8_base = create_non_utf8_base(&dir);
        let parser_dir = non_utf8_base.join("parser");
        fs::create_dir_all(&parser_dir).unwrap();
        fs::write(parser_dir.join("rust.so"), "").unwrap();

        let search_paths = vec![non_utf8_base];
        let result = QueryLoader::resolve_library_path(None, "rust", &search_paths);
        assert!(
            result.is_some(),
            "Should resolve parser under non-UTF-8 path"
        );
    }

    #[test]
    fn test_find_query_files_with_path_ref() {
        let dir = tempdir().unwrap();
        let query_dir = dir.path().join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();
        let query_file = query_dir.join("highlights.scm");
        fs::write(&query_file, "(identifier) @variable").unwrap();

        let search_paths: Vec<&Path> = vec![dir.path()];
        let result = QueryLoader::find_query_files(&search_paths, "rust", "highlights.scm");
        assert_eq!(result, vec![query_file]);
    }

    #[test]
    fn test_format_search_paths_empty() {
        let paths: Vec<PathBuf> = vec![];
        assert_eq!(format_search_paths(&paths), "(no search paths configured)");
    }

    #[test]
    fn test_format_search_paths_single() {
        let paths = vec![PathBuf::from("/path/one")];
        assert_eq!(format_search_paths(&paths), "[/path/one]");
    }

    #[test]
    fn test_format_search_paths_multiple() {
        let paths = vec![PathBuf::from("/path/one"), PathBuf::from("/path/two")];
        assert_eq!(format_search_paths(&paths), "[/path/one, /path/two]");
    }

    #[test]
    fn test_load_query_with_inheritance_pathbuf_search_paths() {
        // Uses Rust grammar with arbitrary language names to test that
        // PathBuf search paths work with the inheritance mechanism.
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let dir = tempdir().unwrap();

        // Create base_lang query
        let base_dir = dir.path().join("queries").join("base_lang");
        fs::create_dir_all(&base_dir).unwrap();
        fs::write(base_dir.join("highlights.scm"), "(identifier) @variable\n").unwrap();

        // Create child_lang query (inherits base_lang)
        let child_dir = dir.path().join("queries").join("child_lang");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(
            child_dir.join("highlights.scm"),
            "; inherits: base_lang\n(string_literal) @string\n",
        )
        .unwrap();

        // Pass PathBuf search paths (not String) to exercise the generic bound
        let search_paths: Vec<PathBuf> = vec![dir.path().to_path_buf()];
        let result = QueryLoader::load_query_with_inheritance(
            &language,
            &search_paths,
            "child_lang",
            "highlights.scm",
        );
        assert!(result.is_ok(), "Should resolve with PathBuf search paths");
        let parsed = result.unwrap();
        assert!(parsed.query.is_some(), "Should produce a valid query");
        assert!(parsed.multi_file, "Should detect inheritance");
    }

    // Tests for `;; extends` overlays across search paths

    /// Write `<base>/queries/<lang>/highlights.scm` under a fresh base dir.
    fn write_highlights(base: &Path, lang: &str, content: &str) {
        let dir = base.join("queries").join(lang);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("highlights.scm"), content).unwrap();
    }

    fn position(haystack: &str, needle: &str) -> usize {
        haystack
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing from:\n{haystack}"))
    }

    #[test]
    fn find_query_files_lists_every_search_path_hit_in_order() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let third = tempdir().unwrap();
        write_highlights(first.path(), "rust", "(a) @a\n");
        write_highlights(third.path(), "rust", "(c) @c\n");

        let bases = [
            first.path().to_path_buf(),
            second.path().to_path_buf(),
            third.path().to_path_buf(),
        ];
        let found = QueryLoader::find_query_files(&bases, "rust", "highlights.scm");
        assert_eq!(
            found,
            vec![
                first.path().join("queries/rust/highlights.scm"),
                third.path().join("queries/rust/highlights.scm"),
            ]
        );
        assert!(QueryLoader::find_query_files(&bases, "../rust", "highlights.scm").is_empty());
    }

    /// Neovim runs its runtime hits through `dedupe_files`; without the same,
    /// a search path listed twice would append every overlay in it twice.
    #[test]
    fn a_search_path_listed_twice_contributes_its_overlay_once() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "rust", "(identifier) @base\n");
        write_highlights(
            overlay.path(),
            "rust",
            ";; extends\n(string_literal) @overlay\n",
        );

        let bases = [
            base.path().to_path_buf(),
            overlay.path().to_path_buf(),
            overlay.path().join("."),
        ];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        assert_eq!(
            content.matches("@overlay").count(),
            1,
            "the same file must not be concatenated twice:\n{content}"
        );
    }

    #[test]
    fn extends_overlay_in_a_later_search_path_is_appended_to_the_base() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "rust", "(identifier) @base\n");
        write_highlights(
            overlay.path(),
            "rust",
            ";; extends\n(string_literal) @overlay\n",
        );

        let bases = [base.path().to_path_buf(), overlay.path().to_path_buf()];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        assert!(
            position(&content, "@base") < position(&content, "@overlay"),
            "the base query comes first, then the overlay:\n{content}"
        );
    }

    /// Neovim orders `base, extensions...` regardless of where in the runtime
    /// path each was found: an overlay stays an overlay even when it sorts
    /// ahead of the base.
    #[test]
    fn extends_overlay_in_an_earlier_search_path_still_follows_the_base() {
        let overlay = tempdir().unwrap();
        let base = tempdir().unwrap();
        write_highlights(
            overlay.path(),
            "rust",
            ";; extends\n(string_literal) @overlay\n",
        );
        write_highlights(base.path(), "rust", "(identifier) @base\n");

        let bases = [overlay.path().to_path_buf(), base.path().to_path_buf()];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        assert!(
            position(&content, "@base") < position(&content, "@overlay"),
            "the base query comes first, then the overlay:\n{content}"
        );
    }

    #[test]
    fn a_second_plain_query_is_shadowed_by_the_first() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        write_highlights(first.path(), "rust", "(identifier) @first\n");
        write_highlights(second.path(), "rust", "(identifier) @second\n");

        let bases = [first.path().to_path_buf(), second.path().to_path_buf()];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        assert!(content.contains("@first"));
        assert!(
            !content.contains("@second"),
            "a plain query in a later search path replaces nothing:\n{content}"
        );

        let parsed =
            QueryLoader::load_query_with_inheritance(&language, &bases, "rust", "highlights.scm")
                .unwrap();
        assert!(
            !parsed.multi_file,
            "a shadowed file contributed nothing, so line numbers are the base file's own"
        );
    }

    /// Neovim reads the modeline of every hit, shadowed ones included, so a
    /// parent named only by the shadowed file is still prepended. This is
    /// the one way a shadowed file still matters.
    #[test]
    fn a_shadowed_plain_query_still_contributes_its_parents() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        write_highlights(first.path(), "rust", "(identifier) @first\n");
        write_highlights(first.path(), "parent", "(boolean_literal) @parent\n");
        write_highlights(
            second.path(),
            "rust",
            "; inherits: parent\n(string_literal) @second\n",
        );

        let bases = [first.path().to_path_buf(), second.path().to_path_buf()];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        assert!(
            position(&content, "@parent") < position(&content, "@first"),
            "the shadowed file's parent still comes first:\n{content}"
        );
        assert!(!content.contains("@second"), "{content}");
    }

    #[test]
    fn every_extends_overlay_is_appended_in_search_path_order() {
        let base = tempdir().unwrap();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        write_highlights(base.path(), "rust", "(identifier) @base\n");
        write_highlights(first.path(), "rust", ";; extends\n(a) @first\n");
        write_highlights(second.path(), "rust", ";; extends\n(b) @second\n");

        let bases = [
            first.path().to_path_buf(),
            base.path().to_path_buf(),
            second.path().to_path_buf(),
        ];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        let base_at = position(&content, "@base");
        let first_at = position(&content, "@first");
        let second_at = position(&content, "@second");
        assert!(base_at < first_at && first_at < second_at, "{content}");
    }

    /// Neovim loads a language whose only query files are overlays (its
    /// `base_query` stays nil and the extensions load alone); so does kakehashi.
    #[test]
    fn extends_overlays_load_without_a_base() {
        let overlay = tempdir().unwrap();
        write_highlights(overlay.path(), "rust", ";; extends\n(identifier) @only\n");

        let content =
            resolve_query(&[overlay.path().to_path_buf()], "rust", "highlights.scm").unwrap();
        assert!(content.contains("@only"));
    }

    /// Neovim's `add_included_lang` treats `; inherits: rust` inside rust's
    /// own query file as the `extends` marker, so the file must merge as an
    /// overlay rather than trip the cycle check and drop the language's query.
    #[test]
    fn a_file_that_inherits_its_own_language_is_an_overlay() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "rust", "(identifier) @base\n");
        write_highlights(
            overlay.path(),
            "rust",
            "; inherits: rust\n(string_literal) @overlay\n",
        );

        // The self-inheriting file sits first, where a plain file would be the
        // base: it must still follow the real base as an overlay.
        let bases = [overlay.path().to_path_buf(), base.path().to_path_buf()];
        let content = resolve_query(&bases, "rust", "highlights.scm").unwrap();
        assert!(
            position(&content, "@base") < position(&content, "@overlay"),
            "the base query comes first, then the self-inheriting overlay:\n{content}"
        );
    }

    /// Every hit must be readable: a broken overlay in a later search path
    /// fails the language rather than silently loading without it, the same
    /// "present but broken is not absence" rule the single-hit case follows
    /// (and what Neovim's `get_files` does — `io.open` failure is an error).
    #[cfg(unix)]
    #[test]
    fn an_unreadable_overlay_in_a_later_search_path_fails_the_load() {
        use std::os::unix::fs::symlink;

        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "rust", "(identifier) @base\n");
        let overlay_dir = overlay.path().join("queries/rust");
        fs::create_dir_all(&overlay_dir).unwrap();
        let dangling = overlay_dir.join("highlights.scm");
        symlink("missing-target.scm", &dangling).unwrap();

        let bases = [base.path().to_path_buf(), overlay.path().to_path_buf()];
        let err = resolve_query(&bases, "rust", "highlights.scm")
            .expect_err("a present-but-broken overlay must surface its read failure");
        let message = err.to_string();
        assert!(message.contains("Failed to read query file"), "{message}");
        assert!(
            message.contains(&dangling.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn extends_overlay_may_pull_in_inherits_parents() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "parent", "(identifier) @parent\n");
        write_highlights(base.path(), "child", "(string_literal) @child\n");
        write_highlights(
            overlay.path(),
            "child",
            ";; extends\n;; inherits: parent\n(boolean_literal) @overlay\n",
        );

        // Overlay first in the search paths, so the asserted order is not
        // the search-path order.
        let bases = [overlay.path().to_path_buf(), base.path().to_path_buf()];
        let content = resolve_query(&bases, "child", "highlights.scm").unwrap();
        let parent_at = position(&content, "@parent");
        let child_at = position(&content, "@child");
        let overlay_at = position(&content, "@overlay");
        assert!(
            parent_at < child_at && child_at < overlay_at,
            "parents, then base, then overlays:\n{content}"
        );
    }

    /// Neovim's `get_files` inherits a parenthesized parent, `(cpp)`, only
    /// when the file is loaded for its own language; a file reached as a
    /// parent skips its optional parents, so a chain stops there instead of
    /// pulling in — or failing on — a grandparent the child never asked for.
    #[test]
    fn an_optional_parent_is_inherited_only_at_the_top_of_the_chain() {
        let base = tempdir().unwrap();
        write_highlights(base.path(), "cpp", "(identifier) @cpp\n");
        write_highlights(base.path(), "c", "; inherits: (cpp)\n(string_literal) @c\n");
        write_highlights(
            base.path(),
            "cuda",
            "; inherits: c\n(boolean_literal) @cuda\n",
        );

        let bases = [base.path().to_path_buf()];
        let c = resolve_query(&bases, "c", "highlights.scm").unwrap();
        assert!(
            position(&c, "@cpp") < position(&c, "@c\n"),
            "loaded for itself, c inherits (cpp):\n{c}"
        );
        let cuda = resolve_query(&bases, "cuda", "highlights.scm").unwrap();
        assert!(
            !cuda.contains("@cpp"),
            "reached as a parent, c skips its optional (cpp):\n{cuda}"
        );
        assert!(position(&cuda, "@c\n") < position(&cuda, "@cuda"), "{cuda}");

        // The skipped grandparent need not even exist for the child to load.
        fs::remove_file(base.path().join("queries/cpp/highlights.scm")).unwrap();
        assert!(resolve_query(&bases, "cuda", "highlights.scm").is_ok());
        assert!(resolve_query(&bases, "c", "highlights.scm").is_err());
    }

    #[test]
    fn a_parent_named_by_both_base_and_overlay_is_concatenated_once() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "parent", "(identifier) @parent\n");
        write_highlights(
            base.path(),
            "child",
            "; inherits: parent\n(string_literal) @child\n",
        );
        write_highlights(
            overlay.path(),
            "child",
            ";; extends\n;; inherits: parent\n(boolean_literal) @overlay\n",
        );

        let bases = [base.path().to_path_buf(), overlay.path().to_path_buf()];
        let content = resolve_query(&bases, "child", "highlights.scm").unwrap();
        assert_eq!(
            content.matches("@parent").count(),
            1,
            "one parent, however many files name it:\n{content}"
        );
    }

    #[test]
    fn an_inherited_parent_brings_its_own_extends_overlays() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "parent", "(identifier) @parent\n");
        write_highlights(
            base.path(),
            "child",
            "; inherits: parent\n(string_literal) @child\n",
        );
        write_highlights(
            overlay.path(),
            "parent",
            ";; extends\n(boolean_literal) @parent_overlay\n",
        );

        // Overlay first in the search paths, so the asserted order is not
        // the search-path order.
        let bases = [overlay.path().to_path_buf(), base.path().to_path_buf()];
        let content = resolve_query(&bases, "child", "highlights.scm").unwrap();
        let parent_at = position(&content, "@parent\n");
        let parent_overlay_at = position(&content, "@parent_overlay");
        let child_at = position(&content, "@child");
        assert!(
            parent_at < parent_overlay_at && parent_overlay_at < child_at,
            "a parent resolves with its overlays before the child:\n{content}"
        );
    }

    #[test]
    fn multi_file_flag_reports_any_multi_file_query() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "alone", "(identifier) @variable\n");
        write_highlights(base.path(), "extended", "(identifier) @variable\n");
        write_highlights(
            overlay.path(),
            "extended",
            ";; extends\n(string_literal) @string\n",
        );

        let bases = [base.path().to_path_buf(), overlay.path().to_path_buf()];
        let alone =
            QueryLoader::load_query_with_inheritance(&language, &bases, "alone", "highlights.scm")
                .unwrap();
        assert!(!alone.multi_file, "one file, line numbers are the file's");
        let extended = QueryLoader::load_query_with_inheritance(
            &language,
            &bases,
            "extended",
            "highlights.scm",
        )
        .unwrap();
        assert!(
            extended.multi_file,
            "an overlay shifts line numbers like a parent does"
        );
        assert!(extended.query.is_some());
    }

    /// With a base and any number of overlays each allowed to name parents,
    /// the error for a parent that does not exist must say which file named
    /// it, or a typo in one overlay sends the reader through every file.
    #[test]
    fn a_missing_parent_is_reported_against_the_file_that_named_it() {
        let base = tempdir().unwrap();
        let overlay = tempdir().unwrap();
        write_highlights(base.path(), "rust", "(identifier) @base\n");
        write_highlights(
            overlay.path(),
            "rust",
            ";; extends\n;; inherits: rsut\n(string_literal) @overlay\n",
        );

        let bases = [base.path().to_path_buf(), overlay.path().to_path_buf()];
        let err = resolve_query(&bases, "rust", "highlights.scm").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not found for language rsut"), "{message}");
        assert!(
            message.contains(&format!(
                "inherited by rust in {}",
                overlay.path().join("queries/rust/highlights.scm").display()
            )),
            "{message}"
        );
    }

    #[test]
    fn missing_query_is_not_found_even_when_the_search_path_holds_other_languages() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let base = tempdir().unwrap();
        write_highlights(base.path(), "other", "(identifier) @variable\n");

        let result = QueryLoader::load_query_with_inheritance(
            &language,
            &[base.path().to_path_buf()],
            "rust",
            "highlights.scm",
        );
        assert!(matches!(result, Err(QueryLoadError::NotFound)));
    }

    // Tests for query inheritance

    #[test]
    fn test_resolve_query_no_inheritance() {
        // ecma has no inheritance - the content is the file's own
        let dir = tempdir().unwrap();

        // Create ecma query
        let ecma_dir = dir.path().join("queries").join("ecma");
        fs::create_dir_all(&ecma_dir).unwrap();
        fs::write(ecma_dir.join("highlights.scm"), "(identifier) @variable\n").unwrap();

        let result = resolve_query(&[dir.path().to_path_buf()], "ecma", "highlights.scm");
        assert!(result.is_ok());
        let content = result.unwrap();
        assert!(content.contains("(identifier) @variable"));
    }

    #[test]
    fn test_resolve_query_single_parent() {
        // typescript inherits from ecma
        let dir = tempdir().unwrap();

        // Create ecma query (base)
        let ecma_dir = dir.path().join("queries").join("ecma");
        fs::create_dir_all(&ecma_dir).unwrap();
        fs::write(ecma_dir.join("highlights.scm"), "(identifier) @variable\n").unwrap();

        // Create typescript query (inherits ecma)
        let ts_dir = dir.path().join("queries").join("typescript");
        fs::create_dir_all(&ts_dir).unwrap();
        fs::write(
            ts_dir.join("highlights.scm"),
            "; inherits: ecma\n\n\"require\" @keyword.import\n",
        )
        .unwrap();

        let result = resolve_query(&[dir.path().to_path_buf()], "typescript", "highlights.scm");
        assert!(result.is_ok());
        let content = result.unwrap();

        // Should have ecma content first, then typescript
        assert!(content.contains("(identifier) @variable"));
        assert!(content.contains("\"require\" @keyword.import"));

        // ecma content should come before typescript
        let ecma_pos = content.find("(identifier)").unwrap();
        let ts_pos = content.find("\"require\"").unwrap();
        assert!(ecma_pos < ts_pos, "Parent query should come before child");
    }

    #[test]
    fn test_resolve_query_shared_ancestor_is_not_circular() {
        let dir = tempdir().unwrap();

        let shared_dir = dir.path().join("queries").join("shared");
        fs::create_dir_all(&shared_dir).unwrap();
        fs::write(shared_dir.join("highlights.scm"), "(identifier) @shared\n").unwrap();

        let parent_a_dir = dir.path().join("queries").join("parent_a");
        fs::create_dir_all(&parent_a_dir).unwrap();
        fs::write(
            parent_a_dir.join("highlights.scm"),
            "; inherits: shared\n(string_literal) @parent_a\n",
        )
        .unwrap();

        let parent_b_dir = dir.path().join("queries").join("parent_b");
        fs::create_dir_all(&parent_b_dir).unwrap();
        fs::write(
            parent_b_dir.join("highlights.scm"),
            "; inherits: shared\n(raw_string_literal) @parent_b\n",
        )
        .unwrap();

        let child_dir = dir.path().join("queries").join("child");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(
            child_dir.join("highlights.scm"),
            "; inherits: parent_a,parent_b\n(boolean_literal) @child\n",
        )
        .unwrap();

        let result = resolve_query(&[dir.path().to_path_buf()], "child", "highlights.scm");

        assert!(
            result.is_ok(),
            "Shared ancestors should not be treated as circular: {:?}",
            result.err()
        );
        let content = result.unwrap();
        assert!(content.contains("(identifier) @shared"));
        assert!(content.contains("(string_literal) @parent_a"));
        assert!(content.contains("(raw_string_literal) @parent_b"));
        assert!(content.contains("(boolean_literal) @child"));
    }

    #[test]
    fn test_resolve_query_keeps_the_modeline_as_an_inert_comment() {
        // Nothing needs to strip the directive: tree-sitter reads a `;` line
        // as a comment, and leaving it keeps the child's line numbers intact.
        let dir = tempdir().unwrap();

        let ecma_dir = dir.path().join("queries").join("ecma");
        fs::create_dir_all(&ecma_dir).unwrap();
        fs::write(ecma_dir.join("highlights.scm"), "(identifier) @variable\n").unwrap();

        let ts_dir = dir.path().join("queries").join("typescript");
        fs::create_dir_all(&ts_dir).unwrap();
        fs::write(
            ts_dir.join("highlights.scm"),
            "; inherits: ecma\n(string_literal) @string\n",
        )
        .unwrap();

        let content =
            resolve_query(&[dir.path().to_path_buf()], "typescript", "highlights.scm").unwrap();
        assert!(content.contains("; inherits: ecma"));
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        assert!(
            tree_sitter::Query::new(&language, &content).is_ok(),
            "the directive must compile away as a comment"
        );
    }

    #[test]
    fn test_resolve_query_with_real_typescript() {
        // Integration test with actual installed queries
        let search_path = PathBuf::from("/Users/atusy/Library/Application Support/kakehashi");

        // Skip if queries aren't installed
        let ts_path = search_path
            .join("queries")
            .join("typescript")
            .join("highlights.scm");
        if !ts_path.exists() {
            eprintln!("Skipping: TypeScript queries not installed");
            return;
        }

        let result = resolve_query(&[search_path], "typescript", "highlights.scm");

        assert!(
            result.is_ok(),
            "Should resolve TypeScript query: {:?}",
            result.err()
        );
        let content = result.unwrap();

        // Should have ecma content (from inheritance)
        assert!(
            content.contains("(identifier) @variable"),
            "Should contain ecma patterns"
        );

        // Should have typescript-specific content
        assert!(
            content.contains("@keyword.import"),
            "Should contain typescript patterns"
        );
    }

    #[test]
    fn test_resolve_query_circular_detection() {
        // a inherits b, b inherits a - should detect and error
        let dir = tempdir().unwrap();

        let a_dir = dir.path().join("queries").join("lang_a");
        fs::create_dir_all(&a_dir).unwrap();
        fs::write(a_dir.join("highlights.scm"), "; inherits: lang_b\n(a) @a\n").unwrap();

        let b_dir = dir.path().join("queries").join("lang_b");
        fs::create_dir_all(&b_dir).unwrap();
        fs::write(b_dir.join("highlights.scm"), "; inherits: lang_a\n(b) @b\n").unwrap();

        let result = resolve_query(&[dir.path().to_path_buf()], "lang_a", "highlights.scm");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("circular") || err.to_string().contains("Circular"));
    }

    #[test]
    fn test_resolve_query_with_real_javascript_multiple_inheritance() {
        // Integration test: JavaScript inherits from BOTH ecma AND jsx
        let search_path = PathBuf::from("/Users/atusy/Library/Application Support/kakehashi");

        // Skip if queries aren't installed
        let js_path = search_path
            .join("queries")
            .join("javascript")
            .join("highlights.scm");
        let jsx_path = search_path
            .join("queries")
            .join("jsx")
            .join("highlights.scm");
        if !js_path.exists() || !jsx_path.exists() {
            eprintln!("Skipping: JavaScript or JSX queries not installed");
            return;
        }

        let result = resolve_query(&[search_path], "javascript", "highlights.scm");

        assert!(
            result.is_ok(),
            "Should resolve JavaScript query: {:?}",
            result.err()
        );
        let content = result.unwrap();

        // Should have ecma content (from inheritance)
        assert!(
            content.contains("(identifier) @variable"),
            "Should contain ecma patterns"
        );

        // Should have jsx content (from inheritance)
        assert!(
            content.contains("jsx_element") || content.contains("jsx_opening_element"),
            "Should contain jsx patterns"
        );

        // Should have javascript-specific content
        assert!(
            content.contains("@variable.parameter"),
            "Should contain javascript patterns"
        );
    }

    // ============================================================
    // Tests for tolerant query parsing
    // ============================================================

    #[test]
    fn test_parse_query_valid_query() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = "(identifier) @variable\n(string_literal) @string";

        let result = QueryLoader::parse_query(&language, query, false);

        assert!(result.query.is_some());
        assert!(result.skipped.is_empty());
        assert!(result.failure_reason.is_none());
        assert_eq!(result.query.unwrap().pattern_count(), 2);
    }

    #[test]
    fn test_parse_query_all_invalid() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        // "nonexistent_node" doesn't exist in Rust grammar
        let query = "(nonexistent_node_type_1) @foo\n(nonexistent_node_type_2) @bar";

        let result = QueryLoader::parse_query(&language, query, false);

        assert!(result.query.is_none());
        assert_eq!(result.skipped.len(), 2);
        assert_eq!(
            result.failure_reason,
            Some(ParseFailure::AllPatternsInvalid)
        );
    }

    #[test]
    fn test_parse_query_mixed_valid_invalid() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = r#"
(identifier) @variable

(nonexistent_node_type) @invalid

(string_literal) @string
"#;

        let result = QueryLoader::parse_query(&language, query, false);

        // Should have a query with 2 patterns (skipped the invalid one)
        assert!(result.query.is_some());
        // failure_reason is None because we successfully built a query
        assert!(result.failure_reason.is_none());
        let query = result.query.unwrap();
        assert_eq!(query.pattern_count(), 2);

        // Should have 1 skipped pattern
        assert_eq!(result.skipped.len(), 1);
        assert!(result.skipped[0].text.contains("nonexistent_node_type"));
        assert!(result.skipped[0].error.contains("nonexistent_node_type"));
    }

    #[test]
    fn test_parse_query_invalid_field() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        // "nonexistent_field" is not a valid field in Rust grammar
        let query = r#"
(identifier) @variable

(function_item
  nonexistent_field: (identifier) @invalid)

(string_literal) @string
"#;

        let result = QueryLoader::parse_query(&language, query, false);

        // Should have a query with 2 patterns
        assert!(result.query.is_some());
        let query = result.query.unwrap();
        assert_eq!(query.pattern_count(), 2);

        // Should have 1 skipped pattern with field error
        assert_eq!(result.skipped.len(), 1);
        assert!(result.skipped[0].text.contains("nonexistent_field"));
    }

    #[test]
    fn test_parse_query_preserves_line_numbers() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = r#"; comment on line 1
(identifier) @variable

; comment on line 4
(nonexistent_node) @invalid

(string_literal) @string
"#;

        let result = QueryLoader::parse_query(&language, query, false);

        assert_eq!(result.skipped.len(), 1);
        // The invalid pattern starts on line 5 (1-indexed), which is line 4 in 0-indexed
        // After +1 conversion, it should be 5
        assert_eq!(result.skipped[0].start_line, 5);
    }

    /// Test that parse_query handles the edge case where patterns validate
    /// individually but fail when combined (e.g., internal tree-sitter errors).
    ///
    /// This is a documentation test - the scenario is rare but the code path should
    /// return None and log a warning rather than panic.
    #[test]
    fn test_parse_query_combined_failure_returns_none() {
        // Note: It's hard to construct a real-world case where patterns validate
        // individually but fail when combined. Tree-sitter's Query::new is designed
        // to be consistent. This test verifies the code structure handles this case.
        //
        // The code path (query_loader.rs lines ~325-339) is defensive:
        // - If all patterns validate individually but combination fails, return None
        // - Log a warning with pattern count and error message
        //
        // Since we can't easily trigger this, we test the normal case to ensure
        // the combination step works correctly.
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();

        // Multiple valid patterns should combine successfully
        let query = r#"
(identifier) @variable
(string_literal) @string
(function_item name: (identifier) @func_name)
"#;

        let result = QueryLoader::parse_query(&language, query, false);

        assert!(
            result.query.is_some(),
            "Valid patterns should combine successfully"
        );
        assert!(result.failure_reason.is_none());
        assert_eq!(result.query.unwrap().pattern_count(), 3);
        assert!(result.skipped.is_empty());
    }

    // ============================================================
    // Tests for failure_reason field
    // ============================================================

    #[test]
    fn test_parse_query_failure_reason_none_on_success() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        // Valid query should have failure_reason = None
        let query = "(identifier) @variable";

        let result = QueryLoader::parse_query(&language, query, false);

        assert!(result.query.is_some());
        assert!(result.failure_reason.is_none());
    }

    #[test]
    fn test_parse_query_failure_reason_all_patterns_invalid() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        // All patterns invalid should set AllPatternsInvalid
        let query = "(nonexistent_type_1) @a\n(nonexistent_type_2) @b";

        let result = QueryLoader::parse_query(&language, query, false);

        assert!(result.query.is_none());
        assert_eq!(
            result.failure_reason,
            Some(ParseFailure::AllPatternsInvalid)
        );
        // The skipped vec should contain both patterns
        assert_eq!(result.skipped.len(), 2);
    }

    #[test]
    fn test_parse_query_failure_reason_with_partial_success() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        // Mixed valid/invalid: query succeeds, failure_reason is None
        let query = "(identifier) @valid\n(nonexistent) @invalid";

        let result = QueryLoader::parse_query(&language, query, false);

        // Query succeeds (we have valid patterns)
        assert!(result.query.is_some());
        // failure_reason is None because overall parsing succeeded
        assert!(result.failure_reason.is_none());
        // But we still have skipped patterns
        assert_eq!(result.skipped.len(), 1);
    }
}
