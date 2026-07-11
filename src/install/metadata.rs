//! Parser metadata fetching from nvim-treesitter.
//!
//! This module fetches parser information dynamically from nvim-treesitter's
//! parsers.lua file, supporting all languages that nvim-treesitter supports (300+ languages).
//!
//! The main branch of nvim-treesitter uses a consolidated format where each language
//! entry contains url, revision, and location all in one place.

use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use super::cache::MetadataCache;
use super::http::agent_with_timeout;

/// URL for nvim-treesitter parsers.lua on GitHub (main branch).
const PARSERS_LUA_URL: &str = "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/main/lua/nvim-treesitter/parsers.lua";
/// Timeout for fetching parser metadata; keeps metadata lookups bounded.
///
/// 60 seconds is chosen as a conservative upper bound for downloading a single
/// small Lua file from GitHub over typical Internet connections. This avoids
/// hanging indefinitely in CI or interactive use while still tolerating
/// transient network slowness. Adjust if real-world latency characteristics
/// change significantly.
const PARSERS_LUA_HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Options for fetching metadata.
#[derive(Debug, Clone)]
pub struct FetchOptions<'a> {
    /// Data directory for caching (if None, no caching is used).
    pub data_dir: Option<&'a Path>,
    /// Whether to use the cache (if false, always fetch fresh).
    pub use_cache: bool,
}

/// Parser metadata containing repository URL and revision.
#[derive(Debug, Clone)]
pub struct ParserMetadata {
    /// Git repository URL for the parser.
    pub url: String,
    /// Git revision (commit hash or tag).
    pub revision: String,
    /// Optional subdirectory within the repository (for monorepos).
    pub location: Option<String>,
}

/// Error types for metadata operations.
#[derive(Debug)]
pub enum MetadataError {
    /// Language not found in metadata.
    LanguageNotFound(String),
    /// HTTP request failed.
    HttpError(String),
    /// JSON parsing failed.
    ParseError(String),
    /// Metadata existed but contained no languages.
    EmptyMetadata,
    /// Metadata fetch exceeded the allowed time.
    Timeout,
    /// Internal task or processing failure.
    TaskFailure(String),
}

impl std::fmt::Display for MetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LanguageNotFound(lang) => {
                write!(
                    f,
                    "Language '{}' not found in nvim-treesitter metadata",
                    lang
                )
            }
            Self::HttpError(msg) => write!(f, "HTTP error: {}", msg),
            Self::ParseError(msg) => write!(f, "Parse error: {}", msg),
            Self::EmptyMetadata => write!(
                f,
                "Metadata did not contain any languages; cache may be empty or outdated"
            ),
            Self::Timeout => write!(f, "Metadata fetch timed out"),
            Self::TaskFailure(msg) => write!(f, "Task failure: {}", msg),
        }
    }
}

impl std::error::Error for MetadataError {}

/// Fetch and parse parsers.lua with optional caching support.
///
/// If `options` is provided and caching is enabled, the function will:
/// 1. Check for a fresh cached copy
/// 2. If cache hit, use cached content
/// 3. If cache miss, fetch from network and update cache
fn fetch_parsers_lua_with_options(
    options: Option<&FetchOptions>,
) -> Result<HashMap<String, ParserMetadata>, MetadataError> {
    // Try to get cache if options provided with caching enabled
    let cache = options.and_then(|opts| {
        if opts.use_cache {
            opts.data_dir.map(MetadataCache::with_default_ttl)
        } else {
            None
        }
    });

    load_parsers_lua(cache.as_ref(), fetch_parsers_lua)
}

fn load_parsers_lua(
    cache: Option<&MetadataCache>,
    fetch: impl FnOnce() -> Result<String, MetadataError>,
) -> Result<HashMap<String, ParserMetadata>, MetadataError> {
    if let Some(cache) = cache
        && let Some(cached_content) = cache.read()
        && let Ok(parsers) = parse_complete_parsers_lua(&cached_content)
    {
        return Ok(parsers);
    }

    let content = fetch()?;
    let parsers = parse_complete_parsers_lua(&content)?;
    if let Some(cache) = cache {
        // Ignore cache write errors - caching is best-effort
        let _ = cache.write(&content);
    }
    Ok(parsers)
}

