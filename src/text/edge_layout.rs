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
/// A whitespace-only `original` or `formatted` has no content between its
/// edges, so it is returned as formatted.
pub(crate) fn restore_edge_layout(original: &str, formatted: &str, opens_mid_line: bool) -> String {
    if original.trim().is_empty() || formatted.trim().is_empty() {
        return formatted.to_string();
    }
    let (mut body, formatted_tail) = formatted.split_at(tail_start(formatted));

    // The formatted text plus the edges it may have lost, usually a few bytes.
    let mut restored = String::with_capacity(formatted.len() + 16);
    if opens_mid_line {
        let original_head = head_lines(original);
        let formatted_head = head_lines(body);
        body = &body[formatted_head.iter().map(|line| line.len()).sum::<usize>()..];
        if formatted_head == original_head {
            // Untouched: kept as is, whatever its line breaks.
            restored.extend(formatted_head);
        } else if let Some((layout, original_blank_lines)) = original_head.split_first() {
            // The original's line break ending the delimiter's line, then the
            // blank lines the formatter kept, no more than the original had,
            // each ending with that line break (see the trailing layout).
            restored.push_str(layout);
            let line_break = layout.trim_start_matches([' ', '\t']);
            for line in formatted_head
                .iter()
                .skip(1)
                .take(original_blank_lines.len())
            {
                restored.push_str(line.trim_end_matches(['\r', '\n']));
                restored.push_str(line_break);
            }
        }
    }
    restored.push_str(body);
    let original_tail = &original[tail_start(original)..];
    if formatted_tail == original_tail {
        // Untouched: kept as is, whatever its line breaks.
        restored.push_str(formatted_tail);
        return restored;
    }
    let original_tail = tail_lines(original_tail);
    if let Some((final_line, original_blank_lines)) = original_tail.split_last() {
        // The blank lines the formatter kept, no more than the original had,
        // then the original's final line break and indentation. Every one
        // ends with the original's line break: the layout keeps one style,
        // and a kept lone CR cannot merge with an LF after it into one CRLF.
        let original_break =
            &final_line[..final_line.len() - final_line.trim_start_matches(['\r', '\n']).len()];
        let formatted_tail = tail_lines(formatted_tail);
        let blank_lines = formatted_tail.len().saturating_sub(1);
        for line in &formatted_tail[..blank_lines.min(original_blank_lines.len())] {
            restored.push_str(original_break);
            restored.push_str(line.trim_start_matches(['\r', '\n']));
        }
        restored.push_str(final_line);
    }
    restored
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
    #[case::kept_blank_lines_take_the_original_line_break(
        "x\r\n\r\n  ",
        "x\n\n",
        true,
        "x\r\n\r\n  "
    )]
    #[case::a_kept_lone_cr_does_not_merge_into_a_crlf("x\n\n", "x\r\r\n", false, "x\n\n")]
    #[case::trailing_spaces_on_the_last_line_are_content("x  \n  ", "x\n", true, "x\n  ")]
    #[case::mixed_trailing_breaks_left_alone("a\r\n\n", "a\r\n\n", false, "a\r\n\n")]
    #[case::mixed_leading_breaks_left_alone("\r\n\na", "\r\n\na", true, "\r\n\na")]
    #[case::lone_cr_tail_left_alone("a\n\r", "a\n\r", false, "a\n\r")]
    #[case::edges_already_match("\n  x\n  ", "\n  y;\n  ", true, "\n  y;\n  ")]
    #[case::whitespace_only_original("\n  ", "", true, "")]
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
