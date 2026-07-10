use crate::language::injection::InjectionOffset;
use crate::text::{ceil_char_boundary, floor_char_boundary};

/// Represents a byte range with start and end positions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: usize,
    pub end: usize,
}

impl ByteRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Represents an effective range after applying offset
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveRange {
    pub start: usize,
    pub end: usize,
}

impl EffectiveRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Calculates the effective range by applying both row and column offsets
///
/// This function clamps the resulting range to `[0, text.len()]`, snaps both
/// ends inward to char boundaries (column deltas are byte counts, so a
/// misconfigured query can land inside a multi-byte character), and ensures
/// `start <= end` to prevent panics when slicing. Malformed or malicious
/// query offsets cannot crash the server.
pub fn calculate_effective_range(
    text: &str,
    byte_range: ByteRange,
    offset: InjectionOffset,
) -> EffectiveRange {
    let text_len = text.len();

    // Calculate raw effective positions
    let (raw_start, raw_end) = if offset.start_row == 0 && offset.end_row == 0 {
        // Column offsets only - apply directly
        let start = apply_column_offset(byte_range.start, offset.start_column);
        let end = apply_column_offset(byte_range.end, offset.end_column);
        (start, end)
    } else {
        // Row offsets require text scanning
        let start = apply_offset_to_position(
            text,
            byte_range.start,
            offset.start_row,
            offset.start_column,
        );
        let end = apply_offset_to_position(text, byte_range.end, offset.end_row, offset.end_column);
        (start, end)
    };

    // Clamp to valid range [0, text.len()] and snap inward to char boundaries
    let snapped_start = ceil_char_boundary(text, raw_start.min(text_len));
    let snapped_end = floor_char_boundary(text, raw_end.min(text_len));

    // Ensure start <= end invariant
    let (final_start, final_end) = if snapped_start <= snapped_end {
        (snapped_start, snapped_end)
    } else {
        // When start > end, return empty range at snapped_end
        (snapped_end, snapped_end)
    };

    EffectiveRange::new(final_start, final_end)
}

