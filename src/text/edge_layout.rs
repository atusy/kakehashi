//! Keep an injection region's edge layout through formatting.
//!
//! A virtual document spans its host region exactly, so the line breaks at its
//! edges belong to the host's layout: the line break that ends a delimiter's
//! line (Nix `''`, Lua `[[`) and the final line break with the indentation of
//! the closing delimiter (or the line break before a closing fence).
//! Formatters treat the document as a file of their own and normalize those
//! edges.

/// Return `formatted` with the edge layout of `original` restored.
///
/// - Leading: when the region opens mid-line, after a delimiter, its leading
///   line break with any spaces or tabs before it (or the absence of one) is
///   kept. Blank lines after it stay the formatter's to trim, but no more of
///   them than `original` had. A region opening at a line start has none of
///   the host's: a leading line break there is a blank line of content, the
///   formatter's to keep or remove.
/// - Trailing: the final line break and the whitespace after it are kept, as
///   is the absence of a final line break. Blank lines before it stay the
///   formatter's to trim, but no more of them than `original` had.
///
/// A whitespace-only `original` is all layout (an empty Nix `''` string's
/// line breaks), so it is kept as it is. A whitespace-only `formatted` has
/// no content between its edges to restore them around, so it is returned
/// as formatted.
pub(crate) fn restore_edge_layout(original: &str, formatted: &str, opens_mid_line: bool) -> String {
    restore_edges(original, formatted, opens_mid_line).apply(formatted)
}

/// How [`restore_edge_layout`] changes a formatted text: its first
/// `head_end` bytes become `head`, and its bytes from `tail_start` on become
/// `tail`, each narrowed to what differs. An edge left as formatted replaces
/// nothing with nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EdgeRestore {
    pub(crate) head_end: usize,
    pub(crate) head: String,
    pub(crate) tail_start: usize,
    pub(crate) tail: String,
}

impl EdgeRestore {
    fn unchanged(formatted: &str) -> Self {
        Self {
            head_end: 0,
            head: String::new(),
            tail_start: formatted.len(),
            tail: String::new(),
        }
    }

    pub(crate) fn apply(&self, formatted: &str) -> String {
        let body = &formatted[self.head_end..self.tail_start];
        let mut restored = String::with_capacity(self.head.len() + body.len() + self.tail.len());
        restored.push_str(&self.head);
        restored.push_str(body);
        restored.push_str(&self.tail);
        restored
    }
}

/// The [`EdgeRestore`] that [`restore_edge_layout`] applies.
pub(crate) fn restore_edges(original: &str, formatted: &str, opens_mid_line: bool) -> EdgeRestore {
    let mut edges = EdgeRestore::unchanged(formatted);
    if original.trim().is_empty() {
        edges.head_end = formatted.len();
        edges.head = original.to_string();
        return narrowed(edges, formatted);
    }
    if formatted.trim().is_empty() {
        return edges;
    }
    let formatted_tail_start = tail_start(formatted);

    if opens_mid_line {
        let original_head = head_lines(original);
        let formatted_head = head_lines(&formatted[..formatted_tail_start]);
        // Untouched, the head is kept as is, whatever its line breaks.
        if formatted_head != original_head {
            edges.head_end = formatted_head.iter().map(|line| line.len()).sum();
            if let Some((layout, original_blank_lines)) = original_head.split_first() {
                // The original's line break ending the delimiter's line, then
                // the blank lines the formatter kept, no more than the
                // original had.
                edges.head.push_str(layout);
                for line in formatted_head
                    .iter()
                    .skip(1)
                    .take(original_blank_lines.len())
                {
                    push_line(&mut edges.head, line);
                }
            }
        }
    }

    let original_tail = &original[tail_start(original)..];
    let formatted_tail = &formatted[formatted_tail_start..];
    // Untouched, the tail is kept as is, whatever its line breaks.
    if formatted_tail != original_tail {
        edges.tail_start = formatted_tail_start;
        let original_tail = tail_lines(original_tail);
        if let Some((final_line, original_blank_lines)) = original_tail.split_last() {
            // The blank lines the formatter kept, no more than the original
            // had, then the original's final line break and indentation.
            let formatted_tail = tail_lines(formatted_tail);
            let blank_lines = formatted_tail.len().saturating_sub(1);
            for line in &formatted_tail[..blank_lines.min(original_blank_lines.len())] {
                push_line(&mut edges.tail, line);
            }
            push_line(&mut edges.tail, final_line);
        }
    }

    narrowed(edges, formatted)
}

