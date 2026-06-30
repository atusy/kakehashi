//! Semantic-token caches with `result_id` validation and injection-region tracking.
//!
//! Three layers (all `DashMap`-backed for concurrent access):
//! - `SemanticTokenCache`: per-URI tokens, served when LSP `result_id` matches.
//! - `InjectionMap`: per-URI interval tree (rust_lapper) of injection regions
//!   keyed by region_id (ULID), giving O(log n) overlap queries so an edit only
//!   invalidates regions it actually touches.
//! - `InjectionTokenCache`: per-(URI, region_id) tokens, reusable when an edit
//!   lies outside that region.

use crate::language::injection::CacheableInjectionRegion;
use dashmap::DashMap;
use rust_lapper::{Interval, Lapper};
use tower_lsp_server::ls_types::SemanticTokens;
use url::Url;

/// Cached semantic tokens for delta calculations.
#[derive(Clone)]
pub struct CachedSemanticTokens {
    pub tokens: SemanticTokens,
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
    cache: DashMap<Url, CachedSemanticTokens>,
}

impl SemanticTokenCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Store semantic tokens for a document, tagged with the `cache_key` they
    /// were computed under (see [`get_if_current`](Self::get_if_current)).
    pub fn store(&self, uri: Url, tokens: SemanticTokens, cache_key: u64) {
        self.cache
            .insert(uri, CachedSemanticTokens { tokens, cache_key });
    }

    /// Retrieve semantic tokens for a document.
    pub fn get(&self, uri: &Url) -> Option<CachedSemanticTokens> {
        self.cache.get(uri).map(|entry| entry.clone())
    }

    /// Get cached tokens if they were computed under this `cache_key` — i.e. the
    /// document text AND the settings generation are unchanged since they were
    /// cached, so re-tokenizing would reproduce them. Returns None on a mismatch
    /// (text edit or config reload) or no entry.
    pub fn get_if_current(&self, uri: &Url, cache_key: u64) -> Option<CachedSemanticTokens> {
        self.cache.get(uri).and_then(|entry| {
            if entry.cache_key == cache_key {
                Some(entry.clone())
            } else {
                None
            }
        })
    }

    /// Drop every cached entry. Used (alongside a generation bump) on a
    /// settings/query reload to reclaim memory; the generation bump is what makes
    /// the invalidation race-safe, this just stops the dead entries from leaking.
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Get cached tokens if the result_id matches.
    ///
    /// Returns None if:
    /// - No tokens are cached for this URI
    /// - The cached result_id doesn't match the expected one
    pub fn get_if_valid(
        &self,
        uri: &Url,
        expected_result_id: &str,
    ) -> Option<CachedSemanticTokens> {
        self.cache.get(uri).and_then(|entry| {
            if entry.tokens.result_id.as_deref() == Some(expected_result_id) {
                Some(entry.clone())
            } else {
                None
            }
        })
    }

    /// Remove cached tokens for a document (e.g., on document close).
    pub fn remove(&self, uri: &Url) {
        self.cache.remove(uri);
    }
}

impl Default for SemanticTokenCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe map of injection regions per document.
///
/// Tracks all `CacheableInjectionRegion`s for each document URI,
/// enabling targeted cache invalidation when only specific injections change.
/// Uses interval tree (rust_lapper) for O(log n) overlap queries.
pub struct InjectionMap {
    /// Stores interval trees per document URI
    /// Each Interval contains the byte range and the full CacheableInjectionRegion as data
    lappers: DashMap<Url, Lapper<usize, CacheableInjectionRegion>>,
}

impl InjectionMap {
    /// Create a new empty injection map.
    pub fn new() -> Self {
        Self {
            lappers: DashMap::new(),
        }
    }

