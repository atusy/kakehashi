//! Cross-snapshot, content-addressed cache of per-layer captures query
//! matches (`kakehashi/captures/*`).
//!
//! The captures walk re-executes every layer's kind query on every new
//! snapshot, but a keystroke leaves most layers' *content* untouched — an
//! edit above a fence only shifts it. This cache keys each layer's
//! [`MatchData`] output by what the query result actually depends on: the
//! kind file, the layer language, and the layer's included ranges (their
//! text AND their geometry relative to the layer anchor). A layer whose
//! content merely moved hits, and the walk skips `execute_query` for it.
//!
//! What is cached is deliberately the ID-free, pre-JSON stage
//! ([`MatchData`]: byte offsets + capture names + `#set!` metadata), stored
//! ANCHOR-RELATIVE. ULID minting, coordinate conversion, and wire shaping
//! run per request in the walk exactly as on a fresh compute — the
//! NodeTracker contract (currency-gated minting, post-walk reconciliation)
//! is untouched, so a cache hit hands out byte-identical ids to the ones a
//! fresh walk would mint. Node `kind`s inside [`MatchData`] are
//! `&'static str` interned in the grammar's loaded library — the same
//! process-lifetime assumption the `NodeTracker` already makes for its
//! `(start, end, kind)` keys.
//!
//! Eviction mirrors the injection-token-cache discipline:
//! - the host slot (depth 0) holds ONE entry per kind and self-replaces on
//!   store — never more than the latest host content per kind;
//! - injected-layer entries are swept per completed, current-at-entry
//!   full-coverage walk (`sweep_layers` retains only the hashes that walk
//!   touched, scoped to its kind so other kinds' entries survive);
//! - `clear_document` drops the document on didClose.
//!
//! Resident bound while a document is open: for each kind that was ever
//! full-walked, its LAST live layer set (a kind the client stops requesting
//! keeps that final set until didClose — kinds are bounded by real `.scm`
//! files, not by client strings, since only `Loaded` queries store). A
//! burst of stale-walk stores can transiently exceed the live set until
//! the next current walk's sweep.
//!
//! The 64-bit validity hash accepts the same collision bound as
//! `region_validity_hash` (semantic token cache): a collision requires two
//! different layer contents in ONE document colliding under FNV-1a while
//! also sharing kind and language — accepted there, accepted here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use url::Url;

use crate::language::query_exec::{CapturedNode, MatchData};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// One included range as the cache key consumes it: byte span plus the
/// COLUMNS tree-sitter was given for its endpoints. Rows are deliberately
/// absent — a vertical shift changes rows but cannot change the parse or
/// what predicates read — but columns can: an indentation-sensitive
/// injected grammar (Python, YAML) parses differently when the range
/// starts at a different column (`content.rs` computes real column
/// offsets for exactly this reason).
pub(in crate::lsp::lsp_impl) struct KeyRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// The cache key of one layer's walk: `(anchor, validity_hash)`.
///
/// `anchor` is the layer's first included-range start — the offset cached
/// matches are stored relative to, and the offset a hit re-adds. The hash
/// folds the kind file name, the layer language, each included range's
/// anchor-relative byte geometry and endpoint columns, and the layer's
/// ENTIRE outer span text (first range start to last range end, gaps
/// included, one contiguous pass). It is invariant under translation (an
/// edit outside the span shifts the anchor and rows, neither of which the
/// parse or the queries can see) but changes whenever any byte the walk
/// could observe changes.
///
/// Gap bytes between ranges must be hashed even though the PARSER never
/// reads them: query predicates (`#match?`, `#eq?`, `#lua-match?`, …)
/// slice the full host text by node span, and a captured node straddling
/// a gap observes the gap bytes — kind queries come from user-configurable
/// search paths, so no grammar-level invariant can rule that out. One
/// contiguous pass over the span is also cheaper than per-range slicing.
///
/// Ranges are clamped to `text.len()` before hashing: a host tree parsed
/// without explicit included ranges reports one `0..u32::MAX`-ish range.
pub(in crate::lsp::lsp_impl) fn layer_cache_key(
    kind: &str,
    language: &str,
    ranges: impl IntoIterator<Item = KeyRange>,
    text: &str,
) -> (usize, u64) {
    let mut hash = FNV_OFFSET;
    fnv1a(&mut hash, kind.as_bytes());
    // Domain separator between the two variable-length prefix fields (kind
    // is charset-validated and cannot contain 0xFF; language names cannot
    // either, both being path components).
    fnv1a(&mut hash, &[0xFF]);
    fnv1a(&mut hash, language.as_bytes());
    let mut anchor = None;
    let (mut span_start, mut span_end) = (usize::MAX, 0usize);
    for range in ranges {
        let start = range.start_byte.min(text.len());
        let end = range.end_byte.min(text.len());
        let base = *anchor.get_or_insert(start);
        span_start = span_start.min(start.min(end));
        span_end = span_end.max(end);
        // Separator + relative geometry: two range lists with the same
        // span content but different range boundaries must not collide.
        fnv1a(&mut hash, &[0xFE]);
        fnv1a(&mut hash, &(start.wrapping_sub(base) as u64).to_le_bytes());
        fnv1a(&mut hash, &(end.wrapping_sub(base) as u64).to_le_bytes());
        fnv1a(&mut hash, &(range.start_col as u64).to_le_bytes());
        fnv1a(&mut hash, &(range.end_col as u64).to_le_bytes());
    }
    // The whole outer span in ONE contiguous pass — gaps included, see the
    // predicate note above. (`span_start` guards the degenerate empty-range
    // iterator, where it stays at usize::MAX.)
    if span_start < span_end {
        fnv1a(&mut hash, &text.as_bytes()[span_start..span_end]);
    }
    (anchor.unwrap_or(0), hash)
}

