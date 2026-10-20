//! A completion's snippet as every client expands it, and the indentation a
//! prepared document's map gives that expansion carried back into the
//! snippet.
//!
//! A prepared document's map checks the text a client inserts, which for a
//! snippet is its expansion with every tabstop at its default. What goes out
//! is still the snippet: [`Expansion::carry`] writes into it the indentation
//! the map wrote before the expansion, so that the client expands it to the
//! mapped text.

use std::collections::HashMap;

/// A snippet and the text it expands to with every tabstop and placeholder
/// left at its default: what the client inserts unless the user types over
/// it. Every later occurrence of a tabstop mirrors the default of its first
/// — `${1:y}x$1` expands to `yxy`.
#[derive(Debug)]
pub(super) struct Expansion {
    snippet: String,
    text: String,
    /// Whether the snippet holds a tabstop at all.
    tabstops: bool,
}

/// A run of the snippet: literal text, or one occurrence of a tabstop.
enum Piece {
    Text(String),
    Tabstop(String),
}

impl Expansion {
    /// The expansion of `snippet`, when every client agrees on it: literal
    /// text, tabstops, and at most one placeholder per tabstop whose
    /// default holds no `$` — no nesting, no mirror inside a default — all
    /// on one line, without a tab. `None` for anything else: text the
    /// client decides at insert time (a variable), that this expansion
    /// does not compute (a transform), or that clients resolve differently
    /// (several defaults for one tabstop, defaults holding defaults, a
    /// choice — VS Code inserts its first option, Neovim nothing until the
    /// user reaches it —, a default escaping anything but `$`, `}` or `\`,
    /// a `$` opening nothing, the final tabstop twice, a line break — a
    /// snippet engine indents the lines after the first by the caret
    /// line's, whatever the item asks — or a tab — one turns tabs into
    /// spaces); and for an expansion far longer than the snippet (one
    /// default mirrored many times).
    pub(super) fn of(snippet: &str) -> Option<Self> {
        // Neovim's vim.snippet.expand (and mini.snippets) prefix every line
        // after the first with the caret line's indentation, and under
        // `expandtab` turn every tab into spaces, whatever `insertTextMode`
        // says. The expansion only holds what the snippet does.
        if snippet.contains(['\n', '\r', '\t']) {
            return None;
        }
        let mut chars = snippet.chars().peekable();
        let mut pieces = Vec::new();
        let mut defaults: HashMap<String, String> = HashMap::new();
        let mut occurrences: HashMap<String, usize> = HashMap::new();
        let mut text = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(escaped @ ('$' | '}' | '\\')) => text.push(escaped),
                    Some(other) => {
                        text.push('\\');
                        text.push(other);
                    }
                    None => text.push('\\'),
                },
                '$' => {
                    let number = if chars.next_if_eq(&'{').is_some() {
                        // A variable (or anything but a tabstop).
                        let number = tabstop(&mut chars)?;
                        let default = match chars.next()? {
                            '}' => None,
                            ':' => Some(placeholder(&mut chars)?),
                            // A choice, which clients insert apart, or a
                            // transform.
                            _ => return None,
                        };
                        // A default must stand on its tabstop's first
                        // occurrence: mini.snippets takes the first as the
                        // tabstop, inserting `x` for `$1x${1:y}` where
                        // vim.snippet inserts `yxy`.
                        if default.is_some() && occurrences.contains_key(&number) {
                            return None;
                        }
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
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                    pieces.push(Piece::Tabstop(number));
                }
                c => text.push(c),
            }
        }
        pieces.push(Piece::Text(text));
        // The final tabstop is where the cursor ends: Neovim refuses a
        // snippet holding it more than once (VS Code takes the first).
        if occurrences.get("0").is_some_and(|&count| count > 1) {
            return None;
        }
        // A client may tell a snippet from plain text by its text alone:
        // mini.completion takes an item for a snippet only when a `$` not
        // after a backslash opens a word (`$1`, `${1`), and inserts any
        // other as is, escapes and all (`a\\$1` with both backslashes).
        if !occurrences.is_empty() && !reads_as_snippet(snippet) {
            return None;
        }
        // Mirrors repeat their default: bounded by the snippet's own size.
        let limit = snippet.len().saturating_mul(4).max(4096);
        let mut out = String::new();
        for piece in &pieces {
            out.push_str(match piece {
                Piece::Text(text) => text,
                Piece::Tabstop(number) => defaults.get(number).map_or("", String::as_str),
            });
            if out.len() > limit {
                return None;
            }
        }
        Some(Self {
            snippet: snippet.to_string(),
            text: out,
            tabstops: !occurrences.is_empty(),
        })
    }

    /// The text the snippet expands to.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Whether the snippet holds a tabstop: without one, it is the text its
    /// escapes stand for.
    pub(super) fn has_tabstop(&self) -> bool {
        self.tabstops
    }

    /// The snippet expanding to `mapped`: this expansion after spaces (the
    /// indentation a prepared map regains on the line it starts), the
    /// spaces written before the snippet. `None` when `mapped` differs
    /// otherwise, or regains a tab (see [`Self::of`]).
    pub(super) fn carry(&self, mapped: &str) -> Option<String> {
        let indent = mapped.strip_suffix(self.text.as_str())?;
        if !indent.chars().all(|c| c == ' ') {
            return None;
        }
        Some(format!("{indent}{}", self.snippet))
    }
}

