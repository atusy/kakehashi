//! Semantic token methods for Kakehashi.
//!
//! # Cancel Handling
//!
//! This module supports immediate cancellation of semantic token requests:
//! - When `$/cancelRequest` is received, the handler aborts and returns `RequestCancelled` (-32800)
//! - Uses `tokio::select!` to race between cancel notification and token computation
//! - The blocking Rayon computation is cancelled *cooperatively*: the handler
//!   flips a [`CancelToken`](crate::cancel::CancelToken) (also flipped when a
//!   newer request supersedes this one, or the document closes) and the compute
//!   polls it at coarse checkpoints — after the host pass, inside the injection
//!   discovery loop, and at each per-region fan-out entry — bailing early. A region
//!   already mid-parse runs to completion, but not-yet-started work returns
//!   immediately, so an obsolete request stops burning CPU instead of computing
//!   a result that is only discarded.
//!
//! This is achieved by subscribing to cancel notifications via `CancelForwarder::subscribe()`
//! and using biased `tokio::select!` to prioritize cancel handling.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;
use tree_sitter::Parser;

use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::{
    SemanticTokens, SemanticTokensDeltaParams, SemanticTokensFullDeltaResult, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
};
use tree_sitter::Tree;
use url::Url;

#[cfg(test)]
use tower_lsp_server::ls_types::{
    PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams,
};

use crate::analysis::{
    calculate_delta_or_full, handle_semantic_tokens_full,
    handle_semantic_tokens_range_parallel_async, next_result_id,
};
use crate::lsp::current_upstream_id;

use super::super::{Kakehashi, uri_to_url};

/// Reason why a semantic token request was cancelled.
#[derive(Debug, Clone, Copy)]
enum CancellationReason {
    StaleText,
    DocumentMissing,
}

impl Kakehashi {
    /// Check if the document text matches the expected text, returning the cancellation reason if not.
    fn check_text_staleness(&self, uri: &Url, expected_text: &str) -> Option<CancellationReason> {
        match self.documents.get(uri) {
            Some(doc) if doc.text() == expected_text => None,
            Some(_) => Some(CancellationReason::StaleText),
            None => Some(CancellationReason::DocumentMissing),
        }
    }

    /// Get the syntax tree for a document, waiting for parse completion or parsing on-demand.
    ///
    /// This handles the race condition where semantic tokens are requested before
    /// `didOpen`/`didChange` finishes parsing. Strategy:
    /// 1. Wait up to 200ms for any in-flight parse to complete
    /// 2. Try to use the tree from the document store (preferred for incremental tokenization)
    /// 3. Parse on-demand as fallback if tree is missing or stale
    ///
    /// Returns `(tree, text)` tuple where tree was verified to be parsed from text,
    /// or `None` if the document is missing or parsing failed.
    async fn get_tree_with_wait(&self, uri: &Url, language_name: &str) -> Option<(Tree, String)> {
        // If the document isn't open there is nothing to settle or snapshot.
        // Return before taking the edit lock so we don't create a lock entry for
        // a never-opened/closed URI (language detection can resolve a language
        // from the path alone, so this point is reachable without a document).
        // The handle is dropped immediately; we only need the existence check.
        self.documents.get(uri)?;

        // Settle in-flight edits before snapshotting. A large paste arrives as
        // several back-to-back `didChange` chunks; the editor then sends one
        // semantic-tokens request for the final state. Each `didChange` holds
        // the document's edit lock across its reparse, so acquiring the same
        // lock here waits for any edit currently applying/parsing to finish
        // before snapshotting. Without it the request can snapshot a tree from a
        // half-applied paste and return tokens for only the first chunks — the
        // later lines render unhighlighted (white). Acquisition follows first-poll
        // order (a practical mitigation, not a hard JSON-RPC wire-order
        // guarantee — see https://github.com/atusy/kakehashi/issues/342), so a
        // request polled before a still-pending edit may not wait for it; the
        // common debounced-after-paste case settles correctly. The guard is held
        // across the tree read (and the on-demand parse fallback) so no edit can
        // interleave between settling and snapshotting; it is released when this
        // function returns, before token computation, so edits never wait on the
        // (slower) tokenization.
        let edit_lock = self.documents.edit_lock(uri);
        let _settle_guard = edit_lock.lock().await;

        // Re-check existence now that we hold the lock: the document could have
        // been closed between the pre-check and here (e.g. `didClose` took the
        // edit lock first, removed the document — and its lock entry — then
        // released). In that case `edit_lock()` above re-created a fresh entry,
        // so drop it and bail rather than leaving an orphan behind for a gone
        // document.
        if self.documents.get(uri).is_none() {
            drop(_settle_guard);
            self.documents.remove_edit_lock(uri);
            return None;
        }

        // Settle the in-flight parse before snapshotting, under a SINGLE 200ms
        // budget shared across both waits so the `edit_lock` held here is never
        // pinned longer than the pre-existing has-tree budget (the two waits do
        // not stack to ~400ms).
        //
        // First wait on the parse **watermark** for this reader's tail edit
        // (per-document-parse-scheduler ADR). In the current inline-parse world this
        // returns effectively immediately — the parse runs within the writer's
        // gated critical section, so by the time the gate releases this reader the
        // watermark already covers the tail ticket — leaving the full budget for
        // the has-tree wait below (so today this adds no measurable latency). Once
        // the per-document parse scheduler runs the parse off the ingress ticket this
        // genuinely waits — that is the point: a bare `has_tree` check would
        // instead pass while the store still holds the *old* tree (the #342/#374
        // stale-tree race), and this watermark wait is what closes it.
        let settle_budget = Duration::from_millis(200);
        let settle_start = std::time::Instant::now();
        if let Some(tail) = crate::lsp::current_reader_tail() {
            self.documents
                .wait_for_epoch(uri, tail, settle_budget)
                .await;
        }

        // Wait for any in-flight parse to complete with whatever budget remains.
        let remaining = settle_budget.saturating_sub(settle_start.elapsed());
        self.documents
            .wait_for_parse_completion(uri, remaining)
            .await;

        // First, try to use the tree already in the document store, to avoid a
        // redundant parse here: the store's tree is kept current by didOpen's parse
        // and the off-ingress edit reparse.
        if let Some(doc) = self.documents.get(uri) {
            let text = doc.text().to_string();
            if let Some(tree) = doc.tree().cloned() {
                log::debug!(
                    target: "kakehashi::semantic",
                    "Using existing tree from document store for {}",
                    uri.path()
                );
                return Some((tree, text));
            }
        }

        // Fallback: parse on-demand if no tree is available.
        // This handles race conditions where semantic tokens are requested before
        // didOpen/didChange finishes parsing.
        log::debug!(
            target: "kakehashi::semantic",
            "Parsing on-demand for {} (no tree in store)",
            uri.path()
        );
        self.try_parse_and_update_document(uri, language_name).await
    }

