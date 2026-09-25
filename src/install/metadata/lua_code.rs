//! Code-only view of Lua source for the lightweight parsers.lua reader.
//!
//! The reader finds structure with brace counting and regexes, which only
//! work if braces, quotes and keys inside string literals and comments are
//! out of the way. [`LuaCode`] blanks those while keeping every byte offset,
//! so a position found in the blanked text is valid in the source too.

use std::ops::Range;

use super::MetadataError;

/// Lua source with comments and string contents blanked out.
pub(super) struct LuaCode<'a> {
    /// The original source.
    pub(super) source: &'a str,
    /// Same length as `source`. Comments and string contents are spaces,
    /// newlines kept; string delimiters and all code are unchanged.
    pub(super) code: String,
}

impl<'a> LuaCode<'a> {
    pub(super) fn new(source: &'a str) -> Result<Self, MetadataError> {
        let blanked = blanked_ranges(source.as_bytes())?;
        Ok(Self {
            source,
            code: blank(source, &blanked),
        })
    }
}

/// Byte ranges covering comments and string contents, in source order.
fn blanked_ranges(b: &[u8]) -> Result<Vec<Range<usize>>, MetadataError> {
    let mut ranges = Vec::new();
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
                        .position(|&c| c == b'\n')
                        .map_or(b.len(), |n| i + n)
                };
                ranges.push(i..end);
                i = end;
            }
            quote @ (b'"' | b'\'') => {
                let close =
                    short_string_close(b, i + 1, quote).ok_or_else(|| unterminated("string", i))?;
                ranges.push(i + 1..close);
                i = close + 1;
            }
            b'[' => {
                if let Some(level) = long_bracket_level(b, i) {
                    let open_end = i + level + 2;
                    let close = long_bracket_close(b, open_end, level)
                        .ok_or_else(|| unterminated("string", i))?;
                    ranges.push(open_end..close);
                    i = close + level + 2;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Ok(ranges)
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
/// `from`. A backslash escapes the next byte; a raw line break or the end
/// of input leaves the string unterminated, as in Lua.
fn short_string_close(b: &[u8], from: usize, quote: u8) -> Option<usize> {
    let mut j = from;
    while j < b.len() {
        match b[j] {
            b'\\' if b[j + 1..].starts_with(b"\r\n") => j += 3,
            b'\\' => j += 2,
            b'\n' | b'\r' => return None,
            c if c == quote => return Some(j),
            _ => j += 1,
        }
    }
    None
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
