//! didOpen notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidOpenTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};
use crate::language::LanguageEvent;

impl Kakehashi {
    pub(crate) async fn did_open_impl(&self, params: DidOpenTextDocumentParams) {
        let language_id = params.text_document.language_id.clone();
        let lsp_uri = params.text_document.uri.clone();
        let text = params.text_document.text.clone();

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didOpen: {}", lsp_uri.as_str());
            return;
        };

        // Try to determine the language
        let language_name = self
            .language
            .language_for_path(uri.path())
            .or_else(|| Some(language_id.clone()));

        // Insert document immediately (without tree) so concurrent requests can find it.
        // This handles race conditions where semanticTokens/full arrives before
        // parse_document completes. The tree will be updated by parse_document.
        self.documents
            .insert(uri.clone(), text.clone(), language_name.clone(), None);

        // Check if we need to auto-install
        let mut deferred_events = Vec::new();
        let mut skip_parse = false; // Track if auto-install was triggered

        if let Some(ref lang) = language_name {
            let load_result = self.language.ensure_language_loaded(lang);

            // Defer SemanticTokensRefresh events until after parse_document completes
            // to avoid race condition where tokens are requested before tree exists.
            // Log events immediately but defer refresh.
            for event in &load_result.events {
                match event {
                    LanguageEvent::SemanticTokensRefresh { .. } => {
                        deferred_events.push(event.clone());
                    }
                    _ => {
                        self.notifier()
                            .log_language_events(std::slice::from_ref(event))
                            .await;
                    }
                }
            }

            if !load_result.success {
                if self.settings_manager.is_auto_install_enabled() {
                    // Language failed to load and auto-install is enabled
                    // is_injection=false: This is the document's main language
                    // If install is triggered, skip parse_document here - reload_language_after_install will handle it
                    skip_parse = self
                        .install_coordinator()
                        .maybe_auto_install_language(lang, uri.clone(), text.clone(), false)
                        .await;
                } else {
                    // Notify user that parser is missing and needs manual installation
                    let reason = self.install_coordinator().auto_install_disabled_reason();
                    self.install_coordinator()
                        .notify_parser_missing(lang, &reason)
                        .await;
                }
            }
        }

        // Only parse if auto-install was NOT triggered
        // If auto-install was triggered, reload_language_after_install will call parse_document
        // after the parser file is completely written, preventing race condition
        if !skip_parse {
            self.parse_coordinator()
                .parse_document(
                    uri.clone(),
                    params.text_document.text,
                    Some(&language_id),
                    vec![], // No edits for initial document open
                )
                .await;
        }

        // Now handle deferred SemanticTokensRefresh events after document is parsed
        if !deferred_events.is_empty() {
            self.notifier().log_language_events(&deferred_events).await;
        }

        // Process injected languages: auto-install missing parsers and spawn bridge servers.
        // This must be called AFTER parse_document so we have access to the AST.
        self.injection_coordinator()
            .process_injections(&uri, false)
            .await;

        // Host-layer eager-open (#429): open the real host document on any `_self`
        // host-bridge server so a push-only host server (e.g. lua-language-server)
        // starts analyzing and pushing diagnostics on open, instead of only after
        // the first host-bridged request lazily opens it. No-op when host bridging
        // is off for the language; spawns fire-and-forget tasks (non-blocking).
        if let Some(ref lang) = language_name {
            let settings = self.settings_manager.load_settings();
            self.bridge
                .eager_open_host_document_on_servers(&settings, lang, &uri, &text);
        }

        // pull-first-diagnostic-forwarding Phase 2: Trigger synthetic diagnostic push on didOpen
        // This provides proactive diagnostics for clients that don't support pull diagnostics.
        self.diagnostic_scheduler()
            .spawn_synthetic_diagnostic_task(uri);

        // NOTE: No semantic_tokens_refresh() on didOpen.
        // Capable LSP clients should request by themselves.
        // Calling refresh would be redundant and can cause deadlocks with clients
        // like vim-lsp that don't respond to workspace/semanticTokens/refresh requests.