    /// Parse the document on-demand and update the store if successful.
    ///
    /// This is a fallback path when the normal parse pipeline hasn't completed.
    /// Side effects:
    /// - Updates the document store with the parsed tree (if text unchanged)
    /// - Clears any failed parser state for recovery
    ///
    /// Returns `(tree, text)` tuple where `text` is the exact text the tree was
    /// parsed from (and verified unchanged). This prevents race conditions where
    /// the document changes after parsing but before the caller captures text.
    async fn try_parse_and_update_document(
        &self,
        uri: &Url,
        language_name: &str,
    ) -> Option<(Tree, String)> {
        let doc = self.documents.get(uri)?;
        let text = doc.text().to_string();
        drop(doc);

        let text_clone = text.clone();

        // Parse with panic protection via catch_unwind, delegating pool
        // management to the shared parse_with_pool helper
        let parse_result: Option<Tree> = self
            .parse_coordinator()
            .parse_with_pool(language_name, uri, text.len(), move |mut parser: Parser| {
                let result = catch_unwind(AssertUnwindSafe(|| parser.parse(&text_clone, None)))
                    .ok()
                    .flatten();
                (parser, result)
            })
            .await;

        if let Some(tree) = parse_result {
            let mut doc_is_current = false;
            let mut should_update = false;
            if let Some(current_doc) = self.documents.get(uri)
                && current_doc.text() == text
            {
                doc_is_current = true;
                should_update = current_doc.tree().is_none();
            }

            if should_update {
                self.documents
                    .update_document(uri.clone(), text.clone(), Some(tree.clone()));
            }

            if doc_is_current {
                if self.auto_install.is_parser_failed(language_name)
                    && let Err(error) = self.auto_install.clear_failed(language_name)
                {
                    log::warn!(
                        target: "kakehashi::crash_recovery",
                        "Failed to clear failed parser state for '{}': {}",
                        language_name,
                        error
                    );
                }
                // Return both tree and the validated text to prevent TOCTOU race
                return Some((tree, text));
            }
        }

        None
    }

