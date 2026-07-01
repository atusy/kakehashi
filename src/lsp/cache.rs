//! `CacheCoordinator` unifies the semantic-token caches under one API:
//! `SemanticTokenCache`, `SemanticTokenRangeCache`, `InjectionMap`,
//! `InjectionTokenCache`, and `SemanticRequestTracker` (in-flight cancellation).
//!
//! The semantic-token cache is intentionally **not** invalidated on `didChange`
//! — `semanticTokens/full/delta` needs the previous version for delta
//! calculation, and dropping it on every edit would disable that. Two keys gate
//! lookups instead: `result_id` (for the delta baseline) and a `cache_key` (the
//! text content hash folded with a settings generation) that lets an unchanged
//! document skip re-tokenizing. A query/config reload bumps the generation (see
//! `bump_semantic_token_generation`) so post-reload requests recompute; `didChange`
//! never invalidates.

use std::collections::{HashMap, HashSet};

use tower_lsp_server::ls_types::SemanticTokens;
use tree_sitter::{InputEdit, Tree};
use url::Url;

use crate::analysis::{
    InjectionMap, InjectionTokenCache, SemanticTokenCache, SemanticTokenRangeCache,
};
use crate::language::LanguageCoordinator;
use crate::language::NodeTracker;
use crate::language::injection::{CacheableInjectionRegion, collect_all_injections};

use super::semantic_request_tracker::SemanticRequestTracker;

/// Request ID type for tracking semantic token requests.
pub(crate) type RequestId = u64;

/// Coordinates all cache structures for semantic token operations.
///
/// This struct wraps five underlying caches (full tokens, range tokens, the
/// injection map, injection-region tokens, and request tracking) and provides a
/// unified API for document lifecycle management, edit handling, and token operations.
pub(crate) struct CacheCoordinator {
    semantic_cache: SemanticTokenCache,
    /// Most-recent `semanticTokens/range` result per URI (#535), keyed by viewport
    /// range + the same `cache_key` as `semantic_cache`. Cleared on a generation
    /// bump and evicted on `didClose` alongside `semantic_cache`.
    semantic_range_cache: SemanticTokenRangeCache,
    injection_map: InjectionMap,
    injection_token_cache: std::sync::Arc<InjectionTokenCache>,
    request_tracker: SemanticRequestTracker,
    /// Settings generation folded into every semantic-token `cache_key`. Bumped
    /// on a query/config reload so cached tokens computed under the old queries
    /// stop matching — even one stored *after* the reload's `clear` by a request
    /// that was already computing (it captured the pre-bump generation), which a
    /// bare clear could not prevent.
    semantic_token_generation: std::sync::atomic::AtomicU64,
}

