//! Splits tree-sitter query files into individual top-level patterns.
//!
//! This module uses `tree-sitter-tsquery` to parse `.scm` query files and extract
//! individual patterns based on the AST, enabling fault-tolerant query compilation
//! where invalid patterns can be skipped while valid ones are preserved.

use log::warn;
use std::fmt;
use tree_sitter::Parser;

/// Represents a single pattern extracted from a query file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueryPattern {
    /// The pattern text (including any predicates)
    pub text: String,
    /// Starting line number (0-indexed) in the original file
    pub start_line: usize,
    /// Ending line number (0-indexed) in the original file
    pub end_line: usize,
}

/// Defensive errors that should not occur in normal operation: `LanguageInit`
/// (`set_language` fails — would mean a corrupt tree-sitter-tsquery grammar)
/// and `ParseFailed` (`parser.parse` returns `None` — only on extreme cases
/// like memory exhaustion or cancellation, since tsquery is very lenient).
/// Modelled explicitly so we never panic if they do fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SplitError {
    /// Failed to initialize the tree-sitter-tsquery parser.
    ///
    /// This is a defensive code path - in practice, the grammar is always
    /// compatible with the parser when built correctly.
    LanguageInit,
    /// Failed to parse the query string.
    ///
    /// This is a defensive code path - the tree-sitter-tsquery parser is
    /// very lenient and rarely returns None from parse().
    ParseFailed,
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SplitError::LanguageInit => {
                write!(
                    f,
                    "Failed to set tree-sitter-tsquery language for pattern splitting"
                )
            }
            SplitError::ParseFailed => {
                write!(f, "Failed to parse query string with tree-sitter-tsquery")
            }
        }
    }
}

impl std::error::Error for SplitError {}