        self.notifier().log_info("file opened!").await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::WorkspaceSettings;
    use crate::config::settings::{
        AggregationConfig, BridgeLanguageConfig, BridgeServerConfig, HOST_BRIDGE_KEY,
        LanguageSettings,
    };
    use crate::lsp::bridge::VirtualDocumentUri;
    use tokio::time::{Duration, timeout};
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{DidOpenTextDocumentParams, TextDocumentItem};
    use tree_sitter::Query;
    use url::Url;

    async fn wait_until(condition: impl Fn() -> bool) {
        timeout(Duration::from_secs(1), async {
            loop {
                if condition() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("condition should become true");
    }

    #[tokio::test]
    async fn did_open_parses_before_eager_opening_injected_virtual_documents() {
        // Test isolation is automatic via the `cfg(test)` branch in
        // `kakehashi::install::default_data_dir()`, which redirects every
        // `Kakehashi::new` call in this binary to a project-local data dir
        // under `deps/test/kakehashi/` (see `test_support::test_data_dir_path`)
        // and clears stale crash-recovery state once per process. No env-var
        // dance or per-test setup needed.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        let registry = server.language.language_registry_for_parallel();
        registry.register("markdown".to_string(), tree_sitter_md::LANGUAGE.into());
        registry.register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        let markdown_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let injection_query = Query::new(
            &markdown_language,
            r#"
            (fenced_code_block
              (info_string
                (language) @injection.language)
              (code_fence_content) @injection.content)
            "#,
        )
        .expect("valid markdown injection query");
        server
            .language
            .query_store()
            .insert_injection_query("markdown".to_string(), std::sync::Arc::new(injection_query));

        let mut language_servers = HashMap::new();
        language_servers.insert(
            "rust-bridge".to_string(),
            BridgeServerConfig {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "cat > /dev/null".to_string(),
                ],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                root_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            language_servers,
            ..Default::default()
        });

        server
            .bridge
            .insert_ready_test_connection("rust-bridge")
            .await;

        let uri = Url::parse("file:///test/did_open_injection.md").expect("valid test URI");
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");
        let text = r#"# Example

```rust
print("hello")
```
"#;

        server
            .did_open_impl(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: lsp_uri.clone(),
                    language_id: "markdown".to_string(),
                    version: 1,
                    text: text.to_string(),
                },
            })
            .await;

        let document = server
            .documents
            .get(&uri)
            .expect("document should be stored");
        assert!(
            document.tree().is_some(),
            "did_open should parse the document"
        );
        drop(document);

        let injections = server
            .cache
            .get_injections(&uri)
            .expect("parse should populate injection cache");
        assert_eq!(injections.len(), 1, "expected one fenced code injection");

        let virtual_uri = VirtualDocumentUri::new(&lsp_uri, "rust", &injections[0].region_id);

        wait_until(|| server.bridge.pool().is_document_opened(&virtual_uri)).await;

        assert!(
            server.bridge.pool().is_document_opened(&virtual_uri),
            "did_open should eagerly open the parsed injection as a virtual document"
        );
    }

