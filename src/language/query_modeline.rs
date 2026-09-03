//! The `;` comment modelines Neovim reads at the top of a query file.
//!
//! nvim-treesitter queries declare what they build on through comments such
//! as `; inherits: ecma`, and user overlays mark themselves `;; extends`;
//! kakehashi reads the same modelines so those query files load unchanged.
//! This module is the one parser for them: the loader, the installer, and the
//! in-repo asset tests all read a query's modelines through it, so a form
//! accepted in one place is accepted everywhere.
//!
//! The grammar is Neovim's (`runtime/lua/vim/treesitter/query.lua`): the
//! modeline block is the run of lines at the top of the file that start with
//! `;`, and within it
//!
//! ```text
//! ;+ inherits: lang1,lang2    ; the colon is optional
//! ;+ extends
//! ```
//!
//! are directives while every other `;` line is an ordinary comment. Both
//! directives may repeat; the first line that does not start with `;` — a
//! blank line included — ends the block.
//!
//! Neovim matches `inherits` against `^;+%s*inherits%s*:?%s*([a-z_,()]+)%s*$`
//! and then splits the list on commas: the keyword needs no separator before
//! the list, the list is one run of list characters with no whitespace inside
//! it, and a line whose operand holds anything else — `; inherits the ecma
//! queries` — is prose, not a directive. Within a line that matches, each
//! entry stands alone: `; inherits: ecma,` still inherits `ecma`, and the
//! empty entry after the comma is skipped (see [`inherits_operand`] for why
//! it is skipped rather than looked up). So a comment that mentions
//! inheritance never becomes a parent that cannot be found, and a stray
//! comma never loses the parents beside it. The one widening of Neovim's
//! grammar is digits in a name: nvim-treesitter ships languages such as
//! `m68k` and `t32` that Neovim's own character class cannot name. A name in
//! this class is also what the installer needs of a path and URL segment, so
//! [`is_safe_language_name`] is the one definition both sides use.

/// What the modeline block at the top of a query file declares.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct QueryModeline {
    /// Parent languages named by `inherits` directives, in declaration order,
    /// each name at most once; a name written both with and without
    /// parentheses is required.
    pub inherits: Vec<InheritedLanguage>,
    /// Whether an `extends` directive marks the file as an overlay to merge
    /// onto the language's base query instead of replacing it.
    pub extends: bool,
}

/// One entry of an `inherits` list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InheritedLanguage {
    pub name: String,
    /// Written in parentheses, `(cpp)`: Neovim's `get_files` inherits such a
    /// parent only when the file is loaded for its own language, not when
    /// the file is itself being inherited, so a chain stops there.
    pub optional: bool,
}

/// Read the modeline block at the top of `content`.
pub(crate) fn parse_modeline(content: &str) -> QueryModeline {
    let mut modeline = QueryModeline::default();
    for line in content.lines().take_while(|line| line.starts_with(';')) {
        let directive = line.trim_start_matches(';').trim_matches(is_lua_space);
        if directive == "extends" {
            modeline.extends = true;
        } else if let Some(parents) = inherits_operand(directive) {
            for parent in parents {
                match modeline.inherits.iter_mut().find(|p| p.name == parent.name) {
                    // Named again without parentheses, a parent is required:
                    // Neovim would follow that occurrence wherever the file
                    // is reached, so one entry must say the same.
                    Some(known) => known.optional &= parent.optional,
                    None => modeline.inherits.push(parent),
                }
            }
        }
    }
    modeline
}

/// The languages named by an `inherits` directive, or `None` when `directive`
/// is not one.
///
/// Mirrors Neovim's `inherits%s*:?%s*([a-z_,()]+)%s*$` followed by a split on
/// commas: after the keyword, optional whitespace, an optional colon, and
/// optional whitespace, the rest of the line must be one run of list
/// characters — any other operand makes the line a comment, as it is in
/// Neovim. Within a matching line each comma-separated entry is a name on
/// its own: one that is not a name after all (empty, or `(cpp` with its
/// parenthesis unmatched) is skipped, and the parents beside it stand.
/// Neovim would look such an entry up and find nothing; skipping it here
/// keeps a parent that cannot exist from failing the whole query.
fn inherits_operand(directive: &str) -> Option<Vec<InheritedLanguage>> {
    let rest = directive
        .strip_prefix("inherits")?
        .trim_start_matches(is_lua_space);
    let operand = rest
        .strip_prefix(':')
        .unwrap_or(rest)
        .trim_start_matches(is_lua_space);
    let is_list_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_,()".contains(&b);
    if operand.is_empty() || !operand.bytes().all(is_list_char) {
        return None;
    }
    Some(
        operand
            .split(',')
            .map(|entry| {
                let (name, optional) = strip_optional_parentheses(entry);
                InheritedLanguage {
                    name: name.to_string(),
                    optional,
                }
            })
            .filter(|parent| {
                let is_name = is_safe_language_name(&parent.name);
                if !is_name {
                    log::debug!(
                        "Ignoring `{}` in an inherits modeline: not a language name",
                        parent.name
                    );
                }
                is_name
            })
            .collect(),
    )
}

