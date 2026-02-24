//! Shared position/range translation helpers for host <-> virtual coordinate conversion.
//!
//! All translation functions use in-place `&mut` mutation and saturating arithmetic
//! for race-condition safety (stale region data after document edits).

use tower_lsp_server::ls_types::{Position, Range};

// =============================================================================
// Virtual -> Host (response direction)
// =============================================================================

/// Translate a single virtual position to host coordinates.
///
/// On virtual line 0, adds column offset to character.
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_virtual_position_to_host(
    pos: &mut Position,
    region_start_line: u32,
    region_start_column: u32,
) {
    let was_first_line = pos.line == 0;
    pos.line = pos.line.saturating_add(region_start_line);
    if was_first_line {
        pos.character = pos.character.saturating_add(region_start_column);
    }
}

/// Translate a virtual range to host coordinates.
///
/// Applies position translation to both start and end.
pub(crate) fn translate_virtual_range_to_host(
    range: &mut Range,
    region_start_line: u32,
    region_start_column: u32,
) {
    translate_virtual_position_to_host(&mut range.start, region_start_line, region_start_column);
    translate_virtual_position_to_host(&mut range.end, region_start_line, region_start_column);
}

// =============================================================================
// Host -> Virtual (request direction)
// =============================================================================

/// Translate a single host position to virtual coordinates.
///
/// When the resulting virtual line is 0, subtracts column offset from character.
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_host_position_to_virtual(
    pos: &mut Position,
    region_start_line: u32,
    region_start_column: u32,
) {
    pos.line = pos.line.saturating_sub(region_start_line);
    if pos.line == 0 {
        pos.character = pos.character.saturating_sub(region_start_column);
    }
}

/// Translate a host range to virtual coordinates.
///
/// Applies position translation to both start and end.
pub(crate) fn translate_host_range_to_virtual(
    range: &mut Range,
    region_start_line: u32,
    region_start_column: u32,
) {
    translate_host_position_to_virtual(&mut range.start, region_start_line, region_start_column);
    translate_host_position_to_virtual(&mut range.end, region_start_line, region_start_column);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ======================================================================
    // translate_virtual_position_to_host
    // ======================================================================

    #[test]
    fn position_to_host_first_line_adds_column_offset() {
        let mut pos = Position {
            line: 0,
            character: 5,
        };
        translate_virtual_position_to_host(&mut pos, 10, 4);
        assert_eq!(pos.line, 10);
        assert_eq!(pos.character, 9); // 5 + 4
    }

    #[test]
    fn position_to_host_non_first_line_ignores_column_offset() {
        let mut pos = Position {
            line: 2,
            character: 5,
        };
        translate_virtual_position_to_host(&mut pos, 10, 4);
        assert_eq!(pos.line, 12);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_host_column_offset_saturates_on_overflow() {
        let mut pos = Position {
            line: 0,
            character: u32::MAX,
        };
        translate_virtual_position_to_host(&mut pos, 10, 4);
        assert_eq!(pos.character, u32::MAX);
    }

    // ======================================================================
    // translate_virtual_range_to_host
    // ======================================================================

    #[test]
    fn range_to_host_first_line_range_adds_column_offset() {
        let mut range = Range {
            start: Position {
                line: 0,
                character: 2,
            },
            end: Position {
                line: 0,
                character: 8,
            },
        };
        translate_virtual_range_to_host(&mut range, 5, 3);
        assert_eq!(range.start.line, 5);
        assert_eq!(range.start.character, 5); // 2 + 3
        assert_eq!(range.end.line, 5);
        assert_eq!(range.end.character, 11); // 8 + 3
    }

    #[test]
    fn range_to_host_spanning_lines_only_adjusts_first_line_column() {
        let mut range = Range {
            start: Position {
                line: 0,
                character: 2,
            },
            end: Position {
                line: 1,
                character: 8,
            },
        };
        translate_virtual_range_to_host(&mut range, 5, 3);
        assert_eq!(range.start.line, 5);
        assert_eq!(range.start.character, 5); // 2 + 3
        assert_eq!(range.end.line, 6);
        assert_eq!(range.end.character, 8); // unchanged
    }

    #[test]
    fn range_to_host_non_first_line_range_ignores_column_offset() {
        let mut range = Range {
            start: Position {
                line: 1,
                character: 2,
            },
            end: Position {
                line: 3,
                character: 8,
            },
        };
        translate_virtual_range_to_host(&mut range, 5, 3);
        assert_eq!(range.start.line, 6);
        assert_eq!(range.start.character, 2); // unchanged
        assert_eq!(range.end.line, 8);
        assert_eq!(range.end.character, 8); // unchanged
    }

    // ======================================================================
    // translate_host_position_to_virtual
    // ======================================================================

    #[test]
    fn position_to_virtual_first_line_subtracts_column_offset() {
        // Host line 10, char 9; region starts at line 10, col 4
        // -> virtual line 0, char 5 (9 - 4)
        let mut pos = Position {
            line: 10,
            character: 9,
        };
        translate_host_position_to_virtual(&mut pos, 10, 4);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 5); // 9 - 4
    }

    #[test]
    fn position_to_virtual_non_first_line_ignores_column_offset() {
        // Host line 12, char 5; region starts at line 10, col 4
        // -> virtual line 2, char 5 (unchanged)
        let mut pos = Position {
            line: 12,
            character: 5,
        };
        translate_host_position_to_virtual(&mut pos, 10, 4);
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_virtual_line_saturates_on_underflow() {
        let mut pos = Position {
            line: 5,
            character: 8,
        };
        translate_host_position_to_virtual(&mut pos, 10, 4);
        assert_eq!(pos.line, 0);
        // Line saturated to 0, so column offset is subtracted
        assert_eq!(pos.character, 4); // 8 - 4
    }

    #[test]
    fn position_to_virtual_column_saturates_on_underflow() {
        let mut pos = Position {
            line: 10,
            character: 2,
        };
        translate_host_position_to_virtual(&mut pos, 10, 10);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 0); // saturated
    }

    // ======================================================================
    // translate_host_range_to_virtual
    // ======================================================================

    #[test]
    fn range_to_virtual_first_line_range_subtracts_column_offset() {
        let mut range = Range {
            start: Position {
                line: 5,
                character: 7,
            },
            end: Position {
                line: 5,
                character: 13,
            },
        };
        translate_host_range_to_virtual(&mut range, 5, 3);
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 4); // 7 - 3
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 10); // 13 - 3
    }

    #[test]
    fn range_to_virtual_spanning_lines_only_adjusts_first_line_column() {
        let mut range = Range {
            start: Position {
                line: 5,
                character: 7,
            },
            end: Position {
                line: 6,
                character: 8,
            },
        };
        translate_host_range_to_virtual(&mut range, 5, 3);
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 4); // 7 - 3
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 8); // unchanged
    }

    #[test]
    fn range_to_virtual_non_first_line_range_ignores_column_offset() {
        let mut range = Range {
            start: Position {
                line: 6,
                character: 2,
            },
            end: Position {
                line: 8,
                character: 8,
            },
        };
        translate_host_range_to_virtual(&mut range, 5, 3);
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 2); // unchanged
        assert_eq!(range.end.line, 3);
        assert_eq!(range.end.character, 8); // unchanged
    }
}
