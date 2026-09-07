//! Heuristic-based language detection using syntect.
//!
//! This module provides language detection via:
//! - Token matching (e.g., "py", "js", "bash" from code fences)
//! - Shebang lines (e.g., `#!/usr/bin/env python`)
//! - Emacs/Vim mode lines (e.g., `# -*- mode: ruby -*-`)
//!
//! Uses syntect's Sublime Text syntax definitions for comprehensive coverage.
//! Part of the detection fallback chain (language-detection-fallback-chain).
//!
//! ## Token Extraction from Paths
//!
//! The `extract_token_from_path` function enables unified detection by converting
//! file paths to tokens that can be passed to `detect_from_token`:
//! - Files with extension: `foo.py` → `"py"`
//! - Files without extension: `Makefile` → `"Makefile"`

use std::path::Path;
use std::sync::LazyLock;
use syntect::parsing::SyntaxSet;

/// Lazily initialized syntax set with extended syntaxes (via two-face).
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Scans per token, for the memo tests: other tests look tokens and lines up
/// concurrently, so a process-wide total would count their scans too.
#[cfg(test)]
static SYNTAX_SCANS: LazyLock<dashmap::DashMap<String, usize>> =
    LazyLock::new(dashmap::DashMap::new);

#[cfg(test)]
fn record_syntax_scan(key: &str) {
    *SYNTAX_SCANS.entry(key.to_string()).or_insert(0) += 1;
}

#[cfg(not(test))]
fn record_syntax_scan(_key: &str) {}

#[cfg(test)]
fn syntax_scans(key: &str) -> usize {
    SYNTAX_SCANS.get(key).map_or(0, |scans| *scans)
}

/// Token → canonical name memo for [`detect_from_token`]. The lookup is a
/// scan over every syntax's extension list and name; injection resolution
/// asks it once per region, and a document repeats a handful of identifiers
/// thousands of times. Misses are remembered too (an unknown fence
/// identifier is the common case for prose fences). Bounded by
/// [`TOKEN_MEMO_CAP`]: identifiers come from document content, so a hostile
/// document could otherwise grow it without limit.
static TOKEN_MEMO: LazyLock<dashmap::DashMap<String, Option<String>>> =
    LazyLock::new(dashmap::DashMap::new);

/// Distinct identifiers the memo holds before it resets. Far above any real
/// vocabulary of fence identifiers (a few dozen), far below anything that
/// costs memory: a reset re-scans, it never answers wrong.
const TOKEN_MEMO_CAP: usize = 4096;

#[cfg(test)]
fn clear_token_memo() {
    TOKEN_MEMO.clear();
}

#[cfg(test)]
fn token_memo_len() -> usize {
    TOKEN_MEMO.len()
}

/// Held by every test that clears or fills the process-wide memo, so two of
/// them cannot race each other's reset.
#[cfg(test)]
static TOKEN_MEMO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Detect language from a token (e.g., "py", "js", "bash").
///
/// Used for code fence language identifiers in Markdown/HTML.
/// Uses syntect's find_syntax_by_token which searches extension list then name.
/// Returns the syntax name in lowercase if found, None otherwise.
pub(crate) fn detect_from_token(token: &str) -> Option<String> {
    if let Some(known) = TOKEN_MEMO.get(token) {
        return known.clone();
    }
    record_syntax_scan(token);
    let detected = SYNTAX_SET
        .find_syntax_by_token(token)
        .map(|syntax| normalize_syntax_name(&syntax.name));
    if TOKEN_MEMO.len() >= TOKEN_MEMO_CAP {
        TOKEN_MEMO.clear();
    }
    detected
}

/// Detect language from file content's first line (shebang, mode line).
///
/// Uses syntect's regex-based detection from Sublime Text syntax definitions.
/// Returns the syntax name in lowercase if found, None otherwise.
pub(crate) fn detect_from_first_line(content: &str) -> Option<String> {
    let first_line = content.lines().next()?;
    let syntax = SYNTAX_SET.find_syntax_by_first_line(first_line)?;
    Some(normalize_syntax_name(&syntax.name))
}