/// What Lua's `%s` matches in Neovim's modeline patterns: C `isspace`, that
/// is ASCII whitespace plus the vertical tab `char::is_ascii_whitespace`
/// leaves out. A U+3000 or a no-break space before the keyword keeps the
/// line a comment in Neovim, so it does here.
fn is_lua_space(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\x0B'
}

/// The character class of a language name: `[a-z0-9_]+`.
///
/// This is what a modeline may name (Neovim's class is `[a-z_]`; see the
/// module doc for why digits are added) and what the installer accepts as a
/// language to fetch: such a name is one normal path component and a safe
/// URL segment, so it can never escape `queries/` on disk or the query
/// source's URL. `pub` because the `kakehashi` binary validates language
/// names — CLI arguments and on-disk entries alike — through the installer's
/// re-export of it.
pub fn is_safe_language_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Split an entry into its bare name and whether it was parenthesized.
/// Only a *matched* pair marks an optional name: `(cpp` is not a name at
/// all, and the caller then skips it rather than guessing `cpp`.
fn strip_optional_parentheses(entry: &str) -> (&str, bool) {
    match entry
        .strip_prefix('(')
        .and_then(|name| name.strip_suffix(')'))
    {
        Some(name) => (name, true),
        None => (entry, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inherits(content: &str) -> Vec<String> {
        parse_modeline(content)
            .inherits
            .into_iter()
            .map(|parent| parent.name)
            .collect()
    }

    fn parent(name: &str, optional: bool) -> InheritedLanguage {
        InheritedLanguage {
            name: name.to_string(),
            optional,
        }
    }

    #[test]
    fn single_parent() {
        // TypeScript inherits from ecma
        let content = "; inherits: ecma\n\n\"require\" @keyword.import\n";
        assert_eq!(inherits(content), vec!["ecma"]);
    }

    #[test]
    fn multiple_parents_in_declaration_order() {
        // JavaScript inherits from ecma and jsx
        let content = "; inherits: ecma,jsx\n\n(identifier) @variable\n";
        assert_eq!(inherits(content), vec!["ecma", "jsx"]);
    }

    #[test]
    fn no_directive() {
        assert_eq!(
            parse_modeline("(identifier) @variable\n"),
            QueryModeline::default()
        );
        assert_eq!(parse_modeline(""), QueryModeline::default());
        assert_eq!(
            parse_modeline(";; just a comment\n(identifier) @variable\n"),
            QueryModeline::default()
        );
    }

    #[test]
    fn parenthesized_names_are_optional_parents() {
        assert_eq!(
            parse_modeline("; inherits: c,(cpp),(cuda)\n(identifier) @variable\n").inherits,
            vec![
                parent("c", false),
                parent("cpp", true),
                parent("cuda", true)
            ]
        );
        assert_eq!(inherits("; inherits: (cpp)\n"), vec!["cpp"]);
        assert_eq!(inherits("; inherits:(cpp),c\n"), vec!["cpp", "c"]);
        // Named again without parentheses, a parent is required, in either
        // order: Neovim would follow the bare occurrence wherever the file is
        // reached, and one entry must say the same.
        assert_eq!(
            parse_modeline("; inherits: c,(c)\n").inherits,
            vec![parent("c", false)]
        );
        assert_eq!(
            parse_modeline("; inherits: (c),c\n").inherits,
            vec![parent("c", false)]
        );
        assert_eq!(
            parse_modeline("; inherits: (c)\n; inherits: c\n").inherits,
            vec![parent("c", false)]
        );
        assert_eq!(
            parse_modeline("; inherits: (c),(c)\n").inherits,
            vec![parent("c", true)]
        );
    }

    /// Neovim splits a matching list on commas and looks each entry up on its
    /// own, so an entry that is no name finds nothing while its neighbours
    /// are still inherited. Reading the whole line as a comment instead would
    /// lose real parents to a stray comma, silently.
    #[test]
    fn an_entry_that_is_not_a_name_is_skipped_and_its_neighbours_kept() {
        assert_eq!(inherits("; inherits: ecma,\n"), vec!["ecma"]);
        assert_eq!(inherits("; inherits: ecma,,jsx\n"), vec!["ecma", "jsx"]);
        assert_eq!(inherits("; inherits: c,(cpp\n"), vec!["c"]);
        assert_eq!(inherits("; inherits: ecma,jsx)\n"), vec!["ecma"]);
        assert_eq!(inherits("; inherits: ()\n"), Vec::<String>::new());
    }

    /// A CRLF file yields the same modeline as an LF file: `lines` and the
    /// spacing trim each drop the `\r` before it can fail the name class.
    #[test]
    fn crlf_line_endings_do_not_reach_the_names() {
        assert_eq!(
            parse_modeline("; inherits: ecma,jsx\r\n;; extends\r\n(identifier) @variable\r\n"),
            QueryModeline {
                inherits: vec![parent("ecma", false), parent("jsx", false)],
                extends: true,
            }
        );
    }

    /// Neovim matches `^;+%s*inherits%s*:?%s*`: any run of semicolons, any
    /// spacing, the colon optional, and no separator needed after the keyword.
    #[test]
    fn any_number_of_semicolons_and_an_optional_colon() {
        assert_eq!(inherits(";; inherits: ecma\n"), vec!["ecma"]);
        assert_eq!(inherits(";;; inherits: ecma\n"), vec!["ecma"]);
        assert_eq!(inherits(";inherits:ecma\n"), vec!["ecma"]);
        assert_eq!(inherits("; inherits ecma\n"), vec!["ecma"]);
        assert_eq!(inherits("; inheritsecma\n"), vec!["ecma"]);
        assert_eq!(
            inherits(";  inherits  :  ecma,jsx  \n"),
            vec!["ecma", "jsx"]
        );
        assert_eq!(inherits("; inherits: m68k,t32\n"), vec!["m68k", "t32"]);
    }

    /// Neovim's operand is one run of `[a-z_,()]`: a line whose operand holds
    /// anything else is prose. Reading it as a directive would name a parent
    /// that cannot exist and fail the whole query for a comment.
    #[test]
    fn a_line_whose_operand_is_not_a_name_list_is_a_comment() {
        assert!(inherits("; inherits the ecma queries\n").is_empty());
        assert!(inherits("; inherits: ecma (see ecma/highlights.scm)\n").is_empty());
        assert!(inherits("; inherits: ecma , jsx\n").is_empty());
        assert!(inherits("; inherits: ecma jsx\n").is_empty());
        assert!(inherits("; inherits: Ecma\n").is_empty());
        assert!(inherits("; inherits: with-dash\n").is_empty());
        assert!(inherits("; inherits: ../../evil\n").is_empty());
        assert!(inherits("; inherits:\n").is_empty());
        assert!(inherits("; inherits\n").is_empty());
        assert!(!parse_modeline("; extendsfoo\n").extends);
        assert!(!parse_modeline("; extends foo\n").extends);
        // Lua's `%s` is C `isspace`: a vertical tab is spacing, an
        // ideographic space or an NBSP is not.
        assert_eq!(inherits("; inherits:\x0Becma\n"), vec!["ecma"]);
        assert!(parse_modeline(";\x0Bextends\x0B\n").extends);
        assert!(inherits(";\u{3000}inherits: ecma\n").is_empty());
        assert!(inherits("; inherits:\u{a0}ecma\n").is_empty());
        assert!(!parse_modeline(";\u{3000}extends\n").extends);
    }

    #[test]
    fn extends_directive() {
        assert!(parse_modeline(";; extends\n(identifier) @variable\n").extends);
        assert!(parse_modeline("; extends\n").extends);
        assert!(parse_modeline(";extends\n").extends);
        assert!(parse_modeline(";;   extends   \n").extends);
        assert!(!parse_modeline("(identifier) @variable\n").extends);
    }

    /// Both orders from `:h treesitter-query-modeline` are valid, and plain
    /// comment lines inside the block are skipped over.
    #[test]
    fn modeline_block_may_span_several_lines() {
        assert_eq!(
            parse_modeline(";; inherits: typescript,jsx\n;; extends\n"),
            QueryModeline {
                inherits: vec![parent("typescript", false), parent("jsx", false)],
                extends: true,
            }
        );
        assert_eq!(
            parse_modeline(";; extends\n;;\n;; inherits: css\n(identifier) @variable\n"),
            QueryModeline {
                inherits: vec![parent("css", false)],
                extends: true,
            }
        );
        assert_eq!(
            inherits("; inherits: c\n; inherits: cpp,c\n"),
            vec!["c", "cpp"],
            "repeated directives accumulate, each parent once"
        );
    }

    /// The block is the leading run of `;` lines: a blank line or a pattern
    /// ends it, and a directive after that is an ordinary comment.
    #[test]
    fn modeline_block_ends_at_the_first_non_comment_line() {
        assert!(inherits("(identifier) @variable\n; inherits: ecma\n").is_empty());
        assert_eq!(
            parse_modeline(";; extends\n\n; inherits: ecma\n"),
            QueryModeline {
                inherits: vec![],
                extends: true,
            }
        );
    }
}