/// `edges` with each edge replacing only what differs from the formatter's
/// own text: the rest stays formatted text, so the formatter's edits there
/// are kept.
fn narrowed(mut edges: EdgeRestore, formatted: &str) -> EdgeRestore {
    // Neither boundary splits a CRLF of the formatted text: a position
    // between its CR and LF names no column.
    let splits_crlf =
        |at: usize| formatted[..at].ends_with('\r') && formatted[at..].starts_with('\n');
    let mut same_suffix = common_len(
        edges.head.chars().rev(),
        formatted[..edges.head_end].chars().rev(),
    );
    if splits_crlf(edges.head_end - same_suffix) {
        same_suffix -= 1;
    }
    edges.head_end -= same_suffix;
    edges.head.truncate(edges.head.len() - same_suffix);
    let mut same_prefix = common_len(edges.tail.chars(), formatted[edges.tail_start..].chars());
    if splits_crlf(edges.tail_start + same_prefix) {
        same_prefix -= 1;
    }
    edges.tail_start += same_prefix;
    edges.tail.drain(..same_prefix);
    edges
}

/// The byte length of the longest run `a` and `b` share from their start.
fn common_len(a: impl Iterator<Item = char>, b: impl Iterator<Item = char>) -> usize {
    a.zip(b)
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum()
}

/// Append a whitespace-only `line` of the layout, its line breaks as they
/// are. Pieces of two texts meet here: should a lone CR meet an LF, an LF
/// goes between them, as one CRLF would read as a single line break.
fn push_line(restored: &mut String, line: &str) {
    if restored.ends_with('\r') && line.starts_with('\n') {
        restored.push('\n');
    }
    restored.push_str(line);
}

/// The rest of the delimiter's line that `text` opens with: spaces or tabs
/// and the line break after them, or `""` when no line break follows them
/// (they then indent content).
fn leading_layout(text: &str) -> &str {
    let rest = text.trim_start_matches([' ', '\t']);
    let blank = text.len() - rest.len();
    let line_break = if rest.starts_with("\r\n") {
        2
    } else if rest.starts_with(['\n', '\r']) {
        1
    } else {
        return "";
    };
    &text[..blank + line_break]
}

/// The whitespace-only lines `text` opens with, each ending with its line
/// break: the rest of the delimiter's line, then blank lines.
fn head_lines(mut text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    loop {
        let line = leading_layout(text);
        if line.is_empty() {
            return lines;
        }
        lines.push(line);
        text = &text[line.len()..];
    }
}

/// Where the trailing layout of `text` starts: the first line break of its
/// trailing whitespace, or `text.len()` when that whitespace has none.
/// Whitespace before that line break ends the last content line, so it is
/// content.
fn tail_start(text: &str) -> usize {
    let whitespace_start = text.trim_end_matches([' ', '\t', '\r', '\n']).len();
    text[whitespace_start..]
        .find(['\r', '\n'])
        .map_or(text.len(), |index| whitespace_start + index)
}

