mod delta;
mod finalize;
mod injection;
mod legend;
mod parallel;
pub(crate) use parallel::invalidate_thread_local_parser_caches;
mod range;
mod semantic_artifact;
mod token_collector;

use crate::config::CaptureMappings;
use tower_lsp_server::ls_types::SemanticTokensResult;
use tree_sitter::{Query, Tree};

// Re-export crate-internal API from submodules
pub(crate) use delta::calculate_delta_or_full;
pub(crate) use legend::{LEGEND_MODIFIERS, LEGEND_TYPES};
#[cfg(test)]
pub(crate) use parallel::DISCOVERY_REUSE_HITS;
pub(crate) use parallel::build_document_discovery;
pub(crate) use range::filter_semantic_tokens_by_range;
pub(crate) use semantic_artifact::{
    SemanticArtifact, SemanticArtifactConsumer, SemanticArtifactIdentity, SemanticArtifactSlot,
};

// Re-export for parallel processing
use parallel::{INJECTION_CACHE_MIN_REGIONS, InjectionCacheCtx, collect_injection_tokens_parallel};

/// Owned handle the LSP layer passes into [`handle_semantic_tokens_full`] to
/// enable per-region injection-token caching (#529). `None` disables caching
/// (range requests, tests), reproducing the pre-#529 behavior. Borrowed into an
/// [`InjectionCacheCtx`] inside the blocking task.
pub(crate) struct InjectionCacheParams {
    pub uri: url::Url,
    pub tracker: std::sync::Arc<crate::language::NodeTracker>,
    pub cache: std::sync::Arc<crate::analysis::InjectionTokenCache>,
    pub generation: u64,
    /// The live-input store plus the snapshot identity this compute serves,
    /// re-checked INSIDE the work-unit: region-id minting writes into the
    /// live-coordinate `NodeTracker`, which a stale serve must not do (the
    /// same "a stale read never mints" invariant the captures walk enforces).
    pub documents: std::sync::Arc<crate::document::DocumentStore>,
    pub parsed_version: u64,
    pub incarnation: u64,
    /// Owned injection discovery from the snapshot being tokenized
    /// (parse-snapshot ADR §3, the don't-discover-twice lever): rebuilt into
    /// contexts instead of re-running the injection query, when its
    /// `generation` still matches `generation` above. `None` re-discovers
    /// inline (below the region gate, combined groups, or discovery
    /// incomplete at parse time).
    pub discovery: Option<std::sync::Arc<crate::document::DiscoveredInjections>>,
}

// Internal re-exports for production code
use finalize::finalize_tokens_cancellable;
use token_collector::{build_line_start_bytes, collect_host_tokens};

// Region-local token type persisted by the injection-token cache (#529). Lives
// in `token_collector`; re-exported here so `semantic_cache` can name it.
pub(crate) use token_collector::RawToken;
// `TokenKind` is only needed to construct `RawToken`s in `semantic_cache` tests
// (the production re-anchor path touches only line/column), so its re-export is
// test-gated to avoid an unused import in release builds.
#[cfg(test)]
pub(crate) use token_collector::TokenKind;

// Test-only imports
#[cfg(test)]
use {delta::calculate_semantic_tokens_delta, tower_lsp_server::ls_types::SemanticTokens};

fn should_parallelize_host_and_injections(
    compute_threads: usize,
    current_generation: u64,
    discovery: Option<(u64, bool, usize)>,
) -> bool {
    compute_threads >= 3
        && discovery.is_some_and(|(generation, complete, region_count)| {
            generation == current_generation
                && complete
                && region_count >= INJECTION_CACHE_MIN_REGIONS
        })
}

fn run_sequential_injection<T>(
    host_complete: bool,
    cancelled: bool,
    injection_work: impl FnOnce() -> T,
) -> Option<T> {
    (!cancelled && host_complete).then(injection_work)
}

