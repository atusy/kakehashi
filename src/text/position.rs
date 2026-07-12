use std::borrow::Cow;

use line_index::{LineIndex, WideEncoding, WideLineCol};
use tower_lsp_server::ls_types::Position;

use super::char_boundary::floor_char_boundary;

/// Position mapper for converting between LSP positions and byte offsets
pub struct PositionMapper<'text> {
    /// LSP line index, including lone-CR line boundaries.
    line_index: LineIndex,
    /// Only needed for lone-CR documents: tree-sitter counts only LF as a row
    /// boundary, while `line_index` above follows LSP semantics in that case.
    tree_line_index: Option<LineIndex>,
    text: &'text str,
}

impl<'text> PositionMapper<'text> {
    /// Create a new PositionMapper with pre-computed line starts
    pub fn new(text: &'text str) -> Self {
        // `line-index` recognizes LF (and therefore CRLF) boundaries, while
        // the LSP position model also treats a lone CR as a line terminator.
        // Replacing only lone CR bytes preserves every byte offset and UTF-16
        // width, allowing the dependency's logarithmic lookups to implement
        // the full LSP line-ending contract without altering document text.
        let indexed_text = normalize_lone_carriage_returns(text);
        let tree_line_index = matches!(indexed_text, Cow::Owned(_)).then(|| LineIndex::new(text));
        let line_index = LineIndex::new(&indexed_text);
        Self {
            line_index,
            tree_line_index,
            text,
        }
    }
}

fn normalize_lone_carriage_returns(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let Some(first_lone_cr) = bytes.iter().enumerate().find_map(|(index, byte)| {
        (*byte == b'\r' && bytes.get(index + 1) != Some(&b'\n')).then_some(index)
    }) else {
        return Cow::Borrowed(text);
    };

    let mut normalized = bytes.to_vec();
    normalized[first_lone_cr] = b'\n';
    for index in first_lone_cr + 1..normalized.len() {
        if normalized[index] == b'\r' && normalized.get(index + 1) != Some(&b'\n') {
            normalized[index] = b'\n';
        }
    }
    Cow::Owned(String::from_utf8(normalized).expect("replacing ASCII bytes preserves valid UTF-8"))
}

