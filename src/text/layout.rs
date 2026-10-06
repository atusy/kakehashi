//! Keeping a virtual document's boundary layout through formatting.
//!
//! A virtual document spans its host region exactly, so its edges are the
//! host's layout, not the embedded language's: the line break right after a
//! Nix `''`, and the final line break plus the indentation before a closing
//! `''`. A formatter treating the document as a file of its own strips them
//! (`}''`, `''{`), so they are put back before the result is mapped to the
//! host.

/// `formatted` with the boundary layout of the `original` it was formatted
/// from: the leading line break, if `original` opens with one, and whatever
/// follows `original`'s last line break when that is only whitespace (the
/// final line break and the indentation after it), or no final line break
/// at all when `original` has none.
///
/// Only those are layout. Blank lines a formatter removes before the final
/// line break (at the end of a markdown fence, say) stay removed, and a blank
/// text either side is left to the formatter.
pub(crate) fn restore_boundary_layout(original: &str, formatted: &str) -> String {
    if original.trim().is_empty() || formatted.trim().is_empty() {
        return formatted.to_string();
    }
    let leading = leading_line_break(original);
    let tail = &original[tail_start(original)..];
    let body = &formatted[..tail_start(formatted)];
    let mut restored = String::with_capacity(leading.len() + body.len() + tail.len());
    if !leading.is_empty() && leading_line_break(body).is_empty() {
        restored.push_str(leading);
    }
    restored.push_str(body);
    restored.push_str(tail);
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

/// Where `text`'s trailing layout starts: its last line break, when only
/// spaces or tabs follow it; `text.len()` when something else does, or
/// there is no line break.
fn tail_start(text: &str) -> usize {
    let Some(last_break) = text.rfind(['\n', '\r']) else {
        return text.len();
    };
    if !text[last_break + 1..]
        .chars()
        .all(|c| c == ' ' || c == '\t')
    {
        return text.len();
    }
    if text[..last_break + 1].ends_with("\r\n") {
        last_break - 1
    } else {
        last_break
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