/// Whether `text` holds a `$` opening a word (`$1`, `${1`), at its start or
/// after anything but a backslash: what mini.completion reads as snippet
/// text (with a tab or a line break, which no expansion holds).
pub(super) fn reads_as_snippet(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().any(|(at, &byte)| {
        let opens_word = |from: usize| bytes.get(from).is_some_and(u8::is_ascii_alphanumeric);
        byte == b'$'
            && (at == 0 || bytes[at - 1] != b'\\')
            && (opens_word(at + 1) || (bytes.get(at + 1) == Some(&b'{') && opens_word(at + 2)))
    })
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Read a tabstop number (digits only: `$0name` is tabstop 0, then text),
/// without leading zeros (`$01` is tabstop 1); `None`, consuming nothing,
/// before anything else.
fn tabstop(chars: &mut Chars<'_>) -> Option<String> {
    let mut number = String::new();
    let mut read = false;
    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
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
fn placeholder(chars: &mut Chars<'_>) -> Option<String> {
    let mut text = String::new();
    loop {
        match chars.next()? {
            '\\' => match chars.next()? {
                escaped @ ('$' | '}' | '\\') => text.push(escaped),
                // VS Code keeps the backslash; Neovim's grammar reads no
                // other escape in a default.
                _ => return None,
            },
            '}' => break,
            // A nested placeholder, a mirror or a variable in a default.
            '$' => return None,
            c => text.push(c),
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expanded(snippet: &str) -> Option<String> {
        Expansion::of(snippet).map(|expansion| expansion.text)
    }

    #[test]
    fn a_snippet_expands_to_its_defaults() {
        assert_eq!(expanded("f(${1})$0").as_deref(), Some("f()"));
        assert_eq!(expanded("f(${1:x}, ${2:y})").as_deref(), Some("f(x, y)"));
        assert_eq!(expanded("\\$1 \\} \\x").as_deref(), Some("$1 } \\x"));
        assert_eq!(expanded("$0name").as_deref(), Some("name"));
        assert_eq!(expanded("${1}x$12y").as_deref(), Some("xy"));
        assert_eq!(expanded("${1:a\\}b\\$c}").as_deref(), Some("a}b$c"));
        assert_eq!(expanded("a}b\\").as_deref(), Some("a}b\\"));
        // Every later occurrence mirrors the default of its first.
        assert_eq!(expanded("${1:y}x$1").as_deref(), Some("yxy"));
        assert_eq!(expanded("$00").as_deref(), Some(""));
        // Other tabstops mirror however often they occur.
        assert_eq!(expanded("${1:y}x$1$1$0").as_deref(), Some("yxyy"));
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
        // A default after its tabstop's first occurrence, which
        // mini.snippets does not mirror back.
        assert_eq!(expanded("$1x${1:y}"), None);
        assert_eq!(expanded("$1x$1${1:y}"), None);
        // Several defaults for one tabstop.
        assert_eq!(expanded("${1:a} ${1:b}"), None);
        assert_eq!(expanded("$1x${1:}${1:y}"), None);
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

    /// Lines after the first, and tabs, are what snippet engines rewrite
    /// whatever the item asks (Neovim's vim.snippet.expand indents the
    /// later lines by the caret line's and, under `expandtab`, turns tabs
    /// into spaces; mini.snippets indents them too).
    #[test]
    fn a_snippet_breaking_a_line_or_holding_a_tab_has_no_expansion() {
        for snippet in [
            "{\n$0\n}",
            "do\n\t${1:body}\n$0end",
            "${1:a\nb}",
            "a\r\nb$0",
            "a\rb$0",
            "\\\n$0",
            "\tf($1)",
            "f(${1:\t})",
        ] {
            assert_eq!(expanded(snippet), None, "{snippet:?}");
        }
    }

    /// A snippet whose every tabstop follows a backslash reads as plain
    /// text to a client telling snippets by their text.
    #[test]
    fn a_snippet_reading_as_plain_text_has_no_expansion() {
        assert_eq!(expanded("a\\\\$1"), None);
        assert_eq!(expanded("a\\\\${1:x}"), None);
        assert_eq!(expanded("a\\\\$1 $0").as_deref(), Some("a\\ "));
        assert_eq!(expanded("$1").as_deref(), Some(""));
        assert!(reads_as_snippet("x${1:a}"));
        assert!(reads_as_snippet("price: $5"));
        assert!(!reads_as_snippet("a\\$1"));
        assert!(!reads_as_snippet("a $ b"));
        assert!(!reads_as_snippet("${}"));
    }

    #[test]
    fn a_snippet_without_a_tabstop_is_known() {
        assert!(!Expansion::of("x\\$").unwrap().has_tabstop());
        assert!(Expansion::of("x$0").unwrap().has_tabstop());
        assert!(Expansion::of("${1:x}").unwrap().has_tabstop());
    }

    #[test]
    fn indentation_regained_before_the_expansion_is_written_before_the_snippet() {
        let carried = |snippet: &str, mapped: &str| Expansion::of(snippet)?.carry(mapped);
        assert_eq!(
            carried("f(${1:x})$0", "  f(x)").as_deref(),
            Some("  f(${1:x})$0")
        );
        assert_eq!(carried("f($1)", "f()").as_deref(), Some("f($1)"));
        // A tab some engines turn into spaces, or text other than
        // indentation before the expansion or after it.
        assert_eq!(carried("f($1)", "\tf()"), None);
        assert_eq!(carried("f($1)", "- f()"), None);
        assert_eq!(carried("f($1)", "f() "), None);
        assert_eq!(carried("f($1)", "f("), None);
    }
}