impl PositionMapper<'_> {
    /// Convert LSP Position to byte offset in the document
    pub fn position_to_byte(&self, position: Position) -> Option<usize> {
        // LSP positions are UTF-16 based
        let wide_line_col = WideLineCol {
            line: position.line,
            col: position.character,
        };

        // Convert from UTF-16 position to byte offset
        let line_col = self
            .line_index
            .to_utf8(WideEncoding::Utf16, wide_line_col)?;
        let text_size = self.line_index.offset(line_col)?;
        Some(text_size.into())
    }

    /// Convert an LSP `Position` to a byte offset, clamping out-of-bounds
    /// positions to the nearest valid offset.
    ///
    /// `position_to_byte` does not guard either bound: a `character` past a
    /// line's end yields a byte offset running past that line (it is computed
    /// as `line_start + character`), and a `line` past EOF yields `None`.
    /// Both must be reined in, and *differently*:
    /// - **character past the line's end** → clamp to the end of that line's
    ///   content, before its `\n` or `\r\n` terminator. The request stays
    ///   within its own line and cannot consume the terminator or reach a later
    ///   line's content — unlike snapping to the document end, which would.
    /// - **line past the last line** → clamp to the document's end.
    ///
    /// An in-bounds position maps exactly (identical to `position_to_byte`):
    /// the largest in-bounds offset on a line is its end-of-content, which is
    /// `<= line(l).end()`, so the `min` never alters it.
    pub fn position_to_byte_clamped(&self, position: Position) -> usize {
        match self.line_index.line(position.line) {
            // Line exists: take the mapped offset but clamp it to the line's
            // end so an over-long character can't spill past this line.
            Some(line_range) => {
                let line_start: usize = line_range.start().into();
                let line_range_end: usize = line_range.end().into();
                let line = &self.text[line_start..line_range_end];
                let content = line
                    .strip_suffix('\n')
                    .map(|line| line.strip_suffix('\r').unwrap_or(line))
                    .unwrap_or(line);
                let line_end = line_start + content.len();
                let byte = self
                    .position_to_byte(position)
                    .unwrap_or(line_end)
                    .min(line_end);
                floor_char_boundary(self.text, byte)
            }
            // The line itself is past EOF: clamp to the document end.
            None => self.line_index.len().into(),
        }
    }

    /// Convert an LSP `Position` to a byte offset, returning `None` for any
    /// position that does not address a real location in the document.
    ///
    /// Unlike [`position_to_byte`](Self::position_to_byte) — which, as its own
    /// callers' docs note, spills a `character` past a line's end into a byte on
    /// a *later* line (`line_start + character`) — this validates the input by
    /// requiring the offset to round-trip: `byte_to_position(b) == position`. An
    /// over-long column maps to some `b` whose real position differs from the
    /// requested one, so it is rejected rather than silently resolving the wrong
    /// location. Use this for *client-supplied* positions where an out-of-bounds
    /// column must mean "no such location" (null), not "snap somewhere".
    pub fn position_to_byte_strict(&self, position: Position) -> Option<usize> {
        let byte = self.position_to_byte(position)?;
        (self.byte_to_position(byte)? == position).then_some(byte)
    }

    /// Convert a byte offset to a tree-sitter `Point` (row, byte-column).
    ///
    /// Reuses the precomputed `LineIndex` (O(log n) line lookup) instead of
    /// rescanning the text, and yields a byte-based column — exactly what a
    /// tree-sitter `InputEdit` needs. The offset is clamped to the document
    /// length, so `try_line_col` resolves for every input; the `None` arm is
    /// unreachable and degrades to the document start.
    pub fn byte_to_point(&self, offset: usize) -> tree_sitter::Point {
        let line_index = self.tree_line_index.as_ref().unwrap_or(&self.line_index);
        let len: usize = line_index.len().into();
        let offset = offset.min(len);
        match offset
            .try_into()
            .ok()
            .and_then(|o| line_index.try_line_col(o))
        {
            Some(line_col) => {
                tree_sitter::Point::new(line_col.line as usize, line_col.col as usize)
            }
            None => tree_sitter::Point::new(0, 0),
        }
    }

    /// Convert byte offset to LSP Position
    pub fn byte_to_position(&self, offset: usize) -> Option<Position> {
        // Convert byte offset to LineCol
        let line_col = self.line_index.try_line_col(offset.try_into().ok()?)?;

        // Convert to UTF-16 position for LSP
        let wide_line_col = self.line_index.to_wide(WideEncoding::Utf16, line_col)?;

        Some(Position::new(wide_line_col.line, wide_line_col.col))
    }
}

/// Convert byte column position to UTF-16 column position within a line.
///
/// If the byte position is invalid (mid-character or past end of line), walks
/// back to the nearest valid byte boundary and returns that UTF-16 column
/// instead. This can occur when tree-sitter byte offsets from injection
/// coordinate mapping land inside a multi-byte character boundary.
#[inline]
pub(crate) fn byte_to_utf16_col(line: &str, byte_col: usize) -> usize {
    convert_byte_to_utf16_in_line(line, byte_col).unwrap_or_else(|| {
        let mut valid_col = byte_col;
        while valid_col > 0 {
            valid_col -= 1;
            if let Some(utf16) = convert_byte_to_utf16_in_line(line, valid_col) {
                return utf16;
            }
        }
        0
    })
}

/// Convert byte position to UTF-16 position within a line
/// Returns None if the byte position is invalid (e.g., in the middle of a multi-byte character)
#[inline(always)]
pub fn convert_byte_to_utf16_in_line(line_text: &str, byte_pos: usize) -> Option<usize> {
    // Fast path: when the prefix up to `byte_pos` is pure ASCII, every byte is a
    // single-byte UTF-8 char and a single UTF-16 code unit, so the UTF-16 column
    // equals `byte_pos`. `<[u8]>::is_ascii` is vectorized by the standard library,
    // which beats the per-`char` decode loop on the common all-ASCII source line.
    // Multi-byte prefixes fall through to the exact loop below.
    if byte_pos <= line_text.len() && line_text.as_bytes()[..byte_pos].is_ascii() {
        return Some(byte_pos);
    }

    let mut utf16_offset = 0;
    let mut byte_count = 0;

    for ch in line_text.chars() {
        if byte_count == byte_pos {
            return Some(utf16_offset);
        }
        let ch_bytes = ch.len_utf8();
        if byte_count + ch_bytes > byte_pos {
            // Position is in the middle of a multi-byte character
            return None;
        }
        byte_count += ch_bytes;
        utf16_offset += ch.len_utf16();
    }

    // If we reached the end and the position matches exactly, return the end position
    if byte_count == byte_pos {
        Some(utf16_offset)
    } else {
        // Position is beyond the end of the line
        None
    }
}

