//! Terminal-safe rendering for untrusted CLI fields.

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
