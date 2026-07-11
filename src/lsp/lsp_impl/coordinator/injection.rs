use std::collections::HashSet;

use url::Url;

use crate::document::DocumentStore;
use crate::language::injection::{InjectionResolver, collect_all_injections};
use crate::language::{DocumentParserPool, LanguageCoordinator, LanguageEvent};
use crate::lsp::auto_install::AutoInstallManager;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::bridge::coordinator::BridgeInjection;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use crate::lsp::diagnostic_cache::{DiagnosticAggregator, DiagnosticSource};
use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::settings_manager::SettingsManager;
use tower_lsp_server::Client;

use crate::lsp::lsp_impl::{build_notifier, detect_document_language};

use super::InstallCoordinator;
use super::install::InstallCoordinatorDeps;

/// `Clone` is a cheap refcount bump of the shared coordinators (every field is a
/// `Client` / `Arc<_>` / `AutoInstallManager`, all `Clone`); it lets
/// `process_injections` hand an owned handle to a spawned off-ingress install task.
#[derive(Clone)]
pub(crate) struct InjectionCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    documents: std::sync::Arc<DocumentStore>,
    cache: std::sync::Arc<CacheCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    auto_install: AutoInstallManager,
    bridge: std::sync::Arc<BridgeCoordinator>,
    diagnostics: std::sync::Arc<DiagnosticAggregator>,
}