/// Per-line byte→UTF-16 column lookup, amortizing the cost of repeated
/// `byte_to_utf16_col` calls against the same line across multiple tokens.
///
/// ASCII lines keep `byte_to_utf16_col`'s fast path (byte offset == UTF-16
/// offset, clamped to the line length) with no allocation. Non-ASCII lines
/// build a sorted `(byte, utf16)` boundary table once, O(line_len), then
/// resolve each query via binary search, O(log line_len) — replacing the
/// O(line_len) per-char decode loop `byte_to_utf16_col` would otherwise
/// re-run from column 0 on every call against that line.
pub(crate) struct Utf16LineIndex {
    line_len: usize,
    // None for ASCII lines (fast path needs no table). Always starts at
    // (0, 0) and ends with (line_len, total_utf16_width) for non-ASCII lines.
    boundaries: Option<Vec<(usize, usize)>>,
}

impl Utf16LineIndex {
    pub(crate) fn new(line: &str) -> Self {
        if line.as_bytes().is_ascii() {
            return Self {
                line_len: line.len(),
                boundaries: None,
            };
        }

        let mut boundaries = Vec::with_capacity(line.len() + 1);
        let mut utf16_offset = 0;
        for (byte, ch) in line.char_indices() {
            boundaries.push((byte, utf16_offset));
            utf16_offset += ch.len_utf16();
        }
        boundaries.push((line.len(), utf16_offset));
        Self {
            line_len: line.len(),
            boundaries: Some(boundaries),
        }
    }

