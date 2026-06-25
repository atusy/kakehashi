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
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) {
        if VirtualDocumentUri::is_virtual_uri(&uri) {
            self.publish_region_push(&uri, server, connection_id, diagnostics)
                .await;
        } else {
            self.publish_host_push(&uri, server, connection_id, diagnostics)
                .await;
        }
    }

    /// Record a `_self` host-layer push and republish the host (host-document-bridge).
    ///
    /// A host server pushes for the **real** host URI in host coordinates. Accept
    /// it only when that URI names an **open** document AND the pushing `server` is
    /// a configured `_self` host server for that document's language. This drops
    /// both the common stray case (a push for a workspace file the editor doesn't
    /// have open) and a real-URI push from a server that isn't a host server for
    /// this language. Host diagnostics need no coordinate transform.
    pub(crate) async fn publish_host_push(
        &self,
        host_uri: &str,
        server: String,
        connection_id: ProgressConnectionId,
        diagnostics: Vec<Diagnostic>,
    ) {
        let Ok(host) = Url::parse(host_uri) else {
            return;
        };
        let Some(language_name) = self.open_document_language(&host) else {
            return; // not an open document
        };
        let settings = self.settings_manager.load_settings();
        let is_host_server = self
            .bridge
            .get_host_configs_for_language(&settings, &language_name)
            .iter()
            .any(|config| config.server_name == server);
        if !is_host_server {
            // `_self` host bridging is off for this language, or `server` is not a
            // configured host server for it — not a host-layer contribution.
            return;
        }
        self.aggregator.record(
            &host,
            DiagnosticSource::Host,
            server,
            Some(connection_id),
            diagnostics,
        );
        self.republish(&host).await;
    }

    /// Detect the language of an *open* document, or `None` if it isn't open.
    ///
    /// Borrows the document text directly (no `snapshot()` clone) — detection
    /// never touches the tree, so this also avoids spuriously dropping a push for
    /// an open-but-not-yet-parsed document.
    fn open_document_language(&self, uri: &Url) -> Option<String> {
        let doc = self.documents.get(uri)?;
        self.language
            .detect_language(uri.path(), doc.text(), None, doc.language_id())
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
        // from the snapshot (no key clones on this republish hot path).
        let mut push_servers = std::collections::HashSet::new();
        for (source, servers) in snapshot.iter() {
            if matches!(source, DiagnosticSource::PullLayer) {
                continue;
            }
            push_servers.extend(servers.keys().map(String::as_str));
        }
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
    pub(crate) async fn publish_region_push(
        &self,
        virtual_uri: &str,
        server: String,
        connection_id: ProgressConnectionId,
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
            Some(connection_id),
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
    pub(crate) async fn clear_pull_layer(&self, host: &Url) {
        self.aggregator
            .evict_source(host, &DiagnosticSource::PullLayer);
        self.republish(host).await;
    }

    /// Evict every diagnostic slot a now-exited downstream connection produced and
    /// republish the affected hosts (#469). Called when a connection's reader exits
    /// (crash/respawn); a restart's slots carry a fresh connection id and survive,
    /// so this clears only the dead server's contribution.
    pub(crate) async fn evict_connection_diagnostics(&self, connection_id: ProgressConnectionId) {
        let affected = self.aggregator.evict_connection(connection_id);
        for host in affected {
            self.republish(&host).await;
        }
    }

    /// Drop the host's cache entry and publish the now-empty set (host `didClose`).
    pub(crate) async fn clear_host(&self, host: &Url) {
        self.aggregator.evict_host(host);
        self.republish(host).await;
        // The host is closed: forget its last-published set so the entry does not
        // linger and a later re-open publishes afresh (#422). Done after the
        // clear-republish above so that publish still sees the prior set.
        self.aggregator.forget_published(host);
        // didClose is off the hot path: reclaim republish-lock entries whose lock
        // now has no live holder — this host's, once the clear-republish above
        // released it, plus any earlier-closed hosts that have since drained (#466).
        self.aggregator.reclaim_republish_locks();
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
        // time this host's republish lock is held.
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

        // Suppress a republish that would re-send the exact set the editor already
        // has — a redundant publishDiagnostics is needless flicker/noise (#422).
        // Done under the per-host republish lock (held above), so the compare-and-set
        // is serialized with other republishes for this host.
        if !self.aggregator.published_set_changed(host, &diagnostics) {
            log::debug!(
                target: LOG_TARGET,
                "skip republish for {host}: merged set unchanged ({} diagnostics)",
                diagnostics.len()
            );
            return;
        }

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
        WorkspaceSettings,
    };
    use std::collections::HashMap;
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{Position, Range};

    fn diag(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn rust_server_config() -> (String, BridgeServerConfig) {
        (
            "rust_ls".to_string(),
            BridgeServerConfig {
                cmd: vec!["true".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                root_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
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
