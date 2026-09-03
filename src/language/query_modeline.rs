//! The `;` comment modelines Neovim reads at the top of a query file.
//!
//! nvim-treesitter queries declare what they build on through comments such
//! as `; inherits: ecma`; kakehashi reads the same modelines so those query
//! files load unchanged. This module is the one parser for them: the loader,
//! the installer, and the in-repo asset tests all resolve a query's parents
//! through it, so a form accepted in one place is accepted everywhere.

const INHERITS_DIRECTIVE_PREFIX: &str = "; inherits:";

/// The parent languages named by the `; inherits: lang1,lang2` directive on
/// the first line of `content`, in declaration order. Empty without one.
///
/// A parenthesized name (`(cpp)`) is read as the bare name: Neovim uses the
/// parentheses to stop recursion when the file is itself being inherited,
/// which kakehashi does not distinguish.
pub(crate) fn parse_inherits_directive(content: &str) -> Vec<String> {
    let first_line = content.lines().next().unwrap_or("");
    if let Some(rest) = first_line.strip_prefix(INHERITS_DIRECTIVE_PREFIX) {
        rest.split(',')
            .map(|s| normalize_inherited_language_name(s.trim()))
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        Vec::new()
    }
}

/// Whether the first line of `content` is the `; inherits:` directive.
pub(crate) fn starts_with_inherits_directive(content: &str) -> bool {
    content
        .lines()
        .next()
        .is_some_and(|first_line| first_line.starts_with(INHERITS_DIRECTIVE_PREFIX))
}

/// Strip only a *matched* pair of parentheses: on `(cpp` the loader looks for
/// a language literally called `(cpp`, and an installer that helpfully
/// fetched `cpp` would report success over a language it still cannot load.
fn normalize_inherited_language_name(name: &str) -> String {
    name.strip_prefix('(')
        .and_then(|name| name.strip_suffix(')'))
        .unwrap_or(name)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_parent() {
        // TypeScript inherits from ecma
        let content = "; inherits: ecma\n\n\"require\" @keyword.import\n";
        assert_eq!(parse_inherits_directive(content), vec!["ecma"]);
    }

    #[test]
    fn multiple_parents_in_declaration_order() {
        // JavaScript inherits from ecma and jsx
        let content = "; inherits: ecma,jsx\n\n(identifier) @variable\n";
        assert_eq!(parse_inherits_directive(content), vec!["ecma", "jsx"]);
    }

    #[test]
    fn no_directive() {
        assert!(parse_inherits_directive("(identifier) @variable\n").is_empty());
        assert!(parse_inherits_directive("").is_empty());
    }

    #[test]
    fn spaces_around_names() {
        assert_eq!(
            parse_inherits_directive("; inherits: ecma , jsx\n"),
            vec!["ecma", "jsx"]
        );
    }

    #[test]
    fn parenthesized_names_read_as_bare_names() {
        assert_eq!(
            parse_inherits_directive("; inherits: c, (cpp), ( cuda )\n(identifier) @variable\n"),
            vec!["c", "cpp", "cuda"]
        );
    }

    #[test]
    fn unmatched_parentheses_are_kept_verbatim() {
        assert_eq!(parse_inherits_directive("; inherits: (cpp\n"), vec!["(cpp"]);
        assert_eq!(parse_inherits_directive("; inherits: cpp)\n"), vec!["cpp)"]);
    }

    #[test]
    fn directive_is_recognized_on_the_first_line_only() {
        assert!(parse_inherits_directive("(identifier) @variable\n; inherits: ecma\n").is_empty());
        assert!(starts_with_inherits_directive("; inherits: ecma\n"));
        assert!(!starts_with_inherits_directive(
            "(identifier) @variable\n; inherits: ecma\n"
        ));
        assert!(!starts_with_inherits_directive(""));
    }
}