/// [`layer_cache_key`] over a parsed layer tree — `Tree::included_ranges`
/// recovers the exact absolute ranges (bytes AND points) the layer was
/// parsed with (for a host tree, the whole document).
pub(in crate::lsp::lsp_impl) fn tree_cache_key(
    kind: &str,
    language: &str,
    tree: &tree_sitter::Tree,
    text: &str,
) -> (usize, u64) {
    layer_cache_key(
        kind,
        language,
        tree.included_ranges().iter().map(|r| KeyRange {
            start_byte: r.start_byte,
            end_byte: r.end_byte,
            start_col: r.start_point.column,
            end_col: r.end_point.column,
        }),
        text,
    )
}

/// Shift every capture offset down by `anchor`, in place. Returns `false`
/// (leaving the matches untouched) if any capture starts below the anchor —
/// a tree whose root node reaches outside its included ranges must simply
/// not be cached, never corrupted by a wrapping subtraction. Ignoring the
/// verdict would cache un-rebased (absolute) offsets, hence `must_use`.
#[must_use]
pub(in crate::lsp::lsp_impl) fn rebase_matches(matches: &mut [MatchData], anchor: usize) -> bool {
    if matches
        .iter()
        .flat_map(|m| &m.captures)
        .any(|c| c.start_byte < anchor)
    {
        return false;
    }
    for m in matches {
        for c in &mut m.captures {
            // The guard above makes plain `-` safe for both offsets
            // (end >= start >= anchor by construction).
            c.start_byte -= anchor;
            c.end_byte -= anchor;
        }
    }
    true
}

/// One cached host-layer walk (depth 0). Keyed by kind in [`DocMatchCache`];
/// the validity hash lives in the entry because the slot self-replaces —
/// one host content per kind, no sweep needed.
struct CachedHostMatches {
    validity_hash: u64,
    generation: u64,
    matches: Arc<Vec<MatchData>>,
}

/// One cached injected-layer walk (depth > 0). Keyed by validity hash; the
/// kind is carried for the per-kind sweep scope.
struct CachedLayerMatches {
    kind: String,
    generation: u64,
    matches: Arc<Vec<MatchData>>,
}

#[derive(Default)]
struct DocMatchCache {
    host: HashMap<String, CachedHostMatches>,
    layers: HashMap<u64, CachedLayerMatches>,
}

/// See the module docs. One instance lives on the server, shared with the
/// compute-pool walks by `Arc`.
#[derive(Default)]
pub(crate) struct CapturesMatchCache {
    cache: dashmap::DashMap<Url, DocMatchCache>,
}

fn estimated_metadata_bytes(metadata: &[(String, Option<String>)], capacity: usize) -> usize {
    (capacity * std::mem::size_of::<(String, Option<String>)>()).saturating_add(
        metadata.iter().fold(0, |bytes, (key, value)| {
            bytes
                .saturating_add(key.capacity())
                .saturating_add(value.as_ref().map(String::capacity).unwrap_or_default())
        }),
    )
}

