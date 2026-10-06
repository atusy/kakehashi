//! Shared position/range translation helpers for host <-> virtual coordinate conversion.
//!
//! All translation functions use in-place `&mut` mutation and saturating arithmetic
//! for race-condition safety (stale region data after document edits).

use std::sync::Arc;

use tower_lsp_server::ls_types::{Position, Range, TextEdit};

use super::prepare::{Bias, PreparedMap};

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
/// each representing the width of the blockquote prefix (e.g., `> ` = 2) —
/// plus, when the content ends with a newline, a trailing `0` entry for the
/// row where the included ranges end (the closing-fence line). That row's
/// real prefix is unrecorded; `workspace_edit_preserves_line_prefixes`
/// derives its boundary semantics from the region end instead.
///
/// When the virtual document was prepared (`kakehashi/virtualDocument/prepare`),
/// downstream servers see the prepared text, and `prepared` maps between it and
/// the virtual document. `line`/`columns` always describe the virtual document,
/// so the translation functions below compose prepared ↔ virtual ↔ host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionOffset {
    /// The starting line of the injection region in the host document.
    line: u32,
    /// Per-virtual-line column offsets (UTF-16 code units).
    /// Index = virtual line number, value = column offset for that line.
    columns: Vec<u32>,
    /// Prepared ↔ virtual coordinates, when downstream servers see a
    /// prepared document.
    prepared: Option<Arc<PreparedMap>>,
}

impl RegionOffset {
    /// Construct a `RegionOffset` with a single column offset (non-blockquote case).
    pub(crate) fn new(line: u32, column: u32) -> Self {
        Self {
            line,
            columns: vec![column],
            prepared: None,
        }
    }

    /// Construct a `RegionOffset` with per-line column offsets (blockquote case).
    pub(crate) fn with_per_line_offsets(line: u32, columns: Vec<u32>) -> Self {
        Self {
            line,
            columns,
            prepared: None,
        }
    }

    /// Attach the prepared ↔ virtual map of the document downstream servers
    /// were sent.
    pub(crate) fn with_prepared(mut self, prepared: Option<Arc<PreparedMap>>) -> Self {
        self.prepared = prepared;
        self
    }

    /// The prepared ↔ virtual map, when downstream servers see a prepared
    /// document.
    pub(crate) fn prepared(&self) -> Option<&PreparedMap> {
        self.prepared.as_deref()
    }

    /// This offset without its prepared map: virtual ↔ host only.
    pub(crate) fn unprepared(&self) -> Self {
        Self {
            line: self.line,
            columns: self.columns.clone(),
            prepared: None,
        }
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
    translate_virtual_position_to_host_biased(pos, offset, Bias::Start);
}

fn translate_virtual_position_to_host_biased(
    pos: &mut Position,
    offset: &RegionOffset,
    bias: Bias,
) {
    if let Some(prepared) = offset.prepared() {
        *pos = prepared.to_virtual(*pos, bias);
    }
    translate_unprepared_position_to_host(pos, offset);
}

/// V → host only, ignoring `offset`'s prepared map: for coordinates already
/// mapped out of the prepared document.
fn translate_unprepared_position_to_host(pos: &mut Position, offset: &RegionOffset) {
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
    let empty = range.start == range.end;
    translate_virtual_position_to_host_biased(&mut range.start, offset, Bias::Start);
    // An empty range stays empty: biasing its end differently from its start
    // could turn it inside out at a prepared gap.
    if empty {
        range.end = range.start;
    } else {
        translate_virtual_position_to_host_biased(&mut range.end, offset, Bias::End);
    }
}

/// Translate one edit a downstream server made into host coordinates.
///
/// Unlike a bare range, an edit to a prepared document must also restore
/// the indentation its new lines lost and must not touch a gap (host-owned
/// text), so it goes through [`PreparedMap::edit_to_virtual`]. Returns
/// `false` — the caller drops the edit or its carrier — when it is refused.
pub(crate) fn translate_virtual_text_edit_to_host(
    edit: &mut TextEdit,
    offset: &RegionOffset,
) -> bool {
    if let Some(prepared) = offset.prepared() {
        let Some(virtual_edit) = prepared.edit_to_virtual(edit) else {
            return false;
        };
        *edit = virtual_edit;
        translate_unprepared_position_to_host(&mut edit.range.start, offset);
        translate_unprepared_position_to_host(&mut edit.range.end, offset);
    } else {
        translate_virtual_range_to_host(&mut edit.range, offset);
    }
    true
}

/// Whether any two of `ranges` overlap (touching is fine), as LSP forbids
/// within one edit set. Edits mapped one by one out of a prepared document
/// can come to overlap: a change at a dedented line's start also covers the
/// indent the peer removed, which an edit ending there covers too.
pub(crate) fn ranges_overlap<'a>(ranges: impl IntoIterator<Item = &'a Range>) -> bool {
    let mut ranges: Vec<&Range> = ranges.into_iter().collect();
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.windows(2).any(|pair| pair[0].end > pair[1].start)
}

