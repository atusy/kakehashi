//! Lone carriage returns: line ends the LSP position model has and
//! tree-sitter rows lack.

/// Byte offsets of every lone CR (a CR not followed by LF) in a text.
///
/// The LSP position model, like [`super::PositionMapper`], ends a line at a
/// lone CR, while tree-sitter rows split only at LF. Built once per document,
/// it turns a tree-sitter position into an LSP one with a binary search, and
/// a document without lone CRs, the usual case, costs one scan for `\r`.
#[derive(Debug)]
pub(crate) struct LoneCarriageReturns(Vec<usize>);

impl LoneCarriageReturns {
    pub(crate) fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        Self(
            text.match_indices('\r')
                .map(|(index, _)| index)
                .filter(|&index| bytes.get(index + 1) != Some(&b'\n'))
                .collect(),
        )
    }

    /// The number of lone CRs before `byte`: how many more LSP lines than
    /// tree-sitter rows precede it.
    fn count_before(&self, byte: usize) -> usize {
        self.0.partition_point(|&cr| cr < byte)
    }

    /// The LSP (line, UTF-16 column) of `byte`, given its tree-sitter row and
    /// UTF-16 column.
    pub(crate) fn lsp_position(
        &self,
        text: &str,
        byte: usize,
        tree_row: u32,
        tree_column: u32,
    ) -> (u32, u32) {
        let before = self.count_before(byte);
        if before == 0 {
            return (tree_row, tree_column);
        }
        // A lone CR precedes `byte`, so it may also start `byte`'s LSP line:
        // measure the column from the nearest line end, an LF or a lone CR.
        // The scan is bounded by that LSP line's length.
        let byte = text.floor_char_boundary(byte.min(text.len()));
        let bytes = text.as_bytes();
        let mut search_end = byte;
        let line_start = loop {
            match text[..search_end].rfind(['\n', '\r']) {
                // The CR of a CRLF ends no line by itself; `byte` is its LF.
                Some(index) if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') => {
                    search_end = index;
                }
                found => break found.map_or(0, |index| index + 1),
            }
        };
        let column = text[line_start..byte].encode_utf16().count() as u32;
        let line = tree_row.saturating_add(u32::try_from(before).unwrap_or(u32::MAX));
        (line, column)
    }

    /// The LSP line of `byte`, given its tree-sitter row.
    pub(crate) fn lsp_line(&self, byte: usize, tree_row: u32) -> u32 {
        let before = self.count_before(byte);
        tree_row.saturating_add(u32::try_from(before).unwrap_or(u32::MAX))
    }

    /// Whether `byte` starts an LSP line (column 0).
    pub(crate) fn starts_lsp_line(&self, text: &str, byte: usize) -> bool {
        let Some(previous) = byte.checked_sub(1) else {
            return true;
        };
        text.as_bytes().get(previous) == Some(&b'\n') || self.0.binary_search(&previous).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::PositionMapper;
    use rstest::rstest;

    /// Tree-sitter rows count only LF; columns are UTF-16 widths from the
    /// last LF.
    fn tree_position(text: &str, byte: usize) -> (u32, u32) {
        let row = text[..byte].matches('\n').count() as u32;
        let line_start = text[..byte].rfind('\n').map_or(0, |index| index + 1);
        (row, text[line_start..byte].encode_utf16().count() as u32)
    }

    #[rstest]
    #[case::no_cr("ab\ncd\nef", 7)]
    #[case::crlf("ab\r\ncd\r\nef", 9)]
    #[case::lone_cr_on_an_earlier_row("ab\rcd\nef", 7)]
    #[case::lone_cr_earlier_on_the_same_row("ab\ncd\ref", 7)]
    #[case::right_after_a_lone_cr("ab\rcd", 3)]
    #[case::at_a_lone_cr("ab\rcd", 2)]
    #[case::non_ascii_after_a_lone_cr("ab\r\u{3042}\u{3044}x", 9)]
    #[case::mixed_terminators("a\rb\r\nc\nd\re", 10)]
    #[case::inside_a_crlf_after_a_lone_cr("a\rb\r\nc", 4)]
    fn lsp_position_matches_position_mapper(#[case] text: &str, #[case] byte: usize) {
        let (tree_row, tree_column) = tree_position(text, byte);
        let expected = PositionMapper::new(text)
            .byte_to_position(byte)
            .expect("in bounds");

        let lone_crs = LoneCarriageReturns::new(text);

        assert_eq!(
            lone_crs.lsp_position(text, byte, tree_row, tree_column),
            (expected.line, expected.character)
        );
        assert_eq!(lone_crs.lsp_line(byte, tree_row), expected.line);
        assert_eq!(
            lone_crs.starts_lsp_line(text, byte),
            expected.character == 0
        );
    }
}
