//! Code-only view of Lua source for the lightweight parsers.lua reader.
//!
//! The reader finds structure with brace counting and regexes, which only
//! work if braces, quotes and keys inside string literals and comments are
//! out of the way. [`LuaCode`] blanks those while keeping every byte offset,
//! so a position found in the blanked text is valid in the source too, and
//! keeps each string's decoded value for reading field values.

use std::collections::HashMap;
use std::ops::Range;

use super::MetadataError;

/// Lua source with comments and string contents blanked out.
pub(super) struct LuaCode {
    /// Same length as the source. Comments and string contents are spaces,
    /// newlines kept; string delimiters and all code are unchanged.
    pub(super) code: String,
    /// Decoded value of each string literal, keyed by the offset of its
    /// opening delimiter.
    strings: HashMap<usize, String>,
}

impl LuaCode {
    pub(super) fn new(source: &str) -> Result<Self, MetadataError> {
        let Lexed { blanked, strings } = lex(source.as_bytes())?;
        Ok(Self {
            code: blank(source, &blanked),
            strings,
        })
    }

    /// The value of the string literal starting at `offset`, if one does.
    pub(super) fn string_at(&self, offset: usize) -> Option<&str> {
        self.strings.get(&offset).map(String::as_str)
    }
}

struct Lexed {
    /// Byte ranges covering comments and string contents, in source order.
    blanked: Vec<Range<usize>>,
    strings: HashMap<usize, String>,
}

fn lex(b: &[u8]) -> Result<Lexed, MetadataError> {
    let mut ranges = Vec::new();
    let mut strings = HashMap::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let end = if let Some(level) = long_bracket_level(b, i + 2) {
                    let close = long_bracket_close(b, i + 2 + level + 2, level)
                        .ok_or_else(|| unterminated("comment", i))?;
                    close + level + 2
                } else {
                    b[i..]
                        .iter()
                        .position(|&c| matches!(c, b'\n' | b'\r'))
                        .map_or(b.len(), |n| i + n)
                };
                ranges.push(i..end);
                i = end;
            }
            quote @ (b'"' | b'\'') => {
                let close =
                    short_string_close(b, i + 1, quote).ok_or_else(|| unterminated("string", i))?;
                ranges.push(i + 1..close);
                strings.insert(i, decode_short_string(&b[i + 1..close]));
                i = close + 1;
            }
            b'[' => {
                if let Some(level) = long_bracket_level(b, i) {
                    let open_end = i + level + 2;
                    let close = long_bracket_close(b, open_end, level)
                        .ok_or_else(|| unterminated("string", i))?;
                    ranges.push(open_end..close);
                    strings.insert(i, long_string_value(&b[open_end..close]));
                    i = close + level + 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Ok(Lexed {
        blanked: ranges,
        strings,
    })
}

fn unterminated(what: &str, at: usize) -> MetadataError {
    MetadataError::ParseError(format!("unterminated {what} at byte {at}"))
}

/// The level of a long bracket (`[[` is 0, `[==[` is 2) opening at `i`.
fn long_bracket_level(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let level = b[i + 1..].iter().take_while(|&&c| c == b'=').count();
    (b.get(i + 1 + level) == Some(&b'[')).then_some(level)
}

/// Offset of the `]` starting the first closing long bracket of `level`
/// at or after `from`.
fn long_bracket_close(b: &[u8], from: usize, level: usize) -> Option<usize> {
    (from..b.len()).find(|&j| {
        b[j] == b']'
            && b.get(j + 1..j + 1 + level)
                .is_some_and(|eqs| eqs.iter().all(|&c| c == b'='))
            && b.get(j + 1 + level) == Some(&b']')
    })
}

/// Offset of the quote closing a short string whose contents start at
/// `from`. A raw line break outside an escape, or the end of input, leaves
/// the string unterminated, as in Lua.
fn short_string_close(b: &[u8], from: usize, quote: u8) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        match b[j] {
            b'\\' => j = escape_end(b, j),
            b'\n' | b'\r' => return None,
            c if c == quote => return Some(j),
            _ => j += 1,
        }
    }
    None
}

/// Offset just past the escape whose backslash is at `j`, for finding the
/// string end: `\z` spans the whitespace after it, and a line break pair
/// (CR LF or LF CR) counts as one line break.
fn escape_end(b: &[u8], j: usize) -> usize {
    match b.get(j + 1) {
        Some(b'z') => j + 2 + b[j + 2..].iter().take_while(|&&c| is_lua_space(c)).count(),
        Some(&c @ (b'\n' | b'\r')) => {
            let pair = b
                .get(j + 2)
                .is_some_and(|&d| d != c && matches!(d, b'\n' | b'\r'));
            j + 2 + usize::from(pair)
        }
        _ => j + 2,
    }
}

