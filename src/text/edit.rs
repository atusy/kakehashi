//! Apply LSP `TextEdit`s to a text buffer.
//!
//! Formatting and rangeFormatting responses are `TextEdit[]` whose ranges are
//! all relative to the *original* document and which, per the LSP spec, must
//! not overlap. The concatenated formatting pipeline
//! (concatenated-formatting-pipeline) feeds each server's output into the next,
//! which requires materializing the post-edit text — this module does that.

use tower_lsp_server::ls_types::TextEdit;

use super::position::PositionMapper;

/// Apply `edits` to `text`, returning the resulting string.
///
/// All edit ranges are interpreted against the original `text` (LSP semantics),
/// so edits are applied highest-position-first; that way an earlier edit never
/// shifts the byte offsets a later edit still refers to. Out-of-bounds
/// positions are clamped, and an inverted range (`start` after `end`) is
/// normalized rather than panicking.
pub(crate) fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    if edits.is_empty() {
        return text.to_string();
    }

    let mapper = PositionMapper::new(text);
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let a = mapper.position_to_byte_clamped(e.range.start);
            let b = mapper.position_to_byte_clamped(e.range.end);
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            (start, end, e.new_text.as_str())
        })
        .collect();

    // Apply from the end of the document backwards so that replacing an earlier
    // span never invalidates the byte offsets of a later (non-overlapping) edit.
    // Sort by `(start, end)`: when several edits share a start (e.g. a zero-width
    // insertion and a replacement), the wider span must be applied first under
    // the reversed iteration, so the insertion is not swallowed by the
    // replacement's range.
    byte_edits.sort_by_key(|&(start, end, _)| (start, end));

    let mut result = text.to_string();
    // Defensive against buggy downstream servers: clamp offsets to UTF-8 char
    // boundaries (`replace_range` panics otherwise) and skip an edit that
    // overlaps one already applied (LSP forbids overlap, but we never trust the
    // server enough to panic). `last_start` is the start of the most recently
    // applied edit; since we go highest-first, a later edit whose `end` reaches
    // past it overlaps and is dropped.
    let mut last_start = result.len();
    for (mut start, mut end, new_text) in byte_edits.into_iter().rev() {
        while start > 0 && !result.is_char_boundary(start) {
            start -= 1;
        }
        while end > start && !result.is_char_boundary(end) {
            end -= 1;
        }
        if end > last_start {
            continue;
        }
        result.replace_range(start..end, new_text);
        last_start = start;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn edit(sl: u32, sc: u32, el: u32, ec: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line: sl,
                    character: sc,
                },
                end: Position {
                    line: el,
                    character: ec,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn applies_single_edit_replacing_a_span() {
        let text = "hello world";
        let edits = vec![edit(0, 6, 0, 11, "rust")];
        assert_eq!(apply_text_edits(text, &edits), "hello rust");
    }

    #[test]
    fn empty_edits_return_text_unchanged() {
        assert_eq!(apply_text_edits("unchanged", &[]), "unchanged");
    }

    #[test]
    fn applies_multiple_disjoint_edits_regardless_of_order() {
        // Two edits given in ascending order; applying the later one first must
        // not shift the earlier one. Replace "a"->"AAAA" and "c"->"C".
        let text = "a b c";
        let edits = vec![edit(0, 0, 0, 1, "AAAA"), edit(0, 4, 0, 5, "C")];
        assert_eq!(apply_text_edits(text, &edits), "AAAA b C");
    }

    #[test]
    fn applies_a_pure_insertion_at_a_zero_width_range() {
        let text = "ab";
        // Insert "X" between a and b (zero-width range at col 1).
        let edits = vec![edit(0, 1, 0, 1, "X")];
        assert_eq!(apply_text_edits(text, &edits), "aXb");
    }

    #[test]
    fn applies_a_multi_line_replacement() {
        let text = "line1\nline2\nline3";
        // Replace from (0,0) through (1,5) — i.e. "line1\nline2" — with "L".
        let edits = vec![edit(0, 0, 1, 5, "L")];
        assert_eq!(apply_text_edits(text, &edits), "L\nline3");
    }

    #[test]
    fn full_document_replacement_replaces_everything() {
        let text = "old\ncontent";
        let edits = vec![edit(0, 0, 1, 7, "new")];
        assert_eq!(apply_text_edits(text, &edits), "new");
    }

    #[test]
    fn insertion_and_replacement_at_same_start_both_apply() {
        // A zero-width insertion of "X" at col 0 and a replacement of "ab"
        // (0..2) with "Y", both starting at col 0. The insertion must survive
        // rather than being swallowed by the replacement's range.
        let text = "ab";
        let edits = vec![edit(0, 0, 0, 0, "X"), edit(0, 0, 0, 2, "Y")];
        assert_eq!(apply_text_edits(text, &edits), "XY");
    }

    #[test]
    fn overlapping_edits_are_dropped_rather_than_panicking() {
        // Two edits whose original ranges overlap (0..3 and 2..5). Applying both
        // verbatim would corrupt or panic; the second (lower-priority by
        // position) overlapping edit is skipped defensively.
        let text = "abcdef";
        let edits = vec![edit(0, 0, 0, 3, "A"), edit(0, 2, 0, 5, "B")];
        // Highest-start first: (2..5)->"B" applied, then (0..3) overlaps and is
        // dropped, leaving the tail intact.
        let out = apply_text_edits(text, &edits);
        assert_eq!(out, "abBf");
    }

    #[test]
    fn multibyte_text_is_not_corrupted() {
        // "café" — 'é' is two bytes. Replace "café" (cols 0..4 in UTF-16) with
        // "tea" and confirm no panic / clean result.
        let text = "café";
        let edits = vec![edit(0, 0, 0, 4, "tea")];
        assert_eq!(apply_text_edits(text, &edits), "tea");
    }
}
