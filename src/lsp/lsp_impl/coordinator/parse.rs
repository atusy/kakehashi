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

/// Timeout for spawn_blocking parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) struct ParseCoordinatorDeps {
    pub(super) client: Client,
    pub(super) language: std::sync::Arc<LanguageCoordinator>,
    pub(super) parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
    pub(super) documents: std::sync::Arc<DocumentStore>,
    pub(super) cache: std::sync::Arc<CacheCoordinator>,
    pub(super) settings_manager: std::sync::Arc<SettingsManager>,
    pub(super) auto_install: AutoInstallManager,
    pub(super) bridge: std::sync::Arc<BridgeCoordinator>,
}

pub(crate) struct ParseCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
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
            documents: deps.documents,
            cache: deps.cache,
            settings_manager: deps.settings_manager,
            auto_install: deps.auto_install,
            bridge: deps.bridge,
        }
    }

    /// Shared parsing orchestration: acquire parser from pool, run parse logic in
    /// `spawn_blocking` with timeout, release parser back to pool.
    ///
    /// The caller provides the actual parse logic via `parse_fn`, which receives a
    /// `tree_sitter::Parser` and must return it along with an optional result.
    /// On normal completion, this ensures the parser is returned to the pool.
    /// The parser is not returned if the blocking task times out (it keeps
    /// running) or if the task fails or is cancelled and yields a `JoinError`.
    ///
    /// Returns `None` if:
    /// - No parser is available for the language
    /// - The parse task panicked or was cancelled (JoinError; parser not returned)
    /// - The parse timed out after `PARSE_TIMEOUT` (parser not returned)
    /// - The closure returned `None`
    pub(crate) async fn parse_with_pool<T, F>(
        &self,
        language_name: &str,
        uri: &Url,
        text_len: usize,
        parse_fn: F,
    ) -> Option<T>
    where
        F: FnOnce(tree_sitter::Parser) -> (tree_sitter::Parser, Option<T>) + Send + 'static,
        T: Send + 'static,
    {
        let parser = {
            let mut pool = self.parser_pool.lock().await;
            pool.acquire(language_name)
        };

        let parser = parser?;

        let result = tokio::time::timeout(
            PARSE_TIMEOUT,
            tokio::task::spawn_blocking(move || parse_fn(parser)),
        )
        .await;

        match result {
            Ok(Ok((parser, value))) => {
                let mut pool = self.parser_pool.lock().await;
                pool.release(language_name.to_string(), parser);
                value
            }
            Ok(Err(join_error)) => {
                if join_error.is_panic() {
                    log::error!(
                        "Parse task panicked for language '{}' on document {}: {}",
                        language_name,
                        uri,
                        join_error
                    );
                } else {
                    log::warn!(
                        "Parse task was cancelled for language '{}' on document {}: {}",
                        language_name,
                        uri,
                        join_error
                    );
                }
                None
            }
            Err(_timeout) => {
                log::warn!(
                    "Parse timeout after {:?} for language '{}' on document {} ({} bytes)",
                    PARSE_TIMEOUT,
                    language_name,
                    uri,
                    text_len
                );
                None
            }
        }
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
        let Some((text, incarnation)) = self
            .documents
            .get(&uri)
            .map(|doc| (doc.text_arc(), doc.incarnation()))
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
                    .is_some()
                {
                    self.documents
                        .mark_parse_finished(&uri, parse_generation, false);
                }
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                // No tree landed (the parser previously crashed): the open caller
                // must not run its tree-dependent downstream.
                return false;
            }

            let load_result = self.language.ensure_language_loaded(&language_name);
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

            let parsed_tree = self
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    let _ = auto_install.begin_parsing(&language_name_clone);
                    let parse_result = parser.parse(&*text_for_parse, None);
                    let _ = auto_install.end_parsing(&language_name_clone);
                    (parser, parse_result)
                })
                .await;

            if let Some(tree) = parsed_tree {
                // Persist FIRST through the text + incarnation CAS (non-inserting), so
                // a tree parsed from open-time text/lifetime is dropped when a
                // `didChange` moved the text on or a `didClose`/reopen changed the
                // incarnation — instead of clobbering the newer state. Only populate
                // the injection caches when the tree actually landed, so a `didClose`
                // racing this off-ingress parse can't leave stale injection entries for
                // a gone document. (`Tree` clone is a cheap refcount bump.)
                let stored = self
                    .documents
                    .set_parse_result_if_text_and_incarnation_unchanged(
                        &uri,
                        &text,
                        incarnation,
                        Some(&language_name),
                        Some(tree.clone()),
                    );
                if let Some(parse_epoch) = stored {
                    let discovery = self.cache.populate_injections(
                        &uri,
                        &text,
                        &tree,
                        &language_name,
                        &self.language,
                        self.bridge.node_tracker(),
                    );
                    // Attach the owned discovery to this exact tree (#529): the
                    // epoch/incarnation-guarded write-back no-ops if a later parse
                    // already moved the tree on, so a stale discovery is never served.
                    if let Some(discovery) = discovery {
                        self.documents.set_injections_if_epoch_unchanged(
                            &uri,
                            incarnation,
                            parse_epoch,
                            discovery,
                        );
                    }
                    self.documents
                        .mark_parse_finished(&uri, parse_generation, true);
                }
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                // `stored.is_some()` is exactly "this call's CAS landed the tree":
                // `None` when a racing `didChange`/reopen moved the text or incarnation
                // on and the edit reparse won, in which case the open downstream must
                // NOT re-run over the edit's tree.
                return stored.is_some();
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
                .is_some()
            {
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
            }
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
            .is_some()
        {
            self.documents
                .mark_parse_finished(&uri, parse_generation, false);
        }
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
            return;
        };
        if self.auto_install.is_parser_failed(&language_name) {
            return;
        }
        let load_result = self.language.ensure_language_loaded(&language_name);
        let events = load_result.events;

        for _ in 0..MAX_REPARSE_ATTEMPTS {
            // Re-read the latest text each attempt. Gone => closed (no resurrect);
            // already has a tree => a concurrent parse won; a changed incarnation =>
            // a close+reopen, whose new lifetime drives its own parse — stop rather
            // than parse its text with this lifetime's (possibly relabelled-away)
            // grammar (the CAS would reject it anyway; this just avoids the wasted
            // parses).
            let text = {
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
                doc.text_arc()
            };

            let text_len = text.len();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();
            // Hand a cheap `Arc<str>` clone (refcount bump) to the blocking closure;
            // the original stays here for the CAS below, so the (potentially large)
            // document text is never copied.
            let text_for_parse = text.clone();
            let parsed = self
                .parse_with_pool(&language_name, &uri, text_len, move |mut parser| {
                    let _ = auto_install.begin_parsing(&language_name_clone);
                    let result = parser.parse(&*text_for_parse, None);
                    let _ = auto_install.end_parsing(&language_name_clone);
                    (parser, result)
                })
                .await;

            let Some(tree) = parsed else { break };

            // Persist FIRST through the non-inserting, tree-absent CAS: a closed
            // (Vacant) document, one whose text moved (a concurrent `didChange`),
            // or one a concurrent parse already gave a tree all drop this tree —
            // the tree-absent check makes the "don't clobber a concurrent parse"
            // guard atomic with the write. Only populate the injection caches when
            // the tree actually landed, so a `didClose` racing this reparse can't
            // leave stale injection entries for a gone document. (`Tree` clone is a
            // cheap refcount bump.)
            let stored = self.documents.attach_tree_if_absent(
                &uri,
                &text,
                expected_language_id.as_deref(),
                expected_incarnation,
                tree.clone(),
            );
            if let Some(parse_epoch) = stored {
                let discovery = self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.node_tracker(),
                );
                if let Some(discovery) = discovery {
                    self.documents.set_injections_if_epoch_unchanged(
                        &uri,
                        expected_incarnation,
                        parse_epoch,
                        discovery,
                    );
                }
                break;
            }
            // CAS rejected: the text moved under us (a concurrent `didChange`).
            // Loop to re-read the latest text and try again.
        }

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
        let (language_name, language_id, text, seed, incarnation) = {
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
            (language_name, language_id, text, seed, incarnation)
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
            advance_watermark();
            return;
        };
        if self.auto_install.is_parser_failed(&language_name) {
            advance_watermark();
            return;
        }
        let load_result = self.language.ensure_language_loaded(&language_name);

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
            .parse_with_pool(&language_name, uri, text_len, move |mut parser| {
                let _ = auto_install.begin_parsing(&language_name_clone);
                let result = parser.parse(&*text_for_parse, seed.as_ref());
                let _ = auto_install.end_parsing(&language_name_clone);
                (parser, result)
            })
            .await;

        if let Some(tree) = parsed {
            // Text + language + incarnation CAS (non-inserting), all three checked
            // atomically under the tree-write shard lock. Text rejects a
            // within-lifetime stale parse (a `didChange` landed mid-parse);
            // language rejects a reopen that relabelled the URI; incarnation
            // rejects a same-language, identical-text reopen — the tree belongs to
            // the prior lifetime and must not attach to the reopened document (nor
            // let the watermark advance below run on the old lifetime's ticket).
            let stored = self.documents.update_tree_if_text_and_language_unchanged(
                uri,
                &text,
                language_id.as_deref(),
                incarnation,
                tree.clone(),
            );
            if let Some(parse_epoch) = stored {
                let discovery = self.cache.populate_injections(
                    uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.node_tracker(),
                );
                if let Some(discovery) = discovery {
                    self.documents.set_injections_if_epoch_unchanged(
                        uri,
                        incarnation,
                        parse_epoch,
                        discovery,
                    );
                }
            }
        }

        advance_watermark();
        self.notifier()
            .log_language_events(&load_result.events)
            .await;
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }
}
