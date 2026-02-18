//! Heuristic-based language detection using syntect.
//!
//! This module provides language detection via:
//! - Token matching (e.g., "py", "js", "bash" from code fences)
//! - Shebang lines (e.g., `#!/usr/bin/env python`)
//! - Emacs/Vim mode lines (e.g., `# -*- mode: ruby -*-`)
//!
//! Uses syntect's Sublime Text syntax definitions for comprehensive coverage.
//! Part of the detection fallback chain (ADR-0005).
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

/// Lazily initialized syntax set with default syntaxes.
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Detect language from a token (e.g., "py", "js", "bash").
///
/// Used for code fence language identifiers in Markdown/HTML.
/// Uses syntect's find_syntax_by_token which searches extension list then name.
/// Returns the syntax name in lowercase if found, None otherwise.
pub(crate) fn detect_from_token(token: &str) -> Option<String> {
    let syntax = SYNTAX_SET.find_syntax_by_token(token)?;
    Some(normalize_syntax_name(&syntax.name))
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
        // Common languages with different naming
        "JavaScript" => "javascript".to_string(),
        "TypeScript" => "typescript".to_string(),
        "Python" => "python".to_string(),
        "Ruby" => "ruby".to_string(),
        "Rust" => "rust".to_string(),
        "Go" => "go".to_string(),
        "C++" => "cpp".to_string(),
        "C" => "c".to_string(),
        "Java" => "java".to_string(),
        "Perl" => "perl".to_string(),
        "PHP" => "php".to_string(),
        "Lua" => "lua".to_string(),
        "R" => "r".to_string(),
        "Makefile" => "make".to_string(),
        "Dockerfile" => "dockerfile".to_string(),
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
    #[case::no_shebang_code("print('hello')", None)]
    #[case::no_shebang_empty("", None)]
    fn test_detect_from_first_line(#[case] content: &str, #[case] expected: Option<&str>) {
        assert_eq!(detect_from_first_line(content), expected.map(String::from));
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