impl InjectionCoordinator {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            parser_pool: std::sync::Arc::clone(&server.parser_pool),
            compute_pool: std::sync::Arc::clone(&server.compute_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            auto_install: server.auto_install.clone(),
            bridge: std::sync::Arc::clone(&server.bridge),
            diagnostics: std::sync::Arc::clone(&server.diagnostics),
        }
    }

    /// Send didClose for invalidated virtual documents.
    ///
    /// Region IDs invalidated by edits touching their START orphan their virtual
    /// documents downstream; this delegates their didClose to the
    /// BridgeCoordinator and evicts their diagnostic slots (the injection-token
    /// cache is content-addressed and needs no per-ULID eviction — see below).
    /// Never-opened documents are skipped automatically (no didOpen was sent).
    pub(crate) async fn close_invalidated_virtual_docs(
        &self,
        host_uri: &Url,
        invalidated_ulids: &[ulid::Ulid],
    ) {
        if invalidated_ulids.is_empty() {
            return;
        }

        // No token-cache eviction here: the injection-token cache is
        // content-addressed (no ULID keys), self-validating on read, and
        // swept by populate's live-hash retain.
        self.bridge
            .close_invalidated_docs(host_uri, invalidated_ulids)
            .await;

        // Evict each invalidated region's diagnostic slots from the cache (#424) —
        // AFTER `close_invalidated_docs` removed the virtual-doc tracking, so a
        // racing queued push can no longer resolve the orphaned region's URI and
        // recreate the slot we just evicted. The merge already *skips* a region with
        // no current offset, so the editor never sees stale diagnostics; this
        // reclaims the lingering slot on the edit that orphaned it.
        for ulid in invalidated_ulids {
            self.diagnostics
                .evict_source(host_uri, &DiagnosticSource::Region(ulid.to_string()));
        }
    }

    /// Resolve all injection regions for a document, with stable region IDs from
    /// `NodeTracker` (lazy-node-identity-tracking). Empty Vec when nothing matches.
    ///
    /// Lock safety: the document store lock is held only long enough to clone the
    /// tree and text, then released before the tree traversal — no DashMap deadlock risk.
    pub(crate) fn resolve_injection_data(
        &self,
        uri: &Url,
        host_language: &str,
    ) -> Vec<BridgeInjection> {
        // Fast path (parse-snapshot ADR §3, never discover twice): the parse
        // pass that scheduled this downstream already derived the bridge
        // regions from its single injection-query run and published them on
        // the snapshot. Consume them when the snapshot is CURRENT — this
        // downstream runs right after the publish, so it virtually always is;
        // a raced edit falls back to the inline resolution below (which reads
        // the live tree, exactly as before).
        if let Some(view) = self.documents.latest_snapshot(uri)
            && let Some(snapshot) = &view.slot.snapshot
            && snapshot.parsed_version == view.content_version
            && let Some((stamped_generation, bridge_regions)) = &snapshot.bridge_regions
            // Generation gate (like resolved_regions): a reload can change
            // the injection query without a new snapshot — consuming the old
            // query's regions would open/update virtual documents the new
            // query would not discover. Mismatch falls back inline below.
            && *stamped_generation == self.cache.semantic_token_generation()
        {
            return bridge_regions
                .iter()
                .map(|region| BridgeInjection {
                    language: region.language.clone(),
                    region_id: region.region_id.clone(),
                    content: region.content.clone(),
                })
                .collect();
        }

        let Some(injection_query) = self.language.injection_query(host_language) else {
            return Vec::new();
        };

        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(tree) = doc.tree().cloned() else {
            return Vec::new();
        };
        let text = doc.text_arc();
        let incarnation = doc.incarnation();
        drop(doc);

        let Some(regions) =
            collect_all_injections(&tree.root_node(), &text, Some(injection_query.as_ref()))
        else {
            return Vec::new();
        };

        if regions.is_empty() {
            return Vec::new();
        }

        InjectionResolver::resolve_from_regions(
            &self.language,
            self.bridge.node_tracker(),
            uri,
            &regions,
            &text,
            incarnation,
        )
        .into_iter()
        .map(|region| BridgeInjection {
            language: region.injection_language,
            region_id: region.region.region_id,
            content: region.virtual_content,
        })
        .collect()
    }

    /// Process injected languages: resolve injection data, optionally forward didChange,
    /// auto-install missing parsers, and eagerly open virtual documents.
    ///
    /// Resolves injection data once and reuses it across all three steps. Must be
    /// called AFTER parse_document so the AST is available.
    pub(crate) async fn process_injections(&self, uri: &Url, forward_did_change: bool) {
        let Some(host_language) = self.get_language_for_document(uri) else {
            self.bridge.cancel_eager_open(uri);
            return;
        };
        let Some(incarnation) = self.documents.get(uri).map(|doc| doc.incarnation()) else {
            self.bridge.cancel_eager_open(uri);
            return;
        };
        let injections = self.resolve_injection_data(uri, &host_language);
        if injections.is_empty() {
            self.bridge.cancel_eager_open(uri);
            return;
        }

        let replaced_regions = self.bridge.close_replaced_docs(uri, &injections).await;
        for region_id in replaced_regions {
            self.diagnostics
                .evict_source(uri, &DiagnosticSource::Region(region_id));
        }

        if forward_did_change {
            self.bridge
                .forward_didchange_to_opened_docs(uri, incarnation, &injections)
                .await;
        }

        let languages: HashSet<String> =
            injections.iter().map(|inj| inj.language.clone()).collect();

        // Re-home the injected-language parser install OFF this task (#480 liveness;
        // the parse-actor ADR's "PR-3"). When a region's injected language has no
        // parser yet, `check_injected_languages_auto_install` awaits a compile that
        // can take seconds; on the parse-loop path that would stall the per-URI
        // coalescing loop's next pass, and on the didOpen install-retry path it would
        // re-hold work the ingress already returned from. Spawn it instead:
        //
        // - The didChange forwarding above already completed (ordered-after-parse,
        //    before this spawn), so forwarding order is unaffected.
        // - The injected-language **parser** install only enables kakehashi to PARSE
        //   deeper-nested regions later; it is independent of the bridge-server
        //   warmup below, which spawns the **external** language servers for these
        //   regions (no tree-sitter parser needed), so that warmup still runs
        //   promptly without waiting on the compile.
        // - `maybe_auto_install_language`'s `InstallingLanguages` tracker dedupes
        //   concurrent attempts, and the injected install (`is_injection=true`) never
        //   reparses or resurrects the host document, so a `didClose` racing this
        //   spawn is benign (it just finishes installing a global parser).
        let install_task = {
            let coordinator = self.clone();
            let uri = uri.clone();
            async move {
                coordinator
                    .check_injected_languages_auto_install(&uri, &languages)
                    .await;
            }
        };
        tokio::spawn(install_task);

        self.eager_spawn_bridge_servers(uri, incarnation, &host_language, injections)
            .await;
    }

    /// Check injected languages and handle missing parsers: auto-install when enabled,
    /// otherwise notify the user.
    ///
    /// The `InstallingLanguages` tracker in `maybe_auto_install_language` prevents
    /// duplicate install attempts.
    pub(crate) async fn check_injected_languages_auto_install(
        &self,
        uri: &Url,
        languages: &HashSet<String>,
    ) {
        let install = self.install_coordinator();
        let auto_install_enabled = self.settings_manager.is_auto_install_enabled();

        let reason = if auto_install_enabled {
            String::new()
        } else {
            install.auto_install_disabled_reason()
        };

        // Events emitted while loading an injected-language parser that was on disk
        // but not yet loaded — a one-time `Log` plus (when the language has queries)
        // a `SemanticTokensRefresh`. These were previously dropped here, so a freshly
        // loaded injected language never nudged the editor to re-pull its tokens
        // (#532). Collected and forwarded once after the loop; #531's batch dedup
        // collapses multiple refresh events into a single workspace-wide request.
        //
        // Scope/value: this only matters in the narrow window where the editor
        // already pulled tokens before this background install-check loaded the
        // parser. The async auto-install case is already covered by
        // `reload_language_after_install`, and when a parser is on disk the
        // token-collection path loads it inline and returns the injected tokens in
        // the same `semanticTokens/full` response. So this is a defensive nudge, not
        // a hot path.
        //
        // Only the success path is collected: the failure path delegates to
        // `notify_parser_missing` / `maybe_auto_install_language`, which emit their
        // own user-facing messages (and the install path forwards its own load
        // events), so forwarding failure events here would double-message. The whole
        // success vector is forwarded (log + refresh) for consistency with the
        // host/derived load path; it fires at most once per language (a re-load of
        // an already-registered language returns no events).
        let mut load_events: Vec<LanguageEvent> = Vec::new();

        for lang in languages {
            let resolved_lang = if self.language.has_parser_available(lang) {
                lang.clone()
            } else if let Some(normalized) = crate::language::heuristic::detect_from_token(lang) {
                normalized
            } else {
                lang.clone()
            };

            let load_result = self
                .language
                .ensure_language_loaded_async(&resolved_lang)
                .await;
            if load_result.success {
                load_events.extend(load_result.events);
                continue;
            }

            if !auto_install_enabled {
                install.notify_parser_missing(&resolved_lang, &reason).await;
                continue;
            }

            // Only attempt the (injected-language) install while the host document
            // is still open. The injected-language install does not reparse the
            // host document (is_injection=true), so no host text is needed here.
            if self.documents.get(uri).is_some() {
                let _ = install
                    .maybe_auto_install_language(&resolved_lang, uri.clone(), true)
                    .await;
            }
        }

        // Forward the collected load events (deduped to ≤1 refresh by #531) so the
        // editor re-pulls tokens for any injected language loaded just now.
        if !load_events.is_empty() {
            self.notifier().log_language_events(&load_events).await;
        }
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }

    /// Eagerly spawn bridge servers and open virtual documents for detected injections.
    ///
    /// This warms up language servers (spawn + handshake + didOpen) in the background
    /// for injection regions found in the document. Downstream servers receive
    /// document content immediately, enabling faster diagnostic responses.
    pub(crate) async fn eager_spawn_bridge_servers(
        &self,
        uri: &Url,
        incarnation: u64,
        host_language: &str,
        injections: Vec<BridgeInjection>,
    ) {
        let settings = self.settings_manager.load_settings();
        self.bridge
            .eager_spawn_and_open_documents(&settings, host_language, uri, incarnation, injections)
            .await;
    }

    fn get_language_for_document(&self, uri: &Url) -> Option<String> {
        detect_document_language(&self.language, &self.documents, uri)
    }

    /// The host language and resolved bridge injections for `uri`, or `None`
    /// when the document has no detectable language. Lets a caller re-derive the
    /// injected regions on demand (executeCommand doc-sync), mirroring the
    /// didOpen/didChange discovery.
    pub(crate) fn bridge_injections(&self, uri: &Url) -> Option<(String, Vec<BridgeInjection>)> {
        let host_language = self.get_language_for_document(uri)?;
        let injections = self.resolve_injection_data(uri, &host_language);
        Some((host_language, injections))
    }

    fn install_coordinator(&self) -> InstallCoordinator {
        InstallCoordinator::from_parts(InstallCoordinatorDeps {
            client: self.client.clone(),
            language: std::sync::Arc::clone(&self.language),
            parser_pool: std::sync::Arc::clone(&self.parser_pool),
            compute_pool: std::sync::Arc::clone(&self.compute_pool),
            documents: std::sync::Arc::clone(&self.documents),
            cache: std::sync::Arc::clone(&self.cache),
            settings_manager: std::sync::Arc::clone(&self.settings_manager),
            auto_install: self.auto_install.clone(),
            bridge: std::sync::Arc::clone(&self.bridge),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::InjectionResolver;
    use tower_lsp_server::LspService;
    use tree_sitter::Query;
    use url::Url;

    /// The snapshot fast path of `resolve_injection_data` must produce
    /// exactly what the inline (live-tree) resolution produces — the fast
    /// path's output is forwarded verbatim to downstream servers, so a
    /// divergence would silently mis-forward.
    #[tokio::test]
    async fn bridge_fast_path_matches_inline_resolution() {
        let (service, _socket) = LspService::new(crate::lsp::lsp_impl::Kakehashi::new);
        let server = service.inner();

        let registry = server.language.language_registry_for_parallel();
        registry.register("markdown".to_string(), tree_sitter_md::LANGUAGE.into());
        registry.register("python".to_string(), tree_sitter_python::LANGUAGE.into());
        let markdown_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let injection_query = Query::new(
            &markdown_language,
            r#"
            (fenced_code_block
              (info_string (language) @_lang)
              (code_fence_content) @injection.content
              (#set-lang-from-info-string! @_lang)
              (#offset! @injection.content 1 0 0 0))
            "#,
        )
        .expect("valid markdown injection query");
        server
            .language
            .query_store()
            .insert_injection_query("markdown".to_string(), std::sync::Arc::new(injection_query));

        let text = "# T\n\n```py\nignored\nx = 1\n```\n\n```unknown\n#!/usr/bin/env lua\n#!/usr/bin/env python\ny = 2\n```\n";
        let mut pool = server.language.create_document_parser_pool();
        let mut parser = pool.acquire("markdown").expect("registered parser");
        let tree = parser.parse(text, None).expect("parse markdown");
        pool.release("markdown".to_string(), parser);

        let uri = Url::parse("file:///bridge_fast_path.md").unwrap();
        server
            .documents
            .insert(uri.clone(), text.to_string(), Some("markdown".into()), None);
        server
            .documents
            .update_document(uri.clone(), text.to_string(), Some(tree.clone()));

        let injection = server.injection_coordinator();

        // INLINE: a current snapshot with no bridge_regions falls through to
        // the live-tree resolution.
        let incarnation = server
            .documents
            .latest_snapshot(&uri)
            .unwrap()
            .slot
            .current_incarnation;
        let content_version = server.documents.get(&uri).unwrap().content_version();
        let publish = |bridge_regions, parsed_version| {
            let landed = server
                .documents
                .get(&uri)
                .map(|doc| {
                    doc.publish_snapshot(std::sync::Arc::new(
                        crate::document::snapshot::ParseSnapshot {
                            text: std::sync::Arc::from(text),
                            tree: Some(tree.clone()),
                            language: Some("markdown".to_string()),
                            parsed_version,
                            incarnation,
                            injection_regions: None,
                            bridge_regions,
                            resolved_regions: None,
                            layer_trees: std::sync::OnceLock::new(),
                        },
                    ))
                })
                .unwrap_or(false);
            assert!(landed, "test publish must land");
        };
        publish(None, content_version);
        let inline = injection.resolve_injection_data(&uri, "markdown");
        assert!(!inline.is_empty(), "the two fences must resolve");
        assert!(
            inline.iter().all(|region| region.language == "python"),
            "inline eager discovery must use the request resolver's canonical language"
        );
        let interactive = InjectionResolver::resolve_all(
            &server.language,
            server.bridge.node_tracker(),
            &uri,
            &tree,
            text,
            &server
                .language
                .injection_query("markdown")
                .expect("registered injection query"),
        );
        assert!(
            interactive
                .iter()
                .all(|region| region.injection_language == "python"),
            "interactive request discovery must use the canonical language"
        );

        // FAST PATH: populate derives the bridge regions from the same tree
        // (same tracker → same region ids); a newer current snapshot carries
        // them and the resolver consumes them verbatim.
        let populated = server.cache.populate_injections(
            &uri,
            text,
            &tree,
            "markdown",
            &server.language,
            &server.bridge.node_tracker_arc(),
            server.bridge.node_tracker_arc().mint_epoch(&uri),
            incarnation,
            true,
            true,
        );
        let populated = populated.expect("current pass populates");
        let bridge_regions = populated.bridge_regions.expect("gate was true");
        server
            .documents
            .update_document(uri.clone(), text.to_string(), Some(tree.clone()));
        let content_version = server.documents.get(&uri).unwrap().content_version();
        publish(
            Some((populated.generation, std::sync::Arc::new(bridge_regions))),
            content_version,
        );
        let fast = injection.resolve_injection_data(&uri, "markdown");
        assert!(
            fast.iter().all(|region| region.language == "python"),
            "cached eager discovery must use the request resolver's canonical language"
        );

        assert_eq!(
            inline.len(),
            fast.len(),
            "fast path must resolve the same regions"
        );
        for (a, b) in inline.iter().zip(fast.iter()) {
            assert_eq!(a.language, b.language, "canonical language must match");
            assert_eq!(a.region_id, b.region_id, "region id must match");
            assert_eq!(a.content, b.content, "clean content must match");
        }
    }
}