/// Whitespace as Lua's lexer sees it (C `isspace`).
fn is_lua_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Decode the escape sequences of a short string's contents, as Lua 5.4
/// does. An escape Lua would reject, or a `\u{...}` that is not a Rust
/// `char` (a surrogate or above `10FFFF`), is kept as written.
fn decode_short_string(body: &[u8]) -> String {
    let mut out = Vec::with_capacity(body.len());
    let mut j = 0;
    while j < body.len() {
        if body[j] != b'\\' {
            out.push(body[j]);
            j += 1;
            continue;
        }
        let Some(&c) = body.get(j + 1) else {
            out.push(b'\\');
            break;
        };
        j += 2;
        match c {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'\\' | b'"' | b'\'' => out.push(c),
            b'\n' | b'\r' => {
                // An escaped line break is one newline, even as `\r\n`.
                out.push(b'\n');
                if body
                    .get(j)
                    .is_some_and(|&d| d != c && matches!(d, b'\n' | b'\r'))
                {
                    j += 1;
                }
            }
            b'z' => {
                while body.get(j).is_some_and(|&d| is_lua_space(d)) {
                    j += 1;
                }
            }
            b'x' => match body.get(j..j + 2).and_then(|h| parse_hex(h, 2)) {
                Some(byte) => {
                    out.push(byte);
                    j += 2;
                }
                None => out.extend_from_slice(b"\\x"),
            },
            b'0'..=b'9' => {
                let digits = 1 + body[j..]
                    .iter()
                    .take(2)
                    .take_while(|d| d.is_ascii_digit())
                    .count();
                let start = j - 1;
                match std::str::from_utf8(&body[start..start + digits])
                    .ok()
                    .and_then(|d| d.parse::<u8>().ok())
                {
                    Some(byte) => {
                        out.push(byte);
                        j = start + digits;
                    }
                    None => out.extend_from_slice(&[b'\\', c]),
                }
            }
            b'u' => match unicode_escape(&body[j..]) {
                Some((ch, len)) => {
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                    j += len;
                }
                None => out.extend_from_slice(b"\\u"),
            },
            _ => out.extend_from_slice(&[b'\\', c]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse the `{XXX}` of a `\u{XXX}` escape, returning the character and
/// the length of the braced part. Lua allows at most 8 hex digits
/// (`7FFFFFFF`); only values that are Rust `char`s decode.
fn unicode_escape(rest: &[u8]) -> Option<(char, usize)> {
    let digits = rest.strip_prefix(b"{")?;
    let len = digits
        .iter()
        .take(8)
        .take_while(|c| c.is_ascii_hexdigit())
        .count();
    if digits.get(len) != Some(&b'}') {
        return None;
    }
    let ch = char::from_u32(parse_hex(&digits[..len], 8)?)?;
    Some((ch, len + 2))
}

/// Parse 1 to `max` hex digits, rejecting any other byte (including the
/// sign `from_str_radix` would accept).
fn parse_hex<T: TryFrom<u32>>(digits: &[u8], max: usize) -> Option<T> {
    if digits.is_empty() || digits.len() > max || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let value = digits
        .iter()
        .try_fold(0u32, |acc, &d| Some(acc * 16 + char::from(d).to_digit(16)?))?;
    T::try_from(value).ok()
}

/// A long string's value: its contents without a first line break. Unlike
/// Lua, other CR or CR LF line breaks are kept as written rather than
/// turned into LF; the fields read from parsers.lua are single-line.
fn long_string_value(body: &[u8]) -> String {
    let body = body
        .strip_prefix(b"\r\n")
        .or_else(|| body.strip_prefix(b"\n\r"))
        .or_else(|| body.strip_prefix(b"\n"))
        .or_else(|| body.strip_prefix(b"\r"))
        .unwrap_or(body);
    String::from_utf8_lossy(body).into_owned()
}

/// Copy `source`, replacing every character inside `ranges` except
/// newlines with as many spaces as its UTF-8 length.
fn blank(source: &str, ranges: &[Range<usize>]) -> String {
    let mut code = String::with_capacity(source.len());
    let mut ranges = ranges.iter().peekable();
    for (i, c) in source.char_indices() {
        while ranges.next_if(|r| r.end <= i).is_some() {}
        let inside = ranges.peek().is_some_and(|r| r.contains(&i));
        if inside && c != '\n' {
            code.extend(std::iter::repeat_n(' ', c.len_utf8()));
        } else {
            code.push(c);
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_comment_ends_at_a_lone_carriage_return() {
        let lua = LuaCode::new("-- c\rx = '}'").unwrap();

        assert_eq!(lua.code, "    \rx = ' '");
    }
}
