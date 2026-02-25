//! Shared position/range translation helpers for host <-> virtual coordinate conversion.
//!
//! All translation functions use in-place `&mut` mutation and saturating arithmetic
//! for race-condition safety (stale region data after document edits).

use tower_lsp_server::ls_types::{Position, Range};

/// The starting offset of an injection region in the host document.
///
/// Bundles the line offset and per-virtual-line column offsets that are always
/// passed together through the bridge request/response pipeline for coordinate
/// translation.
///
/// For non-blockquote injections, `columns` is `vec![start_column]`:
/// virtual line 0 gets `start_column`, line 1+ gets `0` (via fallback).
///
/// For blockquoted injections, `columns` has one entry per virtual line,
/// each representing the width of the blockquote prefix (e.g., `> ` = 2).
#[derive(Debug, Clone)]
pub(crate) struct RegionOffset {
    /// The starting line of the injection region in the host document.
    line: u32,
    /// Per-virtual-line column offsets (UTF-16 code units).
    /// Index = virtual line number, value = column offset for that line.
    columns: Vec<u32>,
}

impl RegionOffset {
    /// Construct a `RegionOffset` with a single column offset (non-blockquote case).
    pub(crate) fn new(line: u32, column: u32) -> Self {
        Self {
            line,
            columns: vec![column],
        }
    }

    /// Construct a `RegionOffset` with per-line column offsets (blockquote case).
    pub(crate) fn with_per_line_offsets(line: u32, columns: Vec<u32>) -> Self {
        Self { line, columns }
    }

    /// Get the starting line of the injection region.
    pub(crate) fn line(&self) -> u32 {
        self.line
    }

    /// Get a slice of all per-line column offsets.
    pub(crate) fn columns(&self) -> &[u32] {
        &self.columns
    }

    /// Get the column offset for the given virtual line.
    ///
    /// Returns the per-line offset if available, otherwise 0.
    pub(crate) fn column_for_line(&self, virtual_line: u32) -> u32 {
        self.columns
            .get(virtual_line as usize)
            .copied()
            .unwrap_or(0)
    }
}

// =============================================================================
// Virtual -> Host (response direction)
// =============================================================================

/// Translate a single virtual position to host coordinates.
///
/// Applies the per-line column offset for the virtual line, then adds the
/// line offset. For non-blockquote injections (single-element `columns`),
/// only line 0 gets a column adjustment (line 1+ falls back to 0).
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_virtual_position_to_host(pos: &mut Position, offset: &RegionOffset) {
    let virtual_line = pos.line;
    pos.line = pos.line.saturating_add(offset.line());
    pos.character = pos
        .character
        .saturating_add(offset.column_for_line(virtual_line));
}

/// Translate a virtual range to host coordinates.
///
/// Applies position translation to both start and end.
pub(crate) fn translate_virtual_range_to_host(range: &mut Range, offset: &RegionOffset) {
    translate_virtual_position_to_host(&mut range.start, offset);
    translate_virtual_position_to_host(&mut range.end, offset);
}

// =============================================================================
// Host -> Virtual (request direction)
// =============================================================================

/// Translate a single host position to virtual coordinates.
///
/// Subtracts the line offset, then applies the per-line column offset for the
/// resulting virtual line. When the line underflows (stale region data / race
/// condition), column offset is NOT applied to avoid compounding the error.
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_host_position_to_virtual(pos: &mut Position, offset: &RegionOffset) {
    let underflowed = pos.line < offset.line();
    pos.line = pos.line.saturating_sub(offset.line());
    if !underflowed {
        pos.character = pos
            .character
            .saturating_sub(offset.column_for_line(pos.line));
    }
}

