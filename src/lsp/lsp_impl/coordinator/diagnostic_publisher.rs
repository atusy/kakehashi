//! The single proactive `textDocument/publishDiagnostics` publisher
//! (push-propagation-diagnostic-forwarding).
//!
//! Proactive diagnostic feeds write slots into the [`DiagnosticAggregator`]
//! and then ask this publisher to **republish** the host (a drained downstream
//! burst records all slots first and asks once per affected host): it snapshots the
//! cache, transforms region push slots to host coordinates against the region's
//! *current* offset (lazy re-anchor), merges with the host-event pull blob, and
//! emits at most one `publishDiagnostics` — the wire send is coalesced per host
//! (the quiet window), can be sealed off by config, and is deferred while the
//! region geometry is unknown (see [`DiagnosticPublisher::republish`]). Routing
//! every feed through one publisher is what keeps sibling regions intact
//! against the client's URI-level clobber.

use std::collections::HashMap;
use std::sync::Arc;

use url::Url;

use crate::document::DocumentStore;
use crate::language::{InjectionResolver, LanguageCoordinator};
use crate::lsp::bridge::{
    BridgeCoordinator, ProgressConnectionId, RegionOffset, VirtualDocumentUri,
};
use crate::lsp::diagnostic_cache::{
    DiagnosticAggregator, DiagnosticSource, merge_cached_diagnostics,
};
use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::Diagnostic;

/// Logging target for proactive push diagnostics.
const LOG_TARGET: &str = "kakehashi::push_diag";

pub(crate) struct DiagnosticPush {
    pub(crate) uri: String,
    pub(crate) server: String,
    pub(crate) connection_id: ProgressConnectionId,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// Outcome of one [`DiagnosticPublisher::republish`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepublishOutcome {
    /// The merged set changed and was recorded as the new last-published set
    /// (and sent to the wire, unless the quiet window withheld it or the
    /// config seal skipped it).
    Changed,
    /// The merge produced nothing new — identical to the recorded set (or the
    /// host URI was unusable).
    Unchanged,
    /// Region geometry is unknown (tree pending): nothing was merged, recorded
    /// or sent; the reparse loop's post-parse republish is the retry. The
    /// CACHE did just change for a push-origin caller — that's why it asked to
    /// republish — so push-origin callers treat this like `Changed` for the
    /// coverage bump + `workspace/diagnostic/refresh` nudge: the retry nudges
    /// only as degraded-pull recovery (per-host debt — see the reparse loop),
    /// and a pull-mode client whose own re-pull
    /// raced ahead of the push would otherwise rot on a stale set until the
    /// next edit (the #496 symptom). The nudged re-pull is safe mid-window —
    /// the pull path waits for the tree and folds cached pushes with fresh
    /// geometry.
    Deferred,
}

impl RepublishOutcome {
    /// Whether a push-origin caller should bump coverage and nudge
    /// `workspace/diagnostic/refresh` for this outcome (see [`Self::Deferred`]).
    fn nudges_pull_clients(self) -> bool {
        matches!(self, Self::Changed | Self::Deferred)
    }
}

/// Programmed quiet-window default used by paused-time tests. Runtime scheduling
/// reads the top-level feature policy on each set-changing admission.
#[cfg(test)]
const WIRE_PUBLISH_QUIET_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);
#[cfg(test)]
const FORWARDED_REFRESH_SETTLE_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);
#[cfg(test)]
const FORWARDED_REFRESH_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

async fn wait_for_forwarded_refresh_settle(
    aggregator: &DiagnosticAggregator,
    mut snapshot: crate::lsp::diagnostic_cache::ForwardedRefreshWaitSnapshot,
    deadline: tokio::time::Instant,
    settle_window: std::time::Duration,
) -> crate::lsp::diagnostic_cache::ForwardedRefreshWait {
    loop {
        tokio::time::sleep_until((snapshot.last_activity_at + settle_window).min(deadline)).await;
        let force = tokio::time::Instant::now() >= deadline;
        match aggregator.finish_forwarded_refresh_wait(snapshot.generation, force) {
            crate::lsp::diagnostic_cache::ForwardedRefreshWait::Restart(latest) => {
                snapshot = latest;
            }
            decision => return decision,
        }
    }
}

