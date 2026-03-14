use line_index::{LineIndex, WideEncoding, WideLineCol};
use tower_lsp_server::ls_types::{Position, Range};

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

    /// Convert byte offset to LSP Position
    pub fn byte_to_position(&self, offset: usize) -> Option<Position> {
        // Convert byte offset to LineCol
        let line_col = self.line_index.try_line_col(offset.try_into().ok()?)?;

        // Convert to UTF-16 position for LSP
        let wide_line_col = self.line_index.to_wide(WideEncoding::Utf16, line_col)?;

        Some(Position::new(wide_line_col.line, wide_line_col.col))
    }

    /// Convert byte range to LSP Range
    pub fn byte_range_to_range(&self, start: usize, end: usize) -> Option<Range> {
        let start_pos = self.byte_to_position(start)?;
        let end_pos = self.byte_to_position(end)?;
        Some(Range::new(start_pos, end_pos))
    }

    /// Convert LSP Position to tree-sitter Point with proper byte column
    ///
    /// LSP Position uses UTF-16 code units for the character field.
    /// Tree-sitter Point uses byte offsets for the column field.
    /// This method performs the correct conversion.
    pub fn position_to_point(&self, position: Position) -> Option<tree_sitter::Point> {
        // First get the byte offset for this position
        let byte_offset = self.position_to_byte(position)?;

        // Then get the LineCol (which has byte-based column) from the byte offset
        let line_col = self.line_index.try_line_col(byte_offset.try_into().ok()?)?;

        Some(tree_sitter::Point::new(
            line_col.line as usize,
            line_col.col as usize,
        ))
    }
}

/// Convert UTF-16 position to byte position within a line
/// Returns None if the UTF-16 position is invalid
#[inline(always)]
pub fn convert_utf16_to_byte_in_line(line_text: &str, utf16_pos: usize) -> Option<usize> {
    let mut byte_offset = 0;
    let mut utf16_offset = 0;

    for ch in line_text.chars() {
        if utf16_offset >= utf16_pos {
            return Some(byte_offset);
        }
        utf16_offset += ch.len_utf16();
        byte_offset += ch.len_utf8();
    }

    // If we reached the end and the position matches exactly, return the end position
    if utf16_offset == utf16_pos {
        Some(byte_offset)
    } else {
        // Position is beyond the end of the line
        None
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
    fn utf16_to_byte_column_for_multibyte() {
        // "あいうx" — each hiragana is 3 bytes UTF-8, 1 code unit UTF-16
        // UTF-16 col 3 (after 3 hiragana) → byte col 9
        let text = "あいうx\n";
        let mapper = PositionMapper::new(text);
        let point = mapper
            .position_to_point(tower_lsp_server::ls_types::Position::new(0, 3))
            .expect("valid position");
        assert_eq!(point.row, 0);
        assert_eq!(point.column, 9);
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
    fn byte_to_utf16_col_emoji() {
        // Emoji (4 bytes in UTF-8, 2 code units in UTF-16)
        let line = "hello 👋 world";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 6), 6); // After "hello "
        assert_eq!(byte_to_utf16_col(line, 10), 8); // After emoji (4 bytes -> 2 UTF-16)
    }
}