    /// Register the rust grammar and apply settings with a single `rust_ls` `_self`
    /// host server (host bridging on for rust). Caller then inserts a ready test
    /// connection for `rust_ls`.
    fn configure_rust_self_host(server: &Kakehashi) {
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        let mut language_servers = HashMap::new();
        language_servers.insert(
            "rust_ls".to_string(),
            BridgeServerConfig {
                // Inert: the caller pre-inserts a Ready handle via
                // `insert_ready_test_connection`, so this cmd is never spawned.
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "cat > /dev/null".to_string(),
                ],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                root_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "rust".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::from([(
                    HOST_BRIDGE_KEY.to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(true),
                        aggregation: None,
                    },
                )])),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            language_servers,
            languages,
            ..Default::default()
        });
    }

    /// Host-layer eager-open (#429): the host document is opened (didOpen sent) on
    /// a `_self` host server directly by the eager-open path. Exercised in
    /// isolation via `eager_open_host_document_on_servers` (not `did_open_impl`)
    /// so the host-event synthetic diagnostic pull — which would also open the doc
    /// — can't mask whether eager-open itself works.
    #[tokio::test]
    async fn eager_open_sends_didopen_to_self_host_server() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        configure_rust_self_host(server);
        server.bridge.insert_ready_test_connection("rust_ls").await;

        let uri = Url::parse("file:///test/host_eager.rs").unwrap();
        let settings = server.settings_manager.load_settings();
        server
            .bridge
            .eager_open_host_document_on_servers(&settings, "rust", &uri, "fn main() {}");

        timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .bridge
                    .pool()
                    .is_host_document_opened(&uri, "rust_ls")
                    .await
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("eager-open should send didOpen for the host document to the _self host server");
    }

    /// On-edit host re-sync (#431): a second `eager_sync_host_document_on_servers`
    /// with changed text sends a versioned `didChange` (version advances to 2), so
    /// a push-only host server re-analyzes current text instead of stale text. The
    /// debounced diagnostic path routes through the same method at its cadence.
    #[tokio::test]
    async fn eager_sync_resyncs_host_document_on_changed_text() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        configure_rust_self_host(server);
        server.bridge.insert_ready_test_connection("rust_ls").await;

        let uri = Url::parse("file:///test/host_resync.rs").unwrap();
        let settings = server.settings_manager.load_settings();
        let configs = server
            .bridge
            .get_host_configs_for_language(&settings, "rust");

        // First sync opens the doc at version 1.
        server.bridge.eager_sync_host_document_on_servers(
            &uri,
            "rust",
            std::sync::Arc::from("fn main() {}"),
            configs.clone(),
        );
        timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .bridge
                    .pool()
                    .host_document_version(&uri, "rust_ls")
                    .await
                    == Some(1)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first eager-sync should didOpen the host document at version 1");

        // A re-sync with changed text must send a didChange, advancing to version 2.
        server.bridge.eager_sync_host_document_on_servers(
            &uri,
            "rust",
            std::sync::Arc::from("fn other() {}"),
            configs,
        );
        timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .bridge
                    .pool()
                    .host_document_version(&uri, "rust_ls")
                    .await
                    == Some(2)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("re-sync with changed text should send a didChange advancing to version 2");
    }

    /// On-edit re-sync wiring (#431): when the debounced diagnostic *fires*,
    /// `execute_debounced_diagnostic` re-syncs the host doc to its `_self` servers
    /// via the coordinator. Drives `DebouncedDiagnosticsManager::schedule` with a
    /// zero debounce and a host snapshot, then asserts the host doc gets synced.
    /// The test `rust_ls` connection advertises no `diagnosticProvider`, so the
    /// capability-gated pull skips it — only the eager-sync hook can sync it, which
    /// isolates the wiring (removing the hook would fail this test).
    #[tokio::test]
    async fn debounced_fire_resyncs_host_document() {
        use crate::config::settings::{AggregationStrategy, LayerSource, ResolvedLayerConfig};
        use crate::lsp::debounced_diagnostics::DebouncedDiagnosticsManager;
        use crate::lsp::lsp_impl::DiagnosticPublisher;
        use crate::lsp::lsp_impl::bridge_context::HostRequestContext;
        use crate::lsp::lsp_impl::text_document::publish_diagnostic::DiagnosticSnapshot;
        use crate::lsp::synthetic_diagnostics::SyntheticDiagnosticsManager;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);
        server.bridge.insert_ready_test_connection("rust_ls").await;

        let uri = Url::parse("file:///test/host_debounce.rs").unwrap();
        let settings = server.settings_manager.load_settings();
        let configs = server
            .bridge
            .get_host_configs_for_language(&settings, "rust");

        let snapshot = DiagnosticSnapshot {
            virt_contexts: vec![],
            host_pull_enabled: true,
            host: Some(HostRequestContext {
                uri: uri.clone(),
                language_id: "rust".to_string(),
                text: std::sync::Arc::from("fn main() {}"),
                configs,
                priorities: vec![],
                strategy: AggregationStrategy::Concatenated,
                max_fan_out: None,
                upstream_request_id: None,
            }),
            layer_cfg: ResolvedLayerConfig {
                priorities: vec![LayerSource::Host],
                strategy: AggregationStrategy::Concatenated,
            },
        };

        // Fire immediately (zero debounce) and assert the host doc was synced.
        DebouncedDiagnosticsManager::with_duration(Duration::ZERO).schedule(
            uri.clone(),
            Some(snapshot),
            server.bridge.pool_arc(),
            std::sync::Arc::clone(&server.bridge),
            std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            std::sync::Arc::new(DiagnosticPublisher::new(server)),
        );

        timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .bridge
                    .pool()
                    .host_document_version(&uri, "rust_ls")
                    .await
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("debounce fire should re-sync the host document to the _self server");
    }

    /// Locks the load-bearing property behind #431: `prepare_diagnostic_snapshot`
    /// builds the host context for a `_self`-bridged doc WITHOUT gating on the
    /// server advertising `diagnosticProvider`. A push-only `rust_ls` (no
    /// diagnosticProvider, like marksman) is therefore in `snapshot.host.configs`,
    /// so the debounce-fire re-sync reaches it. If the snapshot were
    /// capability-gated, the whole on-edit re-sync would be a no-op for exactly the
    /// push-only servers it exists to serve.
    #[tokio::test]
    async fn prepare_diagnostic_snapshot_includes_push_only_host_server() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/host_snapshot.rs").unwrap();
        let text = "fn main() {}".to_string();
        server
            .documents
            .insert(uri.clone(), text.clone(), Some("rust".to_string()), None);
        server
            .parse_coordinator()
            .parse_document(uri.clone(), text, Some("rust"), vec![])
            .await;

        let snapshot = server
            .diagnostic_scheduler()
            .prepare_diagnostic_snapshot(&uri)
            .expect("a parsed _self-bridged doc yields a diagnostic snapshot");
        let host = snapshot.host.expect(
            "host context is built for a _self-bridged doc, not gated on diagnosticProvider",
        );
        assert!(
            host.configs.iter().any(|c| c.server_name == "rust_ls"),
            "the push-only rust_ls must be in the host context so the debounce re-sync reaches it"
        );
    }

    /// #425 regression guard: host `pullFallback = false` gates the host **pull**
    /// (`host_pull_enabled = false`) but must NOT drop the host context — the
    /// #431 debounced re-sync still needs it to push current text to a push-only
    /// `_self` server. If the gate dropped the context, that server would analyze
    /// stale text (re-opening #380 for the host).
    #[tokio::test]
    async fn prepare_diagnostic_snapshot_keeps_host_for_resync_when_pull_gated() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        // Override the `_self` aggregation: pullFallback = false for the
        // proactive-publish method.
        let mut settings = (*server.settings_manager.load_settings()).clone();
        if let Some(bridge) = settings
            .languages
            .get_mut("rust")
            .and_then(|lang| lang.bridge.as_mut())
        {
            bridge.insert(
                HOST_BRIDGE_KEY.to_string(),
                BridgeLanguageConfig {
                    enabled: Some(true),
                    aggregation: Some(HashMap::from([(
                        "textDocument/publishDiagnostics".to_string(),
                        AggregationConfig {
                            pull_fallback: Some(false),
                            ..Default::default()
                        },
                    )])),
                },
            );
        }
        server.settings_manager.apply_settings(settings);

        let uri = Url::parse("file:///test/host_pull_gated.rs").unwrap();
        let text = "fn main() {}".to_string();
        server
            .documents
            .insert(uri.clone(), text.clone(), Some("rust".to_string()), None);
        server
            .parse_coordinator()
            .parse_document(uri.clone(), text, Some("rust"), vec![])
            .await;

        let snapshot = server
            .diagnostic_scheduler()
            .prepare_diagnostic_snapshot(&uri)
            .expect("a parsed _self-bridged doc still yields a snapshot when the pull is gated");
        assert!(
            snapshot.host.is_some(),
            "the host context is kept for the re-sync even when the pull is gated off"
        );
        assert!(
            !snapshot.host_pull_enabled,
            "pullFallback = false disables the host pull (host_pull_enabled = false)"
        );
    }
}