/// Bundles the state needed to merge the cache and publish for a host, so the
/// notification feeds (reader push, host-event pull) can trigger a republish
/// without each holding `Kakehashi`. `Clone` is cheap (a `Client` handle plus
/// `Arc`s) — the trailing wire-publish task clones it to re-run `republish`
/// after the quiet window.
#[derive(Clone)]
pub(crate) struct DiagnosticPublisher {
    client: Client,
    language: Arc<LanguageCoordinator>,
    documents: Arc<DocumentStore>,
    bridge: Arc<BridgeCoordinator>,
    settings_manager: Arc<SettingsManager>,
    cache: Arc<crate::lsp::cache::CacheCoordinator>,
    aggregator: Arc<DiagnosticAggregator>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl DiagnosticPublisher {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: Arc::clone(&server.language),
            documents: Arc::clone(&server.documents),
            bridge: Arc::clone(&server.bridge),
            settings_manager: Arc::clone(&server.settings_manager),
            cache: Arc::clone(&server.cache),
            aggregator: Arc::clone(&server.diagnostics),
            shutdown: server.shutdown_token.clone(),
        }
    }

    fn forwarded_refresh_timing(&self) -> (std::time::Duration, std::time::Duration) {
        let timing = self
            .settings_manager
            .load_settings()
            .features
            .workspace_diagnostic_refresh;
        (
            std::time::Duration::from_millis(timing.debounce_ms),
            std::time::Duration::from_millis(timing.max_wait_ms),
        )
    }

    fn admit_forwarded_refresh(
        &self,
    ) -> Option<(
        crate::lsp::diagnostic_cache::ForwardedRefreshWaitSnapshot,
        std::time::Duration,
        std::time::Duration,
    )> {
        // Read timing before the idle→active state transition. Once begin()
        // admits this cycle, its timing is already an owned snapshot; a live
        // settings swap can only affect a later admission.
        let (settle_window, max_wait) = self.forwarded_refresh_timing();
        self.aggregator
            .begin_forwarded_refresh_debounce()
            .map(|snapshot| (snapshot, settle_window, max_wait))
    }

    /// Forward the first downstream `workspace/diagnostic/refresh` immediately,
    /// then coalesce later burst activity into at most one trailing refresh
    /// after quiet or the max-wait deadline (#789).
    pub(crate) fn request_forwarded_diagnostic_refresh(&self) {
        if self.shutdown.is_cancelled() {
            return;
        }
        if !self.diagnostic_refresh_supported() {
            return;
        }
        // Preserve the metrics contract: each capability-eligible downstream
        // ask is counted before this debounce decides how many wire sends survive.
        self.aggregator.record_refresh_requested();
        let Some((snapshot, settle_window, max_wait)) = self.admit_forwarded_refresh() else {
            return;
        };
        // Snapshot timing once per admitted cycle. Live configuration updates
        // affect the next idle→active admission without moving this cycle's
        // already-established quiet/max-wait boundaries.
        if self.request_pull_diagnostic_refresh_inner(true, false) {
            self.aggregator
                .mark_forwarded_refresh_covered(snapshot.generation);
        }
        let publisher = self.clone();
        tokio::spawn(async move {
            let mut snapshot = snapshot;
            let mut deadline = snapshot.last_activity_at + max_wait;
            loop {
                tokio::select! {
                    biased;
                    _ = publisher.shutdown.cancelled() => {
                        publisher.aggregator.cancel_forwarded_refresh_debounce();
                        break;
                    }
                    decision = wait_for_forwarded_refresh_settle(
                        &publisher.aggregator,
                        snapshot,
                        deadline,
                        settle_window,
                    ) => {
                        // Cancellation can race immediately after `select!` chose the
                        // timer branch, so re-check at the refresh admission boundary.
                        if publisher.shutdown.is_cancelled() {
                            publisher.aggregator.cancel_forwarded_refresh_debounce();
                            break;
                        }
                        match decision {
                            crate::lsp::diagnostic_cache::ForwardedRefreshWait::SendTrailing(
                                admitted,
                            ) => {
                                if publisher.request_pull_diagnostic_refresh_inner(true, false) {
                                    publisher
                                        .aggregator
                                        .mark_forwarded_refresh_covered(admitted.generation);
                                    if let Some(latest) = publisher
                                        .aggregator
                                        .finish_forwarded_refresh_admission(admitted.generation)
                                    {
                                        snapshot = latest;
                                        deadline = tokio::time::Instant::now() + max_wait;
                                        continue;
                                    }
                                } else {
                                    publisher.aggregator.cancel_forwarded_refresh_debounce();
                                }
                                break;
                            }
                            crate::lsp::diagnostic_cache::ForwardedRefreshWait::Settled => break,
                            crate::lsp::diagnostic_cache::ForwardedRefreshWait::MaxWait {
                                snapshot: latest,
                                send_trailing,
                            } => {
                                if send_trailing {
                                    if publisher
                                        .request_pull_diagnostic_refresh_inner(true, false)
                                    {
                                        publisher
                                            .aggregator
                                            .mark_forwarded_refresh_covered(latest.generation);
                                    } else {
                                        publisher.aggregator.cancel_forwarded_refresh_debounce();
                                        break;
                                    }
                                }
                                snapshot = latest;
                                deadline = tokio::time::Instant::now() + max_wait;
                            }
                            crate::lsp::diagnostic_cache::ForwardedRefreshWait::Restart(_) => {
                                unreachable!("the settle waiter consumes restart decisions")
                            }
                        }
                    }
                }
            }
        });
    }

    fn diagnostic_refresh_supported(&self) -> bool {
        self.settings_manager
            .client_capabilities_lock()
            .get()
            .is_some_and(crate::lsp::client::check_diagnostic_refresh_support)
    }

    /// Route a downstream `publishDiagnostics` push into the cache, classifying by
    /// its URI: a virtual injection URI becomes a region push; anything else is
    /// treated as a candidate `_self` host-layer push for the real host document.
    pub(crate) async fn publish_push(
        &self,
        uri: String,
        server: String,
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) {
        self.publish_push_batch(vec![DiagnosticPush {
            uri,
            server,
            connection_id,
            diagnostics,
        }])
        .await;
    }

    /// Record a drained run of downstream pushes, then publish each affected
    /// host's final aggregate once. Distinct virtual URIs can all map to the
    /// same host, so batching at this resolved-host boundary avoids serializing
    /// and enqueueing a multi-megabyte intermediate aggregate per region.
    pub(crate) async fn publish_push_batch(&self, pushes: Vec<DiagnosticPush>) -> usize {
        let mut seen = std::collections::HashSet::new();
        let mut hosts = Vec::new();
        for push in pushes {
            let host = if VirtualDocumentUri::is_virtual_uri(&push.uri) {
                self.record_region_push(
                    &push.uri,
                    push.server,
                    push.connection_id,
                    push.diagnostics,
                )
                .await
            } else {
                self.record_host_push(&push.uri, push.server, push.connection_id, push.diagnostics)
            };
            if let Some(host) = host
                && seen.insert(host.clone())
            {
                hosts.push(host);
            }
        }
        self.publish_recorded_hosts(hosts).await
    }

    async fn publish_recorded_hosts(&self, hosts: Vec<Url>) -> usize {
        // Counts hosts whose republish warrants the pull-client nudge (Changed
        // or Deferred) — not wire sends, which the quiet window/seal may
        // withhold.
        let mut nudged = 0;
        for host in hosts {
            if self.republish(&host).await.nudges_pull_clients() {
                self.bump_current_if_open(&host);
                nudged += 1;
            }
        }
        if nudged > 0 {
            self.request_pull_diagnostic_refresh(false);
        }
        nudged
    }

    /// Record a `_self` host-layer push and republish the host (host-document-bridge).
    ///
    /// A host server pushes for the **real** host URI in host coordinates. Accept
    /// it only when that URI names an **open** document AND the pushing `server` is
    /// a configured `_self` host server for that document's language. This drops
    /// both the common stray case (a push for a workspace file the editor doesn't
    /// have open) and a real-URI push from a server that isn't a host server for
    /// this language. Host diagnostics need no coordinate transform.
    #[cfg(test)]
    pub(crate) async fn publish_host_push(
        &self,
        host_uri: &str,
        server: String,
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) {
        if let Some(host) = self.record_host_push(host_uri, server, connection_id, diagnostics) {
            self.publish_recorded_hosts(vec![host]).await;
        }
    }

    fn record_host_push(
        &self,
        host_uri: &str,
        server: String,
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) -> Option<Url> {
        let Ok(host) = Url::parse(host_uri) else {
            return None;
        };
        let Some(language_name) = self.open_document_language(&host) else {
            return None; // not an open document
        };
        let settings = self.settings_manager.load_settings();
        let is_host_server = self
            .bridge
            .get_host_configs_for_language(&settings, &language_name)
            .iter()
            .any(|config| config.server_name == server);
        if !is_host_server {
            // `_self` host bridging is off for this language, or `server` is not a
            // configured host server for it — not a host-layer contribution. This
            // also covers a server disabled (`enabled: false`) after it already
            // spawned and pushed: its live connection can still emit pushes here,
            // but they must not be recorded. Its previously-published diagnostics
            // can linger until some other trigger republishes this host — the same
            // deferred config-change re-merge gap `_self`-disable and empty-cmd
            // already have (see `republish`'s doc comment); `didChangeConfiguration`
            // does not proactively republish open hosts. Not fixed here: doing so
            // unconditionally on every rejected push previously cost a full
            // lock+snapshot+merge republish per push for the life of the document
            // (caught by review), for a gap this branch didn't introduce.
            return None;
        }
        self.aggregator.record(
            &host,
            DiagnosticSource::Host,
            server,
            Some(connection_id),
            diagnostics,
        );
        Some(host)
    }

    /// Bump a host's coverage version, but only if it is still an open document
    /// (#497). The open-check at the top of a push path and this bump straddle the
    /// push's `record` + `republish().await`, so a `didClose` in that macro-window
    /// could otherwise strand a coverage entry on a now-closed host that no pull will
    /// ever clear — defeating the workspace-wide suppression for the rest of the
    /// session. Re-reading `documents` here collapses that to the same narrow
    /// resolve-vs-`didClose` micro-window the codebase already accepts (see
    /// `text_document/did_close.rs`); fully closing it needs the deferred per-host
    /// tombstone/epoch gate.
    fn bump_current_if_open(&self, host: &Url) {
        if self.documents.get(host).is_some() {
            self.aggregator.bump_current(host);
        }
    }

    /// Detect the language of an *open* document, or `None` if it isn't open.
    ///
    /// Borrows the document text directly (no `snapshot()` clone) — detection
    /// never touches the tree, so this also avoids spuriously dropping a push for
    /// an open-but-not-yet-parsed document.
    ///
    /// Uses the trace-level detection variant: this runs per downstream push
    /// (`record_host_push`) and up to twice more per changed republish
    /// (`filter_stale_host_slots`, `publish_sealed`), so the debug-level
    /// variant would re-grow the per-event `language_detection` log volume
    /// that PR #677 moved off the hot paths. Detection itself is cheap here —
    /// the `languageId` short-circuits at the first stage for open documents.
    fn open_document_language(&self, uri: &Url) -> Option<String> {
        let doc = self.documents.get(uri)?;
        self.language
            .detect_language_trace(uri.path(), doc.text(), None, doc.language_id())
    }

    /// Remove `Host` push slots from `snapshot` whose server is no longer a
    /// configured `_self` host server for `host`'s current language, so stale host
    /// diagnostics are filtered out of the publish after a config change. Operates
    /// on the snapshot clone only; the cache is untouched.
    fn filter_stale_host_slots(
        &self,
        host: &Url,
        snapshot: &mut crate::lsp::diagnostic_cache::SourceSlots,
    ) {
        // Single map lookup via the entry API (no separate contains_key/get_mut/remove).
        let mut entry = match snapshot.entry(DiagnosticSource::Host) {
            std::collections::hash_map::Entry::Occupied(entry) => entry,
            std::collections::hash_map::Entry::Vacant(_) => return, // no host push slots
        };
        // If the doc is gone or its language no longer opts into `_self`, no server
        // is valid — drop the whole Host source.
        let Some(language_name) = self.open_document_language(host) else {
            entry.remove();
            return;
        };
        let settings = self.settings_manager.load_settings();
        let configs = self
            .bridge
            .get_host_configs_for_language(&settings, &language_name);
        if configs.is_empty() {
            // No host servers for this language (or `_self` disabled) — every host
            // slot is stale. Drop the whole source without scanning the slots.
            entry.remove();
            return;
        }
        let slots = entry.get_mut();
        // A language has only a handful of host servers (usually 1–2), so a linear
        // scan over `configs` is cheaper than allocating a lookup set on every
        // republish (no `server_name` clones either).
        slots.retain(|server, _| configs.iter().any(|c| &c.server_name == server));
        if slots.is_empty() {
            entry.remove();
        }
    }

    /// Remove cached **push** slots (`Region`/`Host`) whose server is
    /// **pull-driven** when a `PullLayer` blob is present, so a pull-driven
    /// server that both answers the host-event pull (landing in `PullLayer`)
    /// AND spontaneously pushes `publishDiagnostics` is not counted twice —
    /// each server has exactly one native source
    /// (push-propagation-diagnostic-forwarding "Per-server source and
    /// fallback", #425).
    ///
    /// Classification is live (static initialize caps + dynamic registrations,
    /// `LanguageServerPool::pull_driven_servers`) and non-creating. The push
    /// slots stay cached — this filters only the publish snapshot clone, like
    /// [`Self::filter_stale_host_slots`] — so the pull contribution wins while a
    /// later crash/edit eviction still clears the push slot.
    ///
    /// Interim limitation: `PullLayer` is one host-wide blob with no per-server
    /// identity, so the trigger is "any PullLayer present", not "this exact
    /// server was pulled". With a *mixed* per-region `pullFallback` (one
    /// region's pull-driven server pulled, a sibling's not), a pull-driven
    /// server whose region set `pullFallback = false` can still have its push
    /// suppressed while the blob carries the sibling region. The deferred
    /// per-source fan-in (per-`(source, server)` pull slots) removes this.
    async fn filter_pull_driven_push_slots(
        &self,
        snapshot: &mut crate::lsp::diagnostic_cache::SourceSlots,
    ) {
        if !snapshot.contains_key(&DiagnosticSource::PullLayer) {
            // No pull blob to double-count against.
            return;
        }
        // Distinct push-server names across the Region/Host sources, borrowed
        // from the snapshot (no key clones on this republish hot path) — the same
        // set Path B's fold derives, so share the helper.
        let push_servers = crate::lsp::diagnostic_cache::push_slot_servers(snapshot);
        if push_servers.is_empty() {
            return; // PullLayer-only snapshot (the common pull-driven case).
        }
        let pull_driven = self.bridge.pool().pull_driven_servers(&push_servers).await;
        // Drop the borrow of `snapshot` before the mutable retain below.
        drop(push_servers);
        retain_non_pull_driven_push_slots(snapshot, &pull_driven);
    }

    /// Record a downstream region push and republish the host (Path A).
    ///
    /// `virtual_uri` is the URI the downstream published for; it is resolved to
    /// its host document + region id. A push for a URI that resolves to no live
    /// region (a closed/edited-away region, or a non-bridged document) is dropped.
    /// Diagnostics are stored in virtual coordinates and transformed at publish.
    #[cfg(test)]
    pub(crate) async fn publish_region_push(
        &self,
        virtual_uri: &str,
        server: String,
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) {
        if let Some(host) = self
            .record_region_push(virtual_uri, server, connection_id, diagnostics)
            .await
        {
            self.publish_recorded_hosts(vec![host]).await;
        }
    }

    async fn record_region_push(
        &self,
        virtual_uri: &str,
        server: String,
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) -> Option<Url> {
        let Some((host, region_id)) = self.bridge.resolve_virtual_uri(virtual_uri).await else {
            log::debug!(
                target: LOG_TARGET,
                "push for unresolved virtual uri {virtual_uri}, dropping"
            );
            return None;
        };
        // A server no longer spawnable (disabled via `enabled: false`, or no
        // longer configured at all) after it already spawned can still emit
        // region pushes on its still-live connection; drop them rather than
        // recording fresh diagnostics for a server the user no longer wants
        // running (mirrors publish_host_push's is_host_server gate).
        // Spawnability is a per-server-name property, so this only needs the
        // pushing server's own resolved config, not the region's injection
        // language — checked via the allocation-free is_server_spawnable
        // rather than a full resolve_with_wildcard merge, since this runs on
        // every push.
        let settings = self.settings_manager.load_settings();
        if !crate::config::is_server_spawnable(&settings.language_servers, &server) {
            log::debug!(
                target: LOG_TARGET,
                "push from unspawnable server {server}, dropping"
            );
            return None;
        }
        self.aggregator.record(
            &host,
            DiagnosticSource::Region(region_id),
            server,
            Some(connection_id),
            diagnostics,
        );
        Some(host)
    }

    /// Feed the host-event pull's combined result into the cache and republish.
    ///
    /// The pull blob is already host-local; it replaces the
    /// [`DiagnosticSource::PullLayer`] slot, then the merge folds in region push
    /// slots.
    ///
    /// Staged limitation: `SyntheticDiagnosticsManager` aborts a superseded pull
    /// task, but the abort cannot preempt the synchronous `set_pull_layer` write
    /// below — so a superseded task can leave a slightly stale `PullLayer` that a
    /// later republish includes until the next pull completes. This is the same
    /// staleness class the deferred `content_epoch` version gate
    /// (push-propagation-diagnostic-forwarding) handles generally; until then it
    /// self-heals on the next completed pull.
    pub(crate) async fn publish_pull_layer(&self, host: &Url, diagnostics: Vec<Diagnostic>) {
        self.aggregator.set_pull_layer(host, diagnostics);
        self.republish(host).await;
    }

    /// Evict the host's `PullLayer` blob and republish — the host-event pull had
    /// no contributors this event (every layer `pullFallback`-gated, or none).
    ///
    /// Eviction (vs publishing an empty blob) is deliberate: an **absent**
    /// `PullLayer` both clears any stale pull result and stops
    /// [`Self::filter_pull_driven_push_slots`] from treating "no pull ran" as
    /// "pull present", which would otherwise suppress a pull-driven server's
    /// spontaneous push under `pullFallback = false` (#425). A pull that ran and
    /// returned clean keeps its empty `PullLayer` (via [`Self::publish_pull_layer`]),
    /// so that path still suppresses a stale push — the two empties differ.
    ///
    /// Deliberately does **not** emit `workspace/diagnostic/refresh` (#499). This is
    /// the empty-contributors branch of the same host-event pull task as
    /// [`Self::publish_pull_layer`], which #496 left no-refresh: both are always
    /// downstream of a host event (didOpen/didChange/didSave) the editor originated
    /// and re-pulls for on its own. There is no spontaneous `clear_pull_layer`, so a
    /// refresh here would be redundant (the editor's own re-pull already covers it).
    pub(crate) async fn clear_pull_layer(&self, host: &Url) {
        self.aggregator
            .evict_source(host, &DiagnosticSource::PullLayer);
        self.republish(host).await;
    }

    /// Evict every diagnostic slot a now-exited downstream connection produced and
    /// republish the affected hosts (#469). Called when a connection's reader exits
    /// (crash/respawn); a restart's slots carry a fresh connection id and survive,
    /// so this clears only the dead server's contribution.
    ///
    /// A downstream reader exit emits **no LSP event the editor sees**, so a
    /// pull-mode client (which displays the diagnostics it *pulled*, not our
    /// `publishDiagnostics`) has no trigger to re-pull — the crashed server's now
    /// removed diagnostics would rot until the next edit-triggered pull. So when an
    /// eviction actually changed a host's merged set, nudge pull-mode clients to
    /// re-pull, exactly like the spontaneous-push paths (#499, push/pull-divergence
    /// #422). `workspace/diagnostic/refresh` is workspace-wide, so emit it once for
    /// the whole eviction, not per host. Loop-safe: a refresh-induced pull is
    /// answered inline by `diagnostic_impl` without republishing.
    ///
    /// Scope: `evict_connection` clears only **push** slots (the `PullLayer` blob is
    /// recorded with no connection id, so it survives eviction), so a *purely
    /// pull-driven* server's crash evicts nothing here and isn't refreshed by this
    /// path — that case is out of scope and self-heals on the next host-event pull,
    /// matching the intentional push-only asymmetry of `evict_connection` (#469).
    pub(crate) async fn evict_connection_diagnostics(&self, connection_id: ProgressConnectionId) {
        let affected = self.aggregator.evict_connection(connection_id);
        let mut any_changed = false;
        for host in affected {
            if self.republish(&host).await.nudges_pull_clients() {
                // A crash eviction is a push-origin change the editor doesn't know
                // about → bump coverage so the gated refresh below fires (#497).
                self.bump_current_if_open(&host);
                any_changed = true;
            }
        }
        if any_changed {
            self.request_pull_diagnostic_refresh(false);
        }
    }

    /// Drop the host's cache entry and publish the now-empty set (host `didClose`).
    ///
    /// Deliberately does **not** emit `workspace/diagnostic/refresh` (#499): the
    /// editor itself originated the `didClose`, so it is not displaying (and won't
    /// re-pull) a closed document — a refresh would be redundant. This is the
    /// editor-originated sibling of [`Self::clear_pull_layer`], opposite the
    /// crash-driven [`Self::evict_connection_diagnostics`] (which the editor has no
    /// event to learn about, so it does refresh).
    pub(crate) async fn clear_host(&self, host: &Url) {
        self.aggregator.evict_host(host);
        // The clearing republish needs no gate preparation: `republish` bypasses
        // the quiet window for a closed host (the document is already removed by
        // `did_close` when this runs). Deliberately do NOT forget the wire-gate
        // state before it — the `dirty` marker of a still-withheld send is what
        // forces the clearing publish past the unchanged-skip when the withheld
        // set was itself the empty set (all contributors cleared just before the
        // close; the eviction then merges `[]` against a recorded `[]`, and
        // without `dirty` the clearing publish would be skipped while the
        // trailing task dies on its closed-document guard — pinning the closed
        // buffer's stale diagnostics in the editor forever).
        self.republish(host).await;
        // The host is closed: forget its last-published set so the entry does not
        // linger and a later re-open publishes afresh (#422). Done after the
        // clear-republish above so that publish still sees the prior set.
        self.aggregator.forget_published(host);
        // Likewise forget its coverage versions (#497) — a closed doc can't be
        // pulled, so it must not keep the workspace dirty; a re-open starts fresh at
        // 0. (`clear_host` is editor-originated, so its republish doesn't bump
        // `current`; this just drops any prior push-origin coverage state.)
        self.aggregator.forget_coverage(host);
        // And any degraded-pull debt — a closed doc owes no recovery refresh.
        self.aggregator.forget_degraded_pull(host);
        // And any wire-gate state left by earlier open-host publishes. The
        // clearing republish above bypasses the gate because the host is closed.
        self.aggregator.forget_wire_gate(host);
        // didClose is off the hot path: reclaim republish-lock entries whose lock
        // now has no live holder — this host's, once the clear-republish above
        // released it, plus any earlier-closed hosts that have since drained (#466).
        self.aggregator.reclaim_republish_locks();
    }

    /// Merge the host's cached slots and publish the cumulative result. Region
    /// slots are transformed against the host document's *current* injection
    /// offsets; an empty merge clears the editor's diagnostics for the host.
    /// For a pull-capable client that has been observed pulling this host, the
    /// wire send may lag: the quiet window can withhold it into the trailing
    /// flush, and the config seal can skip it entirely — see the gates below.
    ///
    /// Returns [`RepublishOutcome::Changed`] when the merged set changed and
    /// was recorded (the wire send may still be withheld or sealed),
    /// [`RepublishOutcome::Unchanged`] when the merge produced nothing new,
    /// and [`RepublishOutcome::Deferred`] when region geometry was unknown
    /// mid-reparse (nothing merged or recorded; the reparse loop retries).
    /// Push-origin callers (`publish_recorded_hosts`,
    /// [`Self::evict_connection_diagnostics`]) nudge pull-mode clients with
    /// [`Self::request_pull_diagnostic_refresh`] on `Changed` OR `Deferred` —
    /// see [`RepublishOutcome::Deferred`] for why the deferral still nudges.
    pub(crate) async fn republish(&self, host: &Url) -> RepublishOutcome {
        self.republish_with_wire_activity(host, true).await
    }

    async fn republish_with_wire_activity(
        &self,
        host: &Url,
        wire_activity: bool,
    ) -> RepublishOutcome {
        // Serialize the whole snapshot→merge→publish so concurrent republishes
        // (region push vs host-event pull) emit in order and a stale snapshot can
        // never publish after a fresh one (push-propagation-diagnostic-forwarding).
        //
        // The lock is held across the editor `publish_diagnostics` await below
        // because the ordering guarantee requires it (releasing before the send
        // would let two publishes reorder on the wire). The lock is **per host**
        // (#426), so a slow editor publish for one host stalls only that host's
        // republishes, not every host's; different hosts proceed concurrently.
        // publish_diagnostics is a fire-and-forget notification, so the stall window
        // is the client's outbound-channel send, not a round-trip.
        let _guard = self.aggregator.lock_republish(host).await;

        let mut snapshot = self.aggregator.snapshot(host);
        // Drop Host push slots whose server is no longer a configured `_self` host
        // server for the document's current language — so a host server's pushed
        // diagnostics don't linger in the editor after the user disables `_self`
        // (or unconfigures the server) via `workspace/didChangeConfiguration`. The
        // slots stay cached (cleared on `didClose`); they're just filtered out of
        // this publish. (The analogous Region/config-change re-merge is deferred.)
        self.filter_stale_host_slots(host, &mut snapshot);
        // Drop a pull-driven server's push slots when the host-event pull blob
        // (`PullLayer`) is present: that server already contributes via the
        // pull, so keeping its spontaneous push too would double-count it
        // (#425). The cache keeps the slot; only this publish snapshot is
        // filtered.
        self.filter_pull_driven_push_slots(&mut snapshot).await;
        // Recompute injection offsets only when there are region push slots to
        // transform. A PullLayer-only snapshot (the common pull-driven case) needs
        // none, so skip the whole-document injection resolution — and shorten the
        // time this host's republish lock is held. The predicate IS
        // `DiagnosticAggregator::has_region_slots`'s (the shared
        // `has_live_region_slots`): the geometry-unknown deferral below relies
        // on the reparse loop's post-parse republish, which is gated on
        // exactly that check.
        let needs_geometry = crate::lsp::diagnostic_cache::has_live_region_slots(&snapshot);
        let region_offsets = if needs_geometry {
            match self.current_region_offsets(host) {
                Some(offsets) => offsets,
                // The document is open but has no parse snapshot: `did_change`
                // cleared the tree and the off-ingress reparse hasn't landed yet,
                // so the regions' current offsets are UNKNOWN — not gone. Merging
                // now would silently drop every region push slot and publish a
                // set missing whole servers, which the post-parse republish then
                // reverts: on a diagnostics-heavy host that flapped the editor
                // between the full and the region-less set on every edit cycle
                // (~2 × ~1 MB publishes per keystroke settle, plus visible
                // flicker for push-mode clients). Defer instead: publish nothing,
                // record nothing. The reparse loop re-runs `republish` after the
                // parse lands whenever non-empty region slots remain
                // (`has_region_slots` — the same predicate as `needs_geometry`),
                // and the pass's debounced pull re-feeds and republishes too.
                // Caveats, accepted: a parse give-up (timeout / parser gone)
                // leaves the tree absent, so both retries stay deferred until
                // the next edit (main published a region-LESS set there — worse);
                // and an edit that evicts the very slots that forced this defer
                // hands coverage to the debounced pull alone. Push-origin
                // delivery does not depend on the retry: `Deferred` nudges
                // pull-mode clients at defer time (see `RepublishOutcome`).
                None => {
                    log::debug!(
                        target: LOG_TARGET,
                        "defer republish for {host}: tree pending, region geometry unknown"
                    );
                    return RepublishOutcome::Deferred;
                }
            }
        } else {
            HashMap::new()
        };
        let diagnostics = merge_cached_diagnostics(host, snapshot, &region_offsets);

        let lsp_uri = match crate::lsp::lsp_impl::url_to_uri(host) {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!(target: LOG_TARGET, "skip publish, bad host URI {host}: {e}");
                return RepublishOutcome::Unchanged;
            }
        };

        // Suppress a republish that would re-send the exact set the editor already
        // has — a redundant publishDiagnostics is needless flicker/noise (#422).
        // Done under the per-host republish lock (held above), so the compare-and-set
        // is serialized with other republishes for this host. A withheld wire
        // send (`wire_gate_is_dirty`) still proceeds: the recorded set is ahead
        // of what actually reached the editor, so the trailing republish must
        // send even though its own merge compares unchanged.
        let changed = if self.aggregator.published_set_changed(host, &diagnostics) {
            RepublishOutcome::Changed
        } else {
            RepublishOutcome::Unchanged
        };
        if changed == RepublishOutcome::Unchanged && !self.aggregator.wire_gate_is_dirty(host) {
            log::debug!(
                target: LOG_TARGET,
                "skip republish for {host}: merged set unchanged ({} diagnostics)",
                diagnostics.len()
            );
            return RepublishOutcome::Unchanged;
        }

        // The cross-layer gate for `textDocument/publishDiagnostics` doubles as a
        // WIRE SEAL when it resolves to no layers at all: a pull-first editor
        // setup can stop the proactive publishes entirely via existing config.
        // On a diagnostics-heavy host they dominated the editor pipe (measured:
        // 51 publishes × ~1 MB in a 44 s typing burst, 68% of all server→client
        // bytes — head-of-line blocking every other response) while a pulling
        // editor ignores them or renders them as a duplicate namespace. The
        // merge above still ran and recorded the set: change detection keeps
        // driving the push-origin coverage bump and
        // `workspace/diagnostic/refresh`, which IS the sealed setup's delivery
        // signal (refresh → client re-pull). The wire-gate state is dropped —
        // there is no wire under the seal, so nothing can be owed to it (a
        // stale `dirty` from before a mid-session seal would otherwise linger).
        if self.publish_sealed(host) {
            self.aggregator.forget_wire_gate(host);
            log::debug!(
                target: LOG_TARGET,
                "seal: skipping publish of {} merged diagnostics for {} \
                 (layers.aggregation publishDiagnostics resolves to no layers)",
                diagnostics.len(),
                host
            );
            return changed;
        }

        // A republish for a CLOSED host bypasses the quiet window and never
        // touches gate state. This is (or races) `didClose`'s clearing publish,
        // and both halves matter: a *deferred* clearing publish would be
        // dropped by the trailing task's closed-document guard, pinning the
        // closed buffer's last diagnostics in the editor forever; and a racing
        // in-flight republish that *stamped* fresh gate state here (after
        // `clear_host`'s forget, which does not hold the republish lock) would
        // be exactly what defers that clearing publish. Both republishes are
        // serialized on the per-host lock, so the clearing (empty) publish
        // always lands last. (This ungated send shares the pre-existing
        // record-before-send hazard: an abort landing exactly at the send
        // below loses it — same class as main, bounded to closed-host races.)
        if self.documents.get(host).is_none() {
            log::debug!(
                target: LOG_TARGET,
                "publishing {} merged diagnostics for closed host {} (quiet window bypassed)",
                diagnostics.len(),
                host
            );
            self.client
                .publish_diagnostics(lsp_uri, diagnostics, None)
                .await;
            return changed;
        }

        // Coalesce the wire sends per host (the quiet window): a burst of
        // set-changing feeds — spontaneous pushes, the pull-layer completion,
        // the post-parse geometry re-merge, each ~1 MB on a diagnostics-heavy
        // host — collapses into at most one publish per window, re-merged from
        // the LATEST cache by the trailing republish. An isolated change sends
        // immediately. Deferral withholds only the wire: the set was already
        // recorded above, so the return value (and with it the push-origin
        // coverage bump + refresh nudge) is unaffected.
        let timing = self
            .settings_manager
            .load_settings()
            .features
            .text_document_publish_diagnostics;
        match self.aggregator.wire_debounce_admit(
            host,
            std::time::Duration::from_millis(timing.debounce_ms),
            std::time::Duration::from_millis(timing.max_wait_ms),
            wire_activity,
        ) {
            crate::lsp::diagnostic_cache::WireAdmit::SendNow => {
                log::debug!(
                    target: LOG_TARGET,
                    "publishing {} merged diagnostics for {}",
                    diagnostics.len(),
                    host
                );
                self.client
                    .publish_diagnostics(lsp_uri, diagnostics, None)
                    .await;
                // Stamp the send only AFTER it completed: this republish can run
                // inside an abortable task (the synthetic pull is aborted on
                // supersession), and an abort landing at the send await must not
                // leave gate state claiming a send that never happened — that
                // would both defer the next change and consume a withheld
                // `dirty` debt. Same lock hold as the admit, so the pair is
                // atomic per host.
                self.aggregator.wire_gate_commit_send(host);
            }
            crate::lsp::diagnostic_cache::WireAdmit::Defer {
                schedule_trailing,
                remaining,
            } => {
                log::debug!(
                    target: LOG_TARGET,
                    "withholding publish of {} merged diagnostics for {} ({}ms of quiet window left)",
                    diagnostics.len(),
                    host,
                    remaining.as_millis()
                );
                if schedule_trailing {
                    self.spawn_trailing_wire_publish(host.clone(), remaining);
                }
            }
        }
        changed
    }

    /// Re-run [`Self::republish`] for `host` once the quiet window elapses,
    /// sending the (re-merged, latest) withheld set to the wire. At most one
    /// task is parked per host at a time ([`WireAdmit::Defer`]'s
    /// `schedule_trailing`). Clears the gate's `pending` marker *before*
    /// re-running, so a defer racing the re-run schedules a fresh task rather
    /// than being absorbed by one that already woke — and BAILS when the gate
    /// entry is gone (`didClose` or the seal forgot it while this task was
    /// parked): the debt was cancelled. (A stale task waking against a
    /// reopened incarnation's fresh entry is instead kept safe by the
    /// defer-reschedule design — see `wire_gate_take_pending`.)
    ///
    /// Shutdown races the timer against the shared cancellation token, forgets
    /// this host's gate state, and exits without a wire send.
    ///
    /// [`WireAdmit::Defer`]: crate::lsp::diagnostic_cache::WireAdmit::Defer
    fn spawn_trailing_wire_publish(&self, host: Url, remaining: std::time::Duration) {
        let publisher = self.clone();
        // Anchor the deadline NOW (at defer time), not at the task's first poll
        // — under load the first poll can lag, which would silently stretch the
        // quiet window.
        let deadline = tokio::time::Instant::now() + remaining;
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = publisher.shutdown.cancelled() => {
                    publisher.aggregator.forget_wire_gate(&host);
                    return;
                }
                _ = tokio::time::sleep_until(deadline) => {}
            }
            if !publisher.aggregator.wire_gate_take_pending(&host) {
                return; // gate forgotten while parked: debt cancelled
            }
            // Belt to the bail above: a host closed while this task was parked
            // (but not yet forgotten) is `clear_host`'s to finish — its
            // clearing republish bypasses the gate and lands last on the
            // per-host lock.
            if publisher.documents.get(&host).is_none() {
                return;
            }
            publisher.republish_with_wire_activity(&host, false).await;
        });
    }

    /// Whether the editor-facing `publishDiagnostics` wire sends are sealed for
    /// `host` by config: the cross-layer gate for
    /// `textDocument/publishDiagnostics` resolves to an **empty** priorities
    /// list (`layers.aggregation."textDocument/publishDiagnostics".priorities =
    /// []` on the host's language or the `_` wildcard).
    ///
    /// The seal is deliberately **binary** (all layers gated off), not
    /// per-layer: a partially gated list still publishes the full merge, and
    /// per-layer selection keeps applying to the debounced pull feed
    /// (`coordinator/diagnostic.rs`) as before — filtering the wire set by
    /// layer would diverge it from the change-detection set that drives the
    /// refresh nudging. Fails open (not sealed) when the document is gone or
    /// its language is undetectable, so a `didClose` clearing publish still
    /// goes out.
    fn publish_sealed(&self, host: &Url) -> bool {
        self.open_document_language(host)
            .is_some_and(|language_name| {
                crate::lsp::lsp_impl::bridge_context::resolve_layer_config_from_settings(
                    &self.settings_manager.load_settings(),
                    &language_name,
                    "textDocument/publishDiagnostics",
                )
                .priorities
                .is_empty()
            })
    }

    /// Ask pull-mode clients to re-pull diagnostics (`workspace/diagnostic/refresh`)
    /// after a change the editor has no event to learn about — a **spontaneous
    /// downstream push**, a **crash-driven eviction**, or a **downstream server's
    /// own refresh request** forwarded upstream — moved (or invalidated) the set.
    ///
    /// kakehashi advertises `diagnosticProvider`, so a pull-mode editor (e.g.
    /// Neovim) displays the diagnostics it *pulled* and ignores our
    /// `publishDiagnostics`. A push-only `_self` host server (e.g. panache)
    /// analyzes the latest text **asynchronously**, lands its push after the
    /// editor's last pull, and updates our cache — but the editor never re-pulls,
    /// so its displayed (pull-namespace) diagnostics rot until the next
    /// edit-triggered pull (the "stays stale until you edit another line"
    /// symptom). This refresh closes that gap: it tells the client to re-pull,
    /// which returns the now-current merged set.
    ///
    /// Emitted **off** the per-host republish lock and **only** from the origins
    /// the editor can't learn about on its own — the push-origin paths
    /// ([`Self::publish_host_push`], [`Self::publish_region_push`]), the
    /// crash-driven eviction ([`Self::evict_connection_diagnostics`]), the
    /// upstream forwarding loop relaying a downstream server's own
    /// `workspace/diagnostic/refresh` (`deliver_upstream_notification`), and the
    /// reparse loop's post-parse backstop (and the pull-side TOCTOU guard) as
    /// **degraded-pull recovery** — a pull that raced the parse was answered
    /// without the region fold; the per-host debt keys the call, and it is
    /// FORCED past the coverage gate because the debt proves the client holds
    /// a non-covering answer the version-based gate cannot see (an edit-race
    /// degradation leaves `served == current`) — never from the pull-origin republish
    /// ([`Self::publish_pull_layer`]) nor the editor-originated eviction paths
    /// ([`Self::clear_pull_layer`], [`Self::clear_host`]): those carry no *new*
    /// result the editor is unaware of — a pull-origin set is already the
    /// answer to a pull the editor made, and an ordinary edit-origin re-merge
    /// is covered by the editor's own `didChange` re-pull — so a refresh there
    /// would be redundant. (No tight loop forms: a
    /// refresh-induced pull is answered inline by `diagnostic_impl`, which
    /// never republishes; the one pull-side refresh — the degraded-pull TOCTOU
    /// guard — consumes its debt on firing and the induced re-pull sees ready
    /// geometry and answers covering, so it begets at most one round; the indirect
    /// push→refresh→re-pull→downstream-re-push→here path is bounded by
    /// `published_set_changed`, converging once the re-pushed set stabilizes.)
    ///
    /// **Spawned, not awaited:** `workspace/diagnostic/refresh` is a request whose
    /// future resolves only when the editor answers, so awaiting it inline would
    /// block the push path on the client round-trip (and never resolve in a test
    /// that doesn't answer). Detaching it keeps the push path non-blocking and
    /// avoids the upstream-notification loop's inline-await head-of-line block.
    ///
    /// **Single-flight (#497):** `workspace/diagnostic/refresh` is param-less and
    /// workspace-wide, so concurrent refreshes are redundant. A burst of
    /// set-changing pushes would otherwise spawn one detached refresh request each —
    /// every one an un-acked tower-lsp pending-request entry. The aggregator's
    /// guard ([`DiagnosticAggregator::try_begin_refresh`]) collapses the burst: at
    /// most one refresh is in flight (awaiting the editor's ack); requests during
    /// that window set `pending`, and the spawned task loops to fire exactly one
    /// more on completion ([`DiagnosticAggregator::finish_refresh`]). The trailing
    /// refresh still guarantees the editor re-pulls after the last change.
    ///
    /// Gated on the client advertising `workspace.diagnostics.refreshSupport`: a
    /// client that supports pull but not refresh would silently ignore the request,
    /// leaking a tower-lsp pending-request entry plus a parked task — the same gate
    /// the `semantic_tokens_refresh` path uses.
    pub(crate) fn request_pull_diagnostic_refresh(&self, forced: bool) {
        self.request_pull_diagnostic_refresh_inner(forced, true);
    }

    fn request_pull_diagnostic_refresh_inner(&self, forced: bool, record_request: bool) -> bool {
        if self.shutdown.is_cancelled() || !self.diagnostic_refresh_supported() {
            return false;
        }
        // Count the ask before the gates so `requested - sent` measures total
        // debounce/single-flight/coverage savings (#533, #789).
        if record_request {
            self.aggregator.record_refresh_requested();
        }
        // Coalesce against any in-flight refresh and apply the coverage gate (#497):
        // `false` here means either one is already outstanding (recorded as `pending`,
        // so the outstanding task's loop fires the trailing) or — for a non-`forced`
        // request — nothing is dirty (the editor already has the current set). A
        // `forced` request bypasses the coverage gate: the downstream-forwarded
        // refresh (#521) and the degraded-pull recovery (whose per-host debt
        // proves a non-covering answer no coverage version represents).
        if !self.aggregator.try_begin_refresh(forced) {
            return forced;
        }
        let client = self.client.clone();
        let aggregator = Arc::clone(&self.aggregator);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            loop {
                // `workspace_diagnostic_refresh()` resolves when the editor answers
                // the request. A conformant client always answers (the transport is
                // reliable), so this resolves and `finish_refresh` runs. The lone
                // wedge is a live client that advertises `refreshSupport`, receives
                // the request, and never answers (a protocol violation, or a client
                // bug): the await never resolves, `in_flight` stays set, and further
                // refreshes coalesce into a `pending` that never fires. That is
                // accepted degradation — such a client ignores refreshes anyway, so
                // suppressing further (useless) ones is harmless. We deliberately do
                // **not** wrap this in `tokio::time::timeout`: dropping the request
                // future strands tower-lsp's pending-request entry (it is reaped only
                // on a matching response or socket close, never on receiver-drop), so
                // re-firing on timeout would accumulate one stranded entry per change
                // — the very leak this single-flight exists to bound.
                //
                // Panic-safety: the one panic this task can hit is tower-lsp's
                // shutdown-time `expect("sender already dropped")` in the response
                // await — reachable only when the whole pending-request map drops
                // with the `ClientSocket` at teardown (a real response always *sends*
                // on the waiter, never drops it). At shutdown the stuck `in_flight`
                // is moot (the aggregator is being dropped) and tokio isolates the
                // panicking task, so no `catch_unwind` is warranted. TRIPWIRE: if a
                // future change adds a *pre-shutdown* panic source to this task,
                // revisit — it would wedge the guard (and a drop-guard "fix" would
                // reopen the `finish_refresh` lost-wakeup, so clear it atomically).
                // Admission can race shutdown before this detached task first runs.
                // Do not start a request once cancellation is observable, and clear
                // the single-flight claim that admission already took.
                if shutdown.is_cancelled() {
                    aggregator.cancel_refresh_flight();
                    break;
                }
                // Count each wire send, including trailing fires (#533).
                aggregator.record_refresh_sent();
                if let Err(e) = client.workspace_diagnostic_refresh().await {
                    log::debug!(
                        target: LOG_TARGET,
                        "workspace/diagnostic/refresh failed: {e}"
                    );
                }
                // Fire exactly one more iff a refresh was requested while this one
                // was in flight; otherwise the guard is now clear and we stop.
                if shutdown.is_cancelled() {
                    aggregator.cancel_refresh_flight();
                    break;
                }
                if !aggregator.finish_refresh() {
                    break;
                }
            }
        });
        true
    }

    /// Map each currently-resolvable injection region of the host document to its
    /// offset, recomputed from the live document so region push slots re-anchor
    /// after edits above them.
    ///
    /// `None` means the geometry is **unknown**: the document is open but has no
    /// parse snapshot (`did_change` cleared the tree; the off-ingress reparse
    /// hasn't landed) — the caller must defer publishing rather than treat the
    /// regions as gone. `Some(empty)` means there legitimately are no regions to
    /// anchor: the document is closed, or its language resolves to no injection
    /// query — stale region slots drop from the merge.
    fn current_region_offsets(&self, host: &Url) -> Option<HashMap<String, RegionOffset>> {
        let mut offsets = HashMap::new();

        let Some(doc) = self.documents.get(host) else {
            return Some(offsets); // closed host: nothing to anchor against
        };
        let Some(snapshot) = doc.snapshot() else {
            return None; // open but tree pending: geometry unknown, defer
        };
        // Trace-level detection: this runs on the republish path whenever
        // region slots are present (every keystroke settle during a typing
        // burst) — the debug variant would re-grow the per-event
        // `language_detection` log volume PR #677 moved off hot paths.
        let Some(language_name) = self.language.detect_language_trace(
            host.path(),
            snapshot.text(),
            None,
            doc.language_id(),
        ) else {
            return Some(offsets);
        };
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Some(offsets);
        };

        let resolved_regions = match self
            .documents
            .current_resolved_regions(host, self.cache.semantic_token_generation())
        {
            Some(regions) => regions,
            None => std::sync::Arc::new(InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                host,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
                snapshot.incarnation(),
            )),
        };
        for resolved in resolved_regions.iter() {
            offsets.insert(
                resolved.region.region_id.clone(),
                RegionOffset::with_per_line_offsets(
                    resolved.region.line_range.start,
                    resolved.line_column_offsets.clone(),
                ),
            );
        }
        Some(offsets)
    }
}

