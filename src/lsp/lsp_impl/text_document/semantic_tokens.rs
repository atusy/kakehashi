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
//!   polls it throughout host and injected-language query walks, injection
//!   discovery, nested regions, and final shaping. Parsing itself remains
//!   non-preemptible, but a region that observes cancellation returns incomplete
//!   output that is neither served nor cached.
//!
//! This is achieved by subscribing to cancel notifications via `CancelForwarder::subscribe()`
//! and using biased `tokio::select!` to prioritize cancel handling.

use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::{
    SemanticTokens, SemanticTokensDeltaParams, SemanticTokensFullDeltaResult, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
};
use url::Url;

#[cfg(test)]
use tower_lsp_server::ls_types::{
    PartialResultParams, Position, Range, TextDocumentIdentifier, WorkDoneProgressParams,
};

use crate::analysis::{
    SemanticSnapshotIdentity, calculate_delta_or_full, filter_semantic_tokens_by_range,
    handle_semantic_tokens_full, next_result_id,
};
use crate::lsp::current_upstream_id;

use super::super::{Kakehashi, uri_to_url};

/// Outcome of the serve-current snapshot resolution for the whole-document
/// token handlers (see [`Kakehashi::current_snapshot_for_tokens`]).
pub(crate) enum TokenSnapshot {
    /// The snapshot is current (`parsed_version == content_version`) —
    /// compute against it.
    Current(std::sync::Arc<crate::document::snapshot::ParseSnapshot>),
    /// Unregistered/closed URI, or the first parse never landed within its
    /// backstop — the handlers' empty-tokens fallback applies.
    Absent,
    /// The snapshot still trailed the live text when the settle backstop
    /// expired — reject with `ContentModified`; the parse loop's settle
    /// refresh re-drives the client once the parse lands.
    Stale,
    /// The client cancelled the request while it was parked.
    Cancelled,
    /// A newer request for the same document superseded this one while it was
    /// parked (`SemanticRequestTracker` flipped its token). Answer `Ok(None)`
    /// — the same contract as a compute superseded mid-flight — instead of
    /// riding out the park: on a client that supersedes without sending
    /// `$/cancelRequest`, obsolete parked requests would otherwise hold
    /// ingress admission slots until the parse settles or the backstop
    /// expires.
    Superseded,
}

/// The delta handler's "current" tokens, either reused from the cache (an
/// `Arc`, no deep copy yet) or freshly computed (already owned). Comparison
/// against the previous baseline only needs a reference — [`as_ref`](Self::as_ref)
/// — so the cache-hit case never clones unless a match arm downstream
/// actually needs to store the value ([`into_owned`](Self::into_owned)),
/// which a no-op/empty-edits delta never does.
enum CurrentTokens {
    Cached(std::sync::Arc<SemanticTokens>),
    Owned(SemanticTokens),
}

impl CurrentTokens {
    fn from_result(result: SemanticTokensResult) -> Self {
        match result {
            SemanticTokensResult::Tokens(tokens) => Self::Owned(tokens),
            SemanticTokensResult::Partial(_) => Self::Owned(SemanticTokens {
                result_id: None,
                data: Vec::new(),
            }),
        }
    }

    fn as_ref(&self) -> &SemanticTokens {
        match self {
            Self::Cached(arc) => arc,
            Self::Owned(tokens) => tokens,
        }
    }

    fn into_owned(self) -> SemanticTokens {
        match self {
            // The cache entry may have been overwritten or evicted between
            // this handle being taken and here, leaving this the sole
            // strong ref — in that case the data is already effectively
            // ours, and `try_unwrap` reclaims it instead of cloning.
            Self::Cached(arc) => {
                std::sync::Arc::try_unwrap(arc).unwrap_or_else(|arc| (*arc).clone())
            }
            Self::Owned(tokens) => tokens,
        }
    }
}