/// Translate a whole set of edits to one virtual document (a formatting
/// result) into host coordinates.
///
/// For a prepared document the edits are applied and re-diffed
/// ([`PreparedMap::edits_to_virtual`]), so a whole-document replacement maps
/// as precisely as small edits. `None` when the set is refused.
pub(crate) fn translate_virtual_text_edits_to_host(
    edits: Vec<TextEdit>,
    offset: &RegionOffset,
) -> Option<Vec<TextEdit>> {
    let mut edits = match offset.prepared() {
        Some(prepared) => prepared.edits_to_virtual(&edits)?,
        None => edits,
    };
    for edit in &mut edits {
        translate_unprepared_position_to_host(&mut edit.range.start, offset);
        translate_unprepared_position_to_host(&mut edit.range.end, offset);
    }
    Some(edits)
}

// =============================================================================
// Host -> Virtual (request direction)
// =============================================================================

/// Whether `host_position` can be translated into virtual coordinates without
/// underflow, i.e. it does not fall before the injection region's *leading*
/// boundary. This checks the lower bound only — a position past the region's
/// end still returns `true` here; for the trailing bound see
/// [`host_position_within_region_bounds`].
///
/// A position *above* the region (`line < region start line`) signals stale
/// region data — typically an in-flight request whose region was shifted by a
/// concurrent host edit. A position on a region line but *before* that line's
/// content start column (e.g. the cursor is on the markdown fence backticks or
/// inside a blockquote `> ` prefix rather than the injected content) is
/// likewise outside.
///
/// Either case would otherwise be silently mistranslated by the `saturating_sub`
/// in [`translate_host_position_to_virtual`] and forwarded as wrong coordinates:
/// a line above the region clamps the line to 0 (the column is deliberately left
/// unadjusted there, so the character is preserved → `(0, character)`), while a
/// position before the start column on an in-range line clamps the character to 0
/// (→ `(virtual_line, 0)`). The column boundary is checked against the same
/// per-virtual-line offset used by translation, so non-blockquote lines past the
/// first (offset 0) never trigger a false abort.
pub(crate) fn host_position_within_region(host_position: Position, offset: &RegionOffset) -> bool {
    if host_position.line < offset.line() {
        return false;
    }
    let virtual_line = host_position.line - offset.line();
    host_position.character >= offset.column_for_line(virtual_line)
}

/// Whether `host_position` lies within indentation the prepare peer removed
/// from the document `offset` maps to. Downstream sees the caret at the
/// line's content instead, so an edit it anchors there (a completion's
/// replace range) would not contain the caret the client asked at.
pub(crate) fn host_position_in_removed_indent(
    host_position: Position,
    offset: &RegionOffset,
) -> bool {
    let Some(prepared) = offset.prepared() else {
        return false;
    };
    let mut position = host_position;
    translate_host_position_to_virtual(&mut position, &offset.unprepared());
    prepared.virtual_position_in_removed_indent(position)
}