/// Apply row and column offset to a byte position in text
///
/// Walks only the span between `byte_pos` and the target line, in whichever
/// direction `row_offset` points — never the whole document. `#offset!` rows
/// are small (typically ±1, e.g. skipping a fence/frontmatter delimiter), so
/// this is O(distance moved) rather than the O(document) two-full-scan
/// approach it replaced.
fn apply_offset_to_position(
    text: &str,
    byte_pos: usize,
    row_offset: i32,
    col_offset: i32,
) -> usize {
    if row_offset == 0 {
        // No row offset, just apply column offset
        return apply_column_offset(byte_pos, col_offset);
    }

    // Start of the line containing byte_pos, found by scanning backward from
    // byte_pos rather than forward from the document start. Snap to a char
    // boundary first: unlike the char_indices()-based scan this replaced,
    // slicing at a raw byte_pos panics if it lands mid-codepoint (e.g. a
    // stale tree-sitter offset) — the caller's later clamp/snap only applies
    // to the *returned* value, not this internal slice.
    let current_line_start = text[..floor_char_boundary(text, byte_pos)]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);

    let target_line_start = if row_offset > 0 {
        // Overshooting the last line stops at that line's start (matching the
        // original whole-scan behavior), not the document end.
        let mut pos = current_line_start;
        for _ in 0..row_offset {
            match text[pos..].find('\n') {
                Some(nl) => pos += nl + 1,
                None => break,
            }
        }
        pos
    } else {
        let mut pos = current_line_start;
        for _ in 0..row_offset.unsigned_abs() {
            if pos == 0 {
                break;
            }
            pos = text[..pos - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        pos
    };

    let original_column = byte_pos.saturating_sub(current_line_start);
    apply_column_offset(
        target_line_start.saturating_add(original_column),
        col_offset,
    )
}

fn apply_column_offset(base: usize, offset: i32) -> usize {
    let result = base as i128 + offset as i128;
    if result <= 0 {
        0
    } else {
        usize::try_from(result).unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::injection::InjectionOffset;
    use rstest::rstest;

    /// Parameterized test for offset clamping edge cases
    ///
    /// This test consolidates three duplicate clamping tests:
    /// - Column offset extending past EOF
    /// - Start offset past end position
    /// - Row offset extending beyond last line
    ///
    /// All tests verify the safety invariants:
    /// 1. effective.start <= effective.end (no reversed ranges)
    /// 2. effective.end <= text.len() (no out-of-bounds)
    /// 3. Slicing text[start..end] doesn't panic
    #[rstest]
    #[case::end_column_past_eof(
        "hello",
        ByteRange::new(0, 5),
        InjectionOffset { start_row: 0, start_column: 0, end_row: 0, end_column: 5 },
        "offsets extending past EOF don't cause panics - \
         a malformed query might specify #offset! @injection.content 0 0 0 1 \
         which extends the end past the buffer"
    )]
    #[case::start_past_end(
        "hello",
        ByteRange::new(0, 5),
        InjectionOffset { start_row: 0, start_column: 10, end_row: 0, end_column: 0 },
        "offset makes start > end - should normalize to empty range"
    )]
    #[case::end_row_past_eof(
        "line 1\nline 2",
        ByteRange::new(0, 6),
        InjectionOffset { start_row: 0, start_column: 0, end_row: 5, end_column: 0 },
        "row offset moving end beyond last line"
    )]
    #[case::start_lands_mid_multibyte_char(
        "aあいう",
        ByteRange::new(0, 10),
        InjectionOffset { start_row: 0, start_column: 2, end_row: 0, end_column: 0 },
        "column offsets are byte counts; +2 lands inside あ (bytes 1..4) \
         and must snap to a char boundary so slicing cannot panic"
    )]
    #[case::end_lands_mid_multibyte_char(
        "あいう",
        ByteRange::new(0, 9),
        InjectionOffset { start_row: 0, start_column: 0, end_row: 0, end_column: -1 },
        "column offsets are byte counts; -1 lands inside う (bytes 6..9) \
         and must snap to a char boundary so slicing cannot panic"
    )]
    fn test_offset_clamping_edge_cases(
        #[case] text: &str,
        #[case] byte_range: ByteRange,
        #[case] offset: InjectionOffset,
        #[case] description: &str,
    ) {
        let effective = calculate_effective_range(text, byte_range, offset);

        // Invariant 1: start <= end (no reversed ranges)
        assert!(
            effective.start <= effective.end,
            "{}: Start {} should be <= end {}",
            description,
            effective.start,
            effective.end
        );

        // Invariant 2: Both positions within bounds
        assert!(
            effective.start <= text.len() && effective.end <= text.len(),
            "{}: Both start {} and end {} should be <= text len {}",
            description,
            effective.start,
            effective.end,
            text.len()
        );

        // Invariant 3: Slicing should not panic
        let _ = &text[effective.start..effective.end];
    }

    #[test]
    fn test_calculate_effective_range_with_positive_offset() {
        // Use text long enough to accommodate the range
        let text = "0123456789012345678901234567890";
        let byte_range = ByteRange::new(10, 20);
        let offset = InjectionOffset {
            start_row: 0,
            start_column: 5,
            end_row: 0,
            end_column: -3,
        }; // Column offsets: +5 start, -3 end

        let effective = calculate_effective_range(text, byte_range, offset);

        assert_eq!(effective.start, 15);
        assert_eq!(effective.end, 17);
    }

    #[test]
    fn test_calculate_effective_range_with_default_offset() {
        let text = "0123456789012345678901234567890";
        let byte_range = ByteRange::new(10, 20);

        let effective = calculate_effective_range(text, byte_range, InjectionOffset::default());

        assert_eq!(effective.start, 10);
        assert_eq!(effective.end, 20);
    }

    #[test]
    fn test_calculate_effective_range_column_only() {
        let text = "line 1\nline 2\nline 3";
        let byte_range = ByteRange::new(7, 13); // "line 2"
        let offset = InjectionOffset {
            start_row: 0,
            start_column: 3,
            end_row: 0,
            end_column: -1,
        }; // Column offsets only

        let effective = calculate_effective_range(text, byte_range, offset);

        assert_eq!(effective.start, 10); // 7 + 3
        assert_eq!(effective.end, 12); // 13 - 1
    }

    #[test]
    fn test_calculate_effective_range_positive_row_offset() {
        let text = "line 1\nline 2\nline 3 with content\nline 4";
        // Node starts at byte 7 (start of "line 2")
        let byte_range = ByteRange::new(7, 13); // "line 2"
        let offset = InjectionOffset {
            start_row: 1,
            start_column: 0,
            end_row: 0,
            end_column: 0,
        }; // Move start down 1 row

        let effective = calculate_effective_range(text, byte_range, offset);

        // Original raw values: start=14, end=13 (start > end)
        // With safety clamping: when start > end, we normalize to empty range at end
        assert_eq!(effective.start, 13);
        assert_eq!(effective.end, 13);
        // Slicing should be safe
        let _ = &text[effective.start..effective.end];
    }

    #[test]
    fn test_apply_offset_to_position_positive_row_overshoot_clamps_to_last_line_start() {
        // A row_offset overshooting the last line must clamp to that last
        // line's start, not the document end — pins the exact overshoot
        // value against the whole-scan behavior this function replaced.
        let text = "line 1\nline 2";
        assert_eq!(apply_offset_to_position(text, 0, 5, 0), 7);
    }

    #[test]
    fn test_apply_offset_to_position_mid_codepoint_byte_pos_does_not_panic() {
        // A stale/malformed byte_pos landing inside a multi-byte character
        // must not panic the internal line-start slice — it should behave as
        // if snapped to the enclosing char boundary.
        let text = "line 1\nあ\nline 3";
        // Byte 8 is the second byte of "あ" (starts at byte 7, 3 bytes).
        let _ = apply_offset_to_position(text, 8, 1, 0);
        let _ = apply_offset_to_position(text, 8, -1, 0);
    }

    #[test]
    fn test_apply_offset_to_position_row_offset_preserves_original_column() {
        let text = "aaXX\nbbYY\n";

        assert_eq!(
            apply_offset_to_position(text, 2, 1, 1),
            8,
            "row offsets should apply column deltas relative to the original column"
        );
    }

    #[test]
    fn test_apply_column_offset_does_not_narrow_large_positions() {
        let base = i32::MAX as usize + 10;

        assert_eq!(apply_column_offset(base, 5), base + 5);
        assert_eq!(apply_column_offset(3, -10), 0);
        assert_eq!(apply_offset_to_position("", base, 0, 5), base + 5);
    }

    #[test]
    fn test_calculate_effective_range_negative_row_offset() {
        let text = "line 1\nline 2\nline 3";
        // Node starts at byte 14 (start of "line 3")
        let byte_range = ByteRange::new(14, 20);
        let offset = InjectionOffset {
            start_row: -1,
            start_column: 0,
            end_row: 0,
            end_column: 0,
        }; // Move start up 1 row

        let effective = calculate_effective_range(text, byte_range, offset);

        // Should move start to byte 7 (start of "line 2")
        assert_eq!(effective.start, 7);
        assert_eq!(effective.end, 20); // End unchanged
    }

    #[test]
    fn test_calculate_effective_range_row_and_column_offset() {
        let text = "line 1\nline 2\nline 3 with content";
        // Node starts at byte 7 (start of "line 2")
        let byte_range = ByteRange::new(7, 13);
        // Move start down 1 row and 5 columns right
        let offset = InjectionOffset {
            start_row: 1,
            start_column: 5,
            end_row: 0,
            end_column: 0,
        };

        let effective = calculate_effective_range(text, byte_range, offset);

        // Original raw values: start=19, end=13 (start > end)
        // With safety clamping: when start > end, we normalize to empty range at end
        assert_eq!(effective.start, 13);
        assert_eq!(effective.end, 13);
        // Slicing should be safe
        let _ = &text[effective.start..effective.end];
    }

    #[test]
    fn test_lua_doc_comment_offset() {
        // Real-world example: Lua doc comment
        // ---@param x number
        // The offset (0, 3, 0, 0) means start at column 3 of the same row
        let text = "---@param x number\nfunction foo(x) end";
        let byte_range = ByteRange::new(0, 18); // The doc comment
        let offset = InjectionOffset {
            start_row: 0,
            start_column: 3,
            end_row: 0,
            end_column: 0,
        }; // Skip "---"

        let effective = calculate_effective_range(text, byte_range, offset);

        assert_eq!(effective.start, 3); // Skip "---" prefix
        assert_eq!(effective.end, 18);
    }
}

