//! Language injection processing for semantic tokens.
//!
//! This module handles the discovery and recursive processing of language
//! injections (e.g., Lua code blocks inside Markdown).

use std::sync::Arc;

use tree_sitter::Query;

/// Maximum recursion depth for nested injections to prevent stack overflow
pub(super) const MAX_INJECTION_DEPTH: usize = 10;

/// Data for processing a single injection (parser-agnostic).
///
/// This struct captures all the information needed to process an injection
/// before the actual parsing step. Used by parallel injection processing.
pub(super) struct InjectionContext<'a> {
    /// The resolved language name (e.g., "lua", "python")
    pub resolved_lang: String,
    /// The highlight query for this language
    pub highlight_query: Arc<Query>,
    /// The text content of the injection
    pub content_text: &'a str,
    /// Byte offset in the host document where this injection starts
    pub host_start_byte: usize,
    /// Included ranges for the injection parser (content-text-relative).
    /// When `Some`, `Parser::set_included_ranges()` restricts parsing to these
    /// byte ranges, excluding named children like `block_continuation`.
    pub included_ranges: Option<Vec<tree_sitter::Range>>,
    /// Per-content-line byte prefix widths (e.g., `[0, 2, 2, 2]` for blockquote).
    /// Empty when no `included_ranges` (non-blockquote injections).
    pub prefix_byte_widths: Vec<usize>,
}
