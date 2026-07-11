use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use tower_lsp_server::Client;
use url::Url;

use crate::lsp::lsp_impl::{Kakehashi, build_notifier};
use crate::lsp::settings_manager::SettingsManager;

/// Everything one populate pass derives for the snapshot it rides on
/// (parse-snapshot ADR §3): all `None` when populate was skipped (raced CAS)
/// or the pool work-unit panicked — readers then fall back to inline
/// resolution for that snapshot.
#[derive(Default)]
struct PopulatedSnapshotRegions {
    discovery: Option<std::sync::Arc<crate::document::DiscoveredInjections>>,
    bridge_regions: Option<(
        u64,
        std::sync::Arc<Vec<crate::document::DiscoveredBridgeRegion>>,
    )>,
    resolved_regions: Option<(
        u64,
        std::sync::Arc<Vec<crate::language::injection::ResolvedInjection>>,
    )>,
}

/// Timeout for compute-pool parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
/// THE SAME constant bounds the injected-layer re-parses (`parse_with_ranges`),
/// so host and injected parses cannot silently drift to different budgets.
const PARSE_TIMEOUT: std::time::Duration = crate::language::injection::NATIVE_PARSE_BUDGET;

/// The awaiter-side backstop for a pooled parse: pool-queue wait (a burst of
/// opens on a small pool can queue parses for a while) plus the in-parse
/// budget, with slack. Deliberately generous: the publish happens in the
/// CALLER after this await, so an awaiter that gives up while its work-unit
/// is still queued silently drops the parse result — the document then
/// serves stale (or empty) until the next edit with nothing to heal it. The
/// pool thread itself is protected by the in-parse abort, not by this.
const PARSE_AWAIT_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);

/// Host-parse with a wall-clock abort — the shared
/// [`parse_with_deadline`](crate::language::injection::parse_with_deadline)
/// primitive under the name the parse-loop call sites use.
///
/// `parse_with_pool`'s `tokio::time::timeout` only abandons the *awaiter*;
/// the in-parse abort is what actually reclaims the bounded-pool thread from
/// a pathological parse (see the primitive's doc).
fn parse_text_with_deadline(
    parser: &mut tree_sitter::Parser,
    text: &str,
    old_tree: Option<&tree_sitter::Tree>,
    deadline: std::time::Instant,
) -> Option<tree_sitter::Tree> {
    crate::language::injection::parse_with_deadline(parser, text, old_tree, deadline)
}

/// The settled+stale gate for the parse loop's `semanticTokens/refresh`
/// emission (its full rationale lives at the call site): emit only when the
/// published parse is still the LIVE content version (settled — mid-burst
/// publishes skip; the newer text's own publish re-evaluates) AND some
/// client's last served tokens predate it (a served-version mark exists and
/// is older — no mark means nobody highlights this document).
fn should_emit_settle_refresh(
    documents: &DocumentStore,
    cache: &CacheCoordinator,
    uri: &Url,
    content_version: u64,
) -> bool {
    let settled = documents
        .get(uri)
        .is_some_and(|doc| doc.content_version() == content_version);
    let client_is_stale = cache
        .served_semantic_version(uri)
        .is_some_and(|served| served < content_version);
    settled && client_is_stale
}

pub(super) struct ParseCoordinatorDeps {
    pub(super) client: Client,
    pub(super) language: std::sync::Arc<LanguageCoordinator>,
    pub(super) parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    pub(super) compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    pub(super) documents: std::sync::Arc<DocumentStore>,
    pub(super) cache: std::sync::Arc<CacheCoordinator>,
    pub(super) settings_manager: std::sync::Arc<SettingsManager>,
    pub(super) auto_install: AutoInstallManager,
    pub(super) bridge: std::sync::Arc<BridgeCoordinator>,
}

pub(crate) struct ParseCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    documents: std::sync::Arc<DocumentStore>,
    cache: std::sync::Arc<CacheCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    auto_install: AutoInstallManager,
    bridge: std::sync::Arc<BridgeCoordinator>,
}

impl ParseCoordinator {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self::from_parts(ParseCoordinatorDeps {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            parser_pool: std::sync::Arc::clone(&server.parser_pool),
            compute_pool: std::sync::Arc::clone(&server.compute_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            auto_install: server.auto_install.clone(),
            bridge: std::sync::Arc::clone(&server.bridge),
        })
    }

    pub(super) fn from_parts(deps: ParseCoordinatorDeps) -> Self {
        Self {
            client: deps.client,
            language: deps.language,
            parser_pool: deps.parser_pool,
            compute_pool: deps.compute_pool,
            documents: deps.documents,
            cache: deps.cache,
            settings_manager: deps.settings_manager,
            auto_install: deps.auto_install,
            bridge: deps.bridge,
        }
    }