/// Whether `host_position` lies strictly inside a gap of the prepared
/// document `offset` maps to — host-owned text (an interpolation, say) that
/// no downstream request should be made at: an implicit completion there
/// would insert into it.
pub(crate) fn host_position_in_prepared_gap(
    host_position: Position,
    offset: &RegionOffset,
) -> bool {
    let Some(prepared) = offset.prepared() else {
        return false;
    };
    let mut position = host_position;
    translate_host_position_to_virtual(&mut position, &offset.unprepared());
    prepared.virtual_position_in_gap(position)
}

/// [`host_position_within_region`] plus the trailing bound: the position must
/// also not lie past `region_end` — the region's end-of-content mapped to host
/// coordinates (`region_host_end(virtual_content, offset)`).
///
/// `<=` at the end is INCLUSIVE on purpose: end-of-content is a valid LSP
/// position (the caret at the tail of the injected text maps to the virtual
/// document's EOF), the same rule the code-action diagnostic filter pins.
/// A position past it has no virtual coordinate at all — `saturating_sub`
/// translation would forward it as a plausible-but-wrong position beyond the
/// virtual document's end (e.g. inside a trailing named child the query
/// excluded from the virtual content).
pub(crate) fn host_position_within_region_bounds(
    host_position: Position,
    offset: &RegionOffset,
    region_end: Position,
) -> bool {
    host_position_within_region(host_position, offset) && host_position <= region_end
}

/// Translate a single host position to virtual coordinates.
///
/// Subtracts the line offset, then applies the per-line column offset for the
/// resulting virtual line. When the line underflows (stale region data / race
/// condition), column offset is NOT applied to avoid compounding the error.
/// Uses saturating arithmetic for race-condition safety.
pub(crate) fn translate_host_position_to_virtual(pos: &mut Position, offset: &RegionOffset) {
    translate_host_position_to_virtual_biased(pos, offset, Bias::Start);
}