    pub(crate) async fn semantic_tokens_full_impl(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let upstream_id = current_upstream_id();
        let (cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in semanticTokens/full: {}", lsp_uri.as_str());
            return Ok(None);
        };

        // Start tracking this request - supersedes any previous request for this URI.
        // `cancel_token` is flipped when a newer request supersedes this one (or
        // the document closes); it is threaded into the blocking compute so a
        // superseded request stops mid-flight instead of running to completion.
        let (request_id, cancel_token) = self.cache.start_request(&uri);

        // Snapshot the settings generation NOW, before reading any
        // settings-dependent tokenization input (language resolution, queries,
        // capture mappings) below. Folded into the cache key once the text is
        // available; pinning it here means a settings reload racing this request
        // leaves our stored tokens on the old generation — invisible to
        // post-reload requests — so we can't poison the cache (see `cache_key_for`).
        let token_generation = self.cache.semantic_token_generation();

        log::debug!(
            target: "kakehashi::semantic",
            "[SEMANTIC_TOKENS] START uri={} req={}",
            uri, request_id
        );

        // Early exit if request was superseded
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={}",
                uri, request_id
            );
            return Ok(None);
        }

        let Some(language_name) = self.document_language(&uri) else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        // Ensure language is loaded before trying to get queries.
        // This handles the race condition where semanticTokens/full arrives
        // before didOpen finishes loading the language.
        let load_result = self.language.ensure_language_loaded(&language_name);
        if !load_result.success {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        }

        // Early exit check after loading language
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (after language load)",
                uri, request_id
            );
            return Ok(None);
        }

        let Some(query) = self.language.highlight_query(&language_name) else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        // Read the remaining settings-dependent tokenization inputs HERE — together
        // with the query above and BEFORE the get_tree_with_wait().await below — so
        // a settings reload during that await can't split them into an inconsistent
        // mix (e.g. old query + new capture mappings). All are consistent with the
        // `token_generation` snapshotted at the top.
        let capture_mappings = self.language.capture_mappings();
        let supports_multiline = self.settings_manager.supports_multiline_tokens();

        // Early exit check before expensive computation
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (before compute)",
                uri, request_id
            );
            return Ok(None);
        }

        // Get tree and text, waiting for parse completion or parsing on-demand
        let Some((tree, text)) = self.get_tree_with_wait(&uri, &language_name).await else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        // Validity key for the snapshotted text under the generation captured at
        // the top (before any query/capture-mapping read): keys both the
        // unchanged-document cache short-circuit below and the store of the freshly
        // computed tokens. Pinning to the early generation is what keeps a
        // concurrent settings reload from making this request poison the cache.
        let cache_key = self.cache.cache_key_for(&text, token_generation);

        // Get document data and compute tokens
        let (result, text_used) = {
            if let Some(reason) = self.check_text_staleness(&uri, &text) {
                self.cache.finish_request(&uri, request_id);
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS] CANCELLED uri={} req={} ({:?})",
                    uri, request_id, reason
                );
                return Ok(None);
            }

            // Early exit check after waiting for parse completion
            if !self.cache.is_request_active(&uri, request_id) {
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS] CANCELLED uri={} req={}",
                    uri, request_id
                );
                return Ok(None);
            }

            // Unchanged document: tokens already cached for this exact text are
            // still correct, so skip re-tokenizing (the expensive work). The
            // `result_id` can't signal "unchanged" — it's a fresh global counter
            // per response — but the content hash can. Returns the cached tokens
            // with their original `result_id`, keeping a client's delta baseline
            // stable across idle re-requests. Dropped wholesale on settings reload.
            //
            // No `.await` runs between the staleness check above and this serve, so
            // no `didChange` can interleave — the cached tokens stay consistent with
            // that check. (The compute path below DOES await, which is why it
            // re-checks staleness after the block; this early return needs no such
            // re-check.)
            if let Some(cached) = self
                .cache
                .get_current_tokens(&uri, &language_name, cache_key)
            {
                self.cache.finish_request(&uri, request_id);
                return Ok(Some(SemanticTokensResult::Tokens(cached)));
            }

            // capture_mappings and supports_multiline were read before the await
            // above (consistent with the query and token_generation). Rayon-based
            // parallel injection processing uses thread-local parser caching
            // instead of the shared parser pool, avoiding lock contention.
            let coordinator = std::sync::Arc::clone(&self.language);

            // Enable per-region injection-token reuse (#529). The generation is
            // the one snapshotted at the top of the handler (same value folded
            // into `cache_key`), so a config reload racing this request can't make
            // it serve or store stale-query tokens.
            let injection_cache = Some(crate::analysis::semantic::InjectionCacheParams {
                uri: uri.clone(),
                tracker: self.bridge.node_tracker_arc(),
                cache: self.cache.injection_token_cache_arc(),
                generation: token_generation,
            });

            // Compute tokens, racing against cancel notification if provided
            let compute_future = handle_semantic_tokens_full(
                std::sync::Arc::clone(&self.compute_pool),
                text.clone(),
                tree.clone(),
                query,
                Some(language_name.clone()),
                Some(capture_mappings),
                coordinator,
                supports_multiline,
                injection_cache,
                Some(cancel_token.clone()),
            );

            let result = if let Some(cancel_rx) = cancel_rx {
                // Race between computation and cancel notification
                tokio::pin!(cancel_rx);
                tokio::select! {
                    biased;

                    // Cancel notification received - abort immediately. Flip the
                    // token so the now-detached blocking compute stops early
                    // instead of running to completion for a discarded result.
                    _ = &mut cancel_rx => {
                        cancel_token.cancel();
                        self.cache.finish_request(&uri, request_id);
                        log::debug!(
                            target: "kakehashi::semantic",
                            "[SEMANTIC_TOKENS] CANCELLED via $/cancelRequest uri={} req={}",
                            uri, request_id
                        );
                        return Err(Error::request_cancelled());
                    }

                    // Computation completed
                    result = compute_future => result,
                }
            } else {
                // No cancel support - just await the computation
                compute_future.await
            };

            (result, text)
        }; // doc reference is dropped here

        // A supersede/close between compute start and here flips the token; the
        // compute then bailed at a checkpoint and returned `None`, so drop the
        // request rather than storing an unwanted result over the cache.
        if cancel_token.is_cancelled() {
            self.cache.finish_request(&uri, request_id);
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (compute superseded)",
                uri, request_id
            );
            return Ok(None);
        }

        if let Some(reason) = self.check_text_staleness(&uri, &text_used) {
            self.cache.finish_request(&uri, request_id);
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} ({:?})",
                uri, request_id, reason
            );
            return Ok(None);
        }

        // Early exit check before storing - prevents superseded request from overwriting cache
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (before store)",
                uri, request_id
            );
            return Ok(None);
        }

        let mut tokens_with_id = match result.unwrap_or_else(|| {
            tower_lsp_server::ls_types::SemanticTokensResult::Tokens(
                tower_lsp_server::ls_types::SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                },
            )
        }) {
            tower_lsp_server::ls_types::SemanticTokensResult::Tokens(tokens) => tokens,
            tower_lsp_server::ls_types::SemanticTokensResult::Partial(_) => {
                tower_lsp_server::ls_types::SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                }
            }
        };
        // Use atomic sequential ID for efficient cache validation
        tokens_with_id.result_id = Some(next_result_id());
        let stored_tokens = tokens_with_id.clone();
        let lsp_tokens = tokens_with_id;
        // Store keyed by result_id (delta baseline) AND cache_key (so an
        // unchanged-document repeat request short-circuits the re-tokenization
        // above). `language_name` is unused after this, so move it in.
        self.cache
            .store_tokens(uri.clone(), stored_tokens, language_name, cache_key);

        // Finish tracking this request
        self.cache.finish_request(&uri, request_id);

        log::debug!(
            target: "kakehashi::semantic",
            "[SEMANTIC_TOKENS] DONE uri={} req={} tokens={}",
            uri, request_id, lsp_tokens.data.len()
        );

        Ok(Some(SemanticTokensResult::Tokens(lsp_tokens)))
    }

    pub(crate) async fn semantic_tokens_full_delta_impl(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let upstream_id = current_upstream_id();
        let (cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let lsp_uri = params.text_document.uri;
        let previous_result_id = params.previous_result_id;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(
                "Invalid URI in semanticTokens/full/delta: {}",
                lsp_uri.as_str()
            );
            return Ok(None);
        };

        // Start tracking this request - supersedes any previous request for this
        // URI. `cancel_token` (flipped on supersede/close) is threaded into the
        // blocking compute so a superseded delta stops mid-flight — this is the
        // steady-state typing path where the pile-up is worst.
        let (request_id, cancel_token) = self.cache.start_request(&uri);

        // Snapshot the settings generation NOW, before any settings-dependent
        // tokenization input is read below (same reload-race safety as
        // semanticTokens/full; folded into the cache key once the text is known).
        let token_generation = self.cache.semantic_token_generation();

        log::debug!(
            target: "kakehashi::semantic",
            "[SEMANTIC_TOKENS_DELTA] START uri={} req={}",
            uri, request_id
        );

        // Early exit if request was superseded
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={}",
                uri, request_id
            );
            return Ok(None);
        }

        let Some(language_name) = self.document_language(&uri) else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
            )));
        };

        // Ensure language is loaded before trying to get queries.
        // This handles the race condition where semanticTokens/full/delta arrives
        // before didOpen finishes loading the language.
        let load_result = self.language.ensure_language_loaded(&language_name);
        if !load_result.success {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
            )));
        }

        // Early exit check after loading language
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} (after language load)",
                uri, request_id
            );
            return Ok(None);
        }

        let Some(query) = self.language.highlight_query(&language_name) else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
            )));
        };

        // Read the remaining settings-dependent tokenization inputs HERE — with the
        // query above and BEFORE the get_tree_with_wait().await below — so a settings
        // reload during that await can't split them into an inconsistent mix
        // (same as semanticTokens/full; all consistent with `token_generation`).
        let capture_mappings = self.language.capture_mappings();
        let supports_multiline = self.settings_manager.supports_multiline_tokens();

        // Early exit check before expensive computation
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} (before compute)",
                uri, request_id
            );
            return Ok(None);
        }

        // Get tree and text, waiting for parse completion or parsing on-demand
        let Some((tree, text)) = self.get_tree_with_wait(&uri, &language_name).await else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
            )));
        };

        // Validity key for the snapshotted text under the generation captured at
        // the top (before the tokenization inputs are read, as in
        // semanticTokens/full): keys the unchanged-document reuse below and the
        // store of freshly computed tokens.
        let cache_key = self.cache.cache_key_for(&text, token_generation);

        // Get document data and compute tokens (same as semanticTokens/full)
        let (result, text_used) = {
            if let Some(reason) = self.check_text_staleness(&uri, &text) {
                self.cache.finish_request(&uri, request_id);
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} ({:?})",
                    uri, request_id, reason
                );
                return Ok(None);
            }

            // Early exit check after waiting for parse completion
            if !self.cache.is_request_active(&uri, request_id) {
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={}",
                    uri, request_id
                );
                return Ok(None);
            }

            // Unchanged document: reuse the cached full tokens instead of
            // re-tokenizing. No `.await` runs between the staleness check above and
            // here, so the cached tokens stay consistent with it. Cleared wholesale
            // on a settings reload.
            if let Some(cached) = self
                .cache
                .get_current_tokens(&uri, &language_name, cache_key)
            {
                // Fast path: the client's baseline already IS these cached tokens,
                // so the delta is necessarily empty — return it directly and skip
                // the `previous_tokens` clone + O(N) `calculate_delta` below.
                if cached.result_id.as_deref() == Some(previous_result_id.as_str()) {
                    self.cache.finish_request(&uri, request_id);
                    return Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
                        tower_lsp_server::ls_types::SemanticTokensDelta {
                            result_id: Some(previous_result_id),
                            edits: vec![],
                        },
                    )));
                }
                // Baseline differs: fall through to diff the cached tokens against
                // the client's `previous_result_id` (still skips re-tokenization).
                (Some(SemanticTokensResult::Tokens(cached)), text)
            } else {
                // capture_mappings and supports_multiline were read before the await
                // above (consistent with the query and token_generation). Rayon-based
                // parallel injection processing (SAME as semanticTokens/full).
                let coordinator = std::sync::Arc::clone(&self.language);

                // Enable per-region injection-token reuse (#529) on the delta
                // path too — this is the steady-state typing path the cache
                // targets. Generation pinned to the top-of-handler snapshot.
                let injection_cache = Some(crate::analysis::semantic::InjectionCacheParams {
                    uri: uri.clone(),
                    tracker: self.bridge.node_tracker_arc(),
                    cache: self.cache.injection_token_cache_arc(),
                    generation: token_generation,
                });

                // Compute tokens, racing against cancel notification if provided
                let compute_future = handle_semantic_tokens_full(
                    std::sync::Arc::clone(&self.compute_pool),
                    text.clone(),
                    tree.clone(),
                    query,
                    Some(language_name.clone()),
                    Some(capture_mappings),
                    coordinator,
                    supports_multiline,
                    injection_cache,
                    Some(cancel_token.clone()),
                );

                let result = if let Some(cancel_rx) = cancel_rx {
                    // Race between computation and cancel notification
                    tokio::pin!(cancel_rx);
                    tokio::select! {
                        biased;

                        // Cancel notification received - abort immediately. Flip
                        // the token so the now-detached blocking compute stops
                        // early instead of running to completion for a discarded
                        // result.
                        _ = &mut cancel_rx => {
                            cancel_token.cancel();
                            self.cache.finish_request(&uri, request_id);
                            log::debug!(
                                target: "kakehashi::semantic",
                                "[SEMANTIC_TOKENS_DELTA] CANCELLED via $/cancelRequest uri={} req={}",
                                uri, request_id
                            );
                            return Err(Error::request_cancelled());
                        }

                        // Computation completed
                        result = compute_future => result,
                    }
                } else {
                    // No cancel support - just await the computation
                    compute_future.await
                };

                (result, text)
            }
        };

        // A supersede/close between compute start and here flips the token; the
        // compute then bailed at a checkpoint and returned `None`, so drop the
        // request rather than diffing/storing an unwanted result over the cache.
        if cancel_token.is_cancelled() {
            self.cache.finish_request(&uri, request_id);
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} (compute superseded)",
                uri, request_id
            );
            return Ok(None);
        }

        if let Some(reason) = self.check_text_staleness(&uri, &text_used) {
            self.cache.finish_request(&uri, request_id);
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} ({:?})",
                uri, request_id, reason
            );
            return Ok(None);
        }

        // Extract current tokens from the result
        let current_tokens = match result.unwrap_or_else(|| {
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: Vec::new(),
            })
        }) {
            SemanticTokensResult::Tokens(tokens) => tokens,
            SemanticTokensResult::Partial(_) => SemanticTokens {
                result_id: None,
                data: Vec::new(),
            },
        };

        // Early exit check before storing - prevents superseded request from overwriting cache
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} (before store)",
                uri, request_id
            );
            return Ok(None);
        }

        // Get previous tokens from cache for delta calculation
        let previous_tokens = self.cache.get_tokens_if_valid(&uri, &previous_result_id);

        // Calculate delta or return full tokens
        let delta_result = match previous_tokens {
            Some(prev) => calculate_delta_or_full(&prev, &current_tokens, &previous_result_id),
            None => SemanticTokensFullDeltaResult::Tokens(current_tokens.clone()),
        };

        // Assign new result_id and store in cache
        let final_result = match delta_result {
            SemanticTokensFullDeltaResult::Tokens(mut tokens) => {
                tokens.result_id = Some(next_result_id());
                self.cache.store_tokens(
                    uri.clone(),
                    tokens.clone(),
                    language_name.clone(),
                    cache_key,
                );
                SemanticTokensFullDeltaResult::Tokens(tokens)
            }
            SemanticTokensFullDeltaResult::TokensDelta(mut delta) if delta.edits.is_empty() => {
                // No-op delta: recomputed tokens are byte-identical to the cached
                // tokens, which already carry `previous_result_id`. Reuse that id
                // and skip re-storing — the version token shouldn't advance when
                // nothing changed, and the cache entry stays valid for the next
                // request. Saves a clone + cache store + id rotation.
                delta.result_id = Some(previous_result_id.clone());
                SemanticTokensFullDeltaResult::TokensDelta(delta)
            }
            SemanticTokensFullDeltaResult::TokensDelta(mut delta) => {
                // For delta, we still need to store the current tokens with new result_id
                let mut stored_tokens = current_tokens;
                stored_tokens.result_id = Some(next_result_id());
                delta.result_id = stored_tokens.result_id.clone();
                self.cache.store_tokens(
                    uri.clone(),
                    stored_tokens,
                    language_name.clone(),
                    cache_key,
                );
                SemanticTokensFullDeltaResult::TokensDelta(delta)
            }
            SemanticTokensFullDeltaResult::PartialTokensDelta { .. } => {
                // PartialTokensDelta is not produced by our delta calculation logic,
                // but we handle it explicitly to maintain exhaustive matching.
                // Fall back to full tokens response with proper result_id and cache update.
                log::warn!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS_DELTA] Unexpected PartialTokensDelta variant for uri={}",
                    uri
                );
                let mut tokens = current_tokens;
                tokens.result_id = Some(next_result_id());
                self.cache.store_tokens(
                    uri.clone(),
                    tokens.clone(),
                    language_name.clone(),
                    cache_key,
                );
                SemanticTokensFullDeltaResult::Tokens(tokens)
            }
        };

        // Finish tracking this request
        self.cache.finish_request(&uri, request_id);

        log::debug!(
            target: "kakehashi::semantic",
            "[SEMANTIC_TOKENS_DELTA] DONE uri={} req={}",
            uri, request_id
        );

        Ok(Some(final_result))
    }

    pub(crate) async fn semantic_tokens_range_impl(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in semanticTokens/range: {}", lsp_uri.as_str());
            return Ok(None);
        };

        let domain_range = range;

        // Snapshot the settings generation at the top, before any await (#535): a
        // reload that bumps the generation after this leaves this request's stored
        // key on the old generation — invisible to post-reload requests (which
        // compute the new-generation key) — so a stale entry can never be served.
        let generation = self.cache.semantic_token_generation();

        let Some(language_name) = self.document_language(&uri) else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        let Some(query) = self.language.highlight_query(&language_name) else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        // Ensure a fresh tree before snapshotting: `didChange` clears the tree and
        // reparses off-ingress, so without this the visible-range tokens go blank
        // for the reparse window after every edit. (The full/delta paths get this
        // via `get_tree_with_wait`; the range path reads the store directly.)
        self.ensure_document_parsed(&uri).await;

        let Some(doc) = self.documents.get(&uri) else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        let text = doc.text();
        let Some(tree) = doc.tree() else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };

        // Short-circuit an identical-viewport re-request of an unchanged document
        // (#535). `cache_key` folds the document text with the settings generation,
        // and the entry also pins the viewport `range`, so a hit means re-tokenizing
        // would reproduce these exact tokens. Misses (scroll, edit, or reload) fall
        // through to the recompute below, which restores the entry.
        let cache_key = self.cache.cache_key_for(text, generation);
        if let Some(tokens) =
            self.cache
                .get_current_range_tokens(&uri, &domain_range, &language_name, cache_key)
        {
            return Ok(Some(SemanticTokensRangeResult::Tokens(tokens)));
        }

        // Get capture mappings for token type resolution
        let capture_mappings = self.language.capture_mappings();

        // Use Rayon-based parallel injection processing
        let supports_multiline = self.settings_manager.supports_multiline_tokens();
        let coordinator = std::sync::Arc::clone(&self.language);

        let result = handle_semantic_tokens_range_parallel_async(
            std::sync::Arc::clone(&self.compute_pool),
            text.to_string(),
            tree.clone(),
            query,
            domain_range,
            Some(language_name.clone()),
            Some(capture_mappings),
            coordinator,
            supports_multiline,
        )
        .await;

        // Convert to RangeResult. Cache ONLY a clean `Tokens` result (#535); a
        // `Partial` is passed through to the client as-is but NOT cached (it is a
        // degraded response), and a `None` becomes an empty `Tokens` and is not
        // cached either (transient miss/cancel) — caching either could serve a
        // degraded set on a later identical-viewport re-request.
        let domain_range_result = match result {
            Some(tower_lsp_server::ls_types::SemanticTokensResult::Tokens(tokens)) => {
                // `uri` and `language_name` are unused after this arm, so move them
                // (no clone); `tokens` is cloned because the store and the response
                // below each need an owned copy.
                self.cache.store_range_tokens(
                    uri,
                    domain_range,
                    language_name,
                    tokens.clone(),
                    cache_key,
                );
                tower_lsp_server::ls_types::SemanticTokensRangeResult::from(tokens)
            }
            Some(tower_lsp_server::ls_types::SemanticTokensResult::Partial(partial)) => {
                tower_lsp_server::ls_types::SemanticTokensRangeResult::from(partial)
            }
            None => tower_lsp_server::ls_types::SemanticTokensRangeResult::Tokens(
                tower_lsp_server::ls_types::SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                },
            ),
        };

        Ok(Some(domain_range_result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep, timeout};
    use tower_lsp_server::LspService;
    use url::Url;

    #[tokio::test]
    async fn semantic_tokens_delta_does_not_overwrite_newer_text() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_delta_race.lua").expect("should construct test uri");

        let mut initial_text = String::from("local M = {}\n");
        for _ in 0..2000 {
            initial_text.push_str("local x = 1\n");
        }
        initial_text.push_str("return M\n");

        server
            .documents
            .insert(uri.clone(), initial_text, Some("lua".to_string()), None);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        let new_text = "local LONG_NAME = {}\nreturn LONG_NAME\n".to_string();
        let new_text_clone = new_text.clone();

        let update_future = async {
            sleep(Duration::from_millis(10)).await;
            server
                .documents
                .insert(uri.clone(), new_text_clone, Some("lua".to_string()), None);
        };

        let params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: "0".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let (result, _) = tokio::join!(
            server.semantic_tokens_full_delta_impl(params),
            update_future
        );

        assert!(
            result.is_ok(),
            "semantic tokens delta request should complete without error"
        );

        let doc = server
            .documents
            .get(&uri)
            .expect("document should still exist after delta request");

        assert_eq!(
            doc.text(),
            new_text,
            "delta path should not overwrite newer document text"
        );
    }

    #[tokio::test]
    async fn semantic_tokens_full_times_out_but_parses_on_demand() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_timeout.rs").expect("should construct test uri");

        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("rust");
        if !load_result.success || server.language.highlight_query("rust").is_none() {
            eprintln!("Skipping: rust highlight query not available");
            return;
        }

        server.documents.mark_parse_started(&uri);

        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let result = timeout(
            Duration::from_secs(2),
            server.semantic_tokens_full_impl(params),
        )
        .await;

        assert!(
            result.is_ok(),
            "semantic tokens full should complete after waiting timeout"
        );
        let result = result.unwrap();
        assert!(
            result.is_ok(),
            "semantic tokens full should return without error"
        );

        let doc = server
            .documents
            .get(&uri)
            .expect("document should exist after on-demand parse");
        assert!(
            doc.tree().is_some(),
            "on-demand parse should populate a syntax tree"
        );
    }

    /// Test that delta response has result_id and cache is updated correctly.
    ///
    /// This verifies that when returning TokensDelta:
    /// 1. The delta response contains a non-None result_id
    /// 2. The cache is updated with full tokens (not just delta)
    /// 3. The cache entry has the same result_id as the delta response
    /// 4. Subsequent delta requests can use this new result_id
    #[tokio::test]
    async fn semantic_tokens_delta_response_has_result_id_and_updates_cache() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///delta_result_id.lua").expect("should construct test uri");

        // Insert initial document
        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // First request: semanticTokens/full to get initial result_id
        let full_params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let full_result = server
            .semantic_tokens_full_impl(full_params)
            .await
            .expect("full request should succeed")
            .expect("should return tokens");

        let initial_result_id = match full_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Update document to trigger delta calculation
        server.documents.update_document(
            uri.clone(),
            "local y = 2".to_string(),
            None, // tree will be None until next parse
        );

        // Second request: semanticTokens/full/delta with previous_result_id
        let delta_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: initial_result_id.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let delta_result = server
            .semantic_tokens_full_delta_impl(delta_params)
            .await
            .expect("delta request should succeed")
            .expect("should return delta or tokens");

        // ASSERTION 1: Response has non-None result_id
        let delta_result_id = match &delta_result {
            SemanticTokensFullDeltaResult::TokensDelta(d) => {
                d.result_id.clone().expect("delta should have result_id")
            }
            SemanticTokensFullDeltaResult::Tokens(t) => {
                t.result_id.clone().expect("tokens should have result_id")
            }
            _ => panic!("unexpected variant"),
        };

        // ASSERTION 2: result_id is different from initial
        assert_ne!(
            delta_result_id, initial_result_id,
            "new result_id should be assigned"
        );

        // ASSERTION 3: Cache is updated with the new result_id
        let cached = server.cache.get_tokens_if_valid(&uri, &delta_result_id);
        assert!(
            cached.is_some(),
            "cache should contain tokens with new result_id '{}'",
            delta_result_id
        );

        // ASSERTION 4: Subsequent delta request works with new result_id
        let follow_up_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: delta_result_id,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let follow_up_result = server
            .semantic_tokens_full_delta_impl(follow_up_params)
            .await;
        assert!(
            follow_up_result.is_ok(),
            "follow-up delta request should succeed"
        );
    }

    /// Test that a no-op delta (no document change) reuses the previous
    /// result_id instead of rotating it and re-storing identical tokens.
    ///
    /// When the document is unchanged, recomputed tokens are byte-identical to
    /// the cached tokens, so the delta has zero edits. The LSP result_id is a
    /// version token the client echoes back; keeping it stable avoids a wasted
    /// clone + cache store + id increment, and the cache entry under the
    /// previous id stays valid for the next request.
    #[tokio::test]
    async fn semantic_tokens_noop_delta_reuses_previous_result_id() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///noop_delta.lua").expect("should construct test uri");

        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        // Configure the grammar search path so the language actually loads
        // (grammars live under deps/tree-sitter, or TREE_SITTER_GRAMMARS in Nix).
        let settings = crate::config::WorkspaceSettings {
            search_paths: vec![
                std::env::var("TREE_SITTER_GRAMMARS")
                    .unwrap_or_else(|_| "deps/tree-sitter".to_string()),
            ],
            ..Default::default()
        };
        let _ = server.language.load_settings(&settings);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // First request: semanticTokens/full to get the initial result_id.
        let full_params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let full_result = server
            .semantic_tokens_full_impl(full_params)
            .await
            .expect("full request should succeed")
            .expect("should return tokens");
        let initial_result_id = match full_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Second request: delta WITHOUT changing the document → no edits.
        let delta_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: initial_result_id.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let delta_result = server
            .semantic_tokens_full_delta_impl(delta_params)
            .await
            .expect("delta request should succeed")
            .expect("should return delta");

        let delta = match delta_result {
            SemanticTokensFullDeltaResult::TokensDelta(d) => d,
            other => panic!("expected TokensDelta for unchanged document, got {other:?}"),
        };

        // No edits, since nothing changed.
        assert!(
            delta.edits.is_empty(),
            "unchanged document should produce a delta with no edits, got {:?}",
            delta.edits
        );

        // The result_id must be reused, not rotated.
        assert_eq!(
            delta.result_id.as_deref(),
            Some(initial_result_id.as_str()),
            "no-op delta should reuse the previous result_id"
        );

        // The cache entry under the initial result_id must still be valid.
        assert!(
            server
                .cache
                .get_tokens_if_valid(&uri, &initial_result_id)
                .is_some(),
            "cache should still hold tokens under the reused result_id"
        );
    }

    /// Test that semantic token cache is preserved for delta calculations.
    ///
    /// This verifies the fix for the issue where `invalidate_semantic()` was being
    /// called on every `didChange`, preventing delta calculations from ever working.
    #[tokio::test]
    async fn semantic_tokens_cache_preserved_for_delta() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///cache_test.lua").expect("should construct test uri");

        // Insert a document
        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // First request: semanticTokens/full to populate the cache
        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let result = server.semantic_tokens_full_impl(params).await;
        assert!(result.is_ok(), "semantic_tokens_full should succeed");

        let tokens_result = result.unwrap();
        assert!(tokens_result.is_some(), "should return tokens");

        // Extract the result_id from the response
        let result_id = match tokens_result.unwrap() {
            SemanticTokensResult::Tokens(tokens) => tokens.result_id,
            _ => panic!("expected Tokens variant"),
        };
        assert!(result_id.is_some(), "should have result_id");
        let result_id = result_id.unwrap();

        // Verify the cache contains tokens with this result_id
        let cached = server.cache.get_tokens_if_valid(&uri, &result_id);
        assert!(
            cached.is_some(),
            "cache should contain tokens with result_id '{}'",
            result_id
        );

        // Simulate a document change (this would normally be done via didChange)
        // In production, didChange does NOT invalidate semantic cache anymore
        server.documents.update_document(
            uri.clone(),
            "local y = 2".to_string(),
            None, // tree will be None until next parse
        );

        // The cache must retain previous tokens after didChange — the delta
        // calculation on the next semanticTokens request depends on them.
        let still_cached = server.cache.get_tokens_if_valid(&uri, &result_id);
        assert!(
            still_cached.is_some(),
            "cache should STILL contain tokens after document update - needed for delta calculations"
        );
    }

    /// An unchanged document must reuse cached tokens instead of re-tokenizing:
    /// the second `semanticTokens/full` returns the SAME `result_id` as the first.
    /// Before content-hash keying, every full response drew a fresh `result_id`,
    /// so this asserts the cache short-circuit (skipped recomputation) is live.
    #[tokio::test]
    async fn semantic_tokens_full_reuses_cached_tokens_for_unchanged_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///unchanged.lua").expect("should construct test uri");

        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        let make_params = || SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let first = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("first full request should succeed")
            .expect("should return tokens");
        let first_id = match first {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Second request, document UNCHANGED: must serve the cached tokens (same
        // result_id), proving the re-tokenization was skipped.
        let second = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("second full request should succeed")
            .expect("should return tokens");
        let second_id = match second {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        assert_eq!(
            first_id, second_id,
            "an unchanged document should reuse cached tokens (stable result_id), \
             not recompute with a fresh id"
        );
    }

    /// End-to-end guard for #549: the same URI, re-assigned to a DIFFERENT
    /// language without any text change, must recompute rather than serve the
    /// first language's cached tokens. The text (and thus `cache_key`) is
    /// identical across both requests, so only the language dimension of the key
    /// prevents the collision. This deliberately does NOT go through `did_close`
    /// (which evicts the entry): the bug it locks is the lingering entry being
    /// re-read under the new language (the store-after-evict / reopen race), so
    /// the entry must survive into the second request.
    #[tokio::test]
    async fn semantic_tokens_full_recomputes_after_language_switch_same_text() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///switch.txt").expect("should construct test uri");
        // Text is only ever compared for equality across the two requests; it need
        // not be valid in either grammar (tree-sitter still yields a tree + tokens).
        let text = "local x = 1".to_string();

        for lang in ["lua", "rust"] {
            let load_result = server.language.ensure_language_loaded(lang);
            if !load_result.success || server.language.highlight_query(lang).is_none() {
                eprintln!("Skipping: {lang} parser or highlight query not available");
                return;
            }
        }

        let make_params = || SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        // Open as lua, compute + cache tokens under (uri, lua, cache_key).
        server
            .documents
            .insert(uri.clone(), text.clone(), Some("lua".to_string()), None);
        let lua_result = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("lua full request should succeed")
            .expect("should return tokens");
        let lua_id = match lua_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Re-assign the SAME uri + SAME text to rust WITHOUT closing (so the lua
        // cache entry lingers). The cache_key is unchanged (text + generation are),
        // so only the language guard can force a miss here.
        server
            .documents
            .insert(uri.clone(), text, Some("rust".to_string()), None);
        let rust_result = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("rust full request should succeed")
            .expect("should return tokens");
        let rust_id = match rust_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        assert_ne!(
            lua_id, rust_id,
            "switching the document's language (same text) must recompute, not \
             serve the previous language's cached tokens"
        );
    }

    /// A delta request on an unchanged document whose baseline matches the cached
    /// tokens returns an empty delta with the same `result_id` — the fast path that
    /// skips the `previous_tokens` clone and the O(N) diff entirely.
    #[tokio::test]
    async fn semantic_tokens_delta_returns_empty_delta_for_unchanged_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///delta_noop.lua").expect("should construct test uri");

        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // Full request establishes the baseline result_id.
        let full = server
            .semantic_tokens_full_impl(SemanticTokensParams {
                text_document: TextDocumentIdentifier {
                    uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("full request should succeed")
            .expect("should return tokens");
        let baseline_id = match full {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Delta on the UNCHANGED document with the matching baseline: empty delta,
        // same result_id (the fast path).
        let delta = server
            .semantic_tokens_full_delta_impl(SemanticTokensDeltaParams {
                text_document: TextDocumentIdentifier {
                    uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
                },
                previous_result_id: baseline_id.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("delta request should succeed")
            .expect("should return a delta");
        match delta {
            SemanticTokensFullDeltaResult::TokensDelta(d) => {
                assert_eq!(
                    d.result_id,
                    Some(baseline_id),
                    "no-op delta should reuse the baseline result_id"
                );
                assert!(
                    d.edits.is_empty(),
                    "an unchanged document should yield an empty delta"
                );
            }
            other => panic!("expected an empty TokensDelta, got {:?}", other),
        }
    }

    /// Test that semantic tokens full request returns RequestCancelled (-32800) when cancelled.
    ///
    /// This verifies the fix for immediate cancellation support:
    /// 1. Start a semantic tokens request for a large document
    /// 2. Immediately trigger cancellation via CancelForwarder
    /// 3. Verify that RequestCancelled error (-32800) is returned
    #[tokio::test]
    async fn semantic_tokens_full_returns_request_cancelled_when_cancelled() {
        use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
        use crate::lsp::request_id::CancelForwarder;
        use std::sync::Arc;

        // Create shared pool and cancel forwarder
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));

        // Create server with shared cancel forwarder
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, pool, cancel_forwarder.clone())
        });
        let server = service.inner();
        let uri = Url::parse("file:///cancel_test.lua").expect("should construct test uri");

        // Create a moderately large document to ensure processing takes some time
        let mut text = String::from("local M = {}\n");
        for i in 0..500 {
            text.push_str(&format!("local var_{} = {}\n", i, i));
        }
        text.push_str("return M\n");

        server
            .documents
            .insert(uri.clone(), text, Some("lua".to_string()), None);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success {
            eprintln!("Skipping: lua language parser not available for cancel test");
            return;
        }

        // Trigger cancel immediately (simulating $/cancelRequest arrival)
        // We set a task-local request ID so subscribe_cancel() can subscribe,
        // then notify on the same ID.
        let cancel_forwarder_clone = cancel_forwarder.clone();
        tokio::spawn(async move {
            // Small delay to ensure the request starts processing and subscribes
            sleep(Duration::from_millis(1)).await;
            cancel_forwarder_clone.notify_cancel(&UpstreamId::Number(999));
        });

        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        // Call the public implementation within a task-local request ID scope
        // so subscribe_cancel() can subscribe to cancel notifications
        let result = crate::lsp::request_id::CURRENT_REQUEST_ID
            .scope(
                Some(tower_lsp_server::jsonrpc::Id::Number(999)),
                server.semantic_tokens_full_impl(params),
            )
            .await;

        // Verify we got RequestCancelled error (-32800)
        match result {
            Err(e) => {
                assert_eq!(
                    e.code,
                    tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled,
                    "should return RequestCancelled error code (-32800), got: {:?}",
                    e.code
                );
            }
            Ok(_) => {
                // If the request completed before cancel was processed, that's also acceptable
                // (cancel is best-effort per LSP spec). But we expect cancel to win for large docs.
                eprintln!(
                    "Note: request completed before cancel - this is acceptable but unexpected for large docs"
                );
            }
        }
    }

    /// Test that semantic tokens full delta request returns RequestCancelled (-32800) when cancelled.
    ///
    /// Similar to the full request test, but specifically tests the delta endpoint:
    /// 1. First request semantic tokens to establish a baseline result_id
    /// 2. Start a semantic tokens delta request for a large document
    /// 3. Immediately trigger cancellation via CancelForwarder
    /// 4. Verify that RequestCancelled error (-32800) is returned
    #[tokio::test]
    async fn semantic_tokens_full_delta_returns_request_cancelled_when_cancelled() {
        use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
        use crate::lsp::request_id::CancelForwarder;
        use std::sync::Arc;

        // Create shared pool and cancel forwarder
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));

        // Create server with shared cancel forwarder
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, pool, cancel_forwarder.clone())
        });
        let server = service.inner();
        let uri = Url::parse("file:///cancel_delta_test.lua").expect("should construct test uri");

        // Create a moderately large document to ensure processing takes some time
        let mut text = String::from("local M = {}\n");
        for i in 0..500 {
            text.push_str(&format!("local var_{} = {}\n", i, i));
        }
        text.push_str("return M\n");

        server
            .documents
            .insert(uri.clone(), text, Some("lua".to_string()), None);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success {
            eprintln!("Skipping: lua language parser not available for cancel delta test");
            return;
        }

        // First, get initial tokens to establish a result_id for delta requests
        let full_params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let initial_result = server.semantic_tokens_full_impl(full_params).await;

        let previous_result_id = match initial_result {
            Ok(Some(SemanticTokensResult::Tokens(tokens))) => {
                tokens.result_id.expect("should have result_id")
            }
            _ => {
                eprintln!("Skipping: could not get initial tokens for delta test");
                return;
            }
        };

        // Trigger cancel immediately (simulating $/cancelRequest arrival)
        // We set a task-local request ID so subscribe_cancel() can subscribe,
        // then notify on the same ID.
        let cancel_forwarder_clone = cancel_forwarder.clone();
        tokio::spawn(async move {
            // Small delay to ensure the request starts processing and subscribes
            sleep(Duration::from_millis(1)).await;
            cancel_forwarder_clone.notify_cancel(&UpstreamId::Number(999));
        });

        let delta_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        // Call the public delta implementation within a task-local request ID scope
        // so subscribe_cancel() can subscribe to cancel notifications
        let result = crate::lsp::request_id::CURRENT_REQUEST_ID
            .scope(
                Some(tower_lsp_server::jsonrpc::Id::Number(999)),
                server.semantic_tokens_full_delta_impl(delta_params),
            )
            .await;

        // Verify we got RequestCancelled error (-32800)
        match result {
            Err(e) => {
                assert_eq!(
                    e.code,
                    tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled,
                    "should return RequestCancelled error code (-32800), got: {:?}",
                    e.code
                );
            }
            Ok(_) => {
                // If the request completed before cancel was processed, that's also acceptable
                // (cancel is best-effort per LSP spec). But we expect cancel to win for large docs.
                eprintln!(
                    "Note: delta request completed before cancel - this is acceptable but unexpected for large docs"
                );
            }
        }
    }
}
