//! Semantic-token caches with `result_id` validation and injection-region tracking.
//!
//! Three layers (all `DashMap`-backed for concurrent access):
//! - `SemanticTokenCache`: per-URI tokens, served when LSP `result_id` matches.
//! - `InjectionMap`: per-URI set of injection regions from the last committed
//!   populate pass (spatial queries are test-only since token eviction went
//!   content-addressed).
//! - `InjectionTokenCache`: per-(URI, content-validity-hash) region tokens —
//!   content-addressed, so an edit outside a region can't touch its entry and
//!   byte-identical regions reuse each other's tokens.

use crate::analysis::semantic::RawToken;
use crate::language::injection::CacheableInjectionRegion;
use dashmap::DashMap;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tower_lsp_server::ls_types::{Range, SemanticTokens};
use url::Url;

const SEMANTIC_BASELINE_HISTORY: usize = 8;
/// Retain roughly three generations of the 40,480-u32 profiling payload
/// (~162 KiB each), while small documents may still use the count cap above.
/// The newest baseline is always kept even when one result alone exceeds this.
const SEMANTIC_BASELINE_BYTES_PER_DOCUMENT: usize = 512 * 1024;

/// Identity of one immutable parse snapshot under one settings generation.
/// Parsed versions restart when a URI closes and reopens, so incarnation is
/// part of the key rather than an optional lifecycle check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SemanticSnapshotIdentity {
    pub(crate) parsed_version: u64,
    pub(crate) incarnation: u64,
    pub(crate) generation: u64,
}

/// Cached semantic tokens for delta calculations.
#[derive(Clone)]
pub struct CachedSemanticTokens {
    pub tokens: Arc<SemanticTokens>,
    /// The resolved language these tokens were computed for. The `cache_key`
    /// folds only text ⊕ settings generation, so it can't tell the same URL
    /// re-opened under a DIFFERENT language apart. `didClose` evicts the entry
    /// (`CacheCoordinator::remove_document`), but a pre-close request already
    /// computing can `store` its tokens *after* that eviction under the unchanged
    /// `cache_key`; the reopen under the new language neither edits the text nor
    /// bumps the generation, so a bare `(uri, cache_key)` lookup would then serve
    /// those wrong-language tokens. Pinning the language makes such a request miss
    /// and recompute instead. (A *same*-language re-open yields identical tokens,
    /// so it correctly hits.) Mirrors the range (#535) and injection (#529)
    /// caches, which already fold their language in.
    pub language: String,
    /// Exact parse-snapshot and settings identity used by the hash-free fast path.
    pub snapshot: SemanticSnapshotIdentity,
    /// Opaque validity key these tokens were computed under (built by
    /// `CacheCoordinator::token_cache_key`: the FNV-1a hash of the document text
    /// folded with the settings generation). Lets a repeat request on an
    /// *unchanged* document under *unchanged* settings serve the cached tokens
    /// instead of re-tokenizing — the `result_id` can't (it is a fresh global
    /// counter per response). A text edit OR a config/query reload changes the
    /// key, so stale tokens stop matching.
    pub cache_key: u64,
}

/// Thread-safe semantic token cache.
pub struct SemanticTokenCache {
    documents: DashMap<Url, SemanticDocumentCache>,
}

#[derive(Default)]
struct SemanticDocumentCache {
    current: Option<CachedSemanticTokens>,
    baselines: HashMap<String, Arc<SemanticTokens>>,
    /// Baseline IDs from least to most recently stored or read.
    baseline_order: VecDeque<String>,
    baseline_bytes: usize,
}