fn translate_host_position_to_virtual_biased(
    pos: &mut Position,
    offset: &RegionOffset,
    bias: Bias,
) {
    let underflowed = pos.line < offset.line();
    pos.line = pos.line.saturating_sub(offset.line());
    if !underflowed {
        pos.character = pos
            .character
            .saturating_sub(offset.column_for_line(pos.line));
    }
    if let Some(prepared) = offset.prepared() {
        *pos = prepared.to_prepared(*pos, bias);
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
    let empty = range.start == range.end;
    translate_host_position_to_virtual_biased(&mut range.start, offset, Bias::Start);
    if empty {
        range.end = range.start;
    } else {
        translate_host_position_to_virtual_biased(&mut range.end, offset, Bias::End);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A region at host line 10, column 4, whose two virtual lines lost a
    /// two-space indent to the prepare peer.
    fn prepared_offset() -> RegionOffset {
        let virtual_text = "  if x:\n    y\n";
        let layout = super::super::prepare::VirtualLayout::single(virtual_text);
        let result = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]}]}))
        .unwrap();
        let prepared =
            super::super::prepare::apply_prepare_result(virtual_text, &layout, Some(result))
                .unwrap();
        RegionOffset::new(10, 4).with_prepared(prepared.map)
    }

    #[test]
    fn a_host_position_inside_a_prepared_gap_is_recognized() {
        use super::super::prepare::{SegmentKind, VirtualLayout, apply_prepare_result};
        // `x = ${a}` at host line 10, the interpolation a gap.
        let virtual_text = "x =     \n";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..8, "${a}".to_string()),
                (SegmentKind::Content, 8..9, String::new()),
            ],
        );
        let result = serde_json::from_value(serde_json::json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(virtual_text, &layout, Some(result)).unwrap();
        let offset = RegionOffset::new(10, 0).with_prepared(prepared.map);
        assert!(host_position_in_prepared_gap(Position::new(10, 6), &offset));
        // Its edges are not inside it.
        assert!(!host_position_in_prepared_gap(
            Position::new(10, 4),
            &offset
        ));
        assert!(!host_position_in_prepared_gap(
            Position::new(10, 8),
            &offset
        ));
        assert!(!host_position_in_prepared_gap(
            Position::new(10, 2),
            &offset
        ));
    }

    #[test]
    fn a_position_in_removed_indent_is_recognised() {
        use super::super::prepare::{VirtualLayout, apply_prepare_result};
        let virtual_text = "  foo\n";
        let result = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]}]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(result),
        )
        .unwrap();
        let offset = RegionOffset::new(10, 0).with_prepared(prepared.map);
        assert!(host_position_in_removed_indent(
            Position::new(10, 0),
            &offset
        ));
        assert!(host_position_in_removed_indent(
            Position::new(10, 1),
            &offset
        ));
        assert!(!host_position_in_removed_indent(
            Position::new(10, 2),
            &offset
        ));
        assert!(!host_position_in_removed_indent(
            Position::new(10, 4),
            &offset
        ));
    }

    #[test]
    fn prepared_positions_compose_with_the_region_offset() {
        let offset = prepared_offset();
        // P (1, 2) is `y`: V (1, 4), host (11, 4).
        let mut pos = Position::new(1, 2);
        translate_virtual_position_to_host(&mut pos, &offset);
        assert_eq!(pos, Position::new(11, 4));
        translate_host_position_to_virtual(&mut pos, &offset);
        assert_eq!(pos, Position::new(1, 2));
    }

    #[test]
    fn prepared_text_edit_restores_the_indent_in_host_coordinates() {
        let offset = prepared_offset();
        let mut edit = TextEdit {
            range: Range::new(Position::new(1, 3), Position::new(1, 3)),
            new_text: "\n  z".to_string(),
        };
        assert!(translate_virtual_text_edit_to_host(&mut edit, &offset));
        assert_eq!(
            edit.range,
            Range::new(Position::new(11, 5), Position::new(11, 5))
        );
        assert_eq!(edit.new_text, "\n    z");
    }

    #[test]
    fn offsets_compare_unequal_once_a_prepared_map_differs() {
        assert_ne!(prepared_offset(), RegionOffset::new(10, 4));
        assert_eq!(prepared_offset(), prepared_offset());
    }

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

    // ======================================================================
    // host_position_within_region
    // ======================================================================

    #[test]
    fn position_within_region_when_on_start_line_at_or_after_start_column() {
        // On the start line, at the start column → inside.
        let pos = Position {
            line: 10,
            character: 4,
        };
        assert!(host_position_within_region(pos, &RegionOffset::new(10, 4)));
    }

    #[test]
    fn position_within_region_when_below_start_line() {
        // Non-blockquote: lines past the first have column offset 0, so any
        // character is inside — no false abort.
        let pos = Position {
            line: 15,
            character: 0,
        };
        assert!(host_position_within_region(pos, &RegionOffset::new(10, 4)));
    }

    #[test]
    fn position_outside_region_when_above_start_line() {
        // Stale region data: host position is above where the region starts.
        let pos = Position {
            line: 9,
            character: 0,
        };
        assert!(!host_position_within_region(pos, &RegionOffset::new(10, 4)));
    }

    #[test]
    fn position_outside_region_when_before_start_column_on_start_line() {
        // On the start line but left of the start column (e.g. cursor on the
        // fence backticks) → outside; translation would clamp to (0, 0).
        let pos = Position {
            line: 10,
            character: 2,
        };
        assert!(!host_position_within_region(pos, &RegionOffset::new(10, 4)));
    }

    #[test]
    fn position_outside_region_within_blockquote_prefix() {
        // Blockquote: every virtual line carries a `> ` prefix width of 2.
        // A cursor inside that prefix on line 1 (char 1) is outside the content.
        let offset = RegionOffset::with_per_line_offsets(10, vec![2, 2]);
        let inside_prefix = Position {
            line: 11,
            character: 1,
        };
        assert!(!host_position_within_region(inside_prefix, &offset));

        let at_content = Position {
            line: 11,
            character: 2,
        };
        assert!(host_position_within_region(at_content, &offset));
    }

    // ======================================================================
    // host_position_within_region_bounds
    // ======================================================================

    #[test]
    fn bounds_rejects_position_past_region_end() {
        let offset = RegionOffset::new(2, 4);
        let region_end = Position {
            line: 2,
            character: 10,
        };
        // Past the end on the same line, and on a later line: either would
        // translate to a virtual coordinate beyond the document's EOF.
        assert!(!host_position_within_region_bounds(
            Position {
                line: 2,
                character: 11,
            },
            &offset,
            region_end,
        ));
        assert!(!host_position_within_region_bounds(
            Position {
                line: 3,
                character: 0,
            },
            &offset,
            region_end,
        ));
    }

    #[test]
    fn bounds_accepts_region_end_inclusive_and_keeps_lower_bound() {
        let offset = RegionOffset::new(2, 4);
        let region_end = Position {
            line: 2,
            character: 10,
        };
        // End-of-content is a valid caret position (virtual EOF) — inclusive.
        assert!(host_position_within_region_bounds(
            Position {
                line: 2,
                character: 10,
            },
            &offset,
            region_end,
        ));
        assert!(host_position_within_region_bounds(
            Position {
                line: 2,
                character: 4,
            },
            &offset,
            region_end,
        ));
        // The lower bound from host_position_within_region still applies.
        assert!(!host_position_within_region_bounds(
            Position {
                line: 2,
                character: 3,
            },
            &offset,
            region_end,
        ));
        assert!(!host_position_within_region_bounds(
            Position {
                line: 1,
                character: 9,
            },
            &offset,
            region_end,
        ));
    }

    #[test]
    fn bounds_compare_lexicographically_across_lines() {
        // Multi-line region ending at (4, 2). A component-wise comparison
        // (line <= end.line && character <= end.character) would wrongly
        // reject any character above 2 on the earlier lines.
        let offset = RegionOffset::new(2, 4);
        let region_end = Position {
            line: 4,
            character: 2,
        };
        assert!(host_position_within_region_bounds(
            Position {
                line: 2,
                character: 15,
            },
            &offset,
            region_end,
        ));
        assert!(host_position_within_region_bounds(
            Position {
                line: 3,
                character: 99,
            },
            &offset,
            region_end,
        ));
        // Past the end on the end line itself.
        assert!(!host_position_within_region_bounds(
            Position {
                line: 4,
                character: 3,
            },
            &offset,
            region_end,
        ));
    }

    #[test]
    fn bounds_compose_with_blockquote_prefix_offsets() {
        // Blockquote: `> ` prefix width 2 on both virtual lines; region ends
        // mid-line at (11, 8). The leading (per-line prefix) and trailing
        // bounds must both hold.
        let offset = RegionOffset::with_per_line_offsets(10, vec![2, 2]);
        let region_end = Position {
            line: 11,
            character: 8,
        };
        // Inside the prefix on the end line: below the leading bound.
        assert!(!host_position_within_region_bounds(
            Position {
                line: 11,
                character: 1,
            },
            &offset,
            region_end,
        ));
        // End-of-content: accepted, inclusive.
        assert!(host_position_within_region_bounds(
            Position {
                line: 11,
                character: 8,
            },
            &offset,
            region_end,
        ));
        // One past the end: rejected.
        assert!(!host_position_within_region_bounds(
            Position {
                line: 11,
                character: 9,
            },
            &offset,
            region_end,
        ));
    }
}