    /// Shared parsing orchestration: run parser acquisition + parse logic as one
    /// work-unit on the bounded compute pool, with timeout.
    ///
    /// The caller provides the actual parse logic via `parse_fn`, which receives a
    /// `tree_sitter::Parser`, the work-unit's wall-clock deadline (feed it to
    /// [`parse_text_with_deadline`]), and whether this attempt follows a parser
    /// generation change. Incremental callers must drop old-tree seeds on that
    /// retry because the replacement parser may use a different grammar.
    /// The `parser_pool` sync mutex is acquired **only on the pool thread** (the
    /// parse-snapshot ADR's Stage-1 obligation: a tokio worker must never block on
    /// a mutex a compute thread holds), briefly around acquire and release; the
    /// parse itself runs unlocked. On normal completion the parser returns to the
    /// pool — including after this caller timed out, since the release runs inside
    /// the work-unit. A panicking work-unit loses its parser.
    ///
    /// Returns `None` if:
    /// - No parser is available for the language
    /// - The parse work-unit panicked
    /// - The native parse aborted itself at its `PARSE_TIMEOUT` in-parse
    ///   deadline (anchored at dequeue, via tree-sitter's progress callback),
    ///   so a pathological parse cannot pin a bounded-pool thread past its
    ///   budget (the pool is sized as low as ONE thread on small hosts, where
    ///   a pinned thread would stall every document's tree-CPU)
    /// - The `PARSE_AWAIT_BACKSTOP` awaiter timeout fired (extreme queue
    ///   pressure; the work-unit still self-bounds)
    /// - Settings changed during both the initial parse and its one retry, so
    ///   neither result belongs to the current parser generation
    /// - The closure returned `None`
    pub(crate) async fn parse_with_pool<T, F>(
        &self,
        language_name: &str,
        uri: &Url,
        text_len: usize,
        parse_fn: F,
    ) -> Option<T>
    where
        F: FnMut(tree_sitter::Parser, std::time::Instant, bool) -> (tree_sitter::Parser, Option<T>)
            + Send
            + 'static,
        T: Send + 'static,
    {
        use crate::error::LockResultExt;

        let parser_pool = std::sync::Arc::clone(&self.parser_pool);
        let language_name_owned = language_name.to_string();
        let result = tokio::time::timeout(
            PARSE_AWAIT_BACKSTOP,
            self.compute_pool.run(None, move || {
                let mut parse_fn = parse_fn;
                let mut language_name_owned = language_name_owned;
                for attempt in 0..2 {
                    let (parser, parser_generation) = parser_pool
                        .lock()
                        .recover_poison("ParseCoordinator::parse_with_pool(acquire)")
                        .acquire_versioned(&language_name_owned)?;
                    // The in-parse abort deadline is anchored at DEQUEUE, not
                    // submission: a parse that sat in the pool queue behind other
                    // documents' work still gets its full budget of actual parse
                    // CPU (a submission-anchored deadline let a burst of opens
                    // expire healthy parses in the queue, leaving those documents
                    // tree-less until the next edit). The awaiter above covers
                    // queue + parse with slack, so the result is not dropped.
                    let deadline = std::time::Instant::now() + PARSE_TIMEOUT;
                    let (parser, value) = parse_fn(parser, deadline, attempt != 0);
                    match parser_pool
                        .lock()
                        .recover_poison("ParseCoordinator::parse_with_pool(release)")
                        .release_versioned(language_name_owned, parser, parser_generation)
                    {
                        Ok(()) => return value,
                        Err(stale_language_name) => {
                            language_name_owned = stale_language_name;
                        }
                    }
                }
                None
            }),
        )
        .await;

        match result {
            // Outer Option: None = the work-unit panicked (logged with its
            // payload by the pool). Inner Option: None = no parser available
            // for this language, or the closure itself yielded nothing.
            Ok(Some(value)) => value,
            Ok(None) => None,
            Err(_timeout) => {
                log::warn!(
                    "Parse await backstop hit after {:?} for language '{}' on document {} ({} bytes)",
                    PARSE_AWAIT_BACKSTOP,
                    language_name,
                    uri,
                    text_len
                );
                None
            }
        }
    }

    /// Publish a parse pass's [`ParseSnapshot`](crate::document::snapshot::ParseSnapshot)
    /// into the document's snapshot cell (parse-snapshot ADR §2, Stage-2
    /// dual-write). The cell's own guard (incarnation + strict version) decides
    /// admission; a document a `didClose` already removed simply has no cell to
    /// publish into (its detached cell holds the closed sentinel). Returns
    /// whether the publish landed.
    ///
    /// During the dual-write stage this runs alongside the legacy tree CAS and
    /// may admit a snapshot the legacy CAS rejects — deliberately: a parse of
    /// text an edit has since moved on from is *stale but consistent*, exactly
    /// what serve-stale readers consume, while the legacy tree must stay
    /// current-text-only. No single reader mixes the two sources (cell readers
    /// consume the snapshot's own (text, tree); legacy readers snapshot the
    /// store under one shard Ref), and every parse pass runs the legacy CAS
    /// BEFORE this publish: a legacy-tree reader woken by the publish (the
    /// explicit-action waiters read `doc.snapshot()` after
    /// `wait_for_current_snapshot`) therefore finds the store already
    /// settled — either the tree attached, or a racing edit rejected the CAS,
    /// in which case the cell is not Current for that reader either. The
    /// legacy store and its remaining readers go away in Stage 3.
    fn publish_parse_snapshot(
        &self,
        uri: &Url,
        snapshot: crate::document::snapshot::ParseSnapshot,
    ) -> bool {
        let (version, incarnation) = (snapshot.parsed_version, snapshot.incarnation);
        let landed = self
            .documents
            .get(uri)
            .map(|doc| doc.publish_snapshot(std::sync::Arc::new(snapshot)))
            .unwrap_or(false);
        if !landed {
            log::debug!(
                target: "kakehashi::snapshot",
                "publish rejected for {uri}: v{version} inc{incarnation}"
            );
        }
        landed
    }