/// Synchronously compute full-document semantic tokens from request-scoped
/// inputs. The caller must already be running on the bounded compute pool; a
/// nested Rayon iterator then stays on that pool. Thread-local parser caches
/// avoid cross-thread synchronization on parse.
///
/// `cancel` is polled during the host-query walk, per region inside the
/// injection pass, and throughout final token shaping. Returns `None` once
/// cancelled so callers cannot publish a partial result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_semantic_tokens_full(
    compute_threads: usize,
    text: std::sync::Arc<str>,
    tree: Tree,
    query: std::sync::Arc<Query>,
    filetype: Option<String>,
    capture_mappings: Option<std::sync::Arc<CaptureMappings>>,
    coordinator: std::sync::Arc<crate::language::LanguageCoordinator>,
    supports_multiline: bool,
    injection_cache: Option<InjectionCacheParams>,
    cancel: Option<crate::cancel::CancelToken>,
) -> Option<SemanticTokensResult> {
    let is_cancelled = || crate::cancel::is_cancelled(cancel.as_ref());
    let compute_started = std::time::Instant::now();
    let mut host_tokens: Vec<RawToken> = Vec::with_capacity(1000);
    let lines: Vec<&str> = text.lines().collect();
    let line_starts = build_line_start_bytes(&text);

    // Borrow the owned cache handle into a request-scoped context for the
    // injection pass (#529); `None` keeps the uncached behavior.
    let cache_ctx = injection_cache.as_ref().map(|p| InjectionCacheCtx {
        uri: &p.uri,
        tracker: p.tracker.as_ref(),
        cache: p.cache.as_ref(),
        generation: p.generation,
        incarnation: p.incarnation,
        // Currency latch for region-id minting, taken here inside the
        // work-unit so the race window is the compute itself, not the
        // pool-queue wait. A stale serve goes read-only on the tracker
        // (reuse for unshifted regions, no cache entry for unknown ones).
        mint_regions: p.documents.latest_snapshot(&p.uri).is_some_and(|view| {
            view.slot.current_incarnation == p.incarnation
                && view.content_version == p.parsed_version
        }),
        discovery: p.discovery.as_deref(),
    });

    let should_parallelize = cache_ctx.as_ref().is_some_and(|ctx| {
        should_parallelize_host_and_injections(
            compute_threads,
            ctx.generation,
            ctx.discovery.map(|discovery| {
                (
                    discovery.generation,
                    discovery.complete,
                    discovery.regions.len(),
                )
            }),
        )
    });
    let mut host_work = || {
        let started = std::time::Instant::now();
        let complete = collect_host_tokens(
            &text,
            &tree,
            &query,
            filetype.as_deref(),
            capture_mappings.as_deref(),
            &text,
            &lines,
            &line_starts,
            0,
            0,
            supports_multiline,
            &[],
            &[],
            cancel.as_ref(),
            &mut host_tokens,
        );
        (complete, started.elapsed())
    };
    let injection_work = || {
        let started = std::time::Instant::now();
        let result = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            filetype.as_deref(),
            &coordinator,
            capture_mappings.as_deref(),
            supports_multiline,
            cache_ctx.as_ref(),
            cancel.as_ref(),
        );
        (result, started.elapsed())
    };

    // Host highlighting and a substantial injection pass read the same
    // immutable snapshot and only meet during finalization. Overlap them
    // when discovery proves enough injection work exists to amortize the
    // second Rayon branch; otherwise retain the sequential fast path.
    let ((host_complete, host_elapsed), (injection_result, injections_elapsed)) =
        if should_parallelize {
            // Match the sequential injection boundary: do not dispatch
            // either Rayon branch when supersession has already landed.
            if is_cancelled() {
                return None;
            }
            rayon::join(host_work, injection_work)
        } else {
            let host_result = host_work();
            let injection_result =
                run_sequential_injection(host_result.0, is_cancelled(), injection_work)?;
            (host_result, injection_result)
        };

    if !host_complete {
        return None;
    }
    let (injection_tokens, active_injection_regions) = injection_result;

    // A supersede observed during the injection pass leaves a partial token
    // set; drop it so the handler never stores partial results.
    if is_cancelled() {
        return None;
    }

    // Merge injection tokens with host tokens
    host_tokens.extend(injection_tokens);

    let finalize_start = std::time::Instant::now();
    let result = finalize_tokens_cancellable(
        host_tokens,
        &active_injection_regions,
        &lines,
        cancel.as_ref(),
    );
    let finalize_elapsed = finalize_start.elapsed();
    let compute_elapsed = compute_started.elapsed();
    if is_cancelled() {
        return None;
    }

    // Host/injection durations overlap when `parallel=true`, so `compute`
    // is the authoritative wall time; branch durations must not be summed.
    log::debug!(
        target: "kakehashi::semantic",
        "[SEMANTIC_TOKENS] compute phases: compute={}us host={}us injections={}us finalize={}us parallel={} regions_reused={}",
        compute_elapsed.as_micros(),
        host_elapsed.as_micros(),
        injections_elapsed.as_micros(),
        finalize_elapsed.as_micros(),
        should_parallelize,
        injection_cache
            .as_ref()
            // The same generation gate the reuse path applies: a
            // present-but-reload-stale discovery is NOT reused, and
            // reporting its count would mislead profiling.
            .and_then(|p| {
                p.discovery
                    .as_ref()
                    .filter(|d| d.generation == p.generation && d.complete)
            })
            .map(|d| d.regions.len().to_string())
            .unwrap_or_else(|| "none".to_string()),
    );

    result
}