fn parse_complete_parsers_lua(
    content: &str,
) -> Result<HashMap<String, ParserMetadata>, MetadataError> {
    // A prefix ending after one complete language entry can still yield a
    // non-empty map. Require the outer `return { ... }` table to close before
    // accepting or publishing metadata.
    if find_matching_brace(content).is_none() {
        return Err(MetadataError::ParseError(
            "incomplete parsers.lua document".to_string(),
        ));
    }
    parse_parsers_lua(content)
}

fn fetch_parsers_lua() -> Result<String, MetadataError> {
    let agent = agent_with_timeout(PARSERS_LUA_HTTP_TIMEOUT);
    let mut response = agent.get(PARSERS_LUA_URL).call().map_err(|e| match e {
        ureq::Error::Timeout(_) => MetadataError::Timeout,
        ureq::Error::StatusCode(code) => {
            MetadataError::HttpError(format!("HTTP {} fetching parsers.lua", code))
        }
        e => MetadataError::HttpError(e.to_string()),
    })?;

    response.body_mut().read_to_string().map_err(|e| match e {
        ureq::Error::Timeout(_) => MetadataError::Timeout,
        e => MetadataError::HttpError(e.to_string()),
    })
}

/// Parse the parsers.lua content to extract parser information.
///
/// Handles the main branch format where languages are direct table keys.
fn parse_parsers_lua(content: &str) -> Result<HashMap<String, ParserMetadata>, MetadataError> {
    let mut parsers = HashMap::new();

    // Pattern to match parser entries in main branch format: lang = { ... }
    // The pattern matches language names at the start of a line (with optional indentation)
    // followed by = {
    let lang_pattern = Regex::new(r#"(?m)^([ \t]*)([a-zA-Z][a-zA-Z0-9_]*)\s*=\s*\{"#)
        .expect("valid regex for lang pattern");

    // Language entries share the table's minimum indentation. Restricting to
    // that level keeps nested install_info fields from looking like languages.
    let candidates: Vec<(usize, String)> = lang_pattern
        .captures_iter(content)
        .filter_map(|cap| Some((cap.get(1)?.as_str().len(), cap.get(2)?.as_str().to_string())))
        // Filter out non-language keys like "return", "install_info", etc.
        .filter(|(_, name)| !is_reserved_key(name))
        .collect();
    let top_level_indent = candidates.iter().map(|(indent, _)| *indent).min();
    let lang_names = candidates
        .into_iter()
        .filter(|(indent, _)| Some(*indent) == top_level_indent)
        .map(|(_, name)| name);

    // For each language, find its block and extract metadata
    for lang in lang_names {
        let block = parser_metadata_block(content, &lang).ok_or_else(|| {
            MetadataError::ParseError(format!("incomplete parser block for {lang}"))
        })?;
        // Query-only entries (for example upstream `ecma`) intentionally have
        // no install_info and are not installable parsers.
        if block.contains("install_info") {
            let info = extract_parser_metadata(content, &lang).ok_or_else(|| {
                MetadataError::ParseError(format!("incomplete parser metadata for {lang}"))
            })?;
            parsers.insert(lang, info);
        }
    }

    if parsers.is_empty() {
        return Err(MetadataError::EmptyMetadata);
    }

    Ok(parsers)
}

/// Check if a key is a reserved/internal key (not a language name)
fn is_reserved_key(name: &str) -> bool {
    matches!(
        name,
        "return"
            | "install_info"
            | "maintainers"
            | "requires"
            | "tier"
            | "readme_note"
            | "experimental"
            | "filetype"
    )
}

/// Extract parser metadata for a specific language from parsers.lua content.
fn extract_parser_metadata(content: &str, language: &str) -> Option<ParserMetadata> {
    let block_content = parser_metadata_block(content, language)?;

    // Extract URL (required)
    let url_re = Regex::new(r#"url\s*=\s*'([^']+)'"#).ok()?;
    let url = url_re
        .captures(block_content)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())?;

    // Extract revision (required) - in main branch, revision is inside install_info
    let revision_re = Regex::new(r#"revision\s*=\s*'([^']+)'"#).ok()?;
    let revision = revision_re
        .captures(block_content)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string())?;

    // Extract location (optional)
    let location_re = Regex::new(r#"location\s*=\s*'([^']+)'"#).ok()?;
    let location = location_re
        .captures(block_content)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().to_string());

    Some(ParserMetadata {
        url,
        revision,
        location,
    })
}

fn parser_metadata_block<'a>(content: &'a str, language: &str) -> Option<&'a str> {
    // Find the start of this language's block
    // Use word boundary to avoid matching substrings (e.g., "c" matching "cpp")
    let block_start_pattern = format!(r#"(?m)^\s*{}\s*=\s*\{{"#, regex::escape(language));
    let block_start_re = Regex::new(&block_start_pattern).ok()?;

    let block_start = block_start_re.find(content)?;
    let start_pos = block_start.start();

    // Find the end of this block by counting braces
    find_matching_brace(&content[start_pos..])
}

