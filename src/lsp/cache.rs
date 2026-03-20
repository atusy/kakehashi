//! Cache coordination for semantic token operations.
//!
//! This module provides `CacheCoordinator` which unifies four cache structures
//! under a single coherent API with clear document lifecycle management:
//!
//! - `SemanticTokenCache` - Document-level token caching by URI with result_id validation
//! - `InjectionMap` - Tracks injection regions per document using interval trees
//! - `InjectionTokenCache` - Per-injection semantic tokens by (URI, region_id)
//! - `SemanticRequestTracker` - Cancellation support for in-flight requests
//!
//! ## Architecture
//!
//! ```text
//!                     CacheCoordinator
//!                           │
//!     ┌─────────────────────┼─────────────────────┐
//!     │         │           │          │          │
//!     ▼         ▼           ▼          ▼          ▼
//! Semantic   Injection   Injection   Request    Document
//! Cache      Map         TokenCache  Tracker    Lifecycle
//! ```
//!
//! The coordinator provides:
//! - Document lifecycle management (remove_document for did_close)
//! - Edit handling (invalidate_for_edits for injection regions)
//! - Injection map operations (populate, get, find_overlapping)
//! - Semantic token operations (get, store)
//! - Request tracking (start, is_active, finish, cancel)
//!
//! ## Semantic Token Cache Lifecycle
//!
//! The semantic token cache is NOT invalidated on `didChange`. This is intentional:
//! - Cached tokens are needed for `semanticTokens/full/delta` requests
//! - The `result_id` validation at lookup ensures stale tokens aren't returned
//! - Invalidating on edit would prevent delta calculations entirely

use std::collections::{HashMap, HashSet};

use tower_lsp_server::ls_types::SemanticTokens;
use tree_sitter::{InputEdit, Tree};
use url::Url;

use crate::analysis::{InjectionMap, InjectionTokenCache, SemanticTokenCache};
use crate::language::LanguageCoordinator;
use crate::language::RegionIdTracker;
use crate::language::injection::{CacheableInjectionRegion, collect_all_injections};

use super::semantic_request_tracker::SemanticRequestTracker;

/// Request ID type for tracking semantic token requests.
pub(crate) type RequestId = u64;

/// Coordinates all cache structures for semantic token operations.
///
/// This struct wraps four underlying caches and provides a unified API
/// for document lifecycle management, edit handling, and token operations.
pub(crate) struct CacheCoordinator {
    semantic_cache: SemanticTokenCache,
    injection_map: InjectionMap,
    injection_token_cache: InjectionTokenCache,
    request_tracker: SemanticRequestTracker,
}

impl CacheCoordinator {
    /// Create a new cache coordinator with empty caches.
    pub(crate) fn new() -> Self {
        Self {
            semantic_cache: SemanticTokenCache::new(),
            injection_map: InjectionMap::new(),
            injection_token_cache: InjectionTokenCache::new(),
            request_tracker: SemanticRequestTracker::new(),
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
        self.injection_map.clear(uri);
        self.injection_token_cache.clear_document(uri);
        self.request_tracker.cancel_all_for_uri(uri);
    }

    // ========================================================================
    // Edit handling (did_change)
    // ========================================================================

    /// Invalidate injection caches for regions that overlap with edits.
    ///
    /// Called BEFORE parse_document to use pre-edit byte offsets against pre-edit
    /// injection regions. This implements AC4/AC5 (PBI-083): edits outside injections
    /// preserve caches, edits inside invalidate only affected regions.
    ///
    /// PBI-167: Uses O(log n) interval tree query instead of O(n) iteration.
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
    /// This enables targeted cache invalidation (PBI-083): when an edit occurs,
    /// we can check which injection regions overlap and only invalidate those.
    ///
    /// Region IDs are generated using `RegionIdTracker` which provides position-based
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
        tracker: &RegionIdTracker,
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
            // RegionIdTracker provides stable IDs based on (uri, start_byte, end_byte, kind)
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

    /// Store semantic tokens for a document.
    pub(crate) fn store_tokens(&self, uri: Url, tokens: SemanticTokens) {
        self.semantic_cache.store(uri, tokens);
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
    use tower_lsp_server::ls_types::SemanticToken;

    fn create_test_uri(path: &str) -> Url {
        Url::parse(&format!("file:///{}", path)).unwrap()
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
        cache.store_tokens(uri.clone(), tokens);

        // Start a request
        let _request_id = cache.start_request(&uri);

        // Remove the document
        cache.remove_document(&uri);

        // Verify all caches are cleared
        assert!(cache.get_tokens_if_valid(&uri, "test-id").is_none());
        assert!(cache.get_injections(&uri).is_none());
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
        cache.store_tokens(uri.clone(), tokens);

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
    /// - Uses `RegionIdTracker` with position-based ULIDs (not `next_result_id()`)
    /// - Simulates a language change at a stable position
    /// - Verifies that cached tokens are invalidated when language changes
    ///
    /// See: Finding 1 in review - tests must exercise production behavior.
    #[test]
    fn test_language_change_invalidates_cache_with_region_id_tracker() {
        use tree_sitter::{Parser, Query};

        let cache = CacheCoordinator::new();
        let tracker = RegionIdTracker::new();
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
        assert_eq!(regions[0].language, "lua");

        // Store tokens for this injection region
        let lua_tokens = SemanticTokens {
            result_id: Some("lua-tokens".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 1,
                token_modifiers_bitset: 0,
            }],
        };
        cache
            .injection_token_cache
            .store(&uri, &initial_region_id, lua_tokens);

        // Verify tokens are stored
        assert!(
            cache
                .injection_token_cache
                .get(&uri, &initial_region_id)
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
        //   1. apply_text_diff() adjusts positions in RegionIdTracker
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
                .get(&uri, &initial_region_id)
                .is_none(),
            "cached tokens should be invalidated when language changes"
        );
    }

    /// Integration test: content change with stable region_id triggers cache invalidation.
    ///
    /// Similar to the language change test, but tests content_changed path.
    #[test]
    fn test_content_change_invalidates_cache_with_region_id_tracker() {
        use tree_sitter::{Parser, Query};

        let cache = CacheCoordinator::new();
        let tracker = RegionIdTracker::new();
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

        // Store tokens
        let lua_tokens = SemanticTokens {
            result_id: Some("lua-tokens".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 1,
                token_modifiers_bitset: 0,
            }],
        };
        cache
            .injection_token_cache
            .store(&uri, &initial_region_id, lua_tokens);

        assert!(
            cache
                .injection_token_cache
                .get(&uri, &initial_region_id)
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
                .get(&uri, &initial_region_id)
                .is_none(),
            "cached tokens should be invalidated when content changes"
        );
    }
}
