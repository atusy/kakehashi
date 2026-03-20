//! Rayon-based parallel injection processing for semantic tokens.
//!
//! This module provides work-stealing parallelism for processing language
//! injections, replacing the previous JoinSet + Semaphore async model.
//!
//! Key design:
//! - Thread-local parser caching (no cross-thread synchronization during parsing)
//! - Work-stealing via Rayon's par_iter() for top-level injections
//! - Sequential processing for nested injections (same thread, no coordination)
//! - Single spawn_blocking bridge at the top level

use std::cell::RefCell;
use std::collections::HashMap;

use tree_sitter::{Parser, Tree};

use super::injection::InjectionContext;
use super::token_collector::{ActiveInjectionBounds, RawToken, collect_host_tokens};
use crate::config::CaptureMappings;
use crate::language::LanguageCoordinator;
use crate::language::injection::{
    MAX_INJECTION_DEPTH, compute_included_ranges, intersect_included_ranges, parse_with_ranges,
    sub_select_included_ranges,
};
use crate::text::position::byte_to_utf16_col;

/// Maximum number of parsers to cache per Rayon worker thread.
///
/// This bounds memory usage for long-running LSP servers. The value balances:
/// - Typical workloads (Markdown with 3-5 different injection languages)
/// - Memory per parser (roughly 1-5MB depending on grammar complexity)
/// - Rayon worker threads (typically num_cpus, e.g., 8-16 threads)
///
/// With 8 parsers × 16 threads × 5MB = ~640MB worst case, though typical
/// usage is much lower since most documents use only 1-2 injection languages.
const MAX_CACHED_PARSERS: usize = 8;

/// Simple LRU cache for parsers with bounded size.
///
/// Uses a HashMap for O(1) lookup and a Vec for LRU tracking.
/// When full, evicts the least recently used parser.
struct LruParserCache {
    /// Map from language_id to parser
    parsers: HashMap<String, Parser>,
    /// LRU order: most recently used at the end
    order: Vec<String>,
}

impl LruParserCache {
    fn new() -> Self {
        Self {
            parsers: HashMap::with_capacity(MAX_CACHED_PARSERS),
            order: Vec::with_capacity(MAX_CACHED_PARSERS),
        }
    }

    /// Get a mutable reference to a parser, updating LRU order.
    fn get_mut(&mut self, language_id: &str) -> Option<&mut Parser> {
        if self.parsers.contains_key(language_id) {
            // Move to end of LRU order (most recently used)
            if let Some(pos) = self.order.iter().position(|k| k == language_id) {
                let key = self.order.remove(pos);
                self.order.push(key);
            }
            self.parsers.get_mut(language_id)
        } else {
            None
        }
    }

    /// Insert a parser, evicting LRU entry if at capacity.
    fn insert(&mut self, language_id: String, parser: Parser) {
        if self.parsers.len() >= MAX_CACHED_PARSERS {
            // Evict least recently used (front of order vec)
            if !self.order.is_empty() {
                let lru_key = self.order.remove(0);
                self.parsers.remove(&lru_key);
            }
        }
        self.parsers.insert(language_id.clone(), parser);
        self.order.push(language_id);
    }

    /// Check if language is cached.
    fn contains(&self, language_id: &str) -> bool {
        self.parsers.contains_key(language_id)
    }

    /// Clear the cache.
    #[cfg(test)]
    fn clear(&mut self) {
        self.parsers.clear();
        self.order.clear();
    }

    /// Get current cache size (for testing).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.parsers.len()
    }
}

// Thread-local parser cache for Rayon worker threads.
//
// Each Rayon worker thread maintains its own bounded LRU cache of parsers.
// This avoids cross-thread synchronization during parallel injection processing
// while preventing unbounded memory growth in long-running LSP servers.
thread_local! {
    static PARSER_CACHE: RefCell<LruParserCache> = RefCell::new(LruParserCache::new());
}