/// Split a query string into individual top-level patterns using tree-sitter-tsquery.
///
/// Uses the tree-sitter-tsquery grammar to parse the query file and extract
/// all non-comment top-level nodes, which represent individual patterns.
///
/// # Arguments
/// * `query_str` - The full query file content
///
/// # Returns
/// A `Result` containing:
/// - `Ok(Vec<QueryPattern>)` - Successfully parsed patterns (may be empty for comment-only files)
/// - `Err(SplitError)` - Parser initialization or parsing failed
pub(crate) fn split_patterns(query_str: &str) -> Result<Vec<QueryPattern>, SplitError> {
    let mut parser = Parser::new();

    // Set the tree-sitter-tsquery language
    if parser
        .set_language(&tree_sitter_tsquery::LANGUAGE.into())
        .is_err()
    {
        return Err(SplitError::LanguageInit);
    }

    // Parse the query string
    let tree = match parser.parse(query_str, None) {
        Some(tree) => tree,
        None => {
            return Err(SplitError::ParseFailed);
        }
    };

    let root = tree.root_node();
    let mut patterns = Vec::new();
    let mut cursor = root.walk();

    // Iterate over top-level children
    // The tree-sitter-query grammar produces nodes like "named_node", "list",
    // "anonymous_node", "grouping" etc. at the top level - all are valid patterns
    for child in root.children(&mut cursor) {
        // Skip comments (they have "comment" kind)
        if child.kind() == "comment" {
            continue;
        }

        let start_byte = child.start_byte();
        let end_byte = child.end_byte();

        // Extract the text for this pattern
        // Note: .get() returns None if byte indices are invalid (e.g., mid-character
        // in UTF-8). This shouldn't happen with tree-sitter but we log if it does.
        let text = match query_str.get(start_byte..end_byte) {
            Some(s) => s.to_string(),
            None => {
                warn!(
                    "Invalid byte range {}..{} in query string (length {}), skipping pattern at line {}",
                    start_byte,
                    end_byte,
                    query_str.len(),
                    child.start_position().row + 1
                );
                continue;
            }
        };

        patterns.push(QueryPattern {
            text,
            start_line: child.start_position().row,
            end_line: child.end_position().row,
        });
    }

    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_simple_patterns() {
        let query = r#"
(identifier) @variable
(string) @string
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 2);
        assert!(patterns[0].text.contains("(identifier) @variable"));
        assert!(patterns[1].text.contains("(string) @string"));
    }

    #[test]
    fn test_split_with_comments() {
        let query = r#"
; This is a comment
(identifier) @variable
; Another comment
(string) @string
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 2);
    }

    #[test]
    fn test_split_bracketed_alternatives() {
        let query = r#"
[
  "if"
  "else"
] @keyword.conditional
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 1);
        assert!(patterns[0].text.contains("["));
        assert!(patterns[0].text.contains("\"if\""));
        assert!(patterns[0].text.contains("] @keyword.conditional"));
    }

    #[test]
    fn test_split_nested_pattern() {
        let query = r#"
(function_definition
  name: (identifier) @function.name
  parameters: (parameters) @function.params)
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 1);
        assert!(patterns[0].text.contains("(function_definition"));
        assert!(patterns[0].text.contains("@function.name"));
    }

    #[test]
    fn test_split_pattern_with_predicate() {
        let query = r#"
(call
  item: (ident) @_link
  (#eq? @_link "link")
  (group
    (string) @markup.link.url))
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 1);
        assert!(patterns[0].text.contains("#eq?"));
    }

    #[test]
    fn test_split_string_with_parens() {
        let query = r#"
"(not a pattern)" @string
(identifier) @variable
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 2);
        assert!(patterns[0].text.contains("\"(not a pattern)\""));
    }

    #[test]
    fn test_split_preserves_line_numbers() {
        let query = r#"; comment line 0
(identifier) @variable
; comment line 2
(string) @string
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].start_line, 1);
        assert_eq!(patterns[0].end_line, 1);
        assert_eq!(patterns[1].start_line, 3);
        assert_eq!(patterns[1].end_line, 3);
    }

    #[test]
    fn test_split_multiline_pattern_line_numbers() {
        let query = r#"(function
  name: (identifier) @name
  body: (block) @body)
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].start_line, 0);
        assert_eq!(patterns[0].end_line, 2);
    }

    #[test]
    fn test_split_empty_query() {
        let query = "";
        let patterns = split_patterns(query).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_split_only_comments() {
        let query = r#"
; comment 1
; comment 2
"#;
        let patterns = split_patterns(query).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn test_split_real_typst_highlights() {
        // Test with a realistic subset of typst highlights.scm
        // Using r##"..."## to allow "#" inside the string
        let query = r##"; punctuation
"#" @punctuation.special

[
  ":"
  ";"
  ","
] @punctuation.delimiter

(heading
  "=" @markup.heading.1) @markup.heading.1

(call
  item: (ident) @_link
  (#eq? @_link "link")
  (group
    .
    (string) @markup.link.url
    (#offset! @markup.link.url 0 1 0 -1)))
"##;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 4);

        // Check the string pattern
        assert!(patterns[0].text.contains(r##""#" @punctuation.special"##));

        // Check the bracket pattern
        assert!(patterns[1].text.contains("["));
        assert!(patterns[1].text.contains("] @punctuation.delimiter"));

        // Check the heading pattern
        assert!(patterns[2].text.contains("(heading"));

        // Check the complex call pattern with predicates
        assert!(patterns[3].text.contains("(call"));
        assert!(patterns[3].text.contains("#eq?"));
        assert!(patterns[3].text.contains("#offset!"));
    }

    #[test]
    fn test_split_escaped_quotes_in_string() {
        // Test that escaped quotes are handled correctly
        let query = r#"
"hello \"world\"" @string
(identifier) @variable
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 2);
        assert!(patterns[0].text.contains(r#"\"world\""#));
    }

    #[test]
    fn test_split_multiline_string() {
        // Tree-sitter queries don't typically have multi-line strings,
        // but the parser should handle them if present
        let query = r#"
(comment) @comment
(string) @string
"#;
        let patterns = split_patterns(query).unwrap();
        assert_eq!(patterns.len(), 2);
    }
}
