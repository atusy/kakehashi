//! Owned, borrow-free injection **discovery** attached to a document's parse
//! result (#529 companion lever — "don't discover twice").
//!
//! The off-ingress `populate_injections` runs the injection query once over a
//! freshly parsed tree and records the result here (regions whose injected
//! language cannot be resolved to a parser are DROPPED from the stored set —
//! sound because a failed load is negative-cached until a reload, which bumps
//! the generation and invalidates this discovery).
//! A later `semanticTokens` request bound to that same tree rebuilds its
//! injection contexts from this owned data instead of re-running the query — the
//! query-match loop (`Q`) is the dominant cost of injection discovery, so
//! skipping the second run on the request path is the whole point of the lever.
//!
//! Everything here is **fully owned**: no borrow of the document text (the
//! content slice is stored as a byte range, re-sliced from the current text at
//! reuse) and no `Arc<Query>` (the highlight query is looked up fresh by
//! `resolved_lang`, so a settings reload can't serve a stale query — see
//! `generation`). This is what lets it ride the
//! [`ParseSnapshot`](super::snapshot::ParseSnapshot) across requests. Reuse
//! is bound to the exact tree it was discovered on by snapshot immutability:
//! text, tree, and regions are one value, so the regions can never be
//! consumed against a different tree.

/// The injection-token-cache identity of a discovered region (#529 token half),
/// or absent when the region isn't token-cacheable (combined groups, or below
/// the region-count gate). The owned twin of the analysis layer's
/// `RegionCacheInfo`, kept here so `document` need not depend on `analysis`.
#[derive(Clone)]
pub(crate) struct DiscoveredRegionCache {
    /// Content hash folded with the resolved language — the injection-token
    /// cache's content-addressed key (parse-snapshot ADR §3 companion
    /// decision), needing no tracker identity at all.
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
/// Stored by the off-ingress write-back with unresolvable regions DROPPED
/// (their injected language has no parser; the inline path would produce no
/// tokens for them either, and the failed load is negative-cached until a
/// reload — which bumps `generation`, invalidating this discovery). A document
/// with an `injection.combined` group stores a PARTIAL discovery (singles
/// only, `complete: false`): the token-cache eviction sweep still needs the
/// singles' cache identities, but context reuse must fall back inline (those
/// whole-group contexts aren't part of the owned form in v1). A resolvable
/// language whose highlight query isn't loaded is *not* excluded — the query
/// is re-resolved fresh at reuse, so that case self-heals rather than
/// persisting an incomplete set.
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
    /// `true` when `regions` covers every top-level injection region of the
    /// parse. `false` when an `injection.combined` group was dropped: the
    /// singles in `regions` are still authoritative token-cache identities
    /// (read/store and the eviction sweep key off them), but the semantic
    /// context-reuse path must not consume a partial region set — it would
    /// silently drop the combined group's tokens.
    pub complete: bool,
    pub regions: Vec<DiscoveredRegion>,
}

/// What one populate pass publishes for the **bridge** downstream
/// (`process_injections` → eager spawn / didChange forwarding / auto-install),
/// per document: either every region resolved with the content the bridge
/// opens downstream — a runnable bridge server handles one of the document's
/// region languages — or the roster of a document nothing routes. The two are
/// distinct shapes on purpose: a roster region has no content to open, and
/// "resolved" is a property of the document, not of a region, so a reader
/// cannot mistake one region's content for its being routed.
#[derive(Clone)]
pub(crate) enum BridgeRegions {
    /// Every region, with its virtual-document content; the pass resolved the
    /// document because a runnable bridge server handles one of its region
    /// languages. A region here is still only routed if the bridge's routing
    /// rule names a server for its language.
    Resolved(std::sync::Arc<Vec<DiscoveredBridgeRegion>>),
    /// Every region's language and identity and nothing else: no runnable
    /// server handles any of the document's region languages (or the
    /// document has no regions), so nothing opens them — the roster still
    /// drives the injected-grammar load and the closing of a virtual document
    /// a region used to have.
    Roster(std::sync::Arc<Vec<BridgeRosterRegion>>),
}

