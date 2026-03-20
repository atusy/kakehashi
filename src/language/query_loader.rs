use crate::error::{LspError, LspResult};
use log::warn;
use path_clean::PathClean;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Language, Query};

/// Parser library file extensions for different platforms
const PARSER_EXTENSIONS: &[&str] = &["so", "dylib", "dll"];
const INHERITS_DIRECTIVE_PREFIX: &str = "; inherits:";

/// Information about a pattern that was skipped during tolerant parsing.
///
/// This is returned alongside the successfully parsed query to allow
/// callers to log or report which patterns failed.
///
/// # Line Number Limitation
///
/// When queries use inheritance (e.g., TypeScript's `; inherits: ecma`),
/// line numbers refer to positions in the **combined** query string, not the
/// original source file. For example, if `ecma/highlights.scm` has 100 lines
/// and is inherited by `typescript/highlights.scm`, a pattern on line 5 of
/// the TypeScript file would be reported as line 105.
///
/// This is a known limitation of the current implementation. Future versions
/// may track source file information to provide more accurate diagnostics.
#[derive(Debug, Clone)]
pub(crate) struct SkippedPattern {
    /// The pattern text that failed to compile
    pub text: String,
    /// Starting line number (1-indexed for display).
    ///
    /// **Note**: When query inheritance is used, this refers to the line
    /// in the combined query string, not the original source file.
    pub start_line: usize,
    /// Ending line number (1-indexed for display).
    ///
    /// **Note**: When query inheritance is used, this refers to the line
    /// in the combined query string, not the original source file.
    pub end_line: usize,
    /// The error message from tree-sitter
    pub error: String,
}