    /// Store injection regions for a document, replacing any existing regions.
    /// Builds an interval tree from the regions for efficient overlap queries.
    pub fn insert(&self, uri: Url, regions: Vec<CacheableInjectionRegion>) {
        // Convert regions to intervals for the Lapper
        let intervals: Vec<Interval<usize, CacheableInjectionRegion>> = regions
            .into_iter()
            .map(|region| {
                let start = region.byte_range.start;
                let stop = region.byte_range.end;
                Interval {
                    start,
                    stop,
                    val: region,
                }
            })
            .collect();

        // Create Lapper from intervals (builds interval tree)
        let lapper = Lapper::new(intervals);
        self.lappers.insert(uri, lapper);
    }

    /// Retrieve all injection regions for a document.
    pub fn get(&self, uri: &Url) -> Option<Vec<CacheableInjectionRegion>> {
        self.lappers
            .get(uri)
            .map(|entry| entry.iter().map(|interval| interval.val.clone()).collect())
    }

    /// Remove all injection regions for a document (e.g., on document close).
    pub fn clear(&self, uri: &Url) {
        self.lappers.remove(uri);
    }

    /// Find the injection region containing the given byte position (test-only).
    #[cfg(test)]
    pub fn find_at_position(
        &self,
        uri: &Url,
        byte_position: usize,
    ) -> Option<CacheableInjectionRegion> {
        self.lappers.get(uri).and_then(|lapper| {
            // Find intervals that overlap the single byte position
            lapper
                .find(byte_position, byte_position + 1)
                .next()
                .map(|interval| interval.val.clone())
        })
    }

    /// Find all injection regions that overlap with the given byte range.
    ///
    /// Uses O(log n) interval tree query instead of O(n) iteration through all regions.
    ///
    /// Returns `Some(Vec)` with overlapping regions (may be empty), or `None` if URI unknown.
    pub fn find_overlapping(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
    ) -> Option<Vec<CacheableInjectionRegion>> {
        self.lappers.get(uri).map(|lapper| {
            lapper
                .find(start, end)
                .map(|interval| interval.val.clone())
                .collect()
        })
    }
}

impl Default for InjectionMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe cache for per-injection semantic tokens.
///
/// Unlike `SemanticTokenCache` which stores tokens per document, this cache
/// stores tokens keyed by (uri, region_id), enabling injection-level caching.
pub struct InjectionTokenCache {
    cache: DashMap<(Url, String), SemanticTokens>,
}

