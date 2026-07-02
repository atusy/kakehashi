//! Owned, borrow-free injection **discovery** attached to a document's parse
//! result (#529 companion lever — "don't discover twice").
//!
//! The off-ingress `populate_injections` runs the injection query once over a
//! freshly parsed tree and, when discovery is complete, records the result here
//! via [`DocumentStore::set_injections_if_epoch_unchanged`](crate::document::DocumentStore).
//! A later `semanticTokens` request bound to that same tree rebuilds its
//! injection contexts from this owned data instead of re-running the query — the
//! query-match loop (`Q`) is the dominant cost of injection discovery, so
//! skipping the second run on the request path is the whole point of the lever.
//!
//! Everything here is **fully owned**: no borrow of the document text (the
//! content slice is stored as a byte range, re-sliced from the current text at
//! reuse) and no `Arc<Query>` (the highlight query is looked up fresh by
//! `resolved_lang`, so a settings reload can't serve a stale query — see
//! `generation`). This is what lets it live on the [`Document`](super::Document)
//! across requests. Reuse is bound to the exact tree it was discovered on by the
//! document's `parse_epoch`; see the write-back CAS.

/// The injection-token-cache identity of a discovered region (#529 token half),
/// or absent when the region isn't token-cacheable (combined groups, or below
/// the region-count gate). The owned twin of the analysis layer's
/// `RegionCacheInfo`, kept here so `document` need not depend on `analysis`.
#[derive(Clone)]
pub(crate) struct DiscoveredRegionCache {
    /// Position-stable region id, byte-identical to the one `populate_injections`
    /// minted for this region in the `InjectionMap` (the discovery build reuses
    /// that same id — no new off-ingress minting).
    pub region_id: String,
    /// Content hash folded with the resolved language (the injection-token
    /// cache's per-region validity key).
    pub validity_hash: u64,
    /// The region's first host line, added back on token re-anchor.
    pub line_start: u32,
    /// Whether the region satisfied the token-reuse translation predicate
    /// (`start_column == 0` ∧ no per-row prefixes ∧ not combined) at discovery.
    pub eligible: bool,
}

/// One discovered top-level injection region, in the owned form the semantic
/// path rebuilds an `InjectionContext` from. Mirrors the fields
/// `collect_injection_contexts_sync` computes, minus the borrowed `content_text`
/// (stored as `content_start_byte..content_end_byte`) and the `Arc<Query>`
/// (re-resolved from `resolved_lang`).
#[derive(Clone)]
pub(crate) struct DiscoveredRegion {
    /// Resolved injection language (e.g. `"lua"`); the highlight query is looked
    /// up fresh from this at reuse, never persisted.
    pub resolved_lang: String,
    /// Content byte range, relative to the text the discovery ran over
    /// (offset-directive applied): the injection content is
    /// `&text[content_start_byte..content_end_byte]`, and the host start byte is
    /// `content_start_byte_of_the_pass + content_start_byte`.
    pub content_start_byte: usize,
    pub content_end_byte: usize,
    /// Included ranges for the injection parser (content-relative), or `None`
    /// when the whole content is parsed. Owned so it survives the request.
    pub included_ranges: Option<Vec<tree_sitter::Range>>,
    /// Per-content-line byte prefix widths (empty when no `included_ranges`).
    pub prefix_byte_widths: Vec<usize>,
    /// Token-cache identity, or `None` when the region isn't token-cacheable.
    pub token_cache: Option<DiscoveredRegionCache>,
}

/// The complete owned injection discovery for one parse of a document: every
/// top-level single region plus the settings `generation` it was resolved under.
///
/// Stored on the [`Document`](super::Document) by the off-ingress write-back only
/// when discovery was *complete* (no region dropped because its injected language
/// couldn't be resolved to a parser) and the document had no `injection.combined`
/// group (those keep the inline path in v1), so a present value is always the full
/// single-region set for the bound tree. A resolvable language whose highlight
/// query isn't loaded is *not* excluded — the query is re-resolved fresh at reuse,
/// so that case self-heals rather than persisting an incomplete set.
#[derive(Clone)]
pub(crate) struct DiscoveredInjections {
    /// Settings generation at discovery time. The reader skips reuse when it no
    /// longer matches the current generation — a reload rebuilt the injection /
    /// highlight queries, so the owned contexts (and the language resolution
    /// behind them) may be stale. One integer compare; the same discipline the
    /// injection-token cache key uses. (On-`Document` discovery is not reached by
    /// `bump_semantic_token_generation`, which clears the side caches only, so
    /// this gate is what a reload relies on.)
    pub generation: u64,
    pub regions: Vec<DiscoveredRegion>,
}

/// One discovered injection region in the owned form the **bridge** downstream
/// (`process_injections` → eager spawn / didChange forwarding / auto-install)
/// consumes — the parse-pass twin of `BridgeInjection`, kept here so `document`
/// need not depend on `lsp::bridge`. Built by `populate_injections` from the
/// same single injection-query pass as everything else (never re-discovered on
/// the downstream path), and carried on the `ParseSnapshot`.
#[derive(Clone)]
pub(crate) struct DiscoveredBridgeRegion {
    /// The RAW injection language identifier (e.g. `"py"`), exactly as the
    /// query captured it — bridge config lookup and auto-install resolve it
    /// themselves, so no canonicalization here (identical to the inline path).
    pub raw_language: String,
    /// Position-stable tracker ULID, byte-identical to the one minted for the
    /// `InjectionMap` (same `get_or_create` on the same node).
    pub region_id: String,
    /// The region's clean content (gap ranges removed), the exact virtual-
    /// document text the bridge opens downstream.
    pub content: String,
}