impl CacheCoordinator {
    /// Create a new cache coordinator with empty caches.
    pub(crate) fn new() -> Self {
        Self {
            semantic_cache: SemanticTokenCache::new(),
            semantic_range_cache: SemanticTokenRangeCache::new(),
            injection_map: InjectionMap::new(),
            injection_token_cache: std::sync::Arc::new(InjectionTokenCache::new()),
            request_tracker: SemanticRequestTracker::new(),
            semantic_token_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    // ========================================================================
    // Document lifecycle (did_close)
    // ========================================================================

    /// Remove all cached data for a document.
    ///
    /// Called when a document is closed to clean up:
    /// - Semantic token cache
    /// - Injection map
    /// - Injection token cache
    /// - Request tracking state
    pub(crate) fn remove_document(&self, uri: &Url) {
        self.semantic_cache.remove(uri);
        self.semantic_range_cache.remove(uri);
        self.injection_map.clear(uri);
        self.injection_token_cache.clear_document(uri);
        self.request_tracker.cancel_all_for_uri(uri);
    }

    // ========================================================================
    // Edit handling (did_change)
    // ========================================================================

    /// Invalidate injection caches for regions that overlap with edits.
    ///
    /// Called by `did_change` BEFORE the tree is cleared and the off-ingress reparse
    /// is scheduled, so it uses pre-edit byte offsets against pre-edit injection
    /// regions: edits outside injections preserve caches, edits inside invalidate
    /// only affected regions.
    ///
    /// Uses an O(log n) interval tree query.
    pub(crate) fn invalidate_for_edits(&self, uri: &Url, edits: &[InputEdit]) {
        if edits.is_empty() {
            return;
        }

        // Find all regions that overlap with any edit using O(log n) queries
        for edit in edits {
            let edit_start = edit.start_byte;
            let edit_end = edit.old_end_byte;

            // Query interval tree for overlapping regions (O(log n) instead of O(n))
            if let Some(overlapping_regions) = self
                .injection_map
                .find_overlapping(uri, edit_start, edit_end)
            {
                for region in overlapping_regions {
                    // This region is affected - invalidate its cache
                    self.injection_token_cache.remove(uri, &region.region_id);
                    log::debug!(
                        target: "kakehashi::injection_cache",
                        "Invalidated injection cache for {} region (edit bytes {}..{})",
                        region.language,
                        edit_start,
                        edit_end
                    );
                }
            }
        }
    }

    /// Invalidate semantic token cache for a document.
    ///
    /// Note: This should NOT be called during `didChange` - the cached tokens are
    /// needed for delta calculations. Instead, the cache is validated via `result_id`
    /// matching at lookup time. This function is used primarily in tests and for
    /// explicit cache reset scenarios.
    #[cfg(test)]
    pub(crate) fn invalidate_semantic(&self, uri: &Url) {
        // Log the result_id being invalidated (if any) for debugging cache behavior
        if let Some(cached) = self.semantic_cache.get(uri) {
            log::debug!(
                target: "kakehashi::semantic_cache",
                "Invalidating semantic cache for {} (result_id was '{}')",
                uri.path(),
                cached.tokens.result_id.as_deref().unwrap_or("<none>")
            );
        }
        self.semantic_cache.remove(uri);
    }

    /// Remove injection token cache entries for specific ULIDs.
    ///
    /// Called when region IDs are invalidated (e.g., due to edits touching their START).
    /// The corresponding virtual documents become orphaned in downstream LSs.
    pub(crate) fn remove_injection_tokens_for_ulids(&self, uri: &Url, ulids: &[ulid::Ulid]) {
        for ulid in ulids {
            self.injection_token_cache.remove(uri, &ulid.to_string());
        }
    }

    // ========================================================================
    // Injection map (post-parse)
    // ========================================================================

    /// Populate InjectionMap with injection regions from the parsed tree.
    ///
    /// Enables targeted cache invalidation: when an edit occurs, we can check
    /// which injection regions overlap and only invalidate those.
    ///
    /// Region IDs are generated using `NodeTracker` which provides position-based
    /// ULIDs. The same (uri, start_byte, end_byte, kind) always produces the same ULID,
    /// enabling stable IDs across document edits when position adjustments are applied.
    ///
    /// Also clears stale InjectionTokenCache entries for removed regions.
    pub(crate) fn populate_injections(
        &self,
        uri: &Url,
        text: &str,
        tree: &Tree,
        language_name: &str,
        language: &LanguageCoordinator,
        tracker: &NodeTracker,
    ) {
        // Get the injection query for this language
        let injection_query = match language.injection_query(language_name) {
            Some(q) => q,
            None => {
                // No injection query = no injections to track
                // Clear any stale injection caches
                self.injection_map.clear(uri);
                self.injection_token_cache.clear_document(uri);
                return;
            }
        };

        // Collect all injection regions from the parsed tree
        if let Some(regions) =
            collect_all_injections(&tree.root_node(), text, Some(injection_query.as_ref()))
        {
            if regions.is_empty() {
                // Clear any existing regions and caches for this document
                self.injection_map.clear(uri);
                self.injection_token_cache.clear_document(uri);
                return;
            }

            // Get existing regions for cache cleanup and content comparison
            let existing_regions = self.injection_map.get(uri);

            // Build lookup map for existing regions by region_id (skip if no existing regions)
            let existing_by_id: Option<HashMap<&str, &CacheableInjectionRegion>> = existing_regions
                .as_ref()
                .map(|regions| regions.iter().map(|r| (r.region_id.as_str(), r)).collect());

            // Convert to CacheableInjectionRegion using position-based ULIDs
            // NodeTracker provides stable IDs based on (uri, start_byte, end_byte, kind)
            let cacheable_regions: Vec<CacheableInjectionRegion> = regions
                .iter()
                .map(|info| {
                    // Get position-based ULID from tracker
                    let ulid = tracker.get_or_create(
                        uri,
                        info.content_node.start_byte(),
                        info.content_node.end_byte(),
                        info.content_node.kind(),
                    );
                    let region_id = ulid.to_string();
                    let new_region =
                        CacheableInjectionRegion::from_region_info(info, &region_id, text);

                    // Check if content_hash or language changed - invalidate semantic token cache
                    // Position-based ULIDs are stable, but cached tokens become invalid when:
                    // - content_hash changes: code content was modified
                    // - language changes: info string changed (e.g., ```lua → ```python)
                    //
                    // NOTE: This may double-invalidate regions already handled by invalidate_for_edits().
                    // This is intentional and correct because the two functions serve different purposes:
                    // - invalidate_for_edits(): Called BEFORE parse, handles spatial overlap (edit touched region)
                    // - This code: Called AFTER parse, handles semantic change (content/language changed)
                    // Double removal is idempotent (DashMap remove on missing key is a no-op), so this
                    // is safe. Combining these checks would require tracking invalidation state, adding
                    // complexity for negligible performance gain.
                    //
                    // Skip this check entirely if no existing regions (first document open)
                    if let Some(ref map) = existing_by_id
                        && let Some(old) = map.get(region_id.as_str())
                    {
                        let content_changed = old.content_hash != new_region.content_hash;
                        let language_changed = old.language != new_region.language;
                        if content_changed || language_changed {
                            self.injection_token_cache.remove(uri, &region_id);
                        }
                    }

                    new_region
                })
                .collect();

            // Find stale region IDs that are no longer present
            if let Some(old_regions) = existing_regions {
                let new_region_ids: HashSet<_> = cacheable_regions
                    .iter()
                    .map(|r| r.region_id.as_str())
                    .collect();
                for old in old_regions.iter() {
                    if !new_region_ids.contains(old.region_id.as_str()) {
                        // This region no longer exists - clear its cache
                        self.injection_token_cache.remove(uri, &old.region_id);
                    }
                }
            }

            // Store in injection map
            self.injection_map.insert(uri.clone(), cacheable_regions);
        }
    }

    /// Get all injection regions for a document (test helper).
    #[cfg(test)]
    pub(crate) fn get_injections(&self, uri: &Url) -> Option<Vec<CacheableInjectionRegion>> {
        self.injection_map.get(uri)
    }

    /// Share the per-region injection token cache for use on the blocking
    /// semantic-token pool (#529), where the hot path reuses/stores region tokens.
    pub(crate) fn injection_token_cache_arc(&self) -> std::sync::Arc<InjectionTokenCache> {
        std::sync::Arc::clone(&self.injection_token_cache)
    }

    // ========================================================================
    // Semantic tokens (semantic_tokens.rs)
    // ========================================================================

    /// Get cached semantic tokens if the result_id matches.
    ///
    /// Returns None if:
    /// - No tokens are cached for this URI
    /// - The cached result_id doesn't match the expected one
    pub(crate) fn get_tokens_if_valid(
        &self,
        uri: &Url,
        expected_result_id: &str,
    ) -> Option<SemanticTokens> {
        let result = self.semantic_cache.get_if_valid(uri, expected_result_id);

        if result.is_some() {
            log::debug!(
                target: "kakehashi::semantic_cache",
                "Cache HIT: found tokens for {} with result_id '{}'",
                uri.path(),
                expected_result_id
            );
        } else {
            self.log_cache_miss(uri, expected_result_id);
        }

        result.map(|cached| cached.tokens)
    }

    /// Log diagnostic information for cache misses.
    fn log_cache_miss(&self, uri: &Url, expected_result_id: &str) {
        if let Some(cached) = self.semantic_cache.get(uri) {
            log::debug!(
                target: "kakehashi::semantic_cache",
                "Cache MISS: result_id mismatch for {} - expected '{}', cached '{}'",
                uri.path(),
                expected_result_id,
                cached.tokens.result_id.as_deref().unwrap_or("<none>")
            );
        } else {
            log::debug!(
                target: "kakehashi::semantic_cache",
                "Cache MISS: no entry for {} (expected result_id '{}')",
                uri.path(),
                expected_result_id
            );
        }
    }

    /// Snapshot the current settings generation. Take this at the TOP of a token
    /// handler — before reading ANY settings-dependent tokenization input
    /// (language resolution, `ensure_language_loaded`, highlight query, capture
    /// mappings) — and pass it to [`cache_key_for`](Self::cache_key_for) once the
    /// text is available. A reload that bumps the generation after this snapshot
    /// leaves the request's stored key on the old generation, invisible to
    /// post-reload requests (which compute the new-generation key) — so a request
    /// racing a `didChangeConfiguration` can never poison the cache, regardless of
    /// whether it observed the old or new queries.
    pub(crate) fn semantic_token_generation(&self) -> u64 {
        self.semantic_token_generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Build the cache validity key for `text` under a previously snapshotted
    /// `generation` (see [`semantic_token_generation`](Self::semantic_token_generation)):
    /// the FNV-1a content hash folded with the generation, so distinct
    /// generations yield distinct keys for identical text.
    pub(crate) fn cache_key_for(&self, text: &str, generation: u64) -> u64 {
        crate::text::fnv1a_hash(text) ^ generation.wrapping_mul(0x100000001b3)
    }

    /// Store semantic tokens for a document, tagged with the `cache_key` they
    /// were computed under so an unchanged-document repeat request (same text and
    /// settings) can skip re-tokenizing via [`get_current_tokens`](Self::get_current_tokens).
    pub(crate) fn store_tokens(&self, uri: Url, tokens: SemanticTokens, cache_key: u64) {
        self.semantic_cache.store(uri, tokens, cache_key);
    }

    /// Store the most-recent `semanticTokens/range` result for a document (#535),
    /// tagged with the viewport `range` and the `cache_key` it was computed under.
    pub(crate) fn store_range_tokens(
        &self,
        uri: Url,
        range: tower_lsp_server::ls_types::Range,
        language: String,
        tokens: SemanticTokens,
        cache_key: u64,
    ) {
        self.semantic_range_cache
            .store(uri, range, language, tokens, cache_key);
    }

    /// Return the cached range tokens iff they were computed for this exact
    /// viewport `range` and `language` under this `cache_key` (same text + settings
    /// generation), letting the caller skip the range re-tokenization. None on a
    /// scroll, a language switch, a text edit, a settings reload, or no entry.
    pub(crate) fn get_current_range_tokens(
        &self,
        uri: &Url,
        range: &tower_lsp_server::ls_types::Range,
        language: &str,
        cache_key: u64,
    ) -> Option<SemanticTokens> {
        self.semantic_range_cache
            .get_if_current(uri, range, language, cache_key)
    }

    /// Return the cached tokens iff they were computed under this `cache_key`
    /// (same text and settings generation), letting the caller skip the full
    /// re-tokenization. None on a text edit, a settings reload, or no entry.
    pub(crate) fn get_current_tokens(&self, uri: &Url, cache_key: u64) -> Option<SemanticTokens> {
        self.semantic_cache
            .get_if_current(uri, cache_key)
            .map(|cached| cached.tokens)
    }

    /// Bump the settings generation on a settings/query reload so every cached
    /// token (computed under the old queries) stops matching, and clear the map
    /// to reclaim the now-dead entries. The bump — not the clear — is what makes
    /// this race-safe against a request that stores tokens after the clear.
    pub(crate) fn bump_semantic_token_generation(&self) {
        self.semantic_token_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.semantic_cache.clear();
        // The injection token cache folds the same generation into its key, so
        // old-generation entries are already unreachable after the bump; clear
        // them too so a reload doesn't leak per-region entries until each
        // document closes.
        self.injection_token_cache.clear();
        // The range cache folds the same generation into its key, so the bump
        // already invalidates it; clear to reclaim the dead entries (#535).
        self.semantic_range_cache.clear();
    }

    // ========================================================================
    // Request tracking (semantic_tokens.rs)
    // ========================================================================

    /// Start tracking a new request for the given URI.
    ///
    /// Returns a request ID that should be passed to subsequent operations.
    /// Automatically supersedes any previous request for the same URI.
    pub(crate) fn start_request(&self, uri: &Url) -> RequestId {
        self.request_tracker.start_request(uri)
    }

    /// Check if a request is still active (not superseded by a newer one).
    ///
    /// Returns true if the request should continue, false if it should abort.
    pub(crate) fn is_request_active(&self, uri: &Url, request_id: RequestId) -> bool {
        self.request_tracker.is_active(uri, request_id)
    }

    /// Finish a request, removing it from tracking if it's still the active one.
    pub(crate) fn finish_request(&self, uri: &Url, request_id: RequestId) {
        self.request_tracker.finish_request(uri, request_id);
    }

    /// Cancel all requests for a given URI (test helper).
    #[cfg(test)]
    pub(crate) fn cancel_requests(&self, uri: &Url) {
        self.request_tracker.cancel_all_for_uri(uri);
    }
}

impl Default for CacheCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::semantic::{RawToken, TokenKind};
    use tower_lsp_server::ls_types::SemanticToken;

    fn create_test_uri(path: &str) -> Url {
        Url::parse(&format!("file:///{}", path)).unwrap()
    }

    /// One region-local `RawToken`, for seeding the injection token cache.
    fn injection_raw_tokens() -> Vec<RawToken> {
        vec![RawToken {
            line: 0,
            column: 0,
            length: 5,
            kind: TokenKind::Mapped(1, 0),
            depth: 1,
            pattern_index: 0,
            priority: 100,
            node_byte_len: 5,
        }]
    }

    #[test]
    fn test_remove_document_clears_all_caches() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("test.rs");

        // Store some tokens
        let tokens = SemanticTokens {
            result_id: Some("test-id".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };
        cache.store_tokens(uri.clone(), tokens, 0);

        // Start a request
        let _request_id = cache.start_request(&uri);

        // Remove the document
        cache.remove_document(&uri);

        // Verify all caches are cleared
        assert!(cache.get_tokens_if_valid(&uri, "test-id").is_none());
        assert!(cache.get_injections(&uri).is_none());
    }

    #[test]
    fn bump_generation_invalidates_pre_reload_tokens() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("reload_race.rs");
        let text = "fn main() {}";
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![],
        };

        // A request snapshots the generation, then builds its key (generation 0).
        let gen_before = cache.semantic_token_generation();
        let key_before = cache.cache_key_for(text, gen_before);
        cache.store_tokens(uri.clone(), tokens.clone(), key_before);
        assert!(cache.get_current_tokens(&uri, key_before).is_some());

        // A settings/query reload bumps the generation (and clears the map).
        cache.bump_semantic_token_generation();

        // Race: a request that began tokenizing under the OLD queries (it captured
        // `key_before`) stores its now-stale tokens AFTER the reload's clear.
        cache.store_tokens(uri.clone(), tokens, key_before);

        // A fresh post-reload request snapshots the new generation and builds its
        // key for the same text. The stale, old-generation tokens must NOT be
        // served — this is what the generation (not a bare clear) buys us.
        let gen_after = cache.semantic_token_generation();
        let key_after = cache.cache_key_for(text, gen_after);
        assert_ne!(
            key_before, key_after,
            "the generation bump must change the key for identical text"
        );
        assert!(
            cache.get_current_tokens(&uri, key_after).is_none(),
            "tokens stored under the pre-reload generation must not survive a reload"
        );
    }

