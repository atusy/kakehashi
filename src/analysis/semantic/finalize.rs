//! Token post-processing and LSP encoding.
//!
//! This module handles the final steps of semantic token processing:
//! - Excluding host tokens inside active injection regions
//! - Splitting overlapping tokens via sweep line algorithm
//! - Converting to LSP SemanticToken format with delta-relative positions
//!
//! Note: The term "delta encoding" in this module refers to the LSP protocol's
//! relative position encoding (delta_line, delta_start), not the
//! SemanticTokensDelta optimization which is handled by the `delta` module.

use tower_lsp_server::ls_types::{SemanticToken, SemanticTokens, SemanticTokensResult};

use super::legend::map_capture_to_token_type_and_modifiers;
use super::token_collector::{InjectionRegion, RawToken};

/// Priority key for token comparison. Higher values win.
fn token_priority(t: &RawToken) -> (usize, usize) {
    (t.depth, t.pattern_index)
}

/// Compute the UTF-16 width of a string.
fn utf16_width(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

/// Split multiline tokens into per-line tokens.
///
/// The sweep line algorithm groups tokens by `token.line` and treats
/// `[column, column+length)` as a 1D interval on that line. Multiline
/// tokens encode their total UTF-16 length (including +1 per inter-line
/// newline) in `length`, producing invalid fragments when the sweep line
/// splits around other tokens on the same start line.
fn split_multiline_tokens(tokens: Vec<RawToken>, lines: &[&str]) -> Vec<RawToken> {
    let mut result = Vec::with_capacity(tokens.len());
    for token in tokens {
        // If the token's line is beyond the lines array, keep as-is (no line
        // info to determine whether it's multiline).
        let Some(line_text) = lines.get(token.line) else {
            result.push(token);
            continue;
        };

        let line_width = utf16_width(line_text);

        // Single-line token: column + length fits within the line
        if token.column + token.length <= line_width {
            result.push(token);
            continue;
        }

        // Multiline token: split into per-line fragments
        let mut remaining = token.length;
        let mut current_line = token.line;
        let mut start_col = token.column;

        while remaining > 0 && current_line < lines.len() {
            let current_line_width = utf16_width(lines[current_line]);
            let per_line_len = remaining.min(current_line_width.saturating_sub(start_col));

            result.push(RawToken {
                line: current_line,
                column: start_col,
                length: per_line_len,
                mapped_name: token.mapped_name.clone(),
                depth: token.depth,
                pattern_index: token.pattern_index,
            });

            // Subtract per_line_len + 1 (the +1 accounts for the newline between lines)
            remaining = remaining.saturating_sub(per_line_len + 1);
            current_line += 1;
            start_col = 0; // subsequent lines start at column 0
        }
    }
    result
}

/// Split overlapping tokens on the same line using a sweep line algorithm.
///
/// For each line, collects breakpoints (start/end columns of all tokens),
/// then for each interval picks the highest-priority token as the winner.
/// Priority is determined by `(depth DESC, pattern_index DESC)`.
///
/// This replaces the previous dedup-at-same-position approach, producing
/// non-overlapping fragments that preserve both parent and child semantics.
fn split_overlapping_tokens(mut tokens: Vec<RawToken>) -> Vec<RawToken> {
    if tokens.is_empty() {
        return tokens;
    }

    // Sort by line first, then by start column for grouping
    tokens.sort_by(|a, b| a.line.cmp(&b.line).then(a.column.cmp(&b.column)));

    let mut result = Vec::with_capacity(tokens.len());

    // Group tokens by line and process each line independently
    let mut line_start = 0;
    while line_start < tokens.len() {
        let current_line = tokens[line_start].line;
        let mut line_end = line_start;
        while line_end < tokens.len() && tokens[line_end].line == current_line {
            line_end += 1;
        }

        let line_tokens = &tokens[line_start..line_end];

        // 1. Collect all breakpoints (start and end columns)
        let mut breakpoints = Vec::with_capacity(line_tokens.len() * 2);
        for t in line_tokens {
            breakpoints.push(t.column);
            breakpoints.push(t.column + t.length);
        }
        breakpoints.sort_unstable();
        breakpoints.dedup();

        // 2. For each interval [bp[i], bp[i+1]), find the winner
        for window in breakpoints.windows(2) {
            let interval_start = window[0];
            let interval_end = window[1];

            if interval_start == interval_end {
                continue; // zero-length interval
            }

            // Find the highest-priority token covering this interval
            let winner = line_tokens
                .iter()
                .filter(|t| t.column <= interval_start && t.column + t.length >= interval_end)
                .max_by_key(|t| token_priority(t));

            if let Some(winner) = winner {
                // Emit a fragment with the winner's properties for this interval
                result.push(RawToken {
                    line: current_line,
                    column: interval_start,
                    length: interval_end - interval_start,
                    mapped_name: winner.mapped_name.clone(),
                    depth: winner.depth,
                    pattern_index: winner.pattern_index,
                });
            }
        }

        line_start = line_end;
    }

    // Merge adjacent fragments with the same properties to reduce output size
    merge_adjacent_fragments(&mut result);

    result
}

/// Merge adjacent fragments on the same line that have the same token type.
///
/// After sweep line splitting, fragments like `keyword[0,3) + keyword[3,5)` can
/// be merged into a single `keyword[0,5)` to reduce the number of tokens in output.
fn merge_adjacent_fragments(tokens: &mut Vec<RawToken>) {
    if tokens.len() < 2 {
        return;
    }
    let mut write = 0;
    for read in 1..tokens.len() {
        let can_merge = tokens[write].line == tokens[read].line
            && tokens[write].column + tokens[write].length == tokens[read].column
            && tokens[write].mapped_name == tokens[read].mapped_name
            && tokens[write].depth == tokens[read].depth
            && tokens[write].pattern_index == tokens[read].pattern_index;

        if can_merge {
            tokens[write].length += tokens[read].length;
        } else {
            write += 1;
            if write != read {
                tokens[write] = tokens[read].clone();
            }
        }
    }
    tokens.truncate(write + 1);
}

/// Check whether a single-line token is inside any active injection region.
///
/// Uses lexicographic tuple comparison: `(line, col) >= (start_line, start_col)`
/// covers all four cases (middle line, start line, end line, single-line region).
fn is_in_active_injection_region(token: &RawToken, regions: &[InjectionRegion]) -> bool {
    let token_end = token.column + token.length;
    regions.iter().any(|r| {
        (token.line, token.column) >= (r.start_line, r.start_col)
            && (token.line, token_end) <= (r.end_line, r.end_col)
    })
}

/// Post-process and delta-encode raw tokens into SemanticTokensResult.
///
/// This shared helper:
/// 1. Excludes host tokens inside active injection regions
/// 2. Splits overlapping tokens via sweep line
/// 3. Delta-encodes for LSP protocol
pub(super) fn finalize_tokens(
    all_tokens: Vec<RawToken>,
    active_injection_regions: &[InjectionRegion],
    lines: &[&str],
) -> Option<SemanticTokensResult> {
    // Split multiline tokens into per-line tokens before the sweep line,
    // which treats [column, column+length) as a 1D interval on a single line.
    let mut all_tokens = split_multiline_tokens(all_tokens, lines);

    // Filter out zero-length tokens before the sweep line overlap resolution.
    // Unknown captures are already filtered at collection time (apply_capture_mapping returns None).
    all_tokens.retain(|token| token.length > 0);

    // Injection region exclusion: remove host tokens (depth=0) inside active
    // injection regions.
    if !active_injection_regions.is_empty() {
        all_tokens.retain(|token| {
            // Non-host tokens (injection tokens) are always kept
            if token.depth > 0 {
                return true;
            }
            // Remove host tokens inside active injection regions
            !is_in_active_injection_region(token, active_injection_regions)
        });
    }

    // Split overlapping tokens using sweep line algorithm.
    // This replaces the old sort + dedup approach, producing non-overlapping
    // fragments that preserve both parent and child semantics.
    let all_tokens = split_overlapping_tokens(all_tokens);

    if all_tokens.is_empty() {
        return None;
    }

    // Delta-encode
    let mut data = Vec::with_capacity(all_tokens.len());
    let mut last_line = 0usize;
    let mut last_start = 0usize;

    for token in all_tokens {
        // Unknown types are already filtered at collection time (apply_capture_mapping returns None),
        // so map_capture_to_token_type_and_modifiers should always return Some here.
        let (token_type, token_modifiers_bitset) =
            map_capture_to_token_type_and_modifiers(&token.mapped_name)
                .expect("all tokens should have known types after apply_capture_mapping filtering");

        let delta_line = token.line - last_line;
        let delta_start = if delta_line == 0 {
            token.column - last_start
        } else {
            token.column
        };

        data.push(SemanticToken {
            delta_line: delta_line as u32,
            delta_start: delta_start as u32,
            length: token.length as u32,
            token_type,
            token_modifiers_bitset,
        });

        last_line = token.line;
        last_start = token.column;
    }

    Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Helper to create a RawToken for testing
    fn make_token(
        line: usize,
        column: usize,
        length: usize,
        name: &str,
        depth: usize,
        pattern_index: usize,
    ) -> RawToken {
        RawToken {
            line,
            column,
            length,
            mapped_name: name.to_string(),
            depth,
            pattern_index,
        }
    }

    #[test]
    fn finalize_tokens_returns_none_for_empty_input() {
        let tokens: Vec<RawToken> = vec![];
        assert!(finalize_tokens(tokens, &[], &[]).is_none());
    }

    #[test]
    fn finalize_tokens_filters_zero_length_tokens() {
        let tokens = vec![
            make_token(0, 0, 0, "keyword", 0, 0), // zero length - should be filtered
            make_token(0, 5, 3, "variable", 0, 0), // valid
        ];
        let result = finalize_tokens(tokens, &[], &[]);
        assert!(result.is_some());

        if let Some(SemanticTokensResult::Tokens(semantic_tokens)) = result {
            assert_eq!(semantic_tokens.data.len(), 1);
            assert_eq!(semantic_tokens.data[0].delta_start, 5);
        } else {
            panic!("Expected Tokens variant");
        }
    }

    #[test]
    fn finalize_tokens_returns_none_when_all_tokens_are_zero_length() {
        let tokens = vec![
            make_token(0, 0, 0, "keyword", 0, 0),
            make_token(1, 5, 0, "variable", 0, 0),
        ];
        assert!(finalize_tokens(tokens, &[], &[]).is_none());
    }

    #[test]
    fn finalize_tokens_sorts_by_position() {
        let tokens = vec![
            make_token(1, 0, 3, "keyword", 0, 0),  // line 1
            make_token(0, 10, 3, "string", 0, 0),  // line 0, col 10
            make_token(0, 0, 3, "function", 0, 0), // line 0, col 0
        ];
        let result = finalize_tokens(tokens, &[], &[]);
        assert!(result.is_some());

        if let Some(SemanticTokensResult::Tokens(semantic_tokens)) = result {
            assert_eq!(semantic_tokens.data.len(), 3);
            // First token: line 0, col 0 (delta_line=0, delta_start=0)
            assert_eq!(semantic_tokens.data[0].delta_line, 0);
            assert_eq!(semantic_tokens.data[0].delta_start, 0);
            // Second token: line 0, col 10 (delta_line=0, delta_start=10)
            assert_eq!(semantic_tokens.data[1].delta_line, 0);
            assert_eq!(semantic_tokens.data[1].delta_start, 10);
            // Third token: line 1, col 0 (delta_line=1, delta_start=0)
            assert_eq!(semantic_tokens.data[2].delta_line, 1);
            assert_eq!(semantic_tokens.data[2].delta_start, 0);
        } else {
            panic!("Expected Tokens variant");
        }
    }

    #[test]
    fn finalize_tokens_delta_encoding_same_line() {
        // Multiple tokens on the same line use delta_start relative to previous token
        let tokens = vec![
            make_token(0, 0, 3, "keyword", 0, 0),
            make_token(0, 5, 4, "function", 0, 0),
            make_token(0, 12, 2, "variable", 0, 0),
        ];
        let result = finalize_tokens(tokens, &[], &[]);
        assert!(result.is_some());

        if let Some(SemanticTokensResult::Tokens(semantic_tokens)) = result {
            assert_eq!(semantic_tokens.data.len(), 3);
            // First: delta_line=0, delta_start=0
            assert_eq!(semantic_tokens.data[0].delta_line, 0);
            assert_eq!(semantic_tokens.data[0].delta_start, 0);
            // Second: delta_line=0, delta_start=5 (relative to previous start=0)
            assert_eq!(semantic_tokens.data[1].delta_line, 0);
            assert_eq!(semantic_tokens.data[1].delta_start, 5);
            // Third: delta_line=0, delta_start=7 (12 - 5 = 7)
            assert_eq!(semantic_tokens.data[2].delta_line, 0);
            assert_eq!(semantic_tokens.data[2].delta_start, 7);
        } else {
            panic!("Expected Tokens variant");
        }
    }

    #[test]
    fn finalize_tokens_delta_encoding_new_line_resets_column() {
        // When moving to a new line, delta_start is absolute column (not relative)
        let tokens = vec![
            make_token(0, 5, 3, "keyword", 0, 0),
            make_token(1, 10, 4, "function", 0, 0),
        ];
        let result = finalize_tokens(tokens, &[], &[]);
        assert!(result.is_some());

        if let Some(SemanticTokensResult::Tokens(semantic_tokens)) = result {
            assert_eq!(semantic_tokens.data.len(), 2);
            // First: delta_line=0, delta_start=5
            assert_eq!(semantic_tokens.data[0].delta_line, 0);
            assert_eq!(semantic_tokens.data[0].delta_start, 5);
            // Second: delta_line=1, delta_start=10 (absolute, not relative)
            assert_eq!(semantic_tokens.data[1].delta_line, 1);
            assert_eq!(semantic_tokens.data[1].delta_start, 10);
        } else {
            panic!("Expected Tokens variant");
        }
    }

    // ── sweep line (split_overlapping_tokens) tests ──────────────────

    /// Helper to extract RawTokens from split_overlapping_tokens output for assertion.
    /// Returns (column, length, mapped_name) tuples sorted by position.
    fn extract_fragments(tokens: Vec<RawToken>) -> Vec<(usize, usize, String)> {
        let result = split_overlapping_tokens(tokens);
        result
            .into_iter()
            .map(|t| (t.column, t.length, t.mapped_name.clone()))
            .collect()
    }

    #[test]
    fn split_no_overlap_both_survive() {
        // Two disjoint tokens on the same line → both survive unchanged.
        let tokens = vec![
            make_token(0, 0, 3, "keyword", 0, 0),
            make_token(0, 5, 4, "variable", 0, 0),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![
                (0, 3, "keyword".to_string()),
                (5, 4, "variable".to_string()),
            ]
        );
    }

    #[test]
    fn split_full_containment_parent_splits_into_two() {
        // Parent [0,10) at pattern_index=0, child [3,7) at pattern_index=1.
        // Child's later pattern_index wins; parent should split into [0,3) and [7,10).
        let tokens = vec![
            make_token(0, 0, 10, "keyword", 0, 0),
            make_token(0, 3, 4, "variable", 0, 1),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![
                (0, 3, "keyword".to_string()),
                (3, 4, "variable".to_string()),
                (7, 3, "keyword".to_string()),
            ]
        );
    }

    #[test]
    fn split_multiple_children_three_fragments() {
        // Parent [0,15) at pattern_index=0.
        // Child A [2,5) at pattern_index=1, Child B [8,12) at pattern_index=1.
        // Expected: parent [0,2), child A [2,5), parent [5,8), child B [8,12), parent [12,15).
        let tokens = vec![
            make_token(0, 0, 15, "keyword", 0, 0),
            make_token(0, 2, 3, "variable", 0, 1),
            make_token(0, 8, 4, "string", 0, 1),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![
                (0, 2, "keyword".to_string()),
                (2, 3, "variable".to_string()),
                (5, 3, "keyword".to_string()),
                (8, 4, "string".to_string()),
                (12, 3, "keyword".to_string()),
            ]
        );
    }

    #[test]
    fn split_adjacent_children_no_gap() {
        // Parent [0,10) at pattern_index=0.
        // Child A [0,5) and Child B [5,10) at pattern_index=1 — no gap.
        // Parent should produce no fragments (entirely covered by children).
        let tokens = vec![
            make_token(0, 0, 10, "keyword", 0, 0),
            make_token(0, 0, 5, "variable", 0, 1),
            make_token(0, 5, 5, "string", 0, 1),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![(0, 5, "variable".to_string()), (5, 5, "string".to_string()),]
        );
    }

    #[test]
    fn split_same_position_same_depth_latest_pattern_wins() {
        // Two tokens at the same position and depth.
        // Higher pattern_index wins — no split needed.
        let tokens = vec![
            make_token(0, 0, 5, "variable", 0, 0),
            make_token(0, 0, 5, "type.builtin", 0, 10),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(fragments, vec![(0, 5, "type.builtin".to_string())]);
    }

    #[test]
    fn split_same_position_different_depth_higher_wins() {
        // Two tokens at same position, different injection depth.
        // Higher injection depth wins.
        let tokens = vec![
            make_token(0, 0, 5, "string", 0, 0),
            make_token(0, 0, 5, "keyword", 1, 0),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(fragments, vec![(0, 5, "keyword".to_string())]);
    }

    #[test]
    fn split_three_level_nesting() {
        // heading [0,20) pi=0, bold [5,15) pi=1, italic [8,12) pi=2.
        // Expected: heading [0,5), bold [5,8), italic [8,12), bold [12,15), heading [15,20).
        let tokens = vec![
            make_token(0, 0, 20, "keyword", 0, 0),
            make_token(0, 5, 10, "variable", 0, 1),
            make_token(0, 8, 4, "string", 0, 2),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![
                (0, 5, "keyword".to_string()),
                (5, 3, "variable".to_string()),
                (8, 4, "string".to_string()),
                (12, 3, "variable".to_string()),
                (15, 5, "keyword".to_string()),
            ]
        );
    }

    #[test]
    fn split_zero_length_fragment_filtered() {
        // Parent [0,5) at pattern_index=0, child [0,5) at pattern_index=1.
        // Parent is entirely covered → produces zero-length fragments → filtered.
        let tokens = vec![
            make_token(0, 0, 5, "keyword", 0, 0),
            make_token(0, 0, 5, "variable", 0, 1),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(fragments, vec![(0, 5, "variable".to_string())]);
    }

    #[test]
    fn split_partial_overlap_without_containment() {
        // Token A [0,10) at pattern_index=0, Token B [5,15) at pattern_index=1.
        // Expected: A [0,5), B [5,15).
        let tokens = vec![
            make_token(0, 0, 10, "keyword", 0, 0),
            make_token(0, 5, 10, "variable", 0, 1),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![
                (0, 5, "keyword".to_string()),
                (5, 10, "variable".to_string()),
            ]
        );
    }

    #[test]
    fn split_across_multiple_lines() {
        // Line 0: parent [0,10) pi=0, child [3,7) pi=1.
        // Line 1: single token [2,5) pi=0.
        // Each line processed independently.
        let tokens = vec![
            make_token(0, 0, 10, "keyword", 0, 0),
            make_token(0, 3, 4, "variable", 0, 1),
            make_token(1, 2, 3, "string", 0, 0),
        ];
        let result = split_overlapping_tokens(tokens);
        let line0: Vec<_> = result
            .iter()
            .filter(|t| t.line == 0)
            .map(|t| (t.column, t.length, t.mapped_name.clone()))
            .collect();
        let line1: Vec<_> = result
            .iter()
            .filter(|t| t.line == 1)
            .map(|t| (t.column, t.length, t.mapped_name.clone()))
            .collect();
        assert_eq!(
            line0,
            vec![
                (0, 3, "keyword".to_string()),
                (3, 4, "variable".to_string()),
                (7, 3, "keyword".to_string()),
            ]
        );
        assert_eq!(line1, vec![(2, 3, "string".to_string())]);
    }

    #[test]
    fn split_higher_pattern_index_beats_deeper_node() {
        // Simulates Rust `///` doc comments: the anonymous `"/"` node is deeper
        // in the AST (node_depth=3) than `line_comment` (node_depth=1), but
        // `@comment` appears later in the query file (pattern_index=10 > 5).
        // Neovim uses pattern_index to resolve this; node_depth should NOT override it.
        let tokens = vec![
            make_token(0, 0, 3, "operator", 0, 5), // earlier pattern
            make_token(0, 0, 3, "comment", 0, 10), // later pattern — wins
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(fragments, vec![(0, 3, "comment".to_string())]);
    }

    // ── injection region exclusion tests ──────────────────────────────

    #[test]
    fn finalize_excludes_host_token_inside_active_injection_region() {
        // Host token (depth=0) on line 3 falls inside injection region lines 2-4.
        // Should be excluded.
        let tokens = vec![
            make_token(0, 0, 5, "keyword", 0, 0), // line 0 — outside region
            make_token(3, 0, 12, "string", 0, 0), // line 3 — inside region
        ];
        let regions = vec![InjectionRegion {
            start_line: 2,
            start_col: 0,
            end_line: 4,
            end_col: 0,
        }];
        let result = finalize_tokens(tokens, &regions, &[]);
        assert!(result.is_some());
        let SemanticTokensResult::Tokens(st) = result.unwrap() else {
            panic!("Expected Tokens");
        };
        // Only line 0 token should survive
        assert_eq!(
            st.data.len(),
            1,
            "Host token inside injection should be excluded"
        );
        assert_eq!(st.data[0].delta_line, 0);
        assert_eq!(st.data[0].delta_start, 0);
        assert_eq!(st.data[0].length, 5);
    }

    #[test]
    fn finalize_preserves_injection_tokens_inside_region() {
        // depth=1 tokens (injection) should always be kept.
        let tokens = vec![
            make_token(3, 0, 5, "keyword", 1, 0), // injection token
        ];
        let regions = vec![InjectionRegion {
            start_line: 2,
            start_col: 0,
            end_line: 4,
            end_col: 0,
        }];
        let result = finalize_tokens(tokens, &regions, &[]);
        assert!(result.is_some(), "Injection tokens should always survive");
    }

    #[test]
    fn finalize_no_exclusion_when_no_active_regions() {
        // No active regions → all host tokens survive.
        let tokens = vec![make_token(3, 0, 12, "string", 0, 0)];
        let result = finalize_tokens(tokens, &[], &[]);
        assert!(result.is_some());
    }

    // At same position, the sweep line picks the winner by priority (depth DESC, pattern_index DESC).
    #[rstest]
    #[case::deeper_injection_wins(
        ("string", 0, 0), ("keyword", 1, 0), "keyword"
    )]
    #[case::latest_pattern_wins(
        ("variable", 0, 0), ("type.builtin", 0, 10), "type.builtin"
    )]
    #[case::latest_pattern_wins_reversed_insertion(
        ("type.builtin", 0, 10), ("variable", 0, 0), "type.builtin"
    )]
    #[case::depth_beats_pattern_index(
        ("variable", 0, 99), ("keyword", 1, 0), "keyword"
    )]
    fn finalize_tokens_sweep_priority(
        #[case] token_a: (&str, usize, usize),
        #[case] token_b: (&str, usize, usize),
        #[case] expected_winner: &str,
    ) {
        let tokens = vec![
            make_token(0, 0, 5, token_a.0, token_a.1, token_a.2),
            make_token(0, 0, 5, token_b.0, token_b.1, token_b.2),
        ];
        let result = finalize_tokens(tokens, &[], &[]);

        let SemanticTokensResult::Tokens(semantic_tokens) = result.expect("should produce tokens")
        else {
            panic!("Expected Tokens variant");
        };
        assert_eq!(semantic_tokens.data.len(), 1);
        let (expected_type, _) = map_capture_to_token_type_and_modifiers(expected_winner)
            .expect("expected_winner should be a known capture");
        assert_eq!(semantic_tokens.data[0].token_type, expected_type);
    }

    // ── split_multiline_tokens tests ─────────────────────────────────

    /// Helper to extract (line, column, length) tuples from split_multiline_tokens output.
    fn extract_split(tokens: Vec<RawToken>, lines: &[&str]) -> Vec<(usize, usize, usize)> {
        split_multiline_tokens(tokens, lines)
            .into_iter()
            .map(|t| (t.line, t.column, t.length))
            .collect()
    }

    #[test]
    fn split_multiline_single_line_passthrough() {
        // Token fits within line → kept as-is.
        let lines = &["hello world"];
        let tokens = vec![make_token(0, 0, 5, "keyword", 0, 0)];
        let result = extract_split(tokens, lines);
        assert_eq!(result, vec![(0, 0, 5)]);
    }

    #[test]
    fn split_multiline_two_line_token() {
        // Line 0: "abcdef" (width 6), Line 1: "ghij" (width 4)
        // Token at (line=0, col=3, length=8): occupies col 3..6 on line 0 (len=3),
        // then +1 for newline (remaining = 8-3-1=4), then col 0..4 on line 1 (len=4).
        let lines = &["abcdef", "ghij"];
        let tokens = vec![make_token(0, 3, 8, "string", 0, 0)];
        let result = extract_split(tokens, lines);
        assert_eq!(result, vec![(0, 3, 3), (1, 0, 4)]);
    }

    #[test]
    fn split_multiline_three_lines_with_empty_middle() {
        // Line 0: "abc" (width 3), Line 1: "" (width 0), Line 2: "de" (width 2)
        // Token at (line=0, col=0, length=6): line 0 len=3, newline -1 → remaining=2,
        // line 1 len=0, newline -1 → remaining=1, line 2 len=1... wait let me recalculate.
        //
        // Total length encoding: line0_content(3) + newline(1) + line1_content(0) + newline(1) + line2_content(2) = 7
        // But we want length=6 to test partial coverage of last line:
        // line0: 3, newline: 1, line1: 0, newline: 1, line2: 1 → total = 6
        let lines = &["abc", "", "de"];
        let tokens = vec![make_token(0, 0, 6, "string", 0, 0)];
        let result = extract_split(tokens, lines);
        // line 0: min(6, 3-0)=3, remaining=6-3-1=2
        // line 1: min(2, 0-0)=0, remaining=2-0-1=1
        // line 2: min(1, 2-0)=1, remaining=1-1-1=0 (saturating)
        assert_eq!(result, vec![(0, 0, 3), (1, 0, 0), (2, 0, 1)]);
    }

    #[test]
    fn split_multiline_unicode_content() {
        // CJK characters: 1 UTF-16 code unit each, but 3 bytes in UTF-8.
        // Line 0: "あいう" (UTF-16 width = 3), Line 1: "えお" (UTF-16 width = 2)
        // Token at (line=0, col=1, length=5): line 0 len=min(5,3-1)=2, remaining=5-2-1=2,
        // line 1 len=min(2,2-0)=2, remaining=2-2-1=0 (saturating)
        let lines = &["あいう", "えお"];
        let tokens = vec![make_token(0, 1, 5, "string", 0, 0)];
        let result = extract_split(tokens, lines);
        assert_eq!(result, vec![(0, 1, 2), (1, 0, 2)]);
    }

    #[test]
    fn split_multiline_token_at_eof() {
        // Token extends beyond available lines → splits what it can,
        // remaining is discarded when current_line >= lines.len().
        let lines = &["abc"];
        // Token claims to span 2 lines but only 1 line exists
        let tokens = vec![make_token(0, 0, 5, "string", 0, 0)];
        let result = extract_split(tokens, lines);
        // line 0: min(5, 3-0)=3, remaining=5-3-1=1, then current_line=1 >= lines.len()=1 → stop
        assert_eq!(result, vec![(0, 0, 3)]);
    }

    #[test]
    fn split_multiline_no_lines_passthrough() {
        // When lines is empty, tokens pass through unchanged (no line info to judge).
        let tokens = vec![make_token(0, 0, 10, "string", 0, 0)];
        let result = extract_split(tokens, &[]);
        assert_eq!(result, vec![(0, 0, 10)]);
    }

    #[test]
    fn split_multiline_preserves_metadata() {
        // Verify that split fragments retain depth, pattern_index, and mapped_name.
        let lines = &["ab", "cd"];
        let tokens = vec![make_token(0, 0, 5, "string", 1, 42)];
        let result = split_multiline_tokens(tokens, lines);
        assert_eq!(result.len(), 2);
        for frag in &result {
            assert_eq!(frag.mapped_name, "string");
            assert_eq!(frag.depth, 1);
            assert_eq!(frag.pattern_index, 42);
        }
        assert_eq!(
            (result[0].line, result[0].column, result[0].length),
            (0, 0, 2)
        );
        assert_eq!(
            (result[1].line, result[1].column, result[1].length),
            (1, 0, 2)
        );
    }
}
