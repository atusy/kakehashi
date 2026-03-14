use crate::language::injection::InjectionOffset;

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
/// This function clamps the resulting range to `[0, text.len()]` and ensures
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
        let start = (byte_range.start as i32 + offset.start_column).max(0) as usize;
        let end = (byte_range.end as i32 + offset.end_column).max(0) as usize;
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

    // Clamp to valid range [0, text.len()]
    let clamped_start = raw_start.min(text_len);
    let clamped_end = raw_end.min(text_len);

    // Ensure start <= end invariant
    let (final_start, final_end) = if clamped_start <= clamped_end {
        (clamped_start, clamped_end)
    } else {
        // When start > end, return empty range at clamped_end
        (clamped_end, clamped_end)
    };

    EffectiveRange::new(final_start, final_end)
}

/// Apply row and column offset to a byte position in text
fn apply_offset_to_position(
    text: &str,
    byte_pos: usize,
    row_offset: i32,
    col_offset: i32,
) -> usize {
    if row_offset == 0 {
        // No row offset, just apply column offset
        return (byte_pos as i32 + col_offset).max(0) as usize;
    }

    // Find the line containing the byte position
    let mut current_line = 0;
    let mut line_start_byte = 0;

    for (i, ch) in text.char_indices() {
        if i >= byte_pos {
            // Found the line containing our position
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start_byte = i + 1;
        }
    }

    // Now find the target line (current_line + row_offset)
    let target_line = (current_line as i32 + row_offset).max(0) as usize;

    if row_offset > 0 {
        // Moving forward - find the target line
        let mut lines_seen = current_line;
        let mut target_line_start = line_start_byte;

        for (i, ch) in text[line_start_byte..].char_indices() {
            if lines_seen == target_line {
                break;
            }
            if ch == '\n' {
                lines_seen += 1;
                target_line_start = line_start_byte + i + 1;
            }
        }

        // Apply column offset from the start of the target line
        (target_line_start as i32 + col_offset).max(0) as usize
    } else {
        // Moving backward - count lines from the beginning
        let mut lines_seen = 0;
        let mut target_line_start = 0;

        for (i, ch) in text.char_indices() {
            if lines_seen == target_line {
                target_line_start = i;
                break;
            }
            if ch == '\n' {
                lines_seen += 1;
            }
        }

        // Apply column offset from the start of the target line
        (target_line_start as i32 + col_offset).max(0) as usize
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