fn estimated_match_bytes(matches: &[MatchData], capacity: usize) -> usize {
    std::mem::size_of::<Vec<MatchData>>()
        .saturating_add(capacity * std::mem::size_of::<MatchData>())
        .saturating_add(matches.iter().fold(0, |bytes, matched| {
            let captures = matched
                .captures
                .iter()
                .fold(0_usize, |capture_bytes, capture| {
                    capture_bytes
                        .saturating_add(capture.name.capacity())
                        .saturating_add(estimated_metadata_bytes(
                            &capture.metadata,
                            capture.metadata.capacity(),
                        ))
                });
            bytes
                .saturating_add(matched.captures.capacity() * std::mem::size_of::<CapturedNode>())
                .saturating_add(captures)
                .saturating_add(estimated_metadata_bytes(
                    &matched.metadata,
                    matched.metadata.capacity(),
                ))
        }))
}

impl CapturesMatchCache {
    pub(in crate::lsp::lsp_impl) fn new() -> Self {
        Self::default()
    }

    pub(in crate::lsp::lsp_impl) fn get_host(
        &self,
        uri: &Url,
        kind: &str,
        validity_hash: u64,
        generation: u64,
    ) -> Option<Arc<Vec<MatchData>>> {
        let doc = self.cache.get(uri)?;
        let hit = doc.host.get(kind)?;
        (hit.validity_hash == validity_hash && hit.generation == generation)
            .then(|| Arc::clone(&hit.matches))
    }

    pub(in crate::lsp::lsp_impl) fn store_host(
        &self,
        uri: &Url,
        kind: &str,
        validity_hash: u64,
        generation: u64,
        matches: Arc<Vec<MatchData>>,
        doc_open: impl FnOnce() -> bool,
    ) {
        let entry = CachedHostMatches {
            validity_hash,
            generation,
            matches,
        };
        // get_mut before entry: the steady-state store hits an existing doc
        // and skips the owned-key clones the entry APIs require (the `Url`
        // here, the kind `String` below).
        if let Some(mut doc) = self.cache.get_mut(uri) {
            if let Some(slot) = doc.host.get_mut(kind) {
                *slot = entry;
            } else {
                doc.host.insert(kind.to_string(), entry);
            }
            return;
        }
        // Creating the doc slot needs a liveness handshake: an airborne walk
        // whose document was closed mid-flight must not resurrect the entry
        // `clear_document` just dropped — nothing would ever reclaim it (the
        // pre-existing walk memo shares this race; its entries are small,
        // these hold full match sets). INSERT-THEN-VERIFY, paired with
        // didClose's `documents.remove` → `clear_document` ordering, closes
        // the leak entirely (codex review): if the post-insert probe sees
        // the document open, the remove has not run yet, so the close's
        // later `clear_document` reclaims this insert; if it sees it
        // closed, we drop the slot ourselves. (A probe-BEFORE-insert had a
        // TOCTOU hole: close landing between check and insert leaked.) The
        // self-remove can transiently drop a reopen-race walk's fresh
        // entries — a recomputable miss, never staleness. `doc_open` runs
        // only on this create path; the steady state never probes.
        self.cache
            .entry(uri.clone())
            .or_default()
            .host
            .insert(kind.to_string(), entry);
        if !doc_open() {
            self.cache.remove(uri);
        }
    }

    pub(in crate::lsp::lsp_impl) fn get_layer(
        &self,
        uri: &Url,
        validity_hash: u64,
        generation: u64,
    ) -> Option<Arc<Vec<MatchData>>> {
        let doc = self.cache.get(uri)?;
        let hit = doc.layers.get(&validity_hash)?;
        (hit.generation == generation).then(|| Arc::clone(&hit.matches))
    }

