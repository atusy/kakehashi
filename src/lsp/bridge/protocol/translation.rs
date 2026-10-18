//! Shared position/range translation helpers for host <-> virtual coordinate conversion.
//!
//! All translation functions use in-place `&mut` mutation and saturating arithmetic
//! for race-condition safety (stale region data after document edits).

use std::sync::Arc;

use tower_lsp_server::ls_types::{Position, Range, TextEdit};

use super::prepare::{Bias, EditMode, PreparedMap};

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
/// When downstream servers hold a prepared document
/// (`kakehashi/virtualDocument/prepare`), `prepared` maps between it and the
/// virtual document. `line`/`columns` always describe the virtual document,
/// so the translation functions below compose prepared ↔ virtual ↔ host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionOffset {
    /// The starting line of the injection region in the host document.
    line: u32,
    /// Per-virtual-line column offsets (UTF-16 code units).
    /// Index = virtual line number, value = column offset for that line.
    columns: Vec<u32>,
    /// Prepared ↔ virtual coordinates, when downstream servers hold a
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

    /// Attach the prepared ↔ virtual map of the text downstream servers hold.
    pub(crate) fn with_prepared(mut self, prepared: Arc<PreparedMap>) -> Self {
        self.prepared = Some(prepared);
        self
    }

    /// The prepared ↔ virtual map, when downstream servers hold a prepared
    /// document.
    pub(crate) fn prepared(&self) -> Option<&PreparedMap> {
        self.prepared.as_deref()
    }

    /// [`Self::prepared`], shared: sent along with the prepared text.
    pub(crate) fn prepared_map(&self) -> Option<&Arc<PreparedMap>> {
        self.prepared.as_ref()
    }

    /// Get the starting line of the injection region.
    pub(crate) fn line(&self) -> u32 {
        self.line
    }

    /// Get a slice of all per-line column offsets.
    pub(crate) fn columns(&self) -> &[u32] {
        &self.columns
    }

    /// Whether the region opens mid-line, after a host delimiter on the same
    /// line (a Nix `''`), rather than at a line start. A region with one
    /// column offset past 0 does; a prefixed (blockquote) region has a column
    /// per line, and one opening with a line break spans at least two lines.
    ///
    /// One line is ambiguous: a blockquoted fence ending at EOF without a
    /// line break has one column offset too, and reads as opening mid-line.
    pub(crate) fn opens_mid_line(&self) -> bool {
        matches!(self.columns.as_slice(), [column] if *column > 0)
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
    translate_unprepared_virtual_position_to_host(pos, offset);
}

/// V → host only, ignoring `offset`'s prepared map: for coordinates already
/// mapped out of the prepared document.
fn translate_unprepared_virtual_position_to_host(pos: &mut Position, offset: &RegionOffset) {
    let virtual_line = pos.line;
    pos.line = pos.line.saturating_add(offset.line());
    pos.character = pos
        .character
        .saturating_add(offset.column_for_line(virtual_line));
}

/// Translate a range and one LSP requires it to contain (a symbol's
/// selection, a link's target selection) to host coordinates. Mapped on its
/// own through a prepared map, the inner one can land outside the outer (a
/// selection at a dedented line's start, or at an emptied gap): it is then
/// clamped back into it.
pub(crate) fn translate_contained_ranges_to_host(
    outer: &mut Range,
    inner: &mut Range,
    offset: &RegionOffset,
) {
    translate_virtual_range_to_host(outer, offset);
    translate_virtual_range_to_host(inner, offset);
    if offset.prepared().is_some() {
        clamp_range_into(inner, *outer);
    }
}

/// Clamp `inner` into `outer`; an inverted `outer` (a malformed answer)
/// leaves it as is.
pub(crate) fn clamp_range_into(inner: &mut Range, outer: Range) {
    if outer.start > outer.end {
        return;
    }
    inner.start = inner.start.clamp(outer.start, outer.end);
    inner.end = inner.end.clamp(inner.start, outer.end);
}

/// Translate a virtual range to host coordinates.
///
/// Applies position translation to both start and end. Through a prepared
/// map, a range end strictly inside a replaced gap maps to the gap's end, so
/// the range covers the host text the gap stands for.
pub(crate) fn translate_virtual_range_to_host(range: &mut Range, offset: &RegionOffset) {
    // Empty as the prepared document reads it: endpoints past a line's end
    // clamp onto one offset, which opposite biases would map apart.
    let empty = range.start == range.end
        || offset
            .prepared()
            .is_some_and(|prepared| prepared.same_prepared_offset(range.start, range.end));
    translate_virtual_position_to_host_biased(&mut range.start, offset, Bias::Start);
    // An empty range stays empty: biasing its end differently from its start
    // could turn it inside out at a prepared gap.
    if empty {
        range.end = range.start;
    } else {
        translate_virtual_position_to_host_biased(&mut range.end, offset, Bias::End);
    }
}