    /// Run `CacheCoordinator::populate_injections` as a compute-pool work-unit
    /// and await it.
    ///
    /// The injection walk (injection-query execution + per-region ULID mint +
    /// content hash) is O(regions) synchronous tree-CPU — hundreds of ms on an
    /// injection-heavy document — and previously ran inline on a tokio worker
    /// right after the parse, starving the runtime (parse-snapshot ADR, Context).
    /// It is **awaited**, not detached, preserving the `populate → mark finished
    /// → downstream` ordering the injection-map invalidation depends on (Stage-1
    /// obligation). All parameters are cheap clones (refcount bumps).
    /// Returns everything the populate pass derived from its single injection
    /// query — the semantic discovery and the bridge-downstream regions — both
    /// destined for the snapshot this pass publishes (ADR §3,
    /// don't-discover-twice). `(None, None)` when the work-unit panicked.
    async fn populate_injections_on_pool(
        &self,
        uri: Url,
        text: std::sync::Arc<str>,
        tree: tree_sitter::Tree,
        language_name: String,
        incarnation: u64,
        content_version: u64,
    ) -> PopulatedSnapshotRegions {
        let cache = std::sync::Arc::clone(&self.cache);
        let language = std::sync::Arc::clone(&self.language);
        let tracker = self.bridge.node_tracker_arc();
        // Latch + at-mint validity gate, taken UNDER this document's edit
        // lock so the pair is atomic against `did_change` (which holds the
        // same lock across its tracker edit-shift AND its
        // `content_version` bump — the ADR's "only the fast tracker-mint
        // runs under edit_lock" obligation). Lock-free latch-then-validate
        // is NOT enough here: didChange shifts the tracker BEFORE it bumps
        // the version, so a latch taken after the shift with a version read
        // before the bump would look current on both counts and let this
        // pass mint its old-tree coordinates into the shifted index as
        // correct-at-birth. Under the lock, the gate checks:
        // - liveness + lifetime (a didClose that ran to COMPLETION leaves
        //   the tracker at `(0, epoch+1)` — indistinguishable from a
        //   reopen's first mint, so the latch alone cannot refuse it; the
        //   reopen case fails the incarnation check);
        // - currency (`content_version` unchanged since the parse's CAS —
        //   an edit that already landed makes this pass's tree stale).
        // Anything landing AFTER the lock drops is caught by the latch
        // re-check inside the batch mint / commit (`cleanup` bumps the
        // epoch before it removes; an edit-shift bumps the generation).
        // Skipping populate matches the stale/closed outcome everywhere
        // else: the snapshot (if it still publishes) rides without regions.
        let entry_mint_epoch = {
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let latch = tracker.mint_epoch(&uri);
            let latest = self.documents.latest_snapshot(&uri);
            let valid = latest.as_ref().is_some_and(|view| {
                view.slot.current_incarnation == incarnation
                    && view.content_version == content_version
            });
            if !valid {
                // The edit_lock() accessor above materializes a lock entry
                // even for a document a didClose already removed — drop it
                // so raced closes can't grow the map (the did_change stray
                // rule). Identity- AND share-checked: a reopen racing this
                // probe may already be reusing this very entry (or have
                // installed a new one) for the new lifetime's edits, and
                // removing it from under a queued edit would let the next
                // edit mint a fresh mutex and run concurrently.
                if latest.is_none() {
                    self.documents
                        .remove_edit_lock_if_unshared(&uri, &edit_lock);
                }
                return PopulatedSnapshotRegions::default();
            }
            latch
        };
        // Coarse per-parse gate: with no runnable bridge server configured,
        // the bridge-region build (per-region content copies) and fully
        // resolved downstream regions are pure waste on the pre-publish
        // critical path. `None` on the snapshot makes a bridge configured by
        // a later reload fall back to inline resolution.
        let build_bridge_regions = self
            .settings_manager
            .load_settings()
            .any_bridge_server_runnable();
        self.compute_pool
            .run(None, move || {
                // A refused pass (`None`) maps to all-`None` region fields —
                // the snapshot then rides WITHOUT regions and readers fall
                // back to inline resolution. Mapping it to the ran-and-empty
                // shape instead would publish "no injections" for a pass
                // that never derived anything, blanking the document's
                // injections until the next parse.
                let Some(populated) = cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &language,
                    &tracker,
                    entry_mint_epoch,
                    build_bridge_regions,
                    build_bridge_regions,
                ) else {
                    return PopulatedSnapshotRegions::default();
                };
                PopulatedSnapshotRegions {
                    discovery: populated.discovery.map(std::sync::Arc::new),
                    bridge_regions: populated
                        .bridge_regions
                        .map(|regions| (populated.generation, std::sync::Arc::new(regions))),
                    resolved_regions: populated
                        .resolved_regions
                        .map(|regions| (populated.generation, std::sync::Arc::new(regions))),
                }
            })
            .await
            .unwrap_or_default()
    }

    /// Parse the (already-registered) document at `uri` and publish the result.
    ///
    /// The registering `didOpen` inserts the document — **with its text** — before
    /// calling this, so the parse re-reads that stored text (a cheap `Arc<str>`
    /// refcount bump, [`text_arc`](crate::document::Document::text_arc)) rather than
    /// carrying a second owned `String`, and records the detected language + tree
    /// **in place** via the non-inserting, text + incarnation CAS
    /// [`set_parse_result_if_text_and_incarnation_unchanged`] instead of re-inserting a
    /// fresh copy of the text. Net: zero full-document text copies in the open parse.
    /// That store write is **non-inserting** and lifetime-guarded, so it is
    /// resurrection-safe and stale-safe once the open parse moves off the ingress
    /// ticket: a `didClose` racing it stays closed, and a `didChange` / reopen landing
    /// mid-parse drops the now-stale tree rather than clobbering the newer state.
    ///
    /// `ticket` is the ingress writer ticket of the mutation that scheduled this
    /// parse, or `None` for a caller outside the ingress sequence. On every resolution
    /// path that still observes this lifetime — a tree, a parsed-to-nothing, a
    /// previously-crashed parser, or no detectable language — the parse advances the
    /// store's per-document **watermark** to `ticket` (guarded by the open
    /// incarnation), releasing a reader waiting on it. The one path that does **not**
    /// advance is a document already gone (a `didClose` removed it): its watermark
    /// channel is gone too, so its readers have already fallen back.
    ///
    /// Returns `true` iff **this** call's CAS landed a tree (i.e. it is the parse
    /// whose tree is now current). The off-ingress open caller gates its
    /// tree-dependent downstream (`process_injections(forward=false)`, the deferred
    /// refresh, the synthetic diagnostic) on this — **not** on "the document has a
    /// tree": a `didChange` racing this parse can move the text on and let the edit
    /// reparse attach the newer tree (and run `process_injections(forward=true)`)
    /// first; this parse's CAS then rejects, and re-checking `tree().is_some()` would
    /// wrongly see the edit's tree and re-run the *open* downstream over it,
    /// superseding the edit's eager-open batch. Gating on the own-CAS result is the
    /// same discipline `reparse_latest` follows for its `populate_injections`.
    pub(crate) async fn parse_document(
        &self,
        uri: Url,
        language_id: Option<&str>,
        ticket: Option<u64>,
    ) -> bool {
        let mut events = Vec::new();

        // Read the text the registering didOpen already stored (a refcount bump, not
        // a copy), together with the open lifetime's **incarnation** — BEFORE marking
        // the parse started, so a document a `didClose` already removed leaves neither
        // a resurrected document nor an orphan parse-state entry for the now-closed
        // URI. A missing document stops **without** touching the watermark: the
        // watermark is per-lifetime, so a plain advance with this prior-lifetime
        // ticket could inflate a reopen's freshly-seeded channel and prematurely
        // release a new-lifetime reader; a genuine close instead drops the channel and
        // wakes its readers (they fall back). Unreachable while this parse is inline on
        // the writer ticket (a `didClose` is gated behind the open); the guard is for
        // the off-ingress open flip (#6), where a `didClose`/reopen can race it.
        let Some((text, incarnation, content_version)) = self
            .documents
            .get(&uri)
            .map(|doc| (doc.text_arc(), doc.incarnation(), doc.content_version()))
        else {
            return false;
        };

        // Publish the watermark on whichever path resolves the parse below, but
        // **only if this lifetime is still current**: a close + reopen re-seeds the
        // watermark at 0, and this (prior-lifetime) ticket must not inflate it. Same
        // lifetime → advances (releasing a gated reader even on the no-language /
        // no-tree paths, to the empty fallback). Mirrors `reparse_latest`.
        let advance_watermark = || {
            if let Some(ticket) = ticket {
                self.documents
                    .advance_watermark_for_incarnation(&uri, ticket, incarnation);
            }
        };

        let parse_generation = self.documents.mark_parse_started(&uri);

        let language_name = self
            .language
            .detect_language(uri.path(), &text, None, language_id);

        if let Some(language_name) = language_name {
            if self.auto_install.is_parser_failed(&language_name) {
                log::warn!(
                    target: "kakehashi::crash_recovery",
                    "Skipping parsing for '{}' - parser previously crashed",
                    language_name
                );
                // Mark the parse finished only if the result actually landed: a
                // `didClose` that removed the document mid-parse (or a `didChange` /
                // reopen that moved the text or incarnation on) makes the CAS a no-op,
                // and `mark_parse_finished` would otherwise recreate a parse-state
                // entry (via `parse_sender`'s vacant insert) for the gone URI. The
                // text + incarnation guard is what makes this resurrection-safe once
                // the open parse runs off the ingress ticket (#6).
                if self
                    .documents
                    .set_parse_result_if_text_and_incarnation_unchanged(
                        &uri,
                        &text,
                        incarnation,
                        Some(&language_name),
                        None,
                    )
                {
                    self.documents
                        .mark_parse_finished(&uri, parse_generation, false);
                }
                // Resolved-but-tree-less snapshot (ADR §2): the parse completed
                // with no usable tree; publishing it advances parsed_version so
                // a first-parse waiter releases to its empty fallback.
                self.publish_parse_snapshot(
                    &uri,
                    crate::document::snapshot::ParseSnapshot {
                        text: text.clone(),
                        tree: None,
                        language: Some(language_name.clone()),
                        parsed_version: content_version,
                        incarnation,
                        injection_regions: None,
                        bridge_regions: None,
                        resolved_regions: None,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                );
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                // No tree landed (the parser previously crashed): the open caller
                // must not run its tree-dependent downstream.
                return false;
            }

            let load_result = self
                .language
                .ensure_language_loaded_async(&language_name)
                .await;
            events.extend(load_result.events);

            // This is the document-open parse: there is no prior tree to seed an
            // incremental parse from, so it is always a full parse. (The off-ingress
            // edit reparse — `reparse_latest` — is the incremental path, seeded from
            // `Document::pending_seed`.) A full parse is also the only safe option
            // without an edited old tree: reusing an unedited tree against different
            // text violates tree-sitter's incremental contract and corrupts external
            // scanners (#348).
            let text_for_parse = text.clone();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();

            let parsed_tree = if load_result.success {
                self.parse_with_pool(
                    &language_name,
                    &uri,
                    text.len(),
                    move |mut parser, deadline, _generation_retry| {
                        let _ = auto_install.begin_parsing(&language_name_clone);
                        let parse_result =
                            parse_text_with_deadline(&mut parser, &text_for_parse, None, deadline);
                        let _ = auto_install.end_parsing(&language_name_clone);
                        (parser, parse_result)
                    },
                )
                .await
            } else {
                None
            };

            if let Some(tree) = parsed_tree {
                // Legacy tree CAS BEFORE the snapshot publish: a legacy-store
                // reader woken by the publish (the explicit-action waiters
                // read `doc.snapshot()` after `wait_for_current_snapshot`)
                // must find the tree already attached — publish-first opened
                // a window where the cell said Current while the legacy store
                // was still tree-less, silently no-opping a user-triggered
                // formatting. The CAS is non-inserting (text + incarnation
                // checked), so a tree parsed from open-time text/lifetime is
                // dropped when a `didChange` moved the text on or a
                // `didClose`/reopen changed the incarnation — the publish
                // below still lands in that case (stale-but-consistent is
                // exactly what serve-stale readers consume; the cell guard
                // independently rejects out-of-order versions). Only populate
                // the injection caches when the tree actually landed, so a
                // `didClose` racing this off-ingress parse can't leave stale
                // injection entries for a gone document. (`Tree` clone is a
                // cheap refcount bump.)
                let stored = self
                    .documents
                    .set_parse_result_if_text_and_incarnation_unchanged(
                        &uri,
                        &text,
                        incarnation,
                        Some(&language_name),
                        Some(tree.clone()),
                    );
                // Populate BEFORE the publish so the derived discovery rides the
                // snapshot (ADR §3 don't-discover-twice); readers keep serving
                // the previous snapshot meanwhile. A rejected CAS (raced) skips
                // populate — the snapshot then publishes without discovery and
                // readers discover inline for that (already-superseded) snapshot.
                let regions = if stored {
                    self.populate_injections_on_pool(
                        uri.clone(),
                        text.clone(),
                        tree.clone(),
                        language_name.clone(),
                        incarnation,
                        content_version,
                    )
                    .await
                } else {
                    PopulatedSnapshotRegions::default()
                };
                self.publish_parse_snapshot(
                    &uri,
                    crate::document::snapshot::ParseSnapshot {
                        text: text.clone(),
                        tree: Some(tree.clone()),
                        language: Some(language_name.clone()),
                        parsed_version: content_version,
                        incarnation,
                        injection_regions: regions.discovery,
                        bridge_regions: regions.bridge_regions,
                        resolved_regions: regions.resolved_regions,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                );
                if stored {
                    // AFTER the publish: a downstream task woken by this mark
                    // on another runtime thread must find the snapshot (and
                    // its fast-path regions) already in the cell — marking
                    // first let it read the previous snapshot and fall back
                    // to inline resolution for one cycle.
                    self.documents
                        .mark_parse_finished(&uri, parse_generation, true);
                }
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                // `stored` is exactly "this call's CAS landed the tree": false when a
                // racing `didChange`/reopen moved the text or incarnation on and the
                // edit reparse won, in which case the open downstream must NOT re-run
                // over the edit's tree.
                return stored;
            }

            // Parse produced no tree (timeout / parser unavailable / join error) but
            // the language WAS detected — record it with no tree, rather than falling
            // through to the no-language path below which would null it out. Host
            // bridging needs only text + language (never a tree), so preserving the
            // language keeps a host-bridged document working after a parse failure.
            if self
                .documents
                .set_parse_result_if_text_and_incarnation_unchanged(
                    &uri,
                    &text,
                    incarnation,
                    Some(&language_name),
                    None,
                )
            {
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
            }
            self.publish_parse_snapshot(
                &uri,
                crate::document::snapshot::ParseSnapshot {
                    text: text.clone(),
                    tree: None,
                    language: Some(language_name.clone()),
                    parsed_version: content_version,
                    incarnation,
                    injection_regions: None,
                    bridge_regions: None,
                    resolved_regions: None,
                    layer_trees: std::sync::OnceLock::new(),
                },
            );
            advance_watermark();
            self.notifier().log_language_events(&events).await;
            return false;
        }

        // No language detected at all → store no language, no tree.
        if self
            .documents
            .set_parse_result_if_text_and_incarnation_unchanged(
                &uri,
                &text,
                incarnation,
                None,
                None,
            )
        {
            self.documents
                .mark_parse_finished(&uri, parse_generation, false);
        }
        self.publish_parse_snapshot(
            &uri,
            crate::document::snapshot::ParseSnapshot {
                text: text.clone(),
                tree: None,
                language: None,
                parsed_version: content_version,
                incarnation,
                injection_regions: None,
                bridge_regions: None,
                resolved_regions: None,
                layer_trees: std::sync::OnceLock::new(),
            },
        );
        advance_watermark();
        self.notifier().log_language_events(&events).await;
        false
    }

    /// Re-parse a document after its parser finished installing, **off the
    /// ingress path** and **resurrection-safely**.
    ///
    /// Called from the spawned auto-install task (see `did_open`), so by the time
    /// it runs the originating `didOpen` writer ticket has already completed.
    /// Unlike [`parse_document`](Self::parse_document) it:
    ///
    /// - re-reads the **latest** store text rather than the open-time text (a
    ///   `didChange` may have landed while the install ran), and
    /// - persists through the **non-inserting**, tree-absent `attach_tree_if_absent`
    ///   CAS, so a `didClose` during the install leaves the document gone instead
    ///   of resurrecting it (the install/parse resurrection vector the actor ADR
    ///   calls out), and a `didChange` between the read and the write drops the
    ///   now-stale tree.
    ///
    /// No watermark advance: the originating `didOpen`'s skip-parse branch already
    /// resolved that ticket's watermark, and this reparse carries no ticket.
    ///
    /// Because the install is now off-ingress, a `didChange` can run *concurrently*
    /// with this reparse (it is no longer gated behind the install). A `didChange`
    /// that lands while the parser is still loading stores its new text with **no
    /// tree** (the parser wasn't available), and would then CAS-reject this
    /// reparse's now-stale tree — leaving the document tree-less. To converge, this
    /// re-reads the latest text and retries a bounded number of times until the
    /// tree lands (or another parse wins). Sustained editing falls back to the
    /// reader's on-demand parse; the parse actor replaces this with a proper
    /// coalescing loop.
    pub(crate) async fn reparse_installed_document(&self, uri: Url, language_id: Option<String>) {
        /// Bound on the convergence retries (a burst of edits landing exactly as
        /// the install completes); past this the reader on-demand parse covers it.
        const MAX_REPARSE_ATTEMPTS: usize = 8;

        // Resolve the language under one read guard, short-circuiting if the
        // document is gone (a `didClose` ran during the install — do not
        // resurrect it) or already parsed (a concurrent parse won — nothing to do,
        // and skip the `ensure_language_loaded` work). Detection borrows the stored
        // text (synchronous, no `.await` and no document write under the `Ref`).
        // Capture the grammar **and** the (language_id, incarnation) it is resolved
        // for, together under one read guard. `language_name` is fixed for the whole
        // loop, so the CAS must check against the language_id/incarnation captured
        // *here* — not re-read per attempt. Otherwise a relabelling reopen mid-loop
        // would have its new language_id captured per attempt, satisfy the CAS's
        // language check, and let a tree parsed by the *old* grammar attach to the
        // relabelled document. The incarnation is likewise lifetime-stable; only the
        // text legitimately changes within a lifetime (a `didChange`), so only the
        // text is re-read per attempt.
        let (language_name, expected_language_id, expected_incarnation) = {
            let Some(doc) = self.documents.get(&uri) else {
                return;
            };
            if doc.tree().is_some() {
                return;
            }
            let language_name =
                self.language
                    .detect_language(uri.path(), doc.text(), None, language_id.as_deref());
            (
                language_name,
                doc.language_id().map(|s| s.to_string()),
                doc.incarnation(),
            )
        };
        let Some(language_name) = language_name else {
            // Give-up: release a parked first-parse waiter (bootstrap-gated).
            self.documents.publish_giveup_snapshot(&uri);
            return;
        };
        if self.auto_install.is_parser_failed(&language_name) {
            self.documents.publish_giveup_snapshot(&uri);
            return;
        }
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
        let mut events = load_result.events;
        if !load_result.success {
            self.documents.publish_giveup_snapshot(&uri);
            self.notifier().log_language_events(&events).await;
            return;
        }

        for _ in 0..MAX_REPARSE_ATTEMPTS {
            // Re-read the latest text each attempt. Gone => closed (no resurrect);
            // already has a tree => a concurrent parse won; a changed incarnation =>
            // a close+reopen, whose new lifetime drives its own parse — stop rather
            // than parse its text with this lifetime's (possibly relabelled-away)
            // grammar (the CAS would reject it anyway; this just avoids the wasted
            // parses).
            let (text, content_version) = {
                let Some(doc) = self.documents.get(&uri) else {
                    break;
                };
                if doc.tree().is_some() {
                    break;
                }
                if doc.incarnation() != expected_incarnation {
                    break;
                }
                // `text_arc()` is a refcount bump, not a full copy (#498) — the
                // original stays here for the CAS while a cheap clone goes to the
                // blocking parse closure.
                (doc.text_arc(), doc.content_version())
            };

            let text_len = text.len();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();
            // Hand a cheap `Arc<str>` clone (refcount bump) to the blocking closure;
            // the original stays here for the CAS below, so the (potentially large)
            // document text is never copied.
            let text_for_parse = text.clone();
            let parsed = self
                .parse_with_pool(
                    &language_name,
                    &uri,
                    text_len,
                    move |mut parser, deadline, _generation_retry| {
                        let _ = auto_install.begin_parsing(&language_name_clone);
                        let result =
                            parse_text_with_deadline(&mut parser, &text_for_parse, None, deadline);
                        let _ = auto_install.end_parsing(&language_name_clone);
                        (parser, result)
                    },
                )
                .await;

            let Some(tree) = parsed else { break };

            // Persist FIRST through the non-inserting, tree-absent CAS —
            // before the snapshot publish, so a legacy-store reader the
            // publish wakes finds the tree already attached (see the
            // parse_document ordering note). A closed (Vacant) document, one
            // whose text moved (a concurrent `didChange`), or one a
            // concurrent parse already gave a tree all drop this tree — the
            // tree-absent check makes the "don't clobber a concurrent parse"
            // guard atomic with the write. Only populate the injection caches
            // when the tree actually landed, so a `didClose` racing this
            // reparse can't leave stale injection entries for a gone
            // document. (`Tree` clone is a cheap refcount bump.)
            let stored = self.documents.attach_tree_if_absent(
                &uri,
                &text,
                expected_language_id.as_deref(),
                expected_incarnation,
                tree.clone(),
            );
            // Populate BEFORE the publish so the discovery rides the snapshot
            // (ADR §3); the cell guard still rejects a stale lifetime or an
            // out-of-order version at publish. A rejected CAS skips populate —
            // the snapshot still publishes as stale-but-consistent.
            let regions = if stored {
                self.populate_injections_on_pool(
                    uri.clone(),
                    text.clone(),
                    tree.clone(),
                    language_name.clone(),
                    expected_incarnation,
                    content_version,
                )
                .await
            } else {
                PopulatedSnapshotRegions::default()
            };
            let published = self.publish_parse_snapshot(
                &uri,
                crate::document::snapshot::ParseSnapshot {
                    text: text.clone(),
                    tree: Some(tree.clone()),
                    language: Some(language_name.clone()),
                    parsed_version: content_version,
                    incarnation: expected_incarnation,
                    injection_regions: regions.discovery,
                    bridge_regions: regions.bridge_regions,
                    resolved_regions: regions.resolved_regions,
                    layer_trees: std::sync::OnceLock::new(),
                },
            );
            // Serve-stale's heal signal, mirroring reparse_latest: a token
            // request answered empty (or 15s-capped) while the install was
            // still compiling has no lineage to re-drive it — without the
            // refresh, a slow install leaves the document unhighlighted
            // until an incidental edit.
            if published {
                events.push(crate::language::LanguageEvent::semantic_tokens_refresh(
                    language_name.clone(),
                ));
            }
            if stored {
                break;
            }
            // CAS rejected: the text moved under us (a concurrent `didChange`).
            // Loop to re-read the latest text and try again.
        }

        // Covers the give-up exits of the retry loop (parser still
        // unavailable after the install, exhausted attempts): a no-op after
        // a successful publish (bootstrap gate), otherwise it releases a
        // parked first-parse waiter.
        self.documents.publish_giveup_snapshot(&uri);
        self.notifier().log_language_events(&events).await;
    }

    /// Re-parse `uri`'s **latest** store text off the ingress path, for the
    /// per-document parse scheduler (`Kakehashi::schedule_reparse`).
    ///
    /// `did_change` clears the reader-visible tree synchronously and schedules this;
    /// it runs in a spawned loop, *not* on the writer ticket. When the edit stashed a
    /// `pending_seed` (the pre-edit tree with this edit's `InputEdit`s applied) the
    /// parse is **incremental**, seeded from it; a full-text sync stashes no seed and
    /// parses from scratch (which keeps #348 closed). The tree write
    /// is the non-inserting text **and language** CAS
    /// [`update_tree_if_text_and_language_unchanged`]: a closed (Vacant) document is
    /// left gone (resurrection-safe), a text that moved on (a `didChange` landed
    /// while parsing) is dropped — the scheduler's `dirty` loop then reparses the
    /// newer text — and a reopen that changed the language is rejected (no
    /// wrong-grammar tree). On **every** resolution path the parse
    /// advances the store watermark to `ticket`, so a virt/native reader gated
    /// behind the originating edit is released once its parse resolved.
    ///
    /// The semantic-token `full/delta` path is unaffected by the off-ingress move:
    /// it diffs cached token arrays by `result_id` (never `changed_ranges`), so as
    /// long as the seed keeps this reparse cheap the delta stays cheap too.
    pub(crate) async fn reparse_latest(&self, uri: &Url, ticket: Option<u64>) {
        // Re-read the latest text + detect the language under one read guard. A
        // missing document means a `didClose` ran — stop without touching the
        // watermark (no resurrection). Advancing it here would be unsafe: the
        // watermark is per-lifetime, so if a reopen has *already* re-seeded a fresh
        // channel, a plain advance with this prior-lifetime ticket would inflate it
        // and prematurely release a new-lifetime reader — and it is also
        // unnecessary, since a genuine close drops the channel and wakes its readers
        // (they fall back). The incarnation isn't known on this path, but it isn't
        // needed: only the post-read paths below (which captured it) advance, and
        // they gate on it. `language_id` is captured so the tree write can reject a
        // reopen that changed the language while this parse was in flight. The
        // `pending_seed` (a cheap `Tree` refcount-clone) is the edit's incremental
        // parse seed stashed by `didChange`; `None` for a full-text sync / freshly
        // installed parse, in which case we parse from scratch.
        let (language_name, language_id, text, seed, incarnation, content_version) = {
            let Some(doc) = self.documents.get(uri) else {
                return;
            };
            // `text_arc()` is a refcount bump, not a full copy of the document text
            // (#498) — cheap on this reparse hot path.
            let text = doc.text_arc();
            let language_id = doc.language_id().map(|s| s.to_string());
            let seed = doc.pending_seed().cloned();
            // The lifetime this parse is for: a close+reopen before the tree write
            // changes it, and the CAS below rejects on the mismatch (so a tree from
            // this lifetime never attaches to a reopened document).
            let incarnation = doc.incarnation();
            let language_name =
                self.language
                    .detect_language(uri.path(), &text, None, language_id.as_deref());
            (
                language_name,
                language_id,
                text,
                seed,
                incarnation,
                doc.content_version(),
            )
        };

        // Post-read resolutions advance the watermark **only if this lifetime is
        // still current** — a close+reopen re-seeds the watermark at 0, and this
        // (prior-lifetime) ticket must not inflate it and prematurely release a
        // new-lifetime reader. Same lifetime → advances (releasing readers even on
        // the no-language / no-tree paths, to the empty fallback).
        let advance_watermark = || {
            if let Some(ticket) = ticket {
                self.documents
                    .advance_watermark_for_incarnation(uri, ticket, incarnation);
            }
        };

        let Some(language_name) = language_name else {
            // Give-up: release a parked first-parse waiter with a tree-less
            // snapshot (bootstrap-gated inside) rather than letting every
            // request burn the full first-parse backstop.
            self.documents.publish_giveup_snapshot(uri);
            advance_watermark();
            return;
        };
        if self.auto_install.is_parser_failed(&language_name) {
            self.documents.publish_giveup_snapshot(uri);
            advance_watermark();
            return;
        }
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
        if !load_result.success {
            self.documents.publish_giveup_snapshot(uri);
            advance_watermark();
            self.notifier()
                .log_language_events(&load_result.events)
                .await;
            return;
        }

        let text_len = text.len();
        let auto_install = self.auto_install.clone();
        let language_name_clone = language_name.clone();
        // Hand a cheap `Arc<str>` clone (refcount bump) to the blocking closure; the
        // original stays here for the CAS + injection populate below. The seed (also
        // a cheap `Tree` refcount-clone) makes this an **incremental** parse when an
        // edit stashed one: tree-sitter reuses the unchanged subtrees and reparses
        // only the edited region. `None` (full-text sync / install) parses from
        // scratch. The seed already has this edit's `InputEdit`s applied
        // (`didChange` → `apply_edit_and_seed`), satisfying tree-sitter's contract.
        let text_for_parse = text.clone();
        let parsed = self
            .parse_with_pool(
                &language_name,
                uri,
                text_len,
                move |mut parser, deadline, generation_retry| {
                    let _ = auto_install.begin_parsing(&language_name_clone);
                    let result = parse_text_with_deadline(
                        &mut parser,
                        &text_for_parse,
                        if generation_retry {
                            None
                        } else {
                            seed.as_ref()
                        },
                        deadline,
                    );
                    let _ = auto_install.end_parsing(&language_name_clone);
                    (parser, result)
                },
            )
            .await;

        let mut events = load_result.events;
        if let Some(tree) = parsed {
            // Legacy tree CAS BEFORE the snapshot publish (see the
            // parse_document ordering note): a legacy-store reader woken by
            // the publish must find the tree already attached. Text +
            // language + incarnation checked atomically under the tree-write
            // shard lock: text rejects a within-lifetime stale parse (a
            // `didChange` landed mid-parse); language rejects a reopen that
            // relabelled the URI; incarnation rejects a same-language,
            // identical-text reopen — the tree belongs to the prior lifetime
            // and must not attach to the reopened document (nor let the
            // watermark advance below run on the old lifetime's ticket).
            let stored = self.documents.update_tree_if_text_and_language_unchanged(
                uri,
                &text,
                language_id.as_deref(),
                incarnation,
                tree.clone(),
            );
            // Populate BEFORE the publish so the derived discovery rides the
            // snapshot (ADR §3, don't-discover-twice); readers keep serving the
            // previous snapshot for populate's duration. A rejected legacy CAS
            // (the text moved on mid-parse) skips populate — the snapshot still
            // publishes as stale-but-consistent (its readers discover inline;
            // the scheduler's dirty loop is already reparsing the newer text).
            let regions = if stored {
                self.populate_injections_on_pool(
                    uri.clone(),
                    text.clone(),
                    tree.clone(),
                    language_name.clone(),
                    incarnation,
                    content_version,
                )
                .await
            } else {
                PopulatedSnapshotRegions::default()
            };
            let published = self.publish_parse_snapshot(
                uri,
                crate::document::snapshot::ParseSnapshot {
                    text: text.clone(),
                    tree: Some(tree.clone()),
                    language: Some(language_name.clone()),
                    parsed_version: content_version,
                    incarnation,
                    injection_regions: regions.discovery,
                    bridge_regions: regions.bridge_regions,
                    resolved_regions: regions.resolved_regions,
                    layer_trees: std::sync::OnceLock::new(),
                },
            );
            // Serve-stale's heal signal (ADR §3), narrowed to the cases the
            // workspace-scoped request is actually FOR. `refresh` is expensive
            // for the client (Neovim's handler cancels its in-flight token
            // request and re-tokenizes every attached buffer), and clients
            // already re-request per didChange — so a publish emits it only
            // when ALL of:
            // - the publish landed (a rejected publish emits nothing);
            // - the document has SETTLED (this parse's version is still the
            //   live content_version — during a typing burst the scheduler is
            //   already reparsing newer text, whose own publish re-evaluates);
            // - some client actually consumes this document's semantic tokens
            //   (a served-version mark exists) AND its last served tokens
            //   predate this snapshot (otherwise its own didChange-driven
            //   request already caught up).
            // Net: at most one refresh per settle, none mid-burst, none for
            // documents nobody highlights. Emitted from the parse loop, never
            // didChange (synchronous clients can't answer a server request
            // mid-notification).
            if published
                && should_emit_settle_refresh(&self.documents, &self.cache, uri, content_version)
            {
                events.push(crate::language::LanguageEvent::semantic_tokens_refresh(
                    language_name.clone(),
                ));
            }
        }

        // Covers the parse-produced-no-tree path (timeout / parser
        // unavailable): a no-op after a successful publish (bootstrap gate),
        // otherwise it releases a parked first-parse waiter.
        self.documents.publish_giveup_snapshot(uri);
        advance_watermark();
        self.notifier().log_language_events(&events).await;
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four documented invariants of the settle-refresh gate.
    #[test]
    fn settle_refresh_gate_emits_only_for_settled_and_stale() {
        let documents = DocumentStore::new();
        let cache = CacheCoordinator::new();
        let uri = url::Url::parse("file:///settle_gate.rs").unwrap();
        documents.insert(uri.clone(), "a".into(), Some("rust".into()), None);
        // content_version == 0 now.

        // No served mark: nobody highlights this document -> no refresh.
        assert!(!should_emit_settle_refresh(&documents, &cache, &uri, 0));

        // Client already served THIS version -> its didChange-driven request
        // caught up -> no refresh.
        cache.record_served_semantic_version(&uri, 0);
        assert!(!should_emit_settle_refresh(&documents, &cache, &uri, 0));

        // An edit bumps the live version; the publish for v1 finds the client
        // stale (served 0 < 1) and the document settled (live == 1) -> emit.
        documents.update_document(uri.clone(), "ab".into(), None);
        assert!(should_emit_settle_refresh(&documents, &cache, &uri, 1));

        // Mid-burst: another edit already moved the live version past this
        // publish (live 2, publish v1) -> not settled -> no refresh (the v2
        // publish re-evaluates).
        documents.update_document(uri.clone(), "abc".into(), None);
        assert!(!should_emit_settle_refresh(&documents, &cache, &uri, 1));

        // The mark is monotonic: a stale serve cannot regress it.
        cache.record_served_semantic_version(&uri, 2);
        cache.record_served_semantic_version(&uri, 1);
        assert!(!should_emit_settle_refresh(&documents, &cache, &uri, 2));
    }

    /// The deadline must actually abort the native parse (an expired one
    /// yields `None` fast) and must not poison the parser for reuse.
    #[test]
    fn parse_text_with_deadline_aborts_when_expired_and_parses_within_it() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        // Large enough that tree-sitter fires the progress callback at least
        // once before completing.
        let text = "fn f() { let x = 1 + 2 * 3; }\n".repeat(4000);

        let expired = std::time::Instant::now();
        let started = std::time::Instant::now();
        let aborted = parse_text_with_deadline(&mut parser, &text, None, expired);
        assert!(aborted.is_none(), "an expired deadline aborts the parse");
        assert!(
            started.elapsed() < PARSE_TIMEOUT,
            "the abort happens in-parse, not after the full parse"
        );

        // The reset parser is immediately reusable on the same input.
        let future = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let parsed = parse_text_with_deadline(&mut parser, &text, None, future);
        assert!(parsed.is_some(), "a live deadline parses normally");
    }
}