/// Factory for creating parsers with thread-local caching.
///
/// Uses the `LanguageRegistry` to create parsers on demand, caching them
/// in thread-local storage for reuse within the same Rayon worker.
pub(crate) struct ThreadLocalParserFactory {
    registry: crate::language::registry::LanguageRegistry,
}

impl ThreadLocalParserFactory {
    /// Create a new factory with the given language registry.
    pub fn new(registry: crate::language::registry::LanguageRegistry) -> Self {
        Self { registry }
    }

    /// Parse text using a cached parser for the given language.
    ///
    /// The parser is created on first use and cached in thread-local storage.
    /// This method handles the borrowing internally, returning an owned Tree.
    ///
    /// # Arguments
    /// * `language_id` - The language to use for parsing
    /// * `text` - The source text to parse
    /// * `included_ranges` - Optional ranges to restrict parsing to (content-text-relative).
    ///   When `Some`, only these byte ranges are visible to the parser, excluding
    ///   structural markers like blockquote `> ` prefixes.
    ///
    /// # Returns
    /// - `Some(tree)` if parsing succeeds
    /// - `None` if the language is not registered or parsing fails
    pub fn parse(
        &self,
        language_id: &str,
        text: &str,
        included_ranges: Option<&[tree_sitter::Range]>,
    ) -> Option<Tree> {
        PARSER_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();

            // Get or create parser for this language
            if !cache.contains(language_id) {
                let language = self.registry.get(language_id)?;
                let mut parser = Parser::new();
                parser.set_language(&language).ok()?;
                cache.insert(language_id.to_string(), parser);
            }

            // Parse using the cached parser, delegating the set/parse/reset
            // protocol to the shared parse_with_ranges function.
            let parser = cache.get_mut(language_id)?;
            parse_with_ranges(
                parser,
                text,
                included_ranges,
                "kakehashi::semantic",
                language_id,
            )
        })
    }

    /// Check if a language is available for parsing.
    ///
    /// # Returns
    /// `true` if the language is registered in the registry
    #[cfg(test)]
    pub fn has_language(&self, language_id: &str) -> bool {
        self.registry.contains(language_id)
    }

    /// Clear the thread-local parser cache.
    ///
    /// Useful for testing or when languages are reloaded.
    #[cfg(test)]
    pub fn clear_cache() {
        PARSER_CACHE.with(|cache| {
            cache.borrow_mut().clear();
        });
    }
}

/// Process a single injection synchronously, collecting tokens.
///
/// This function parses the injection content and collects semantic tokens,
/// including any nested injections (processed recursively in the same thread).
///
/// # Arguments
/// * `ctx` - The injection context containing language and content info
/// * `factory` - Thread-local parser factory for creating parsers
/// * `coordinator` - Language coordinator for nested injection resolution
/// * `capture_mappings` - Optional capture mappings for token type translation
/// * `host_text` - The full host document text (for position calculations)
/// * `host_lines` - Pre-split lines of the host document
/// * `depth` - Current injection depth (0 = host document)
/// * `supports_multiline` - Whether the client supports multiline tokens
///
/// # Returns
/// Vector of raw tokens collected from this injection and any nested injections
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_injection_sync(
    ctx: &InjectionContext<'_>,
    factory: &ThreadLocalParserFactory,
    coordinator: &LanguageCoordinator,
    capture_mappings: Option<&CaptureMappings>,
    host_text: &str,
    host_lines: &[&str],
    depth: usize,
    supports_multiline: bool,
) -> Vec<RawToken> {
    // Check recursion depth
    if depth >= MAX_INJECTION_DEPTH {
        return Vec::new();
    }

    // Parse the injection content (with optional included ranges for blockquotes)
    let Some(tree) = factory.parse(
        &ctx.resolved_lang,
        ctx.content_text,
        ctx.included_ranges.as_deref(),
    ) else {
        return Vec::new();
    };

    // Discover nested injections BEFORE collecting tokens so we can compute
    // exclusion ranges. This suppresses this level's captures within regions
    // that will be handled by deeper injection languages.
    let (nested_contexts, nested_exclusion_ranges) = collect_injection_contexts_sync(
        ctx.content_text,
        &tree,
        Some(&ctx.resolved_lang),
        coordinator,
        ctx.host_start_byte,
        ctx.included_ranges.as_deref(),
    );

    let mut tokens = Vec::new();

    // Collect tokens from this injection's highlight query, excluding
    // regions covered by nested injections
    collect_host_tokens(
        ctx.content_text,
        &tree,
        &ctx.highlight_query,
        Some(&ctx.resolved_lang),
        capture_mappings,
        host_text,
        host_lines,
        ctx.host_start_byte,
        depth,
        supports_multiline,
        &nested_exclusion_ranges,
        &ctx.prefix_byte_widths,
        &mut tokens,
    );

    // Recursively process nested injections (same thread, no parallelism)
    for nested_ctx in nested_contexts {
        let nested_tokens = process_injection_sync(
            &nested_ctx,
            factory,
            coordinator,
            capture_mappings,
            host_text,
            host_lines,
            depth + 1,
            supports_multiline,
        );
        tokens.extend(nested_tokens);
    }

    tokens
}