    pub(in crate::lsp::lsp_impl) fn store_layer(
        &self,
        uri: &Url,
        kind: &str,
        validity_hash: u64,
        generation: u64,
        matches: Arc<Vec<MatchData>>,
        doc_open: impl FnOnce() -> bool,
    ) {
        if let Some(mut doc) = self.cache.get_mut(uri) {
            // In-place refresh keeps the existing kind `String`: kind is
            // folded into the validity hash, so a same-hash entry already
            // carries this kind (modulo the accepted 64-bit bound).
            if let Some(slot) = doc.layers.get_mut(&validity_hash) {
                slot.generation = generation;
                slot.matches = matches;
            } else {
                doc.layers.insert(
                    validity_hash,
                    CachedLayerMatches {
                        kind: kind.to_string(),
                        generation,
                        matches,
                    },
                );
            }
            return;
        }
        let entry = CachedLayerMatches {
            kind: kind.to_string(),
            generation,
            matches,
        };
        // See `store_host`: create-path insert-then-verify handshake
        // against the close-during-walk resurrection leak.
        self.cache
            .entry(uri.clone())
            .or_default()
            .layers
            .insert(validity_hash, entry);
        if !doc_open() {
            self.cache.remove(uri);
        }
    }

    /// Retain, for `kind`, only the layer entries a completed full-coverage
    /// walk touched; other kinds' entries are untouched. The host slot needs
    /// no sweep (it self-replaces per kind).
    pub(in crate::lsp::lsp_impl) fn sweep_layers(
        &self,
        uri: &Url,
        kind: &str,
        touched: &HashSet<u64>,
    ) {
        if let Some(mut doc) = self.cache.get_mut(uri) {
            doc.layers
                .retain(|hash, entry| entry.kind != kind || touched.contains(hash));
        }
    }

    pub(crate) fn clear_document(&self, uri: &Url) {
        self.cache.remove(uri);
    }

