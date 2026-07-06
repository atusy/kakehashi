//! Language injection processing for semantic tokens.
//!
//! This module handles the discovery and recursive processing of language
//! injections (e.g., Lua code blocks inside Markdown).

use std::sync::Arc;

use tree_sitter::Query;

/// Cache identity for a top-level injection region (#529).
///
/// Populated only for top-level singles when injection-token caching is active
/// (a cache handle is threaded in and the region count clears the gate). Carries
/// everything the reuse path needs without re-touching the tree-sitter node:
/// the position-stable `region_id` (minted the same way as `populate_injections`),
/// the content hash + the region's first host line for re-anchoring, and whether
/// the region satisfies the language-agnostic translation predicate this request.
pub(super) struct RegionCacheInfo {
    /// Content-addressed cache key ([`region_validity_hash`]): the region's
    /// content hash folded with its resolved injection language. The content
    /// half distinguishes an edit; the language fold means identical bytes
    /// injecting *different* languages get different keys (barring a 64-bit
    /// collision), so one can't serve the other's tokens.
    pub validity_hash: u64,
    /// `line_range.start`: the region's first host line, added back on reuse
    /// (`host_line = line_start + token.line`).
    pub line_start: u32,
    /// Whether this region qualifies for the trivial re-anchor this request:
    /// `start_column == 0` ∧ no per-row prefix widths ∧ not `injection.combined`.
    /// Recomputed every request from the fresh context, so a region that stopped
    /// qualifying is recomputed rather than served a stale hit.
    pub eligible: bool,
}

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
    /// Whether this context merges multiple `injection.combined` regions into
    /// one parse (#187). Combined contexts force per-line token emission and
    /// clip tokens to `included_ranges`, because a capture node may span the
    /// excluded text between blocks.
    pub combined: bool,
    /// Cache identity for the injection-token cache (#529), or `None` when this
    /// context is nested, combined, or caching is gated off for this request.
    /// Index-aligned with the per-context token results so the store/reuse
    /// decision is made single-threaded outside the Rayon fan-out.
    pub region_cache: Option<RegionCacheInfo>,
}