/// Collect injection contexts from a parsed tree (sync version).
///
/// This is a synchronous version of the injection context collection that
/// works without mutable parser access. It discovers all injections in the
/// given tree and returns their contexts for processing.
///
/// Returns `(contexts, exclusion_ranges)` where exclusion_ranges are the
/// content-local byte ranges of each resolved injection. These ranges
/// correspond to the regions where child injections produce their own tokens,
/// so parent captures overlapping these ranges should be suppressed.
fn collect_injection_contexts_sync<'a>(
    text: &'a str,
    tree: &Tree,
    filetype: Option<&str>,
    coordinator: &LanguageCoordinator,
    content_start_byte: usize,
    parent_included_ranges: Option<&[tree_sitter::Range]>,
) -> (Vec<InjectionContext<'a>>, Vec<(usize, usize)>) {
    use crate::language::injection::{collect_all_injections, parse_offset_directive_for_pattern};

    let current_lang = filetype.unwrap_or("unknown");
    let Some(injection_query) = coordinator.get_injection_query(current_lang) else {
        return (Vec::new(), Vec::new());
    };

    let Some(injections) = collect_all_injections(&tree.root_node(), text, Some(&injection_query))
    else {
        return (Vec::new(), Vec::new());
    };

    let mut contexts = Vec::with_capacity(injections.len());
    let mut exclusion_ranges = Vec::with_capacity(injections.len());

    for injection in injections {
        let start = injection.content_node.start_byte();
        let end = injection.content_node.end_byte();

        // Validate bounds
        if start > end || end > text.len() {
            continue;
        }

        // Extract injection content for language detection
        let injection_content = &text[start..end];

        // Resolve injection language
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&injection.language, injection_content)
        else {
            continue;
        };

        // Get highlight query for resolved language
        let Some(highlight_query) = coordinator.get_highlight_query(&resolved_lang) else {
            continue;
        };

        // Get offset directive if any
        let offset = parse_offset_directive_for_pattern(&injection_query, injection.pattern_index);

        // Calculate effective content range
        let content_node = injection.content_node;
        let (inj_start_byte, inj_end_byte) = if let Some(off) = offset {
            use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
            let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
            let effective = calculate_effective_range(text, byte_range, off);
            (effective.start, effective.end)
        } else {
            (content_node.start_byte(), content_node.end_byte())
        };

        // Validate effective range after offset adjustment
        if inj_start_byte > inj_end_byte || inj_end_byte > text.len() {
            continue;
        }

        // Compute included ranges for the injection parser (Problem 1: blockquote prefixes).
        // When include_children is false and content_node has named children
        // (e.g., block_continuation), we compute gap ranges so the injection parser
        // only sees actual code content.
        //
        // Disabled when an offset directive is active: compute_included_ranges()
        // returns ranges relative to content_node.start_byte(), but content_text
        // starts at inj_start_byte (offset-adjusted). Passing misaligned ranges
        // to the parser would produce incorrect results.
        let included_ranges = if offset.is_some() {
            None
        } else {
            let from_children =
                compute_included_ranges(&injection.content_node, injection.include_children);
            let from_parent = parent_included_ranges.and_then(|parent_ranges| {
                sub_select_included_ranges(parent_ranges, inj_start_byte, inj_end_byte)
            });
            match (from_children, from_parent) {
                (Some(child_ranges), Some(parent_ranges)) => {
                    let intersected = intersect_included_ranges(&child_ranges, &parent_ranges);
                    if intersected.is_empty() {
                        None
                    } else {
                        Some(intersected)
                    }
                }
                (Some(ranges), None) | (None, Some(ranges)) => Some(ranges),
                (None, None) => None,
            }
        };

        // Record exclusion ranges for parent token suppression (Problem 2: host token leaking).
        // When we have per-gap included ranges, push EACH gap as a separate exclusion entry
        // so that compute_active_injection_regions() produces per-line ActiveInjectionBounds values.
        // Otherwise, push the single full content range as before.
        if let Some(ref ranges) = included_ranges {
            // compute_included_ranges returns ranges relative to content_node.start_byte().
            // In this branch, offset.is_none(), so inj_start_byte == content_node.start_byte();
            // we use content_node.start_byte() explicitly as the base for clarity.
            let base_byte = content_node.start_byte();
            for r in ranges {
                let abs_start = base_byte + r.start_byte;
                let abs_end = base_byte + r.end_byte;
                exclusion_ranges.push((abs_start, abs_end));
            }
        } else {
            exclusion_ranges.push((inj_start_byte, inj_end_byte));
        }

        // Derive per-line byte prefix widths from included_ranges.
        // Each range's start_point.column tells us how many bytes of prefix
        // (e.g., "> ") precede the actual content on that line.
        let prefix_byte_widths = match &included_ranges {
            Some(ranges) => {
                let mut widths = Vec::new();
                for r in ranges {
                    let row = r.start_point.row;
                    if row >= widths.len() {
                        widths.resize(row + 1, 0);
                    }
                    if widths[row] == 0 {
                        widths[row] = r.start_point.column;
                    }
                }
                widths
            }
            None => Vec::new(),
        };

        contexts.push(InjectionContext {
            resolved_lang,
            highlight_query,
            content_text: &text[inj_start_byte..inj_end_byte],
            host_start_byte: content_start_byte + inj_start_byte,
            included_ranges,
            prefix_byte_widths,
        });
    }

    (contexts, exclusion_ranges)
}