/// Reason why tolerant parsing produced no query.
///
/// This enum provides semantic information about why `ParseResult.query`
/// is `None`, allowing callers to log appropriate messages or take different actions.
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
    /// This is a defensive code path that handles theoretical edge cases such as:
    /// - Capture name conflicts across patterns
    /// - Internal tree-sitter limitations when combining patterns
    ///
    /// In practice, this scenario is difficult to trigger because tree-sitter's
    /// pattern validation is consistent - patterns that compile individually
    /// typically combine successfully. This variant exists to ensure graceful
    /// handling rather than panicking if such a case ever occurs.
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
    /// Whether this query used inheritance (e.g., `; inherits: ecma`).
    /// When true, line numbers in `skipped` refer to the combined query,
    /// not the original source file.
    pub used_inheritance: bool,
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
    fn query_file_path(base: &Path, lang_name: &str, file_name: &str) -> PathBuf {
        base.join("queries").join(lang_name).join(file_name).clean()
    }

    fn parser_library_path(base: &Path, language: &str, ext: &str) -> PathBuf {
        base.join("parser")
            .join(format!("{language}.{ext}"))
            .clean()
    }

    /// Resolve query inheritance and return the combined query content.
    ///
    /// Recursively resolves parent queries and concatenates them in the correct order
    /// (parents first, then child). Removes the `; inherits:` directive from the output.
    ///
    /// # Arguments
    /// * `runtime_bases` - Search paths for query files
    /// * `lang_name` - The language to resolve
    /// * `file_name` - The query file name (e.g., "highlights.scm")
    /// * `preloaded_content` - If provided, use this content instead of loading from disk.
    ///   This is used to avoid double-loading when the caller already has the content.
    /// * `visited` - Set of already-visited languages for circular dependency detection
    ///
    /// # Returns
    /// Combined query content with all inherited queries.
    fn resolve_query_recursive<P: AsRef<Path>>(
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
        preloaded_content: Option<String>,
        visited: &mut std::collections::HashSet<String>,
    ) -> LspResult<String> {
        // Check for circular inheritance
        if visited.contains(lang_name) {
            return Err(LspError::query(format!(
                "Circular inheritance detected for language '{}'",
                lang_name
            )));
        }
        visited.insert(lang_name.to_string());

        // Use preloaded content or load from disk
        let content = match preloaded_content {
            Some(c) => c,
            None => Self::load_query_file(runtime_bases, lang_name, file_name)?,
        };

        // Parse inheritance directive
        let parents = Self::parse_inherits_directive(&content);

        // Build combined query: parents first, then child
        let mut combined = String::new();

        // Recursively resolve parent queries (never preloaded)
        for parent in &parents {
            let parent_content =
                Self::resolve_query_recursive(runtime_bases, parent, file_name, None, visited)?;
            combined.push_str(&parent_content);
            combined.push('\n');
        }

        // Add child content (without the inherits directive line)
        let child_content = Self::strip_inherits_directive(&content);
        combined.push_str(&child_content);

        visited.remove(lang_name);

        Ok(combined)
    }

    /// Remove the `; inherits:` line from query content.
    fn strip_inherits_directive(content: &str) -> String {
        let first_line = content.lines().next().unwrap_or("");
        if first_line.starts_with(INHERITS_DIRECTIVE_PREFIX) {
            // Skip the first line
            content.lines().skip(1).collect::<Vec<_>>().join("\n")
        } else {
            content.to_string()
        }
    }

    /// Parse the `; inherits: lang1,lang2` directive from query content.
    ///
    /// nvim-treesitter queries can inherit from other queries using this directive
    /// on the first line. This function extracts the list of parent languages.
    ///
    /// # Returns
    /// A vector of parent language names (empty if no inheritance).
    fn parse_inherits_directive(content: &str) -> Vec<String> {
        let first_line = content.lines().next().unwrap_or("");

        if let Some(rest) = first_line.strip_prefix(INHERITS_DIRECTIVE_PREFIX) {
            rest.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            Vec::new()
        }
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

    /// Find a query file in search paths
    fn find_query_file<P: AsRef<Path>>(
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
    ) -> Option<PathBuf> {
        for base in runtime_bases {
            let candidate = Self::query_file_path(base.as_ref(), lang_name, file_name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }

    /// Load a query file from search paths
    fn load_query_file<P: AsRef<Path>>(
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
    ) -> LspResult<String> {
        match Self::find_query_file(runtime_bases, lang_name, file_name) {
            Some(path) => fs::read_to_string(&path).map_err(|e| {
                LspError::query(format!(
                    "Failed to read query file {}: {}",
                    path.display(),
                    e
                ))
            }),
            None => Err(LspError::query(format!(
                "Query file {} not found for language {} in search paths: {}",
                file_name,
                lang_name,
                format_search_paths(runtime_bases)
            ))),
        }
    }

    /// Parse a query string with fault tolerance.
    ///
    /// First attempts full compilation (fast path). If that fails, splits the
    /// query into individual patterns, validates each separately, and combines
    /// only the valid ones.
    ///
    /// # Arguments
    /// * `language` - The tree-sitter language
    /// * `query_str` - The full query string
    /// * `used_inheritance` - Whether this query was resolved using inheritance
    ///
    /// # Returns
    /// A `ParseResult` containing the compiled query (if any patterns
    /// were valid) and a list of skipped patterns with their errors.
    pub(crate) fn parse_query(
        language: &Language,
        query_str: &str,
        used_inheritance: bool,
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
                used_inheritance,
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
                    used_inheritance,
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
                used_inheritance,
            };
        }

        let combined = valid_patterns.join("\n");
        match Query::new(language, &combined) {
            Ok(q) => ParseResult {
                query: Some(q),
                skipped,
                failure_reason: None,
                used_inheritance,
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
                    used_inheritance,
                }
            }
        }
    }

    /// Load and parse a query from explicit paths with fault tolerance.
    ///
    /// Loads query content from the specified paths and uses tolerant parsing
    /// to skip invalid patterns instead of failing the entire query.
    ///
    /// # Returns
    /// - `Ok(ParseResult)` if the query files were found and at least
    ///   partially parsed
    /// - `Err` if any query file could not be found or read
    pub(crate) fn load_query_from_paths<P: AsRef<Path>>(
        language: &Language,
        paths: &[P],
    ) -> LspResult<ParseResult> {
        let query_str = Self::load_content_from_paths(paths)?;
        Ok(Self::parse_query(language, &query_str, false))
    }

    /// Load and parse a query with inheritance resolution and fault tolerance.
    ///
    /// This resolves `; inherits:` directives, concatenates parent queries,
    /// and uses tolerant parsing to skip invalid patterns instead of failing
    /// the entire query.
    ///
    /// # Line Number Limitation
    ///
    /// When inheritance is used, line numbers in [`SkippedPattern`] refer to the
    /// combined query string (parent + child), not the original source file.
    /// See [`SkippedPattern`] documentation for details.
    ///
    /// # Returns
    /// - `Ok(ParseResult)` if the query file was found and at least
    ///   partially parsed
    /// - `Err` if the query file could not be found or read
    pub(crate) fn load_query_with_inheritance<P: AsRef<Path>>(
        language: &Language,
        runtime_bases: &[P],
        lang_name: &str,
        file_name: &str,
    ) -> LspResult<ParseResult> {
        // Load the original file once and pass to resolver to avoid double-loading
        let original_content = Self::load_query_file(runtime_bases, lang_name, file_name)?;
        let used_inheritance = !Self::parse_inherits_directive(&original_content).is_empty();

        let mut visited = std::collections::HashSet::new();
        let query_str = Self::resolve_query_recursive(
            runtime_bases,
            lang_name,
            file_name,
            Some(original_content),
            &mut visited,
        )?;
        Ok(Self::parse_query(language, &query_str, used_inheritance))
    }

    /// Resolve library path for a language.
    ///
    /// If an explicit `library` path is provided, it is normalized and returned.
    /// Otherwise, searches `search_paths` for `<base>/parser/<language>.<ext>`.
    ///
    /// # Arguments
    /// * `library` - Optional explicit path string to the parser shared library.
    ///   Remains `&str` because it comes from `LanguageSettings.parser: Option<String>`.
    /// * `language` - The language name used for search-path-based discovery
    /// * `search_paths` - Directories to search for `parser/<language>.<ext>`.
    ///   Generic over `AsRef<Path>` to accept both `PathBuf` (from `ConfigStore`)
    ///   and `String` (from `WorkspaceSettings`).
    ///
    /// # Returns
    /// The resolved `PathBuf` if found, or `None` if no matching parser exists.
    pub(crate) fn resolve_library_path<P: AsRef<Path>>(
        library: Option<&str>,
        language: &str,
        search_paths: &[P],
    ) -> Option<PathBuf> {
        // If explicit library path is provided, normalize and use it
        if let Some(lib) = library {
            let normalized = PathBuf::from(lib).clean();
            return Some(normalized);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    const NO_SEARCH_PATHS: &[PathBuf] = &[];

    /// Test helper: resolve query without preloaded content
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
            None,
            &mut visited,
        )
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
    fn test_find_query_file() {
        let dir = tempdir().unwrap();
        let base_path = dir.path().to_str().unwrap().to_string();

        // Create directory structure
        let query_dir = dir.path().join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();

        // Create a query file
        let query_file = query_dir.join("highlights.scm");
        fs::write(&query_file, "(identifier) @variable").unwrap();

        // Test finding the file
        let result = QueryLoader::find_query_file(&[base_path], "rust", "highlights.scm");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), query_file);

        // Test not finding a non-existent file
        let result = QueryLoader::find_query_file(NO_SEARCH_PATHS, "rust", "highlights.scm");
        assert!(result.is_none());
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
    fn test_find_query_file_with_non_utf8_search_path() {
        let dir = tempdir().unwrap();
        let non_utf8_base = create_non_utf8_base(&dir);
        let query_dir = non_utf8_base.join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();
        fs::write(query_dir.join("highlights.scm"), "(identifier) @variable").unwrap();

        let search_paths = vec![non_utf8_base];
        let result = QueryLoader::find_query_file(&search_paths, "rust", "highlights.scm");
        assert!(
            result.is_some(),
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
    fn test_find_query_file_with_path_ref() {
        let dir = tempdir().unwrap();
        let query_dir = dir.path().join("queries").join("rust");
        fs::create_dir_all(&query_dir).unwrap();
        let query_file = query_dir.join("highlights.scm");
        fs::write(&query_file, "(identifier) @variable").unwrap();

        let search_paths: Vec<&Path> = vec![dir.path()];
        let result = QueryLoader::find_query_file(&search_paths, "rust", "highlights.scm");
        assert_eq!(result.unwrap(), query_file);
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
        assert!(parsed.used_inheritance, "Should detect inheritance");
    }

    // ============================================================
    // Tests for query inheritance (PBI-020)
    // ============================================================

    #[test]
    fn test_parse_inherits_directive_single() {
        // TypeScript inherits from ecma
        let content = "; inherits: ecma\n\n\"require\" @keyword.import\n";
        let result = QueryLoader::parse_inherits_directive(content);
        assert_eq!(result, vec!["ecma"]);
    }

    #[test]
    fn test_parse_inherits_directive_multiple() {
        // JavaScript inherits from ecma and jsx
        let content = "; inherits: ecma,jsx\n\n(identifier) @variable\n";
        let result = QueryLoader::parse_inherits_directive(content);
        assert_eq!(result, vec!["ecma", "jsx"]);
    }

    #[test]
    fn test_parse_inherits_directive_none() {
        // ecma has no inheritance
        let content = "(identifier) @variable\n";
        let result = QueryLoader::parse_inherits_directive(content);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_inherits_directive_with_spaces() {
        // Handle spaces around language names
        let content = "; inherits: ecma , jsx\n";
        let result = QueryLoader::parse_inherits_directive(content);
        assert_eq!(result, vec!["ecma", "jsx"]);
    }

    #[test]
    fn test_resolve_query_no_inheritance() {
        // ecma has no inheritance - should return content as-is
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
    fn test_resolve_query_removes_directive() {
        // The "; inherits:" line should be removed from output
        let dir = tempdir().unwrap();

        // Create ecma query
        let ecma_dir = dir.path().join("queries").join("ecma");
        fs::create_dir_all(&ecma_dir).unwrap();
        fs::write(ecma_dir.join("highlights.scm"), "(identifier) @variable\n").unwrap();

        // Create typescript query with inherits
        let ts_dir = dir.path().join("queries").join("typescript");
        fs::create_dir_all(&ts_dir).unwrap();
        fs::write(
            ts_dir.join("highlights.scm"),
            "; inherits: ecma\n\"require\" @keyword\n",
        )
        .unwrap();

        let result = resolve_query(&[dir.path().to_path_buf()], "typescript", "highlights.scm");
        let content = result.unwrap();

        // The inherits directive should not be in the output
        assert!(!content.contains("; inherits:"));
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

        // Should NOT have the inherits directive
        assert!(
            !content.contains("; inherits:"),
            "Should strip inherits directive"
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

        // Should NOT have the inherits directive
        assert!(
            !content.contains("; inherits:"),
            "Should strip inherits directive"
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
