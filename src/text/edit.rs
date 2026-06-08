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
    byte_edits.sort_by_key(|(start, _, _)| *start);

    let mut result = text.to_string();
    for (start, end, new_text) in byte_edits.into_iter().rev() {
        result.replace_range(start..end, new_text);
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
}