/// Find the content within matching braces starting from the first `{`.
fn find_matching_brace(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0;
    let mut end = start;
    let mut quote = None;
    let mut escaped = false;
    let mut line_comment = false;
    let mut long_bracket = None;
    let mut offset = start;

    while offset < s.len() {
        if let Some(equals) = long_bracket {
            if is_long_bracket_close(&s[offset..], equals) {
                offset += equals + 2;
                long_bracket = None;
            } else {
                offset += s[offset..].chars().next()?.len_utf8();
            }
            continue;
        }

        let c = s[offset..].chars().next()?;
        let char_len = c.len_utf8();
        if line_comment {
            if c == '\n' {
                line_comment = false;
            }
            offset += char_len;
            continue;
        }
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == delimiter {
                quote = None;
            }
            offset += char_len;
            continue;
        }

        if s[offset..].starts_with("--") {
            offset += 2;
            if let Some(equals) = long_bracket_open(&s[offset..]) {
                offset += equals + 2;
                long_bracket = Some(equals);
            } else {
                line_comment = true;
            }
            continue;
        }
        if let Some(equals) = long_bracket_open(&s[offset..]) {
            offset += equals + 2;
            long_bracket = Some(equals);
            continue;
        }

        match c {
            '\'' | '"' => quote = Some(c),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = offset + char_len;
                    break;
                }
            }
            _ => {}
        }
        offset += char_len;
    }

    if depth == 0 {
        Some(&s[start..end])
    } else {
        None
    }
}

fn long_bracket_open(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let equals = bytes[1..].iter().take_while(|byte| **byte == b'=').count();
    (bytes.get(equals + 1) == Some(&b'[')).then_some(equals)
}

fn is_long_bracket_close(s: &str, equals: usize) -> bool {
    let bytes = s.as_bytes();
    bytes.first() == Some(&b']')
        && bytes[1..].iter().take(equals).all(|byte| *byte == b'=')
        && bytes.get(equals + 1) == Some(&b']')
}

/// Fetch parser metadata for a language from nvim-treesitter.
///
/// This fetches parsers.lua which contains url, revision, and location
/// all in one place (main branch format).
///
/// Use `options` to enable caching and avoid repeated HTTP requests.
pub fn fetch_parser_metadata(
    language: &str,
    options: Option<&FetchOptions>,
) -> Result<ParserMetadata, MetadataError> {
    let parsers = fetch_parsers_lua_with_options(options)?;

    parsers
        .get(language)
        .cloned()
        .ok_or_else(|| MetadataError::LanguageNotFound(language.to_string()))
}

/// List all supported languages by fetching from nvim-treesitter.
///
/// This returns all languages that nvim-treesitter supports (300+ languages).
///
/// Use `options` to enable caching and avoid repeated HTTP requests.
pub fn list_supported_languages(
    options: Option<&FetchOptions>,
) -> Result<Vec<String>, MetadataError> {
    let parsers = fetch_parsers_lua_with_options(options)?;
    let mut languages: Vec<String> = parsers.keys().cloned().collect();
    languages.sort();
    Ok(languages)
}

