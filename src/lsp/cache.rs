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

use tower_lsp_server::ls_types::SemanticTokens;
use tree_sitter::Tree;
use url::Url;

use crate::analysis::{
    InjectionMap, InjectionTokenCache, SemanticSnapshotIdentity, SemanticTokenCache,
    SemanticTokenRangeCache,
};
use crate::language::LanguageCoordinator;
use crate::language::NodeTracker;
use crate::language::injection::{CacheableInjectionRegion, collect_all_injections};

use super::semantic_request_tracker::{SemanticRequestScope, SemanticRequestTracker};

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
    /// The highest snapshot `parsed_version` whose semantic tokens were
    /// actually SERVED to the client, per document. The parse loop consults
    /// this to decide whether a fresh publish needs a
    /// `workspace/semanticTokens/refresh`: the request is workspace-scoped and
    /// clients (Neovim) already re-request per `didChange`, so a refresh is
    /// warranted only when the document has settled and the client's last
    /// served tokens predate the settled snapshot — one refresh per settle,
    /// none during a typing burst, none for documents whose tokens no client
    /// ever asked for.
    served_semantic_versions: dashmap::DashMap<Url, u64>,
}

/// Everything one `populate_injections` pass derives from its single
/// injection-query run (parse-snapshot ADR §3, never discover twice): the
/// semantic-path discovery (gated) and the bridge-downstream region list
/// (ungated). Both ride the `ParseSnapshot` the parse publishes.
pub(crate) struct PopulatedInjections {
    pub(crate) discovery: Option<crate::document::DiscoveredInjections>,
    /// `None` when the build was skipped (no bridge server configured) —
    /// bridge readers then fall back to inline resolution — vs `Some(empty)`
    /// for "ran, nothing matched" (readers skip their work).
    pub(crate) bridge_regions: Option<Vec<crate::document::DiscoveredBridgeRegion>>,
    /// `None` when fully resolved regions were intentionally skipped on the
    /// parse critical path; readers then fall back to inline resolution.
    pub(crate) resolved_regions: Option<Vec<crate::language::injection::ResolvedInjection>>,
    /// The settings generation this populate pass ran under — stamped onto
    /// the snapshot's `resolved_regions` so reload-stale resolution is never
    /// served (see `ParseSnapshot::resolved_regions`).
    pub(crate) generation: u64,
}