    pub(crate) fn estimated_document_bytes(&self, uri: &Url) -> usize {
        self.cache.get(uri).map_or(0, |document| {
            let host = document
                .host
                .capacity()
                .saturating_mul(std::mem::size_of::<(String, CachedHostMatches)>());
            let layers = document
                .layers
                .capacity()
                .saturating_mul(std::mem::size_of::<(u64, CachedLayerMatches)>());
            let host = document.host.iter().fold(host, |bytes, (kind, cached)| {
                bytes
                    .saturating_add(kind.capacity())
                    .saturating_add(estimated_match_bytes(
                        cached.matches.as_ref(),
                        cached.matches.capacity(),
                    ))
            });
            document
                .layers
                .values()
                .fold(host.saturating_add(layers), |bytes, cached| {
                    bytes.saturating_add(cached.kind.capacity()).saturating_add(
                        estimated_match_bytes(cached.matches.as_ref(), cached.matches.capacity()),
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::query_exec::CapturedNode;

    fn match_data(captures: &[(usize, usize)]) -> MatchData {
        MatchData {
            pattern_index: 0,
            captures: captures
                .iter()
                .map(|&(start_byte, end_byte)| CapturedNode {
                    name: "cap".to_string(),
                    start_byte,
                    end_byte,
                    kind: "identifier",
                    metadata: Vec::new(),
                })
                .collect(),
            metadata: Vec::new(),
        }
    }

    fn uri() -> Url {
        Url::parse("file:///match_cache.md").unwrap()
    }

    /// A `KeyRange` with column-0 endpoints — the common fence shape.
    fn kr(start_byte: usize, end_byte: usize) -> KeyRange {
        KeyRange {
            start_byte,
            end_byte,
            start_col: 0,
            end_col: 0,
        }
    }

    #[test]
    fn key_is_deterministic_and_separates_kind_and_language() {
        let text = "abcdefghij";
        let base = layer_cache_key("highlights", "lua", [kr(2, 8)], text);
        assert_eq!(
            base,
            layer_cache_key("highlights", "lua", [kr(2, 8)], text),
            "same inputs -> same key"
        );
        assert_ne!(
            base.1,
            layer_cache_key("locals", "lua", [kr(2, 8)], text).1,
            "kind separates"
        );
        assert_ne!(
            base.1,
            layer_cache_key("highlights", "vim", [kr(2, 8)], text).1,
            "language separates"
        );
    }

    #[test]
    fn key_is_translation_invariant_but_geometry_sensitive() {
        // Same content, shifted: the anchor moves, the hash does not — this
        // IS the cross-snapshot hit ("edit above the fence").
        let (anchor_a, hash_a) = layer_cache_key("h", "lua", [kr(2, 5), kr(7, 9)], "..abc..de.");
        let (anchor_b, hash_b) =
            layer_cache_key("h", "lua", [kr(5, 8), kr(10, 12)], ".....abc..de..");
        assert_eq!(hash_a, hash_b, "translated identical layer must hit");
        assert_eq!((anchor_a, anchor_b), (2, 5));

        // Same concatenated content ("abcde"), different gap: the layer tree
        // was parsed over different geometry, so the hash must differ.
        let (_, gap_moved) = layer_cache_key("h", "lua", [kr(2, 6), kr(8, 9)], "..abcd..e.");
        assert_ne!(hash_a, gap_moved, "gap geometry must separate");

        // Different content, same geometry.
        let (_, content_changed) = layer_cache_key("h", "lua", [kr(2, 5), kr(7, 9)], "..abX..de.");
        assert_ne!(hash_a, content_changed, "content must separate");
    }

    /// Endpoint columns are parse inputs for indentation-sensitive grammars
    /// (and a gap's internal newline can move a later range's column without
    /// touching any hashed byte) — same bytes at a different column must not
    /// alias.
    #[test]
    fn key_separates_endpoint_columns() {
        let text = "..abc..de.";
        let base = layer_cache_key("h", "python", [kr(2, 5), kr(7, 9)], text);
        let indented = layer_cache_key(
            "h",
            "python",
            [
                KeyRange {
                    start_byte: 2,
                    end_byte: 5,
                    start_col: 4,
                    end_col: 0,
                },
                kr(7, 9),
            ],
            text,
        );
        assert_ne!(base.1, indented.1, "first-range start column separates");
        let gap_newline_moved = layer_cache_key(
            "h",
            "python",
            [
                kr(2, 5),
                KeyRange {
                    start_byte: 7,
                    end_byte: 9,
                    start_col: 2,
                    end_col: 0,
                },
            ],
            text,
        );
        assert_ne!(base.1, gap_newline_moved.1, "later-range column separates");
    }

    /// Gap bytes are predicate-observable (a captured node straddling a gap
    /// is sliced from the FULL host text), so a gap-only mutation that
    /// leaves every included range's text, geometry, and columns identical
    /// must still miss (codex review).
    #[test]
    fn key_separates_gap_bytes() {
        let ranges = || [kr(2, 5), kr(7, 9)];
        let (_, base) = layer_cache_key("h", "lua", ranges(), "..abc..de.");
        let (_, gap_mutated) = layer_cache_key("h", "lua", ranges(), "..abcX.de.");
        assert_ne!(base, gap_mutated, "gap-only mutation must separate");
    }

    #[test]
    fn key_clamps_ranges_to_text_length() {
        // A host tree parsed without explicit ranges reports the sentinel
        // range `0..u32::MAX` with `end_point (u32::MAX, u32::MAX)`. Bytes
        // clamp to the text, so the hashed CONTENT is the whole document;
        // columns are folded as-given (store and lookup both key off the
        // same tree, so the sentinel end column is consistent) — the key
        // must be deterministic for that real host shape.
        let text = "fn main() {}";
        let host_sentinel = || KeyRange {
            start_byte: 0,
            end_byte: u32::MAX as usize,
            start_col: 0,
            end_col: u32::MAX as usize,
        };
        let key = layer_cache_key("h", "rust", [host_sentinel()], text);
        assert_eq!(
            key,
            layer_cache_key("h", "rust", [host_sentinel()], text),
            "the host sentinel shape keys deterministically"
        );
        assert_eq!(key.0, 0, "host anchors at 0");

        // Byte clamping: an oversized end with the SAME columns hashes the
        // same content as the exact span.
        let clamped = layer_cache_key("h", "rust", [kr(0, u32::MAX as usize)], text);
        let whole = layer_cache_key("h", "rust", [kr(0, text.len())], text);
        assert_eq!(clamped, whole);
    }

    #[test]
    fn rebase_shifts_offsets_and_rejects_sub_anchor_captures() {
        let mut ok = vec![match_data(&[(10, 14), (12, 13)])];
        assert!(rebase_matches(&mut ok, 10));
        assert_eq!(ok[0].captures[0].start_byte, 0);
        assert_eq!(ok[0].captures[0].end_byte, 4);
        assert_eq!(ok[0].captures[1].start_byte, 2);

        // A capture below the anchor (root node reaching outside the layer's
        // included ranges) must refuse the rebase and leave data untouched.
        let mut bad = vec![match_data(&[(10, 14)]), match_data(&[(4, 6)])];
        assert!(!rebase_matches(&mut bad, 10));
        assert_eq!(bad[0].captures[0].start_byte, 10, "untouched on refusal");
    }

    #[test]
    fn host_slot_checks_hash_and_generation_and_self_replaces() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let matches = Arc::new(vec![match_data(&[(0, 3)])]);
        cache.store_host(&uri, "highlights", 111, 7, Arc::clone(&matches), || true);

        assert!(cache.get_host(&uri, "highlights", 111, 7).is_some());
        assert!(
            cache.get_host(&uri, "highlights", 222, 7).is_none(),
            "content hash mismatch must miss"
        );
        assert!(
            cache.get_host(&uri, "highlights", 111, 8).is_none(),
            "settings generation mismatch must miss"
        );
        assert!(
            cache.get_host(&uri, "locals", 111, 7).is_none(),
            "kinds have independent host slots"
        );

        // Self-replacement: the new content evicts the old, per kind.
        cache.store_host(&uri, "highlights", 333, 7, matches, || true);
        assert!(cache.get_host(&uri, "highlights", 111, 7).is_none());
        assert!(cache.get_host(&uri, "highlights", 333, 7).is_some());
    }

    #[test]
    fn layer_entries_sweep_per_kind_by_touched_set() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let matches = Arc::new(vec![match_data(&[(0, 3)])]);
        cache.store_layer(&uri, "highlights", 1, 7, Arc::clone(&matches), || true);
        cache.store_layer(&uri, "highlights", 2, 7, Arc::clone(&matches), || true);
        cache.store_layer(&uri, "locals", 3, 7, Arc::clone(&matches), || true);

        // A completed highlights walk touched only hash 1 (the fence at
        // hash 2 was deleted): 2 goes, 1 stays, the other kind's 3 stays.
        cache.sweep_layers(&uri, "highlights", &HashSet::from([1]));
        assert!(cache.get_layer(&uri, 1, 7).is_some());
        assert!(cache.get_layer(&uri, 2, 7).is_none(), "untouched swept");
        assert!(cache.get_layer(&uri, 3, 7).is_some(), "other kind kept");

        assert!(
            cache.get_layer(&uri, 1, 8).is_none(),
            "settings generation mismatch must miss"
        );
    }

    /// The close-during-walk resurrection guard: a store that CREATES the
    /// per-document slot verifies `doc_open` AFTER the insert and drops the
    /// slot for a closed document (insert-then-verify — see `store_host`);
    /// a store into an EXISTING slot never probes (the steady-state path
    /// stays closure-free).
    #[test]
    fn store_into_missing_doc_slot_requires_liveness() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let matches = Arc::new(vec![match_data(&[(0, 3)])]);

        // Closed document (doc_open = false): neither store may resurrect.
        cache.store_host(&uri, "highlights", 1, 7, Arc::clone(&matches), || false);
        cache.store_layer(&uri, "highlights", 2, 7, Arc::clone(&matches), || false);
        assert!(cache.get_host(&uri, "highlights", 1, 7).is_none());
        assert!(cache.get_layer(&uri, 2, 7).is_none());

        // Open document: the create path proceeds...
        cache.store_host(&uri, "highlights", 1, 7, Arc::clone(&matches), || true);
        assert!(cache.get_host(&uri, "highlights", 1, 7).is_some());
        // ...and once the slot exists, stores skip the probe entirely.
        cache.store_layer(&uri, "highlights", 2, 7, matches, || {
            panic!("existing doc slot must not probe liveness")
        });
        assert!(cache.get_layer(&uri, 2, 7).is_some());
    }

    #[test]
    fn cache_reports_document_match_capacity() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let mut matches = Vec::with_capacity(32);
        matches.push(match_data(&[(0, 3)]));
        let match_bytes = matches.capacity() * std::mem::size_of::<MatchData>();
        cache.store_host(&uri, "highlights", 1, 7, Arc::new(matches), || true);

        assert!(cache.estimated_document_bytes(&uri) >= match_bytes);
    }

    #[test]
    fn clear_document_drops_everything_for_the_uri() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let matches = Arc::new(vec![match_data(&[(0, 3)])]);
        cache.store_host(&uri, "highlights", 1, 7, Arc::clone(&matches), || true);
        cache.store_layer(&uri, "highlights", 2, 7, matches, || true);

        cache.clear_document(&uri);
        assert!(cache.get_host(&uri, "highlights", 1, 7).is_none());
        assert!(cache.get_layer(&uri, 2, 7).is_none());
    }
}