impl InjectionTokenCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Store semantic tokens for a specific injection region (test-only).
    #[cfg(test)]
    pub fn store(&self, uri: &Url, region_id: &str, tokens: SemanticTokens) {
        self.cache
            .insert((uri.clone(), region_id.to_string()), tokens);
    }

    /// Retrieve semantic tokens for a specific injection region (test-only).
    #[cfg(test)]
    pub fn get(&self, uri: &Url, region_id: &str) -> Option<SemanticTokens> {
        let result = self
            .cache
            .get(&(uri.clone(), region_id.to_string()))
            .map(|entry| entry.clone());

        if result.is_some() {
            log::debug!(
                target: "kakehashi::injection_cache",
                "Cache HIT for injection region '{}' in {}",
                region_id,
                uri.path()
            );
        } else {
            log::trace!(
                target: "kakehashi::injection_cache",
                "Cache MISS for injection region '{}' in {}",
                region_id,
                uri.path()
            );
        }

        result
    }

    /// Remove cached tokens for a specific injection region.
    pub fn remove(&self, uri: &Url, region_id: &str) {
        self.cache.remove(&(uri.clone(), region_id.to_string()));
    }

    /// Remove all cached tokens for a document (all its injection regions).
    pub fn clear_document(&self, uri: &Url) {
        self.cache.retain(|key, _| &key.0 != uri);
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
        cache.store(uri.clone(), tokens.clone(), 0);

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

        cache.store(uri.clone(), tokens, 0);

        // Matching result_id returns tokens
        let valid = cache.get_if_valid(&uri, "42");
        assert!(
            valid.is_some(),
            "Should return tokens when result_id matches"
        );
        assert_eq!(valid.unwrap().tokens.data[0].length, 10);

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
    fn get_if_current_matches_on_content_hash() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///t.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("7".to_string()),
            data: vec![],
        };
        cache.store(uri.clone(), tokens, 0xABCD);

        // Same content hash => the document is unchanged => serve cached tokens.
        let hit = cache.get_if_current(&uri, 0xABCD);
        assert!(hit.is_some(), "should hit when the content hash matches");
        assert_eq!(hit.unwrap().tokens.result_id, Some("7".to_string()));

        // Different content hash => the text changed => recompute (miss).
        assert!(
            cache.get_if_current(&uri, 0x1234).is_none(),
            "should miss when the content hash differs"
        );

        // Unknown URI => miss.
        let other = Url::parse("file:///o.rs").unwrap();
        assert!(cache.get_if_current(&other, 0xABCD).is_none());
    }

    #[test]
    fn clear_drops_all_entries() {
        let cache = SemanticTokenCache::new();
        let uri = Url::parse("file:///t.rs").unwrap();
        let tokens = SemanticTokens {
            result_id: Some("1".to_string()),
            data: vec![],
        };
        cache.store(uri.clone(), tokens, 0xAA);
        assert!(cache.get_if_current(&uri, 0xAA).is_some());

        // A config reload changes tokenization for the same text, so the whole
        // cache must drop — a later request recomputes against the new queries.
        cache.clear();
        assert!(
            cache.get_if_current(&uri, 0xAA).is_none(),
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
        cache.store(uri.clone(), tokens, 0);
        assert!(cache.get(&uri).is_some(), "Should have cached tokens");

        // Remove on close
        cache.remove(&uri);
        assert!(cache.get(&uri).is_none(), "Should return None after remove");

        // Removing non-existent URI is safe
        let other_uri = Url::parse("file:///other.rs").unwrap();
        cache.remove(&other_uri); // Should not panic
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

    #[test]
    fn test_injection_token_cache_store_retrieve() {
        let cache = InjectionTokenCache::new();
        let uri = Url::parse("file:///test.md").unwrap();

        let tokens1 = SemanticTokens {
            result_id: Some("lua-region-1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 5,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };

        let tokens2 = SemanticTokens {
            result_id: Some("python-region-2".to_string()),
            data: vec![SemanticToken {
                delta_line: 1,
                delta_start: 2,
                length: 10,
                token_type: 1,
                token_modifiers_bitset: 0,
            }],
        };

        // Store tokens for different regions in same document
        cache.store(&uri, "region-1", tokens1.clone());
        cache.store(&uri, "region-2", tokens2.clone());

        // Retrieve by (uri, region_id)
        let retrieved1 = cache.get(&uri, "region-1");
        assert!(retrieved1.is_some(), "Should retrieve tokens for region-1");
        assert_eq!(retrieved1.unwrap().data[0].length, 5);

        let retrieved2 = cache.get(&uri, "region-2");
        assert!(retrieved2.is_some(), "Should retrieve tokens for region-2");
        assert_eq!(retrieved2.unwrap().data[0].length, 10);

        // Non-existent region returns None
        assert!(cache.get(&uri, "region-3").is_none());

        // Non-existent URI returns None
        let other_uri = Url::parse("file:///other.md").unwrap();
        assert!(cache.get(&other_uri, "region-1").is_none());
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

        // Set up cached tokens for each region
        let lua_tokens = SemanticTokens {
            result_id: Some("lua-tokens".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 3,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };
        token_cache.store(&uri, "lua-region-1", lua_tokens);

        // Find region containing byte offset and get its cached tokens
        let regions = injection_map.get(&uri).unwrap();
        let region_at_byte_30 = regions.iter().find(|r| r.byte_range.contains(&30));
        assert!(region_at_byte_30.is_some(), "Should find region at byte 30");

        let region = region_at_byte_30.unwrap();
        assert_eq!(region.language, "lua");

        // Use region_id to get cached tokens
        let cached = token_cache.get(&uri, &region.region_id);
        assert!(cached.is_some(), "Should have cached tokens for lua region");
        assert_eq!(cached.unwrap().data[0].length, 3);
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
