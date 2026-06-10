use line_index::{LineIndex, WideEncoding, WideLineCol};
use tower_lsp_server::ls_types::Position;

/// Position mapper for converting between LSP positions and byte offsets
pub struct PositionMapper {
    line_index: LineIndex,
}

impl PositionMapper {
    /// Create a new PositionMapper with pre-computed line starts
    pub fn new(text: &str) -> Self {
        let line_index = LineIndex::new(text);
        Self { line_index }
    }
}

impl PositionMapper {
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
    /// - **character past the line's end** → clamp to the end of that line
    ///   (`line(l).end()`, which is the start of the next line, i.e. just
    ///   past this line's terminator). The request stays within its own line
    ///   and never reaches a later line's content or injection region —
    ///   unlike snapping to the document end, which would.
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
                let line_end: usize = line_range.end().into();
                self.position_to_byte(position)
                    .unwrap_or(line_end)
                    .min(line_end)
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
        let len: usize = self.line_index.len().into();
        let offset = offset.min(len);
        match offset
            .try_into()
            .ok()
            .and_then(|o| self.line_index.try_line_col(o))
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
        // Line 0 is "hello\n" (bytes 0..6); `line(0).end()` is byte 6, the
        // start of line 1. A character far past the line end clamps there —
        // within line 0's bounds, NOT the document end (12) — so a
        // single-line range can never spill into later lines.
        let text = "hello\nworld\n";
        let mapper = PositionMapper::new(text);
        assert_eq!(mapper.position_to_byte_clamped(Position::new(0, 999)), 6);
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
