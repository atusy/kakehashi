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
//! - injected-layer entries are swept per completed full-coverage walk
//!   (`sweep_layers` retains only the hashes that walk touched, scoped to
//!   its kind so other kinds' entries survive);
//! - `clear_document` drops the document on didClose.
//!
//! The 64-bit validity hash accepts the same collision bound as
//! `region_validity_hash` (semantic token cache): a collision requires two
//! different layer contents in ONE document colliding under FNV-1a while
//! also sharing kind and language — accepted there, accepted here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use url::Url;

use crate::language::query_exec::MatchData;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// The cache key of one layer's walk: `(anchor, validity_hash)`.
///
/// `anchor` is the layer's first included-range start — the offset cached
/// matches are stored relative to, and the offset a hit re-adds. The hash
/// folds the kind file name, the layer language, and each included range's
/// anchor-relative geometry plus its text, so it is translation-invariant
/// (an edit above the layer shifts the anchor, not the hash) but changes
/// whenever the layer's visible bytes or its internal gap structure change.
///
/// Ranges are clamped to `text.len()` before hashing: a host tree parsed
/// without explicit included ranges reports one `0..u32::MAX`-ish range.
pub(in crate::lsp::lsp_impl) fn layer_cache_key(
    kind: &str,
    language: &str,
    ranges: impl IntoIterator<Item = (usize, usize)>,
    text: &str,
) -> (usize, u64) {
    let mut hash = FNV_OFFSET;
    fnv1a(&mut hash, kind.as_bytes());
    fnv1a(&mut hash, &[0xFF]);
    fnv1a(&mut hash, language.as_bytes());
    let mut anchor = None;
    for (start, end) in ranges {
        let start = start.min(text.len());
        let end = end.min(text.len());
        let anchor = *anchor.get_or_insert(start);
        // Separator + relative geometry: two range lists with the same
        // concatenated content but different gaps must not collide.
        fnv1a(&mut hash, &[0xFE]);
        fnv1a(
            &mut hash,
            &(start.wrapping_sub(anchor) as u64).to_le_bytes(),
        );
        fnv1a(&mut hash, &(end.wrapping_sub(anchor) as u64).to_le_bytes());
        fnv1a(&mut hash, &text.as_bytes()[start.min(end)..end]);
    }
    (anchor.unwrap_or(0), hash)
}

/// [`layer_cache_key`] over a parsed layer tree — `Tree::included_ranges`
/// recovers the exact absolute ranges the layer was parsed with (for a host
/// tree, the whole document).
pub(in crate::lsp::lsp_impl) fn tree_cache_key(
    kind: &str,
    language: &str,
    tree: &tree_sitter::Tree,
    text: &str,
) -> (usize, u64) {
    layer_cache_key(
        kind,
        language,
        tree.included_ranges()
            .iter()
            .map(|r| (r.start_byte, r.end_byte)),
        text,
    )
}

