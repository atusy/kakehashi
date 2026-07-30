//! Terminal-safe rendering for untrusted CLI fields.

/// Visibly escape terminal controls while keeping real line breaks intact.
///
/// Use this where the text is meant to be read across several lines — a TOML
/// parse error draws a caret diagram under the offending source line, and
/// collapsing it costs more than the escaping saves. Line-oriented output,
/// where a stray newline would break the one-record-per-line contract, wants
/// [`escape_terminal_controls`] instead.
pub(crate) fn escape_terminal_controls_keeping_newlines(text: &str) -> String {
    text.split('\n')
        .map(|line| escape_terminal_controls(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Visibly escape terminal, bidirectional, and Unicode line-separator
/// characters without changing ordinary text.
pub(crate) fn escape_terminal_controls(text: &str) -> std::borrow::Cow<'_, str> {
    if !text
        .chars()
        .any(|ch| ch.is_control() || is_bidi_control(ch) || is_line_separator(ch))
    {
        return std::borrow::Cow::Borrowed(text);
    }

    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if is_bidi_control(ch) || is_line_separator(ch) {
            escaped.extend(ch.escape_unicode());
        } else if ch.is_control() {
            escaped.extend(ch.escape_default());
        } else {
            escaped.push(ch);
        }
    }
    std::borrow::Cow::Owned(escaped)
}

pub(crate) fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

pub(crate) fn is_line_separator(ch: char) -> bool {
    matches!(ch, '\u{2028}' | '\u{2029}')
}
