mod delta;
mod finalize;
mod injection;
mod legend;
mod parallel;
mod range;
mod token_collector;

use crate::config::CaptureMappings;
use tower_lsp_server::ls_types::SemanticTokensResult;
use tree_sitter::{Query, Tree};

// Re-export crate-internal API from submodules
pub(crate) use delta::calculate_delta_or_full;
pub(crate) use legend::{LEGEND_MODIFIERS, LEGEND_TYPES};
pub(crate) use range::handle_semantic_tokens_range_parallel_async;

// Re-export for parallel processing
use parallel::collect_injection_tokens_parallel;

// Internal re-exports for production code
use finalize::finalize_tokens;
use token_collector::{RawToken, collect_host_tokens};

// Test-only imports
#[cfg(test)]
use {delta::calculate_semantic_tokens_delta, tower_lsp_server::ls_types::SemanticTokens};

/// Handle semantic tokens full request with Rayon parallel injection processing.
///
/// Uses Rayon's work-stealing parallelism for processing multiple injections
/// concurrently. Thread-local parser caching eliminates the need for cross-thread
/// synchronization during parsing. Runs CPU-bound work on tokio's blocking thread
/// pool to avoid blocking the async runtime.
///
/// # Arguments
/// * `text` - The source text (owned for moving into spawn_blocking)
/// * `tree` - The parsed syntax tree (owned for moving into spawn_blocking)
/// * `query` - The tree-sitter query for semantic highlighting (host language)
/// * `filetype` - The filetype of the document being processed
/// * `capture_mappings` - The capture mappings to apply
/// * `coordinator` - Language coordinator for injection queries and language loading
/// * `supports_multiline` - Whether client supports multiline tokens (per LSP 3.16.0+)
///
/// # Returns
/// Semantic tokens for the entire document including injected content,
/// or None if the task was cancelled or failed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_semantic_tokens_full(
    text: String,
    tree: Tree,
    query: std::sync::Arc<Query>,
    filetype: Option<String>,
    capture_mappings: Option<CaptureMappings>,
    coordinator: std::sync::Arc<crate::language::LanguageCoordinator>,
    supports_multiline: bool,
) -> Option<SemanticTokensResult> {
    tokio::task::spawn_blocking(move || {
        let mut all_tokens: Vec<RawToken> = Vec::with_capacity(1000);
        let lines: Vec<&str> = text.lines().collect();

        // Collect host document tokens first (no exclusion — finalize handles it).
        collect_host_tokens(
            &text,
            &tree,
            &query,
            filetype.as_deref(),
            capture_mappings.as_ref(),
            &text,
            &lines,
            0,
            0,
            supports_multiline,
            &[],
            &[],
            &mut all_tokens,
        );

        // Collect injection tokens in parallel using Rayon.
        // Also returns active injection regions for finalize-time exclusion.
        let (injection_tokens, active_injection_regions) = collect_injection_tokens_parallel(
            &text,
            &tree,
            filetype.as_deref(),
            &coordinator,
            capture_mappings.as_ref(),
            supports_multiline,
        );

        // Merge injection tokens with host tokens
        all_tokens.extend(injection_tokens);

        finalize_tokens(all_tokens, &active_injection_regions, &lines)
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Range, SemanticToken};

    /// Returns the search path for tree-sitter grammars.
    /// Uses TREE_SITTER_GRAMMARS env var if set (Nix), otherwise falls back to deps/tree-sitter.
    fn test_search_path() -> String {
        std::env::var("TREE_SITTER_GRAMMARS").unwrap_or_else(|_| "deps/tree-sitter".to_string())
    }

    /// Decode delta-encoded LSP semantic tokens to absolute `(line, col, length, token_type)`.
    fn decode_tokens(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32)> {
        let mut abs_line = 0u32;
        let mut abs_col = 0u32;
        tokens
            .iter()
            .map(|st| {
                abs_line += st.delta_line;
                if st.delta_line > 0 {
                    abs_col = st.delta_start;
                } else {
                    abs_col += st.delta_start;
                }
                (abs_line, abs_col, st.length, st.token_type)
            })
            .collect()
    }

    #[test]
    fn test_semantic_tokens_range() {
        use tower_lsp_server::ls_types::Position;

        // Create mock tokens for a document
        let all_tokens = SemanticTokens {
            result_id: None,
            data: vec![
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
            ],
        };

        // Test range that includes only lines 1-3
        let _range = Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 3,
                character: 100,
            },
        };

        // Tokens in range should be the ones on line 2
        // We'd need actual tree-sitter setup to test the real function,
        // so this is more of a placeholder showing the expected structure
        assert_eq!(all_tokens.data.len(), 4);
    }

    /// Alias for acceptance criteria naming
    #[test]
    fn test_diff_tokens_no_change() {
        // Same as test_semantic_tokens_delta_no_changes
        let tokens = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 10,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };

        let delta = calculate_semantic_tokens_delta(&tokens, &tokens);
        assert!(delta.is_some());
        assert_eq!(delta.unwrap().edits.len(), 0);
    }

    /// Test that suffix matching reduces delta size when change is in the middle.
    ///
    /// Scenario: 5 tokens, only the 3rd token changes length
    /// Expected: Only 1 token in the edit (the changed one), not 3 tokens
    #[test]
    fn test_diff_tokens_suffix_matching() {
        // 5 tokens on the same line (delta_line=0 for all after first)
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // This one changes
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 4,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 10,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // Changed length
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 4,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 1);

        // With suffix matching: start=2 (skip 2 prefix tokens), delete_count=1, data=1 token
        // Without suffix matching: start=2, delete_count=3, data=3 tokens
        let edit = &delta.edits[0];
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(
            edit.start, 10,
            "Should skip 2 prefix tokens (2 * 5 integers)"
        );
        assert_eq!(
            edit.delete_count, 5,
            "Should only delete 1 token (with suffix matching) = 5 integers"
        );
        assert_eq!(
            edit.data.as_ref().unwrap().len(),
            1,
            "Should only include 1 changed token"
        );
    }

    /// Test that line insertion disables suffix optimization (PBI-077 safety).
    ///
    /// When lines are inserted, tokens at the end have the same delta encoding
    /// but are at different absolute positions. We must NOT match them as suffix.
    #[test]
    fn test_diff_tokens_line_insertion_no_suffix() {
        // Before: 3 tokens on lines 0, 1, 2
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }, // line 0
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // line 1
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // line 2
            ],
        };

        // After: 4 tokens on lines 0, 1, 2, 3 (line inserted at position 1)
        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }, // line 0 (same)
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 5,
                    token_modifiers_bitset: 0,
                }, // line 1 (NEW)
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // line 2 (was line 1)
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // line 3 (was line 2)
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 1);

        // The last two tokens in current have SAME delta encoding as last two in previous,
        // but they're at DIFFERENT absolute positions (line 2,3 vs line 1,2).
        // Suffix optimization MUST be disabled.
        let edit = &delta.edits[0];
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(
            edit.start, 5,
            "Should skip 1 prefix token (line 0) = 5 integers"
        );
        // Without suffix: delete_count=2 (tokens at line 1,2), data=3 tokens
        // With incorrect suffix: would wrongly match last token
        assert_eq!(
            edit.delete_count, 10,
            "Should delete 2 original tokens after prefix = 10 integers"
        );
        assert_eq!(
            edit.data.as_ref().unwrap().len(),
            3,
            "Should include 3 new tokens"
        );
    }

    /// Test that same-line edits preserve suffix optimization.
    ///
    /// When editing within a line (no line count change), suffix matching is safe.
    #[test]
    fn test_diff_tokens_same_line_edit_suffix() {
        // 4 tokens all on line 0
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // This changes
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 3,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 4,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        // Second token changes length
        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 8,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // Changed
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 3,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 4,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 1);

        // Same line count, so suffix matching should work
        let edit = &delta.edits[0];
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(edit.start, 5, "Should skip 1 prefix token = 5 integers");
        assert_eq!(
            edit.delete_count, 5,
            "Should only delete 1 token (suffix matched 2) = 5 integers"
        );
        assert_eq!(
            edit.data.as_ref().unwrap().len(),
            1,
            "Should only include 1 changed token"
        );
    }

    /// Test async wrapper for parallel injection processing.
    ///
    /// This verifies the spawn_blocking bridge works correctly when calling
    /// the Rayon-based parallel injection processing from an async context.
    #[tokio::test]
    async fn test_handle_semantic_tokens_full() {
        use crate::config::WorkspaceSettings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        // Set up coordinator with search paths
        let coordinator = Arc::new(LanguageCoordinator::new());

        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load markdown and lua languages
        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua language parser not available");
            return;
        }

        let Some(query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Markdown with a Lua code block
        let text = r#"# Hello

```lua
local x = 42
```
"#
        .to_string();

        // Parse the markdown document
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: could not acquire markdown parser");
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            eprintln!("Skipping: could not parse markdown");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Call the async handler
        let result = handle_semantic_tokens_full(
            text,
            tree,
            query,
            Some("markdown".to_string()),
            None,
            coordinator,
            false,
        )
        .await;

        // Should return tokens including injection tokens
        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        // Should have tokens from the Lua injection
        // Look for a keyword token (the 'local' keyword in Lua)
        let has_keyword_token = tokens.data.iter().any(|t| t.token_type == 1); // keyword = 1
        assert!(
            has_keyword_token,
            "Should have keyword tokens from Lua injection. Got {} tokens",
            tokens.data.len()
        );
    }

    /// Test that async handler returns None for empty document (consistent with sync behavior).
    #[tokio::test]
    async fn test_handle_semantic_tokens_full_with_empty_document() {
        use crate::config::WorkspaceSettings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());

        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        if !md_result.success {
            eprintln!("Skipping: markdown language parser not available");
            return;
        }

        let Some(query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Empty document
        let text = "".to_string();

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Call the async handler with empty document
        let result = handle_semantic_tokens_full(
            text,
            tree,
            query,
            Some("markdown".to_string()),
            None,
            coordinator,
            false,
        )
        .await;

        // Empty document returns None (consistent with finalize_tokens behavior)
        assert!(
            result.is_none(),
            "Empty document should return None (no tokens to return)"
        );
    }

    /// Integration test: Markdown with Lua code block — the finalize pipeline
    /// must exclude host tokens inside the injection region (line 3) while
    /// preserving tokens on other lines.
    #[tokio::test]
    async fn test_no_host_tokens_inside_injection_region() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Markdown with a Lua code block
        let text = "# Hello\n\n```lua\nlocal x = 42\n```\n".to_string();
        // Lines:
        //   0: "# Hello"
        //   1: ""
        //   2: "```lua"
        //   3: "local x = 42"
        //   4: "```"

        // Parse markdown
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Use default capture mappings so markdown captures like @markup.raw.block
        // are translated to `string` (token_type 2).
        let capture_mappings = default_capture_mappings();

        // Use the full pipeline — exclusion now happens in finalize_tokens
        let result = handle_semantic_tokens_full(
            text,
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // Line 3 contains "local x = 42" — injection tokens should be present
        let line3_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 3).collect();
        assert!(
            !line3_tokens.is_empty(),
            "Should have injection tokens on line 3 (Lua). Decoded: {:?}",
            decoded
        );

        // --- Stronger assertions: no host token leaks on content line ---

        // `string` is token_type index 2 in LEGEND_TYPES (SemanticTokenType::STRING).
        // Markdown maps @markup.raw.block to `string`. Host tokens with this type
        // must NOT appear on line 3 (the content line inside the injection region).
        let string_token_type = 2u32;
        let line3_host_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 3 && t.3 == string_token_type)
            .collect();
        assert!(
            line3_host_leaks.is_empty(),
            "Host `string` tokens must not leak onto content line 3 (injection region). \
             Leaked tokens: {:?}. All decoded: {:?}",
            line3_host_leaks,
            decoded
        );

        // Fence lines (2 and 4) SHOULD still have host tokens.
        // Line 2 = "```lua", line 4 = "```" — these are outside the injection region.
        let line2_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 2).collect();
        assert!(
            !line2_tokens.is_empty(),
            "Fence line 2 ('```lua') should have host tokens. Decoded: {:?}",
            decoded
        );
        let line4_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 4).collect();
        assert!(
            !line4_tokens.is_empty(),
            "Fence line 4 ('```') should have host tokens. Decoded: {:?}",
            decoded
        );
    }

    /// Integration test: Sparse injection tokens with gaps must not leak host
    /// fragments. When injection tokens don't cover the full line (e.g., `x = 1`
    /// produces tokens at columns 0, 2, 4 but not at 1 or 3), the host token
    /// for the entire content line must be fully excluded — not split around
    /// the injection tokens by the sweep line.
    #[tokio::test]
    async fn test_no_host_leak_in_gaps_between_sparse_injection_tokens() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Lua code with sparse tokens: `x = 1` has just variable, operator, number
        // with space gaps at columns 1 and 3.
        let text = "```lua\nx = 1\n```\n".to_string();
        // Lines:
        //   0: "```lua"
        //   1: "x = 1"
        //   2: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Use default capture mappings so @markup.raw.block → `string`
        let capture_mappings = default_capture_mappings();

        let result = handle_semantic_tokens_full(
            text,
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2, the host @markup.raw.block type
        let string_token_type = 2u32;

        // Content line 1: "x = 1" — must have NO host `string` fragments
        let line1_host_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 1 && t.3 == string_token_type)
            .collect();
        assert!(
            line1_host_leaks.is_empty(),
            "Host `string` tokens must not leak into gaps on content line 1. \
             Leaked: {:?}. All decoded: {:?}",
            line1_host_leaks,
            decoded
        );

        // Content line 1 should have injection tokens (Lua captures)
        let line1_injection: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        assert!(
            !line1_injection.is_empty(),
            "Should have Lua injection tokens on line 1. Decoded: {:?}",
            decoded
        );

        // Fence lines should still have host tokens
        let line0_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 0).collect();
        assert!(
            !line0_tokens.is_empty(),
            "Fence line 0 should have host tokens. Decoded: {:?}",
            decoded
        );
        let line2_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 2).collect();
        assert!(
            !line2_tokens.is_empty(),
            "Fence line 2 should have host tokens. Decoded: {:?}",
            decoded
        );
    }

    /// Integration test matching real-world scenario: Python code block with
    /// multiline content including blank lines. The `@markup.raw.block` host
    /// token (mapped to `string`) spans the entire fenced_code_block node.
    /// Stage 1 must exclude ALL per-line fragments on content lines.
    #[tokio::test]
    async fn test_no_host_leak_python_multiline_code_block() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let py_result = coordinator.ensure_language_loaded("python");
        if !md_result.success || !py_result.success {
            eprintln!("Skipping: markdown or python parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Multiline Python code block matching the screenshot scenario
        let text = "```python\nx: str = 1\n\ndef f(a: int) -> str:\n    return a\n\nf(x)\n```\n"
            .to_string();
        // Lines:
        //   0: "```python"
        //   1: "x: str = 1"
        //   2: ""
        //   3: "def f(a: int) -> str:"
        //   4: "    return a"
        //   5: ""
        //   6: "f(x)"
        //   7: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = default_capture_mappings();
        let result = handle_semantic_tokens_full(
            text,
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2, the host @markup.raw.block type
        let string_token_type = 2u32;

        // Content lines 1-6 must have NO host `string` fragments
        for content_line in 1..=6 {
            let leaks: Vec<_> = decoded
                .iter()
                .filter(|t| t.0 == content_line && t.3 == string_token_type)
                .collect();
            assert!(
                leaks.is_empty(),
                "Host `string` tokens must not leak onto content line {}. \
                 Leaked: {:?}. All decoded: {:?}",
                content_line,
                leaks,
                decoded
            );
        }

        // Fence lines should have host tokens
        let line0_has_host = decoded.iter().any(|t| t.0 == 0);
        let line7_has_host = decoded.iter().any(|t| t.0 == 7);
        assert!(
            line0_has_host,
            "Fence line 0 should have tokens. Decoded: {:?}",
            decoded
        );
        assert!(
            line7_has_host,
            "Fence line 7 should have tokens. Decoded: {:?}",
            decoded
        );

        // Content lines should have injection tokens (Python captures)
        let line1_injection: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        assert!(
            !line1_injection.is_empty(),
            "Should have Python injection tokens on line 1. Decoded: {:?}",
            decoded
        );
    }

    /// Regression test: when `supports_multiline` is true, multiline host
    /// tokens (e.g., `@markup.raw.block` on `fenced_code_block`) are emitted
    /// as a single token starting at the fence line. Stage 1 exclusion must
    /// still suppress the host token on content lines — it cannot let a
    /// multiline token leak through because its start position is outside
    /// the injection region.
    #[tokio::test]
    async fn test_no_host_leak_with_multiline_support() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        let text = "```lua\nlocal x = 42\n```\n".to_string();
        // Lines:
        //   0: "```lua"
        //   1: "local x = 42"
        //   2: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = default_capture_mappings();

        // KEY: supports_multiline = true
        let result = handle_semantic_tokens_full(
            text,
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            true, // multiline support enabled!
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2
        let string_token_type = 2u32;

        // With multiline support, a single `string` token might start on
        // line 0 (fence) but extend past the fence content into content lines.
        // Line 0 = "```lua" (6 UTF-16 chars). Any `string` token on line 0
        // with col + length > 6 is a multiline host token leaking into content.
        let line0_width = 6u32; // "```lua"
        let multiline_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.3 == string_token_type && t.0 == 0 && t.1 + t.2 > line0_width)
            .collect();
        assert!(
            multiline_leaks.is_empty(),
            "Multiline host `string` tokens must not extend past fence line into content. \
             Leaked: {:?}. All decoded: {:?}",
            multiline_leaks,
            decoded
        );

        // Content line 1 must have NO host `string` tokens starting there either
        let line1_host_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 1 && t.3 == string_token_type)
            .collect();
        assert!(
            line1_host_leaks.is_empty(),
            "Host `string` must not leak onto content line 1. \
             Leaked: {:?}. All decoded: {:?}",
            line1_host_leaks,
            decoded
        );

        // Should still have injection tokens on line 1
        let line1_injection: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        assert!(
            !line1_injection.is_empty(),
            "Should have injection tokens on line 1. Decoded: {:?}",
            decoded
        );
    }

    /// Integration test matching the exact screenshot: Python code block
    /// followed by a non-language code block. Tests that the two blocks
    /// don't interfere with each other's host token exclusion.
    #[tokio::test]
    async fn test_no_host_leak_python_plus_nolang_code_blocks() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let py_result = coordinator.ensure_language_loaded("python");
        if !md_result.success || !py_result.success {
            eprintln!("Skipping: markdown or python parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Document matching screenshot: Python block + no-language block
        let text = "\
```python
x: str = 1

def f(a: int) -> str:
    return a

f(x)
```

```
foo
```
"
        .to_string();
        // Lines:
        //   0: "```python"
        //   1: "x: str = 1"
        //   2: ""
        //   3: "def f(a: int) -> str:"
        //   4: "    return a"
        //   5: ""
        //   6: "f(x)"
        //   7: "```"
        //   8: ""
        //   9: "```"
        //  10: "foo"
        //  11: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = default_capture_mappings();
        let result = handle_semantic_tokens_full(
            text,
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2
        let string_token_type = 2u32;

        // Python content lines 1-6: NO host `string` fragments
        for content_line in 1u32..=6 {
            let leaks: Vec<_> = decoded
                .iter()
                .filter(|t| t.0 == content_line && t.3 == string_token_type)
                .collect();
            assert!(
                leaks.is_empty(),
                "Host `string` tokens must not leak onto Python content line {}. \
                 Leaked: {:?}. All decoded: {:?}",
                content_line,
                leaks,
                decoded
            );
        }

        // No-language block content line 10: `string` SHOULD be present
        // (inactive injection region — no captures produced)
        let line10_string: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 10 && t.3 == string_token_type)
            .collect();
        assert!(
            !line10_string.is_empty(),
            "Non-language block content line 10 should have `string` token. Decoded: {:?}",
            decoded
        );

        // Fence lines should have tokens
        assert!(
            decoded.iter().any(|t| t.0 == 0),
            "Python fence line 0 should have tokens"
        );
        assert!(
            decoded.iter().any(|t| t.0 == 7),
            "Python fence line 7 should have tokens"
        );
    }

    /// Integration test: Blockquote-wrapped fenced code blocks should produce
    /// consistent injection tokens across all content lines, and `> ` prefixes
    /// should not leak into the injection parser or suppress host tokens.
    #[tokio::test]
    async fn test_blockquote_injection_consistent_tokens() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Blockquote with a Lua code block — two identical content lines
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n".to_string();
        // Lines:
        //   0: "> ```lua"
        //   1: "> local x = 1"
        //   2: "> local y = 2"
        //   3: "> ```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = default_capture_mappings();

        let result = handle_semantic_tokens_full(
            text,
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        let keyword_type = 1u32; // SemanticTokenType::KEYWORD
        let string_type = 2u32; // SemanticTokenType::STRING

        // Both content lines should have injection tokens
        let line1_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        let line2_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 2).collect();

        assert!(
            !line1_tokens.is_empty(),
            "Line 1 should have injection tokens. All: {:?}",
            decoded
        );
        assert!(
            !line2_tokens.is_empty(),
            "Line 2 should have injection tokens. All: {:?}",
            decoded
        );

        // Both lines should have `keyword` token for `local` at the same column
        let line1_keywords: Vec<_> = line1_tokens
            .iter()
            .filter(|t| t.3 == keyword_type)
            .collect();
        let line2_keywords: Vec<_> = line2_tokens
            .iter()
            .filter(|t| t.3 == keyword_type)
            .collect();

        assert!(
            !line1_keywords.is_empty(),
            "Line 1 should have keyword token for 'local'. Line 1 tokens: {:?}",
            line1_tokens
        );
        assert!(
            !line2_keywords.is_empty(),
            "Line 2 should have keyword token for 'local'. Line 2 tokens: {:?}",
            line2_tokens
        );

        // Keyword column should be at absolute column 2 (after `> ` prefix)
        assert_eq!(
            line1_keywords[0].1, 2,
            "Keyword 'local' should start at column 2 (after `> `). L1: {:?}",
            line1_keywords
        );
        // Token columns should match between lines (both after "> ")
        assert_eq!(
            line1_keywords[0].1, line2_keywords[0].1,
            "Keyword column should be identical on both lines. L1: {:?}, L2: {:?}",
            line1_keywords, line2_keywords
        );

        // Token sequences (col, len, type) should be identical on both lines
        let line1_types: Vec<(u32, u32, u32)> =
            line1_tokens.iter().map(|t| (t.1, t.2, t.3)).collect();
        let line2_types: Vec<(u32, u32, u32)> =
            line2_tokens.iter().map(|t| (t.1, t.2, t.3)).collect();
        assert_eq!(
            line1_types, line2_types,
            "Both content lines should produce identical token sequences (col, len, type)"
        );

        // No host `string` tokens INSIDE the injection region (after col 2).
        // The `> ` prefix at col 0-2 IS a valid host token (outside injection).
        let line1_string_leaks: Vec<_> = line1_tokens
            .iter()
            .filter(|t| t.3 == string_type && t.1 >= 2)
            .collect();
        let line2_string_leaks: Vec<_> = line2_tokens
            .iter()
            .filter(|t| t.3 == string_type && t.1 >= 2)
            .collect();
        assert!(
            line1_string_leaks.is_empty(),
            "Host `string` tokens must not leak inside injection on line 1. Leaks: {:?}",
            line1_string_leaks
        );
        assert!(
            line2_string_leaks.is_empty(),
            "Host `string` tokens must not leak inside injection on line 2. Leaks: {:?}",
            line2_string_leaks
        );

        // With generic prefix detection, the `> ` prefix is stripped from the
        // host fenced_code_block token (it's structural markup, not code content).
        // The `> ` area has no semantic token since @punctuation.special is not
        // in LEGEND_TYPES.
        let line1_prefix: Vec<_> = line1_tokens
            .iter()
            .filter(|t| t.1 == 0 && t.3 == string_type)
            .collect();
        let line2_prefix: Vec<_> = line2_tokens
            .iter()
            .filter(|t| t.1 == 0 && t.3 == string_type)
            .collect();
        assert!(
            line1_prefix.is_empty(),
            "Line 1 should NOT have host `string` for `> ` prefix (stripped by prefix detection). Got: {:?}",
            line1_prefix
        );
        assert!(
            line2_prefix.is_empty(),
            "Line 2 should NOT have host `string` for `> ` prefix (stripped by prefix detection). Got: {:?}",
            line2_prefix
        );
    }
}