impl PopulatedInjections {
    /// No regions (no injection query, or none matched): the downstream skips
    /// its work, exactly as the inline resolution's empty result did.
    pub(crate) fn empty(generation: u64) -> Self {
        Self {
            discovery: None,
            bridge_regions: Some(Vec::new()),
            resolved_regions: Some(Vec::new()),
            generation,
        }
    }
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
            served_semantic_versions: dashmap::DashMap::new(),
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
        // Cancel any in-flight compute for this URI FIRST, so it has the best
        // chance to observe the flipped token and bail before its injection
        // store-half runs — otherwise a compute already past its last checkpoint
        // could repopulate the caches we clear just below, orphaning entries for
        // a now-closed document. This narrows but does not fully close that
        // window (a compute already inside the store-half still writes); the
        // residual is the same deferred per-document lifecycle-epoch race as the
        // rest of didClose (see `did_close.rs`), and only leaks a bounded set of
        // entries for a closed doc that no request can read.
        self.request_tracker.cancel_all_for_uri(uri);
        self.served_semantic_versions.remove(uri);
        self.semantic_cache.remove(uri);
        self.semantic_range_cache.remove(uri);
        self.injection_map.clear(uri);
        self.injection_token_cache.clear_document(uri);
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
    ///
    /// Returns the owned injection **discovery** for this parse (parse-snapshot
    /// ADR §3, the don't-discover-twice lever) when it should ride the
    /// [`ParseSnapshot`](crate::document::snapshot::ParseSnapshot) for readers
    /// to reuse — `None` when there is nothing worth reusing (no query,
    /// no/empty regions, below the region-count gate, an `injection.combined`
    /// group, or a not-yet-loaded parser). The discovery query (`Q`) is run
    /// **once** here and shared; the request path never re-runs it.
    ///
    /// `entry_mint_epoch` is the tracker latch ([`NodeTracker::mint_epoch`])
    /// the CALLER captured **before** validating the pass's
    /// liveness/lifetime, and every tracker-mutating step below re-checks it
    /// (latch-then-validate): capturing it here, after the caller's check,
    /// would let a close+reopen completing between the two `mint_epoch`
    /// reads produce a `(0, new_epoch)` latch that matches the reopened
    /// index and mints this pass's old-lifetime coordinates into it.
    ///
    /// Returns `None` when the latch REFUSED the pass (at the batch mint or
    /// the final commit) — the pass was outrun and produced nothing, so the
    /// snapshot must ride with `None` region fields and every reader falls
    /// back to inline resolution. This is distinct from
    /// `Some(PopulatedInjections::empty(..))`, which means the pass RAN and
    /// found no injections — consumers then skip injection work outright.
    /// Conflating the two would let a refused pass (e.g. any document's
    /// close bumping the global cleanup epoch mid-populate) publish "no
    /// injections" and blank the document's injections until the next
    /// parse.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn populate_injections(
        &self,
        uri: &Url,
        text: &str,
        tree: &Tree,
        language_name: &str,
        language: &LanguageCoordinator,
        tracker: &NodeTracker,
        entry_mint_epoch: (u64, u64),
        incarnation: u64,
        build_bridge_regions: bool,
        build_resolved_regions: bool,
    ) -> Option<PopulatedInjections> {
        // Snapshot the generation FIRST — before reading the injection query or
        // resolving any language below — so the stamp can never be *newer* than
        // the queries the discovery was built with. A reload swaps queries and
        // then bumps the generation (`lsp_impl::apply_shared_settings`); reading
        // the generation last would let a reload landing mid-`populate` stamp a
        // stale-query region set with the *new* generation, which the consumer's
        // generation gate would then wrongly accept. Read first, a raced
        // discovery carries the *old* generation and the post-reload consumer
        // re-discovers inline — the "snapshot generation at the top" discipline
        // the token handlers use.
        let generation = self.semantic_token_generation();
        // Entry half of the mint reconciliation (parse-snapshot ADR §3):
        // populate runs post-CAS but an edit can land DURING this pass and
        // shift the live-coordinate NodeTracker, making this pass's tree
        // coordinates stale. `entry_mint_epoch` (latched by the caller,
        // BEFORE its liveness validation — see the doc comment) is
        // re-checked inside every tracker-mutating step below (the batch
        // mint and the commit), so a stale pass mints nothing and clobbers
        // nothing — identity defers to the next current pass.

        // Get the injection query for this language
        let injection_query = match language.injection_query(language_name) {
            Some(q) => q,
            None => {
                // No injection query = no injections to track. Clear any
                // stale injection caches — but only when no edit landed
                // during this pass (a stale pass must not clobber state the
                // newer text's own populate maintains). A refused clear
                // aborts the pass (`None`): reporting ran-and-empty while
                // the old entries survive would leave them unreclaimed
                // until another parse.
                tracker.commit_if_unshifted(uri, entry_mint_epoch, incarnation, || {
                    self.injection_map.clear(uri);
                    self.injection_token_cache.clear_document(uri);
                })?;
                return Some(PopulatedInjections::empty(generation));
            }
        };

        // Collect all injection regions from the parsed tree
        if let Some(regions) =
            collect_all_injections(&tree.root_node(), text, Some(injection_query.as_ref()))
        {
            if regions.is_empty() {
                // Clear any existing regions and caches for this document —
                // epoch-gated like the commit below, and aborting (`None`)
                // on refusal like it too: ran-and-empty must only be
                // reported after the clear actually landed.
                tracker.commit_if_unshifted(uri, entry_mint_epoch, incarnation, || {
                    self.injection_map.clear(uri);
                    self.injection_token_cache.clear_document(uri);
                })?;
                return Some(PopulatedInjections::empty(generation));
            }

            // Mint reconciliation step (parse-snapshot ADR §3): map the
            // pass's region geometry to tracker ULIDs in ONE latch-gated
            // batch — reusing an existing id by position, minting a fresh
            // one for a genuinely new region — instead of an inline
            // per-region mint. The latch check runs under the tracker
            // entry's exclusive lock (the lock the didChange edit-shift also
            // takes), so a passing batch is correct-at-birth and needs no
            // purge set; a refused batch minted NOTHING. Refusal means an
            // edit outran this pass: withhold every region-id-bearing
            // product (readers fall back inline, byte-identically to the
            // pre-lever path) and defer identity to the next current pass —
            // the tracker is meanwhile kept live by the ordinary didChange
            // edit-shift.
            let region_keys = regions
                .iter()
                .map(|info| {
                    (
                        info.content_node.start_byte(),
                        info.content_node.end_byte(),
                        info.content_node.kind(),
                        info.pattern_index,
                        info.language.as_str(),
                    )
                })
                .collect::<Vec<_>>();
            let region_ids = tracker.mint_named_batch_if_unshifted_for_incarnation(
                uri,
                entry_mint_epoch,
                incarnation,
                crate::language::injection::REGION_IDENTITY_LAYER_BASE,
                region_keys,
            )?;

            // Convert to CacheableInjectionRegion pairing each region with
            // its reconciled ULID (index-aligned with `regions` by the batch
            // contract).
            let cacheable_regions: Vec<CacheableInjectionRegion> = regions
                .iter()
                .zip(&region_ids)
                .map(|(info, ulid)| {
                    let region_id = ulid.to_string();
                    CacheableInjectionRegion::from_region_info(info, &region_id, text)
                })
                .collect();

            // Resolve once for every consumer that needs resolved language /
            // virtual content. In production the bridge and whole-document
            // gates rise together, so resolving independently would duplicate
            // the language-detection chain on the parse critical path.
            let resolved = (build_bridge_regions || build_resolved_regions).then(|| {
                crate::language::injection::InjectionResolver::resolve_from_prebuilt(
                    language,
                    &regions,
                    &cacheable_regions,
                    text,
                )
            });

            // Bridge-downstream regions from the SAME collected `regions`
            // (parse-snapshot ADR §3, never discover twice): the exact
            // (resolved language, region_id, clean content) triple
            // `resolve_injection_data` used to re-derive by re-running the
            // injection query per downstream pass. Gated on a bridge server
            // actually being configured: populate runs on the pre-publish
            // critical path, and the per-region content copies are pure
            // waste for the (common) bridge-less deployment — `None` makes
            // any late-configured bridge fall back to inline resolution.
            let bridge_regions: Option<Vec<crate::document::DiscoveredBridgeRegion>> =
                build_bridge_regions.then(|| {
                    resolved
                        .as_ref()
                        .expect("bridge regions requested resolution")
                        .iter()
                        .map(|region| crate::document::DiscoveredBridgeRegion {
                            language: region.injection_language.clone(),
                            region_id: region.region.region_id.clone(),
                            content: region.virtual_content.clone(),
                        })
                        .collect()
                });

            // The whole-document readers' fully resolved regions, from the
            // same single query run — and from the SAME per-region ids and
            // content hashes already in `cacheable_regions` (no duplicate
            // mint/hash on this critical path).
            let resolved_regions = build_resolved_regions
                .then(|| resolved.expect("whole-document regions requested resolution"));

            // Producer half of the discovery lever: build the owned discovery
            // from the SAME `regions` just collected — the injection query (`Q`)
            // is not re-run — so a semanticTokens request bound to the snapshot
            // this parse publishes can rebuild its contexts without
            // re-discovering. `None` when not worth reusing
            // (gate/combined/incomplete).
            let discovery = crate::analysis::semantic::build_document_discovery(
                &regions,
                &cacheable_regions,
                injection_query.as_ref(),
                text,
                language,
                uri,
                tracker,
                generation,
                incarnation,
            );

            // Live-hash set for the content-addressed injection-token cache's
            // eviction sweep, taken from the DISCOVERY's own per-region cache
            // identities — the exact fold and language resolution the
            // store/read path uses (deriving it from any other resolver would
            // silently evict live entries whenever the two resolvers
            // disagree). A PARTIAL discovery (combined group present) still
            // carries every single region's identity, so the sweep keeps the
            // entries the inline path stores for those docs. `None` discovery
            // (below the gate — the same singles count that gates the inline
            // store path) yields an empty set: the token cache is inactive
            // there, so sweeping everything for the document is the correct
            // bound.
            let live_hashes: std::collections::HashSet<u64> = discovery
                .as_ref()
                .map(|d| {
                    d.regions
                        .iter()
                        .filter_map(|r| r.token_cache.as_ref().map(|tc| tc.validity_hash))
                        .collect()
                })
                .unwrap_or_default();

            // Exit half of the mint reconciliation, atomic with coordinate
            // shifts: the commit runs under the tracker entry's exclusive
            // lock and only while the (shift generation, cleanup epoch)
            // latch still matches — an edit (or close/reopen) that landed
            // during this pass fails the check INSIDE the lock, so a shift
            // can never interleave between check and commit. On mismatch,
            // keep the injection map's previous entry (the newer text's own
            // populate replaces it) and withhold the region-id-bearing
            // products; the snapshot then rides without regions and every
            // reader falls back inline, byte-identically to the pre-lever
            // path. No tracker purge is needed: the batch mint above was
            // latch-gated too, so every id this pass minted was
            // correct-at-birth — the intervening edit shifts it like any
            // live entry. The token-cache sweep runs inside the commit
            // closure, so a stale pass performs no eviction at all.
            let committed = tracker.commit_if_unshifted(uri, entry_mint_epoch, incarnation, || {
                // Commit point: record the committed pass's region set (test
                // observability today; no production reader since token
                // eviction went content-addressed — a plain move), and run
                // the content-addressed token cache's eviction sweep with the
                // same epoch gate — a stale pass must not sweep entries the
                // LIVE text's regions still hit (a wrongly swept entry is
                // only a recompute, but a gratuitous one).
                self.injection_map.insert(uri.clone(), cacheable_regions);
                self.injection_token_cache
                    .retain_document(uri, &live_hashes);
            });
            committed?;
            return Some(PopulatedInjections {
                discovery,
                bridge_regions,
                resolved_regions,
                generation,
            });
        }

        Some(PopulatedInjections::empty(generation))
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
    ) -> Option<std::sync::Arc<SemanticTokens>> {
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

        result
    }

    /// Log diagnostic information for cache misses. Gated on the target's
    /// debug level so the miss path — the steady-state case for any request
    /// whose `previousResultId` has already advanced — skips the cache
    /// lookup and clone entirely when debug logs aren't emitted.
    fn log_cache_miss(&self, uri: &Url, expected_result_id: &str) {
        if !log::log_enabled!(target: "kakehashi::semantic_cache", log::Level::Debug) {
            return;
        }
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

    /// Record that a semantic-token response computed from snapshot
    /// `parsed_version` was served for `uri` (monotonic max — a stale-serve
    /// racing a fresher one must not regress the mark).
    pub(crate) fn record_served_semantic_version(&self, uri: &Url, parsed_version: u64) {
        // `get_mut` first: the hot path (every token serve) avoids the `Url`
        // clone `entry()` needs for its owned key.
        if let Some(mut served) = self.served_semantic_versions.get_mut(uri) {
            *served = (*served).max(parsed_version);
            return;
        }
        self.served_semantic_versions
            .entry(uri.clone())
            .and_modify(|v| *v = (*v).max(parsed_version))
            .or_insert(parsed_version);
    }

    /// The last snapshot version whose tokens were served for `uri`, or `None`
    /// when no client ever consumed semantic tokens for it.
    pub(crate) fn served_semantic_version(&self, uri: &Url) -> Option<u64> {
        self.served_semantic_versions.get(uri).map(|v| *v)
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

    /// Store semantic tokens for a document, tagged with the resolved `language`
    /// and the `cache_key` they were computed under so an unchanged-document
    /// repeat request (same language, text, and settings) can skip re-tokenizing
    /// via [`get_current_tokens`](Self::get_current_tokens).
    pub(crate) fn store_tokens(
        &self,
        uri: Url,
        tokens: SemanticTokens,
        language: String,
        cache_key: u64,
        snapshot: SemanticSnapshotIdentity,
    ) {
        self.semantic_cache
            .store(uri, tokens, language, cache_key, snapshot);
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
    ) -> Option<std::sync::Arc<SemanticTokens>> {
        self.semantic_range_cache
            .get_if_current(uri, range, language, cache_key)
    }

    /// Return the cached tokens iff they were computed for this `language` under
    /// this `cache_key` (same text and settings generation), letting the caller
    /// skip the full re-tokenization. None on a language switch, a text edit, a
    /// settings reload, or no entry.
    pub(crate) fn get_current_tokens(
        &self,
        uri: &Url,
        language: &str,
        cache_key: u64,
    ) -> Option<std::sync::Arc<SemanticTokens>> {
        self.semantic_cache.get_if_current(uri, language, cache_key)
    }

    /// Return cached full tokens for the exact parse snapshot and settings
    /// generation before rebuilding the full-document content hash key.
    pub(crate) fn get_current_tokens_for_snapshot(
        &self,
        uri: &Url,
        language: &str,
        snapshot: SemanticSnapshotIdentity,
    ) -> Option<std::sync::Arc<SemanticTokens>> {
        self.semantic_cache
            .get_if_same_snapshot(uri, language, snapshot)
    }

    /// Bump the settings generation on a settings/query reload so every cached
    /// token (computed under the old queries) stops matching, and clear the map
    /// to reclaim the now-dead entries. The bump — not the clear — is what makes
    /// this race-safe against a request that stores tokens after the clear.
    pub(crate) fn bump_semantic_token_generation(&self) -> u64 {
        let generation = self
            .semantic_token_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.semantic_cache.clear();
        // The injection token cache folds the same generation into its key, so
        // old-generation entries are already unreachable after the bump; clear
        // them too so a reload doesn't leak per-region entries until each
        // document closes.
        self.injection_token_cache.clear();
        // The range cache folds the same generation into its key, so the bump
        // already invalidates it; clear to reclaim the dead entries (#535).
        self.semantic_range_cache.clear();
        generation
    }

    // ========================================================================
    // Request tracking (semantic_tokens.rs)
    // ========================================================================

    /// Start tracking a new request for the given URI.
    ///
    /// Returns the request ID (for `is_request_active` checkpoints) and a
    /// revision [`CancelToken`](crate::cancel::CancelToken). Same-scope
    /// consumers coexist; only a newer scope supersedes the prior consumers.
    pub(crate) fn start_request(
        &self,
        uri: &Url,
        scope: SemanticRequestScope,
    ) -> (RequestId, crate::cancel::CancelToken) {
        self.request_tracker.start_request(uri, scope)
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

    pub(crate) fn finish_absent_request(
        &self,
        uri: &Url,
        request_id: RequestId,
        scope: SemanticRequestScope,
    ) {
        self.request_tracker
            .finish_absent_request(uri, request_id, scope);
    }

    /// Cancel all requests for a given URI (test helper).
    #[cfg(test)]
    pub(crate) fn cancel_requests(&self, uri: &Url) {
        self.request_tracker.cancel_all_for_uri(uri);
        self.served_semantic_versions.remove(uri);
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

    fn request_scope(version: u64) -> SemanticRequestScope {
        SemanticRequestScope {
            incarnation: 1,
            generation: 0,
            content_version: version,
        }
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
        cache.store_tokens(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0,
            SemanticSnapshotIdentity {
                parsed_version: 0,
                incarnation: 0,
                generation: 0,
            },
        );

        // Start a request
        let (_request_id, _token) = cache.start_request(&uri, request_scope(1));

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
        cache.store_tokens(
            uri.clone(),
            tokens.clone(),
            "rust".to_string(),
            key_before,
            SemanticSnapshotIdentity {
                parsed_version: 0,
                incarnation: 0,
                generation: gen_before,
            },
        );
        assert!(cache.get_current_tokens(&uri, "rust", key_before).is_some());

        // A settings/query reload bumps the generation (and clears the map).
        cache.bump_semantic_token_generation();

        // Race: a request that began tokenizing under the OLD queries (it captured
        // `key_before`) stores its now-stale tokens AFTER the reload's clear.
        cache.store_tokens(
            uri.clone(),
            tokens,
            "rust".to_string(),
            key_before,
            SemanticSnapshotIdentity {
                parsed_version: 0,
                incarnation: 0,
                generation: gen_before,
            },
        );

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
            cache.get_current_tokens(&uri, "rust", key_after).is_none(),
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
        cache.store_tokens(
            uri.clone(),
            tokens,
            "rust".to_string(),
            0,
            SemanticSnapshotIdentity {
                parsed_version: 0,
                incarnation: 0,
                generation: 0,
            },
        );

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
        let (req1, _t1) = cache.start_request(&uri, request_scope(1));
        assert!(cache.is_request_active(&uri, req1));

        // Start another request - should supersede the first
        let (req2, _t2) = cache.start_request(&uri, request_scope(2));
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
        let (req, _token) = cache.start_request(&uri, request_scope(1));
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
        let _ = cache.populate_injections(
            &uri,
            initial_text,
            &tree,
            "markdown",
            &coordinator,
            &tracker,
            tracker.mint_epoch(&uri),
            0,
            true,
            true,
        );

        // Verify we have one injection region
        let regions = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions.len(), 1);
        let initial_region_id = regions[0].region_id.clone();
        let initial_content_hash = regions[0].content_hash;
        assert_eq!(regions[0].language, "lua");

        // Store region-local tokens under the region's LIVE validity hash (the
        // content ⊕ resolved-language fold the production path uses; with no
        // lua parser registered the resolver falls back to the raw
        // identifier, deterministically).
        let initial_validity =
            crate::analysis::semantic_cache::region_validity_hash(initial_content_hash, "lua");
        cache
            .injection_token_cache
            .store(&uri, initial_validity, 0, injection_raw_tokens());

        // Verify tokens are stored
        assert!(
            cache
                .injection_token_cache
                .get(&uri, initial_validity, 0)
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
        let _ = cache.populate_injections(
            &uri,
            edited_text,
            &edited_tree,
            "markdown",
            &coordinator,
            &tracker,
            tracker.mint_epoch(&uri),
            0,
            true,
            true,
        );

        // The region still exists, but changing its dynamic language creates a
        // different virtual-document layer at the same host position. Region
        // identity includes that language discriminator so simultaneous
        // same-range languages cannot alias; the old ID must not be reused.
        let regions_after = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions_after.len(), 1);
        assert_ne!(
            regions_after[0].region_id, initial_region_id,
            "a language change must remint the virtual-document identity"
        );
        assert_eq!(
            regions_after[0].language, "python",
            "language should be updated to python"
        );

        // CRITICAL ASSERTION: the old-language entry must be gone. Content
        // addressing gives this twice over: the read path now looks up the
        // python fold (a natural miss), and populate's eviction sweep drops
        // the lua-fold entry because it is no longer in the live hash set.
        assert!(
            cache
                .injection_token_cache
                .get(&uri, initial_validity, 0)
                .is_none(),
            "the old-language entry must be swept when the language changes"
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
        let _ = cache.populate_injections(
            &uri,
            initial_text,
            &tree,
            "markdown",
            &coordinator,
            &tracker,
            tracker.mint_epoch(&uri),
            0,
            true,
            true,
        );

        let regions = cache.get_injections(&uri).expect("should have injections");
        assert_eq!(regions.len(), 1);
        let initial_region_id = regions[0].region_id.clone();
        let initial_content_hash = regions[0].content_hash;

        // Store region-local tokens under the region's LIVE validity hash.
        let initial_validity =
            crate::analysis::semantic_cache::region_validity_hash(initial_content_hash, "lua");
        cache
            .injection_token_cache
            .store(&uri, initial_validity, 0, injection_raw_tokens());

        assert!(
            cache
                .injection_token_cache
                .get(&uri, initial_validity, 0)
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
        let _ = cache.populate_injections(
            &uri,
            edited_text,
            &edited_tree,
            "markdown",
            &coordinator,
            &tracker,
            tracker.mint_epoch(&uri),
            0,
            true,
            true,
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

        // The old-content entry must be swept: its hash left the live set
        // when the region's content changed (the read path also misses
        // naturally, keyed by the NEW content).
        assert!(
            cache
                .injection_token_cache
                .get(&uri, initial_validity, 0)
                .is_none(),
            "the old-content entry must be swept when the content changes"
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
