//! A completion's snippet as every client expands it, and the indentation a
//! prepared document's map gives that expansion carried back into the
//! snippet.
//!
//! A prepared document's map checks the text a client inserts, which for a
//! snippet is its expansion with every tabstop at its default. What goes out
//! is still the snippet: [`Expansion::carry`] writes into it the indentation
//! the map wrote into the expansion, where the snippet holds the expanded
//! text as literal text (or as the default of a tabstop it holds once), so
//! that the client expands it to the mapped text.

use std::collections::HashMap;

/// A snippet and the text it expands to with every tabstop and placeholder
/// left at its default: what the client inserts unless the user types over
/// it. Every occurrence of a tabstop mirrors the default of its
/// placeholder, wherever that stands — `$1x${1:y}` expands to `yxy`.
#[derive(Debug)]
pub(super) struct Expansion {
    snippet: String,
    text: String,
    /// For each line break in `text`, the byte after it and where the
    /// snippet holds that place as text one can write in: after the same
    /// line break in literal text, or in the default of a placeholder whose
    /// tabstop occurs once. `None` where it does not (a default other
    /// occurrences mirror).
    breaks: Vec<(usize, Option<usize>)>,
}

/// A run of the snippet: literal text, or one occurrence of a tabstop.
enum Piece {
    /// Unescaped text, with each line break's end in it and in the snippet.
    Text(String, Vec<(usize, usize)>),
    Tabstop(String),
}

/// A tabstop's default (a placeholder's text), with each line break's end
/// in it and in the snippet.
struct Default {
    text: String,
    breaks: Vec<(usize, usize)>,
}