/// Check if a language is supported by nvim-treesitter.
///
/// This function checks if the given language name exists in the nvim-treesitter
/// parsers.lua metadata. Uses caching via FetchOptions to avoid repeated HTTP requests.
///
/// Returns `Ok(true)` if the language is supported, `Ok(false)` otherwise.
/// Network errors or parse errors return `Err`.
pub fn is_language_supported(
    language: &str,
    options: Option<&FetchOptions>,
) -> Result<bool, MetadataError> {
    fetch_parsers_lua_with_options(options).map(|parsers| parsers.contains_key(language))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::tempdir;

    #[test]
    fn corrupt_fresh_cache_falls_back_to_fetch() {
        let temp = tempdir().expect("temp dir");
        let cache = MetadataCache::with_default_ttl(temp.path());
        cache.write("truncated").expect("write corrupt cache");
        let fetched = AtomicBool::new(false);

        let parsers = load_parsers_lua(Some(&cache), || {
            fetched.store(true, Ordering::Relaxed);
            Ok("return {\nlua = { install_info = { url = 'https://example.com/lua', revision = 'abc' } },\n}"
                .to_string())
        })
        .expect("corrupt cache should be repaired from the source");

        assert!(fetched.load(Ordering::Relaxed));
        assert!(parsers.contains_key("lua"));
        assert!(
            cache
                .read()
                .and_then(|content| parse_parsers_lua(&content).ok())
                .is_some_and(|cached| cached.contains_key("lua")),
            "successful recovery must replace the corrupt cache"
        );
    }

    #[test]
    fn cache_truncated_after_complete_entry_falls_back_to_fetch() {
        let temp = tempdir().expect("temp dir");
        let cache = MetadataCache::with_default_ttl(temp.path());
        cache
            .write("return {\nlua = { install_info = { url = 'https://stale/lua', revision = 'old' } },")
            .expect("write partial cache");

        let parsers = load_parsers_lua(Some(&cache), || {
            Ok("return {\nrust = { install_info = { url = 'https://example.com/rust', revision = 'new' } },\n}"
                .to_string())
        })
        .expect("partial document must be refetched");

        assert!(parsers.contains_key("rust"));
        assert!(!parsers.contains_key("lua"));
    }

    #[test]
    fn braces_in_strings_and_comments_do_not_hide_truncation() {
        let partial = "return {\n-- } is not the table end\nlua = { url = 'https://example/}', revision = 'ok' },";
        assert!(matches!(
            parse_complete_parsers_lua(partial),
            Err(MetadataError::ParseError(_))
        ));
    }

    #[test]
    fn braces_in_long_strings_and_comments_do_not_hide_truncation() {
        let partial = "return {\n--[=[ } is not the table end ]=]\nlua = { note = [==[ } is data ]==], install_info = { url = 'https://example/lua', revision = 'ok' } },";
        assert!(matches!(
            parse_complete_parsers_lua(partial),
            Err(MetadataError::ParseError(_))
        ));
    }

    #[test]
    fn malformed_top_level_entry_rejects_the_whole_document() {
        let content = "return {\nlua = { install_info = { url = 'https://example/lua', revision = 'ok' } },\nrust = { install_info = { url = 'https://example/rust' } },\n}";
        assert!(matches!(
            parse_complete_parsers_lua(content),
            Err(MetadataError::ParseError(_))
        ));
    }

    #[test]
    fn test_fetch_parser_metadata_with_caching() {
        // This test verifies that FetchOptions can be used to enable caching
        let temp = tempdir().expect("Failed to create temp dir");
        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        // Fetch metadata with caching enabled - this should write to cache
        let result = fetch_parser_metadata("lua", Some(&options));

        // The function should either succeed (if network available)
        // or fail with HttpError (if offline), but not crash
        match result {
            Ok(metadata) => {
                assert!(!metadata.url.is_empty());
                assert!(!metadata.revision.is_empty());

                // Verify cache was written (read returns Some if cache exists and is fresh)
                let cache = MetadataCache::with_default_ttl(temp.path());
                assert!(
                    cache.read().is_some(),
                    "Cache file should exist after fetch"
                );
            }
            Err(MetadataError::HttpError(_)) => {
                // Network unavailable - that's acceptable in tests
            }
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[test]
    fn test_parse_parsers_lua_main_branch_format() {
        // Test the main branch format (no "list." prefix, single quotes, revision inside)
        let content = r#"
return {
  lua = {
    install_info = {
      revision = 'abc123def456',
      url = 'https://github.com/MunifTanjim/tree-sitter-lua',
    },
    maintainers = { '@someone' },
    tier = 2,
  },
  typescript = {
    install_info = {
      location = 'typescript',
      revision = 'def789ghi012',
      url = 'https://github.com/tree-sitter/tree-sitter-typescript',
    },
    maintainers = { '@someone' },
    tier = 2,
  },
}
"#;

        let result = parse_parsers_lua(content).expect("should parse");

        assert!(result.contains_key("lua"));
        let lua = result.get("lua").unwrap();
        assert_eq!(lua.url, "https://github.com/MunifTanjim/tree-sitter-lua");
        assert_eq!(lua.revision, "abc123def456");
        assert!(lua.location.is_none());

        assert!(result.contains_key("typescript"));
        let ts = result.get("typescript").unwrap();
        assert_eq!(
            ts.url,
            "https://github.com/tree-sitter/tree-sitter-typescript"
        );
        assert_eq!(ts.revision, "def789ghi012");
        assert_eq!(ts.location, Some("typescript".to_string()));
    }

    #[test]
    fn test_parse_parsers_lua_returns_empty_metadata_error() {
        let result = parse_parsers_lua("return {}");
        assert!(
            matches!(result, Err(MetadataError::EmptyMetadata)),
            "Expected empty metadata error"
        );
    }

    #[test]
    fn test_find_matching_brace() {
        let s = "{ foo { bar } baz }";
        let result = find_matching_brace(s);
        assert_eq!(result, Some("{ foo { bar } baz }"));

        let s2 = "prefix { inner } suffix";
        let result2 = find_matching_brace(s2);
        assert_eq!(result2, Some("{ inner }"));
    }

    #[test]
    fn test_extract_parser_metadata() {
        let content = r#"
  rust = {
    install_info = {
      revision = 'abc123',
      url = 'https://github.com/tree-sitter/tree-sitter-rust',
    },
    tier = 1,
  },
"#;

        let info = extract_parser_metadata(content, "rust").expect("should extract");
        assert_eq!(info.url, "https://github.com/tree-sitter/tree-sitter-rust");
        assert_eq!(info.revision, "abc123");
        assert!(info.location.is_none());
    }

    #[test]
    fn test_extract_parser_metadata_with_location() {
        let content = r#"
  markdown = {
    install_info = {
      location = 'tree-sitter-markdown',
      revision = 'xyz789',
      url = 'https://github.com/tree-sitter-grammars/tree-sitter-markdown',
    },
    tier = 2,
  },
"#;

        let info = extract_parser_metadata(content, "markdown").expect("should extract");
        assert_eq!(
            info.url,
            "https://github.com/tree-sitter-grammars/tree-sitter-markdown"
        );
        assert_eq!(info.revision, "xyz789");
        assert_eq!(info.location, Some("tree-sitter-markdown".to_string()));
    }

    #[test]
    fn test_is_reserved_key() {
        assert!(is_reserved_key("return"));
        assert!(is_reserved_key("install_info"));
        assert!(is_reserved_key("maintainers"));
        assert!(!is_reserved_key("lua"));
        assert!(!is_reserved_key("rust"));
    }

    #[test]
    fn test_is_language_supported_returns_true_for_known_language() {
        // Test that is_language_supported returns true for known language like 'lua'
        // Uses cached metadata via FetchOptions to avoid repeated HTTP requests
        use crate::install::test_helpers::setup_mock_metadata_cache;

        let temp = tempdir().expect("Failed to create temp dir");
        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        // First, populate the cache by fetching any language (or mock the cache)
        // For unit test, we mock the cache with parsers.lua content
        let mock_parsers_lua = r#"
return {
  lua = {
    install_info = {
      revision = 'abc123',
      url = 'https://github.com/MunifTanjim/tree-sitter-lua',
    },
    tier = 2,
  },
  rust = {
    install_info = {
      revision = 'def456',
      url = 'https://github.com/tree-sitter/tree-sitter-rust',
    },
    tier = 1,
  },
}
"#;
        setup_mock_metadata_cache(temp.path(), mock_parsers_lua);

        // is_language_supported should return true for 'lua' (known language)
        let result = is_language_supported("lua", Some(&options)).expect("metadata available");
        assert!(result, "Expected 'lua' to be supported");
    }

    #[test]
    fn test_is_language_supported_returns_false_for_unsupported_language() {
        // Test that is_language_supported returns false for unsupported language
        // like 'fake_lang_xyz' without error
        use crate::install::test_helpers::setup_mock_metadata_cache;

        let temp = tempdir().expect("Failed to create temp dir");
        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        // Mock the cache with parsers.lua content that does NOT include 'fake_lang_xyz'
        let mock_parsers_lua = r#"
return {
  lua = {
    install_info = {
      revision = 'abc123',
      url = 'https://github.com/MunifTanjim/tree-sitter-lua',
    },
    tier = 2,
  },
}
"#;
        setup_mock_metadata_cache(temp.path(), mock_parsers_lua);

        // is_language_supported should return false for 'fake_lang_xyz' (unsupported)
        let result =
            is_language_supported("fake_lang_xyz", Some(&options)).expect("metadata available");
        assert!(!result, "Expected 'fake_lang_xyz' to be unsupported");
    }

    #[test]
    fn test_is_language_supported_reuses_cached_metadata() {
        // Test that multiple is_language_supported checks reuse cached metadata
        // This verifies the caching behavior via FetchOptions with the 1-hour TTL
        use crate::install::test_helpers::setup_mock_metadata_cache;

        let temp = tempdir().expect("Failed to create temp dir");
        let options = FetchOptions {
            data_dir: Some(temp.path()),
            use_cache: true,
        };

        // Mock the cache with parsers.lua content
        let mock_parsers_lua = r#"
return {
  lua = {
    install_info = {
      revision = 'abc123',
      url = 'https://github.com/MunifTanjim/tree-sitter-lua',
    },
    tier = 2,
  },
  rust = {
    install_info = {
      revision = 'def456',
      url = 'https://github.com/tree-sitter/tree-sitter-rust',
    },
    tier = 1,
  },
  python = {
    install_info = {
      revision = 'ghi789',
      url = 'https://github.com/tree-sitter/tree-sitter-python',
    },
    tier = 1,
  },
}
"#;
        setup_mock_metadata_cache(temp.path(), mock_parsers_lua);

        // Multiple calls should all use cached metadata (no network requests)
        let lua_supported =
            is_language_supported("lua", Some(&options)).expect("metadata available");
        let rust_supported =
            is_language_supported("rust", Some(&options)).expect("metadata available");
        let python_supported =
            is_language_supported("python", Some(&options)).expect("metadata available");
        let fake_supported =
            is_language_supported("nonexistent_lang", Some(&options)).expect("metadata available");

        // Verify all results are correct (proving cache was used)
        assert!(lua_supported, "lua should be supported");
        assert!(rust_supported, "rust should be supported");
        assert!(python_supported, "python should be supported");
        assert!(!fake_supported, "nonexistent_lang should NOT be supported");
    }

    #[test]
    fn invalid_source_still_returns_metadata_error_after_cache_miss() {
        use crate::install::test_helpers::setup_mock_metadata_cache;

        let temp = tempdir().expect("Failed to create temp dir");
        setup_mock_metadata_cache(temp.path(), "truncated cache");
        let cache = MetadataCache::with_default_ttl(temp.path());

        let result = load_parsers_lua(Some(&cache), || Ok("return {}".to_string()));
        assert!(
            matches!(result, Err(MetadataError::EmptyMetadata)),
            "invalid fetched metadata must not be cached as a successful result"
        );
        assert_eq!(cache.read().as_deref(), Some("truncated cache"));
    }
}
