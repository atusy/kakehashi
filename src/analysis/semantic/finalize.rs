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

use std::collections::HashMap;

use tower_lsp_server::ls_types::{SemanticToken, SemanticTokens, SemanticTokensResult};

use super::legend::map_capture_to_token_type_and_modifiers;
use super::token_collector::{InjectionRegion, RawToken};

/// Priority key for token comparison. Higher values win.
///
/// Comparison order: `priority` (from `#set! priority N`, default 100),
/// then `depth` (injection depth), then inverse `node_byte_len` (smaller
/// nodes are more specific and win), then `pattern_index` (later patterns win).
fn token_priority(t: &RawToken) -> (u32, usize, usize, usize) {
    (
        t.priority,
        t.depth,
        usize::MAX - t.node_byte_len,
        t.pattern_index,
    )
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
                priority: token.priority,
                node_byte_len: token.node_byte_len,
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
/// Priority is determined by `(priority DESC, depth DESC, node_byte_len ASC, pattern_index DESC)`.
/// Transparent tokens (empty `mapped_name`) are excluded from winner selection.
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

            // Find the highest-priority *visible* token covering this interval.
            // Transparent tokens (empty mapped_name) only serve as breakpoint
            // generators — they split intervals at their boundaries but don't
            // compete for winning.
            let winner = line_tokens
                .iter()
                .filter(|t| {
                    t.column <= interval_start
                        && t.column + t.length >= interval_end
                        && !t.mapped_name.is_empty()
                })
                .max_by_key(|t| token_priority(t));

            if let Some(winner) = winner {
                result.push(RawToken {
                    line: current_line,
                    column: interval_start,
                    length: interval_end - interval_start,
                    mapped_name: winner.mapped_name.clone(),
                    depth: winner.depth,
                    pattern_index: winner.pattern_index,
                    priority: winner.priority,
                    node_byte_len: winner.node_byte_len,
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
            && tokens[write].pattern_index == tokens[read].pattern_index
            && tokens[write].priority == tokens[read].priority;

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

/// Check whether a single-line token is fully inside any active injection region.
///
/// Uses lexicographic tuple comparison: `(line, col) >= (start_line, start_col)`
/// covers all four cases (middle line, start line, end line, single-line region).
fn is_fully_in_active_injection_region(token: &RawToken, regions: &[InjectionRegion]) -> bool {
    let token_end = token.column + token.length;
    regions.iter().any(|r| {
        (token.line, token.column) >= (r.start_line, r.start_col)
            && (token.line, token_end) <= (r.end_line, r.end_col)
    })
}

/// Check whether a token exactly matches any active injection region's bounds.
///
/// This mirrors the exact-match exception in `is_in_exclusion_range()`: when
/// a host capture sits on the same tree-sitter node as `@injection.content`
/// (e.g., fish `(comment) @comment` + `(comment) @injection.content`), the
/// host token should survive to the sweep-line algorithm, which splits it
/// around injection tokens and preserves the host type in uncovered gaps.
fn is_exact_match_active_injection_region(token: &RawToken, regions: &[InjectionRegion]) -> bool {
    let token_end = token.column + token.length;
    regions.iter().any(|r| {
        token.line == r.start_line
            && token.line == r.end_line
            && token.column == r.start_col
            && token_end == r.end_col
    })
}

/// Collect the injection region intervals that overlap a host token's line.
///
/// For each region that overlaps the token's line, returns the `(start_col, end_col)`
/// interval on that line. Multi-line regions produce intervals that extend to
/// line-end or start from column 0 as appropriate.
///
/// Used in tests as a helper to produce precomputed intervals for
/// `split_host_token_around_regions`. Production code uses
/// `build_region_intervals_map` which precomputes intervals for all lines in one pass.
#[cfg(test)]
fn region_intervals_on_line(
    token_line: usize,
    line_width: usize,
    regions: &[InjectionRegion],
) -> Vec<(usize, usize)> {
    let mut intervals = Vec::new();
    for r in regions {
        // Region must overlap this line
        if token_line < r.start_line || token_line > r.end_line {
            continue;
        }
        // Determine the interval on this line
        let start_col = if token_line == r.start_line {
            r.start_col
        } else {
            0
        };
        let end_col = if token_line == r.end_line {
            r.end_col
        } else {
            line_width
        };
        if start_col < end_col {
            intervals.push((start_col, end_col));
        }
    }
    // Sort intervals by (start_col, end_col). Overlaps are handled correctly
    // by the consumer's cursor-based subtraction (cursor advances past the
    // further end of overlapping intervals).
    intervals.sort_unstable();
    intervals
}

/// Precompute per-line region intervals from all injection regions.
///
/// Builds a map from line number to sorted `(start_col, end_col)` intervals,
/// so that `split_host_token_around_regions` can look up intervals in O(1)
/// instead of scanning all regions per token.
fn build_region_intervals_map(
    regions: &[InjectionRegion],
    lines: &[&str],
) -> HashMap<usize, Vec<(usize, usize)>> {
    let mut map: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
    for r in regions {
        for line_idx in r.start_line..=r.end_line {
            if line_idx >= lines.len() {
                continue;
            }
            let line_width = utf16_width(lines[line_idx]);
            let start_col = if line_idx == r.start_line {
                r.start_col
            } else {
                0
            };
            let end_col = if line_idx == r.end_line {
                r.end_col
            } else {
                line_width
            };
            if start_col < end_col {
                map.entry(line_idx).or_default().push((start_col, end_col));
            }
        }
    }
    // Sort each line's intervals
    for intervals in map.values_mut() {
        intervals.sort_unstable();
    }
    map
}

/// Split a host token around injection region intervals, keeping only the
/// fragments that fall outside all regions.
///
/// This handles the case where a host token (e.g., `@markup.raw.block` covering
/// the full fenced_code_block including `> ` prefix) partially overlaps with an
/// injection region. The overlapping part is removed; non-overlapping fragments
/// (like the `> ` prefix) survive.
///
/// `intervals` should be the precomputed sorted intervals for the token's line,
/// obtained from `build_region_intervals_map`.
fn split_host_token_around_regions(
    token: &RawToken,
    intervals: &[(usize, usize)],
) -> Vec<RawToken> {
    if intervals.is_empty() {
        return vec![token.clone()];
    }

    let token_start = token.column;
    let token_end = token.column + token.length;

    // Subtract region intervals from the token's range
    let mut fragments = Vec::new();
    let mut cursor = token_start;

    for (region_start, region_end) in intervals {
        if cursor >= token_end {
            break;
        }
        // Fragment before this region
        if cursor < *region_start {
            let frag_end = (*region_start).min(token_end);
            if cursor < frag_end {
                fragments.push(RawToken {
                    line: token.line,
                    column: cursor,
                    length: frag_end - cursor,
                    mapped_name: token.mapped_name.clone(),
                    depth: token.depth,
                    pattern_index: token.pattern_index,
                    priority: token.priority,
                    node_byte_len: token.node_byte_len,
                });
            }
        }
        cursor = (*region_end).max(cursor);
    }

    // Fragment after all regions
    if cursor < token_end {
        fragments.push(RawToken {
            line: token.line,
            column: cursor,
            length: token_end - cursor,
            mapped_name: token.mapped_name.clone(),
            depth: token.depth,
            pattern_index: token.pattern_index,
            priority: token.priority,
            node_byte_len: token.node_byte_len,
        });
    }

    fragments
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

    // Injection region exclusion: remove or split host tokens (depth=0) that
    // overlap with active injection regions.
    //
    // Host tokens fully inside a region are removed entirely — UNLESS the
    // token exactly matches a region's bounds. Exact matches occur when a
    // host capture and injection content share the same tree-sitter node
    // (e.g., fish `(comment) @comment` + `(comment) @injection.content`).
    // In that case the sweep-line algorithm resolves the overlap, preserving
    // the host token in gaps not covered by injection tokens.
    //
    // Host tokens that partially overlap (e.g., a `@markup.raw.block` token
    // covering both `> ` prefix and code content in a blockquote) are split,
    // keeping only the fragments outside the injection regions.
    if !active_injection_regions.is_empty() {
        let region_map = build_region_intervals_map(active_injection_regions, lines);
        let mut filtered = Vec::with_capacity(all_tokens.len());
        for token in all_tokens {
            if token.depth > 0 {
                // Non-host tokens (injection tokens) are always kept
                filtered.push(token);
                continue;
            }
            // Exact-match exception: when a host token's bounds match a region
            // exactly, keep it so the sweep-line can split it around injection
            // tokens. This mirrors the is_in_exclusion_range() exact-match
            // exception in token_collector.rs.
            if is_exact_match_active_injection_region(&token, active_injection_regions) {
                filtered.push(token);
                continue;
            }
            if is_fully_in_active_injection_region(&token, active_injection_regions) {
                // Fully contained (strictly) → remove entirely (fast path)
                continue;
            }
            // Check for partial overlap and split if needed.
            let intervals = region_map
                .get(&token.line)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let fragments = split_host_token_around_regions(&token, intervals);
            filtered.extend(fragments);
        }
        all_tokens = filtered;
    }

    // @none pre-processing: split parent tokens around @none regions.
    //
    // @none is a nvim-treesitter convention that resets parent highlighting
    // within a region (e.g., `(interpolation) @none` punches holes in @string
    // for f-string interpolation). We handle this by:
    // 1. Extracting @none token positions
    // 2. Splitting tokens whose node strictly contains the @none node
    //    (parent tokens with larger node_byte_len)
    // 3. Discarding the @none tokens themselves
    //
    // This preserves child tokens (e.g., @number inside interpolation) that
    // have smaller node_byte_len than @none, while removing the parent @string
    // coverage from the @none region.
    let (none_tokens, mut all_tokens): (Vec<_>, Vec<_>) = all_tokens
        .into_iter()
        .partition(|t| t.mapped_name == "none");

    if !none_tokens.is_empty() {
        // Build per-line @none intervals with their node sizes
        let mut none_intervals: HashMap<usize, Vec<(usize, usize, usize)>> = HashMap::new();
        for t in &none_tokens {
            none_intervals.entry(t.line).or_default().push((
                t.column,
                t.column + t.length,
                t.node_byte_len,
            ));
        }
        for intervals in none_intervals.values_mut() {
            intervals.sort_unstable();
        }

        // Split parent tokens around @none intervals
        let mut result = Vec::with_capacity(all_tokens.len());
        for token in all_tokens {
            if let Some(line_intervals) = none_intervals.get(&token.line) {
                // Find @none intervals that are within this token AND have
                // smaller node_byte_len (meaning @none is on a child node)
                let dominated: Vec<(usize, usize)> = line_intervals
                    .iter()
                    .filter(|(start, end, none_len)| {
                        *none_len < token.node_byte_len
                            && *start >= token.column
                            && *end <= token.column + token.length
                    })
                    .map(|(start, end, _)| (*start, *end))
                    .collect();

                if !dominated.is_empty() {
                    let fragments = split_host_token_around_regions(&token, &dominated);
                    result.extend(fragments);
                } else {
                    result.push(token);
                }
            } else {
                result.push(token);
            }
        }
        all_tokens = result;
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
        // Transparent tokens (empty mapped_name from apply_capture_mapping) may reach here
        // after sweep-line processing; they are filtered when map_capture_to_token_type_and_modifiers
        // returns None for the empty string.
        let Some((token_type, token_modifiers_bitset)) =
            map_capture_to_token_type_and_modifiers(&token.mapped_name)
        else {
            log::warn!(
                target: "kakehashi::semantic",
                "Skipping token with unknown type '{}' at line {} col {}",
                token.mapped_name, token.line, token.column
            );
            continue;
        };

        let delta_line = token.line.saturating_sub(last_line);
        let delta_start = if delta_line == 0 {
            token.column.saturating_sub(last_start)
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

    /// Helper to create a RawToken for testing (priority defaults to 100, node_byte_len to 0)
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
            priority: 100,
            node_byte_len: 0,
        }
    }

    /// Helper to create a RawToken with explicit priority
    fn make_token_with_priority(
        line: usize,
        column: usize,
        length: usize,
        name: &str,
        depth: usize,
        pattern_index: usize,
        priority: u32,
    ) -> RawToken {
        RawToken {
            line,
            column,
            length,
            mapped_name: name.to_string(),
            depth,
            pattern_index,
            priority,
            node_byte_len: 0,
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
    fn split_priority_overrides_pattern_index() {
        // Token A: "markup.raw.block" at (0,0,10) with depth=0, pattern_index=10, priority=90
        // Token B: "keyword" at (0,0,10) with depth=0, pattern_index=5, priority=100
        // priority 100 > 90 should win, despite B having lower pattern_index
        let tokens = vec![
            make_token_with_priority(0, 0, 10, "markup.raw.block", 0, 10, 90),
            make_token_with_priority(0, 0, 10, "keyword", 0, 5, 100),
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![(0, 10, "keyword".to_string())],
            "priority 100 should beat priority 90, regardless of pattern_index"
        );
    }

    // ── region_intervals_on_line tests ─────────────────────────────

    #[test]
    fn region_intervals_no_overlap_returns_empty() {
        // Token on line 5, region on lines 0-2 → no overlap
        let regions = vec![InjectionRegion {
            start_line: 0,
            start_col: 0,
            end_line: 2,
            end_col: 10,
        }];
        let result = region_intervals_on_line(5, 80, &regions);
        assert!(result.is_empty());
    }

    #[test]
    fn region_intervals_single_line_region() {
        // Region entirely on line 3: cols 5..15
        let regions = vec![InjectionRegion {
            start_line: 3,
            start_col: 5,
            end_line: 3,
            end_col: 15,
        }];
        let result = region_intervals_on_line(3, 80, &regions);
        assert_eq!(result, vec![(5, 15)]);
    }

    #[test]
    fn region_intervals_multiline_start_line() {
        // Region on lines 3..5, querying line 3 → start_col..line_width
        let regions = vec![InjectionRegion {
            start_line: 3,
            start_col: 10,
            end_line: 5,
            end_col: 20,
        }];
        let result = region_intervals_on_line(3, 80, &regions);
        assert_eq!(result, vec![(10, 80)]);
    }

    #[test]
    fn region_intervals_multiline_middle_line() {
        // Region on lines 3..5, querying line 4 → 0..line_width
        let regions = vec![InjectionRegion {
            start_line: 3,
            start_col: 10,
            end_line: 5,
            end_col: 20,
        }];
        let result = region_intervals_on_line(4, 80, &regions);
        assert_eq!(result, vec![(0, 80)]);
    }

    #[test]
    fn region_intervals_multiline_end_line() {
        // Region on lines 3..5, querying line 5 → 0..end_col
        let regions = vec![InjectionRegion {
            start_line: 3,
            start_col: 10,
            end_line: 5,
            end_col: 20,
        }];
        let result = region_intervals_on_line(5, 80, &regions);
        assert_eq!(result, vec![(0, 20)]);
    }

    #[test]
    fn region_intervals_multiple_regions_sorted() {
        // Two regions on line 1: cols 5..10 and cols 2..4 → sorted by start
        let regions = vec![
            InjectionRegion {
                start_line: 1,
                start_col: 5,
                end_line: 1,
                end_col: 10,
            },
            InjectionRegion {
                start_line: 1,
                start_col: 2,
                end_line: 1,
                end_col: 4,
            },
        ];
        let result = region_intervals_on_line(1, 80, &regions);
        assert_eq!(result, vec![(2, 4), (5, 10)]);
    }

    // ── split_host_token_around_regions tests ────────────────────

    #[test]
    fn split_around_no_overlap_token_survives() {
        // Token at cols 0..5, region at cols 10..20 → no overlap on token range
        let token = make_token(1, 0, 5, "string", 0, 0);
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 10,
            end_line: 1,
            end_col: 20,
        }];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 1);
        assert_eq!((frags[0].column, frags[0].length), (0, 5));
    }

    #[test]
    fn split_around_partial_overlap_prefix_survives() {
        // Token at cols 0..15, region at cols 5..15 → prefix [0,5) survives
        let token = make_token(1, 0, 15, "string", 0, 0);
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 15,
        }];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 1);
        assert_eq!((frags[0].column, frags[0].length), (0, 5));
    }

    #[test]
    fn split_around_partial_overlap_suffix_survives() {
        // Token at cols 5..20, region at cols 5..15 → suffix [15,20) survives
        let token = make_token(1, 5, 15, "string", 0, 0);
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 15,
        }];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 1);
        assert_eq!((frags[0].column, frags[0].length), (15, 5));
    }

    #[test]
    fn split_around_region_in_middle_two_fragments() {
        // Token at cols 0..20, region at cols 5..15 → [0,5) and [15,20)
        let token = make_token(1, 0, 20, "string", 0, 0);
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 15,
        }];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 2);
        assert_eq!((frags[0].column, frags[0].length), (0, 5));
        assert_eq!((frags[1].column, frags[1].length), (15, 5));
    }

    #[test]
    fn split_around_multiple_regions_three_fragments() {
        // Token at cols 0..30, regions at cols 5..10 and 15..20
        // → [0,5), [10,15), [20,30)
        let token = make_token(1, 0, 30, "string", 0, 0);
        let regions = vec![
            InjectionRegion {
                start_line: 1,
                start_col: 5,
                end_line: 1,
                end_col: 10,
            },
            InjectionRegion {
                start_line: 1,
                start_col: 15,
                end_line: 1,
                end_col: 20,
            },
        ];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 3);
        assert_eq!((frags[0].column, frags[0].length), (0, 5));
        assert_eq!((frags[1].column, frags[1].length), (10, 5));
        assert_eq!((frags[2].column, frags[2].length), (20, 10));
    }

    #[test]
    fn split_around_fully_covered_returns_empty() {
        // Token at cols 5..10, region at cols 0..20 → fully covered
        let token = make_token(1, 5, 5, "string", 0, 0);
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 0,
            end_line: 1,
            end_col: 20,
        }];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert!(frags.is_empty());
    }

    #[test]
    fn split_around_preserves_token_metadata() {
        // Verify fragments retain mapped_name, depth, pattern_index, priority
        let token = make_token(1, 0, 20, "markup.raw.block", 0, 5);
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 15,
        }];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 2);
        for frag in &frags {
            assert_eq!(frag.mapped_name, "markup.raw.block");
            assert_eq!(frag.depth, 0);
            assert_eq!(frag.pattern_index, 5);
            assert_eq!(frag.priority, 100);
        }
    }

    #[test]
    fn region_intervals_zero_width_region_filtered_out() {
        // A zero-width region (start_col == end_col) should produce no interval
        let regions = vec![InjectionRegion {
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 5,
        }];
        let result = region_intervals_on_line(1, 80, &regions);
        assert!(result.is_empty());
    }

    #[test]
    fn region_intervals_overlapping_regions_both_emitted() {
        // Two overlapping regions on the same line: (2,8) and (5,12)
        // Both are emitted sorted; the consumer's cursor handles overlap.
        let regions = vec![
            InjectionRegion {
                start_line: 1,
                start_col: 2,
                end_line: 1,
                end_col: 8,
            },
            InjectionRegion {
                start_line: 1,
                start_col: 5,
                end_line: 1,
                end_col: 12,
            },
        ];
        let result = region_intervals_on_line(1, 80, &regions);
        assert_eq!(result, vec![(2, 8), (5, 12)]);
    }

    #[test]
    fn split_around_overlapping_regions_merged_by_cursor() {
        // Token at cols 0..20, overlapping regions (2,8) and (5,12)
        // Cursor advancement merges overlaps: → [0,2) and [12,20)
        let token = make_token(1, 0, 20, "string", 0, 0);
        let regions = vec![
            InjectionRegion {
                start_line: 1,
                start_col: 2,
                end_line: 1,
                end_col: 8,
            },
            InjectionRegion {
                start_line: 1,
                start_col: 5,
                end_line: 1,
                end_col: 12,
            },
        ];
        let intervals = region_intervals_on_line(1, 80, &regions);
        let frags = split_host_token_around_regions(&token, &intervals);
        assert_eq!(frags.len(), 2);
        assert_eq!((frags[0].column, frags[0].length), (0, 2));
        assert_eq!((frags[1].column, frags[1].length), (12, 8));
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

    #[test]
    fn none_splits_parent_string_and_preserves_child_number() {
        // @string (node_byte_len=40) covers [0, 20)
        // @none (node_byte_len=10) covers [5, 15) — punches a hole in @string
        // @number (node_byte_len=1) covers [6, 7) — inside the hole
        // Result: string[0,5), number[6,7), string[15,20)
        // The @none intervals [5,6) and [7,15) should be empty (no token)
        let tokens = vec![
            RawToken {
                line: 0,
                column: 0,
                length: 20,
                mapped_name: "string".to_string(),
                depth: 0,
                pattern_index: 25,
                priority: 100,
                node_byte_len: 40,
            },
            RawToken {
                line: 0,
                column: 5,
                length: 10,
                mapped_name: "none".to_string(),
                depth: 0,
                pattern_index: 1,
                priority: 100,
                node_byte_len: 10,
            },
            RawToken {
                line: 0,
                column: 6,
                length: 1,
                mapped_name: "number".to_string(),
                depth: 0,
                pattern_index: 20,
                priority: 100,
                node_byte_len: 1,
            },
        ];
        let result = finalize_tokens(tokens, &[], &["12345678901234567890"]);
        let SemanticTokensResult::Tokens(st) = result.expect("should produce tokens") else {
            panic!("Expected Tokens variant");
        };
        let (string_type, _) = map_capture_to_token_type_and_modifiers("string").unwrap();
        let (number_type, _) = map_capture_to_token_type_and_modifiers("number").unwrap();
        let types: Vec<(u32, u32, u32)> = st
            .data
            .iter()
            .map(|t| (t.delta_start, t.length, t.token_type))
            .collect();
        // Delta encoding: all on line 0, so delta_start = col - prev_col
        // string at col 0: delta_start=0
        // number at col 6: delta_start=6-0=6
        // string at col 15: delta_start=15-6=9
        assert_eq!(
            types,
            vec![
                (0, 5, string_type), // string[0,5)
                (6, 1, number_type), // number[6,7)
                (9, 5, string_type), // string[15,20)
            ]
        );
    }

    #[test]
    fn split_transparent_token_creates_breakpoint_only() {
        // A transparent token (empty mapped_name) creates breakpoints but does
        // not compete for winning — the best visible token wins each interval.
        //
        // Simulates: block_continuation `> ` at col 0-2 (transparent, p=100, nbl=3)
        //            fenced_code_block at col 0-10 (visible "string", p=90, nbl=50)
        //            block_quote at col 0-10 (visible "keyword", p=90, nbl=200)
        //
        // Expected: fenced_code_block wins both intervals (smaller nbl at same priority)
        //           → merged into single [0,10) "string" token
        let tokens = vec![
            RawToken {
                line: 0,
                column: 0,
                length: 2,
                mapped_name: String::new(), // transparent
                depth: 0,
                pattern_index: 50,
                priority: 100,
                node_byte_len: 3,
            },
            RawToken {
                line: 0,
                column: 0,
                length: 10,
                mapped_name: "string".to_string(), // fenced_code_block
                depth: 0,
                pattern_index: 47,
                priority: 90,
                node_byte_len: 50,
            },
            RawToken {
                line: 0,
                column: 0,
                length: 10,
                mapped_name: "keyword".to_string(), // block_quote
                depth: 0,
                pattern_index: 107,
                priority: 90,
                node_byte_len: 200,
            },
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![(0, 10, "string".to_string())],
            "Transparent tokens are breakpoint-only; best visible token wins"
        );
    }

    #[test]
    fn split_transparent_only_interval_produces_no_token() {
        // When ALL tokens covering an interval are transparent,
        // no token is emitted for that interval.
        let tokens = vec![
            RawToken {
                line: 0,
                column: 0,
                length: 5,
                mapped_name: String::new(), // transparent
                depth: 0,
                pattern_index: 10,
                priority: 100,
                node_byte_len: 5,
            },
            RawToken {
                line: 0,
                column: 5,
                length: 5,
                mapped_name: "keyword".to_string(), // visible
                depth: 0,
                pattern_index: 10,
                priority: 100,
                node_byte_len: 5,
            },
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![(5, 5, "keyword".to_string())],
            "Transparent-only interval [0,5) should produce no token"
        );
    }

    #[test]
    fn split_smaller_node_byte_len_wins_at_same_priority_and_depth() {
        // Two overlapping tokens at same priority (90) and depth (0), but different node_byte_len.
        // The smaller node (fenced_code_block, nbl=50) should win over the larger node
        // (block_quote, nbl=200) because it's more specific.
        let tokens = vec![
            RawToken {
                line: 0,
                column: 0,
                length: 10,
                mapped_name: "keyword".to_string(), // block_quote → markup.quote
                depth: 0,
                pattern_index: 107, // later in query file
                priority: 90,
                node_byte_len: 200, // larger node
            },
            RawToken {
                line: 0,
                column: 0,
                length: 10,
                mapped_name: "string".to_string(), // fenced_code_block → markup.raw.block
                depth: 0,
                pattern_index: 47, // earlier in query file
                priority: 90,
                node_byte_len: 50, // smaller, more specific node
            },
        ];
        let fragments = extract_fragments(tokens);
        assert_eq!(
            fragments,
            vec![(0, 10, "string".to_string())],
            "Smaller node_byte_len (more specific) should win at same priority/depth"
        );
    }

    #[test]
    fn finalize_preserves_host_token_exactly_matching_active_injection_region() {
        // Reproduces the fish comment bug: a host `comment` token (depth=0)
        // exactly matches an active injection region because `(comment) @comment`
        // and `(comment) @injection.content` capture the same node.
        //
        // The comment grammar produces `number` tokens for `#123` but leaves
        // the rest of the comment uncovered. The sweep-line should split the
        // host comment around the injection numbers.
        //
        // Input:  "# comment with #123 text"
        //          0         1         2
        //          0123456789012345678901234
        //
        // Host:   comment [0, 24) depth=0
        // Inj:    number  [15, 19) depth=1  (#123)
        let line_text = "# comment with #123 text";
        let lines: Vec<&str> = vec![line_text];
        let tokens = vec![
            make_token(0, 0, 24, "comment", 0, 0), // host comment (full line)
            make_token(0, 15, 4, "number", 1, 0),  // injection number (#123)
        ];
        // The injection region exactly matches the host comment token
        let regions = vec![InjectionRegion {
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 24,
        }];

        let result = finalize_tokens(tokens, &regions, &lines);
        assert!(result.is_some(), "Should produce tokens");

        let SemanticTokensResult::Tokens(st) = result.unwrap() else {
            panic!("Expected Tokens");
        };

        // Expect 3 tokens: comment[0,15) + number[15,19) + comment[19,24)
        let positions: Vec<(u32, u32, u32)> = st
            .data
            .iter()
            .map(|t| (t.delta_start, t.length, t.token_type))
            .collect();
        let (comment_type, _) = map_capture_to_token_type_and_modifiers("comment").unwrap();
        let (number_type, _) = map_capture_to_token_type_and_modifiers("number").unwrap();

        // delta_start is relative to previous token on the same line
        assert_eq!(
            positions,
            vec![
                (0, 15, comment_type), // comment [0, 15)
                (15, 4, number_type),  // number  [15, 19)  delta=15-0=15
                (4, 5, comment_type),  // comment [19, 24)  delta=19-15=4
            ],
            "Host comment should be split around injection number by sweep-line"
        );
    }
}