impl Expansion {
    /// The expansion of `snippet`, when every client agrees on it: literal
    /// text, tabstops, and at most one placeholder per tabstop whose
    /// default holds no `$` — no nesting, no mirror inside a default.
    /// `None` for anything else: text the client decides at insert time (a
    /// variable), that this expansion does not compute (a transform), or
    /// that clients resolve differently (several defaults for one tabstop,
    /// defaults holding defaults, a choice — VS Code inserts its first
    /// option, Neovim nothing until the user reaches it —, a default
    /// escaping anything but `$`, `}` or `\`, a `$` opening nothing); and
    /// for an expansion far longer than the snippet (one default mirrored
    /// many times).
    pub(super) fn of(snippet: &str) -> Option<Self> {
        let mut chars = snippet.char_indices().peekable();
        let mut pieces = Vec::new();
        let mut defaults: HashMap<String, Default> = HashMap::new();
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        let mut text = String::new();
        let mut text_breaks = Vec::new();
        // Where a char read at `at` ends in the snippet.
        let end_of = |at: usize, c: char| at + c.len_utf8();
        while let Some((at, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, escaped @ ('$' | '}' | '\\'))) => text.push(escaped),
                    Some((other_at, other)) => {
                        text.push('\\');
                        text.push(other);
                        if is_break(other) {
                            text_breaks.push((text.len(), end_of(other_at, other)));
                        }
                    }
                    None => text.push('\\'),
                },
                '$' => {
                    let number = if chars.next_if(|&(_, c)| c == '{').is_some() {
                        // A variable (or anything but a tabstop).
                        let number = tabstop(&mut chars)?;
                        let default = match chars.next()?.1 {
                            '}' => None,
                            ':' => Some(placeholder(&mut chars)?),
                            // A choice, which clients insert apart, or a
                            // transform.
                            _ => return None,
                        };
                        if let Some(default) = default
                            && defaults.insert(number.clone(), default).is_some()
                        {
                            // Several defaults for one tabstop.
                            return None;
                        }
                        number
                    } else {
                        // `None` for a variable, or a `$` opening nothing,
                        // which Neovim's grammar does not read (VS Code
                        // takes it as text).
                        tabstop(&mut chars)?
                    };
                    *occurrences.entry(number.clone()).or_default() += 1;
                    pieces.push(Piece::Text(
                        std::mem::take(&mut text),
                        std::mem::take(&mut text_breaks),
                    ));
                    pieces.push(Piece::Tabstop(number));
                }
                c => {
                    text.push(c);
                    if is_break(c) {
                        text_breaks.push((text.len(), end_of(at, c)));
                    }
                }
            }
        }
        pieces.push(Piece::Text(text, text_breaks));
        // The final tabstop is where the cursor ends: Neovim refuses a
        // snippet holding it more than once (VS Code takes the first).
        if occurrences.get("0").is_some_and(|&count| count > 1) {
            return None;
        }
        // Mirrors repeat their default: bounded by the snippet's own size.
        let limit = snippet.len().saturating_mul(4).max(4096);
        let mut out = String::new();
        let mut breaks = Vec::new();
        for piece in &pieces {
            let (piece_text, piece_breaks, writable) = match piece {
                Piece::Text(text, text_breaks) => (text.as_str(), text_breaks.as_slice(), true),
                Piece::Tabstop(number) => match defaults.get(number) {
                    Some(default) => (
                        default.text.as_str(),
                        default.breaks.as_slice(),
                        occurrences.get(number) == Some(&1),
                    ),
                    None => ("", &[][..], false),
                },
            };
            let base = out.len();
            out.push_str(piece_text);
            breaks.extend(
                piece_breaks
                    .iter()
                    .map(|&(end, raw)| (base + end, writable.then_some(raw))),
            );
            if out.len() > limit {
                return None;
            }
        }
        Some(Self {
            snippet: snippet.to_string(),
            text: out,
            breaks,
        })
    }

    /// The text the snippet expands to.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Whether the snippet breaks a line where it is inserted (a client may
    /// re-indent such a snippet's lines whatever the item asks, see the
    /// caller).
    pub(super) fn breaks_lines(&self) -> bool {
        self.snippet.contains(is_break)
    }

    /// The snippet expanding to `mapped`: this expansion with indentation
    /// written at the start of its lines (spaces and tabs, what a prepared
    /// map regains), that indentation written into the snippet where it
    /// holds those line starts as text one can write in (see `breaks`).
    /// `None` when `mapped` differs otherwise, or indents a line the snippet
    /// holds only as a default other occurrences mirror.
    pub(super) fn carry(&self, mapped: &str) -> Option<String> {
        if mapped == self.text {
            return Some(self.snippet.clone());
        }
        let ours = line_starts(&self.text);
        let theirs = line_starts(mapped);
        if ours.len() != theirs.len() {
            return None;
        }
        let line = |text: &'_ str, starts: &[usize], index: usize| -> (usize, usize) {
            let end = starts.get(index + 1).copied().unwrap_or(text.len());
            (starts[index], end)
        };
        let mut writes: Vec<(usize, &str)> = Vec::new();
        // Line starts and line breaks both ascend: one walk over the breaks.
        let mut breaks = self.breaks.iter().peekable();
        for index in 0..ours.len() {
            let (start, end) = line(&self.text, &ours, index);
            let (mapped_start, mapped_end) = line(mapped, &theirs, index);
            let expanded_line = &self.text[start..end];
            let mapped_line = &mapped[mapped_start..mapped_end];
            let indent = mapped_line.strip_suffix(expanded_line)?;
            if indent.is_empty() {
                continue;
            }
            if !indent.chars().all(|c| c == ' ' || c == '\t') {
                return None;
            }
            let at = if start == 0 {
                0
            } else {
                while breaks.next_if(|&&(end, _)| end < start).is_some() {}
                match breaks.peek() {
                    Some(&&(end, raw)) if end == start => raw?,
                    _ => return None,
                }
            };
            writes.push((at, indent));
        }
        let mut snippet = String::with_capacity(mapped.len() + self.snippet.len());
        let mut cursor = 0;
        writes.sort_by_key(|&(at, _)| at);
        for (at, indent) in writes {
            snippet.push_str(&self.snippet[cursor..at]);
            snippet.push_str(indent);
            cursor = at;
        }
        snippet.push_str(&self.snippet[cursor..]);
        // Written only where the snippet holds text as is: checked, not
        // assumed.
        (Self::of(&snippet)?.text == mapped).then_some(snippet)
    }
}

fn is_break(c: char) -> bool {
    c == '\n' || c == '\r'
}

/// Where each line of `text` starts: after an LF, or a CR no LF follows.
fn line_starts(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut starts = vec![0];
    for (at, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' || (byte == b'\r' && bytes.get(at + 1) != Some(&b'\n')) {
            starts.push(at + 1);
        }
    }
    starts
}

type Chars<'a> = std::iter::Peekable<std::str::CharIndices<'a>>;