#[test]
fn test_markdown_frontmatter_offset() {
    // Real-world example: Markdown YAML frontmatter
    // Offset (#offset! @injection.content 1 0 -1 0)
    // means skip first row (---) and last row (---)
    let text = "---\ntitle: \"awesome\"\narray: [\"xxxx\"]\n---\n";

    // The minus_metadata node spans the entire frontmatter including ---
    let byte_range = ByteRange::new(0, 41); // "---\ntitle...\n---\n"

    // Offset: start_row=1, start_col=0, end_row=-1, end_col=0
    let offset = InjectionOffset {
        start_row: 1,
        start_column: 0,
        end_row: -1,
        end_column: 0,
    };

    let effective = calculate_effective_range(text, byte_range, offset);

    // Start should be at byte 4 (after "---\n")
    assert_eq!(effective.start, 4, "Start should skip the first line");

    // End should be at byte 37 (before "---\n")
    // Line positions:
    // byte 0: "---\n" (4 bytes)
    // byte 4: "title: \"awesome\"\n" (18 bytes) = ends at 21
    // byte 21+1: "array: [\"xxxx\"]\n" (16 bytes) = ends at 37
    // byte 37: "---\n" (4 bytes) = ends at 41
    assert_eq!(effective.end, 37, "End should skip the last line");

    // Verify the content matches expected
    let effective_text = &text[effective.start..effective.end];
    assert_eq!(effective_text, "title: \"awesome\"\narray: [\"xxxx\"]\n");
}