/// Collect semantic tokens from all injections in parallel using Rayon.
///
/// This is the main entry point for parallel injection processing. It:
/// 1. Collects all top-level injection contexts from the host document
/// 2. Processes each injection in parallel using Rayon's work-stealing
/// 3. Merges all tokens and returns them sorted by position
///
/// Nested injections are processed recursively within the same Rayon worker
/// thread (no additional parallelism), avoiding coordination overhead.
///
/// # Arguments
/// * `host_text` - The full text of the host document
/// * `host_tree` - The parsed tree of the host document
/// * `host_filetype` - The filetype of the host document (e.g., "markdown")
/// * `coordinator` - Language coordinator for injection resolution
/// * `capture_mappings` - Optional capture mappings for token type translation
/// * `supports_multiline` - Whether the client supports multiline tokens
///
/// # Returns
/// Tuple of (raw tokens from all injections sorted by position, active injection regions).
///
/// An injection region is **active** if at least one token was produced from it (depth ≥ 1).
/// Inactive regions (injection resolved but no captures) don't suppress parent tokens.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_injection_tokens_parallel(
    host_text: &str,
    host_tree: &Tree,
    host_filetype: Option<&str>,
    coordinator: &LanguageCoordinator,
    capture_mappings: Option<&CaptureMappings>,
    supports_multiline: bool,
) -> (Vec<RawToken>, Vec<ActiveInjectionBounds>) {
    use rayon::prelude::*;

    // Pre-compute host lines for position calculations
    let host_lines: Vec<&str> = host_text.lines().collect();

    // Collect top-level injection contexts and their byte ranges
    let (contexts, exclusion_byte_ranges) =
        collect_injection_contexts_sync(host_text, host_tree, host_filetype, coordinator, 0, None);

    if contexts.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Create factory from coordinator's registry (cloned for thread safety)
    let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_parallel());

    // Threshold for parallel processing - below this, Rayon scheduling overhead exceeds benefit
    const PARALLEL_THRESHOLD: usize = 4;

    // Process injections: parallel for larger collections, sequential for small ones
    let mut all_tokens: Vec<RawToken> = if contexts.len() >= PARALLEL_THRESHOLD {
        // Parallel processing for larger collections
        contexts
            .par_iter()
            .flat_map(|ctx| {
                process_injection_sync(
                    ctx,
                    &factory,
                    coordinator,
                    capture_mappings,
                    host_text,
                    &host_lines,
                    1, // depth 1 (first level of injection, host is 0)
                    supports_multiline,
                )
            })
            .collect()
    } else {
        // Sequential processing for small collections to avoid Rayon overhead
        contexts
            .iter()
            .flat_map(|ctx| {
                process_injection_sync(
                    ctx,
                    &factory,
                    coordinator,
                    capture_mappings,
                    host_text,
                    &host_lines,
                    1,
                    supports_multiline,
                )
            })
            .collect()
    };

    // Sort tokens by position (line, then column)
    all_tokens.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.column.cmp(&b.column)));

    // Convert byte ranges to line/column ActiveInjectionBounds values, but only for
    // regions that actually produced tokens (= "active" injection regions).
    let active_regions = compute_active_injection_regions(
        host_text,
        &host_lines,
        &exclusion_byte_ranges,
        &all_tokens,
    );

    (all_tokens, active_regions)
}

