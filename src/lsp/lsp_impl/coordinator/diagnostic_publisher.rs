//! The single proactive `textDocument/publishDiagnostics` publisher
//! (push-propagation-diagnostic-forwarding).
//!
//! Every proactive diagnostic feed writes slots into the [`DiagnosticAggregator`]
//! and then asks this publisher to **republish** the host: it snapshots the
//! cache, transforms region push slots to host coordinates against the region's
//! *current* offset (lazy re-anchor), merges with the host-event pull blob, and
//! emits one `publishDiagnostics`. Routing every feed through one publisher is
//! what keeps sibling regions intact against the client's URI-level clobber.

use std::collections::HashMap;
use std::sync::Arc;

use url::Url;

use crate::document::DocumentStore;
use crate::language::{InjectionResolver, LanguageCoordinator};
use crate::lsp::bridge::{BridgeCoordinator, RegionOffset, VirtualDocumentUri};
use crate::lsp::diagnostic_cache::{
    DiagnosticAggregator, DiagnosticSource, merge_cached_diagnostics,
};
use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::Diagnostic;

/// Logging target for proactive push diagnostics.
const LOG_TARGET: &str = "kakehashi::push_diag";

/// Bundles the state needed to merge the cache and publish for a host, so the
/// notification feeds (reader push, host-event pull) can trigger a republish
/// without each holding `Kakehashi`.
pub(crate) struct DiagnosticPublisher {
    client: Client,
    language: Arc<LanguageCoordinator>,
    documents: Arc<DocumentStore>,
    bridge: Arc<BridgeCoordinator>,
    settings_manager: Arc<SettingsManager>,
    aggregator: Arc<DiagnosticAggregator>,
}