/// The lines of a trailing layout, each a line break (LF, CRLF or a lone CR)
/// and the whitespace after it.
fn tail_lines(tail: &str) -> Vec<&str> {
    let bytes = tail.as_bytes();
    let starts: Vec<usize> = (0..bytes.len())
        .filter(|&index| match bytes[index] {
            b'\r' => true,
            b'\n' => index == 0 || bytes[index - 1] != b'\r',
            _ => false,
        })
        .collect();
    let ends = starts.iter().skip(1).copied().chain([tail.len()]);
    starts
        .iter()
        .zip(ends)
        .map(|(&start, end)| &tail[start..end])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::closing_indentation_stripped("\n    {\n    }\n  ", "{\n}", true, "\n{\n}\n  ")]
    #[case::leading_and_closing_indentation_lost("\n    x\n  ", "x\n", true, "\nx\n  ")]
    #[case::final_line_break_before_a_fence("{}\n", "{}", false, "{}\n")]
    #[case::no_final_line_break_added("\nx", "x\n", true, "\nx")]
    #[case::no_leading_line_break_added("x", "\nx", true, "x")]
    #[case::spaces_before_the_leading_line_break("  \n  x\n  ", "x\n", true, "  \nx\n  ")]
    #[case::spaces_before_the_leading_line_break_kept("  \n  x\n  ", "  \nx\n", true, "  \nx\n  ")]
    #[case::leading_indentation_is_content("x", "  x", true, "  x")]
    #[case::no_leading_blank_lines_added("x", "\n\nx", true, "x")]
    #[case::no_leading_blank_lines_added_after_the_line_break("\n  x", "\n\nx", true, "\nx")]
    #[case::kept_leading_blank_line("\n\n  x", "\n\nx", true, "\n\nx")]
    #[case::trimmed_leading_blank_line("\n\n  x", "x", true, "\nx")]
    #[case::leading_blank_line_at_a_line_start_is_content("\nx\n", "x\n", false, "x\n")]
    #[case::trimmed_trailing_blank_lines_stay_trimmed("x\n\n\n", "x\n", false, "x\n")]
    #[case::kept_trailing_blank_line("x\n\n  ", "x\n\n", true, "x\n\n  ")]
    #[case::no_trailing_blank_lines_added("x\n", "x\n\n", false, "x\n")]
    #[case::closing_indentation_read_as_a_line_not_doubled("x\n  ", "x\n  \n", true, "x\n  ")]
    #[case::crlf_edges("\r\n  x\r\n  ", "x\n", true, "\r\nx\r\n  ")]
    #[case::kept_blank_lines_keep_their_line_breaks("x\r\n\r\n  ", "x\n\n", true, "x\n\r\n  ")]
    #[case::untouched_blank_line_breaks_stay("x\r\n  \n", "x\r\n\n", false, "x\r\n\n")]
    #[case::a_kept_lone_cr_does_not_merge_into_a_crlf("x\n\n", "x\r\r\n", false, "x\r\n\n")]
    #[case::a_lone_cr_layout_does_not_merge_into_a_crlf("\r\r  x", "\n\nx", true, "\r\n\nx")]
    #[case::trailing_spaces_on_the_last_line_are_content("x  \n  ", "x\n", true, "x\n  ")]
    #[case::mixed_trailing_breaks_left_alone("a\r\n\n", "a\r\n\n", false, "a\r\n\n")]
    #[case::mixed_leading_breaks_left_alone("\r\n\na", "\r\n\na", true, "\r\n\na")]
    #[case::lone_cr_tail_left_alone("a\n\r", "a\n\r", false, "a\n\r")]
    #[case::edges_already_match("\n  x\n  ", "\n  y;\n  ", true, "\n  y;\n  ")]
    #[case::whitespace_only_original_is_all_layout("\n  ", "", true, "\n  ")]
    #[case::whitespace_only_formatted("\n  x\n  ", "\n", true, "\n")]
    fn restores_edge_layout(
        #[case] original: &str,
        #[case] formatted: &str,
        #[case] opens_mid_line: bool,
        #[case] expected: &str,
    ) {
        assert_eq!(
            restore_edge_layout(original, formatted, opens_mid_line),
            expected
        );
    }
}
