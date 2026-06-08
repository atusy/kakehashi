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
/// All edit ranges are interpreted against the original `text` (LSP semantics).
/// Edits are sorted by `(start, end)` and applied in a single **forward copy**
/// (O(n + total edit length)) rather than repeated `String::replace_range`
/// (which is O(n²)): the result is built by copying the gap before each edit,
/// then its `new_text`. Out-of-bounds positions are clamped, an inverted range
/// (`start` after `end`) is normalized, byte offsets are floored to UTF-8 char
/// boundaries, and an edit overlapping one already applied is dropped (LSP
/// forbids overlap, but a buggy server must never make us panic or corrupt) —
/// resolved purely by sort order, so the lower-start edit wins.
pub(crate) fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    if edits.is_empty() {
        return text.to_string();
    }

    let mapper = PositionMapper::new(text);
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let a = floor_char_boundary(text, mapper.position_to_byte_clamped(e.range.start));
            let b = floor_char_boundary(text, mapper.position_to_byte_clamped(e.range.end));
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            (start, end, e.new_text.as_str())
        })
        .collect();

    byte_edits.sort_by_key(|&(start, end, _)| (start, end));

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end, new_text) in byte_edits {
        // Drop an edit that starts before the cursor — it overlaps one already
        // applied; keep the earlier (lower-start) edit.
        if start < cursor {
            continue;
        }
        result.push_str(&text[cursor..start]);
        result.push_str(new_text);
        cursor = end;
    }
    result.push_str(&text[cursor..]);
    result
}

/// Floor `offset` down to the nearest UTF-8 char boundary of `text` (or its
/// length), so slicing never panics on a mid-codepoint offset from a buggy
/// downstream server.
fn floor_char_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
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
        // verbatim would corrupt or panic; the forward copy keeps the earlier
        // (lower-start) edit and drops the later overlapping one.
        let text = "abcdef";
        let edits = vec![edit(0, 0, 0, 3, "A"), edit(0, 2, 0, 5, "B")];
        // (0..3)->"A" applied (cursor=3), then (2..5) starts before the cursor
        // and is dropped, leaving the tail "def".
        let out = apply_text_edits(text, &edits);
        assert_eq!(out, "Adef");
    }

    #[test]
    fn out_of_bounds_position_is_clamped_to_eof() {
        // A downstream server may return an end past EOF (the canonical
        // "insert final newline" shape). The position is clamped, not dropped.
        let text = "abc";
        let edits = vec![edit(0, 0, u32::MAX, u32::MAX, "X")];
        assert_eq!(apply_text_edits(text, &edits), "X");
    }

    #[test]
    fn inverted_range_is_normalized_not_panicking() {
        // start after end: normalize to [end, start) rather than panicking.
        // Here start=(0,3), end=(0,1) over "abcd" → replace "bc" with "Z".
        let text = "abcd";
        let edits = vec![edit(0, 3, 0, 1, "Z")];
        assert_eq!(apply_text_edits(text, &edits), "aZd");
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