/// Convert byte-based exclusion ranges to line/column `ActiveInjectionBounds`s,
/// keeping only those regions that contain at least one injection token.
fn compute_active_injection_regions(
    host_text: &str,
    host_lines: &[&str],
    byte_ranges: &[(usize, usize)],
    tokens: &[RawToken],
) -> Vec<ActiveInjectionBounds> {
    byte_ranges
        .iter()
        .filter_map(|&(start_byte, end_byte)| {
            // Convert byte range to line/col
            let (start_line, start_col) = byte_to_line_col(host_text, host_lines, start_byte);
            let (end_line, end_col) = byte_to_line_col(host_text, host_lines, end_byte);

            // Check if any token (depth ≥ 1) falls within this region
            let has_injection_tokens = tokens.iter().any(|t| {
                t.depth >= 1
                    && ((t.line > start_line || (t.line == start_line && t.column >= start_col))
                        && (t.line < end_line || (t.line == end_line && t.column < end_col)))
            });

            if has_injection_tokens {
                Some(ActiveInjectionBounds {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Convert a byte offset in host_text to a (line, utf16_col) pair.
fn byte_to_line_col(host_text: &str, host_lines: &[&str], byte_offset: usize) -> (usize, usize) {
    let byte_offset = byte_offset.min(host_text.len());
    // Snap to valid UTF-8 char boundary (tree-sitter always provides valid offsets,
    // but guard defensively against unexpected inputs).
    let byte_offset = {
        let mut b = byte_offset;
        while b > 0 && !host_text.is_char_boundary(b) {
            b -= 1;
        }
        b
    };
    let line = host_text[..byte_offset]
        .chars()
        .filter(|c| *c == '\n')
        .count();
    let line_start_byte = if line == 0 {
        0
    } else {
        host_text[..byte_offset].rfind('\n').map_or(0, |p| p + 1)
    };
    let col_byte = byte_offset - line_start_byte;
    let line_text = host_lines.get(line).unwrap_or(&"");
    let col_utf16 = byte_to_utf16_col(line_text, col_byte);
    (line, col_utf16)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tree_sitter::Query;

    use super::*;
    use crate::language::registry::LanguageRegistry;

    fn create_test_registry() -> LanguageRegistry {
        let registry = LanguageRegistry::new();
        registry.register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        registry
    }

    #[test]
    fn test_thread_local_parser_factory_parses_code() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        let code = "fn main() {}";
        let tree = factory.parse("rust", code, None);

        assert!(tree.is_some(), "Should parse registered language");
        let tree = tree.unwrap();
        assert!(
            !tree.root_node().has_error(),
            "Parse tree should not have errors"
        );
    }

    #[test]
    fn test_thread_local_parser_factory_returns_none_for_unknown() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        let tree = factory.parse("unknown_language", "some code", None);
        assert!(
            tree.is_none(),
            "Should return None for unregistered language"
        );
    }

    #[test]
    fn test_thread_local_parser_factory_caches_parser() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        // Clear cache first to ensure clean state
        ThreadLocalParserFactory::clear_cache();

        // First parse creates and caches parser
        let tree1 = factory.parse("rust", "fn main() {}", None);
        assert!(tree1.is_some());

        // Second parse reuses cached parser
        let tree2 = factory.parse("rust", "fn test() {}", None);
        assert!(tree2.is_some());

        // Both should produce valid parse trees
        assert!(!tree1.unwrap().root_node().has_error());
        assert!(!tree2.unwrap().root_node().has_error());
    }

    #[test]
    fn test_thread_local_parser_factory_clear_cache() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        // Parse to create cached parser
        let _ = factory.parse("rust", "fn main() {}", None);

        // Clear the cache
        ThreadLocalParserFactory::clear_cache();

        // Verify can still parse after cache clear (parser recreated)
        let tree = factory.parse("rust", "fn test() {}", None);
        assert!(tree.is_some(), "Should still parse after cache clear");
    }

    #[test]
    fn test_thread_local_parser_factory_has_language() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        assert!(
            factory.has_language("rust"),
            "Should have registered language"
        );
        assert!(
            !factory.has_language("unknown"),
            "Should not have unregistered language"
        );
    }

    #[test]
    fn test_parser_handles_complex_code() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        let code = r#"
            fn main() {
                let x = 42;
                let y = "hello";
                println!("{} {}", x, y);
            }
        "#;

        let tree = factory.parse("rust", code, None);
        assert!(tree.is_some(), "Should parse complex code");
        assert!(
            !tree.unwrap().root_node().has_error(),
            "Complex code should parse without errors"
        );
    }

    // Tests for sync injection processing
    // Note: Full integration tests require LanguageCoordinator setup with search paths,
    // so these are basic structural tests. Full testing is in the parent module.

    #[test]
    fn test_injection_context_struct_fields() {
        // Verify InjectionContext has the expected fields
        let registry = create_test_registry();
        let language = registry.get("rust").unwrap();

        // Create a simple query for testing
        let query = Query::new(&language, "(identifier) @variable").unwrap();

        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query: Arc::new(query),
            content_text: "fn main() {}",
            host_start_byte: 100,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
        };

        assert_eq!(ctx.resolved_lang, "rust");
        assert_eq!(ctx.content_text, "fn main() {}");
        assert_eq!(ctx.host_start_byte, 100);
    }

    #[test]
    fn test_byte_to_line_col_mid_char_boundary() {
        // Test that byte_to_line_col handles mid-UTF-8-character offsets defensively
        // by snapping to the nearest valid char boundary.
        let text = "あいう"; // Three 3-byte UTF-8 characters (9 bytes total)
        let lines: Vec<&str> = text.lines().collect();

        // Offset 0 is valid (start of first char)
        let (line, col) = byte_to_line_col(text, &lines, 0);
        assert_eq!(line, 0);
        assert_eq!(col, 0);

        // Offset 1 is mid-character (should snap to 0)
        let (line, col) = byte_to_line_col(text, &lines, 1);
        assert_eq!(line, 0, "Mid-character offset should snap to line 0");
        assert_eq!(col, 0, "Mid-character offset should snap to col 0");

        // Offset 2 is mid-character (should snap to 0)
        let (line, col) = byte_to_line_col(text, &lines, 2);
        assert_eq!(line, 0);
        assert_eq!(col, 0);

        // Offset 3 is valid (start of second char)
        let (line, col) = byte_to_line_col(text, &lines, 3);
        assert_eq!(line, 0);
        assert_eq!(col, 1); // One UTF-16 code unit (Japanese chars are in BMP)

        // Offset 4 is mid-character (should snap to 3)
        let (line, col) = byte_to_line_col(text, &lines, 4);
        assert_eq!(line, 0);
        assert_eq!(col, 1); // Should snap to start of second char
    }

    #[test]
    fn test_process_injection_sync_with_simple_code() {
        use crate::config::WorkspaceSettings;

        // Set up coordinator with search paths
        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load rust language
        let load_result = coordinator.ensure_language_loaded("rust");
        if !load_result.success {
            // Skip test if rust parser not available in CI
            eprintln!("Skipping: rust parser not available");
            return;
        }

        let Some(highlight_query) = coordinator.get_highlight_query("rust") else {
            eprintln!("Skipping: rust highlight query not available");
            return;
        };

        // Create factory with the coordinator's registry
        let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_testing());

        let code = "fn main() {}";
        let host_text = code;
        let host_lines: Vec<&str> = host_text.lines().collect();

        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query,
            content_text: code,
            host_start_byte: 0,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
        };

        let tokens = process_injection_sync(
            &ctx,
            &factory,
            &coordinator,
            None,
            host_text,
            &host_lines,
            1, // depth 1 (not host document)
            false,
        );

        // Should produce some tokens (at minimum "fn" keyword and "main" identifier)
        assert!(
            !tokens.is_empty(),
            "Should produce tokens for Rust code. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_process_injection_sync_respects_max_depth() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let load_result = coordinator.ensure_language_loaded("rust");
        if !load_result.success {
            eprintln!("Skipping: rust parser not available");
            return;
        }

        let Some(highlight_query) = coordinator.get_highlight_query("rust") else {
            eprintln!("Skipping: rust highlight query not available");
            return;
        };

        let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_testing());

        let code = "fn main() {}";
        let host_text = code;
        let host_lines: Vec<&str> = host_text.lines().collect();

        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query,
            content_text: code,
            host_start_byte: 0,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
        };

        // Process at MAX_INJECTION_DEPTH should return empty
        let tokens = process_injection_sync(
            &ctx,
            &factory,
            &coordinator,
            None,
            host_text,
            &host_lines,
            MAX_INJECTION_DEPTH,
            false,
        );

        assert!(
            tokens.is_empty(),
            "Should return empty at MAX_INJECTION_DEPTH"
        );
    }

    /// Returns the search path for tree-sitter grammars.
    fn test_search_path() -> String {
        std::env::var("TREE_SITTER_GRAMMARS").unwrap_or_else(|_| "deps/tree-sitter".to_string())
    }

    // Tests for parallel token collection

    #[test]
    fn test_collect_injection_tokens_parallel_empty_document() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load markdown (host language)
        let load_result = coordinator.ensure_language_loaded("markdown");
        if !load_result.success {
            eprintln!("Skipping: markdown parser not available");
            return;
        }

        // Parse an empty markdown document
        let text = "";
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Collect tokens - should be empty for empty document
        let (tokens, _regions) = collect_injection_tokens_parallel(
            text,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
        );

        assert!(tokens.is_empty(), "Empty document should have no tokens");
    }

    #[test]
    fn test_collect_injection_tokens_parallel_with_lua_block() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load both markdown and lua
        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        // Markdown with a Lua code block
        let text = r#"# Hello