/// Shift every capture offset down by `anchor`, in place. Returns `false`
/// (leaving the matches untouched) if any capture starts below the anchor —
/// a tree whose root node reaches outside its included ranges must simply
/// not be cached, never corrupted by a wrapping subtraction.
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
            c.start_byte -= anchor;
            c.end_byte = c.end_byte.saturating_sub(anchor);
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
pub(in crate::lsp::lsp_impl) struct CapturesMatchCache {
    cache: dashmap::DashMap<Url, DocMatchCache>,
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
    ) {
        let entry = CachedHostMatches {
            validity_hash,
            generation,
            matches,
        };
        // get_mut before entry: the steady-state store hits an existing doc
        // and skips the owned-key clone the entry API requires.
        if let Some(mut doc) = self.cache.get_mut(uri) {
            doc.host.insert(kind.to_string(), entry);
            return;
        }
        self.cache
            .entry(uri.clone())
            .or_default()
            .host
            .insert(kind.to_string(), entry);
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
        validity_hash: u64,
        kind: &str,
        generation: u64,
        matches: Arc<Vec<MatchData>>,
    ) {
        let entry = CachedLayerMatches {
            kind: kind.to_string(),
            generation,
            matches,
        };
        if let Some(mut doc) = self.cache.get_mut(uri) {
            doc.layers.insert(validity_hash, entry);
            return;
        }
        self.cache
            .entry(uri.clone())
            .or_default()
            .layers
            .insert(validity_hash, entry);
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

    pub(in crate::lsp::lsp_impl) fn clear_document(&self, uri: &Url) {
        self.cache.remove(uri);
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

    #[test]
    fn key_is_deterministic_and_separates_kind_and_language() {
        let text = "abcdefghij";
        let base = layer_cache_key("highlights", "lua", [(2, 8)], text);
        assert_eq!(
            base,
            layer_cache_key("highlights", "lua", [(2, 8)], text),
            "same inputs -> same key"
        );
        assert_ne!(
            base.1,
            layer_cache_key("locals", "lua", [(2, 8)], text).1,
            "kind separates"
        );
        assert_ne!(
            base.1,
            layer_cache_key("highlights", "vim", [(2, 8)], text).1,
            "language separates"
        );
    }

    #[test]
    fn key_is_translation_invariant_but_geometry_sensitive() {
        // Same content, shifted: the anchor moves, the hash does not — this
        // IS the cross-snapshot hit ("edit above the fence").
        let (anchor_a, hash_a) = layer_cache_key("h", "lua", [(2, 5), (7, 9)], "..abc..de.");
        let (anchor_b, hash_b) = layer_cache_key("h", "lua", [(5, 8), (10, 12)], ".....abc..de..");
        assert_eq!(hash_a, hash_b, "translated identical layer must hit");
        assert_eq!((anchor_a, anchor_b), (2, 5));

        // Same concatenated content ("abcde"), different gap: the layer tree
        // was parsed over different geometry, so the hash must differ.
        let (_, gap_moved) = layer_cache_key("h", "lua", [(2, 6), (8, 9)], "..abcd..e.");
        assert_ne!(hash_a, gap_moved, "gap geometry must separate");

        // Different content, same geometry.
        let (_, content_changed) = layer_cache_key("h", "lua", [(2, 5), (7, 9)], "..abX..de.");
        assert_ne!(hash_a, content_changed, "content must separate");
    }

    #[test]
    fn key_clamps_ranges_to_text_length() {
        // A host tree parsed without explicit ranges reports ~0..u32::MAX;
        // the key must behave as "whole document".
        let text = "fn main() {}";
        let clamped = layer_cache_key("h", "rust", [(0, u32::MAX as usize)], text);
        let whole = layer_cache_key("h", "rust", [(0, text.len())], text);
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
        cache.store_host(&uri, "highlights", 111, 7, Arc::clone(&matches));

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
        cache.store_host(&uri, "highlights", 333, 7, matches);
        assert!(cache.get_host(&uri, "highlights", 111, 7).is_none());
        assert!(cache.get_host(&uri, "highlights", 333, 7).is_some());
    }

    #[test]
    fn layer_entries_sweep_per_kind_by_touched_set() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let matches = Arc::new(vec![match_data(&[(0, 3)])]);
        cache.store_layer(&uri, 1, "highlights", 7, Arc::clone(&matches));
        cache.store_layer(&uri, 2, "highlights", 7, Arc::clone(&matches));
        cache.store_layer(&uri, 3, "locals", 7, Arc::clone(&matches));

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

    #[test]
    fn clear_document_drops_everything_for_the_uri() {
        let cache = CapturesMatchCache::new();
        let uri = uri();
        let matches = Arc::new(vec![match_data(&[(0, 3)])]);
        cache.store_host(&uri, "highlights", 1, 7, Arc::clone(&matches));
        cache.store_layer(&uri, 2, "highlights", 7, matches);

        cache.clear_document(&uri);
        assert!(cache.get_host(&uri, "highlights", 1, 7).is_none());
        assert!(cache.get_layer(&uri, 2, 7).is_none());
    }
}