    #[test]
    fn test_invalidate_semantic() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("test.rs");

        // Store tokens
        let tokens = SemanticTokens {
            result_id: Some("test-id".to_string()),
            data: vec![],
        };
        cache.store_tokens(uri.clone(), tokens, 0);

        // Verify stored
        assert!(cache.get_tokens_if_valid(&uri, "test-id").is_some());

        // Invalidate
        cache.invalidate_semantic(&uri);

        // Verify removed
        assert!(cache.get_tokens_if_valid(&uri, "test-id").is_none());
    }

    #[test]
    fn test_request_tracking() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("test.rs");

        // Start a request
        let req1 = cache.start_request(&uri);
        assert!(cache.is_request_active(&uri, req1));

        // Start another request - should supersede the first
        let req2 = cache.start_request(&uri);
        assert!(!cache.is_request_active(&uri, req1));
        assert!(cache.is_request_active(&uri, req2));

        // Finish the second request
        cache.finish_request(&uri, req2);
        assert!(!cache.is_request_active(&uri, req2));
    }

    #[test]
    fn test_cancel_requests() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("test.rs");

        // Start a request
        let req = cache.start_request(&uri);
        assert!(cache.is_request_active(&uri, req));

        // Cancel all requests
        cache.cancel_requests(&uri);
        assert!(!cache.is_request_active(&uri, req));
    }

    /// Integration test: language change with stable region_id triggers cache invalidation.
    ///
    /// This test exercises the production invalidation path in `populate_injections`:
    /// - Uses `NodeTracker` with position-based ULIDs (not `next_result_id()`)
    /// - Simulates a language change at a stable position
    /// - Verifies that cached tokens are invalidated when language changes
    ///
    /// See: Finding 1 in review - tests must exercise production behavior.
    #[test]
    fn test_language_change_invalidates_cache_with_node_tracker() {
        use tree_sitter::{Parser, Query};

        let cache = CacheCoordinator::new();
        let tracker = NodeTracker::new();
        let coordinator = LanguageCoordinator::new();
        let uri = create_test_uri("test_lang_change.md");

        // Register injection query for markdown (required for populate_injections to work)
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let injection_query_str = r#"
            (fenced_code_block
              (info_string (language) @_lang)
              (code_fence_content) @injection.content
              (#set-lang-from-info-string! @_lang))
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("create injection query");
        coordinator
            .query_store()
            .insert_injection_query("markdown".to_string(), std::sync::Arc::new(injection_query));

        // Parse markdown with a Lua code block
        let initial_text = r#"# Test

```lua
print("hello")
```
"#;

        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set markdown");
        let tree = parser.parse(initial_text, None).expect("parse");

        // Populate injections - this should create a region with position-based ULID
        cache.populate_injections(
            &uri,
            initial_text,
            &tree,
            "markdown",
            &coordinator,
            &tracker,
        );

        // Verify we have one injection region
        let regions = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions.len(), 1);
        let initial_region_id = regions[0].region_id.clone();
        let initial_content_hash = regions[0].content_hash;
        assert_eq!(regions[0].language, "lua");

        // Store region-local tokens under the region's current content hash.
        cache.injection_token_cache.store(
            &uri,
            &initial_region_id,
            initial_content_hash,
            0,
            injection_raw_tokens(),
        );

        // Verify tokens are stored
        assert!(
            cache
                .injection_token_cache
                .get(&uri, &initial_region_id, initial_content_hash, 0)
                .is_some(),
            "tokens should be cached before language change"
        );

        // Now simulate editing the document: change ```lua to ```python
        // The code content stays the same, only the language changes
        let edited_text = r#"# Test

```python
print("hello")
```
"#;

        // CRITICAL: Apply the text diff to update tracker positions BEFORE re-parsing.
        // This is the production flow:
        //   1. apply_text_diff() adjusts positions in NodeTracker
        //   2. parse with new text
        //   3. populate_injections() finds existing region_id via adjusted positions
        // Without this step, positions shift and we get a new region_id (no cache hit).
        tracker.apply_text_diff(&uri, initial_text, edited_text);

        // Re-parse with the edited text
        let edited_tree = parser.parse(edited_text, None).expect("parse edited");

        // Re-populate injections
        // Key insight: After apply_text_diff, the tracker's positions are adjusted,
        // so get_or_create returns the SAME ULID. The invalidation check in
        // populate_injections should detect language_changed and remove cached tokens.
        cache.populate_injections(
            &uri,
            edited_text,
            &edited_tree,
            "markdown",
            &coordinator,
            &tracker,
        );

        // Verify the region still exists with the same region_id (position-based stability)
        let regions_after = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions_after.len(), 1);
        assert_eq!(
            regions_after[0].region_id, initial_region_id,
            "region_id should be stable (same position)"
        );
        assert_eq!(
            regions_after[0].language, "python",
            "language should be updated to python"
        );

        // CRITICAL ASSERTION: Cached tokens should be INVALIDATED
        // This is the key behavior that wasn't tested before.
        // The invalidation happens at lines 231-235 in populate_injections:
        //   if content_changed || language_changed {
        //       self.injection_token_cache.remove(uri, &region_id);
        //   }
        assert!(
            cache
                .injection_token_cache
                .get(&uri, &initial_region_id, initial_content_hash, 0)
                .is_none(),
            "cached tokens should be invalidated when language changes"
        );
    }

    /// Integration test: content change with stable region_id triggers cache invalidation.
    ///
    /// Similar to the language change test, but tests content_changed path.
    #[test]
    fn test_content_change_invalidates_cache_with_node_tracker() {
        use tree_sitter::{Parser, Query};

        let cache = CacheCoordinator::new();
        let tracker = NodeTracker::new();
        let coordinator = LanguageCoordinator::new();
        let uri = create_test_uri("test_content_change.md");

        // Register injection query for markdown (required for populate_injections to work)
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let injection_query_str = r#"
            (fenced_code_block
              (info_string (language) @_lang)
              (code_fence_content) @injection.content
              (#set-lang-from-info-string! @_lang))
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("create injection query");
        coordinator
            .query_store()
            .insert_injection_query("markdown".to_string(), std::sync::Arc::new(injection_query));

        // Parse markdown with a Lua code block
        let initial_text = r#"# Test

```lua
print("hello")
```
"#;

        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set markdown");
        let tree = parser.parse(initial_text, None).expect("parse");

        // Populate injections
        cache.populate_injections(
            &uri,
            initial_text,
            &tree,
            "markdown",
            &coordinator,
            &tracker,
        );

        let regions = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions.len(), 1);
        let initial_region_id = regions[0].region_id.clone();
        let initial_content_hash = regions[0].content_hash;

        // Store region-local tokens under the region's current content hash.
        cache.injection_token_cache.store(
            &uri,
            &initial_region_id,
            initial_content_hash,
            0,
            injection_raw_tokens(),
        );

        assert!(
            cache
                .injection_token_cache
                .get(&uri, &initial_region_id, initial_content_hash, 0)
                .is_some(),
            "tokens should be cached before content change"
        );

        // Edit the code content (not the language)
        let edited_text = r#"# Test

```lua
print("goodbye")
```
"#;

        // Apply the text diff to update tracker positions (same as production flow)
        tracker.apply_text_diff(&uri, initial_text, edited_text);

        let edited_tree = parser.parse(edited_text, None).expect("parse edited");

        // Re-populate injections
        cache.populate_injections(
            &uri,
            edited_text,
            &edited_tree,
            "markdown",
            &coordinator,
            &tracker,
        );

        let regions_after = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions_after.len(), 1);
        assert_eq!(
            regions_after[0].region_id, initial_region_id,
            "region_id should be stable"
        );
        assert_eq!(
            regions_after[0].language, "lua",
            "language should remain lua"
        );
        assert_ne!(
            regions_after[0].content_hash, initial_content_hash,
            "content_hash should change"
        );

        // Cached tokens should be invalidated due to content change
        assert!(
            cache
                .injection_token_cache
                .get(&uri, &initial_region_id, initial_content_hash, 0)
                .is_none(),
            "cached tokens should be invalidated when content changes"
        );
    }

    // ========================================================================
    // semanticTokens/range cache (#535)
    // ========================================================================

    fn range_tokens(len: u32) -> SemanticTokens {
        SemanticTokens {
            result_id: None,
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: len,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        }
    }

    fn test_range(start_line: u32, end_line: u32) -> tower_lsp_server::ls_types::Range {
        use tower_lsp_server::ls_types::{Position, Range};
        Range::new(Position::new(start_line, 0), Position::new(end_line, 0))
    }

    #[test]
    fn range_cache_hits_only_on_matching_range_language_and_key() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("range.rs");
        let range = test_range(0, 10);
        let key = cache.cache_key_for("hello", cache.semantic_token_generation());
        cache.store_range_tokens(uri.clone(), range, "rust".to_string(), range_tokens(5), key);

        // Same range + language + key → hit.
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "rust", key)
                .is_some(),
            "identical viewport + language + unchanged text/settings should hit"
        );
        // Different viewport (a scroll) → miss.
        assert!(
            cache
                .get_current_range_tokens(&uri, &test_range(5, 15), "rust", key)
                .is_none(),
            "a different range must miss even with the same language and key"
        );
        // Different language (same URI+text re-opened as another language) → miss.
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "python", key)
                .is_none(),
            "a language switch must miss even with the same range and key"
        );
        // Different key (text edit or settings reload) → miss.
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "rust", key ^ 1)
                .is_none(),
            "a different cache_key must miss even with the same range and language"
        );
    }

    #[test]
    fn range_cache_cleared_on_generation_bump() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("range.rs");
        let range = test_range(0, 10);
        let key = cache.cache_key_for("hello", cache.semantic_token_generation());
        cache.store_range_tokens(uri.clone(), range, "rust".to_string(), range_tokens(5), key);
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "rust", key)
                .is_some()
        );

        // A settings/query reload must drop range entries (same must-do wiring as
        // the full-token cache), or they would serve stale after the reload.
        cache.bump_semantic_token_generation();
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "rust", key)
                .is_none(),
            "generation bump must clear the range cache"
        );
    }

    #[test]
    fn range_cache_evicted_on_remove_document() {
        let cache = CacheCoordinator::new();
        let uri = create_test_uri("range.rs");
        let range = test_range(0, 10);
        let key = cache.cache_key_for("hello", cache.semantic_token_generation());
        cache.store_range_tokens(uri.clone(), range, "rust".to_string(), range_tokens(5), key);
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "rust", key)
                .is_some()
        );

        cache.remove_document(&uri);
        assert!(
            cache
                .get_current_range_tokens(&uri, &range, "rust", key)
                .is_none(),
            "didClose must evict the range cache entry"
        );
    }
}
