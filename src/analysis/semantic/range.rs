//! Range-filtered semantic token retrieval.
//!
//! This module handles the `textDocument/semanticTokens/range` LSP request,
//! which returns semantic tokens for a specific range of the document rather
//! than the entire document.
//!
//! The implementation:
//! 1. Gets all tokens using full tokenization
//! 2. Converts delta-encoded tokens to absolute positions
//! 3. Filters tokens to only those within the requested range
//! 4. Re-encodes the filtered tokens as deltas

use tower_lsp_server::ls_types::{Range, SemanticToken, SemanticTokens, SemanticTokensResult};
use tree_sitter::{Query, Tree};

use super::handle_semantic_tokens_full;

/// Async variant of `handle_semantic_tokens_range`: runs the Rayon work via
/// `spawn_blocking` so the CPU-bound path doesn't block the runtime. `text`
/// and `tree` are moved in; `None` on cancellation/failure.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_semantic_tokens_range_parallel_async(
    text: String,
    tree: Tree,
    query: std::sync::Arc<Query>,
    range: Range,
    filetype: Option<String>,
    capture_mappings: Option<crate::config::CaptureMappings>,
    coordinator: std::sync::Arc<crate::language::LanguageCoordinator>,
    supports_multiline: bool,
) -> Option<SemanticTokensResult> {
    // Get all tokens using the parallel full handler
    let full_result = handle_semantic_tokens_full(
        text,
        tree,
        query,
        filetype,
        capture_mappings,
        coordinator,
        supports_multiline,
    )
    .await?;

    // Extract tokens from result
    let SemanticTokensResult::Tokens(full_tokens) = full_result else {
        return Some(full_result);
    };

    // Filter tokens by range and re-encode as deltas
    let filtered_data = filter_tokens_by_range(&full_tokens.data, &range);

    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data: filtered_data,
    }))
}

/// Filter semantic tokens to only those within the specified range.
///
/// This function:
/// 1. Converts delta-encoded tokens to absolute positions
/// 2. Filters to tokens within the range (inclusive)
/// 3. Re-encodes as delta-encoded tokens
fn filter_tokens_by_range(tokens: &[SemanticToken], range: &Range) -> Vec<SemanticToken> {
    let start_line = range.start.line as usize;
    let end_line = range.end.line as usize;

    let mut abs_line = 0usize;
    let mut abs_col = 0usize;

    // Collect tokens that are within the range (with absolute positions)
    let mut filtered_tokens: Vec<(usize, usize, u32, u32, u32)> = Vec::new();

    for token in tokens {
        // Update absolute position
        abs_line += token.delta_line as usize;
        if token.delta_line > 0 {
            abs_col = token.delta_start as usize;
        } else {
            abs_col += token.delta_start as usize;
        }

        // Check if token is within range
        if abs_line >= start_line && abs_line <= end_line {
            // For boundary lines, check column positions
            if abs_line == end_line && abs_col > range.end.character as usize {
                continue;
            }
            if abs_line == start_line
                && abs_col + token.length as usize <= range.start.character as usize
            {
                continue;
            }

            filtered_tokens.push((
                abs_line,
                abs_col,
                token.length,
                token.token_type,
                token.token_modifiers_bitset,
            ));
        }
    }

    // Re-encode as deltas
    encode_as_deltas(filtered_tokens)
}