/// Read a tabstop number (digits only: `$0name` is tabstop 0, then text),
/// without leading zeros (`$01` is tabstop 1); `None`, consuming nothing,
/// before anything else.
fn tabstop(chars: &mut Chars<'_>) -> Option<String> {
    let mut number = String::new();
    let mut read = false;
    while let Some((_, digit)) = chars.next_if(|(_, c)| c.is_ascii_digit()) {
        read = true;
        if !(number.is_empty() && digit == '0') {
            number.push(digit);
        }
    }
    read.then(|| {
        if number.is_empty() {
            "0".to_string()
        } else {
            number
        }
    })
}

/// A placeholder's default, after its `:`, through its `}`.
fn placeholder(chars: &mut Chars<'_>) -> Option<Default> {
    let mut text = String::new();
    let mut breaks = Vec::new();
    loop {
        let (at, c) = chars.next()?;
        match c {
            '\\' => match chars.next()?.1 {
                escaped @ ('$' | '}' | '\\') => text.push(escaped),
                // VS Code keeps the backslash; Neovim's grammar reads no
                // other escape in a default.
                _ => return None,
            },
            '}' => break,
            // A nested placeholder, a mirror or a variable in a default.
            '$' => return None,
            c => {
                text.push(c);
                if is_break(c) {
                    breaks.push((text.len(), at + c.len_utf8()));
                }
            }
        }
    }
    Some(Default { text, breaks })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(snippet: &str) -> Option<String> {
        Expansion::of(snippet).map(|expansion| expansion.text)
    }

    #[test]
    fn a_snippet_expands_to_its_defaults() {
        assert_eq!(expanded("$0\n").as_deref(), Some("\n"));
        assert_eq!(expanded("f(${1})$0").as_deref(), Some("f()"));
        assert_eq!(expanded("f(${1:x}, ${2:y})").as_deref(), Some("f(x, y)"));
        assert_eq!(expanded("\\$1 \\} \\x").as_deref(), Some("$1 } \\x"));
        assert_eq!(expanded("$0name\n").as_deref(), Some("name\n"));
        assert_eq!(expanded("${1}x$12y").as_deref(), Some("xy"));
        assert_eq!(expanded("${1:a\\}b\\$c}").as_deref(), Some("a}b$c"));
        assert_eq!(expanded("a}b\\").as_deref(), Some("a}b\\"));
        // Every occurrence mirrors its tabstop's default, ahead of it too.
        assert_eq!(expanded("$1x${1:\n}").as_deref(), Some("\nx\n"));
        assert_eq!(expanded("$01x${1:\n}").as_deref(), Some("\nx\n"));
        assert_eq!(expanded("$00").as_deref(), Some(""));
        // Other tabstops mirror however often they occur.
        assert_eq!(expanded("$1x$1${1:y}$0").as_deref(), Some("yxyy"));
    }

    #[test]
    fn a_snippet_clients_expand_differently_has_no_expansion() {
        // A choice: VS Code inserts its first option, Neovim nothing until
        // the user reaches it.
        assert_eq!(expanded("${1|a,b|}c"), None);
        assert_eq!(expanded("${1:x}-${2|a,b|}"), None);
        assert_eq!(expanded("${1|a\\,b\\|\\\\c,d|}"), None);
        // A `$` opening nothing, an escape a default holds of anything but
        // `$`, `}` or `\`: Neovim's grammar reads neither (VS Code reads
        // them as text).
        assert_eq!(expanded("cost: $"), None);
        assert_eq!(expanded("a $ b"), None);
        assert_eq!(expanded("$$1"), None);
        assert_eq!(expanded("${1:a\\qb}"), None);
        // The final tabstop more than once, however it is numbered.
        assert_eq!(expanded("$0x$0"), None);
        assert_eq!(expanded("${0:a}x$0"), None);
        assert_eq!(expanded("$00x${0}"), None);
        // Several defaults for one tabstop.
        assert_eq!(expanded("${1:a} ${1:b}"), None);
        assert_eq!(expanded("$1x${1:}${1:\n}"), None);
        // Defaults holding placeholders or mirrors, which clients resolve
        // differently (and could chain or cycle without end).
        assert_eq!(expanded("f(${1:x, ${2:y}})"), None);
        assert_eq!(expanded("$2 ${1:<$2>} ${2:x}"), None);
        let chain: String = (1..100_000)
            .map(|n| format!("${{{n}:${}}}", n + 1))
            .chain(["${100000:x}".to_string()])
            .collect();
        assert_eq!(expanded(&chain), None);
        assert_eq!(expanded("${1:$2}${2:$1}"), None);
        assert_eq!(expanded("${1:a$1}"), None);
        // A default mirrored many times is bounded.
        let repeated = format!("${{1:{}}}{}", "x".repeat(1000), "$1".repeat(100));
        assert_eq!(expanded(&repeated), None);
        let few = format!("${{1:{}}}{}", "x".repeat(1000), "$1".repeat(3));
        assert_eq!(expanded(&few).map(|text| text.len()), Some(4000));
        // The client decides these.
        assert_eq!(expanded("$TM_X"), None);
        assert_eq!(expanded("${NAME:d}"), None);
        assert_eq!(expanded("${_x}"), None);
        assert_eq!(expanded("${1/a/b/}"), None);
        assert_eq!(expanded("${1:x"), None);
        let deep = format!("{}x{}", "${1:".repeat(10_000), "}".repeat(10_000));
        assert_eq!(expanded(&deep), None);
    }

    #[test]
    fn indentation_written_into_the_expansion_is_written_into_the_snippet() {
        let carried = |snippet: &str, mapped: &str| Expansion::of(snippet)?.carry(mapped);
        // Literal lines, a line holding only a tabstop, a line opening with
        // one: the indent goes before the tabstop.
        assert_eq!(
            carried("{\n$0\n}", "{\n\n  }").as_deref(),
            Some("{\n$0\n  }")
        );
        assert_eq!(
            carried("{\n\t$0\n}", "{\n  \t\n  }").as_deref(),
            Some("{\n  \t$0\n  }")
        );
        assert_eq!(
            carried("if ${1:x}:\n\t$0", "if x:\n  \t").as_deref(),
            Some("if ${1:x}:\n  \t$0")
        );
        assert_eq!(
            carried("fn ${1:name}() {\n\t$0\n}", "  fn name() {\n  \t\n  }").as_deref(),
            Some("  fn ${1:name}() {\n  \t$0\n  }")
        );
        // A placeholder default spanning lines, its tabstop held once.
        assert_eq!(
            carried("${1:a\nb}\nc", "a\n  b\n  c").as_deref(),
            Some("${1:a\n  b}\n  c")
        );
        // CRLF and lone CR line breaks.
        assert_eq!(
            carried("a\r\n$1\rb", "a\r\n  \r  b").as_deref(),
            Some("a\r\n  $1\r  b")
        );
        // Unchanged, it is the snippet as is.
        assert_eq!(carried("$1\n${2:x}", "\nx").as_deref(), Some("$1\n${2:x}"));
    }

    #[test]
    fn indentation_the_snippet_cannot_hold_as_text_is_refused() {
        let carried = |snippet: &str, mapped: &str| Expansion::of(snippet)?.carry(mapped);
        // A default mirrored elsewhere: written once, it would indent every
        // occurrence.
        assert_eq!(carried("$1x${1:a\nb}", "a\n  bxa\n  b"), None);
        assert_eq!(carried("$1x${1:a\nb}", "a\nbxa\n  b"), None);
        // Text other than indentation, or not at a line's start.
        assert_eq!(carried("a\n$0b", "a\nb  "), None);
        assert_eq!(carried("a\n$0b", "a\n- b"), None);
        assert_eq!(carried("a\n$0b", "a\n"), None);
        assert_eq!(carried("a\n$0b", "a\nb\n"), None);
        assert_eq!(carried("a$0b", "a b"), None);
    }

    /// Every line of a long snippet regaining an indent: work linear in its
    /// size, bounded by time far above what it takes (well under 0.2 s).
    #[test]
    fn carrying_an_indent_into_every_line_takes_linear_work() {
        let lines = 80_000;
        let snippet = format!("{}$0", "a\n".repeat(lines));
        let mapped = format!("a\n{}  ", "  a\n".repeat(lines - 1));
        let expansion = Expansion::of(&snippet).unwrap();
        let started = std::time::Instant::now();
        let carried = expansion.carry(&mapped).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(carried, format!("a\n{}  $0", "  a\n".repeat(lines - 1)));
    }

    #[test]
    fn a_snippet_breaks_lines_by_its_text() {
        assert!(Expansion::of("a\n$0").unwrap().breaks_lines());
        assert!(Expansion::of("${1:a\rb}").unwrap().breaks_lines());
        assert!(!Expansion::of("f(${1:x})$0").unwrap().breaks_lines());
    }
}
