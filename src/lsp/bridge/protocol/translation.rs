//! Shared position/range translation helpers for host <-> virtual coordinate conversion.
//!
//! All translation functions use in-place `&mut` mutation and saturating arithmetic
//! for race-condition safety (stale region data after document edits).

use tower_lsp_server::ls_types::{Position, Range};

/// The starting offset of an injection region in the host document.
///
/// Bundles the line and column offset that are always passed together
/// through the bridge request/response pipeline for coordinate translation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegionOffset {
    /// The starting line of the injection region in the host document.
    pub line: u32,
    /// The starting column (UTF-16 code units) on the first host line.
    /// Only applied to virtual line 0.
    pub column: u32,
}

impl RegionOffset {
    /// Construct a `RegionOffset` from line and column values.
    pub(crate) fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

// =============================================================================
// Virtual -> Host (response direction)
// =============================================================================

/// Translate a single virtual position to host coordinates.
///
/// On virtual line 0, adds column offset to character.
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_virtual_position_to_host(pos: &mut Position, offset: RegionOffset) {
    let was_first_line = pos.line == 0;
    pos.line = pos.line.saturating_add(offset.line);
    if was_first_line {
        pos.character = pos.character.saturating_add(offset.column);
    }
}

/// Translate a virtual range to host coordinates.
///
/// Applies position translation to both start and end.
pub(crate) fn translate_virtual_range_to_host(range: &mut Range, offset: RegionOffset) {
    translate_virtual_position_to_host(&mut range.start, offset);
    translate_virtual_position_to_host(&mut range.end, offset);
}

// =============================================================================
// Host -> Virtual (request direction)
// =============================================================================

/// Translate a single host position to virtual coordinates.
///
/// When the position is genuinely on the first line of the region (virtual line 0),
/// subtracts column offset from character. When the line underflows to 0 due to
/// stale region data (race condition), the column offset is NOT applied to avoid
/// compounding the already-invalid result.
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_host_position_to_virtual(pos: &mut Position, offset: RegionOffset) {
    let underflowed = pos.line < offset.line;
    pos.line = pos.line.saturating_sub(offset.line);
    if pos.line == 0 && !underflowed {
        pos.character = pos.character.saturating_sub(offset.column);
    }
}

/// Translate a host range to virtual coordinates.
///
/// Applies position translation to both start and end **independently**.
/// This means asymmetric column treatment is possible: if the start line
/// underflows (stale region data) but the end line does not, only the start
/// will skip column adjustment. This is intentional — each endpoint should
/// degrade independently rather than coupling their error behavior.
pub(crate) fn translate_host_range_to_virtual(range: &mut Range, offset: RegionOffset) {
    translate_host_position_to_virtual(&mut range.start, offset);
    translate_host_position_to_virtual(&mut range.end, offset);
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
        translate_virtual_position_to_host(&mut pos, RegionOffset::new(10, 4));
        assert_eq!(pos.line, 10);
        assert_eq!(pos.character, 9); // 5 + 4
    }

    #[test]
    fn position_to_host_non_first_line_ignores_column_offset() {
        let mut pos = Position {
            line: 2,
            character: 5,
        };
        translate_virtual_position_to_host(&mut pos, RegionOffset::new(10, 4));
        assert_eq!(pos.line, 12);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_host_column_offset_saturates_on_overflow() {
        let mut pos = Position {
            line: 0,
            character: u32::MAX,
        };
        translate_virtual_position_to_host(&mut pos, RegionOffset::new(10, 4));
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
        translate_virtual_range_to_host(&mut range, RegionOffset::new(5, 3));
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
        translate_virtual_range_to_host(&mut range, RegionOffset::new(5, 3));
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
        translate_virtual_range_to_host(&mut range, RegionOffset::new(5, 3));
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
        translate_host_position_to_virtual(&mut pos, RegionOffset::new(10, 4));
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
        translate_host_position_to_virtual(&mut pos, RegionOffset::new(10, 4));
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_virtual_line_saturates_on_underflow() {
        let mut pos = Position {
            line: 5,
            character: 8,
        };
        translate_host_position_to_virtual(&mut pos, RegionOffset::new(10, 4));
        assert_eq!(pos.line, 0);
        // Line underflowed (stale data), so column offset is NOT applied
        assert_eq!(pos.character, 8);
    }

    #[test]
    fn position_to_virtual_column_saturates_on_underflow() {
        let mut pos = Position {
            line: 10,
            character: 2,
        };
        translate_host_position_to_virtual(&mut pos, RegionOffset::new(10, 10));
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
        translate_host_range_to_virtual(&mut range, RegionOffset::new(5, 3));
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
        translate_host_range_to_virtual(&mut range, RegionOffset::new(5, 3));
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
        translate_host_range_to_virtual(&mut range, RegionOffset::new(5, 3));
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 2); // unchanged
        assert_eq!(range.end.line, 3);
        assert_eq!(range.end.character, 8); // unchanged
    }
}