impl BridgeRegions {
    /// The resolved regions, `None` for a roster.
    pub(crate) fn resolved(&self) -> Option<&[DiscoveredBridgeRegion]> {
        match self {
            Self::Resolved(regions) => Some(regions),
            Self::Roster(_) => None,
        }
    }

    /// Whether nothing in the document is routed: a roster.
    pub(crate) fn is_roster(&self) -> bool {
        matches!(self, Self::Roster(_))
    }

    /// Every region's `(language, region_id)`, resolved or on the roster,
    /// borrowed: one of the two chained halves is always empty.
    pub(crate) fn identities(&self) -> impl Iterator<Item = (&str, &str)> {
        let (resolved, roster): (&[DiscoveredBridgeRegion], &[BridgeRosterRegion]) = match self {
            Self::Resolved(regions) => (regions, &[]),
            Self::Roster(regions) => (&[], regions),
        };
        resolved
            .iter()
            .map(|region| (region.language.as_str(), region.region_id.as_str()))
            .chain(
                roster
                    .iter()
                    .map(|region| (region.language.as_str(), region.region_id.as_str())),
            )
    }
}

/// One region of the roster a document nothing routes publishes: its language
/// and identity, enough for the injected-grammar load and the closing of a
/// virtual document it used to have; no content, because nothing opens it.
#[derive(Clone)]
pub(crate) struct BridgeRosterRegion {
    /// The region's canonical language (see [`DiscoveredBridgeRegion::language`]).
    pub language: String,
    /// The region's tracker ULID (see [`DiscoveredBridgeRegion::region_id`]).
    pub region_id: String,
}

/// One discovered injection region in the owned form the **bridge** downstream
/// (`process_injections` → eager spawn / didChange forwarding / auto-install)
/// consumes — the parse-pass twin of `BridgeInjection`, kept here so `document`
/// need not depend on `lsp::bridge`. Built by `populate_injections` from the
/// same single injection-query pass as everything else (never re-discovered on
/// the downstream path), and carried on the `ParseSnapshot` as
/// [`BridgeRegions::Resolved`].
#[derive(Clone)]
pub(crate) struct DiscoveredBridgeRegion {
    /// Resolved injection language, shared by eager lifecycle messages and
    /// interactive request dispatch. Aliases are canonicalized when detection
    /// succeeds (e.g. `"py"` to `"python"`); otherwise this retains the raw
    /// identifier so both paths use the same fallback URI.
    pub language: String,
    /// Tracker ULID shared with the `InjectionMap`, keyed by host geometry plus
    /// the query-pattern/language discriminator so same-range alternatives
    /// remain distinct within the document incarnation.
    pub region_id: String,
    /// The exact virtual-document text the bridge opens downstream (excluded
    /// prefixes removed; an `injection.combined` group preserves host line
    /// numbers with empty lines and uses spaces for later gaps on a line).
    pub content: String,
}

/// One pre-parsed injection layer of a document, in document-order DFS —
/// the owned product of the captures/node layer walk
/// (`walk_document_layers`), built LAZILY by the first walking request on a
/// snapshot (never on the parse critical path) and carried on the
/// `ParseSnapshot` so the per-keystroke `kakehashi/captures` requests (full
/// AND delta both walk) iterate these instead of re-running the injection
/// query, re-resolving every region's language, and re-parsing every
/// injected region per request.
///
/// `tree` is parsed with the exact absolute included ranges the inline walk
/// computes (same code, run at populate time), so consuming these is
/// byte-identical to walking inline over the same snapshot.
#[derive(Clone)]
pub(crate) struct SnapshotLayerTree {
    /// Resolved injection language (e.g. `"markdown_inline"`).
    pub language: String,
    /// The layer's tree, parsed against the host text with the layer's
    /// absolute included ranges (`Tree::included_ranges` recovers them).
    pub tree: tree_sitter::Tree,
    /// Injection depth (host is 0; these start at 1).
    pub depth: usize,
    /// Host-byte span covering the layer's included ranges, for cheap
    /// range-request pruning (conservative: a false-positive visit only makes
    /// the layer's query yield nothing for the clipped range).
    pub span: std::ops::Range<usize>,
}