/// Encode absolute-position tokens as delta-encoded SemanticTokens.
fn encode_as_deltas(tokens: Vec<(usize, usize, u32, u32, u32)>) -> Vec<SemanticToken> {
    let mut last_line = 0;
    let mut last_start = 0;
    let mut data = Vec::with_capacity(tokens.len());

    for (line, col, length, token_type, modifiers) in tokens {
        let delta_line = line - last_line;
        let delta_start = if delta_line == 0 {
            col - last_start
        } else {
            col
        };

        data.push(SemanticToken {
            delta_line: delta_line as u32,
            delta_start: delta_start as u32,
            length,
            token_type,
            token_modifiers_bitset: modifiers,
        });

        last_line = line;
        last_start = col;
    }

    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;

    #[test]
    fn test_filter_tokens_by_range_basic() {
        // Create mock tokens for a document with tokens on lines 0, 2, and 4
        let tokens = vec![
            SemanticToken {
                // Line 0, col 0-10
                delta_line: 0,
                delta_start: 0,
                length: 10,
                token_type: 0,
                token_modifiers_bitset: 0,
            },
            SemanticToken {
                // Line 2, col 0-3
                delta_line: 2,
                delta_start: 0,
                length: 3,
                token_type: 1,
                token_modifiers_bitset: 0,
            },
            SemanticToken {
                // Line 2, col 4-5
                delta_line: 0,
                delta_start: 4,
                length: 1,
                token_type: 17,
                token_modifiers_bitset: 0,
            },
            SemanticToken {
                // Line 4, col 2-8
                delta_line: 2,
                delta_start: 2,
                length: 6,
                token_type: 14,
                token_modifiers_bitset: 0,
            },
        ];

        // Test range that includes only lines 1-3 (should get line 2 tokens)
        let range = Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 3,
                character: 100,
            },
        };

        let filtered = filter_tokens_by_range(&tokens, &range);

        // Should have 2 tokens (both on line 2)
        assert_eq!(filtered.len(), 2, "Expected 2 tokens in range lines 1-3");

        // First filtered token should be at line 2 (delta from 0 = 2)
        // But since we're re-encoding, it becomes delta_line: 0 for the first token
        // (relative to start of filtered set)
        // Actually no - we track absolute position then re-encode
        // So the first token's absolute line is 2, relative to previous (none) is 2
        assert_eq!(filtered[0].delta_line, 2);
        assert_eq!(filtered[0].delta_start, 0);
        assert_eq!(filtered[0].length, 3);

        // Second token is on same line, delta_line = 0
        assert_eq!(filtered[1].delta_line, 0);
        assert_eq!(filtered[1].delta_start, 4);
        assert_eq!(filtered[1].length, 1);
    }

    #[test]
    fn test_filter_tokens_excludes_by_column() {
        // Token at line 1, col 5-10
        let tokens = vec![SemanticToken {
            delta_line: 1,
            delta_start: 5,
            length: 5,
            token_type: 0,
            token_modifiers_bitset: 0,
        }];

        // Range ends before the token starts
        let range = Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 1,
                character: 4, // Token starts at 5, so this excludes it
            },
        };

        let filtered = filter_tokens_by_range(&tokens, &range);
        assert!(
            filtered.is_empty(),
            "Token starting after range.end should be excluded"
        );
    }

    #[test]
    fn test_filter_tokens_excludes_token_ending_before_range() {
        // Token at line 1, col 0-3
        let tokens = vec![SemanticToken {
            delta_line: 1,
            delta_start: 0,
            length: 3,
            token_type: 0,
            token_modifiers_bitset: 0,
        }];

        // Range starts after the token ends
        let range = Range {
            start: Position {
                line: 1,
                character: 5, // Token ends at 3, so this excludes it
            },
            end: Position {
                line: 1,
                character: 100,
            },
        };

        let filtered = filter_tokens_by_range(&tokens, &range);
        assert!(
            filtered.is_empty(),
            "Token ending before range.start should be excluded"
        );
    }

    #[test]
    fn test_encode_as_deltas_empty() {
        let tokens: Vec<(usize, usize, u32, u32, u32)> = vec![];
        let result = encode_as_deltas(tokens);
        assert!(result.is_empty());
    }

    #[test]
    fn test_encode_as_deltas_same_line() {
        // Two tokens on the same line
        let tokens = vec![
            (5, 0, 3, 1, 0),  // line 5, col 0, len 3
            (5, 10, 5, 2, 0), // line 5, col 10, len 5
        ];

        let result = encode_as_deltas(tokens);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].delta_line, 5);
        assert_eq!(result[0].delta_start, 0);
        assert_eq!(result[1].delta_line, 0); // Same line
        assert_eq!(result[1].delta_start, 10); // Delta from col 0 to col 10
    }
}