```lua
local x = 42
```
"#;

        // Parse the markdown document
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Collect tokens in parallel
        let (tokens, _regions) = collect_injection_tokens_parallel(
            text,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
        );

        // Should have tokens from the Lua injection
        assert!(
            !tokens.is_empty(),
            "Should have tokens from Lua injection. Got: {:?}",
            tokens
        );

        // Look for the "local" keyword token (should be at line 3, col 0)
        let has_local_keyword = tokens.iter().any(|t| {
            t.line == 3
                && t.column == 0
                && t.kind
                    == crate::analysis::semantic::token_collector::TokenKind::Mapped(
                        "keyword".to_string(),
                    )
        });

        assert!(
            has_local_keyword,
            "Should have 'local' keyword token at line 3, col 0. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_collect_injection_tokens_parallel_tokens_sorted() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        // Multiple Lua code blocks at different positions
        let text = r#"# Doc

```lua
local a = 1
```

More text

```lua
local b = 2
```
"#;

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let (tokens, _regions) = collect_injection_tokens_parallel(
            text,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
        );

        // Verify tokens are sorted by position
        let mut prev_line = 0usize;
        let mut prev_col = 0usize;
        for token in &tokens {
            assert!(
                token.line > prev_line || (token.line == prev_line && token.column >= prev_col),
                "Tokens should be sorted by (line, column). Got line {} col {} after line {} col {}",
                token.line,
                token.column,
                prev_line,
                prev_col
            );
            prev_line = token.line;
            prev_col = token.column;
        }
    }

    // Tests for LRU cache behavior

    #[test]
    fn test_lru_parser_cache_basic_operations() {
        let mut cache = LruParserCache::new();

        // Initially empty
        assert!(!cache.contains("rust"));
        assert_eq!(cache.len(), 0);

        // Create a test parser
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        // Insert
        cache.insert("rust".to_string(), parser);
        assert!(cache.contains("rust"));
        assert_eq!(cache.len(), 1);

        // Get mutable reference
        let parser_ref = cache.get_mut("rust");
        assert!(parser_ref.is_some());

        // Unknown language returns None
        assert!(cache.get_mut("unknown").is_none());
    }

    #[test]
    fn test_lru_parser_cache_eviction() {
        let mut cache = LruParserCache::new();

        // Fill cache to MAX_CACHED_PARSERS
        for i in 0..MAX_CACHED_PARSERS {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            cache.insert(format!("lang{}", i), parser);
        }

        assert_eq!(cache.len(), MAX_CACHED_PARSERS);
        assert!(cache.contains("lang0")); // First inserted (LRU)

        // Insert one more - should evict lang0 (least recently used)
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        cache.insert("lang_new".to_string(), parser);

        assert_eq!(cache.len(), MAX_CACHED_PARSERS);
        assert!(!cache.contains("lang0"), "LRU entry should be evicted");
        assert!(cache.contains("lang_new"), "New entry should be present");
        assert!(cache.contains("lang1"), "Second-oldest should still exist");
    }

    #[test]
    fn test_lru_parser_cache_access_updates_order() {
        let mut cache = LruParserCache::new();

        // Insert two parsers
        for lang in ["lang0", "lang1"] {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            cache.insert(lang.to_string(), parser);
        }

        // Access lang0 to make it recently used
        let _ = cache.get_mut("lang0");

        // Fill the rest of the cache
        for i in 2..MAX_CACHED_PARSERS {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            cache.insert(format!("lang{}", i), parser);
        }

        // Insert one more - should evict lang1 (now LRU), not lang0
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        cache.insert("lang_new".to_string(), parser);

        assert!(
            cache.contains("lang0"),
            "Recently accessed lang0 should not be evicted"
        );
        assert!(!cache.contains("lang1"), "LRU lang1 should be evicted");
    }

    #[test]
    fn test_lru_parser_cache_clear() {
        let mut cache = LruParserCache::new();

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        cache.insert("rust".to_string(), parser);

        assert_eq!(cache.len(), 1);

        cache.clear();

        assert_eq!(cache.len(), 0);
        assert!(!cache.contains("rust"));
    }
}