/// Translate, in place, a set of edits a downstream server made to one
/// virtual document into host coordinates, as `mode` reads them; `false` —
/// the caller drops the set or its carrier — when the set is refused, with
/// `edits` left as they came.
///
/// An edit to a prepared document must also restore the indentation its new
/// lines lost and keep off the host text the peer hid (gaps, deleted edge
/// lines), so the set goes through [`PreparedMap::map_edits`]. An
/// unprepared document's edits translate one by one, as ranges, allocating
/// nothing.
pub(crate) fn translate_virtual_text_edits_to_host(
    edits: &mut [TextEdit],
    offset: &RegionOffset,
    mode: EditMode,
) -> bool {
    let Some(prepared) = offset.prepared() else {
        for edit in edits.iter_mut() {
            translate_virtual_range_to_host(&mut edit.range, offset);
        }
        return true;
    };
    let Some(mapped) = prepared.map_edits(edits, mode) else {
        return false;
    };
    for (edit, mut mapped) in edits.iter_mut().zip(mapped) {
        translate_unprepared_virtual_position_to_host(&mut mapped.range.start, offset);
        translate_unprepared_virtual_position_to_host(&mut mapped.range.end, offset);
        *edit = mapped;
    }
    true
}

/// Translate, in place, a main edit and the additional edits a client
/// applies together with it, as `mode` reads them. `mains` is the main edit
/// once per range the client may apply it through (an insert/replace edit
/// has two; none for an item with no main edit), each translated alone.
/// `None` — the caller drops the carrier — when a main edit is refused;
/// otherwise whether the additional edits were kept (`true` without any),
/// refused ones left as they came for the caller to drop.
///
/// A prepared document maps each main edit with the additional ones as the
/// one set the client applies: edits harmless one by one can together
/// corrupt host text the document hides. The additional edits are kept only
/// when every set maps, with its main edit mapping as it does alone (so the
/// edits sent are the set checked), and the additional edits mapping alike
/// in every set. With no main edit there is no set to check: refused. An
/// unprepared document's edits translate as ranges, allocating nothing.
pub(crate) fn translate_virtual_main_and_additional_edits_to_host(
    mains: &mut [TextEdit],
    additional: Option<&mut Vec<TextEdit>>,
    offset: &RegionOffset,
    mode: EditMode,
) -> Option<bool> {
    // The main edits as sent, to map with the additional ones: cloned only
    // on a prepared document that has any.
    let sent: Vec<TextEdit> = match &additional {
        Some(_) if offset.prepared().is_some() => mains.to_vec(),
        _ => Vec::new(),
    };
    for main in mains.iter_mut() {
        if !translate_virtual_text_edits_to_host(std::slice::from_mut(main), offset, mode) {
            return None;
        }
    }
    let Some(additional) = additional else {
        return Some(true);
    };
    if offset.prepared().is_none() {
        return Some(translate_virtual_text_edits_to_host(
            additional, offset, mode,
        ));
    }
    let mut agreed: Option<Vec<TextEdit>> = None;
    for (sent, mapped) in sent.into_iter().zip(mains.iter()) {
        let mut set = Vec::with_capacity(additional.len() + 1);
        set.push(sent);
        set.extend(additional.iter().cloned());
        if !translate_virtual_text_edits_to_host(&mut set, offset, mode) || set[0] != *mapped {
            return Some(false);
        }
        let set = set.split_off(1);
        if agreed.as_ref().is_some_and(|agreed| *agreed != set) {
            return Some(false);
        }
        agreed = Some(set);
    }
    match agreed {
        Some(mapped) => {
            *additional = mapped;
            Some(true)
        }
        None => Some(false),
    }
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

/// Whether `host_position` lies where the prepared document downstream
/// servers hold has no position of its own: strictly inside a gap (host text
/// such as an interpolation, which the prepared document shows replaced), or
/// on blank lines the peer deleted from the document's edges. A request
/// there would be answered for another place — the gap's start, the next or
/// previous line — so it is not made at all.
pub(crate) fn host_position_hidden_by_prepare(
    host_position: Position,
    offset: &RegionOffset,
) -> bool {
    let Some(prepared) = offset.prepared() else {
        return false;
    };
    let mut position = host_position;
    translate_host_position_to_unprepared_virtual(&mut position, offset);
    prepared.virtual_position_in_gap(position)
        || prepared.virtual_position_in_removed_lines(position)
}

/// Whether the host range reaches into a gap of the prepared document
/// `offset` maps to: host-owned text a downstream answer keyed to that range
/// (a color presentation's label) would overwrite — the prepared range may
/// even be empty there, where an edit would only insert beside it.
pub(crate) fn host_range_in_prepared_gap(host_range: Range, offset: &RegionOffset) -> bool {
    let Some(prepared) = offset.prepared() else {
        return false;
    };
    let mut range = host_range;
    translate_host_position_to_unprepared_virtual(&mut range.start, offset);
    translate_host_position_to_unprepared_virtual(&mut range.end, offset);
    prepared.virtual_range_in_gap(range)
}

/// Whether `host_position` lies within indentation or blank lines the
/// prepare peer removed from the document `offset` maps to, starts included
/// (and from where the deleted closing lines start). Downstream sees the
/// caret elsewhere instead — at the line's content, the document's start or
/// the content's end — so an edit it anchors there (a completion's replace
/// range) would not contain the caret the client asked at.
pub(crate) fn host_position_in_removed_indent(
    host_position: Position,
    offset: &RegionOffset,
) -> bool {
    let Some(prepared) = offset.prepared() else {
        return false;
    };
    let mut position = host_position;
    translate_host_position_to_unprepared_virtual(&mut position, offset);
    prepared.virtual_position_in_removed_indent(position)
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
    translate_host_position_to_unprepared_virtual(pos, offset);
    if let Some(prepared) = offset.prepared() {
        *pos = prepared.to_prepared(*pos, bias);
    }
}

/// Host → V only, ignoring `offset`'s prepared map: the virtual document's
/// coordinates, before the prepared document's.
fn translate_host_position_to_unprepared_virtual(pos: &mut Position, offset: &RegionOffset) {
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

    #[test]
    fn a_region_opens_mid_line_only_after_a_delimiter_on_its_line() {
        // A Nix `''` string's content follows the delimiter on its line.
        assert!(RegionOffset::new(1, 19).opens_mid_line());
        // A fence's content opens at a line start.
        assert!(!RegionOffset::new(1, 0).opens_mid_line());
        // Blockquoted and list-item fences: a prefix column per line.
        assert!(!RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]).opens_mid_line());
        assert!(!RegionOffset::with_per_line_offsets(3, vec![2, 0]).opens_mid_line());
    }

    fn prepared(
        virtual_text: &str,
        layout: &super::super::prepare::VirtualLayout,
        answer: serde_json::Value,
    ) -> Arc<PreparedMap> {
        let result = serde_json::from_value(answer).unwrap();
        super::super::prepare::apply_prepare_result(virtual_text, layout, Some(result))
            .unwrap()
            .map
    }

    /// A region at host line 10, column 4, whose two virtual lines lost a
    /// two-space indent to the prepare peer.
    fn prepared_offset() -> RegionOffset {
        let virtual_text = "  if x:\n    y\n";
        let layout = super::super::prepare::VirtualLayout::single(virtual_text);
        let map = prepared(
            virtual_text,
            &layout,
            serde_json::json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}]}),
        );
        RegionOffset::new(10, 4).with_prepared(map)
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
    fn a_range_ending_at_a_dedented_line_start_covers_none_of_its_indent() {
        let offset = prepared_offset();
        // P (0,0)-(1,0): the whole first line, line break included.
        let mut range = Range::new(Position::new(0, 0), Position::new(1, 0));
        translate_virtual_range_to_host(&mut range, &offset);
        assert_eq!(
            range,
            Range::new(Position::new(10, 6), Position::new(11, 0))
        );
    }

    #[test]
    fn prepared_text_edits_restore_the_indent_in_host_coordinates() {
        let offset = prepared_offset();
        // A line inserted after `y` (P (1, 2)) regains the removed indent.
        let mut edits = [TextEdit {
            range: Range::new(Position::new(1, 3), Position::new(1, 3)),
            new_text: "\n  z".to_string(),
        }];
        assert!(translate_virtual_text_edits_to_host(
            &mut edits,
            &offset,
            EditMode::Extent
        ));
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(11, 5), Position::new(11, 5))
        );
        assert_eq!(edits[0].new_text, "\n    z");
        // A refused set is left as it came.
        let reversed = TextEdit {
            range: Range::new(Position::new(1, 3), Position::new(0, 0)),
            new_text: "x".to_string(),
        };
        let mut edits = [reversed.clone()];
        assert!(!translate_virtual_text_edits_to_host(
            &mut edits,
            &offset,
            EditMode::Extent
        ));
        assert_eq!(edits, [reversed]);
    }

    #[test]
    fn unprepared_text_edits_translate_as_ranges() {
        let mut edits = [TextEdit {
            range: Range::new(Position::new(0, 1), Position::new(1, 0)),
            new_text: "\nz".to_string(),
        }];
        assert!(translate_virtual_text_edits_to_host(
            &mut edits,
            &RegionOffset::new(10, 4),
            EditMode::Extent
        ));
        assert_eq!(
            edits[0].range,
            Range::new(Position::new(10, 5), Position::new(11, 0))
        );
        assert_eq!(edits[0].new_text, "\nz");
    }

    #[test]
    fn offsets_compare_unequal_once_a_prepared_map_differs() {
        assert_ne!(prepared_offset(), RegionOffset::new(10, 4));
        assert_eq!(prepared_offset(), prepared_offset());
    }

    #[test]
    fn a_host_position_inside_a_prepared_gap_is_hidden() {
        use super::super::prepare::{SegmentKind, VirtualLayout};
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
        let map = prepared(
            virtual_text,
            &layout,
            serde_json::json!({"segments": [
                {"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}
            ]}),
        );
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        assert!(host_position_hidden_by_prepare(
            Position::new(10, 6),
            &offset
        ));
        // Its edges are not inside it.
        for character in [2, 4, 8] {
            assert!(
                !host_position_hidden_by_prepare(Position::new(10, character), &offset),
                "{character}"
            );
        }
        // An unprepared offset hides nothing.
        assert!(!host_position_hidden_by_prepare(
            Position::new(10, 6),
            &RegionOffset::new(10, 0)
        ));
    }

    #[test]
    fn a_host_position_on_removed_edge_lines_is_hidden() {
        use super::super::prepare::VirtualLayout;
        // `''\n  foo\n''` at host line 10: the opening line goes, and the
        // indent of `foo`.
        let virtual_text = "\n  foo\n";
        let map = prepared(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            serde_json::json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""},
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
            ]}]}),
        );
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        assert!(host_position_hidden_by_prepare(
            Position::new(10, 0),
            &offset
        ));
        // A removed indent is no removed line: a request there stands for
        // the line's content.
        assert!(!host_position_hidden_by_prepare(
            Position::new(11, 0),
            &offset
        ));
        assert!(!host_position_hidden_by_prepare(
            Position::new(11, 3),
            &offset
        ));
    }

    #[test]
    fn an_empty_host_range_inside_a_replaced_gap_stays_empty() {
        use super::super::prepare::{SegmentKind, VirtualLayout};
        // `x = ${a}` at host line 10, the interpolation shown as `None`.
        let virtual_text = "x =     \n";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..8, "${a}".to_string()),
                (SegmentKind::Content, 8..9, String::new()),
            ],
        );
        let map = prepared(
            virtual_text,
            &layout,
            serde_json::json!({"segments": [
                {"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}
            ]}),
        );
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        let mut range = Range::new(Position::new(10, 6), Position::new(10, 6));
        translate_host_range_to_virtual(&mut range, &offset);
        assert_eq!(range.start, range.end, "{range:?}");
    }

    #[test]
    fn a_range_clamping_to_one_prepared_offset_stays_empty() {
        use super::super::prepare::{SegmentKind, VirtualLayout};
        // `ab${x}` on host line 10, the interpolation prepared as nothing.
        let virtual_text = "ab    \ncd";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..2, String::new()),
                (SegmentKind::Gap, 2..6, "${x}".to_string()),
                (SegmentKind::Content, 6..9, String::new()),
            ],
        );
        let map = prepared(
            virtual_text,
            &layout,
            serde_json::json!({"segments": [
                {"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}
            ]}),
        );
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        let mut range = Range::new(Position::new(0, 2), Position::new(0, 100));
        translate_virtual_range_to_host(&mut range, &offset);
        assert_eq!(range.start, range.end, "{range:?}");
    }

    #[test]
    fn a_range_ending_inside_a_replaced_gap_covers_its_host_text() {
        use super::super::prepare::{SegmentKind, VirtualLayout};
        // `x = ${a}` at host line 10, the interpolation shown as `None`.
        let virtual_text = "x =     \n";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..4, String::new()),
                (SegmentKind::Gap, 4..8, "${a}".to_string()),
                (SegmentKind::Content, 8..9, String::new()),
            ],
        );
        let map = prepared(
            virtual_text,
            &layout,
            serde_json::json!({"segments": [
                {"type": "content"}, {"type": "gap", "content": "None"}, {"type": "content"}
            ]}),
        );
        let offset = RegionOffset::new(10, 0).with_prepared(map);
        // A diagnostic on `No` of `None`: the gap's start through its end.
        let mut range = Range::new(Position::new(0, 4), Position::new(0, 6));
        translate_virtual_range_to_host(&mut range, &offset);
        assert_eq!(
            range,
            Range::new(Position::new(10, 4), Position::new(10, 8))
        );
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