/// Extract a token from a file path for language detection.
///
/// This enables unified detection by converting paths to tokens:
/// - Files with extension: `foo.py` → `"py"` (extension)
/// - Files without extension: `Makefile` → `"Makefile"` (basename)
///
/// The returned token can be passed to `detect_from_token` for syntect-based detection.
pub(crate) fn extract_token_from_path(path: &str) -> Option<&str> {
    let path = Path::new(path);
    let filename = path.file_name()?.to_str()?;

    // If file has an extension, use extension; otherwise use basename
    // This handles both "script.py" → "py" and "Makefile" → "Makefile"
    path.extension().and_then(|e| e.to_str()).or(Some(filename))
}

/// Normalize syntect syntax name to Tree-sitter parser name.
///
/// Syntect uses Sublime Text naming (e.g., "JavaScript", "Python")
/// while Tree-sitter uses lowercase (e.g., "javascript", "python").
fn normalize_syntax_name(name: &str) -> String {
    // Common mappings from Sublime Text names to Tree-sitter names
    match name {
        // Shell variants
        "Bourne Again Shell (bash)" => "bash".to_string(),
        "Shell-Unix-Generic" => "bash".to_string(),
        // Names that need explicit canonicalization or multi-name normalization
        "JavaScript" | "JavaScript (Babel)" => "javascript".to_string(),
        "TypeScriptReact" => "tsx".to_string(),
        "C++" => "cpp".to_string(),
        "Makefile" => "make".to_string(),
        // Default: lowercase the name
        _ => name.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // Token detection tests (for code fence identifiers)

    #[rstest]
    #[case::py("py", Some("python"))]
    #[case::js("js", Some("javascript"))]
    #[case::bash("bash", Some("bash"))]
    #[case::rust("rust", Some("rust"))]
    #[case::ts("ts", Some("typescript"))]
    #[case::tsx("tsx", Some("tsx"))]
    #[case::unknown("unknown_language_xyz", None)]
    fn test_detect_from_token(#[case] token: &str, #[case] expected: Option<&str>) {
        assert_eq!(detect_from_token(token), expected.map(String::from));
    }

    // Shebang / first-line detection tests

    #[rstest]
    #[case::shebang_python("#!/usr/bin/env python\nprint('hello')", Some("python"))]
    #[case::shebang_python3("#!/usr/bin/env python3\nprint('hello')", Some("python"))]
    #[case::shebang_bash("#!/bin/bash\necho hello", Some("bash"))]
    #[case::shebang_sh("#!/bin/sh\necho hello", Some("bash"))]
    #[case::shebang_node("#!/usr/bin/env node\nconsole.log('hello')", Some("javascript"))]
    #[case::shebang_ruby("#!/usr/bin/env ruby\nputs 'hello'", Some("ruby"))]
    #[case::shebang_perl("#!/usr/bin/perl\nprint 'hello';", Some("perl"))]
    #[case::cpp_mode_line("    -*- C++ -*-", Some("cpp"))]
    #[case::no_shebang_code("print('hello')", None)]
    #[case::no_shebang_empty("", None)]
    fn test_detect_from_first_line(#[case] content: &str, #[case] expected: Option<&str>) {
        assert_eq!(detect_from_first_line(content), expected.map(String::from));
    }

    // syntect compiles first-line regexes lazily on first match attempt and
    // panics on patterns the active regex engine rejects. A line matching no
    // syntax forces every first-line regex in the two-face set through the
    // fancy-regex engine, so an incompatible pattern fails here instead of at
    // detection time in production.
    #[test]
    fn test_first_line_regexes_compile_with_fancy_engine() {
        let result = SYNTAX_SET.find_syntax_by_first_line("\u{1}kakehashi-no-syntax-matches\u{1}");
        assert!(
            result.is_none(),
            "sentinel line unexpectedly matched syntax {:?}; \
             pick a new sentinel so all first-line regexes still get compiled",
            result.map(|s| s.name.clone())
        );
    }

    /// A fence identifier is looked up in syntect's syntax set — a scan over
    /// every syntax's extension list and name — once per identifier, not once
    /// per region: an injection-heavy document repeats a handful of
    /// identifiers thousands of times, and the canonicalization sat at the
    /// top of the per-edit resolution cost. The memo answers repeats,
    /// including a miss, and is bounded: crossing the cap resets it, after
    /// which an evicted identifier is scanned again (never answered wrong)
    /// and the identifier that crossed the cap is kept.
    #[test]
    fn detect_from_token_scans_the_syntax_set_once_per_identifier() {
        let _serial = TOKEN_MEMO_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Tokens no other test looks up, so their scan counts are this
        // test's alone (the memo is process-wide). A token is matched as a
        // whole extension or syntax name, so the hit must be a real one.
        let hit = "clj";
        let miss = "memo-probe-no-such-language";
        clear_token_memo();
        assert_eq!(detect_from_token(hit).as_deref(), Some("clojure"));
        assert_eq!(detect_from_token(hit).as_deref(), Some("clojure"));
        assert_eq!(detect_from_token(miss).as_deref(), None);
        assert_eq!(detect_from_token(miss).as_deref(), None);
        assert_eq!(
            (syntax_scans(hit), syntax_scans(miss)),
            (1, 1),
            "one scan per distinct identifier, hits and misses alike"
        );

        clear_token_memo();
        for i in 0..=TOKEN_MEMO_CAP {
            let _ = detect_from_token(&format!("memo-probe-synthetic-{i}"));
        }
        assert!(
            token_memo_len() < TOKEN_MEMO_CAP,
            "the memo is bounded: crossing the cap resets it instead of growing"
        );
        assert_eq!(detect_from_token("memo-probe-synthetic-0").as_deref(), None);
        assert_eq!(
            syntax_scans("memo-probe-synthetic-0"),
            2,
            "an identifier the reset evicted is scanned again, and still answered right"
        );
        let crossed = format!("memo-probe-synthetic-{TOKEN_MEMO_CAP}");
        let _ = detect_from_token(&crossed);
        assert_eq!(
            syntax_scans(&crossed),
            1,
            "the identifier that crossed the cap is kept by the reset"
        );
    }

    // Token extraction from path tests

    #[rstest]
    #[case::extension_rs("/path/to/file.rs", Some("rs"))]
    #[case::extension_py("/path/to/script.py", Some("py"))]
    #[case::extension_js("/path/to/app.js", Some("js"))]
    #[case::basename_makefile("/path/to/Makefile", Some("Makefile"))]
    #[case::basename_dockerfile("/path/to/Dockerfile", Some("Dockerfile"))]
    #[case::basename_gemfile("/path/to/Gemfile", Some("Gemfile"))]
    #[case::hidden_bashrc("/home/.bashrc", Some(".bashrc"))]
    #[case::hidden_gitignore("/home/.gitignore", Some(".gitignore"))]
    #[case::unknown_file("/path/to/random_file", Some("random_file"))]
    fn test_extract_token(#[case] path: &str, #[case] expected: Option<&str>) {
        assert_eq!(extract_token_from_path(path), expected);
    }

    // Combined token extraction + detection tests (integration)

    #[rstest]
    #[case::rust("/path/to/main.rs", Some("rust"))]
    #[case::python("/path/to/script.py", Some("python"))]
    #[case::makefile("/path/to/Makefile", Some("make"))]
    #[case::gemfile("/path/to/Gemfile", Some("ruby"))]
    #[case::bashrc("/home/.bashrc", Some("bash"))]
    fn test_path_to_token_to_language(#[case] path: &str, #[case] expected: Option<&str>) {
        let token = extract_token_from_path(path).unwrap();
        let result = detect_from_token(token);
        match expected {
            Some(lang) => assert_eq!(result, Some(lang.to_string())),
            None => assert!(result.is_none()),
        }
    }
}