/// Remove `Region`/`Host` push slots whose server is in `pull_driven` when a
/// `PullLayer` blob is present in `snapshot`, dropping any source left empty.
/// The pure core of [`DiagnosticPublisher::filter_pull_driven_push_slots`],
/// split out so the dedup rule is testable without a live pool. A no-op when
/// `pull_driven` is empty or there is no `PullLayer` to double-count against.
fn retain_non_pull_driven_push_slots(
    snapshot: &mut crate::lsp::diagnostic_cache::SourceSlots,
    pull_driven: &std::collections::HashSet<String>,
) {
    if pull_driven.is_empty() || !snapshot.contains_key(&DiagnosticSource::PullLayer) {
        return;
    }
    snapshot.retain(|source, servers| {
        if matches!(source, DiagnosticSource::PullLayer) {
            return true;
        }
        servers.retain(|server, _| !pull_driven.contains(server));
        !servers.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{
        BridgeLanguageConfig, BridgeServerConfig, HOST_BRIDGE_KEY, LanguageSettings,
        LayerAggregationConfig, LayersConfig, WorkspaceSettings,
    };
    use std::collections::HashMap;
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{
        ClientCapabilities, DiagnosticWorkspaceClientCapabilities, Position, Range,
        WorkspaceClientCapabilities,
    };

    fn diag(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_refresh_sends_the_leading_edge_immediately() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_capabilities(ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        let publisher = DiagnosticPublisher::new(server);
        let before = tokio::time::Instant::now();

        publisher.request_forwarded_diagnostic_refresh();
        tokio::time::advance(std::time::Duration::ZERO).await;

        assert_eq!(
            tokio::time::Instant::now(),
            before,
            "the leading send must happen without advancing to the debounce timer"
        );
        assert_eq!(
            server.diagnostics.metrics_snapshot().refreshes_sent,
            1,
            "the first refresh after idle must not wait for the debounce window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_refresh_metrics_count_inputs_before_debounce() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_capabilities(ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        let publisher = DiagnosticPublisher::new(server);

        publisher.request_forwarded_diagnostic_refresh();
        publisher.request_forwarded_diagnostic_refresh();
        publisher.request_forwarded_diagnostic_refresh();

        assert_eq!(server.diagnostics.metrics_snapshot().refreshes_requested, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_refresh_timing_reads_live_settings_for_next_cycle() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_capabilities(ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        let mut settings = (*server.settings_manager.load_settings()).clone();
        settings.features.workspace_diagnostic_refresh.debounce_ms = 250;
        settings.features.workspace_diagnostic_refresh.max_wait_ms = 800;
        server.settings_manager.apply_settings(settings);
        let publisher = DiagnosticPublisher::new(server);

        let (_, admitted_settle, admitted_max_wait) = publisher
            .admit_forwarded_refresh()
            .expect("idle scheduler admits the old timing snapshot");

        let mut settings = (*server.settings_manager.load_settings()).clone();
        settings.features.workspace_diagnostic_refresh.debounce_ms = 20;
        settings.features.workspace_diagnostic_refresh.max_wait_ms = 40;
        server.settings_manager.apply_settings(settings);
        assert_eq!(
            (admitted_settle, admitted_max_wait),
            (
                std::time::Duration::from_millis(250),
                std::time::Duration::from_millis(800)
            ),
            "an update after admission must not change the active cycle"
        );
        server.diagnostics.cancel_forwarded_refresh_debounce();
        let (_, next_settle, next_max_wait) = publisher
            .admit_forwarded_refresh()
            .expect("the next idle cycle reads the live settings");
        assert_eq!(
            (next_settle, next_max_wait),
            (
                std::time::Duration::from_millis(20),
                std::time::Duration::from_millis(40)
            ),
            "the next cycle must use the live update"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_pending_forwarded_refresh() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_capabilities(ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        let publisher = DiagnosticPublisher::new(server);
        publisher.request_forwarded_diagnostic_refresh();
        server.shutdown_token.cancel();

        tokio::time::advance(FORWARDED_REFRESH_MAX_WAIT).await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let metrics = server.diagnostics.metrics_snapshot();
        assert_eq!(metrics.refreshes_requested, 1);
        assert_eq!(metrics.refreshes_sent, 0, "shutdown must suppress the send");
        assert!(
            server
                .diagnostics
                .begin_forwarded_refresh_debounce()
                .is_some(),
            "shutdown must release the debounce task claim"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_rejects_new_forwarded_refresh_inputs() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_capabilities(ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        server.shutdown_token.cancel();
        let publisher = DiagnosticPublisher::new(server);

        publisher.request_forwarded_diagnostic_refresh();

        assert_eq!(
            server.diagnostics.metrics_snapshot(),
            crate::lsp::diagnostic_cache::DiagnosticMetricsSnapshot::default()
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn shutdown_cancels_an_admitted_refresh_before_send() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_capabilities(ClientCapabilities {
                workspace: Some(WorkspaceClientCapabilities {
                    diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                        refresh_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        let publisher = DiagnosticPublisher::new(server);

        publisher.request_pull_diagnostic_refresh(true);
        server.shutdown_token.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let metrics = server.diagnostics.metrics_snapshot();
        assert_eq!(metrics.refreshes_requested, 1);
        assert_eq!(metrics.refreshes_sent, 0, "admitted task must not send");
    }

    fn rust_server_config() -> (String, BridgeServerConfig) {
        (
            "rust_ls".to_string(),
            BridgeServerConfig {
                cmd: vec!["true".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
            },
        )
    }

    /// Settings with a configured rust host server; `_self` host bridging is set
    /// to `enabled` for the rust language.
    fn rust_settings(self_enabled: bool) -> WorkspaceSettings {
        let (name, cfg) = rust_server_config();
        let mut language_servers = HashMap::new();
        language_servers.insert(name, cfg);
        let mut languages = HashMap::new();
        languages.insert(
            "rust".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    HOST_BRIDGE_KEY.to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(self_enabled),
                        aggregation: None,
                    },
                )])),
                ..Default::default()
            },
        );
        WorkspaceSettings {
            auto_install: false,
            language_servers,
            languages,
            ..Default::default()
        }
    }

    /// [`rust_settings`]`(true)` plus the publish wire seal on the rust entry:
    /// `layers.aggregation."textDocument/publishDiagnostics".priorities = []`.
    /// (On the exact language entry because `apply_settings` takes the raw
    /// `WorkspaceSettings`, bypassing Phase 2 base resolution
    /// (`resolve_base_configs`) — in production that phase field-merges the
    /// `_` wildcard's `layers` into every configured language, so a
    /// `languages._` seal DOES reach a language with its own entry there.)
    fn rust_settings_with_publish_seal() -> WorkspaceSettings {
        let mut settings = rust_settings(true);
        let lang = settings
            .languages
            .get_mut("rust")
            .expect("rust_settings defines the rust language");
        lang.layers = Some(LayersConfig {
            aggregation: Some(HashMap::from([(
                "textDocument/publishDiagnostics".to_string(),
                LayerAggregationConfig {
                    priorities: Some(Vec::new()),
                    strategy: None,
                },
            )])),
        });
        settings
    }

    /// `service.inner()` borrows from `service`, so the harness is set up inline
    /// per test; this registers the rust grammar so `detect_language` resolves a
    /// `.rs` document to `"rust"`.
    fn register_rust(server: &Kakehashi) {
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
    }

    #[tokio::test]
    async fn region_push_dropped_when_origin_server_disabled() {
        // Unlike publish_host_push, publish_region_push previously had no
        // config-validity check at all: a disabled server's still-live
        // connection could keep pushing region diagnostics indefinitely.
        // `enabled` is a per-server-name property (not per-language), so the
        // gate only needs the pushing server's own resolved config.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        let host_uri = Url::parse("file:///test/region_disabled.rs").unwrap();
        server.documents.insert(
            host_uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "sql",
            "region-1",
        );
        server
            .bridge
            .register_opened_document_for_test(
                &host_uri,
                &virtual_uri,
                &crate::lsp::bridge::ConnectionKey::for_server("sql_ls"),
            )
            .await;

        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "sql_ls".to_string(),
            BridgeServerConfig {
                cmd: vec!["sql-ls".to_string()],
                enabled: Some(false),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(settings);

        DiagnosticPublisher::new(server)
            .publish_region_push(
                &virtual_uri.to_uri_string(),
                "sql_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("boom")],
            )
            .await;

        assert!(
            server.diagnostics.snapshot(&host_uri).is_empty(),
            "a disabled server's region push must not be recorded"
        );
    }

    #[tokio::test]
    async fn host_push_accepted_for_open_self_bridged_doc() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));

        let uri = Url::parse("file:///test/host.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        DiagnosticPublisher::new(server)
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("boom")],
            )
            .await;

        let snap = server.diagnostics.snapshot(&uri);
        let host_slots = snap
            .get(&DiagnosticSource::Host)
            .expect("a Host slot should be recorded for an open _self-bridged doc");
        assert_eq!(host_slots["rust_ls"].diagnostics.len(), 1);
        assert_eq!(host_slots["rust_ls"].diagnostics[0].message, "boom");
    }

    #[tokio::test]
    async fn push_batch_publishes_only_the_latest_host_state_once() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));

        let uri = Url::parse("file:///test/host-batch.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);

        let published_hosts = publisher
            .publish_push_batch(vec![
                DiagnosticPush {
                    uri: uri.to_string(),
                    server: "rust_ls".to_string(),
                    connection_id: ProgressConnectionId::for_test(1),
                    diagnostics: vec![diag("superseded")],
                },
                DiagnosticPush {
                    uri: uri.to_string(),
                    server: "rust_ls".to_string(),
                    connection_id: ProgressConnectionId::for_test(1),
                    diagnostics: vec![diag("latest")],
                },
            ])
            .await;

        assert_eq!(published_hosts, 1, "one host should be published once");
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Unchanged,
            "the batch must already have published its final aggregate"
        );
        let snapshot = server.diagnostics.snapshot(&uri);
        assert_eq!(
            snapshot[&DiagnosticSource::Host]["rust_ls"].diagnostics[0].message,
            "latest"
        );
    }

    #[tokio::test]
    async fn coverage_bumps_on_push_origin_not_on_pull_layer() {
        // #497: only push/eviction-origin republishes bump the coverage version. An
        // editor-originated `publish_pull_layer` changes the set but must NOT bump —
        // bumping there would strand the host dirty between the editor's own pulls and
        // defeat the gate during active editing (the debounce-vs-pull race).
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));
        let uri = Url::parse("file:///test/host.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);

        assert!(!server.diagnostics.is_dirty(), "fresh host is clean");

        // A push-origin change bumps coverage → dirty.
        publisher
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("boom")],
            )
            .await;
        assert!(
            server.diagnostics.is_dirty(),
            "a push-origin change makes the host dirty"
        );

        // The editor pulls and is answered against the current version → clean.
        let v = server.diagnostics.current_version(&uri);
        server.diagnostics.mark_served(&uri, v);
        assert!(
            !server.diagnostics.is_dirty(),
            "a covering pull clears dirty"
        );

        // An editor-originated pull-layer republish changes the merged set (it is
        // published) but must NOT re-dirty the host.
        publisher
            .publish_pull_layer(&uri, vec![diag("from-pull-layer")])
            .await;
        assert!(
            !server.diagnostics.is_dirty(),
            "pull-layer (editor-origin) republish must not bump the coverage version"
        );
    }

    #[tokio::test]
    async fn republish_reports_whether_the_recorded_set_changed() {
        // `republish` reports whether the merged-and-RECORDED set changed (the
        // wire send may be withheld by the quiet window or sealed) — the gate the
        // push-origin paths use to decide whether to also emit
        // `workspace/diagnostic/refresh` (so a pull-mode editor re-pulls and sees an
        // async host push; push/pull-divergence, #422). A non-empty first publish is
        // a change; re-publishing the identical cache is not (so a no-op push won't
        // spam refreshes). Driven directly (no socket/init) so it stays fast.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));

        let uri = Url::parse("file:///test/host_changed.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let publisher = DiagnosticPublisher::new(server);
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "rust_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("boom")],
        );
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "first publish of a non-empty host set must report a change (drives the refresh)"
        );
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Unchanged,
            "re-publishing the identical set must report unchanged (no redundant refresh)"
        );
    }

    #[tokio::test]
    async fn publish_sealed_reflects_the_layer_gate() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        let uri = Url::parse("file:///test/host_seal.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);

        server.settings_manager.apply_settings(rust_settings(true));
        assert!(
            !publisher.publish_sealed(&uri),
            "default layer priorities publish as before"
        );

        server
            .settings_manager
            .apply_settings(rust_settings_with_publish_seal());
        assert!(
            publisher.publish_sealed(&uri),
            "publishDiagnostics priorities = [] seals the wire sends"
        );

        let closed = Url::parse("file:///test/never_opened.rs").unwrap();
        assert!(
            !publisher.publish_sealed(&closed),
            "an unresolvable document fails open (didClose's clearing publish goes out)"
        );
    }

    #[tokio::test]
    async fn republish_under_seal_keeps_the_change_contract() {
        // With the wire sealed, the merge and the last-published recording must
        // behave exactly as in publish mode: a changed set reports Changed
        // (drives the coverage bump + refresh — the sealed setup's delivery
        // signal), an identical re-merge reports Unchanged. If the seal skipped
        // the recording,
        // every push would re-report "changed" and spam refreshes forever.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server
            .settings_manager
            .apply_settings(rust_settings_with_publish_seal());
        let uri = Url::parse("file:///test/host_sealed_contract.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "rust_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("boom")],
        );

        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "a changed set reports Changed under the seal (drives the refresh)"
        );
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Unchanged,
            "the sealed set was recorded as last-published, so a re-merge is unchanged"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_refresh_stream_is_bounded_by_max_wait() {
        let aggregator = Arc::new(DiagnosticAggregator::new());
        let snapshot = aggregator
            .begin_forwarded_refresh_debounce()
            .expect("first refresh claims the debounce task");
        let deadline = snapshot.last_activity_at + FORWARDED_REFRESH_MAX_WAIT;
        let waiter_aggregator = Arc::clone(&aggregator);
        let waiter = tokio::spawn(async move {
            super::wait_for_forwarded_refresh_settle(
                &waiter_aggregator,
                snapshot,
                deadline,
                FORWARDED_REFRESH_SETTLE_WINDOW,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        for _ in 0..11 {
            tokio::time::advance(std::time::Duration::from_millis(90)).await;
            assert!(
                aggregator.begin_forwarded_refresh_debounce().is_none(),
                "continuous activity reuses the debounce task"
            );
        }
        // The last activity was at 990 ms, so an unbounded quiet-window-only
        // implementation would remain parked until 1090 ms. The anchored
        // one-second deadline must release it after just 10 ms.
        tokio::time::advance(std::time::Duration::from_millis(10)).await;
        assert!(matches!(
            waiter.await.expect("debounce task completes at max wait"),
            crate::lsp::diagnostic_cache::ForwardedRefreshWait::MaxWait {
                send_trailing: true,
                ..
            }
        ));
        assert!(
            aggregator.begin_forwarded_refresh_debounce().is_none(),
            "a max-wait fire must keep the cycle active until activity becomes quiet"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forwarded_refresh_waiter_uses_custom_quiet_window() {
        let aggregator = Arc::new(DiagnosticAggregator::new());
        let snapshot = aggregator
            .begin_forwarded_refresh_debounce()
            .expect("first refresh claims the debounce task");
        aggregator.mark_forwarded_refresh_covered(snapshot.generation);
        assert!(aggregator.begin_forwarded_refresh_debounce().is_none());
        let deadline = snapshot.last_activity_at + std::time::Duration::from_millis(800);
        let waiter_aggregator = Arc::clone(&aggregator);
        let waiter = tokio::spawn(async move {
            super::wait_for_forwarded_refresh_settle(
                &waiter_aggregator,
                snapshot,
                deadline,
                std::time::Duration::from_millis(250),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        tokio::time::advance(std::time::Duration::from_millis(248)).await;
        assert!(!waiter.is_finished());
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert!(matches!(
            waiter.await.unwrap(),
            crate::lsp::diagnostic_cache::ForwardedRefreshWait::SendTrailing(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn trailing_republish_flushes_the_withheld_wire_send() {
        // Inside the quiet window a changed merge is withheld from the wire
        // (dirty) and one trailing task is parked; once the window elapses the
        // task re-runs republish, which sends despite its own merge comparing
        // unchanged, and clears the dirty marker — after which an identical
        // re-merge is a plain skip again.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));
        let uri = Url::parse("file:///test/host_trailing.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);

        // Leading edge: first changed set passes through immediately.
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "rust_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("first")],
        );
        assert_eq!(publisher.republish(&uri).await, RepublishOutcome::Changed);
        assert!(!server.diagnostics.wire_gate_is_dirty(&uri));

        // A second change inside the window: reported as changed, but the wire
        // send is withheld (dirty) with a trailing task parked.
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "rust_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("second")],
        );
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "a withheld change still reports changed (drives the refresh nudge)"
        );
        assert!(
            server.diagnostics.wire_gate_is_dirty(&uri),
            "the change was withheld from the wire"
        );

        // The window elapses; the parked trailing task re-runs republish.
        tokio::time::advance(super::WIRE_PUBLISH_QUIET_WINDOW).await;
        // Advance virtual time once more so Tokio drains the woken trailing
        // task through its nested awaits without relying on a poll count.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(
            !server.diagnostics.wire_gate_is_dirty(&uri),
            "the trailing republish flushed the withheld set"
        );
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Unchanged,
            "after the flush an identical re-merge is a plain unchanged skip"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_pending_trailing_publish() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));
        let uri = Url::parse("file:///test/host_trailing_shutdown.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "rust_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("first")],
        );
        publisher.republish(&uri).await;
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "rust_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("withheld")],
        );
        publisher.republish(&uri).await;
        assert!(server.diagnostics.wire_gate_is_dirty(&uri));

        server.shutdown_token.cancel();
        tokio::time::advance(WIRE_PUBLISH_QUIET_WINDOW).await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(!server.diagnostics.wire_gate_is_dirty(&uri));
        assert!(!server.diagnostics.wire_gate_take_pending(&uri));
    }

    #[tokio::test]
    async fn republish_defers_while_region_geometry_is_unknown() {
        // `did_change` clears the visible tree; until the off-ingress reparse
        // lands, `doc.snapshot()` is `None` and the regions' current offsets are
        // unknown. A republish in that window must DEFER (publish nothing,
        // record nothing) instead of merging with empty offsets — which silently
        // dropped every region push slot and flapped the editor between the full
        // and the region-less set on each edit cycle. The document here is
        // inserted with no tree, modelling exactly that window.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///test/host_geometry_unknown.md").unwrap();
        server.documents.insert(
            uri.clone(),
            "```lua\nlocal x = 1\n```\n".to_string(),
            Some("markdown".to_string()),
            None, // no tree: snapshot() is None
        );
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Region("region-1".to_string()),
            "lua_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("unused variable")],
        );
        let publisher = DiagnosticPublisher::new(server);

        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Deferred,
            "a republish with live region slots but no parse snapshot must defer"
        );
        // Nothing was recorded as last-published: once the parse lands, the same
        // merged set must still count as a change (the deferred publish happens).
        assert!(
            server.diagnostics.published_set_changed(&uri, &[]),
            "deferral must not record a last-published set"
        );
    }

    #[tokio::test]
    async fn republish_proceeds_without_a_tree_when_region_slots_are_empty() {
        // The deferral is keyed on NON-EMPTY region slots (mirroring
        // `has_region_slots`, the reparse loop's backstop gate). A host whose
        // only region slot is an empty clearing push needs no geometry — its
        // merge drops nothing real — so it publishes even while the tree is
        // pending. If this deferred instead, the post-parse backstop would skip
        // it (`has_region_slots` is false) and the publish would be lost.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///test/host_empty_region_slot.md").unwrap();
        server.documents.insert(
            uri.clone(),
            "```lua\nlocal x = 1\n```\n".to_string(),
            Some("markdown".to_string()),
            None,
        );
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Region("region-1".to_string()),
            "lua_ls".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            Vec::new(), // a clearing push: nothing to anchor
        );
        let publisher = DiagnosticPublisher::new(server);

        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "an all-empty region snapshot needs no geometry; the (empty) merge publishes"
        );
    }

    #[tokio::test]
    async fn current_region_offsets_distinguishes_unknown_from_absent_geometry() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let publisher = DiagnosticPublisher::new(server);

        // Open document without a tree: geometry UNKNOWN → None (defer).
        let pending = Url::parse("file:///test/pending.md").unwrap();
        server.documents.insert(
            pending.clone(),
            "# doc".to_string(),
            Some("markdown".to_string()),
            None,
        );
        assert!(
            publisher.current_region_offsets(&pending).is_none(),
            "an open document with no parse snapshot has unknown geometry"
        );

        // Closed (never-opened) document: geometry ABSENT → Some(empty) (stale
        // region slots legitimately drop; didClose's clearing publish proceeds).
        let closed = Url::parse("file:///test/closed.md").unwrap();
        assert!(
            publisher
                .current_region_offsets(&closed)
                .is_some_and(|offsets| offsets.is_empty()),
            "a closed document has no regions to anchor, not unknown geometry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn closed_host_republish_bypasses_the_quiet_window() {
        // A republish for a closed host is (or races) didClose's clearing
        // publish: it must never be deferred — a deferred clear dies on the
        // trailing task's guards, pinning stale diagnostics on a closed buffer
        // — and must never stamp gate state that would defer the clearing
        // publish serialized behind it.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        // Slots recorded for a URI that was never inserted as a document. The
        // pull-layer slot is host-local and unfiltered, so it survives the
        // closed-document merge (Host slots would be stale-filtered away).
        let uri = Url::parse("file:///test/closed_host_bypass.rs").unwrap();
        let publisher = DiagnosticPublisher::new(server);
        server.diagnostics.set_pull_layer(&uri, vec![diag("stale")]);

        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "the first closed-host publish goes straight to the wire"
        );
        server.diagnostics.set_pull_layer(&uri, Vec::new());
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "an immediate second closed-host publish must not be quiet-window deferred"
        );
        assert!(
            !server.diagnostics.wire_gate_is_dirty(&uri),
            "closed-host publishes leave no wire-gate state behind"
        );
        assert!(
            !server.diagnostics.wire_gate_take_pending(&uri),
            "no gate entry was minted for the closed host"
        );
    }

    #[tokio::test]
    async fn deferred_region_push_still_nudges_pull_clients() {
        // A spontaneous region push landing in the tree-pending window defers
        // the publish — but the CACHE changed, so the push-origin path must
        // still bump coverage (making the workspace dirty) so the gated
        // workspace/diagnostic/refresh nudges pull-mode clients. Losing the
        // nudge here would revive the #496 rot: a client whose own re-pull
        // raced ahead of the push keeps a stale set until the next edit.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let host_uri = Url::parse("file:///test/deferred_nudge.md").unwrap();
        server.documents.insert(
            host_uri.clone(),
            "```lua\nlocal x = 1\n```\n".to_string(),
            Some("markdown".to_string()),
            None, // tree pending: geometry unknown
        );
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            "region-1",
        );
        server
            .bridge
            .register_opened_document_for_test(
                &host_uri,
                &virtual_uri,
                &crate::lsp::bridge::ConnectionKey::for_server("lua_ls"),
            )
            .await;
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "lua_ls".to_string(),
            BridgeServerConfig {
                cmd: vec!["lua-language-server".to_string()],
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(settings);

        assert!(!server.diagnostics.is_dirty(), "fresh host is clean");
        DiagnosticPublisher::new(server)
            .publish_region_push(
                &virtual_uri.to_uri_string(),
                "lua_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("unused variable")],
            )
            .await;
        assert!(
            server.diagnostics.is_dirty(),
            "a geometry-deferred push must still bump coverage (drives the refresh nudge)"
        );
    }

    #[tokio::test]
    async fn host_push_dropped_when_document_not_open() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));

        // URI is never inserted into the document store.
        let uri = Url::parse("file:///test/not_open.rs").unwrap();
        DiagnosticPublisher::new(server)
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("x")],
            )
            .await;

        assert!(
            server.diagnostics.snapshot(&uri).is_empty(),
            "a push for a non-open document must be dropped"
        );
    }

    #[tokio::test]
    async fn host_push_dropped_when_self_bridging_disabled() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        // Same rust server, but `_self` host bridging is explicitly disabled.
        server.settings_manager.apply_settings(rust_settings(false));

        let uri = Url::parse("file:///test/host.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        DiagnosticPublisher::new(server)
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("y")],
            )
            .await;

        assert!(
            server.diagnostics.snapshot(&uri).is_empty(),
            "a push for a language without _self host bridging must be dropped"
        );
    }

    #[tokio::test]
    async fn host_push_dropped_for_non_host_server() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        // rust_ls is the configured host server; _self is enabled for rust.
        server.settings_manager.apply_settings(rust_settings(true));

        let uri = Url::parse("file:///test/host.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        // The push comes from a server that is NOT a configured host server for rust.
        DiagnosticPublisher::new(server)
            .publish_host_push(
                uri.as_str(),
                "some_other_server".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("z")],
            )
            .await;

        assert!(
            server.diagnostics.snapshot(&uri).is_empty(),
            "a push from a server that is not a host server for the language must be dropped"
        );
    }

    #[tokio::test]
    async fn host_slots_filtered_from_publish_after_self_disabled() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));

        let uri = Url::parse("file:///test/host.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);
        publisher
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("e")],
            )
            .await;
        assert!(
            server
                .diagnostics
                .snapshot(&uri)
                .contains_key(&DiagnosticSource::Host),
            "host slot is recorded while _self is enabled"
        );

        // Disable _self for rust; the publish-time filter should now exclude the
        // stale host slot, while the cache itself keeps it (cleared on didClose).
        server.settings_manager.apply_settings(rust_settings(false));
        let mut snapshot = server.diagnostics.snapshot(&uri);
        publisher.filter_stale_host_slots(&uri, &mut snapshot);
        assert!(
            !snapshot.contains_key(&DiagnosticSource::Host),
            "stale host slots are filtered out of the publish after _self is disabled"
        );
        assert!(
            server
                .diagnostics
                .snapshot(&uri)
                .contains_key(&DiagnosticSource::Host),
            "the cache still holds the slot; only the publish snapshot is filtered"
        );
    }

    #[tokio::test]
    async fn host_push_dropped_after_server_disabled_but_stale_slot_lingers() {
        // A server disabled via `languageServers.*.enabled: false` after it
        // already spawned and pushed: its still-live connection's next push
        // must be dropped (not recorded as new data), matching every other
        // "not a host server" case. Whether the *previously* published
        // diagnostics get proactively cleared is a separate, pre-existing,
        // deferred concern (`republish`'s doc comment; `_self`-disable and
        // empty-cmd have the identical gap) — not asserted here.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        register_rust(server);
        server.settings_manager.apply_settings(rust_settings(true));

        let uri = Url::parse("file:///test/host_disabled.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let publisher = DiagnosticPublisher::new(server);

        // Enabled: the push is recorded.
        publisher
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("boom")],
            )
            .await;

        // Disable the server (not `_self` — the server itself).
        let (name, mut cfg) = rust_server_config();
        cfg.enabled = Some(false);
        let mut disabled_settings = rust_settings(true);
        disabled_settings.language_servers.insert(name, cfg);
        server.settings_manager.apply_settings(disabled_settings);

        // The still-live connection (not yet torn down) sends another push
        // with different diagnostics — must not be recorded.
        publisher
            .publish_host_push(
                uri.as_str(),
                "rust_ls".to_string(),
                ProgressConnectionId::for_test(1),
                vec![diag("still-live-push")],
            )
            .await;

        let snap = server.diagnostics.snapshot(&uri);
        let host_slots = snap
            .get(&DiagnosticSource::Host)
            .expect("the cache still holds the pre-disable slot");
        assert_eq!(
            host_slots["rust_ls"].diagnostics[0].message, "boom",
            "the post-disable push must not overwrite the cached slot"
        );
    }

    #[tokio::test]
    async fn evict_connection_diagnostics_drops_only_the_dead_connection() {
        // The seam the forwarding loop's EvictConnectionDiagnostics arm invokes:
        // the publisher evicts the dead connection's slots (and republishes the
        // affected host) while the live connection's slots survive (#469).
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///test/host.rs").unwrap();
        let dead = ProgressConnectionId::for_test(1);
        let live = ProgressConnectionId::for_test(2);
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "dead_ls".to_string(),
            Some(dead),
            vec![diag("from dead")],
        );
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "live_ls".to_string(),
            Some(live),
            vec![diag("from live")],
        );

        DiagnosticPublisher::new(server)
            .evict_connection_diagnostics(dead)
            .await;

        let snap = server.diagnostics.snapshot(&uri);
        let host = snap
            .get(&DiagnosticSource::Host)
            .expect("the surviving host slot keeps the source alive");
        assert!(
            !host.contains_key("dead_ls"),
            "the dead connection's slot is evicted"
        );
        assert!(
            host.contains_key("live_ls"),
            "the live connection's slot survives the eviction"
        );
    }

    #[tokio::test]
    async fn evict_connection_diagnostics_republishes_the_changed_set() {
        // The eviction path drives `workspace/diagnostic/refresh` off the same
        // changed-set gate the spontaneous-push paths use (#499): a crashed server's
        // slot is evicted and the now-empty merged set is republished as a *change*
        // the editor (pull-mode) has no event to learn about. We assert that gate
        // (republish's bool) rather than the capability-gated, fire-and-forget
        // refresh emission, matching `republish_reports_whether_the_published_set_changed`.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///test/host.rs").unwrap();
        let dead = ProgressConnectionId::for_test(1);
        server.diagnostics.record(
            &uri,
            DiagnosticSource::Host,
            "dead_ls".to_string(),
            Some(dead),
            vec![diag("from dead")],
        );
        let publisher = DiagnosticPublisher::new(server);
        // Establish the editor's baseline: the non-empty set is published.
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Changed,
            "the initial non-empty set is a change"
        );

        publisher.evict_connection_diagnostics(dead).await;

        // The eviction already republished the now-empty (changed) set, so a
        // follow-up republish is a no-op — confirming the eviction itself carried
        // the change that gates the refresh. The capability-gated, fire-and-forget
        // refresh spawn isn't asserted directly: that needs driving full server
        // `initialize` (server→client messages are suppressed until then), the
        // mock-client harness this file's tests deliberately avoid — true
        // end-to-end refresh coverage belongs in the e2e suite.
        assert_eq!(
            publisher.republish(&uri).await,
            RepublishOutcome::Unchanged,
            "eviction published the empty changed set; re-publishing is unchanged"
        );
    }

    use crate::lsp::diagnostic_cache::SourceSlots;
    use std::collections::HashSet;

    /// Build a snapshot for `host` with a `PullLayer` blob plus two `Host` push
    /// slots: `ra` (a pull-driven server that also pushed) and `linter` (a
    /// push-driven server).
    fn snapshot_with_pull_layer_and_two_host_pushes(host: &Url) -> SourceSlots {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(host, vec![diag("pulled")]);
        agg.record(
            host,
            DiagnosticSource::Host,
            "ra".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("ra-push")],
        );
        agg.record(
            host,
            DiagnosticSource::Host,
            "linter".to_string(),
            Some(ProgressConnectionId::for_test(2)),
            vec![diag("linter-push")],
        );
        agg.snapshot(host)
    }

    #[test]
    fn retain_drops_pull_driven_push_slot_but_keeps_push_driven_and_pull_blob() {
        let host = Url::parse("file:///test/host.rs").unwrap();
        let mut snap = snapshot_with_pull_layer_and_two_host_pushes(&host);

        retain_non_pull_driven_push_slots(&mut snap, &HashSet::from(["ra".to_string()]));

        let host_slots = snap
            .get(&DiagnosticSource::Host)
            .expect("the Host source survives because the push-driven slot remains");
        assert!(
            !host_slots.contains_key("ra"),
            "a pull-driven server's push slot is dropped (the pull covers it)"
        );
        assert!(
            host_slots.contains_key("linter"),
            "a push-driven server's slot is kept (the pull never covered it)"
        );
        assert!(
            snap.contains_key(&DiagnosticSource::PullLayer),
            "the pull blob itself is never filtered"
        );
    }

    #[test]
    fn retain_is_a_noop_without_a_pull_layer() {
        // pullFallback-off / no-pull-yet path: with no PullLayer there is nothing
        // to double-count against, so even a pull-driven server's spontaneous
        // push is published (keeps #380 closed).
        let host = Url::parse("file:///test/host.rs").unwrap();
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host,
            DiagnosticSource::Host,
            "ra".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("ra-push")],
        );
        let mut snap = agg.snapshot(&host);

        retain_non_pull_driven_push_slots(&mut snap, &HashSet::from(["ra".to_string()]));

        assert!(
            snap[&DiagnosticSource::Host].contains_key("ra"),
            "no PullLayer present → no suppression"
        );
    }

    #[test]
    fn retain_drops_a_source_left_empty_after_filtering() {
        let host = Url::parse("file:///test/host.rs").unwrap();
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host, vec![diag("pulled")]);
        agg.record(
            &host,
            DiagnosticSource::Host,
            "ra".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("ra-push")],
        );
        let mut snap = agg.snapshot(&host);

        retain_non_pull_driven_push_slots(&mut snap, &HashSet::from(["ra".to_string()]));

        assert!(
            !snap.contains_key(&DiagnosticSource::Host),
            "a source whose every server was pull-driven is removed entirely"
        );
        assert!(snap.contains_key(&DiagnosticSource::PullLayer));
    }

    #[test]
    fn retain_with_empty_pull_driven_set_keeps_everything() {
        let host = Url::parse("file:///test/host.rs").unwrap();
        let mut snap = snapshot_with_pull_layer_and_two_host_pushes(&host);

        retain_non_pull_driven_push_slots(&mut snap, &HashSet::new());

        let host_slots = &snap[&DiagnosticSource::Host];
        assert!(host_slots.contains_key("ra") && host_slots.contains_key("linter"));
    }
}
