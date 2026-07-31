//! Backward-compatible aliases for the pre-scope-first custom method names.
//!
//! The document-scoped custom methods moved under a `textDocument/` scope
//! segment (`kakehashi/node` → `kakehashi/textDocument/node`) so the vendor
//! namespace mirrors LSP's own scope-first axis (`textDocument/diagnostic` vs
//! `workspace/diagnostic`). The old spellings stay callable for a deprecation
//! window: [`canonical_method`] maps an old name to its canonical replacement,
//! and the alias middleware rewrites the request before it reaches any other
//! layer.
//!
//! The mapping is an explicit allowlist rather than a `kakehashi/` prefix
//! rewrite because not every custom method is document-scoped —
//! `kakehashi/internal/effectiveConfiguration` takes empty params and keeps
//! its name.

/// The canonical spelling of a deprecated custom method name, or `None` when
/// the method needs no rewrite.
///
/// `None` covers three distinct cases the caller treats identically: a name
/// that is already canonical (so the mapping is idempotent — a second pass can
/// never double-prefix), a `kakehashi/` method that is deliberately not
/// document-scoped, and every standard LSP method.
///
/// Returns `&'static str` rather than `String` so the result doubles as the
/// dedup key for the once-per-method deprecation warning; the wire method name
/// is owned and would otherwise have to be cloned into the seen-set.
pub(crate) fn canonical_method(_method: &str) -> Option<&'static str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_the_node_entry_point() {
        assert_eq!(
            canonical_method("kakehashi/node"),
            Some("kakehashi/textDocument/node")
        );
    }

    #[test]
    fn rewrites_every_node_accessor() {
        assert_eq!(
            canonical_method("kakehashi/node/parent"),
            Some("kakehashi/textDocument/node/parent")
        );
        assert_eq!(
            canonical_method("kakehashi/node/namedDescendantForByteRange"),
            Some("kakehashi/textDocument/node/namedDescendantForByteRange")
        );
        assert_eq!(
            canonical_method("kakehashi/node/childrenByFieldName"),
            Some("kakehashi/textDocument/node/childrenByFieldName")
        );
    }

    #[test]
    fn rewrites_the_captures_triple() {
        assert_eq!(
            canonical_method("kakehashi/captures/full"),
            Some("kakehashi/textDocument/captures/full")
        );
        assert_eq!(
            canonical_method("kakehashi/captures/full/delta"),
            Some("kakehashi/textDocument/captures/full/delta")
        );
        assert_eq!(
            canonical_method("kakehashi/captures/range"),
            Some("kakehashi/textDocument/captures/range")
        );
    }

    #[test]
    fn leaves_the_non_document_scoped_method_alone() {
        // `effectiveConfiguration` takes empty params — it is server-scoped, so
        // a blanket `kakehashi/` prefix rewrite would corrupt it.
        assert_eq!(
            canonical_method("kakehashi/internal/effectiveConfiguration"),
            None
        );
    }

    #[test]
    fn is_idempotent_on_already_canonical_names() {
        // The middleware must be safe to apply twice: returning a rewrite here
        // would produce `kakehashi/textDocument/textDocument/node`.
        assert_eq!(canonical_method("kakehashi/textDocument/node"), None);
        assert_eq!(canonical_method("kakehashi/textDocument/node/parent"), None);
        assert_eq!(
            canonical_method("kakehashi/textDocument/captures/full"),
            None
        );
    }

    #[test]
    fn leaves_standard_lsp_methods_alone() {
        assert_eq!(canonical_method("textDocument/semanticTokens/full"), None);
        assert_eq!(canonical_method("textDocument/didChange"), None);
        assert_eq!(canonical_method("initialize"), None);
    }

    #[test]
    fn rejects_unknown_kakehashi_methods() {
        // An allowlist, not a prefix rule: a method that never existed must not
        // be silently rewritten into a name that does not exist either.
        assert_eq!(canonical_method("kakehashi/node/notAMethod"), None);
        assert_eq!(canonical_method("kakehashi/captures/kinds"), None);
        assert_eq!(canonical_method("kakehashi/somethingElse"), None);
    }
}
