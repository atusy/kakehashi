//! Semantic token delta calculation.
//!
//! This module implements the LSP semantic tokens delta algorithm,
//! which efficiently encodes changes between two sets of tokens.

use tower_lsp_server::ls_types::{
    SemanticToken, SemanticTokens, SemanticTokensDelta, SemanticTokensEdit,
    SemanticTokensFullDeltaResult,
};

/// Calculate delta or return full tokens.
///
/// This is a public helper for the incremental tokenization path.
/// It calculates a delta if possible, otherwise returns the current tokens.
pub fn calculate_delta_or_full(
    previous: &SemanticTokens,
    current: &SemanticTokens,
    expected_result_id: &str,
) -> SemanticTokensFullDeltaResult {
    if previous.result_id.as_deref() == Some(expected_result_id)
        && let Some(delta) = calculate_semantic_tokens_delta(previous, current)
    {
        return SemanticTokensFullDeltaResult::TokensDelta(delta);
    }
    SemanticTokensFullDeltaResult::Tokens(current.clone())
}

/// Check if two semantic tokens are equal
#[inline]
pub(super) fn tokens_equal(a: &SemanticToken, b: &SemanticToken) -> bool {
    *a == *b
}

/// Calculate delta between two sets of semantic tokens using prefix-suffix matching.
///
/// Suffix matching is disabled when total line deltas differ, since tokens with
/// identical delta encoding would be at different absolute positions.
pub(super) fn calculate_semantic_tokens_delta(
    previous: &SemanticTokens,
    current: &SemanticTokens,
) -> Option<SemanticTokensDelta> {
    // --- Step 1: Find common prefix ---
    let common_prefix_len = previous
        .data
        .iter()
        .zip(current.data.iter())
        .take_while(|(a, b)| tokens_equal(a, b))
        .count();

    // If all tokens are the same, no edits needed
    if common_prefix_len == previous.data.len() && common_prefix_len == current.data.len() {
        return Some(SemanticTokensDelta {
            result_id: current.result_id.clone(),
            edits: vec![],
        });
    }

    // --- Step 2: Find common suffix (with line change safety) ---
    let prev_suffix = &previous.data[common_prefix_len..];
    let curr_suffix = &current.data[common_prefix_len..];

    // When line count changes, identical delta encoding maps to different absolute
    // positions, so suffix matching is unsafe.
    let prev_total_lines: u32 = previous.data.iter().map(|t| t.delta_line).sum();
    let curr_total_lines: u32 = current.data.iter().map(|t| t.delta_line).sum();

    let common_suffix_len = if prev_total_lines != curr_total_lines {
        // Line count changed - disable suffix optimization
        0
    } else {
        // Safe to find matching suffix
        prev_suffix
            .iter()
            .rev()
            .zip(curr_suffix.iter().rev())
            .take_while(|(a, b)| tokens_equal(a, b))
            .count()
    };

    // --- Step 3: Calculate the edit ---
    // LSP spec requires start and deleteCount to be integer indices into the
    // flattened token array, not token indices. Each SemanticToken serializes
    // to 5 u32 values, so we must multiply by 5.
    let start_token = common_prefix_len;
    let delete_token_count = prev_suffix.len() - common_suffix_len;
    let insert_token_count = curr_suffix.len() - common_suffix_len;
    let data = current.data[start_token..start_token + insert_token_count].to_vec();

    Some(SemanticTokensDelta {
        result_id: current.result_id.clone(),
        edits: vec![SemanticTokensEdit {
            start: (start_token * 5) as u32,
            delete_count: (delete_token_count * 5) as u32,
            data: Some(data),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semantic_tokens_delta() {
        // Create mock semantic tokens for testing
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 10,
                    token_type: 0, // comment
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 3,
                    token_type: 1, // keyword
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 1,
                    token_type: 17, // variable
                    token_modifiers_bitset: 0,
                },
            ],
        };

        // Modified tokens (changed comment length)
        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 14,    // changed length
                    token_type: 0, // comment
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 3,
                    token_type: 1, // keyword
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 1,
                    token_type: 17, // variable
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.result_id, Some("v2".to_string()));
        assert_eq!(delta.edits.len(), 1);
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(delta.edits[0].start, 0);
        // With suffix matching: only the first token (comment) changed
        // The last two tokens (keyword, variable) are suffix matched
        assert_eq!(delta.edits[0].delete_count, 5); // 1 token * 5 integers
        let edits_data = delta.edits[0]
            .data
            .as_ref()
            .expect("delta edits should include replacement data");
        assert_eq!(edits_data.len(), 1);
    }

    #[test]
    fn test_semantic_tokens_delta_no_changes() {
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

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 0);
    }
}