impl SemanticTokenCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            documents: DashMap::new(),
        }
    }

    /// Store semantic tokens for a document, tagged with the resolved `language`
    /// and the `cache_key` they were computed under (see
    /// [`get_if_current`](Self::get_if_current)).
    pub fn store(
        &self,
        uri: Url,
        tokens: SemanticTokens,
        language: String,
        cache_key: u64,
        snapshot: SemanticSnapshotIdentity,
    ) {
        let mut document = self.documents.entry(uri).or_default();
        let result_id = tokens.result_id.clone();
        let tokens = Arc::new(tokens);
        document.current = Some(CachedSemanticTokens {
            tokens: Arc::clone(&tokens),
            language,
            snapshot,
            cache_key,
        });
        if let Some(result_id) = result_id {
            Self::store_baseline(&mut document, result_id, tokens);
        }
    }

    fn store_baseline(
        document: &mut SemanticDocumentCache,
        result_id: String,
        tokens: Arc<SemanticTokens>,
    ) {
        if let Some(previous) = document.baselines.insert(result_id.clone(), tokens.clone()) {
            document.baseline_bytes = document
                .baseline_bytes
                .saturating_sub(Self::token_bytes(&previous));
        }
        document.baseline_bytes += Self::token_bytes(&tokens);
        document.baseline_order.retain(|id| id != &result_id);
        document.baseline_order.push_back(result_id);
        while document.baseline_order.len() > 1
            && (document.baseline_order.len() > SEMANTIC_BASELINE_HISTORY
                || document.baseline_bytes > SEMANTIC_BASELINE_BYTES_PER_DOCUMENT)
        {
            let evicted = document
                .baseline_order
                .pop_front()
                .expect("length checked above");
            if let Some(tokens) = document.baselines.remove(&evicted) {
                document.baseline_bytes = document
                    .baseline_bytes
                    .saturating_sub(Self::token_bytes(&tokens));
            }
        }
    }

    fn token_bytes(tokens: &SemanticTokens) -> usize {
        tokens.data.len() * std::mem::size_of::<tower_lsp_server::ls_types::SemanticToken>()
    }

    fn touch_baseline(document: &mut SemanticDocumentCache, result_id: &str) {
        let Some(index) = document
            .baseline_order
            .iter()
            .position(|id| id == result_id)
        else {
            return;
        };
        if index + 1 == document.baseline_order.len() {
            return;
        }
        let result_id = document
            .baseline_order
            .remove(index)
            .expect("index came from position");
        document.baseline_order.push_back(result_id);
    }

    /// Retrieve semantic tokens for a document.
    pub fn get(&self, uri: &Url) -> Option<CachedSemanticTokens> {
        self.documents
            .get(uri)
            .and_then(|document| document.current.clone())
    }

    /// Get cached tokens if they were computed for this exact `language` under
    /// this `cache_key` — i.e. the resolved language, document text, AND settings
    /// generation are unchanged since they were cached, so re-tokenizing would
    /// reproduce them. Returns None on a mismatch (language switch, text edit, or
    /// config reload) or no entry.
    pub fn get_if_current(
        &self,
        uri: &Url,
        language: &str,
        cache_key: u64,
    ) -> Option<Arc<SemanticTokens>> {
        self.documents.get(uri).and_then(|document| {
            let entry = document.current.as_ref()?;
            if entry.cache_key == cache_key && entry.language == language {
                Some(Arc::clone(&entry.tokens))
            } else {
                None
            }
        })
    }

    /// Get cached tokens for the exact parse snapshot and settings generation.
    ///
    /// This is a narrower but cheaper hit path than [`get_if_current`]: it avoids
    /// rebuilding the full-document content hash when the caller already knows it
    /// is serving the same immutable parse snapshot.
    pub fn get_if_same_snapshot(
        &self,
        uri: &Url,
        language: &str,
        snapshot: SemanticSnapshotIdentity,
    ) -> Option<Arc<SemanticTokens>> {
        self.documents.get(uri).and_then(|document| {
            let entry = document.current.as_ref()?;
            if entry.snapshot == snapshot && entry.language == language {
                Some(Arc::clone(&entry.tokens))
            } else {
                None
            }
        })
    }

    /// Drop every cached entry. Used (alongside a generation bump) on a
    /// settings/query reload to reclaim memory; the generation bump is what makes
    /// the invalidation race-safe, this just stops the dead entries from leaking.
    pub fn clear(&self) {
        self.documents.clear();
    }

    /// Get cached tokens if the result_id matches.
    ///
    /// Returns None if:
    /// - No tokens are cached for this URI
    /// - The cached result_id doesn't match the expected one
    pub fn get_if_valid(&self, uri: &Url, expected_result_id: &str) -> Option<Arc<SemanticTokens>> {
        let mut document = self.documents.get_mut(uri)?;
        if let Some(tokens) = document.current.as_ref().and_then(|entry| {
            (entry.tokens.result_id.as_deref() == Some(expected_result_id))
                .then(|| Arc::clone(&entry.tokens))
        }) {
            return Some(tokens);
        }
        let tokens = document.baselines.get(expected_result_id).map(Arc::clone);
        if tokens.is_some() {
            Self::touch_baseline(&mut document, expected_result_id);
        }
        tokens
    }

    /// Remove cached tokens for a document (e.g., on document close).
    pub fn remove(&self, uri: &Url) {
        self.documents.remove(uri);
    }
}