/// Translate a host range to virtual coordinates.
///
/// Applies position translation to both start and end **independently**.
/// This means asymmetric column treatment is possible: if the start line
/// underflows (stale region data) but the end line does not, only the start
/// will skip column adjustment. This is intentional — each endpoint should
/// degrade independently rather than coupling their error behavior.
pub(crate) fn translate_host_range_to_virtual(range: &mut Range, offset: &RegionOffset) {
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
        translate_virtual_position_to_host(&mut pos, &RegionOffset::new(10, 4));
        assert_eq!(pos.line, 10);
        assert_eq!(pos.character, 9); // 5 + 4
    }

    #[test]
    fn position_to_host_non_first_line_ignores_column_offset() {
        let mut pos = Position {
            line: 2,
            character: 5,
        };
        translate_virtual_position_to_host(&mut pos, &RegionOffset::new(10, 4));
        assert_eq!(pos.line, 12);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_host_column_offset_saturates_on_overflow() {
        let mut pos = Position {
            line: 0,
            character: u32::MAX,
        };
        translate_virtual_position_to_host(&mut pos, &RegionOffset::new(10, 4));
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
        translate_virtual_range_to_host(&mut range, &RegionOffset::new(5, 3));
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
        translate_virtual_range_to_host(&mut range, &RegionOffset::new(5, 3));
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
        translate_virtual_range_to_host(&mut range, &RegionOffset::new(5, 3));
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
        translate_host_position_to_virtual(&mut pos, &RegionOffset::new(10, 4));
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
        translate_host_position_to_virtual(&mut pos, &RegionOffset::new(10, 4));
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_virtual_line_saturates_on_underflow() {
        let mut pos = Position {
            line: 5,
            character: 8,
        };
        translate_host_position_to_virtual(&mut pos, &RegionOffset::new(10, 4));
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
        translate_host_position_to_virtual(&mut pos, &RegionOffset::new(10, 10));
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
        translate_host_range_to_virtual(&mut range, &RegionOffset::new(5, 3));
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
        translate_host_range_to_virtual(&mut range, &RegionOffset::new(5, 3));
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
        translate_host_range_to_virtual(&mut range, &RegionOffset::new(5, 3));
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 2); // unchanged
        assert_eq!(range.end.line, 3);
        assert_eq!(range.end.character, 8); // unchanged
    }

    // ======================================================================
    // Per-line column offset tests (blockquote case)
    // ======================================================================

    #[test]
    fn position_to_host_blockquote_adds_per_line_column_offset() {
        // Blockquote: virtual (1, 5) with per-line offsets [2, 2]
        // Virtual line 1 → host line (start_line + 1), char (5 + 2) = 7
        let mut pos = Position {
            line: 1,
            character: 5,
        };
        let offset = RegionOffset::with_per_line_offsets(10, vec![2, 2]);
        translate_virtual_position_to_host(&mut pos, &offset);
        assert_eq!(pos.line, 11);
        assert_eq!(pos.character, 7); // 5 + 2
    }

    #[test]
    fn position_to_virtual_blockquote_subtracts_per_line_column_offset() {
        // Host (start_line + 1, 7) with per-line offsets [2, 2]
        // → virtual (1, 5): char (7 - 2) = 5
        let mut pos = Position {
            line: 11,
            character: 7,
        };
        let offset = RegionOffset::with_per_line_offsets(10, vec![2, 2]);
        translate_host_position_to_virtual(&mut pos, &offset);
        assert_eq!(pos.line, 1);
        assert_eq!(pos.character, 5); // 7 - 2
    }

    #[test]
    fn position_to_host_blockquote_line_beyond_offsets_no_column_adjust() {
        // Virtual line 3 is beyond offsets [2, 2] (len=2)
        // column_for_line(3) returns 0 → no column adjustment
        let mut pos = Position {
            line: 3,
            character: 5,
        };
        let offset = RegionOffset::with_per_line_offsets(10, vec![2, 2]);
        translate_virtual_position_to_host(&mut pos, &offset);
        assert_eq!(pos.line, 13);
        assert_eq!(pos.character, 5); // unchanged
    }

    #[test]
    fn position_to_virtual_blockquote_column_saturates_on_underflow() {
        // Host char 1 with per-line offset 2 → saturates to 0
        let mut pos = Position {
            line: 10,
            character: 1,
        };
        let offset = RegionOffset::with_per_line_offsets(10, vec![2]);
        translate_host_position_to_virtual(&mut pos, &offset);
        assert_eq!(pos.line, 0);
        assert_eq!(pos.character, 0); // saturated
    }

    #[test]
    fn range_to_host_blockquote_spanning_lines() {
        // Range from virtual (0, 3) to (1, 7) with per-line offsets [2, 2]
        // Start: (0+10, 3+2) = (10, 5)
        // End:   (1+10, 7+2) = (11, 9)
        let mut range = Range {
            start: Position {
                line: 0,
                character: 3,
            },
            end: Position {
                line: 1,
                character: 7,
            },
        };
        let offset = RegionOffset::with_per_line_offsets(10, vec![2, 2]);
        translate_virtual_range_to_host(&mut range, &offset);
        assert_eq!(range.start.line, 10);
        assert_eq!(range.start.character, 5); // 3 + 2
        assert_eq!(range.end.line, 11);
        assert_eq!(range.end.character, 9); // 7 + 2
    }
}