/// Dispatch the synchronous semantic computation as one bounded compute-pool
/// work unit. The cancellation token also acts as the dequeue hook, so work
/// superseded while queued never starts.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_semantic_tokens_full(
    pool: &crate::compute_pool::ComputePool,
    text: std::sync::Arc<str>,
    tree: Tree,
    query: std::sync::Arc<Query>,
    filetype: Option<String>,
    capture_mappings: Option<std::sync::Arc<CaptureMappings>>,
    coordinator: std::sync::Arc<crate::language::LanguageCoordinator>,
    supports_multiline: bool,
    injection_cache: Option<InjectionCacheParams>,
    cancel: Option<crate::cancel::CancelToken>,
) -> Option<SemanticTokensResult> {
    let compute_threads = pool.thread_count();
    let dequeue_cancel = cancel.clone();
    pool.run(dequeue_cancel, move || {
        compute_semantic_tokens_full(
            compute_threads,
            text,
            tree,
            query,
            filetype,
            capture_mappings,
            coordinator,
            supports_multiline,
            injection_cache,
            cancel,
        )
    })
    .await
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_host_injection_gate_checks_pool_and_discovery_contract() {
        let threshold = INJECTION_CACHE_MIN_REGIONS;
        let cases = [
            (1, 7, None, false, "single-thread pool"),
            (2, 7, Some((7, true, threshold)), false, "two-thread pool"),
            (3, 7, None, false, "absent discovery"),
            (3, 7, Some((6, true, threshold)), false, "stale discovery"),
            (
                3,
                7,
                Some((7, false, threshold)),
                false,
                "partial discovery",
            ),
            (
                3,
                7,
                Some((7, true, threshold - 1)),
                false,
                "below threshold",
            ),
            (3, 7, Some((7, true, threshold)), true, "eligible"),
        ];
        for (threads, generation, discovery, expected, label) in cases {
            assert_eq!(
                should_parallelize_host_and_injections(threads, generation, discovery),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn sequential_injection_does_not_start_after_host_cancel() {
        let starts = std::cell::Cell::new(0);
        let mut work = || {
            starts.set(starts.get() + 1);
            "ran"
        };

        assert_eq!(run_sequential_injection(false, false, &mut work), None);
        assert_eq!(run_sequential_injection(true, true, &mut work), None);
        assert_eq!(starts.get(), 0);
        assert_eq!(
            run_sequential_injection(true, false, &mut work),
            Some("ran")
        );
        assert_eq!(starts.get(), 1);
    }
    use tower_lsp_server::ls_types::{Range, SemanticToken};

    /// Returns the search path for tree-sitter grammars.
    /// Uses TREE_SITTER_GRAMMARS env var if set (Nix), otherwise falls back to deps/tree-sitter.
    fn test_search_path() -> String {
        std::env::var("TREE_SITTER_GRAMMARS").unwrap_or_else(|_| "deps/tree-sitter".to_string())
    }

    /// Decode delta-encoded LSP semantic tokens to absolute `(line, col, length, token_type)`.
    fn decode_tokens(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32)> {
        let mut abs_line = 0u32;
        let mut abs_col = 0u32;
        tokens
            .iter()
            .map(|st| {
                abs_line += st.delta_line;
                if st.delta_line > 0 {
                    abs_col = st.delta_start;
                } else {
                    abs_col += st.delta_start;
                }
                (abs_line, abs_col, st.length, st.token_type)
            })
            .collect()
    }

    #[test]
    fn test_semantic_tokens_range() {
        use tower_lsp_server::ls_types::Position;

        // Create mock tokens for a document
        let all_tokens = SemanticTokens {
            result_id: None,
            data: vec![
                SemanticToken {
                    // Line 0, col 0-10
                    delta_line: 0,
                    delta_start: 0,
                    length: 10,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    // Line 2, col 0-3
                    delta_line: 2,
                    delta_start: 0,
                    length: 3,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    // Line 2, col 4-5
                    delta_line: 0,
                    delta_start: 4,
                    length: 1,
                    token_type: 17,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    // Line 4, col 2-8
                    delta_line: 2,
                    delta_start: 2,
                    length: 6,
                    token_type: 14,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        // Test range that includes only lines 1-3
        let _range = Range {
            start: Position {
                line: 1,
                character: 0,
            },
            end: Position {
                line: 3,
                character: 100,
            },
        };

        // Tokens in range should be the ones on line 2
        // We'd need actual tree-sitter setup to test the real function,
        // so this is more of a placeholder showing the expected structure
        assert_eq!(all_tokens.data.len(), 4);
    }

    #[test]
    fn test_diff_tokens_no_change() {
        let tokens = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![SemanticToken {
                delta_line: 0,
                delta_start: 0,
                length: 10,
                token_type: 0,
                token_modifiers_bitset: 0,
            }],
        };

        let delta = calculate_semantic_tokens_delta(&tokens, &tokens);
        assert!(delta.is_some());
        assert_eq!(delta.unwrap().edits.len(), 0);
    }

    /// Test that suffix matching reduces delta size when change is in the middle.
    ///
    /// Scenario: 5 tokens, only the 3rd token changes length
    /// Expected: Only 1 token in the edit (the changed one), not 3 tokens
    #[test]
    fn test_diff_tokens_suffix_matching() {
        // 5 tokens on the same line (delta_line=0 for all after first)
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // This one changes
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 4,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 10,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // Changed length
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 5,
                    token_type: 4,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 1);

        // With suffix matching: start=2 (skip 2 prefix tokens), delete_count=1, data=1 token
        // Without suffix matching: start=2, delete_count=3, data=3 tokens
        let edit = &delta.edits[0];
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(
            edit.start, 10,
            "Should skip 2 prefix tokens (2 * 5 integers)"
        );
        assert_eq!(
            edit.delete_count, 5,
            "Should only delete 1 token (with suffix matching) = 5 integers"
        );
        assert_eq!(
            edit.data.as_ref().unwrap().len(),
            1,
            "Should only include 1 changed token"
        );
    }

    /// When lines are inserted, unchanged encoded suffix tokens can remain in
    /// place because semantic-token edits address the flattened wire array.
    #[test]
    fn test_diff_tokens_line_insertion_retains_encoded_suffix() {
        // Before: 3 tokens on lines 0, 1, 2
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }, // line 0
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // line 1
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // line 2
            ],
        };

        // After: 4 tokens on lines 0, 1, 2, 3 (line inserted at position 1)
        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 5,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }, // line 0 (same)
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 5,
                    token_modifiers_bitset: 0,
                }, // line 1 (NEW)
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // line 2 (was line 1)
                SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 5,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                }, // line 3 (was line 2)
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 1);

        // The last two tokens have the same encoded fields, so the wire edit can
        // retain them even though their decoded absolute lines have shifted.
        let edit = &delta.edits[0];
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(
            edit.start, 5,
            "Should skip 1 prefix token (line 0) = 5 integers"
        );
        assert_eq!(
            edit.delete_count, 0,
            "Should retain both encoded suffix tokens"
        );
        assert_eq!(
            edit.data.as_ref().unwrap().len(),
            1,
            "Should include only the inserted token"
        );
    }

    /// Test that same-line edits preserve suffix optimization.
    ///
    /// When editing within a line (no line count change), suffix matching is safe.
    #[test]
    fn test_diff_tokens_same_line_edit_suffix() {
        // 4 tokens all on line 0
        let previous = SemanticTokens {
            result_id: Some("v1".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 5,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // This changes
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 3,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 4,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        // Second token changes length
        let current = SemanticTokens {
            result_id: Some("v2".to_string()),
            data: vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 3,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 8,
                    token_type: 1,
                    token_modifiers_bitset: 0,
                }, // Changed
                SemanticToken {
                    delta_line: 0,
                    delta_start: 6,
                    length: 3,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 4,
                    length: 4,
                    token_type: 3,
                    token_modifiers_bitset: 0,
                },
            ],
        };

        let delta = calculate_semantic_tokens_delta(&previous, &current);
        assert!(delta.is_some());

        let delta = delta.unwrap();
        assert_eq!(delta.edits.len(), 1);

        // Same line count, so suffix matching should work
        let edit = &delta.edits[0];
        // LSP spec: start and deleteCount are integer indices (each token = 5 integers)
        assert_eq!(edit.start, 5, "Should skip 1 prefix token = 5 integers");
        assert_eq!(
            edit.delete_count, 5,
            "Should only delete 1 token (suffix matched 2) = 5 integers"
        );
        assert_eq!(
            edit.data.as_ref().unwrap().len(),
            1,
            "Should only include 1 changed token"
        );
    }

    /// Test async wrapper for parallel injection processing.
    ///
    /// This verifies the bounded compute-pool bridge works correctly when
    /// calling Rayon-based injection processing from an async context.
    #[tokio::test]
    async fn test_handle_semantic_tokens_full() {
        use crate::config::WorkspaceSettings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        // Set up coordinator with search paths
        let coordinator = Arc::new(LanguageCoordinator::new());

        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load markdown and lua languages
        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua language parser not available");
            return;
        }

        let Some(query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Markdown with a Lua code block
        let text: Arc<str> = Arc::from(
            r#"# Hello

```lua
local x = 42
```
"#,
        );

        // Parse the markdown document
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: could not acquire markdown parser");
            return;
        };
        let Some(tree) = parser.parse(text.as_bytes(), None) else {
            eprintln!("Skipping: could not parse markdown");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Repeated computations over the same immutable inputs must be exact.
        // This pins the request-scoped behavior before the synchronous core is
        // extracted from the compute-pool wrapper.
        let first = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            Arc::clone(&text),
            tree.clone(),
            Arc::clone(&query),
            Some("markdown".to_string()),
            None,
            Arc::clone(&coordinator),
            false,
            None,
            None,
        )
        .await;
        let second = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            text,
            tree,
            query,
            Some("markdown".to_string()),
            None,
            coordinator,
            false,
            None,
            None,
        )
        .await;
        assert_eq!(
            first, second,
            "same immutable inputs must produce exact tokens"
        );

        // Should return tokens including injection tokens
        assert!(first.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = first.unwrap() else {
            panic!("Expected full tokens result");
        };

        // Should have tokens from the Lua injection
        // Look for a keyword token (the 'local' keyword in Lua)
        let has_keyword_token = tokens.data.iter().any(|t| t.token_type == 1); // keyword = 1
        assert!(
            has_keyword_token,
            "Should have keyword tokens from Lua injection. Got {} tokens",
            tokens.data.len()
        );
    }

    /// Test that an empty document returns a successful explicit empty token set.
    #[tokio::test]
    async fn test_handle_semantic_tokens_full_with_empty_document() {
        use crate::config::WorkspaceSettings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());

        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        if !md_result.success {
            eprintln!("Skipping: markdown language parser not available");
            return;
        }

        let Some(query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Empty document
        let text = "".to_string();

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Call the async handler with empty document
        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            query,
            Some("markdown".to_string()),
            None,
            coordinator,
            false,
            None,
            None,
        )
        .await;

        // A successful zero-token computation is distinct from cancellation or
        // producer failure, so clients receive an explicit empty token set.
        assert!(
            matches!(
                result,
                Some(SemanticTokensResult::Tokens(tokens)) if tokens.data.is_empty()
            ),
            "empty document should return an explicit empty token set"
        );
    }

    /// Integration test: Markdown with Lua code block — the finalize pipeline
    /// must exclude host tokens inside the injection region (line 3) while
    /// preserving tokens on other lines.
    #[tokio::test]
    async fn test_no_host_tokens_inside_injection_region() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
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

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Markdown with a Lua code block
        let text = "# Hello\n\n```lua\nlocal x = 42\n```\n".to_string();
        // Lines:
        //   0: "# Hello"
        //   1: ""
        //   2: "```lua"
        //   3: "local x = 42"
        //   4: "```"

        // Parse markdown
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Use default capture mappings so markdown captures like @markup.raw.block
        // are translated to `string` (token_type 2).
        let capture_mappings = std::sync::Arc::new(default_capture_mappings());

        // Use the full pipeline — exclusion now happens in finalize_tokens
        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
            None,
            None,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // Line 3 contains "local x = 42" — injection tokens should be present
        let line3_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 3).collect();
        assert!(
            !line3_tokens.is_empty(),
            "Should have injection tokens on line 3 (Lua). Decoded: {:?}",
            decoded
        );

        // --- Stronger assertions: no host token leaks on content line ---

        // `string` is token_type index 2 in LEGEND_TYPES (SemanticTokenType::STRING).
        // Markdown maps @markup.raw.block to `string`. Host tokens with this type
        // must NOT appear on line 3 (the content line inside the injection region).
        let string_token_type = 2u32;
        let line3_host_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 3 && t.3 == string_token_type)
            .collect();
        assert!(
            line3_host_leaks.is_empty(),
            "Host `string` tokens must not leak onto content line 3 (injection region). \
             Leaked tokens: {:?}. All decoded: {:?}",
            line3_host_leaks,
            decoded
        );

        // Fence lines (2 and 4) SHOULD still have host tokens.
        // Line 2 = "```lua", line 4 = "```" — these are outside the injection region.
        let line2_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 2).collect();
        assert!(
            !line2_tokens.is_empty(),
            "Fence line 2 ('```lua') should have host tokens. Decoded: {:?}",
            decoded
        );
        let line4_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 4).collect();
        assert!(
            !line4_tokens.is_empty(),
            "Fence line 4 ('```') should have host tokens. Decoded: {:?}",
            decoded
        );
    }

    /// Integration test: Sparse injection tokens with gaps must not leak host
    /// fragments. When injection tokens don't cover the full line (e.g., `x = 1`
    /// produces tokens at columns 0, 2, 4 but not at 1 or 3), the host token
    /// for the entire content line must be fully excluded — not split around
    /// the injection tokens by the sweep line.
    #[tokio::test]
    async fn test_no_host_leak_in_gaps_between_sparse_injection_tokens() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
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

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Lua code with sparse tokens: `x = 1` has just variable, operator, number
        // with space gaps at columns 1 and 3.
        let text = "```lua\nx = 1\n```\n".to_string();
        // Lines:
        //   0: "```lua"
        //   1: "x = 1"
        //   2: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Use default capture mappings so @markup.raw.block → `string`
        let capture_mappings = std::sync::Arc::new(default_capture_mappings());

        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
            None,
            None,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2, the host @markup.raw.block type
        let string_token_type = 2u32;

        // Content line 1: "x = 1" — must have NO host `string` fragments
        let line1_host_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 1 && t.3 == string_token_type)
            .collect();
        assert!(
            line1_host_leaks.is_empty(),
            "Host `string` tokens must not leak into gaps on content line 1. \
             Leaked: {:?}. All decoded: {:?}",
            line1_host_leaks,
            decoded
        );

        // Content line 1 should have injection tokens (Lua captures)
        let line1_injection: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        assert!(
            !line1_injection.is_empty(),
            "Should have Lua injection tokens on line 1. Decoded: {:?}",
            decoded
        );

        // Fence lines should still have host tokens
        let line0_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 0).collect();
        assert!(
            !line0_tokens.is_empty(),
            "Fence line 0 should have host tokens. Decoded: {:?}",
            decoded
        );
        let line2_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 2).collect();
        assert!(
            !line2_tokens.is_empty(),
            "Fence line 2 should have host tokens. Decoded: {:?}",
            decoded
        );
    }

    /// Integration test matching real-world scenario: Python code block with
    /// multiline content including blank lines. The `@markup.raw.block` host
    /// token (mapped to `string`) spans the entire fenced_code_block node.
    /// Stage 1 must exclude ALL per-line fragments on content lines.
    #[tokio::test]
    async fn test_no_host_leak_python_multiline_code_block() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let py_result = coordinator.ensure_language_loaded("python");
        if !md_result.success || !py_result.success {
            eprintln!("Skipping: markdown or python parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Multiline Python code block matching the screenshot scenario
        let text = "```python\nx: str = 1\n\ndef f(a: int) -> str:\n    return a\n\nf(x)\n```\n"
            .to_string();
        // Lines:
        //   0: "```python"
        //   1: "x: str = 1"
        //   2: ""
        //   3: "def f(a: int) -> str:"
        //   4: "    return a"
        //   5: ""
        //   6: "f(x)"
        //   7: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = std::sync::Arc::new(default_capture_mappings());
        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
            None,
            None,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2, the host @markup.raw.block type
        let string_token_type = 2u32;

        // Content lines 1-6 must have NO host `string` fragments
        for content_line in 1..=6 {
            let leaks: Vec<_> = decoded
                .iter()
                .filter(|t| t.0 == content_line && t.3 == string_token_type)
                .collect();
            assert!(
                leaks.is_empty(),
                "Host `string` tokens must not leak onto content line {}. \
                 Leaked: {:?}. All decoded: {:?}",
                content_line,
                leaks,
                decoded
            );
        }

        // Fence lines should have host tokens
        let line0_has_host = decoded.iter().any(|t| t.0 == 0);
        let line7_has_host = decoded.iter().any(|t| t.0 == 7);
        assert!(
            line0_has_host,
            "Fence line 0 should have tokens. Decoded: {:?}",
            decoded
        );
        assert!(
            line7_has_host,
            "Fence line 7 should have tokens. Decoded: {:?}",
            decoded
        );

        // Content lines should have injection tokens (Python captures)
        let line1_injection: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        assert!(
            !line1_injection.is_empty(),
            "Should have Python injection tokens on line 1. Decoded: {:?}",
            decoded
        );
    }

    /// Regression test: when `supports_multiline` is true, multiline host
    /// tokens (e.g., `@markup.raw.block` on `fenced_code_block`) are emitted
    /// as a single token starting at the fence line. Stage 1 exclusion must
    /// still suppress the host token on content lines — it cannot let a
    /// multiline token leak through because its start position is outside
    /// the injection region.
    #[tokio::test]
    async fn test_no_host_leak_with_multiline_support() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
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

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        let text = "```lua\nlocal x = 42\n```\n".to_string();
        // Lines:
        //   0: "```lua"
        //   1: "local x = 42"
        //   2: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = std::sync::Arc::new(default_capture_mappings());

        // KEY: supports_multiline = true
        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            true, // multiline support enabled!
            None,
            None,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2
        let string_token_type = 2u32;

        // With multiline support, a single `string` token might start on
        // line 0 (fence) but extend past the fence content into content lines.
        // Line 0 = "```lua" (6 UTF-16 chars). Any `string` token on line 0
        // with col + length > 6 is a multiline host token leaking into content.
        let line0_width = 6u32; // "```lua"
        let multiline_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.3 == string_token_type && t.0 == 0 && t.1 + t.2 > line0_width)
            .collect();
        assert!(
            multiline_leaks.is_empty(),
            "Multiline host `string` tokens must not extend past fence line into content. \
             Leaked: {:?}. All decoded: {:?}",
            multiline_leaks,
            decoded
        );

        // Content line 1 must have NO host `string` tokens starting there either
        let line1_host_leaks: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 1 && t.3 == string_token_type)
            .collect();
        assert!(
            line1_host_leaks.is_empty(),
            "Host `string` must not leak onto content line 1. \
             Leaked: {:?}. All decoded: {:?}",
            line1_host_leaks,
            decoded
        );

        // Should still have injection tokens on line 1
        let line1_injection: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        assert!(
            !line1_injection.is_empty(),
            "Should have injection tokens on line 1. Decoded: {:?}",
            decoded
        );
    }

    /// Integration test matching the exact screenshot: Python code block
    /// followed by a non-language code block. Tests that the two blocks
    /// don't interfere with each other's host token exclusion.
    #[tokio::test]
    async fn test_no_host_leak_python_plus_nolang_code_blocks() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let py_result = coordinator.ensure_language_loaded("python");
        if !md_result.success || !py_result.success {
            eprintln!("Skipping: markdown or python parser not available");
            return;
        }

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Document matching screenshot: Python block + no-language block
        let text = "\
```python
x: str = 1

