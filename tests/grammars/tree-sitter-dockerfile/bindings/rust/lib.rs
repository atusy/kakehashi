//! Dockerfile grammar rebound through `tree_sitter_language::LanguageFn`
//! (see Cargo.toml for why this is vendored).

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_dockerfile() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for this grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dockerfile) };