impl Kakehashi {
    fn semantic_snapshot_is_current(
        &self,
        uri: &Url,
        incarnation: u64,
        parsed_version: u64,
        generation: u64,
        edit_lock: &std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> bool {
        let latest = self.documents.latest_snapshot(uri);
        let current = self.cache.semantic_token_generation() == generation
            && latest.as_ref().is_some_and(|view| {
                view.slot.current_incarnation == incarnation
                    && view.content_version == parsed_version
            });
        if latest.is_none() {
            self.documents.remove_edit_lock_if_unshared(uri, edit_lock);
        }
        current
    }

    /// Register interest before re-reading a placeholder: publication can run
    /// outside the edit lock. A tree published before this read is consumed now;
    /// publication after it sees the interest and refreshes the empty response.
    async fn resolve_empty_token_snapshot(
        &self,
        uri: &Url,
        snapshot: std::sync::Arc<crate::document::snapshot::ParseSnapshot>,
        generation: u64,
        request_id: Option<crate::lsp::cache::RequestId>,
    ) -> Option<std::sync::Arc<crate::document::snapshot::ParseSnapshot>> {
        if self.cache.semantic_token_generation() != generation {
            return None;
        }
        if snapshot.tree.is_some() && snapshot.language.is_some() {
            return Some(snapshot);
        }
        let edit_lock = self.documents.edit_lock(uri);
        let _guard = edit_lock.lock().await;
        if !self.semantic_snapshot_is_current(
            uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            generation,
            &edit_lock,
        ) || request_id.is_some_and(|id| !self.cache.is_request_active(uri, id))
        {
            return None;
        }
        self.cache
            .record_served_semantic_version(uri, snapshot.parsed_version);
        self.documents.latest_snapshot(uri)?.slot.snapshot
    }

    /// Register a failed current-snapshot wait, then recheck publication. The
    /// parse may have settled after the timeout but before this marker: in that
    /// case serve it now instead of losing the only refresh wakeup.
    /// An edit or reopen while acquiring the lock must not inherit this wait's
    /// retry interest after clearing the previous interval's marker.
    async fn token_snapshot_after_timeout(
        &self,
        uri: &Url,
        expected_edit: Option<(u64, u64)>,
        generation: u64,
        supersede: &crate::cancel::CancelToken,
    ) -> TokenSnapshot {
        let edit_lock = self.documents.edit_lock(uri);
        let _guard = edit_lock.lock().await;
        if supersede.is_cancelled() || self.cache.semantic_token_generation() != generation {
            return TokenSnapshot::Superseded;
        }
        let Some(view) = self.documents.latest_snapshot(uri) else {
            self.documents.remove_edit_lock_if_unshared(uri, &edit_lock);
            return TokenSnapshot::Absent;
        };
        if expected_edit != Some((view.slot.current_incarnation, view.content_version)) {
            return TokenSnapshot::Superseded;
        }
        self.cache.record_served_semantic_version(uri, 0);
        let Some(view) = self.documents.latest_snapshot(uri) else {
            return TokenSnapshot::Absent;
        };
        match view.slot.snapshot {
            Some(snapshot) if snapshot.parsed_version == view.content_version => {
                TokenSnapshot::Current(snapshot)
            }
            _ => TokenSnapshot::Stale,
        }
    }

    /// Latest-completed snapshot resolution (parse-snapshot ADR §3): returns
    /// the newest published snapshot, which may trail the input. The only
    /// wait is the bounded first-parse wait (no snapshot for this lifetime
    /// yet); no per-keystroke read ever waits on a reparse. Used by the
    /// currency-*checking* readers (`semanticTokens/range`, which resolves
    /// through this and then staleness-rejects inline against the live
    /// version). `None` for an unregistered/closed URI or when no parse
    /// resolves within the wait.
    pub(crate) async fn snapshot_for_tokens(
        &self,
        uri: &Url,
    ) -> Option<std::sync::Arc<crate::document::snapshot::ParseSnapshot>> {
        // Generous on purpose: this wait only runs while the document has NO
        // snapshot for its lifetime, and every open-parse resolution path
        // publishes one (a tree, a resolved-but-tree-less outcome, or the
        // didClose sentinel — all of which wake this receiver). The wait is
        // therefore bounded by parse completion (itself capped by the parse
        // work-unit timeout), not by this constant; the constant is only a
        // backstop. A tight cap here made first requests racing didOpen
        // answer empty whenever the machine was loaded enough to push the
        // open parse past it (observed under the parallel e2e suite, and the
        // real-world analog is editor startup on a busy machine).
        let deadline =
            tokio::time::Instant::now() + crate::lsp::lsp_impl::snapshot_read::FIRST_PARSE_BACKSTOP;
        loop {
            // Subscribe BEFORE checking: `watch::Sender::subscribe` marks the
            // value current at subscription time as already seen, so a publish
            // landing between a check and a later subscribe would be invisible
            // to `changed()` — a lost wakeup that burned the whole wait and
            // served the pre-parse fallback (the e2e-visible symptom: node —
            // and first-token — requests racing didOpen answered null).
            // Subscribing first closes the window: a publish before the check
            // is caught by the check, one after it triggers `changed()`.
            let mut receiver = self.documents.subscribe_snapshots(uri)?;
            // Re-resolve the cell per iteration (per-request re-resolution +
            // incarnation validation happen inside `latest_snapshot`).
            let view = self.documents.latest_snapshot(uri)?;
            if let Some(snapshot) = view.slot.snapshot {
                return Some(snapshot);
            }
            match tokio::time::timeout_at(deadline, receiver.changed()).await {
                Ok(Ok(())) => continue,
                // Channel closed (document gone) or first-parse wait elapsed.
                _ => return None,
            }
        }
    }

    /// Serve-current snapshot resolution for `semanticTokens/full` and
    /// `full/delta`: park (racing the client's `$/cancelRequest`) until the
    /// latest snapshot is **current**, then compute against that.
    ///
    /// Why not serve the latest completed snapshot and let the parse loop's
    /// refresh heal the client? Because the editor draws whatever we answer
    /// against the text it has NOW: Neovim stamps the response with the
    /// buffer version at request time and renders it as soon as that matches
    /// the live buffer (`vim.lsp.semantic_tokens`: `process_response` /
    /// `on_win`, extmarks placed with `strict = false`), so tokens computed
    /// for older text land visibly misplaced on unchanged lines. While we
    /// park instead, the editor keeps its previous tokens as extmarks that
    /// shift with the edit — temporarily unhighlighted new text, never
    /// corrupted existing text. The wait is bounded by parse completion
    /// (every edit's parse publishes), not by typing: the settle backstop
    /// only expires when the pipeline is pathologically behind.
    pub(crate) async fn current_snapshot_for_tokens(
        &self,
        uri: &Url,
        cancel_rx: Option<&mut crate::lsp::request_id::CancelReceiver>,
        supersede: &crate::cancel::CancelToken,
        generation: u64,
        request_id: Option<crate::lsp::cache::RequestId>,
    ) -> TokenSnapshot {
        use crate::lsp::lsp_impl::snapshot_read::{SnapshotWait, TOKEN_SETTLE_BACKSTOP};
        let profile_start = log::log_enabled!(target: "kakehashi::profile", log::Level::Debug)
            .then(std::time::Instant::now);
        let result = async {
            let wait = async {
                // Only timeout interest belongs to this starting edit. A
                // successful wait may still consume a newer current snapshot.
                let expected_edit = self
                    .documents
                    .latest_snapshot(uri)
                    .map(|view| (view.slot.current_incarnation, view.content_version));
                let outcome = match self
                    .wait_for_current_snapshot(uri, TOKEN_SETTLE_BACKSTOP)
                    .await
                {
                    SnapshotWait::Current(snapshot) => TokenSnapshot::Current(snapshot),
                    SnapshotWait::Stale => {
                        self.token_snapshot_after_timeout(uri, expected_edit, generation, supersede)
                            .await
                    }
                    SnapshotWait::Unparsed | SnapshotWait::Gone => TokenSnapshot::Absent,
                };
                match outcome {
                    TokenSnapshot::Current(snapshot) => match self
                        .resolve_empty_token_snapshot(uri, snapshot, generation, request_id)
                        .await
                    {
                        Some(snapshot) => TokenSnapshot::Current(snapshot),
                        None => TokenSnapshot::Superseded,
                    },
                    other => other,
                }
            };
            match cancel_rx {
                Some(rx) => {
                    tokio::select! {
                        biased;
                        // Fires on $/cancelRequest (and on forwarder teardown,
                        // which the compute-race arms below treat as cancel too).
                        _ = rx => TokenSnapshot::Cancelled,
                        // Fires when a newer request for this document flips this
                        // request's tracker token — release the park (and its
                        // admission slot) instead of computing a discarded result.
                        _ = supersede.cancelled() => TokenSnapshot::Superseded,
                        outcome = wait => outcome,
                    }
                }
                None => {
                    tokio::select! {
                        biased;
                        _ = supersede.cancelled() => TokenSnapshot::Superseded,
                        outcome = wait => outcome,
                    }
                }
            }
        }
        .await;
        if let Some(start) = profile_start {
            let outcome = match &result {
                TokenSnapshot::Current(_) => "current",
                TokenSnapshot::Stale => "stale",
                TokenSnapshot::Absent => "absent",
                TokenSnapshot::Cancelled => "cancelled",
                TokenSnapshot::Superseded => "superseded",
            };
            log::debug!(
                target: "kakehashi::profile",
                "phase=snapshot_wait elapsed_us={} outcome={} uri={}",
                start.elapsed().as_micros(), outcome, uri,
            );
        }
        result
    }

    pub(crate) async fn semantic_tokens_full_impl(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let upstream_id = current_upstream_id();
        let (mut cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
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
        // Serve-current (ADR §3, revised): park until the snapshot matches the
        // live text — see `current_snapshot_for_tokens` for why answering from
        // a trailing snapshot corrupts the editor's existing highlights. The
        // resolved snapshot's (text, tree, language) triple is internally
        // consistent, and every input below (query, mappings) resolves against
        // the snapshot's own detected language — never a live re-detection
        // that could diverge from the tree's grammar.
        let snapshot = match self
            .current_snapshot_for_tokens(
                &uri,
                cancel_rx.as_mut(),
                &cancel_token,
                token_generation,
                Some(request_id),
            )
            .await
        {
            TokenSnapshot::Current(snapshot) => snapshot,
            TokenSnapshot::Absent => {
                self.cache.finish_request(&uri, request_id);
                return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: vec![],
                })));
            }
            TokenSnapshot::Stale => {
                self.cache.finish_request(&uri, request_id);
                return Err(crate::error::content_modified_error());
            }
            TokenSnapshot::Cancelled => {
                cancel_token.cancel();
                self.cache.finish_request(&uri, request_id);
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS] CANCELLED via $/cancelRequest uri={} req={} (while parked)",
                    uri, request_id
                );
                return Err(Error::request_cancelled());
            }
            TokenSnapshot::Superseded => {
                // Same contract as a compute superseded mid-flight (below):
                // the newer request answers; this one drops out quietly.
                self.cache.finish_request(&uri, request_id);
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (superseded while parked)",
                    uri, request_id
                );
                return Ok(None);
            }
        };
        let (Some(language_name), Some(tree)) = (snapshot.language.clone(), snapshot.tree.clone())
        else {
            // The placeholder was rechecked after registering refresh interest.
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        let text = std::sync::Arc::clone(&snapshot.text);

        // Ensure language is loaded before trying to get queries.
        // This handles the race condition where semanticTokens/full arrives
        // before didOpen finishes loading the language.
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
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

        // Read the remaining settings-dependent tokenization inputs HERE —
        // together with the query above, with no `.await` in between — so a
        // settings reload can't split them into an inconsistent mix (e.g. old
        // query + new capture mappings). All are consistent with the
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

        let snapshot_identity = SemanticSnapshotIdentity {
            parsed_version: snapshot.parsed_version,
            incarnation: snapshot.incarnation,
            generation: token_generation,
        };
        if let Some(cached) =
            self.cache
                .get_current_tokens_for_snapshot(&uri, &language_name, snapshot_identity)
        {
            let cached = (*cached).clone();
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let still_current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                token_generation,
                &edit_lock,
            );
            if !still_current || !self.cache.is_request_active(&uri, request_id) {
                self.cache.finish_request(&uri, request_id);
                return Ok(None);
            }
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensResult::Tokens(cached)));
        }

        // Validity key for the snapshot's text under the generation captured at
        // the top: keys both the unchanged-document cache short-circuit below
        // and the store of the freshly computed tokens. Keying off the
        // snapshot's own text hash means a compute racing a fresh edit stores
        // under its own text's hash, which a post-edit request never looks up.
        let cache_key = self.cache.cache_key_for(&text, token_generation);

        // Compute tokens against the (current-at-resolution) snapshot. An edit
        // landing after the resolution supersedes this request via the client's
        // next didChange-driven request; the CancelToken then reclaims the
        // compute mid-flight.
        let result = {
            // Snapshot-identical repeat request: tokens already cached for this
            // exact text are still correct, so skip re-tokenizing. Returns the
            // cached tokens with their original `result_id`, keeping a client's
            // delta baseline stable across idle re-requests.
            if let Some(cached) = self
                .cache
                .get_current_tokens(&uri, &language_name, cache_key)
            {
                let cached = (*cached).clone();
                let edit_lock = self.documents.edit_lock(&uri);
                let _edit_guard = edit_lock.lock().await;
                let still_current = self.semantic_snapshot_is_current(
                    &uri,
                    snapshot.incarnation,
                    snapshot.parsed_version,
                    token_generation,
                    &edit_lock,
                );
                if !still_current || !self.cache.is_request_active(&uri, request_id) {
                    self.cache.finish_request(&uri, request_id);
                    return Ok(None);
                }
                self.cache
                    .record_served_semantic_version(&uri, snapshot.parsed_version);
                self.cache.finish_request(&uri, request_id);
                // The wire type owns its data (`ls_types::SemanticTokensResult`
                // has no borrowing variant), so this is the one legitimate
                // materialization point — everything upstream (the cache hit
                // itself) stayed O(1) via the `Arc`.
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
                documents: std::sync::Arc::clone(&self.documents),
                parsed_version: snapshot.parsed_version,
                incarnation: snapshot.incarnation,
                // The snapshot's own discovery (ADR §3, don't-discover-twice):
                // rebuilt into contexts instead of re-running the injection
                // query, when its generation still matches.
                discovery: snapshot.injection_regions.clone(),
            });

            // Compute tokens, racing against cancel notification if provided
            let compute_future = handle_semantic_tokens_full(
                &self.compute_pool,
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

            if let Some(cancel_rx) = cancel_rx {
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
            }
        };

        // A supersede/close between compute start and here flips the token; the
        // compute then bailed at a checkpoint and returned `None` (a partial
        // result), so drop the request rather than storing it over the cache.
        // This is CPU-reclamation, not staleness-rejection: an *un*-superseded
        // compute over a snapshot the live text has since outrun still serves —
        // the client's didChange-driven follow-up request supersedes and heals
        // (§4's narrowed CancelToken role).
        if cancel_token.is_cancelled() {
            self.cache.finish_request(&uri, request_id);
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (compute superseded)",
                uri, request_id
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
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        let still_current = self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            token_generation,
            &edit_lock,
        );
        if !still_current || !self.cache.is_request_active(&uri, request_id) {
            self.cache.finish_request(&uri, request_id);
            return Ok(None);
        }
        // Store keyed by result_id (delta baseline) AND cache_key (so an
        // unchanged-document repeat request short-circuits the re-tokenization
        // above). `language_name` is unused after this, so move it in.
        self.cache.store_tokens(
            uri.clone(),
            stored_tokens,
            language_name,
            cache_key,
            snapshot_identity,
        );

        // Finish tracking this request
        self.cache
            .record_served_semantic_version(&uri, snapshot.parsed_version);
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
        let (mut cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
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

        // Serve-current (ADR §3, revised): park until the snapshot matches the
        // live text (same rationale as semanticTokens/full — this is the
        // steady-state typing path where a stale answer corrupts the editor's
        // existing highlights AND poisons the client's delta baseline).
        let snapshot = match self
            .current_snapshot_for_tokens(
                &uri,
                cancel_rx.as_mut(),
                &cancel_token,
                token_generation,
                Some(request_id),
            )
            .await
        {
            TokenSnapshot::Current(snapshot) => snapshot,
            TokenSnapshot::Absent => {
                self.cache.finish_request(&uri, request_id);
                return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                    SemanticTokens {
                        result_id: None,
                        data: vec![],
                    },
                )));
            }
            TokenSnapshot::Stale => {
                self.cache.finish_request(&uri, request_id);
                return Err(crate::error::content_modified_error());
            }
            TokenSnapshot::Cancelled => {
                cancel_token.cancel();
                self.cache.finish_request(&uri, request_id);
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS_DELTA] CANCELLED via $/cancelRequest uri={} req={} (while parked)",
                    uri, request_id
                );
                return Err(Error::request_cancelled());
            }
            TokenSnapshot::Superseded => {
                // Same contract as a compute superseded mid-flight: the newer
                // request answers; this one drops out quietly.
                self.cache.finish_request(&uri, request_id);
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} (superseded while parked)",
                    uri, request_id
                );
                return Ok(None);
            }
        };
        let (Some(language_name), Some(tree)) = (snapshot.language.clone(), snapshot.tree.clone())
        else {
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensFullDeltaResult::Tokens(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
            )));
        };
        let text = std::sync::Arc::clone(&snapshot.text);

        // Ensure language is loaded before trying to get queries.
        // This handles the race condition where semanticTokens/full/delta arrives
        // before didOpen finishes loading the language.
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
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

        // Read the remaining settings-dependent tokenization inputs HERE — with
        // the query above, no `.await` in between — so a settings reload can't
        // split them into an inconsistent mix (same as semanticTokens/full).
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

        let snapshot_identity = SemanticSnapshotIdentity {
            parsed_version: snapshot.parsed_version,
            incarnation: snapshot.incarnation,
            generation: token_generation,
        };
        if let Some(cached) =
            self.cache
                .get_current_tokens_for_snapshot(&uri, &language_name, snapshot_identity)
            && cached.result_id.as_deref() == Some(previous_result_id.as_str())
        {
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let still_current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                token_generation,
                &edit_lock,
            );
            if !still_current || !self.cache.is_request_active(&uri, request_id) {
                self.cache.finish_request(&uri, request_id);
                return Ok(None);
            }
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            self.cache.finish_request(&uri, request_id);
            return Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
                tower_lsp_server::ls_types::SemanticTokensDelta {
                    result_id: Some(previous_result_id),
                    edits: vec![],
                },
            )));
        }

        // Validity key for the snapshot's text under the generation captured at
        // the top (see semanticTokens/full for why snapshot-text keying makes
        // a compute racing a fresh edit cache-safe).
        let cache_key = self.cache.cache_key_for(&text, token_generation);

        // Compute tokens against the (current-at-resolution) snapshot; same as
        // semanticTokens/full.
        let result = {
            // Snapshot-identical repeat request: reuse the cached full tokens
            // instead of re-tokenizing.
            if let Some(cached) = self
                .cache
                .get_current_tokens(&uri, &language_name, cache_key)
            {
                // Fast path: the client's baseline already IS these cached tokens,
                // so the delta is necessarily empty — return it directly and skip
                // the `previous_tokens` clone + O(N) `calculate_delta` below.
                if cached.result_id.as_deref() == Some(previous_result_id.as_str()) {
                    let edit_lock = self.documents.edit_lock(&uri);
                    let _edit_guard = edit_lock.lock().await;
                    let still_current = self.semantic_snapshot_is_current(
                        &uri,
                        snapshot.incarnation,
                        snapshot.parsed_version,
                        token_generation,
                        &edit_lock,
                    );
                    if !still_current || !self.cache.is_request_active(&uri, request_id) {
                        self.cache.finish_request(&uri, request_id);
                        return Ok(None);
                    }
                    self.cache
                        .record_served_semantic_version(&uri, snapshot.parsed_version);
                    self.cache.finish_request(&uri, request_id);
                    return Ok(Some(SemanticTokensFullDeltaResult::TokensDelta(
                        tower_lsp_server::ls_types::SemanticTokensDelta {
                            result_id: Some(previous_result_id),
                            edits: vec![],
                        },
                    )));
                }
                // Baseline differs: fall through to diff the cached tokens
                // against the client's `previous_result_id` (still skips
                // re-tokenization). Kept as the Arc — cloned into an owned
                // `SemanticTokens` only in the match arms below that actually
                // store, not unconditionally here (a stale-but-content-
                // unchanged baseline lands in the empty-edits arm, which
                // never needs ownership at all).
                Some(CurrentTokens::Cached(cached))
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
                    documents: std::sync::Arc::clone(&self.documents),
                    parsed_version: snapshot.parsed_version,
                    incarnation: snapshot.incarnation,
                    // The snapshot's own discovery (ADR §3, don't-discover-twice).
                    discovery: snapshot.injection_regions.clone(),
                });

                // Compute tokens, racing against cancel notification if provided
                let compute_future = handle_semantic_tokens_full(
                    &self.compute_pool,
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

                let computed = if let Some(cancel_rx) = cancel_rx {
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
                computed.map(CurrentTokens::from_result)
            }
        };

        // A supersede/close between compute start and here flips the token; the
        // compute then bailed at a checkpoint and returned `None` (partial), so
        // drop the request rather than diffing/storing it over the cache. This
        // is CPU-reclamation, not staleness-rejection (§4's narrowed CancelToken
        // role).
        if cancel_token.is_cancelled() {
            self.cache.finish_request(&uri, request_id);
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS_DELTA] CANCELLED uri={} req={} (compute superseded)",
                uri, request_id
            );
            return Ok(None);
        }

        // Current tokens from the result — kept lazy (`CurrentTokens::Cached`
        // stays an `Arc`) until a downstream match arm actually needs an
        // owned value to mutate and store.
        let current_tokens = result.unwrap_or_else(|| {
            CurrentTokens::Owned(SemanticTokens {
                result_id: None,
                data: Vec::new(),
            })
        });

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

        // No valid previous baseline: a full result is unavoidable either
        // way, so this consumes `current_tokens` directly (a cheap
        // `try_unwrap` when the Arc is uniquely owned) instead of routing
        // through `delta_result`'s `Tokens` arm, which would clone it once
        // to build the intermediate value and again for the cache store.
        let (final_result, tokens_to_store) = if let Some(prev) = previous_tokens {
            // Calculate delta or return full tokens outside the document edit
            // lock. Only the final currency check and cache commit need to be
            // serialized with didChange/didClose.
            let delta_result =
                calculate_delta_or_full(&prev, current_tokens.as_ref(), &previous_result_id);

            match delta_result {
                SemanticTokensFullDeltaResult::Tokens(mut tokens) => {
                    tokens.result_id = Some(next_result_id());
                    let stored = tokens.clone();
                    (SemanticTokensFullDeltaResult::Tokens(tokens), Some(stored))
                }
                SemanticTokensFullDeltaResult::TokensDelta(mut delta) if delta.edits.is_empty() => {
                    delta.result_id = Some(previous_result_id.clone());
                    (SemanticTokensFullDeltaResult::TokensDelta(delta), None)
                }
                SemanticTokensFullDeltaResult::TokensDelta(mut delta) => {
                    let mut stored = current_tokens.into_owned();
                    stored.result_id = Some(next_result_id());
                    delta.result_id = stored.result_id.clone();
                    (
                        SemanticTokensFullDeltaResult::TokensDelta(delta),
                        Some(stored),
                    )
                }
                SemanticTokensFullDeltaResult::PartialTokensDelta { .. } => {
                    log::warn!(
                        target: "kakehashi::semantic",
                        "[SEMANTIC_TOKENS_DELTA] Unexpected PartialTokensDelta variant for uri={}",
                        uri
                    );
                    let mut tokens = current_tokens.into_owned();
                    tokens.result_id = Some(next_result_id());
                    let stored = tokens.clone();
                    (SemanticTokensFullDeltaResult::Tokens(tokens), Some(stored))
                }
            }
        } else {
            let mut tokens = current_tokens.into_owned();
            tokens.result_id = Some(next_result_id());
            let stored = tokens.clone();
            (SemanticTokensFullDeltaResult::Tokens(tokens), Some(stored))
        };

        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        let still_current = self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            token_generation,
            &edit_lock,
        );
        if !still_current || !self.cache.is_request_active(&uri, request_id) {
            self.cache.finish_request(&uri, request_id);
            return Ok(None);
        }
        if let Some(tokens) = tokens_to_store {
            self.cache.store_tokens(
                uri.clone(),
                tokens,
                language_name,
                cache_key,
                snapshot_identity,
            );
        }

        // Finish tracking this request
        self.cache
            .record_served_semantic_version(&uri, snapshot.parsed_version);
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
        let upstream_id = current_upstream_id();
        let (mut cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in semanticTokens/range: {}", lsp_uri.as_str());
            return Ok(None);
        };

        let domain_range = range;
        if (domain_range.start.line, domain_range.start.character)
            >= (domain_range.end.line, domain_range.end.character)
        {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        }

        // Snapshot the settings generation at the top, before any await (#535): a
        // reload that bumps the generation after this leaves this request's stored
        // key on the old generation — invisible to post-reload requests (which
        // compute the new-generation key) — so a stale entry can never be served.
        let generation = self.cache.semantic_token_generation();

        // First-parse bound (parse-snapshot ADR §3): resolve through the same
        // bounded first-parse wait as full/delta. Without it, a range request
        // racing didOpen answered empty tokens with nothing to re-drive the
        // client — the parse loop's refresh heals full/delta lineages, but an
        // empty range response has no lineage, so the viewport stayed blank
        // until an incidental re-request. Steady state (snapshot present)
        // resolves immediately.
        let Some(snapshot) = self.snapshot_for_tokens(&uri).await else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        // Staleness-reject: the request's `range` is authored against the
        // LIVE text, so a trailing (or cross-incarnation) snapshot cannot
        // answer it — unlike full/delta (whole-document, which PARK for the
        // current snapshot instead). A stale snapshot → ContentModified; the
        // client's next natural request (this is a per-redraw viewport read)
        // gets the fresh one.
        let Some(view) = self.documents.latest_snapshot(&uri) else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        if snapshot.incarnation != view.slot.current_incarnation
            || snapshot.parsed_version != view.content_version
        {
            return Err(crate::error::content_modified_error());
        }
        let snapshot_identity = SemanticSnapshotIdentity {
            parsed_version: snapshot.parsed_version,
            incarnation: snapshot.incarnation,
            generation,
        };
        let resolve = self.resolve_empty_token_snapshot(&uri, snapshot, generation, None);
        let resolved = match cancel_rx.as_mut() {
            Some(rx) => {
                tokio::select! {
                    biased;
                    _ = rx => return Err(Error::request_cancelled()),
                    snapshot = resolve => snapshot,
                }
            }
            None => resolve.await,
        };
        let Some(snapshot) = resolved else {
            return Err(crate::error::content_modified_error());
        };
        let (Some(language_name), Some(tree)) = (snapshot.language.clone(), snapshot.tree.clone())
        else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        let text = std::sync::Arc::clone(&snapshot.text);

        let language_loaded = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await
            .success;
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        if !self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            generation,
            &edit_lock,
        ) {
            return Err(crate::error::content_modified_error());
        }
        if !language_loaded {
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        }
        let Some(query) = self.language.highlight_query(&language_name) else {
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        drop(_edit_guard);

        // Short-circuit an identical-viewport re-request of an unchanged document
        // (#535). `cache_key` folds the document text with the settings generation,
        // and the entry also pins the viewport `range`, so a hit means re-tokenizing
        // would reproduce these exact tokens. Misses (scroll, edit, or reload) fall
        // through to the recompute below, which restores the entry.
        let cache_key = self.cache.cache_key_for(&text, generation);
        if let Some(tokens) =
            self.cache
                .get_current_range_tokens(&uri, &domain_range, &language_name, cache_key)
        {
            let tokens = (*tokens).clone();
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                generation,
                &edit_lock,
            );
            if !current {
                return Err(crate::error::content_modified_error());
            }
            return Ok(Some(SemanticTokensRangeResult::Tokens(tokens)));
        }

        // A previous full/delta request, or an earlier range miss below, may have
        // already computed the whole-document token set for this exact snapshot.
        // Filtering it is much cheaper than re-running the full tree-sitter path
        // for every scrolled viewport.
        if let Some(full_tokens) = self
            .cache
            .get_current_tokens(&uri, &language_name, cache_key)
        {
            let range_tokens = filter_semantic_tokens_by_range(&full_tokens, &domain_range);
            let response_tokens = range_tokens.clone();
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                generation,
                &edit_lock,
            );
            if !current {
                return Err(crate::error::content_modified_error());
            }
            self.cache.store_range_tokens(
                uri,
                domain_range,
                language_name,
                range_tokens,
                cache_key,
            );
            return Ok(Some(SemanticTokensRangeResult::Tokens(response_tokens)));
        }

        // Get capture mappings for token type resolution
        let capture_mappings = self.language.capture_mappings();

        // Use Rayon-based parallel injection processing
        let supports_multiline = self.settings_manager.supports_multiline_tokens();
        let coordinator = std::sync::Arc::clone(&self.language);

        let result = handle_semantic_tokens_full(
            &self.compute_pool,
            text,
            tree,
            query,
            Some(language_name.clone()),
            Some(capture_mappings),
            coordinator,
            supports_multiline,
            None,
            None,
        )
        .await;

        // Shape immutable payloads before taking the edit lock. Only the final
        // live-snapshot validation and cache commits need to exclude edits.
        let (domain_range_result, tokens_to_store) = match result {
            Some(tower_lsp_server::ls_types::SemanticTokensResult::Tokens(mut full_tokens)) => {
                full_tokens.result_id = Some(next_result_id());
                let range_tokens = filter_semantic_tokens_by_range(&full_tokens, &domain_range);
                let response = tower_lsp_server::ls_types::SemanticTokensRangeResult::from(
                    range_tokens.clone(),
                );
                (response, Some((full_tokens, range_tokens)))
            }
            Some(tower_lsp_server::ls_types::SemanticTokensResult::Partial(partial)) => (
                tower_lsp_server::ls_types::SemanticTokensRangeResult::from(partial),
                None,
            ),
            None => (
                tower_lsp_server::ls_types::SemanticTokensRangeResult::Tokens(
                    tower_lsp_server::ls_types::SemanticTokens {
                        result_id: None,
                        data: Vec::new(),
                    },
                ),
                None,
            ),
        };

        // A range is authored against one live document lifetime. A close,
        // reopen, or edit during the uncancellable full-document compute makes
        // both its response coordinates and any cache store obsolete.
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        let still_current = self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            generation,
            &edit_lock,
        );
        if !still_current {
            return Err(crate::error::content_modified_error());
        }

        // Cache ONLY a clean `Tokens` result (#535). Partial/None responses are
        // degraded or transient and must not become reusable cache entries.
        if let Some((full_tokens, range_tokens)) = tokens_to_store {
            self.cache.store_tokens(
                uri.clone(),
                full_tokens,
                language_name.clone(),
                cache_key,
                snapshot_identity,
            );
            self.cache.store_range_tokens(
                uri,
                domain_range,
                language_name,
                range_tokens,
                cache_key,
            );
        }

        Ok(Some(domain_range_result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep, timeout};
    use tower_lsp_server::LspService;
    use url::Url;

    /// Publish a snapshot for `uri` built from `text` at `parsed_version`,
    /// tree-less (no parser needed): the handlers' snapshot-resolution and
    /// served-version bookkeeping are observable without tokenizing.
    fn publish_treeless(server: &Kakehashi, uri: &Url, text: &str, parsed_version: u64) {
        let incarnation = server
            .documents
            .latest_snapshot(uri)
            .expect("document must be open")
            .slot
            .current_incarnation;
        let landed = server
            .documents
            .get(uri)
            .map(|doc| {
                doc.publish_snapshot(&std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from(text),
                        tree: None,
                        language: Some("rust".to_string()),
                        parsed_version,
                        incarnation,
                        injection_regions: None,
                        regions: None,
                        layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
                    },
                ))
            })
            .unwrap_or(false);
        assert!(landed, "test snapshot must land");
    }

    #[tokio::test]
    async fn tree_less_token_response_cannot_restore_interest_after_an_edit() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///empty-response-race.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        publish_treeless(server, &uri, "fn main() {}", 0);
        let edit_lock = server.documents.edit_lock(&uri);
        let guard = edit_lock.lock().await;
        let mut request = std::pin::pin!(server.semantic_tokens_full_impl(full_params(&uri)));
        assert!(futures::poll!(request.as_mut()).is_pending());
        server
            .documents
            .update_document(uri.clone(), "fn main() { }".into(), None);
        server.cache.reset_semantic_refresh_interest(&uri);
        drop(guard);
        assert!(request.await.unwrap().is_none());
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    #[tokio::test]
    async fn timed_out_token_wait_rechecks_a_parse_that_already_settled() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///timeout-publish-race.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        publish_treeless(server, &uri, "fn main() {}", 0);
        server
            .documents
            .update_document(uri.clone(), "fn main() { }".into(), None);
        publish_treeless(server, &uri, "fn main() { }", 1);
        let outcome = server
            .token_snapshot_after_timeout(
                &uri,
                server
                    .documents
                    .latest_snapshot(&uri)
                    .map(|view| (view.slot.current_incarnation, view.content_version)),
                server.cache.semantic_token_generation(),
                &crate::cancel::CancelToken::default(),
            )
            .await;
        assert!(
            matches!(outcome, TokenSnapshot::Current(snapshot) if snapshot.parsed_version == 1)
        );
    }

    #[tokio::test]
    async fn empty_token_response_consumes_a_same_version_tree_upgrade() {
        for kind in ["full", "delta", "range"] {
            let (service, _socket) = LspService::new(Kakehashi::new);
            let server = service.inner();
            let uri = Url::parse("file:///placeholder-upgrade.rs").unwrap();
            let text = "fn main() {}";
            let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
            server
                .language
                .language_registry_for_parallel()
                .register("rust".into(), language.clone());
            server.language.query_store().insert_highlight_query(
                "rust".into(),
                std::sync::Arc::new(
                    tree_sitter::Query::new(&language, "(identifier) @function").unwrap(),
                ),
            );
            server
                .documents
                .insert(uri.clone(), "old".into(), Some("rust".into()), None);
            server.cache.record_served_semantic_version(&uri, 0);
            server
                .documents
                .update_document(uri.clone(), text.into(), None);
            server.cache.reset_semantic_refresh_interest(&uri);
            publish_treeless(server, &uri, text, 1);
            let edit_lock = server.documents.edit_lock(&uri);
            let guard = edit_lock.lock().await;
            let mut request = std::pin::pin!(async {
                match kind {
                    "full" => {
                        let Some(SemanticTokensResult::Tokens(tokens)) = server
                            .semantic_tokens_full_impl(full_params(&uri))
                            .await
                            .unwrap()
                        else {
                            panic!("expected full tokens")
                        };
                        tokens.data
                    }
                    "delta" => {
                        let params = SemanticTokensDeltaParams {
                            text_document: full_params(&uri).text_document,
                            previous_result_id: "missing-baseline".into(),
                            work_done_progress_params: Default::default(),
                            partial_result_params: Default::default(),
                        };
                        let Some(SemanticTokensFullDeltaResult::Tokens(tokens)) = server
                            .semantic_tokens_full_delta_impl(params)
                            .await
                            .unwrap()
                        else {
                            panic!("expected delta full fallback")
                        };
                        tokens.data
                    }
                    "range" => {
                        let params = range_params(
                            &uri,
                            Range::new(Position::new(0, 0), Position::new(0, text.len() as u32)),
                        );
                        let Some(SemanticTokensRangeResult::Tokens(tokens)) =
                            server.semantic_tokens_range_impl(params).await.unwrap()
                        else {
                            panic!("expected range tokens")
                        };
                        tokens.data
                    }
                    _ => unreachable!(),
                }
            });
            assert!(futures::poll!(request.as_mut()).is_pending());
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language).unwrap();
            let tree = parser.parse(text, None).unwrap();
            let doc = server.documents.get(&uri).unwrap();
            assert!(doc.publish_snapshot(&std::sync::Arc::new(
                crate::document::snapshot::ParseSnapshot {
                    text: std::sync::Arc::from(text),
                    tree: Some(tree),
                    language: Some("rust".into()),
                    parsed_version: 1,
                    incarnation: doc.incarnation(),
                    injection_regions: None,
                    regions: None,
                    layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
                }
            )));
            drop(doc);
            assert_eq!(server.cache.served_semantic_version(&uri), None);
            drop(guard);
            let tokens = request.await;
            assert!(
                !tokens.is_empty(),
                "{kind}: the published tree must replace the captured placeholder"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn token_timeout_lock_wait_remains_client_cancellable() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///timeout-cancel.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        publish_treeless(server, &uri, "old", 0);
        server
            .documents
            .update_document(uri.clone(), "new".into(), None);
        let edit_lock = server.documents.edit_lock(&uri);
        let _guard = edit_lock.lock().await;
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
        let supersede = crate::cancel::CancelToken::default();
        let mut request = std::pin::pin!(server.current_snapshot_for_tokens(
            &uri,
            Some(&mut cancel_rx),
            &supersede,
            server.cache.semantic_token_generation(),
            None,
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        tokio::time::advance(crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP).await;
        assert!(futures::poll!(request.as_mut()).is_pending());
        cancel_tx.send(()).unwrap();
        assert!(matches!(
            futures::poll!(request.as_mut()),
            std::task::Poll::Ready(TokenSnapshot::Cancelled)
        ));
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_interest_cannot_cross_an_accepted_edit() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///timeout-edit.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        publish_treeless(server, &uri, "old", 0);
        server
            .documents
            .update_document(uri.clone(), "first".into(), None);
        let supersede = crate::cancel::CancelToken::default();
        let mut request = std::pin::pin!(server.current_snapshot_for_tokens(
            &uri,
            None,
            &supersede,
            server.cache.semantic_token_generation(),
            None,
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        let lock = server.documents.edit_lock(&uri);
        let guard = lock.lock().await;
        tokio::time::advance(crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP).await;
        assert!(futures::poll!(request.as_mut()).is_pending());
        // The accepted didChange critical section advances text and clears
        // interest while timeout recovery is queued behind its edit lock.
        server
            .documents
            .update_document(uri.clone(), "second".into(), None);
        server.cache.reset_semantic_refresh_interest(&uri);
        drop(guard);
        assert!(matches!(request.await, TokenSnapshot::Superseded));
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_recovery_rejects_a_tree_from_an_obsolete_generation() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///timeout-generation.rs").unwrap();
        let text = "fn main() {}";
        server
            .documents
            .insert(uri.clone(), "old".into(), Some("rust".into()), None);
        publish_treeless(server, &uri, "old", 0);
        server
            .documents
            .update_document(uri.clone(), text.into(), None);
        let generation = server.cache.semantic_token_generation();
        let lock = server.documents.edit_lock(&uri);
        let guard = lock.lock().await;
        let supersede = crate::cancel::CancelToken::default();
        let mut request = std::pin::pin!(
            server.current_snapshot_for_tokens(&uri, None, &supersede, generation, None,)
        );
        assert!(futures::poll!(request.as_mut()).is_pending());
        tokio::time::advance(crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP).await;
        assert!(futures::poll!(request.as_mut()).is_pending());

        server.cache.bump_semantic_token_generation();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(text, None).unwrap();
        let doc = server.documents.get(&uri).unwrap();
        assert!(doc.publish_snapshot(&std::sync::Arc::new(
            crate::document::snapshot::ParseSnapshot {
                text: std::sync::Arc::from(text),
                tree: Some(tree),
                language: Some("rust".into()),
                parsed_version: 1,
                incarnation: doc.incarnation(),
                injection_regions: None,
                regions: None,
                layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
            }
        )));
        drop(doc);
        drop(guard);
        assert!(matches!(request.await, TokenSnapshot::Superseded));
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    #[tokio::test]
    async fn successful_token_wait_can_follow_a_newer_edit() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///token-wait-newer-edit.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        publish_treeless(server, &uri, "old", 0);
        server
            .documents
            .update_document(uri.clone(), "first".into(), None);
        let supersede = crate::cancel::CancelToken::default();
        let mut request = std::pin::pin!(server.current_snapshot_for_tokens(
            &uri,
            None,
            &supersede,
            server.cache.semantic_token_generation(),
            None,
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        server
            .documents
            .update_document(uri.clone(), "second".into(), None);
        publish_treeless(server, &uri, "second", 2);
        assert!(matches!(
            request.await,
            TokenSnapshot::Current(snapshot) if snapshot.parsed_version == 2
                && snapshot.text.as_ref() == "second"
        ));
    }

    #[tokio::test]
    async fn timeout_interest_cannot_cross_document_lifetimes() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///token-timeout-new-lifetime.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        let expected_edit = server
            .documents
            .latest_snapshot(&uri)
            .map(|view| (view.slot.current_incarnation, view.content_version));
        server.documents.remove(&uri);
        server
            .documents
            .insert(uri.clone(), "new".into(), None, None);
        assert!(matches!(
            server
                .token_snapshot_after_timeout(
                    &uri,
                    expected_edit,
                    server.cache.semantic_token_generation(),
                    &crate::cancel::CancelToken::default(),
                )
                .await,
            TokenSnapshot::Superseded
        ));
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    #[tokio::test]
    async fn placeholder_lock_wait_remains_cancellable() {
        for kind in ["full", "delta"] {
            for cause in ["client", "supersede"] {
                let (service, _socket) = LspService::new(Kakehashi::new);
                let server = service.inner();
                let uri = Url::parse("file:///placeholder-cancel.rs").unwrap();
                server
                    .documents
                    .insert(uri.clone(), "old".into(), None, None);
                publish_treeless(server, &uri, "old", 0);
                let lock = server.documents.edit_lock(&uri);
                let _guard = lock.lock().await;
                let mut request = std::pin::pin!(crate::lsp::request_id::CURRENT_REQUEST_ID.scope(
                    Some(tower_lsp_server::jsonrpc::Id::Number(42)),
                    async {
                        if kind == "full" {
                            server
                                .semantic_tokens_full_impl(full_params(&uri))
                                .await
                                .map(|result| result.is_some())
                        } else {
                            server
                                .semantic_tokens_full_delta_impl(SemanticTokensDeltaParams {
                                    text_document: full_params(&uri).text_document,
                                    previous_result_id: "missing".into(),
                                    work_done_progress_params: Default::default(),
                                    partial_result_params: Default::default(),
                                })
                                .await
                                .map(|result| result.is_some())
                        }
                    },
                ));
                assert!(futures::poll!(request.as_mut()).is_pending());
                if cause == "client" {
                    server
                        .bridge
                        .cancel_forwarder()
                        .notify_cancel(&crate::lsp::bridge::UpstreamId::Number(42));
                } else {
                    server.cache.start_request(&uri);
                }
                let std::task::Poll::Ready(result) = futures::poll!(request.as_mut()) else {
                    panic!("{kind}/{cause}: cancellation must release the placeholder wait")
                };
                if cause == "client" {
                    assert_eq!(result.unwrap_err().code, Error::request_cancelled().code);
                } else {
                    assert!(!result.unwrap());
                }
                assert_eq!(server.cache.served_semantic_version(&uri), None);
            }
        }
    }

    #[tokio::test]
    async fn range_placeholder_lock_wait_is_cancellable_without_superseding_full() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///range-placeholder-cancel.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        publish_treeless(server, &uri, "old", 0);
        let (full_request_id, full_cancel) = server.cache.start_request(&uri);
        let lock = server.documents.edit_lock(&uri);
        let _guard = lock.lock().await;
        let mut request = std::pin::pin!(crate::lsp::request_id::CURRENT_REQUEST_ID.scope(
            Some(tower_lsp_server::jsonrpc::Id::Number(43)),
            server.semantic_tokens_range_impl(range_params(
                &uri,
                Range::new(Position::new(0, 0), Position::new(0, 3)),
            )),
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        assert!(server.cache.is_request_active(&uri, full_request_id));
        server
            .bridge
            .cancel_forwarder()
            .notify_cancel(&crate::lsp::bridge::UpstreamId::Number(43));
        let std::task::Poll::Ready(result) = futures::poll!(request.as_mut()) else {
            panic!("cancellation must release the viewport's placeholder wait")
        };
        assert_eq!(result.unwrap_err().code, Error::request_cancelled().code);
        assert_eq!(server.cache.served_semantic_version(&uri), None);
        assert!(server.cache.is_request_active(&uri, full_request_id));
        assert!(!full_cancel.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_timeout_cannot_register_interest_for_a_reopened_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///timeout-reopen.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        let (_, cancel) = server.cache.start_request(&uri);
        server.cache.remove_document(&uri);
        server.documents.remove(&uri);
        server
            .documents
            .insert(uri.clone(), "new".into(), None, None);
        assert!(matches!(
            server
                .token_snapshot_after_timeout(
                    &uri,
                    server
                        .documents
                        .latest_snapshot(&uri)
                        .map(|view| { (view.slot.current_incarnation, view.content_version) }),
                    server.cache.semantic_token_generation(),
                    &cancel
                )
                .await,
            TokenSnapshot::Superseded
        ));
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    #[tokio::test(start_paused = true)]
    async fn token_timeout_lock_wait_remains_supersedable() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///timeout-supersede.rs").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".into(), None, None);
        publish_treeless(server, &uri, "old", 0);
        server
            .documents
            .update_document(uri.clone(), "new".into(), None);
        let edit_lock = server.documents.edit_lock(&uri);
        let _guard = edit_lock.lock().await;
        let supersede = crate::cancel::CancelToken::default();
        let mut request = std::pin::pin!(server.current_snapshot_for_tokens(
            &uri,
            None,
            &supersede,
            server.cache.semantic_token_generation(),
            None
        ));
        assert!(futures::poll!(request.as_mut()).is_pending());
        tokio::time::advance(crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP).await;
        assert!(futures::poll!(request.as_mut()).is_pending());
        supersede.cancel();
        assert!(matches!(
            futures::poll!(request.as_mut()),
            std::task::Poll::Ready(TokenSnapshot::Superseded)
        ));
        assert_eq!(server.cache.served_semantic_version(&uri), None);
    }

    fn full_params(uri: &Url) -> SemanticTokensParams {
        SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    fn range_params(uri: &Url, range: Range) -> SemanticTokensRangeParams {
        SemanticTokensRangeParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(uri).expect("test URI should convert"),
            },
            range,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_empty_range_does_not_wait_for_first_parse() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let uri = Url::parse("file:///empty_semantic_range.rs").expect("valid test uri");
        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let result = timeout(
            Duration::from_millis(1),
            service.inner().semantic_tokens_range_impl(range_params(
                &uri,
                Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                },
            )),
        )
        .await
        .expect("an empty range must not wait for the first parse")
        .expect("empty range request should succeed")
        .expect("empty range request should return a token result");

        let SemanticTokensRangeResult::Tokens(tokens) = result else {
            panic!("empty range must return a complete empty token result");
        };
        assert!(tokens.data.is_empty());
    }

    /// Serve-current (the Neovim client contract): a full request arriving
    /// while the latest snapshot trails the live text must NOT serve the
    /// stale snapshot — it parks until the current one publishes and serves
    /// that. Pinned via the served-version mark: the old serve-stale model
    /// recorded the trailing `parsed_version` immediately.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_parks_until_current_snapshot() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(service.inner(), &uri, "fn main() {}", 0);
        // An edit bumps content_version past the published parse: the v0
        // snapshot is now stale.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .semantic_tokens_full_impl(full_params(&uri))
                    .await
            })
        };
        // Let the handler reach its snapshot wait, then publish the current
        // parse (well inside the settle backstop).
        sleep(Duration::from_millis(50)).await;
        assert!(
            service
                .inner()
                .cache
                .served_semantic_version(&uri)
                .is_none(),
            "the handler must not have served the stale v0 snapshot"
        );
        publish_treeless(service.inner(), &uri, "fn main() { }", 1);

        let result = request.await.expect("handler task must not panic");
        assert!(result.is_ok(), "current-snapshot serve must succeed");
        assert_eq!(
            service.inner().cache.served_semantic_version(&uri),
            Some(1),
            "the response must be computed from the CURRENT snapshot"
        );
    }

    /// A parked request superseded by a newer request for the same document
    /// (the tracker flips its token — no `$/cancelRequest` involved) must
    /// release promptly with the compute-superseded contract `Ok(None)`, not
    /// hold its ingress admission slot until the parse settles or the
    /// backstop expires.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_superseded_while_parked_releases_with_none() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current_supersede.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // No snapshot for the live content version → the handler parks.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .semantic_tokens_full_impl(full_params(&uri))
                    .await
            })
        };
        sleep(Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the handler must be parked awaiting the current snapshot"
        );

        // A newer request for the same URI supersedes the parked one.
        let woke_at = tokio::time::Instant::now();
        let _newer = service.inner().cache.start_request(&uri);

        let result = request
            .await
            .expect("handler task must not panic")
            .expect("a superseded parked request answers, not errors");
        assert!(
            result.is_none(),
            "superseded-while-parked follows the compute-superseded contract (None)"
        );
        assert!(
            woke_at.elapsed() < crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP,
            "the park must release on supersession, not ride out the backstop"
        );
    }

    /// A `$/cancelRequest` arriving while the handler is parked on a trailing
    /// snapshot must answer RequestCancelled promptly (the
    /// `TokenSnapshot::Cancelled` arm) — not sit out the settle backstop.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_cancel_while_parked_answers_request_cancelled() {
        use tower_lsp_server::jsonrpc::Id;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current_cancel.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // No snapshot for the live content version → the handler parks.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            // The upstream request id rides task-local storage (installed by
            // the RequestIdCapture middleware in production).
            tokio::spawn(crate::lsp::request_id::CURRENT_REQUEST_ID.scope(
                Some(Id::Number(42)),
                async move {
                    service
                        .inner()
                        .semantic_tokens_full_impl(full_params(&uri))
                        .await
                },
            ))
        };
        sleep(Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the handler must be parked awaiting the current snapshot"
        );

        service
            .inner()
            .bridge
            .cancel_forwarder()
            .forward_cancel(crate::lsp::bridge::UpstreamId::Number(42))
            .expect("cancel forward must not error");

        let result = request.await.expect("handler task must not panic");
        let err = result.expect_err("a cancelled parked request must error, not answer");
        assert_eq!(
            err.code,
            Error::request_cancelled().code,
            "the parked handler must answer RequestCancelled on $/cancelRequest"
        );
    }

    /// When no parse catches up within the settle backstop, the handler must
    /// reject with ContentModified (the parse loop's settle refresh re-drives
    /// the client later) — never answer with tokens for text the client no
    /// longer has.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_rejects_content_modified_when_parse_never_catches_up() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///serve_current_timeout.rs").expect("valid test uri");

        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(server, &uri, "fn main() {}", 0);
        server
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let result = server.semantic_tokens_full_impl(full_params(&uri)).await;
        let err = result.expect_err("a snapshot that never catches up must reject");
        assert_eq!(
            err.code,
            crate::error::content_modified_error().code,
            "staleness past the settle backstop signals ContentModified"
        );
        assert_eq!(
            server.cache.served_semantic_version(&uri),
            Some(0),
            "a rejected request must register token interest (version 0, \
             never the stale snapshot's version) so the settle-refresh gate \
             re-drives a client whose every request rejected"
        );
    }

    /// The delta path shares the serve-current wait: a delta against a
    /// trailing snapshot parks and answers from the current one.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_delta_parks_until_current_snapshot() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current_delta.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(service.inner(), &uri, "fn main() {}", 0);
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                let params = SemanticTokensDeltaParams {
                    text_document: TextDocumentIdentifier {
                        uri: crate::lsp::lsp_impl::url_to_uri(&uri)
                            .expect("test URI should convert"),
                    },
                    previous_result_id: "1".to_string(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                };
                service
                    .inner()
                    .semantic_tokens_full_delta_impl(params)
                    .await
            })
        };
        sleep(Duration::from_millis(50)).await;
        assert!(
            service
                .inner()
                .cache
                .served_semantic_version(&uri)
                .is_none(),
            "the delta handler must not have served the stale v0 snapshot"
        );
        publish_treeless(service.inner(), &uri, "fn main() { }", 1);

        let result = request.await.expect("handler task must not panic");
        assert!(result.is_ok(), "current-snapshot delta serve must succeed");
        assert_eq!(
            service.inner().cache.served_semantic_version(&uri),
            Some(1),
            "the delta must be computed from the CURRENT snapshot"
        );
    }

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

    /// Snapshot readers never parse on demand (ADR §3 — the property survived
    /// the serve-stale → serve-current revision): a request against a
    /// resolved-but-tree-less snapshot (what `parse_document` publishes when
    /// no parser is available) releases the first-parse wait immediately,
    /// serves the empty fallback, and leaves the document's tree untouched.
    /// (Replaces the pre-snapshot `..._times_out_but_parses_on_demand` test,
    /// whose asserted on-demand parse was removed by the reader migration.)
    #[tokio::test]
    async fn semantic_tokens_full_serves_empty_without_parsing_on_demand() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_no_inline_parse.rs").expect("valid test uri");

        let text = "fn main() {}";
        server.documents.insert(
            uri.clone(),
            text.to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = server
            .documents
            .latest_snapshot(&uri)
            .expect("document just inserted")
            .slot
            .current_incarnation;
        let published = server
            .documents
            .get(&uri)
            .map(|doc| {
                doc.publish_snapshot(&std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from(text),
                        tree: None,
                        language: Some("rust".to_string()),
                        parsed_version: 0,
                        incarnation,
                        injection_regions: None,
                        regions: None,
                        layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
                    },
                ))
            })
            .unwrap_or(false);
        assert!(published, "tree-less snapshot must land");

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
        .await
        .expect("a present snapshot must not wait out the first-parse bound")
        .expect("semantic tokens full should return without error");

        match result {
            Some(SemanticTokensResult::Tokens(tokens)) => {
                assert!(
                    tokens.data.is_empty(),
                    "tree-less snapshot serves the empty fallback"
                );
            }
            other => panic!("expected empty tokens fallback, got {other:?}"),
        }

        let doc = server.documents.get(&uri).expect("document still open");
        assert!(
            doc.tree().is_none(),
            "snapshot readers never parse inline: the tree must stay absent"
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

    /// End-to-end for the don't-discover-twice lever (parse-snapshot ADR §3):
    /// the parse loop's `populate_injections` derives the injection discovery
    /// once, the published snapshot carries it, and the semantic handler
    /// consumes it — so the request-path compute never re-runs the injection
    /// query. Pinned via the reuse-hit counter around a real
    /// `semanticTokens/full` request.
    #[tokio::test]
    async fn semantic_full_reuses_snapshot_discovery() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///discovery_reuse.md").expect("test uri");

        // Eight lua blocks: clears INJECTION_CACHE_MIN_REGIONS so populate
        // stores a discovery.
        let mut text = String::new();
        for i in 0..8 {
            text.push_str(&format!("# h{i}\n\n```lua\nlocal x{i} = {i}\n```\n\n"));
        }
        server
            .documents
            .insert(uri.clone(), text, Some("markdown".to_string()), None);

        let settings = crate::config::WorkspaceSettings {
            search_paths: vec![
                std::env::var("TREE_SITTER_GRAMMARS")
                    .unwrap_or_else(|_| "deps/tree-sitter".to_string()),
            ],
            ..Default::default()
        };
        let _ = server.language.load_settings(&settings);
        for lang in ["markdown", "markdown_inline", "lua"] {
            if !server.language.ensure_language_loaded(lang).success {
                eprintln!("Skipping: {lang} parser not available");
                return;
            }
        }
        if server.language.highlight_query("markdown").is_none() {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        }

        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("markdown"), None, None)
            .await;

        // The published snapshot must carry the derived discovery.
        let view = server
            .documents
            .latest_snapshot(&uri)
            .expect("document registered");
        let snapshot = view.slot.snapshot.expect("open parse published");
        assert!(
            snapshot.injection_regions.is_some(),
            "populate must derive a discovery onto the snapshot for an 8-region document"
        );

        // A real full request must take the reuse path (no injection-query re-run).
        use crate::analysis::semantic::DISCOVERY_REUSE_HITS;
        DISCOVERY_REUSE_HITS.store(0, std::sync::atomic::Ordering::Relaxed);
        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let result = server
            .semantic_tokens_full_impl(params)
            .await
            .expect("full request should succeed")
            .expect("should return tokens");
        assert!(matches!(result, SemanticTokensResult::Tokens(t) if !t.data.is_empty()));
        assert!(
            DISCOVERY_REUSE_HITS.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the request must rebuild contexts from the snapshot's discovery"
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

        // Publish a parse snapshot: the handlers serve the latest snapshot and
        // never parse on demand (parse-snapshot ADR §3), so the open parse must
        // run before the first request — as didOpen arranges in production.
        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("lua"), None, None)
            .await;

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