impl Default for SemanticTokenCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Cached tokens for a `textDocument/semanticTokens/range` request (#535).
///
/// Exact-viewport companion to the full-token cache. Range is local kakehashi
/// tree-sitter compute (no downstream fan-out), so the result is a pure function
/// of the document text, the requested `range`, and the settings generation —
/// keying by all three is safe. The hit is narrow (only an *identical-viewport*
/// re-request of an unchanged document under unchanged settings), so a single
/// most-recent entry per URI is kept rather than a per-range map. Different
/// scrolled viewports miss here but can still filter the full-token cache when
/// available.
#[derive(Clone)]
pub struct CachedRangeTokens {
    pub tokens: Arc<SemanticTokens>,
    /// The viewport these tokens were computed for. A scroll changes the range, so
    /// a differing range is a miss even when text/settings are unchanged.
    pub range: Range,
    /// The resolved language these tokens were computed for. The `cache_key` folds
    /// only text + settings generation, so it cannot distinguish the same URI+text
    /// re-opened under a DIFFERENT language (a `didClose`/`didOpen` language switch,
    /// which neither edits the text nor bumps the generation). Pinning the language
    /// makes such a request miss and recompute instead of serving wrong-language
    /// tokens. (A *same*-language re-open yields identical tokens, so it correctly
    /// still hits.)
    pub language: String,
    /// The `cache_key` (FNV-1a of text folded with the settings generation, built
    /// by `CacheCoordinator::cache_key_for`) the tokens were computed under.
    pub cache_key: u64,
}

/// Thread-safe most-recent `semanticTokens/range` result cache, one entry per URI.
pub struct SemanticTokenRangeCache {
    cache: DashMap<Url, CachedRangeTokens>,
}

impl SemanticTokenRangeCache {
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Store the most-recent range tokens for a document, tagged with the viewport
    /// `range`, resolved `language`, and the `cache_key` they were computed under.
    pub fn store(
        &self,
        uri: Url,
        range: Range,
        language: String,
        tokens: SemanticTokens,
        cache_key: u64,
    ) {
        self.cache.insert(
            uri,
            CachedRangeTokens {
                tokens: Arc::new(tokens),
                range,
                language,
                cache_key,
            },
        );
    }

    /// Return the cached tokens iff the most-recent entry matches the requested
    /// viewport `range`, the resolved `language`, AND the `cache_key` (text +
    /// settings generation unchanged). None on a scroll, a language switch, a text
    /// edit, a settings reload, or no entry.
    pub fn get_if_current(
        &self,
        uri: &Url,
        range: &Range,
        language: &str,
        cache_key: u64,
    ) -> Option<Arc<SemanticTokens>> {
        self.cache.get(uri).and_then(|entry| {
            if entry.cache_key == cache_key && entry.range == *range && entry.language == language {
                Some(Arc::clone(&entry.tokens))
            } else {
                None
            }
        })
    }

    /// Drop every cached entry (on a settings/query reload, alongside the
    /// generation bump that makes the invalidation race-safe).
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Remove the cached entry for a document (on `didClose`).
    pub fn remove(&self, uri: &Url) {
        self.cache.remove(uri);
    }
}

impl Default for SemanticTokenRangeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe map of injection regions per document: the region set of the
/// last COMMITTED populate pass.
///
/// Content addressing removed the spatial-invalidation consumer, so no
/// production path reads this today — populate writes it at the commit point
/// (a plain move of the already-built region vec, no per-populate index
/// build) and tests observe it to pin what a committed pass recorded. The
/// spatial queries are test-only linear scans; if the §3 mint-reconciliation
/// split doesn't end up consuming the map, delete it and port the tests.
pub struct InjectionMap {
    regions: DashMap<Url, Vec<CacheableInjectionRegion>>,
}

impl InjectionMap {
    /// Create a new empty injection map.
    pub fn new() -> Self {
        Self {
            regions: DashMap::new(),
        }
    }

    /// Store injection regions for a document, replacing any existing regions.
    /// A plain move — nothing on the pre-publish critical path is built here.
    pub fn insert(&self, uri: Url, regions: Vec<CacheableInjectionRegion>) {
        self.regions.insert(uri, regions);
    }

    /// Retrieve all injection regions for a document.
    #[cfg(test)]
    pub fn get(&self, uri: &Url) -> Option<Vec<CacheableInjectionRegion>> {
        self.regions.get(uri).map(|entry| entry.clone())
    }

    /// Remove all injection regions for a document (e.g., on document close).
    pub fn clear(&self, uri: &Url) {
        self.regions.remove(uri);
    }

    /// Find the injection region containing the given byte position (test-only
    /// linear scan).
    #[cfg(test)]
    pub fn find_at_position(
        &self,
        uri: &Url,
        byte_position: usize,
    ) -> Option<CacheableInjectionRegion> {
        self.regions.get(uri).and_then(|regions| {
            regions
                .iter()
                .find(|r| r.byte_range.start <= byte_position && byte_position < r.byte_range.end)
                .cloned()
        })
    }

    /// Find all injection regions that overlap with the given byte range
    /// (test-only linear scan — content addressing removed the spatial
    /// invalidation path that needed an index).
    #[cfg(test)]
    pub fn find_overlapping(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
    ) -> Option<Vec<CacheableInjectionRegion>> {
        self.regions.get(uri).map(|regions| {
            regions
                .iter()
                .filter(|r| r.byte_range.start < end && start < r.byte_range.end)
                .cloned()
                .collect()
        })
    }
}

impl Default for InjectionMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe cache for per-injection-region semantic tokens (#529).
///
/// Unlike `SemanticTokenCache` (per-document, finalized tokens), this stores the
/// **region-local pre-finalize `RawToken`s** of a single injection region so the
/// hot path can reuse them across edits that don't touch the region, re-anchoring
/// them to the region's current host line at read time.
///
/// The key is `(uri, validity_hash)` — **content-addressed** (parse-snapshot
/// ADR §3's companion decision), where `validity_hash` folds the region's
/// content hash with its resolved injection language (see
/// [`region_validity_hash`]). Content addressing replaces the earlier
/// tracker-ULID key: a byte-identical region reuses tokens across edits with
/// no stable id at all (undo round-trips and duplicate fences hit for free),
/// and no read path needs the tracker — the identity decoupling the §3 mint
/// split relies on. An entry can never be *wrong* for its key (the key IS the
/// content), so spatial edit invalidation disappears; eviction is a
/// per-populate sweep ([`retain_document`](Self::retain_document)) that drops
/// hashes no longer present in the parse's live region set — without it,
/// typing inside a region would leak one entry per keystroke content.
///
/// `generation` (the settings/query epoch — bumped on config reload only,
/// never per edit, the #530 discipline) stays in the *value* and is checked
/// at read, so stale-generation entries read as misses even before the
/// reload's `clear()` reclaims them.
///
/// `supports_multiline` is deliberately *not* in the validity: it changes
/// multiline-token emission, but it comes from the client capabilities snapshot
/// (a `OnceLock` set once at `initialize()`), so it is session-constant and
/// cannot flip mid-session — no entry can outlive a change to it.
pub(crate) struct InjectionTokenCache {
    /// Outer key: document; inner key: `validity_hash`. Nested (rather than a
    /// flat `(Url, u64)` key) so the per-populate eviction sweep touches only
    /// the swept document's entries — a flat `DashMap::retain` scans every
    /// shard of every document under write locks on each parse commit. Reads
    /// and stores are single-threaded per request (pre-fan-out resolve loop /
    /// post-fan-in store loop), so the coarser per-document entry adds no
    /// contention, and the read path drops its per-lookup `Url` clone.
    cache: DashMap<Url, std::collections::HashMap<u64, CachedRegionTokens>>,
}

/// The per-region validity/cache key: the region's content hash folded with
/// its **resolved** injection language, so identical bytes injecting a
/// different language (or a language re-resolution after install) get a
/// different key barring a 64-bit collision. One definition shared by the
/// store/read path and populate's eviction sweep — the two must never drift.
///
/// Accepted bound: the 64-bit FNV-1a fold is not collision-resistant, and a
/// collision has no secondary gate — but an accidental one is ~2⁻⁶⁴ and a
/// *crafted* one only mis-highlights the crafting user's own buffer (tokens
/// never cross documents: the outer key is the URI), so a cryptographic hash
/// isn't worth its per-keystroke cost here.
pub(crate) fn region_validity_hash(content_hash: u64, resolved_lang: &str) -> u64 {
    content_hash ^ crate::text::fnv1a_hash(resolved_lang).wrapping_mul(0x100000001b3)
}

/// A region's cached tokens with the generation they were computed under.
#[derive(Clone)]
struct CachedRegionTokens {
    /// Settings/query generation in effect when these were computed.
    generation: u64,
    /// Region-local pre-finalize tokens.
    tokens: Arc<Vec<RawToken>>,
}

impl InjectionTokenCache {
    /// Create a new empty cache.
    pub(crate) fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Store region-local tokens under the region's content-addressed
    /// `validity_hash`, tagged with the settings `generation` they were
    /// computed under. Overwrites any prior entry for the same content —
    /// byte-identical regions (including duplicates within one document)
    /// share the entry.
    pub(crate) fn store(
        &self,
        uri: &Url,
        validity_hash: u64,
        generation: u64,
        tokens: Vec<RawToken>,
    ) {
        // `get_mut` first: the steady-state store (document entry exists)
        // avoids the `Url` clone the `entry` API needs for its owned key —
        // the same discipline as `record_served_semantic_version`.
        if let Some(mut doc) = self.cache.get_mut(uri) {
            doc.insert(
                validity_hash,
                CachedRegionTokens {
                    generation,
                    tokens: Arc::new(tokens),
                },
            );
            return;
        }
        self.cache.entry(uri.clone()).or_default().insert(
            validity_hash,
            CachedRegionTokens {
                generation,
                tokens: Arc::new(tokens),
            },
        );
    }

    /// Retrieve region-local tokens for this exact content (`validity_hash`
    /// keys content ⊕ resolved language) iff they were computed under the
    /// same settings generation. A config reload reads as a miss so the
    /// region is recomputed under the new queries.
    pub(crate) fn get(
        &self,
        uri: &Url,
        validity_hash: u64,
        generation: u64,
    ) -> Option<Arc<Vec<RawToken>>> {
        let result = self.cache.get(uri).and_then(|doc| {
            doc.get(&validity_hash)
                .filter(|entry| entry.generation == generation)
                .map(|entry| Arc::clone(&entry.tokens))
        });

        if result.is_some() {
            log::debug!(
                target: "kakehashi::injection_cache",
                "Cache HIT for injection content {:#018x} in {}",
                validity_hash,
                uri.path()
            );
        } else {
            log::trace!(
                target: "kakehashi::injection_cache",
                "Cache MISS for injection content {:#018x} in {}",
                validity_hash,
                uri.path()
            );
        }

        result
    }

    /// Eviction sweep (the content-addressed replacement for per-region point
    /// deletes): keep only entries whose hash is in the parse's `live` region
    /// set. Called by `populate_injections` after each parse — without it,
    /// typing inside a region would leak one entry per keystroke content.
    /// Entries are never *incorrect* (the key is the content), so a sweep
    /// racing a store of an already-dead hash merely leaks that entry until
    /// the next sweep.
    pub(crate) fn retain_document(&self, uri: &Url, live: &std::collections::HashSet<u64>) {
        // One shard write lock for both the sweep and the empty-entry
        // reclaim: `remove_if_mut` mutates in place and removes only when
        // the closure returns true, so a concurrent store can never land
        // between the sweep and the reclaim.
        self.cache.remove_if_mut(uri, |_, doc| {
            doc.retain(|hash, _| live.contains(hash));
            doc.is_empty()
        });
    }

    /// Remove all cached tokens for a document (all its injection regions).
    pub(crate) fn clear_document(&self, uri: &Url) {
        self.cache.remove(uri);
    }

    /// Drop every cached entry. Used on a settings/query reload (alongside the
    /// generation bump) to reclaim memory: the generation fold already makes
    /// old-generation entries unreachable — this just stops them leaking until
    /// each document closes. Mirrors `SemanticTokenCache::clear`; the fold, not
    /// this clear, is what makes a concurrent store race-safe.
    pub(crate) fn clear(&self) {
        self.cache.clear();
    }

    /// `(validity_hash, generation)` currently stored for a document — lets a
    /// test confirm what the hot path persisted and overwrite a specific entry
    /// to prove the reuse path reads it.
    #[cfg(test)]
    pub(crate) fn test_keys(&self, uri: &Url) -> Vec<(u64, u64)> {
        self.cache
            .get(uri)
            .map(|doc| {
                doc.iter()
                    .map(|(hash, entry)| (*hash, entry.generation))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Default for InjectionTokenCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::SemanticToken;

    fn snapshot(
        parsed_version: u64,
        incarnation: u64,
        generation: u64,
    ) -> SemanticSnapshotIdentity {
        SemanticSnapshotIdentity {
            parsed_version,
            incarnation,
            generation,
        }
    }

    #[test]
    fn test_semantic_cache_store_retrieve() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };

        // Store tokens
        cache.store(
            uri.clone(),
            tokens.clone(),
            "rust".to_string(),
            0,
            snapshot(0, 0, 0),
        );

        // Retrieve tokens
        let retrieved = cache.get(&uri);
        assert!(retrieved.is_some(), "Should retrieve stored tokens");
        let retrieved = retrieved.unwrap();
        assert_eq!(
            retrieved.tokens.result_id,
            Some("1".to_string()),
            "result_id should match"
        );
        assert_eq!(retrieved.tokens.data.len(), 1, "Should have 1 token");
        assert_eq!(
            retrieved.tokens.data[0].length, 5,
            "Token length should match"
        );

        // Non-existent URI returns None
        let other_uri = Url::parse("file:///other.rs").unwrap();
        assert!(
            cache.get(&other_uri).is_none(),
            "Non-existent URI should return None"
        );
    }

    /// Two reads of the same entry must share the SAME allocation (an `Arc`
    /// refcount bump), not deep-copy the token vector per read — the whole
    /// point of wrapping the cache in `Arc` (#576).
    #[test]
    fn repeated_reads_share_the_same_arc_allocation_not_a_deep_copy() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///arc_share.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };
        cache.store(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0,
            snapshot(0, 0, 0),
        );

        let first = cache.get(&uri).unwrap();
        let second = cache.get(&uri).unwrap();
        assert!(
            Arc::ptr_eq(&first.tokens, &second.tokens),
            "repeated reads must share one Arc allocation, not deep-copy"
        );
    }

    /// Pins the named worst-case from #576: a delta-baseline read must not
    /// perform an extra `Arc::clone` (refcount bump) to feed the
    /// comparison — `&Arc<T>` deref-coerces to `&T` at the call, so
    /// `calculate_delta_or_full` borrowing it should leave the strong count
    /// unchanged. This only detects a stray `Arc::clone`; it cannot detect
    /// a deep clone of the underlying `SemanticTokens` (which wouldn't
    /// touch this Arc's refcount at all) — that guarantee instead comes
    /// from `calculate_delta_or_full`'s signature taking `&SemanticTokens`,
    /// not an owned value.
    #[test]
    fn delta_baseline_comparison_does_not_bump_the_cached_arcs_refcount() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///arc_no_clone_on_delta.rs").unwrap();
        let previous = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };
        cache.store(
            uri.clone(),
            previous,
            "rust".to_string(),
            0,
            snapshot(0, 0, 0),
        );

        let cached = cache.get_if_valid(&uri, "1").unwrap();
        let before = Arc::strong_count(&cached);

        let current = SemanticTokens {
            result_id: Some("2".to_string()),
            data: vec![],
        };
        let _ = crate::analysis::semantic::calculate_delta_or_full(&cached, &current, "1");

        assert_eq!(
            Arc::strong_count(&cached),
            before,
            "borrowing the cached Arc for the delta comparison must not clone it"
        );
    }

    #[test]
    fn range_cache_repeated_reads_share_the_same_arc_allocation() {
        let cache = SemanticTokenRangeCache::new();
        let uri = Url::parse("file:///arc_share_range.rs").unwrap();
        let range = Range::default();
        let tokens = SemanticTokens {
            result_id: None,
            data: vec![],
        };
        cache.store(uri.clone(), range, "rust".to_string(), tokens, 0);

        let first = cache.get_if_current(&uri, &range, "rust", 0).unwrap();
        let second = cache.get_if_current(&uri, &range, "rust", 0).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated reads must share one Arc allocation, not deep-copy"
        );
    }

    #[test]
    fn injection_cache_repeated_reads_share_the_same_arc_allocation() {
        let cache = InjectionTokenCache::new();
        let uri = Url::parse("file:///arc_share_injection.rs").unwrap();
        cache.store(&uri, 0xABCD, 0, vec![]);

        let first = cache.get(&uri, 0xABCD, 0).unwrap();
        let second = cache.get(&uri, 0xABCD, 0).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated reads must share one Arc allocation, not deep-copy"
        );
    }

    #[test]
    fn test_semantic_cache_invalid_result_id() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("42".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 10,
                token_type: 1,
                token_modifiers_bitset: 0,
            }],
        };

        cache.store(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0,
            snapshot(0, 0, 0),
        );

        // Matching result_id returns tokens
        let valid = cache.get_if_valid(&uri, "42");
        assert!(
            valid.is_some(),
            "Should return tokens when result_id matches"
        );
        assert_eq!(valid.unwrap().data[0].length, 10);

        // Mismatched result_id returns None
        let invalid = cache.get_if_valid(&uri, "99");
        assert!(
            invalid.is_none(),
            "Should return None when result_id doesn't match"
        );

        // Non-existent URI returns None
        let other_uri = Url::parse("file:///other.rs").unwrap();
        assert!(
            cache.get_if_valid(&other_uri, "42").is_none(),
            "Non-existent URI should return None"
        );
    }

    #[test]
    fn semantic_cache_keeps_previous_delta_baseline() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///test.rs").unwrap();

        cache.store(
            uri.clone(),
            SemanticTokens {
                result_id: Some("old".to_string()),
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 1,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }],
            },
            "rust".to_string(),
            1,
            snapshot(1, 0, 0),
        );
        cache.store(
            uri.clone(),
            SemanticTokens {
                result_id: Some("new".to_string()),
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 2,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }],
            },
            "rust".to_string(),
            2,
            snapshot(2, 0, 0),
        );

        assert!(
            cache.get_if_valid(&uri, "old").is_some(),
            "a slightly stale client baseline should still be available for delta"
        );
        assert!(
            cache.get_if_valid(&uri, "new").is_some(),
            "the latest baseline must remain available"
        );
    }

    #[test]
    fn semantic_baselines_obey_per_document_byte_budget() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///large.rs").unwrap();
        let token = SemanticToken {
            delta_line: 0,
            delta_start: 0,
            length: 1,
            token_type: 0,
            token_modifiers_bitset: 0,
        };
        let tokens_per_result =
            SEMANTIC_BASELINE_BYTES_PER_DOCUMENT / std::mem::size_of::<SemanticToken>() * 3 / 4;
        for (version, id) in [(0, "old"), (1, "new")] {
            cache.store(
                uri.clone(),
                SemanticTokens {
                    result_id: Some(id.to_string()),
                    data: vec![token; tokens_per_result],
                },
                "rust".to_string(),
                version,
                snapshot(version, 0, 0),
            );
        }

        assert!(cache.get_if_valid(&uri, "old").is_none());
        assert!(cache.get_if_valid(&uri, "new").is_some());
    }

    #[test]
    fn reading_stale_delta_baseline_refreshes_its_eviction_recency() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///baseline-recency.rs").unwrap();
        let store = |result_id: usize| {
            cache.store(
                uri.clone(),
                SemanticTokens {
                    result_id: Some(result_id.to_string()),
                    data: vec![],
                },
                "rust".to_string(),
                result_id as u64,
                snapshot(result_id as u64, 0, 0),
            );
        };

        for result_id in 0..SEMANTIC_BASELINE_HISTORY {
            store(result_id);
        }
        assert!(cache.get_if_valid(&uri, "0").is_some());

        store(SEMANTIC_BASELINE_HISTORY);

        assert!(
            cache.get_if_valid(&uri, "0").is_some(),
            "a baseline just reissued to the client must survive the next store"
        );
        assert!(
            cache.get_if_valid(&uri, "1").is_none(),
            "the least recently used baseline should be evicted instead"
        );
    }

    #[test]
    fn get_if_current_matches_on_content_hash() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///t.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("7".to_string()),
            data: vec![],
        };
        cache.store(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0xABCD,
            snapshot(0, 0, 0),
        );

        // Same content hash AND language => the document is unchanged => serve
        // cached tokens.
        let hit = cache.get_if_current(&uri, "rust", 0xABCD);
        assert!(hit.is_some(), "should hit when the content hash matches");
        assert_eq!(hit.unwrap().result_id, Some("7".to_string()));

        // Different content hash => the text changed => recompute (miss).
        assert!(
            cache.get_if_current(&uri, "rust", 0x1234).is_none(),
            "should miss when the content hash differs"
        );

        // Unknown URI => miss.
        let other = Url::parse("file:///o.rs").unwrap();
        assert!(cache.get_if_current(&other, "rust", 0xABCD).is_none());
    }

    #[test]
    fn get_if_same_snapshot_matches_version_generation_and_language() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///snapshot.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("snap".to_string()),
            data: vec![],
        };
        cache.store(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0xABCD,
            snapshot(42, 3, 7),
        );

        assert!(
            cache
                .get_if_same_snapshot(&uri, "rust", snapshot(42, 3, 7))
                .is_some(),
            "same immutable parse snapshot and generation should hit"
        );
        assert!(
            cache
                .get_if_same_snapshot(&uri, "rust", snapshot(43, 3, 7))
                .is_none(),
            "different parsed version must miss"
        );
        assert!(
            cache
                .get_if_same_snapshot(&uri, "rust", snapshot(42, 3, 8))
                .is_none(),
            "different settings generation must miss"
        );
        assert!(
            cache
                .get_if_same_snapshot(&uri, "python", snapshot(42, 3, 7))
                .is_none(),
            "different language must miss"
        );
        assert!(
            cache
                .get_if_same_snapshot(&uri, "rust", snapshot(42, 4, 7))
                .is_none(),
            "same version in a reopened document lifetime must miss"
        );
    }

    #[test]
    fn get_if_current_misses_on_language_switch() {
        // A `didClose`/`didOpen` that re-assigns the same URI to a different
        // language keeps the text (and thus `cache_key`) identical, since the key
        // folds only text ⊕ settings generation. `didClose` evicts, but a
        // pre-close request still computing can store under the unchanged key
        // after that eviction, and the reopen bumps no generation — so without the
        // language guard the cache would serve the previous language's tokens for
        // the new document (#549); with it, the request misses and recomputes.
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///t.txt").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![],
        };
        cache.store(
            uri.clone(),
            tokens,
            "lua".to_string(),
            0xABCD,
            snapshot(0, 0, 0),
        );

        // Same key, same language => hit.
        assert!(
            cache.get_if_current(&uri, "lua", 0xABCD).is_some(),
            "should hit for the same language under the same key"
        );

        // Same key (unchanged text), different language => miss (no wrong-language
        // tokens served).
        assert!(
            cache.get_if_current(&uri, "python", 0xABCD).is_none(),
            "must miss when the resolved language differs even if the text is unchanged"
        );
    }

    #[test]
    fn clear_drops_all_entries() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///t.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![],
        };
        cache.store(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0xAA,
            snapshot(0, 0, 0),
        );
        assert!(cache.get_if_current(&uri, "rust", 0xAA).is_some());

        // A config reload changes tokenization for the same text, so the whole
        // cache must drop — a later request recomputes against the new queries.
        cache.clear();
        assert!(
            cache.get_if_current(&uri, "rust", 0xAA).is_none(),
            "clear() must drop entries so stale-config tokens are not served"
        );
    }

    #[test]
    fn test_semantic_cache_remove_on_close() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };

        // Store tokens
        cache.store(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0,
            snapshot(0, 0, 0),
        );
        assert!(cache.get(&uri).is_some(), "Should have cached tokens");

        // Remove on close
        cache.remove(&uri);
        assert!(cache.get(&uri).is_none(), "Should return None after remove");

        // Removing non-existent URI is safe
        let other_uri = Url::parse("file:///other.rs").unwrap();
        cache.remove(&other_uri); // Should not panic
    }

    #[test]
    fn closed_uri_keys_are_reclaimed() {
        let cache = SemanticTokenCache::new();
        for index in 0..128 {
            cache.remove(&Url::parse(&format!("file:///closed-{index}.rs")).unwrap());
        }
        assert!(cache.documents.is_empty());
    }

    #[test]
    fn test_injection_map_store_retrieve() {
        use crate::language::injection::CacheableInjectionRegion;

        let map = InjectionMap::new();
        let uri = Url::parse("file:///test.md").unwrap();

        let regions = vec![
            CacheableInjectionRegion {
                language: "lua".to_string(),
                byte_range: 10..50,
                line_range: 2..5,
                start_column: 0,
                region_id: "region-1".to_string(),
                content_hash: 12345,
            },
            CacheableInjectionRegion {
                language: "python".to_string(),
                byte_range: 100..200,
                line_range: 10..20,
                start_column: 0,
                region_id: "region-2".to_string(),
                content_hash: 67890,
            },
        ];

        // Insert regions
        map.insert(uri.clone(), regions.clone());

        // Retrieve regions
        let retrieved = map.get(&uri);
        assert!(retrieved.is_some(), "Should retrieve stored regions");
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.len(), 2, "Should have 2 regions");
        assert_eq!(retrieved[0].language, "lua");
        assert_eq!(retrieved[1].language, "python");

        // Non-existent URI returns None
        let other_uri = Url::parse("file:///other.md").unwrap();
        assert!(
            map.get(&other_uri).is_none(),
            "Non-existent URI should return None"
        );
    }

    #[test]
    fn test_injection_map_clear() {
        use crate::language::injection::CacheableInjectionRegion;

        let map = InjectionMap::new();
        let uri = Url::parse("file:///test.md").unwrap();

        let regions = vec![CacheableInjectionRegion {
            language: "lua".to_string(),
            byte_range: 10..50,
            line_range: 2..5,
            start_column: 0,
            region_id: "region-1".to_string(),
            content_hash: 12345,
        }];

        // Insert and verify
        map.insert(uri.clone(), regions);
        assert!(map.get(&uri).is_some(), "Should have stored regions");

        // Clear removes regions
        map.clear(&uri);
        assert!(map.get(&uri).is_none(), "Should return None after clear");

        // Clearing non-existent URI is safe
        let other_uri = Url::parse("file:///other.md").unwrap();
        map.clear(&other_uri); // Should not panic
    }

    /// Minimal region-local `RawToken` for cache tests: an emitted token at the
    /// given region-local line/column with the given UTF-16 length.
    fn raw_token(line: usize, column: usize, length: usize) -> RawToken {
        use crate::analysis::semantic::TokenKind;
        RawToken {
            line,
            column,
            length,
            kind: TokenKind::Mapped(1, 0),
            depth: 1,
            pattern_index: 0,
            priority: 100,
            node_byte_len: length,
        }
    }

    #[test]
    fn test_injection_token_cache_store_retrieve() {
        let cache = InjectionTokenCache::new();
        let uri = Url::parse("file:///test.md").unwrap();

        let tokens1 = vec![raw_token(0, 0, 5)];
        let tokens2 = vec![raw_token(1, 2, 10)];

        // Store region-local tokens under a content hash + generation.
        cache.store(&uri, 0xAA, 0, tokens1.clone());
        cache.store(&uri, 0xBB, 0, tokens2.clone());

        // Retrieve by the full validity key.
        let retrieved1 = cache.get(&uri, 0xAA, 0);
        assert!(retrieved1.is_some(), "Should retrieve tokens for region-1");
        assert_eq!(retrieved1.unwrap()[0].length, 5);

        let retrieved2 = cache.get(&uri, 0xBB, 0);
        assert!(retrieved2.is_some(), "Should retrieve tokens for region-2");
        assert_eq!(retrieved2.unwrap()[0].length, 10);

        // Unknown content hash returns None
        assert!(cache.get(&uri, 0xCC, 0).is_none());

        // Non-existent URI returns None
        let other_uri = Url::parse("file:///other.md").unwrap();
        assert!(cache.get(&other_uri, 0xAA, 0).is_none());
    }

    #[test]
    fn injection_token_cache_validity_key_gates_reads() {
        let cache = InjectionTokenCache::new();
        let uri = Url::parse("file:///t.md").unwrap();
        cache.store(&uri, 0x1111, 7, vec![raw_token(0, 0, 4)]);

        // Exact key hits.
        assert!(cache.get(&uri, 0x1111, 7).is_some());

        // A different content hash (region content changed) misses.
        assert!(
            cache.get(&uri, 0x2222, 7).is_none(),
            "content-hash mismatch must miss so edited content is recomputed"
        );

        // A different generation (settings/query reload) misses, even for the
        // same content — the race-safe fold, not a bare clear.
        assert!(
            cache.get(&uri, 0x1111, 8).is_none(),
            "generation mismatch must miss so post-reload requests recompute"
        );
    }

    #[test]
    fn injection_token_cache_sweep_bounds_distinct_contents() {
        let cache = InjectionTokenCache::new();
        let uri = Url::parse("file:///t.md").unwrap();

        // Content-addressed: distinct contents COEXIST (an edited region's old
        // entry stays readable-by-nobody until swept; an undo back to 0x1111
        // would hit again).
        cache.store(&uri, 0x1111, 0, vec![raw_token(0, 0, 4)]);
        cache.store(&uri, 0x2222, 0, vec![raw_token(0, 0, 5)]);
        assert!(cache.get(&uri, 0x1111, 0).is_some());
        assert!(cache.get(&uri, 0x2222, 0).is_some());

        // Identical content overwrites in place (one entry per content).
        cache.store(&uri, 0x2222, 0, vec![raw_token(0, 0, 6)]);
        assert_eq!(cache.get(&uri, 0x2222, 0).expect("hits")[0].length, 6);

        // The populate sweep is the growth bound: only live hashes survive,
        // and other documents' entries are untouched.
        let other_uri = Url::parse("file:///other.md").unwrap();
        cache.store(&other_uri, 0x1111, 0, vec![raw_token(0, 0, 7)]);
        let live = std::collections::HashSet::from([0x2222]);
        cache.retain_document(&uri, &live);
        assert!(cache.get(&uri, 0x1111, 0).is_none(), "dead hash swept");
        assert!(cache.get(&uri, 0x2222, 0).is_some(), "live hash retained");
        assert!(
            cache.get(&other_uri, 0x1111, 0).is_some(),
            "other documents' entries survive a sweep"
        );
    }

    /// The validity fold must separate resolved languages for identical bytes
    /// (the #530 wrong-language discipline) and be deterministic — the store/
    /// read path and populate's eviction sweep both compute it and must agree.
    #[test]
    fn region_validity_hash_separates_languages_and_is_deterministic() {
        let content_hash = 0xDEAD_BEEF_u64;
        assert_eq!(
            region_validity_hash(content_hash, "lua"),
            region_validity_hash(content_hash, "lua"),
        );
        assert_ne!(
            region_validity_hash(content_hash, "lua"),
            region_validity_hash(content_hash, "python"),
            "identical bytes injecting a different language must key differently"
        );
        assert_ne!(
            region_validity_hash(0x1111, "lua"),
            region_validity_hash(0x2222, "lua"),
            "different content must key differently for the same language"
        );
    }

    #[test]
    fn test_injection_map_get_tokens_via_region_id() {
        use crate::language::injection::CacheableInjectionRegion;

        let injection_map = InjectionMap::new();
        let token_cache = InjectionTokenCache::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Set up injection regions
        let regions = vec![
            CacheableInjectionRegion {
                language: "lua".to_string(),
                byte_range: 10..50,
                line_range: 2..5,
                start_column: 0,
                region_id: "lua-region-1".to_string(),
                content_hash: 11111,
            },
            CacheableInjectionRegion {
                language: "python".to_string(),
                byte_range: 100..200,
                line_range: 10..20,
                start_column: 0,
                region_id: "python-region-2".to_string(),
                content_hash: 22222,
            },
        ];
        injection_map.insert(uri.clone(), regions);

        // Set up cached tokens for the lua region, keyed by the cache's real
        // contract: the content ⊕ resolved-language fold, never the raw
        // content hash (identical bytes injecting a different language must
        // not share an entry).
        token_cache.store(
            &uri,
            region_validity_hash(11111, "lua"),
            0,
            vec![raw_token(0, 0, 3)],
        );

        // Find region containing byte offset and get its cached tokens
        let regions = injection_map.get(&uri).unwrap();
        let region_at_byte_30 = regions.iter().find(|r| r.byte_range.contains(&30));
        assert!(region_at_byte_30.is_some(), "Should find region at byte 30");

        let region = region_at_byte_30.unwrap();
        assert_eq!(region.language, "lua");

        // Content-addressed lookup through the same fold the read path uses.
        let cached = token_cache.get(&uri, region_validity_hash(region.content_hash, "lua"), 0);
        assert!(cached.is_some(), "Should have cached tokens for lua region");
        assert_eq!(cached.unwrap()[0].length, 3);
        // The raw content hash alone must MISS — the fold is load-bearing.
        assert!(
            token_cache.get(&uri, region.content_hash, 0).is_none(),
            "raw content hash without the language fold must not hit"
        );
    }

    #[test]
    fn test_injection_map_find_at_position() {
        use crate::language::injection::CacheableInjectionRegion;

        let map = InjectionMap::new();
        let uri = Url::parse("file:///test.md").unwrap();

        // Two injection regions: lua at bytes 10-50, python at bytes 100-200
        let regions = vec![
            CacheableInjectionRegion {
                language: "lua".to_string(),
                byte_range: 10..50,
                line_range: 2..5,
                start_column: 0,
                region_id: "region-1".to_string(),
                content_hash: 12345,
            },
            CacheableInjectionRegion {
                language: "python".to_string(),
                byte_range: 100..200,
                line_range: 10..20,
                start_column: 0,
                region_id: "region-2".to_string(),
                content_hash: 67890,
            },
        ];
        map.insert(uri.clone(), regions);

        // Find region containing byte 30 (should be lua)
        let found = map.find_at_position(&uri, 30);
        assert!(found.is_some(), "Should find region at byte 30");
        assert_eq!(found.unwrap().language, "lua");

        // Find region containing byte 150 (should be python)
        let found = map.find_at_position(&uri, 150);
        assert!(found.is_some(), "Should find region at byte 150");
        assert_eq!(found.unwrap().language, "python");

        // Byte 5 is before any region
        let found = map.find_at_position(&uri, 5);
        assert!(found.is_none(), "Byte 5 is not in any region");

        // Byte 75 is between regions (gap)
        let found = map.find_at_position(&uri, 75);
        assert!(found.is_none(), "Byte 75 is in the gap between regions");

        // Non-existent URI
        let other_uri = Url::parse("file:///other.md").unwrap();
        let found = map.find_at_position(&other_uri, 30);
        assert!(found.is_none(), "Non-existent URI should return None");
    }

    #[test]
    fn test_injection_map_find_overlapping_efficiently() {
        use crate::language::injection::CacheableInjectionRegion;

        // Verifies the overlap query API. Performance (O(log n)) is guaranteed
        // by the interval tree implementation.

        let map = InjectionMap::new();
        let uri = Url::parse("file:///test_large.md").unwrap();

        // Create many non-overlapping regions to simulate large document
        let regions: Vec<CacheableInjectionRegion> = (0..100)
            .map(|i| {
                let start = i * 100;
                let end = start + 50;
                CacheableInjectionRegion {
                    language: "lua".to_string(),
                    byte_range: start..end,
                    line_range: (i as u32)..(i as u32 + 1),
                    start_column: 0,
                    region_id: format!("region-{}", i),
                    content_hash: i as u64,
                }
            })
            .collect();

        map.insert(uri.clone(), regions);

        // Query for overlapping regions in byte range 225..350
        // Regions are at: [0..50, 100..150, 200..250, 300..350, 400..450, ...]
        // Query [225..350] should overlap:
        //   - region-2 at [200..250] (overlaps [225..250])
        //   - region-3 at [300..350] (overlaps [300..350])
        let overlapping = map.find_overlapping(&uri, 225, 350);

        assert!(overlapping.is_some(), "Should find overlapping regions");
        let overlapping = overlapping.unwrap();

        // Should find regions 2 (200..250) and 3 (300..350)
        assert_eq!(
            overlapping.len(),
            2,
            "Should find exactly 2 overlapping regions"
        );

        let region_ids: Vec<&str> = overlapping.iter().map(|r| r.region_id.as_str()).collect();
        assert!(region_ids.contains(&"region-2"), "Should include region-2");
        assert!(region_ids.contains(&"region-3"), "Should include region-3");

        // Query with no overlaps
        let no_overlap = map.find_overlapping(&uri, 60, 80);
        assert!(
            no_overlap.is_some(),
            "Should return empty vec for no overlaps"
        );
        assert_eq!(
            no_overlap.unwrap().len(),
            0,
            "Should have no overlapping regions"
        );

        // Query on non-existent URI
        let other_uri = Url::parse("file:///other.md").unwrap();
        let not_found = map.find_overlapping(&other_uri, 0, 100);
        assert!(not_found.is_none(), "Non-existent URI should return None");
    }
}