def f(a: int) -> str:
    return a

f(x)
```

```
foo
```
"
        .to_string();
        // Lines:
        //   0: "```python"
        //   1: "x: str = 1"
        //   2: ""
        //   3: "def f(a: int) -> str:"
        //   4: "    return a"
        //   5: ""
        //   6: "f(x)"
        //   7: "```"
        //   8: ""
        //   9: "```"
        //  10: "foo"
        //  11: "```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = std::sync::Arc::new(default_capture_mappings());
        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
            None,
            None,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        // `string` = token_type 2
        let string_token_type = 2u32;

        // Python content lines 1-6: NO host `string` fragments
        for content_line in 1u32..=6 {
            let leaks: Vec<_> = decoded
                .iter()
                .filter(|t| t.0 == content_line && t.3 == string_token_type)
                .collect();
            assert!(
                leaks.is_empty(),
                "Host `string` tokens must not leak onto Python content line {}. \
                 Leaked: {:?}. All decoded: {:?}",
                content_line,
                leaks,
                decoded
            );
        }

        // No-language block content line 10: `string` SHOULD be present
        // (inactive injection region — no captures produced)
        let line10_string: Vec<_> = decoded
            .iter()
            .filter(|t| t.0 == 10 && t.3 == string_token_type)
            .collect();
        assert!(
            !line10_string.is_empty(),
            "Non-language block content line 10 should have `string` token. Decoded: {:?}",
            decoded
        );

        // Fence lines should have tokens
        assert!(
            decoded.iter().any(|t| t.0 == 0),
            "Python fence line 0 should have tokens"
        );
        assert!(
            decoded.iter().any(|t| t.0 == 7),
            "Python fence line 7 should have tokens"
        );
    }

    /// Integration test: Blockquote-wrapped fenced code blocks should produce
    /// consistent injection tokens across all content lines, and `> ` prefixes
    /// should not leak into the injection parser or suppress host tokens.
    #[tokio::test]
    async fn test_blockquote_injection_consistent_tokens() {
        use crate::config::WorkspaceSettings;
        use crate::config::defaults::default_capture_mappings;
        use crate::language::LanguageCoordinator;
        use std::sync::Arc;

        let coordinator = Arc::new(LanguageCoordinator::new());
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

        let Some(md_query) = coordinator.highlight_query("markdown") else {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        };

        // Blockquote with a Lua code block — two identical content lines
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n".to_string();
        // Lines:
        //   0: "> ```lua"
        //   1: "> local x = 1"
        //   2: "> local y = 2"
        //   3: "> ```"

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            return;
        };
        let Some(tree) = parser.parse(&text, None) else {
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let capture_mappings = std::sync::Arc::new(default_capture_mappings());

        let result = handle_semantic_tokens_full(
            &crate::compute_pool::test_pool(),
            std::sync::Arc::from(text),
            tree,
            md_query,
            Some("markdown".to_string()),
            Some(capture_mappings),
            coordinator,
            false,
            None,
            None,
        )
        .await;

        assert!(result.is_some(), "Should return semantic tokens");

        let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
            panic!("Expected full tokens result");
        };

        let decoded = decode_tokens(&tokens.data);

        let keyword_type = 1u32; // SemanticTokenType::KEYWORD
        let string_type = 2u32; // SemanticTokenType::STRING

        // Both content lines should have injection tokens
        let line1_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 1).collect();
        let line2_tokens: Vec<_> = decoded.iter().filter(|t| t.0 == 2).collect();

        assert!(
            !line1_tokens.is_empty(),
            "Line 1 should have injection tokens. All: {:?}",
            decoded
        );
        assert!(
            !line2_tokens.is_empty(),
            "Line 2 should have injection tokens. All: {:?}",
            decoded
        );

        // Both lines should have `keyword` token for `local` at the same column
        let line1_keywords: Vec<_> = line1_tokens
            .iter()
            .filter(|t| t.3 == keyword_type)
            .collect();
        let line2_keywords: Vec<_> = line2_tokens
            .iter()
            .filter(|t| t.3 == keyword_type)
            .collect();

        assert!(
            !line1_keywords.is_empty(),
            "Line 1 should have keyword token for 'local'. Line 1 tokens: {:?}",
            line1_tokens
        );
        assert!(
            !line2_keywords.is_empty(),
            "Line 2 should have keyword token for 'local'. Line 2 tokens: {:?}",
            line2_tokens
        );

        // Keyword column should be at absolute column 2 (after `> ` prefix)
        assert_eq!(
            line1_keywords[0].1, 2,
            "Keyword 'local' should start at column 2 (after `> `). L1: {:?}",
            line1_keywords
        );
        // Token columns should match between lines (both after "> ")
        assert_eq!(
            line1_keywords[0].1, line2_keywords[0].1,
            "Keyword column should be identical on both lines. L1: {:?}, L2: {:?}",
            line1_keywords, line2_keywords
        );

        // Token sequences (col, len, type) should be identical on both lines
        let line1_types: Vec<(u32, u32, u32)> =
            line1_tokens.iter().map(|t| (t.1, t.2, t.3)).collect();
        let line2_types: Vec<(u32, u32, u32)> =
            line2_tokens.iter().map(|t| (t.1, t.2, t.3)).collect();
        assert_eq!(
            line1_types, line2_types,
            "Both content lines should produce identical token sequences (col, len, type)"
        );

        // No host `string` tokens INSIDE the injection region (after col 2).
        // The `> ` prefix at col 0-2 IS a valid host token (outside injection).
        let line1_string_leaks: Vec<_> = line1_tokens
            .iter()
            .filter(|t| t.3 == string_type && t.1 >= 2)
            .collect();
        let line2_string_leaks: Vec<_> = line2_tokens
            .iter()
            .filter(|t| t.3 == string_type && t.1 >= 2)
            .collect();
        assert!(
            line1_string_leaks.is_empty(),
            "Host `string` tokens must not leak inside injection on line 1. Leaks: {:?}",
            line1_string_leaks
        );
        assert!(
            line2_string_leaks.is_empty(),
            "Host `string` tokens must not leak inside injection on line 2. Leaks: {:?}",
            line2_string_leaks
        );

        // With generic prefix detection, the `> ` prefix is stripped from the
        // host fenced_code_block token (it's structural markup, not code content).
        // The `> ` area has no semantic token since @punctuation.special is not
        // in LEGEND_TYPES.
        let line1_prefix: Vec<_> = line1_tokens
            .iter()
            .filter(|t| t.1 == 0 && t.3 == string_type)
            .collect();
        let line2_prefix: Vec<_> = line2_tokens
            .iter()
            .filter(|t| t.1 == 0 && t.3 == string_type)
            .collect();
        assert!(
            line1_prefix.is_empty(),
            "Line 1 should NOT have host `string` for `> ` prefix (stripped by prefix detection). Got: {:?}",
            line1_prefix
        );
        assert!(
            line2_prefix.is_empty(),
            "Line 2 should NOT have host `string` for `> ` prefix (stripped by prefix detection). Got: {:?}",
            line2_prefix
        );
    }
}