    /// Same semantics as `byte_to_utf16_col`: an out-of-bounds or
    /// mid-codepoint `byte_col` resolves to the nearest valid boundary at or
    /// before it.
    pub(crate) fn byte_to_utf16(&self, byte_col: usize) -> usize {
        let Some(boundaries) = &self.boundaries else {
            return byte_col.min(self.line_len);
        };
        let idx = boundaries.partition_point(|&(b, _)| b <= byte_col);
        boundaries[idx.saturating_sub(1)].1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_to_utf16_col_ascii() {
        let line = "hello world";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 5), 5);
        assert_eq!(byte_to_utf16_col(line, 11), 11);
    }

    #[test]
    fn byte_to_utf16_col_japanese() {
        // Japanese text (3 bytes per char in UTF-8, 1 code unit in UTF-16)
        let line = "こんにちは";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 3), 1); // After "こ"
        assert_eq!(byte_to_utf16_col(line, 6), 2); // After "こん"
        assert_eq!(byte_to_utf16_col(line, 15), 5); // After all 5 chars
    }

    #[test]
    fn byte_to_utf16_col_mixed_ascii_and_japanese() {
        let line = "let x = \"あいうえお\"";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 8), 8); // Before '"'
        assert_eq!(byte_to_utf16_col(line, 9), 9); // Before "あ"
        assert_eq!(byte_to_utf16_col(line, 12), 10); // After "あ" (3 bytes -> 1 UTF-16)
        assert_eq!(byte_to_utf16_col(line, 24), 14); // At closing '"' (after "あいうえお")
    }

    #[test]
    fn byte_to_utf16_col_mid_character_fallback() {
        // Japanese text: each char is 3 bytes in UTF-8
        let line = "こんにちは";
        // byte 1 is mid-"こ" → falls back to 0 (start of "こ")
        assert_eq!(byte_to_utf16_col(line, 1), 0);
        // byte 2 is mid-"こ" → falls back to 0
        assert_eq!(byte_to_utf16_col(line, 2), 0);
        // byte 4 is mid-"ん" → falls back to 1 (start of "ん")
        assert_eq!(byte_to_utf16_col(line, 4), 1);

        // Emoji: 4 bytes in UTF-8
        let line = "a👋b";
        // byte 2 is mid-emoji → falls back to 1 (start of emoji)
        assert_eq!(byte_to_utf16_col(line, 2), 1);
        // byte 3 is mid-emoji → falls back to 1
        assert_eq!(byte_to_utf16_col(line, 3), 1);
    }

    #[test]
    fn utf16_line_index_matches_byte_to_utf16_col_ascii() {
        let line = "hello world";
        let index = Utf16LineIndex::new(line);
        for byte_col in [0, 5, 11] {
            assert_eq!(
                index.byte_to_utf16(byte_col),
                byte_to_utf16_col(line, byte_col)
            );
        }
    }

    #[test]
    fn utf16_line_index_matches_byte_to_utf16_col_japanese() {
        let line = "こんにちは";
        let index = Utf16LineIndex::new(line);
        for byte_col in [0, 3, 6, 15] {
            assert_eq!(
                index.byte_to_utf16(byte_col),
                byte_to_utf16_col(line, byte_col)
            );
        }
    }

    #[test]
    fn utf16_line_index_matches_byte_to_utf16_col_mixed_ascii_and_japanese() {
        let line = "let x = \"あいうえお\"";
        let index = Utf16LineIndex::new(line);
        for byte_col in [0, 8, 9, 12, 24] {
            assert_eq!(
                index.byte_to_utf16(byte_col),
                byte_to_utf16_col(line, byte_col)
            );
        }
    }

    #[test]
    fn utf16_line_index_matches_byte_to_utf16_col_mid_character_fallback() {
        let line = "こんにちは";
        let index = Utf16LineIndex::new(line);
        for byte_col in [1, 2, 4] {
            assert_eq!(
                index.byte_to_utf16(byte_col),
                byte_to_utf16_col(line, byte_col)
            );
        }

        let line = "a👋b";
        let index = Utf16LineIndex::new(line);
        for byte_col in [2, 3] {
            assert_eq!(
                index.byte_to_utf16(byte_col),
                byte_to_utf16_col(line, byte_col)
            );
        }
    }

    #[test]
    fn utf16_line_index_matches_byte_to_utf16_col_out_of_bounds() {
        // Past the end of the line, on both ASCII and non-ASCII content —
        // exercises the "no boundary found" fallback, which clamps to the
        // line's end: byte length for ASCII (byte offset == UTF-16 offset),
        // but the line's UTF-16 WIDTH for non-ASCII (e.g. "こんにちは" is
        // 15 bytes but clamps to UTF-16 column 5).
        for line in ["hello", "こんにちは", "a👋b"] {
            let index = Utf16LineIndex::new(line);
            let past_end = line.len() + 5;
            assert_eq!(
                index.byte_to_utf16(past_end),
                byte_to_utf16_col(line, past_end)
            );
        }
    }

    #[test]
    fn position_to_byte_for_multibyte() {
        // "let x = \"あいう\";" — ASCII prefix then 3 hiragana (3 bytes each)
        let text = "let x = \"あいう\";\n";
        let mapper = PositionMapper::new(text);

        // UTF-16 col 9 → byte 9 (all ASCII up to the opening quote content)
        assert_eq!(
            mapper.position_to_byte(tower_lsp_server::ls_types::Position::new(0, 9)),
            Some(9)
        );
        // UTF-16 col 12 → byte 18 (9 ASCII + 3 hiragana × 3 bytes)
        assert_eq!(
            mapper.position_to_byte(tower_lsp_server::ls_types::Position::new(0, 12)),
            Some(18)
        );
    }

    #[test]
    fn byte_to_point_for_multibyte() {
        // Multi-line, multi-byte: byte→Point must use BYTE columns (tree-sitter),
        // so a hiragana past the prefix lands at byte col 9 (9 ASCII), and the
        // first char of line 1 is row 1 col 0. Covers the UTF-16→byte→Point path
        // now that `byte_to_point` feeds the incremental-parse `InputEdit`.
        let text = "let x = \"あいう\";\nlocal y = 2\n";
        let mapper = PositionMapper::new(text);

        // Start of the first hiragana (byte 9, after `let x = "`).
        let p = mapper.byte_to_point(9);
        assert_eq!((p.row, p.column), (0, 9));
        // Just after the 3 hiragana (9 + 3×3 = byte 18) — still row 0, byte col 18.
        let p = mapper.byte_to_point(18);
        assert_eq!((p.row, p.column), (0, 18));
        // First byte of line 1 (`local`) is row 1, col 0.
        let line1_start = text.find("local").unwrap();
        let p = mapper.byte_to_point(line1_start);
        assert_eq!((p.row, p.column), (1, 0));
    }

    #[test]
    fn lone_carriage_return_starts_a_new_line() {
        let text = "hello\rworld";
        let mapper = PositionMapper::new(text);

        assert_eq!(mapper.position_to_byte(Position::new(1, 0)), Some(6));
        assert_eq!(mapper.byte_to_position(6), Some(Position::new(1, 0)));
        // tree-sitter's Point contract differs: its lexer advances rows only
        // for LF, so a lone CR remains one byte on row 0 there.
        let point = mapper.byte_to_point(6);
        assert_eq!((point.row, point.column), (0, 6));
    }

    #[test]
    fn mixed_line_endings_have_one_boundary_each() {
        let text = "a\rb\r\nc\nd";
        let mapper = PositionMapper::new(text);

        for (line, byte) in [(0, 0), (1, 2), (2, 5), (3, 7)] {
            let position = Position::new(line, 0);
            assert_eq!(mapper.position_to_byte(position), Some(byte));
            assert_eq!(mapper.byte_to_position(byte), Some(position));
        }
        assert_eq!(mapper.position_to_byte(Position::new(4, 0)), None);
    }

    #[test]
    fn clamped_maps_in_bounds_position_exactly() {
        let text = "hello\nworld\n";
        let mapper = PositionMapper::new(text);
        assert_eq!(
            mapper.position_to_byte_clamped(Position::new(1, 2)),
            mapper.position_to_byte(Position::new(1, 2)).unwrap()
        );
    }

    #[test]
    fn clamped_snaps_overlong_character_to_line_end_not_document_end() {
        // LineIndex includes the terminator in line 0's byte range (0..6), but
        // the valid LSP end position is byte 5, before `\n`. A far-past column
        // clamps there, not to the start of line 1 or the document end.
        let text = "hello\nworld\n";
        let mapper = PositionMapper::new(text);
        assert_eq!(mapper.position_to_byte_clamped(Position::new(0, 999)), 5);
    }

    #[test]
    fn clamped_snaps_overlong_character_before_crlf() {
        let text = "hello\r\nworld\r\n";
        let mapper = PositionMapper::new(text);

        assert_eq!(mapper.position_to_byte_clamped(Position::new(0, 999)), 5);
    }

    #[test]
    fn clamped_floors_utf16_position_inside_surrogate_pair() {
        let text = "👋x\n";
        let mapper = PositionMapper::new(text);

        assert_eq!(mapper.position_to_byte_clamped(Position::new(0, 1)), 0);
    }

    #[test]
    fn strict_accepts_in_bounds_and_rejects_overlong_or_eof() {
        let text = "hello\nworld\n";
        let mapper = PositionMapper::new(text);

        // In-bounds positions map exactly, identical to position_to_byte.
        assert_eq!(mapper.position_to_byte_strict(Position::new(0, 0)), Some(0));
        assert_eq!(mapper.position_to_byte_strict(Position::new(0, 5)), Some(5)); // end of "hello"
        assert_eq!(mapper.position_to_byte_strict(Position::new(1, 2)), Some(8));

        // A character past the line's end must NOT spill onto the next line.
        assert_eq!(mapper.position_to_byte_strict(Position::new(0, 999)), None);
        // A line past EOF is rejected too.
        assert_eq!(mapper.position_to_byte_strict(Position::new(99, 0)), None);
    }

    #[test]
    fn strict_rejects_overlong_column_for_multibyte_line() {
        // "あい" is 2 chars / 6 bytes / 2 UTF-16 units on line 0.
        let text = "あい\nx\n";
        let mapper = PositionMapper::new(text);
        // col 2 = end of "あい" (byte 6) is valid.
        assert_eq!(mapper.position_to_byte_strict(Position::new(0, 2)), Some(6));
        // col 5 is past the line's end → None, not a byte on line 1.
        assert_eq!(mapper.position_to_byte_strict(Position::new(0, 5)), None);
    }

    #[test]
    fn clamped_snaps_line_past_eof_to_document_end() {
        let text = "hello\nworld\n";
        let mapper = PositionMapper::new(text);
        assert_eq!(
            mapper.position_to_byte_clamped(Position::new(99, 0)),
            text.len()
        );
    }

    #[test]
    fn byte_to_utf16_col_emoji() {
        // Emoji (4 bytes in UTF-8, 2 code units in UTF-16)
        let line = "hello 👋 world";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 6), 6); // After "hello "
        assert_eq!(byte_to_utf16_col(line, 10), 8); // After emoji (4 bytes -> 2 UTF-16)
    }
}
