//! Keeping a virtual document's boundary layout through formatting.
//!
//! A virtual document spans its host region exactly, so its edges are the
//! host's layout, not the embedded language's: the line break right after a
//! Nix `''`, and the final line break plus the indentation before a closing
//! `''`. A formatter treating the document as a file of its own strips them
//! (`}''`, `''{`), so they are put back before the result is mapped to the
//! host.

/// `formatted` with the boundary layout of the `original` it was formatted
/// from: the leading line break, if `original` opens with one, and its final
/// line break with the spaces or tabs after it, or no final line break when
/// `original` has none.
///
/// Only those are layout. The blank lines before the final line break are
/// the formatter's, so a formatter may trim them (at the end of a markdown
/// fence, say), though not add more than `original` had. A blank text either
/// side is left to the formatter.
pub(crate) fn restore_boundary_layout(original: &str, formatted: &str) -> String {
    if original.trim().is_empty() || formatted.trim().is_empty() {
        return formatted.to_string();
    }
    let leading = leading_line_break(original);
    let original_tail = Tail::of(original);
    let formatted_tail = Tail::of(formatted);
    let body = &formatted[..formatted_tail.start];
    let mut restored =
        String::with_capacity(formatted.len() + original.len() - original_tail.start);
    if !leading.is_empty() && leading_line_break(body).is_empty() {
        restored.push_str(leading);
    }
    restored.push_str(body);
    if let Some((line_break, indentation)) = original_tail.last_line {
        // A formatter reads the indentation before a closing delimiter as a
        // last line of its own, and may end it with a line break: counted
        // here, that break is not a blank line.
        let blank_lines = formatted_tail
            .line_breaks
            .saturating_sub(1)
            .min(original_tail.line_breaks - 1);
        for _ in 0..=blank_lines {
            restored.push_str(line_break);
        }
        restored.push_str(indentation);
    }
    restored
}

/// The line break `text` opens with, if any.
fn leading_line_break(text: &str) -> &str {
    if text.starts_with("\r\n") {
        &text[..2]
    } else if text.starts_with(['\n', '\r']) {
        &text[..1]
    } else {
        ""
    }
}

/// The blank tail of a text: the line breaks after its last line with
/// content, and the spaces or tabs after the last of them.
struct Tail<'a> {
    /// Where the tail starts: the first line break after the last content
    /// (spaces or tabs ending that line stay with it).
    start: usize,
    line_breaks: usize,
    /// The last line break and the spaces or tabs after it, when there is a
    /// line break.
    last_line: Option<(&'a str, &'a str)>,
}

impl<'a> Tail<'a> {
    fn of(text: &'a str) -> Self {
        let is_blank = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r');
        let content_end = text.trim_end_matches(is_blank).len();
        let start = text[content_end..]
            .find(['\n', '\r'])
            .map_or(text.len(), |offset| content_end + offset);
        let tail = &text[start..];
        let line_breaks =
            tail.matches('\n').count() + tail.matches('\r').count() - tail.matches("\r\n").count();
        let last_line = tail.rfind(['\n', '\r']).map(|last| {
            let break_start = if tail[..=last].ends_with("\r\n") {
                last - 1
            } else {
                last
            };
            (&tail[break_start..=last], &tail[last + 1..])
        });
        Self {
            start,
            line_breaks,
            last_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_break_after_an_opening_delimiter_is_kept() {
        // shfmt drops the empty first line after Nix `''`.
        assert_eq!(restore_boundary_layout("\n  echo\n", "echo\n"), "\necho\n");
    }

    #[test]
    fn the_indentation_before_a_closing_delimiter_is_kept() {
        // jsonls strips "\n    " before Nix `''`, leaving `}''`.
        assert_eq!(
            restore_boundary_layout("\n{\n  \"a\": 1\n}\n    ", "{\n  \"a\": 1\n}"),
            "\n{\n  \"a\": 1\n}\n    "
        );
    }

    #[test]
    fn an_added_final_line_break_is_not_doubled() {
        assert_eq!(restore_boundary_layout("\n{}\n    ", "{}\n"), "\n{}\n    ");
    }

    #[test]
    fn trailing_blank_lines_a_formatter_trims_stay_trimmed() {
        // Only the final line break is layout: extra blank lines before a
        // markdown fence's closing line are the formatter's to remove.
        assert_eq!(restore_boundary_layout("code\n\n\n", "code\n"), "code\n");
    }

    #[test]
    fn a_line_break_inserted_after_the_closing_indentation_is_not_doubled() {
        // A formatter sees the indentation before Nix's closing `''` as a
        // last line lacking its line break, and inserts one after it.
        assert_eq!(
            restore_boundary_layout("\nX\n    ", "\nX\n    \n"),
            "\nX\n    "
        );
    }

    #[test]
    fn trailing_blank_lines_a_formatter_keeps_stay_kept() {
        assert_eq!(
            restore_boundary_layout("code\n\n\n", "code\n\n\n"),
            "code\n\n\n"
        );
    }

    #[test]
    fn a_formatter_cannot_add_trailing_blank_lines_into_the_layout() {
        assert_eq!(restore_boundary_layout("\nX\n    ", "X\n\n\n"), "\nX\n    ");
    }

    #[test]
    fn a_text_without_a_final_line_break_gains_none() {
        // `''a''` on one line: a final line break would move the closing
        // delimiter onto the next line and change the string.
        assert_eq!(restore_boundary_layout("a\nb", "a\nb\n"), "a\nb");
    }

    #[test]
    fn crlf_boundaries_are_kept_as_they_were() {
        assert_eq!(restore_boundary_layout("\r\n  a\r\n  ", "a"), "\r\na\r\n  ");
    }

    #[test]
    fn a_formatted_text_with_the_same_boundaries_is_unchanged() {
        assert_eq!(restore_boundary_layout("\n a\n  ", "\nb\n  "), "\nb\n  ");
    }

    #[test]
    fn blank_texts_are_left_to_the_formatter() {
        assert_eq!(restore_boundary_layout("\n  ", ""), "");
        assert_eq!(restore_boundary_layout("\na\n  ", "\n"), "\n");
    }
}
