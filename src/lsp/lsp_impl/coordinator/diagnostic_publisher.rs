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
    /// it only when that URI names an **open** document AND the pushing `server` is
    /// a configured `_self` host server for that document's language. This drops
    /// both the common stray case (a push for a workspace file the editor doesn't
    /// have open) and a real-URI push from a server that isn't a host server for
    /// this language. Host diagnostics need no coordinate transform.
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
        self.aggregator
            .record(&host, DiagnosticSource::Host, server, diagnostics);
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
        // Borrowed-`&str` set: O(1) membership, no `server_name` clones.
        let valid: std::collections::HashSet<&str> =
            configs.iter().map(|c| c.server_name.as_str()).collect();
        let slots = entry.get_mut();
        slots.retain(|server, _| valid.contains(server.as_str()));
        if slots.is_empty() {
            entry.remove();
        }
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

        let mut snapshot = self.aggregator.snapshot(host);
        // Drop Host push slots whose server is no longer a configured `_self` host
        // server for the document's current language — so a host server's pushed
        // diagnostics don't linger in the editor after the user disables `_self`
        // (or unconfigures the server) via `workspace/didChangeConfiguration`. The
        // slots stay cached (cleared on `didClose`); they're just filtered out of
        // this publish. (The analogous Region/config-change re-merge is deferred.)
        self.filter_stale_host_slots(host, &mut snapshot);
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
            .publish_host_push(uri.as_str(), "rust_ls".to_string(), vec![diag("boom")])
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
            .publish_host_push(uri.as_str(), "rust_ls".to_string(), vec![diag("x")])
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
            .publish_host_push(uri.as_str(), "rust_ls".to_string(), vec![diag("y")])
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
            .publish_host_push(uri.as_str(), "rust_ls".to_string(), vec![diag("e")])
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
}