impl DiagnosticPublisher {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: Arc::clone(&server.language),
            documents: Arc::clone(&server.documents),
            bridge: Arc::clone(&server.bridge),
            settings_manager: Arc::clone(&server.settings_manager),
            aggregator: Arc::clone(&server.diagnostics),
        }
    }

    /// Route a downstream `publishDiagnostics` push into the cache, classifying by
    /// its URI: a virtual injection URI becomes a region push; anything else is
    /// treated as a candidate `_self` host-layer push for the real host document.
    pub(crate) async fn publish_push(
        &self,
        uri: String,
        server: String,
        diagnostics: Vec<Diagnostic>,
    ) {
        if VirtualDocumentUri::is_virtual_uri(&uri) {
            self.publish_region_push(&uri, server, diagnostics).await;
        } else {
            self.publish_host_push(&uri, server, diagnostics).await;
        }
    }

    /// Record a `_self` host-layer push and republish the host (host-document-bridge).
    ///
    /// A host server pushes for the **real** host URI in host coordinates. Accept
    /// it only when that URI is an **open** document whose language has `_self`
    /// host bridging enabled — otherwise it is a stray push for the server's own
    /// workspace file (not a host-bridged doc the editor has open) and is dropped.
    /// Host diagnostics need no coordinate transform.
    pub(crate) async fn publish_host_push(
        &self,
        host_uri: &str,
        server: String,
        diagnostics: Vec<Diagnostic>,
    ) {
        let Ok(host) = Url::parse(host_uri) else {
            return;
        };
        let Some(language_name) = self.open_document_language(&host) else {
            return; // not an open document
        };
        let settings = self.settings_manager.load_settings();
        if self
            .bridge
            .get_host_configs_for_language(&settings, &language_name)
            .is_empty()
        {
            // `_self` host bridging is off for this language, or no host server is
            // configured — this real-URI push is not a host-layer contribution.
            return;
        }
        self.aggregator
            .record(&host, DiagnosticSource::Host, server, diagnostics);
        self.republish(&host).await;
    }

    /// Detect the language of an *open* document, or `None` if it isn't open.
    fn open_document_language(&self, uri: &Url) -> Option<String> {
        let doc = self.documents.get(uri)?;
        let snapshot = doc.snapshot()?;
        self.language
            .detect_language(uri.path(), snapshot.text(), None, doc.language_id())
    }

    /// Record a downstream region push and republish the host (Path A).
    ///
    /// `virtual_uri` is the URI the downstream published for; it is resolved to
    /// its host document + region id. A push for a URI that resolves to no live
    /// region (a closed/edited-away region, or a non-bridged document) is dropped.
    /// Diagnostics are stored in virtual coordinates and transformed at publish.
    pub(crate) async fn publish_region_push(
        &self,
        virtual_uri: &str,
        server: String,
        diagnostics: Vec<Diagnostic>,
    ) {
        let Some((host, region_id)) = self.bridge.resolve_virtual_uri(virtual_uri).await else {
            log::debug!(
                target: LOG_TARGET,
                "push for unresolved virtual uri {virtual_uri}, dropping"
            );
            return;
        };
        self.aggregator.record(
            &host,
            DiagnosticSource::Region(region_id),
            server,
            diagnostics,
        );
        self.republish(&host).await;
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

    /// Drop the host's cache entry and publish the now-empty set (host `didClose`).
    pub(crate) async fn clear_host(&self, host: &Url) {
        self.aggregator.evict_host(host);
        self.republish(host).await;
    }

    /// Merge the host's cached slots and publish the cumulative result. Region
    /// slots are transformed against the host document's *current* injection
    /// offsets; an empty merge clears the editor's diagnostics for the host.
    pub(crate) async fn republish(&self, host: &Url) {
        // Serialize the whole snapshot→merge→publish so concurrent republishes
        // (region push vs host-event pull) emit in order and a stale snapshot can
        // never publish after a fresh one (push-propagation-diagnostic-forwarding).
        //
        // The lock is held across the editor `publish_diagnostics` await below
        // because the ordering guarantee requires it (releasing before the send
        // would let two publishes reorder on the wire). The cost is that a slow
        // editor stalls *all* hosts' republishes — an accepted staging tradeoff of
        // the global lock; the deferred per-host lock shrinks the blast radius to
        // one host. publish_diagnostics is a fire-and-forget notification, so the
        // stall window is the client's outbound-channel send, not a round-trip.
        let _guard = self.aggregator.lock_republish().await;

        let snapshot = self.aggregator.snapshot(host);
        // Recompute injection offsets only when there are region push slots to
        // transform. A PullLayer-only snapshot (the common pull-driven case) needs
        // none, so skip the whole-document injection resolution — and shorten the
        // time the global republish lock is held.
        let region_offsets = if snapshot
            .keys()
            .any(|source| matches!(source, DiagnosticSource::Region(_)))
        {
            self.current_region_offsets(host)
        } else {
            HashMap::new()
        };
        let diagnostics = merge_cached_diagnostics(host, snapshot, &region_offsets);

        let lsp_uri = match crate::lsp::lsp_impl::url_to_uri(host) {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!(target: LOG_TARGET, "skip publish, bad host URI {host}: {e}");
                return;
            }
        };

        log::debug!(
            target: LOG_TARGET,
            "publishing {} merged diagnostics for {}",
            diagnostics.len(),
            host
        );
        self.client
            .publish_diagnostics(lsp_uri, diagnostics, None)
            .await;
    }

    /// Map each currently-resolvable injection region of the host document to its
    /// offset, recomputed from the live document so region push slots re-anchor
    /// after edits above them. Empty when the document is missing or has no
    /// injections.
    fn current_region_offsets(&self, host: &Url) -> HashMap<String, RegionOffset> {
        let mut offsets = HashMap::new();

        let Some(doc) = self.documents.get(host) else {
            return offsets;
        };
        let Some(snapshot) = doc.snapshot() else {
            return offsets;
        };
        let Some(language_name) =
            self.language
                .detect_language(host.path(), snapshot.text(), None, doc.language_id())
        else {
            return offsets;
        };
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return offsets;
        };

        for resolved in InjectionResolver::resolve_all(
            &self.language,
            self.bridge.node_tracker(),
            host,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        ) {
            offsets.insert(
                resolved.region.region_id.clone(),
                RegionOffset::with_per_line_offsets(
                    resolved.region.line_range.start,
                    resolved.line_column_offsets.clone(),
                ),
            );
        }
        offsets
    }
}
