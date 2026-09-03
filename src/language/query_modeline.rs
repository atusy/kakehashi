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

/// What the modeline block at the top of a query file declares.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct QueryModeline {
    /// Parent languages named by `inherits` directives, in declaration order,
    /// each at most once.
    ///
    /// A parenthesized name (`(cpp)`) is read as the bare name: Neovim uses the
    /// parentheses to stop recursion when the file is itself being inherited,
    /// which kakehashi does not distinguish.
    pub inherits: Vec<String>,
    /// Whether an `extends` directive marks the file as an overlay to merge
    /// onto the language's base query instead of replacing it.
    pub extends: bool,
}

/// Read the modeline block at the top of `content`.
pub(crate) fn parse_modeline(content: &str) -> QueryModeline {
    let mut modeline = QueryModeline::default();
    for line in content.lines().take_while(|line| line.starts_with(';')) {
        let directive = line.trim_start_matches(';').trim();
        if directive == "extends" {
            modeline.extends = true;
        } else if let Some(names) = inherits_operand(directive) {
            for name in names.split(',') {
                let name = normalize_inherited_language_name(name);
                if !name.is_empty() && !modeline.inherits.contains(&name) {
                    modeline.inherits.push(name);
                }
            }
        }
    }
    modeline
}

/// The language list of an `inherits` directive, or `None` when `directive`
/// is not one. The keyword must end with the colon or whitespace, so a
/// comment that merely begins with the word (`inheritsfoo`) is not read as it.
fn inherits_operand(directive: &str) -> Option<&str> {
    let rest = directive.strip_prefix("inherits")?;
    match rest.chars().next() {
        Some(':') => Some(&rest[1..]),
        Some(c) if c.is_whitespace() => {
            let rest = rest.trim_start();
            Some(rest.strip_prefix(':').unwrap_or(rest))
        }
        _ => None,
    }
}

/// Strip only a *matched* pair of parentheses: on `(cpp` the loader looks for
/// a language literally called `(cpp`, and an installer that helpfully
/// fetched `cpp` would report success over a language it still cannot load.
fn normalize_inherited_language_name(name: &str) -> String {
    let name = name.trim();
    name.strip_prefix('(')
        .and_then(|name| name.strip_suffix(')'))
        .unwrap_or(name)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inherits(content: &str) -> Vec<String> {
        parse_modeline(content).inherits
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
    fn spaces_around_names() {
        assert_eq!(inherits("; inherits: ecma , jsx\n"), vec!["ecma", "jsx"]);
    }

    #[test]
    fn parenthesized_names_read_as_bare_names() {
        assert_eq!(
            inherits("; inherits: c, (cpp), ( cuda )\n(identifier) @variable\n"),
            vec!["c", "cpp", "cuda"]
        );
    }

    #[test]
    fn unmatched_parentheses_are_kept_verbatim() {
        assert_eq!(inherits("; inherits: (cpp\n"), vec!["(cpp"]);
        assert_eq!(inherits("; inherits: cpp)\n"), vec!["cpp)"]);
    }

    /// Neovim matches `^;+%s*inherits%s*:?%s*`: any run of semicolons, any
    /// spacing, and the colon is optional.
    #[test]
    fn any_number_of_semicolons_and_an_optional_colon() {
        assert_eq!(inherits(";; inherits: ecma\n"), vec!["ecma"]);
        assert_eq!(inherits(";;; inherits: ecma\n"), vec!["ecma"]);
        assert_eq!(inherits(";inherits:ecma\n"), vec!["ecma"]);
        assert_eq!(inherits("; inherits ecma\n"), vec!["ecma"]);
        assert_eq!(inherits(";  inherits  :  ecma  \n"), vec!["ecma"]);
        assert_eq!(inherits("; inherits ecma, jsx\n"), vec!["ecma", "jsx"]);
    }

    #[test]
    fn a_comment_that_merely_begins_with_the_keyword_is_not_a_directive() {
        assert!(inherits("; inheritsecma\n").is_empty());
        assert!(inherits("; inherits\n").is_empty());
        assert!(!parse_modeline("; extendsfoo\n").extends);
        assert!(!parse_modeline("; extends foo\n").extends);
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
                inherits: vec!["typescript".into(), "jsx".into()],
                extends: true,
            }
        );
        assert_eq!(
            parse_modeline(";; extends\n;;\n;; inherits: css\n(identifier) @variable\n"),
            QueryModeline {
                inherits: vec!["css".into()],
                extends: true,
            }
        );
        assert_eq!(
            inherits("; inherits: c\n; inherits: cpp, c\n"),
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
