//! didOpen notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidOpenTextDocumentParams;

use super::super::{Kakehashi, build_notifier, uri_to_url};
use crate::language::LanguageEvent;

impl Kakehashi {
    pub(crate) async fn did_open_impl(&self, params: DidOpenTextDocumentParams) {
        self.did_open_impl_with_lock_probe(params, std::future::ready(()))
            .await;
    }

    pub(in crate::lsp::lsp_impl) async fn did_open_impl_with_lock_probe(
        &self,
        params: DidOpenTextDocumentParams,
        before_lifecycle_lock: impl std::future::Future<Output = ()>,
    ) {
        let text_document = params.text_document;
        let language_id = text_document.language_id;
        let lsp_uri = text_document.uri;
        let text = text_document.text;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didOpen: {}", lsp_uri.as_str());
            return;
        };

        let language_name = if language_id == "plaintext" {
            self.language
                .candidate_language_for_document(uri.path(), &text)
        } else {
            Some(language_id.clone())
        };

        // Serialize reopen insertion behind the prior lifetime's complete
        // didClose cleanup. didClose deliberately retains this map entry while
        // the document itself is absent, so reopening cannot mint a fresh mutex
        // and expose new state to URI-scoped old-lifetime teardown.
        let edit_lock = self.documents.edit_lock(&uri);
        before_lifecycle_lock.await;
        let edit_guard = edit_lock.lock().await;

        // Insert document immediately (without tree) so concurrent requests can find it.
        // This handles race conditions where semanticTokens/full arrives before
        // parse_document completes. The tree will be updated by parse_document.
        let incarnation =
            self.documents
                .insert(uri.clone(), text.clone(), language_name.clone(), None);
        self.bridge.open_tracker_incarnation(&uri, incarnation);
        drop(edit_guard);

        // Host-tier hoist (parse-decoupled-document-lifecycle ADR): attach the real
        // host document to any `_self` host-bridge server *before* the parser load,
        // the parse, and auto-install — none of which the host tier depends on.
        // `eager_open_host_document_on_servers` needs only the initial language
        // name and text, so a push-only host server (e.g.
        // lua-language-server) starts analyzing and pushing diagnostics immediately
        // instead of waiting out a ~120-310ms parse or an unbounded install. It
        // spawns fire-and-forget per-server tasks (non-blocking); no-op when host
        // bridging is off for the language. Hoisting the open earlier only
        // strengthens the open-before-change invariant — `sync_host_document`'s
        // per-(uri, connection) state machine still emits didOpen before any
        // didChange regardless of task-schedule order (see
        // ls-bridge-message-ordering).
        if let Some(ref lang) = language_name {
            let settings = self.settings_manager.load_settings();
            self.bridge
                .eager_open_host_document_on_servers(&settings, lang, &uri, &text);
        }

        // Check if we need to auto-install
        let mut deferred_events = Vec::new();
        let mut skip_parse = false; // Track if auto-install was triggered

        if let Some(ref lang) = language_name {
            let load_result = self.language.ensure_language_loaded_async(lang).await;

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
                    // Language failed to load and auto-install is enabled.
                    //
                    // Move auto-install OFF the ingress writer ticket (#480
                    // liveness): a slow or hung parser *compile* must not hold the
                    // didOpen ticket and wedge later same-URI readers/writers. The
                    // spawned task installs, reloads, and resurrection-safely
                    // reparses the latest store text; this handler returns
                    // immediately. We skip the inline parse unconditionally — the
                    // parser is not loaded yet, so an inline parse would yield no
                    // tree anyway — and the skip-parse branch below advances the
                    // watermark so a gated reader is not stranded.
                    let install = self.install_coordinator();
                    let injection = self.injection_coordinator();
                    let diagnostic_scheduler = self.diagnostic_scheduler();
                    let documents = std::sync::Arc::clone(&self.documents);
                    let lang = lang.clone();
                    let install_uri = uri.clone();
                    // Mirror the handler's `!is_cli_mode()` gate on the synthetic
                    // diagnostic (#489): in one-shot CLI mode no editor consumes the
                    // proactive publishDiagnostics, so re-firing it from the install
                    // spawn is pure wasted work and races the bridge-state sweep.
                    let is_cli_mode = self.is_cli_mode();
                    tokio::spawn(async move {
                        install
                            .maybe_auto_install_language(&lang, install_uri.clone(), false)
                            .await;
                        // The skipped inline parse meant the handler's
                        // process_injections (below) ran with no tree. Now that the
                        // off-ingress reparse may have produced one, run the normal
                        // post-parse injection workflow — injected-language install
                        // and eager bridge spawn — which also keeps that injected
                        // install off the ingress ticket. forward=false: open path.
                        //
                        // Gate on the document actually having a tree: install can
                        // fail or be deduped (AlreadyInstalling) with no reparse, and
                        // process_injections would otherwise cancel the eager-open
                        // for a still-tree-less document.
                        let has_tree = documents
                            .get(&install_uri)
                            .is_some_and(|doc| doc.tree().is_some());
                        if has_tree {
                            injection.process_injections(&install_uri, false).await;
                            // Re-fire the proactive synthetic diagnostic now that a
                            // tree exists: the handler's spawn (below) ran in the
                            // skip-parse path with no tree, so its snapshot was None
                            // and the pull-layer diagnostics were skipped on this
                            // first open of a just-installed parser. Skipped in CLI
                            // mode (#489), matching the handler's own gate.
                            if !is_cli_mode {
                                diagnostic_scheduler.spawn_synthetic_diagnostic_task(install_uri);
                            }
                        }
                    });
                    skip_parse = true;
                } else {
                    // Notify user that parser is missing and needs manual installation
                    let reason = self.install_coordinator().auto_install_disabled_reason();
                    self.install_coordinator()
                        .notify_parser_missing(lang, &reason)
                        .await;
                }
            }
        }

        // This didOpen's ingress writer ticket (plumbed from IngressOrderGate),
        // used to stamp the store watermark so a reader gated behind this open is
        // released once its parse — or the decision to defer parsing to install —
        // has resolved.
        let ticket = crate::lsp::current_writer_ticket();

        // Parse the document and run its tree-dependent downstream. Three shapes:
        //
        // - **auto-install (`skip_parse`)**: the install task spawned above reparses
        //   off-ingress and runs its own downstream once the parser lands; here we
        //   only resolve this open's watermark (so a gated reader isn't stranded) and
        //   run the non-tree-dependent handler tail.
        // - **CLI one-shot**: parse INLINE and await the downstream. There is one
        //   document and no concurrent same-URI op to wedge, and the caller reads the
        //   result — plus the eager-open handles `process_injections` registers
        //   (`wait_eager_open_finished`) — the moment `did_open_impl` returns, so the
        //   open parse must be complete before returning.
        // - **interactive LSP (#6)**: flip the present-parser parse OFF the ingress
        //   ticket, mirroring the auto-install spawn — a slow/large open parse no
        //   longer holds the writer ticket and wedges later same-URI readers/writers.
        //   The handler returns immediately; the spawned parse advances the watermark
        //   to `ticket` (releasing a reader gated behind this open) and runs the
        //   tree-dependent downstream once the tree lands.
        if skip_parse {
            if let Some(ticket) = ticket {
                // Parsing is deferred to the spawned off-ingress install reparse.
                // Advance the watermark now so a reader gated behind this open does
                // not stall to its timeout waiting for a parse that won't run on the
                // ingress path; it proceeds and observes the (still tree-less)
                // install-pending document via its existing has-tree wait.
                self.documents.advance_watermark(&uri, ticket);
            }
            // Deferred SemanticTokensRefresh (empty unless the load succeeded, which
            // in the skip path it did not — kept for symmetry).
            if !deferred_events.is_empty() {
                self.notifier().log_language_events(&deferred_events).await;
            }
            // pull-first-diagnostic-forwarding Phase 2: proactive synthetic diagnostic
            // for clients without pull support. Skipped in one-shot CLI mode (#489):
            // no editor consumes the proactive `publishDiagnostics` (the stub client
            // pump discards it), the task is wasted downstream work, and its
            // abort-without-join on `didClose` races the bridge-state sweep. The
            // install spawn re-fires it once its reparse produces a tree.
            if !self.is_cli_mode() {
                self.diagnostic_scheduler()
                    .spawn_synthetic_diagnostic_task(uri);
            }
        } else if self.is_cli_mode() {
            self.parse_coordinator()
                .parse_document(uri.clone(), Some(&language_id), ticket)
                .await;
            if !deferred_events.is_empty() {
                self.notifier().log_language_events(&deferred_events).await;
            }
            // AFTER the parse so the AST exists; registers the eager-open handles the
            // CLI's `wait_eager_open_finished` polls. No synthetic diagnostic in CLI
            // mode (#489).
            self.injection_coordinator()
                .process_injections(&uri, false)
                .await;
        } else {
            // #6 off-ingress open flip (interactive LSP). The owned coordinators /
            // Arcs are captured into the spawned task; the handler returns without
            // awaiting the parse.
            let parse = self.parse_coordinator();
            let injection = self.injection_coordinator();
            let diagnostic_scheduler = self.diagnostic_scheduler();
            let client = self.client.clone();
            let settings_manager = std::sync::Arc::clone(&self.settings_manager);
            let parse_uri = uri.clone();
            let parse_language_id = language_id.clone();
            // Move the deferred SemanticTokensRefresh into the task: firing it now (at
            // handler return) would make the client re-request semantic tokens against
            // a still-tree-less document and burn its watermark settle wait; firing it
            // after the tree lands lets the re-request find the tree immediately.
            let deferred = std::mem::take(&mut deferred_events);
            tokio::spawn(async move {
                let landed = parse
                    .parse_document(parse_uri.clone(), Some(parse_language_id.as_str()), ticket)
                    .await;
                // Run the open downstream only when THIS parse's CAS landed the tree —
                // not merely when "a tree exists". A `didChange` racing this open parse
                // can move the text on and let the edit reparse attach the newer tree
                // (and run `process_injections(forward=true)`) first; this parse then
                // loses its CAS (`landed == false`). Re-running the open downstream
                // (`process_injections(forward=false)`) over the edit's tree would
                // supersede the edit's eager-open batch. When `landed` is false the
                // edit reparse owns the current tree and already ran the correct
                // downstream, so there is nothing for the open path to do. (A parse
                // that produced no tree at all also returns false.)
                if landed {
                    injection.process_injections(&parse_uri, false).await;
                    if !deferred.is_empty() {
                        build_notifier(&client, &settings_manager)
                            .log_language_events(&deferred)
                            .await;
                    }
                    diagnostic_scheduler.spawn_synthetic_diagnostic_task(parse_uri);
                }
            });
        }

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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition should become true");
    }

    #[tokio::test]
    async fn plaintext_did_open_without_heuristic_match_stores_unknown_language() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        });

        let uri = Url::parse("file:///test/unknown-buffer").expect("valid test URI");
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");

        server
            .did_open_impl(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: lsp_uri,
                    language_id: "plaintext".to_string(),
                    version: 1,
                    text: "plain text".to_string(),
                },
            })
            .await;

        assert_eq!(
            server
                .documents
                .get(&uri)
                .expect("document should be stored")
                .language_id(),
            None,
            "plaintext should not be treated as a parser language"
        );
    }

    #[tokio::test]
    async fn plaintext_did_open_uses_path_candidate() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        });

        let uri = Url::parse("file:///test/from-path.rs").expect("valid test URI");
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");

        server
            .did_open_impl(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: lsp_uri,
                    language_id: "plaintext".to_string(),
                    version: 1,
                    text: "fn main() {}".to_string(),
                },
            })
            .await;

        assert_eq!(
            server
                .documents
                .get(&uri)
                .expect("document should be stored")
                .language_id(),
            Some("rust"),
            "plaintext should fall back to a path-derived candidate"
        );
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
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
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

        // The present-parser open parse now runs off the ingress ticket (#6), so the
        // tree + injection cache appear asynchronously after `did_open_impl` returns.
        wait_until(|| {
            server
                .cache
                .get_injections(&uri)
                .is_some_and(|injections| injections.len() == 1)
        })
        .await;

        assert!(
            server
                .documents
                .get(&uri)
                .is_some_and(|doc| doc.tree().is_some()),
            "did_open should parse the document"
        );

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
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
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
                tokio::task::yield_now().await;
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
            None,
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
                tokio::task::yield_now().await;
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
            None,
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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("re-sync with changed text should send a didChange advancing to version 2");
    }

    /// Live-reader wiring (#422): the on-edit eager re-sync syncs the reader's
    /// CURRENT text, not its `text` snapshot. With the snapshot held *constant*
    /// across two re-syncs but the reader returning different text, the host
    /// server still advances to version 2 — so a stale snapshot can neither
    /// suppress a real update nor roll the server back, which is what closes the
    /// eager-sync stale-overwrite.
    #[tokio::test]
    async fn eager_sync_syncs_live_reader_text_not_the_snapshot() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        configure_rust_self_host(server);
        server.bridge.insert_ready_test_connection("rust_ls").await;

        let uri = Url::parse("file:///test/host_reader.rs").unwrap();
        let settings = server.settings_manager.load_settings();
        let configs = server
            .bridge
            .get_host_configs_for_language(&settings, "rust");

        // The snapshot is the SAME both times; only the live reader changes.
        let snapshot: std::sync::Arc<str> = std::sync::Arc::from("fn snapshot() {}");
        let reader_v1: crate::lsp::bridge::HostTextReader =
            std::sync::Arc::new(|| Some(std::sync::Arc::from("fn live_v1() {}")));
        let reader_v2: crate::lsp::bridge::HostTextReader =
            std::sync::Arc::new(|| Some(std::sync::Arc::from("fn live_v2() {}")));

        server.bridge.eager_sync_host_document_on_servers(
            &uri,
            "rust",
            std::sync::Arc::clone(&snapshot),
            configs.clone(),
            Some(reader_v1),
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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first eager-sync should didOpen at version 1 using the reader's text");

        // Same snapshot, different reader text: must advance to version 2. If the
        // sync trusted the (unchanged) snapshot it would fingerprint-dedup and stay
        // at version 1 — so reaching version 2 proves the reader drove the sync.
        server.bridge.eager_sync_host_document_on_servers(
            &uri,
            "rust",
            snapshot,
            configs,
            Some(reader_v2),
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
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("changed reader text (constant snapshot) should advance to version 2");
    }

    /// On-edit re-sync wiring (#431): when the debounced diagnostic *fires*,
    /// `execute_debounced_diagnostic` re-syncs the host doc to its `_self` servers
    /// via the coordinator. The test `rust_ls` connection advertises no
    /// `diagnosticProvider`, so the capability-gated pull skips it — only the
    /// eager-sync hook can sync it, which isolates the wiring (removing the hook
    /// would fail this test).
    ///
    /// It also locks the #422 live reader: the debounce carries a CONSTANT snapshot
    /// text while the document's live text in the store changes between fires. Only
    /// reading the store (the live reader the debounce builds) makes the second fire
    /// send new text and advance the host version — a snapshot-only / `None`-reader
    /// sync would fingerprint-dedup and stay at version 1.
    #[tokio::test]
    async fn debounced_fire_resyncs_host_document_from_live_store_text() {
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

        // Snapshot text is CONSTANT across both fires; only the store text changes.
        let snapshot = || DiagnosticSnapshot {
            virt_contexts: vec![],
            host_pull_enabled: true,
            host: Some(HostRequestContext {
                uri: uri.clone(),
                language_id: "rust".to_string(),
                text: std::sync::Arc::from("fn debounce_snapshot() {}"),
                configs: configs.clone(),
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
        let manager = DebouncedDiagnosticsManager::with_duration(Duration::ZERO);

        let wait_version = |want: i32| {
            let uri = &uri;
            async move {
                timeout(Duration::from_secs(1), async {
                    loop {
                        if server
                            .bridge
                            .pool()
                            .host_document_version(uri, "rust_ls")
                            .await
                            == Some(want)
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
            }
        };

        // Live text v1 (≠ snapshot) — first fire opens the doc at version 1.
        server.documents.insert(
            uri.clone(),
            "fn live_v1() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        manager.schedule(
            uri.clone(),
            Some(snapshot()),
            server.bridge.pool_arc(),
            std::sync::Arc::clone(&server.bridge),
            std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            std::sync::Arc::new(DiagnosticPublisher::new(server)),
            std::sync::Arc::clone(&server.documents),
        );
        wait_version(1)
            .await
            .expect("first debounce fire should re-sync the host doc at version 1");

        // Change ONLY the store; the debounce snapshot text stays constant. A live
        // read advances to version 2; a snapshot/None sync would dedup at version 1.
        server.documents.insert(
            uri.clone(),
            "fn live_v2() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        manager.schedule(
            uri.clone(),
            Some(snapshot()),
            server.bridge.pool_arc(),
            std::sync::Arc::clone(&server.bridge),
            std::sync::Arc::new(SyntheticDiagnosticsManager::new()),
            std::sync::Arc::new(DiagnosticPublisher::new(server)),
            std::sync::Arc::clone(&server.documents),
        );
        wait_version(2).await.expect(
            "second debounce fire must re-read the updated store text and advance to version 2 \
             (a snapshot-only sync would dedup at version 1)",
        );
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
            .parse_document(uri.clone(), Some("rust"), None)
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

    /// Regression (parse-actor flip): the host bridge context needs only the
    /// document text, never the parse tree (parse-decoupled ADR), so it must keep
    /// resolving after `did_change` clears the tree — otherwise every host-bridged
    /// request (hover / definition / formatting / diagnostics) would bail for the
    /// whole reparse window after each edit.
    #[tokio::test]
    async fn resolve_host_bridge_context_survives_a_cleared_tree() {
        use std::str::FromStr;
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/host_cleared.rs").unwrap();
        let lsp_uri = tower_lsp_server::ls_types::Uri::from_str(uri.as_str()).unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("rust"), None)
            .await;
        assert!(
            server
                .resolve_host_bridge_context(&lsp_uri, "textDocument/diagnostic")
                .is_some(),
            "sanity: host context resolves with a tree present"
        );

        // did_change clears the tree synchronously.
        server
            .documents
            .update_document(uri.clone(), "fn changed() {}".to_string(), None);
        assert!(
            server.documents.get(&uri).unwrap().tree().is_none(),
            "precondition: the tree is cleared"
        );
        assert!(
            server
                .resolve_host_bridge_context(&lsp_uri, "textDocument/diagnostic")
                .is_some(),
            "host context must survive a cleared tree (it needs only text, not the tree)"
        );
    }

    /// The shared freshness helper never parses inline (parse-snapshot ADR
    /// §3: the reader on-demand parse was a resurrection vector). With no
    /// reparse scheduled for a cleared tree, it returns within its bounded
    /// wait and the caller degrades to its empty fallback instead of
    /// resurrecting or restoring anything.
    // Paused time: with no snapshot ever published, the bounded wait would
    // otherwise burn its full first-parse backstop in real time.
    #[tokio::test(start_paused = true)]
    async fn ensure_document_parsed_never_parses_inline() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        let uri = Url::parse("file:///test/cleared.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        server.ensure_document_parsed(&uri).await;

        assert!(
            server.documents.get(&uri).unwrap().tree().is_none(),
            "no reparse was scheduled, so no tree may appear — readers never parse inline"
        );
    }

    /// `parse_document` now records its result onto the already-registered document
    /// in place (non-inserting). If the document was closed before the parse ran
    /// (no entry in the store), the parse must NOT resurrect it — it reads no text,
    /// stores nothing, and returns. This is the resurrection-safety the open parse
    /// gains for free once it moves off the ingress ticket.
    #[tokio::test]
    async fn parse_document_does_not_resurrect_a_closed_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/closed_before_parse.rs").unwrap();
        // The document is absent (a didClose removed it before this parse ran).
        assert!(server.documents.get(&uri).is_none());

        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("rust"), None)
            .await;

        assert!(
            server.documents.get(&uri).is_none(),
            "parse_document must not resurrect a document closed before it ran"
        );
    }

    /// Regression (parse-actor flip): the debounced diagnostic — which drives the
    /// on-edit host re-sync (#431) that keeps a push host's diagnostics following
    /// edits — must be scheduled AFTER the off-ingress reparse, not in the
    /// `did_change` handler. The handler clears the tree, and
    /// `prepare_diagnostic_snapshot` returns `None` without a tree, so scheduling
    /// the debounce there would capture a `None` snapshot and silently skip the
    /// re-sync (the diagnostics-don't-follow-edits bug). This pins the mechanism:
    /// the snapshot is `None` with the tree cleared and valid again once the
    /// reparse restores it.
    #[tokio::test]
    async fn diagnostic_snapshot_needs_the_reparsed_tree_not_the_cleared_one() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/diag_follow.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("rust"), None)
            .await;
        assert!(
            server
                .diagnostic_scheduler()
                .prepare_diagnostic_snapshot(&uri)
                .is_some(),
            "a parsed self-host doc yields a diagnostic snapshot"
        );

        // What did_change does synchronously: apply the edit and CLEAR the tree.
        server
            .documents
            .update_document(uri.clone(), "fn changed() {}".to_string(), None);
        assert!(
            server
                .diagnostic_scheduler()
                .prepare_diagnostic_snapshot(&uri)
                .is_none(),
            "with the tree cleared, the snapshot is None — scheduling the debounce \
             here (as the handler used to) would skip the on-edit host re-sync"
        );

        // The off-ingress reparse restores the tree → the snapshot is valid again,
        // which is exactly why the debounce is scheduled from the reparse loop.
        server
            .parse_coordinator()
            .reparse_latest(&uri, Some(1))
            .await;
        assert!(
            server
                .diagnostic_scheduler()
                .prepare_diagnostic_snapshot(&uri)
                .is_some(),
            "after the reparse the snapshot is valid again (the debounce is scheduled here)"
        );
    }

    /// Resurrection safety (#480 off-ingress install): a `didClose` landing while
    /// the spawned auto-install runs removes the document; the late
    /// `reparse_installed_document` must NOT recreate it.
    #[tokio::test]
    async fn reparse_installed_document_does_not_resurrect_closed_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/closed_during_install.rs").unwrap();
        // The document was closed during the install: nothing is in the store.
        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), Some("rust".to_string()))
            .await;

        assert!(
            server.documents.get(&uri).is_none(),
            "a reparse after the document was closed must not resurrect it"
        );
    }

    /// The off-ingress install reparse re-reads the latest store text and attaches
    /// a tree to the still-open document.
    #[tokio::test]
    async fn reparse_installed_document_parses_the_open_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/installed.rs").unwrap();
        // Registered (as on didOpen) but not yet parsed — the parser had just
        // finished installing.
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        assert!(server.documents.get(&uri).unwrap().tree().is_none());

        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), Some("rust".to_string()))
            .await;

        assert!(
            server.documents.get(&uri).unwrap().tree().is_some(),
            "the reparse must attach a tree to the open document"
        );
    }

    /// Off-ingress edit reparse (per-document-parse-scheduler flip): `reparse_latest`
    /// parses the latest store text and advances the watermark, but must NOT
    /// resurrect a document a `didClose` removed mid-parse.
    #[tokio::test]
    async fn reparse_latest_does_not_resurrect_closed_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/closed_during_reparse.rs").unwrap();
        // Document already closed (removed) — models a didClose racing the
        // scheduled reparse. The watermark ticket still resolves.
        server
            .parse_coordinator()
            .reparse_latest(&uri, Some(7))
            .await;

        assert!(
            server.documents.get(&uri).is_none(),
            "the off-ingress reparse must not resurrect a closed document"
        );
    }

    /// `reparse_latest` attaches a fresh tree to the open document (whose tree was
    /// cleared by the edit) and advances the watermark to the edit's ticket.
    #[tokio::test]
    async fn reparse_latest_parses_and_advances_watermark() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/edited.rs").unwrap();
        // The edit applied new text and cleared the tree (tree = None), exactly as
        // did_change does before scheduling the reparse.
        server.documents.insert(
            uri.clone(),
            "fn edited() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        assert!(server.documents.get(&uri).unwrap().tree().is_none());

        server
            .parse_coordinator()
            .reparse_latest(&uri, Some(3))
            .await;

        assert!(
            server.documents.get(&uri).unwrap().tree().is_some(),
            "the off-ingress reparse attaches a tree to the edited document"
        );
        // The watermark advance itself is still exercised by the store's own
        // watermark tests; the reader-side wait (`wait_for_epoch`) was removed
        // with the snapshot conversion (parse-snapshot ADR Stage 2).
    }

    /// If a concurrent `didChange` already parsed the document (a tree is
    /// present), the install reparse must short-circuit and not redundantly
    /// reparse / clobber it.
    #[tokio::test]
    async fn reparse_installed_document_skips_when_a_tree_is_already_present() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        configure_rust_self_host(server);

        let uri = Url::parse("file:///test/already_parsed.rs").unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn already() {}", None).unwrap();
        let tree_id = tree.root_node().id();
        server.documents.insert(
            uri.clone(),
            "fn already() {}".to_string(),
            Some("rust".to_string()),
            Some(tree),
        );

        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), Some("rust".to_string()))
            .await;

        let doc = server.documents.get(&uri).unwrap();
        assert_eq!(
            doc.tree().unwrap().root_node().id(),
            tree_id,
            "the existing tree must be left untouched (no redundant reparse)"
        );
    }

    /// Parse-decoupled lifecycle (parse-decoupled-document-lifecycle ADR): the
    /// host document is attached to its `_self` server during `did_open_impl`
    /// WITHOUT waiting for the tree-sitter parse. The host tier needs only the
    /// text and a language name, never the tree, so an unbounded/slow parse (or
    /// install) must not gate it.
    ///
    /// We block the parse deterministically by holding the parser-pool lock, so
    /// the (now off-ingress, #6) spawned `parse_document` -> `parse_with_pool` parks
    /// on `pool.lock().await` and never finishes within the test. The host-tier
    /// hoist runs synchronously in the handler BEFORE the parse is spawned, so the
    /// host doc still attaches while the parse is blocked — proving it does not wait
    /// for the parse. (Before the hoist the attach sat *after* the parse, so with the
    /// pool lock held the host doc would never open and this test would time out.)
    // Holding the sync parser-pool guard across awaits is this test's blocking
    // mechanism (the parked parse blocks a compute-pool thread, not this task,
    // so no deadlock): the lint's advice would defeat the test.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn host_document_attaches_before_parse_completes() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        configure_rust_self_host(server);
        server.bridge.insert_ready_test_connection("rust_ls").await;

        let uri = Url::parse("file:///test/host_attach_no_wait.rs").unwrap();
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");

        // Hold the parser-pool lock so the spawned parse parks indefinitely
        // (above) — its compute-pool work-unit blocks acquiring this sync mutex.
        use crate::error::LockResultExt;
        let pool_guard = server
            .parser_pool
            .lock()
            .recover_poison("test:host_document_attaches_before_parse_completes");

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: lsp_uri,
                language_id: "rust".to_string(),
                version: 1,
                text: "fn main() {}".to_string(),
            },
        };

        // `did_open_impl` returns immediately — the open parse is off the ingress
        // ticket (#6) and parked on the held pool lock. The host doc must already be
        // attaching by the time it returns, since the hoist precedes the spawn.
        server.did_open_impl(params).await;

        timeout(Duration::from_secs(2), async {
            loop {
                if server
                    .bridge
                    .pool()
                    .is_host_document_opened(&uri, "rust_ls")
                    .await
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(
            "host document must attach to the _self server while the parse is blocked \
             (the hoist precedes the off-ingress parse spawn)",
        );

        // Cleanup: release the lock so the parked spawned parse can finish.
        drop(pool_guard);
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
            .parse_document(uri.clone(), Some("rust"), None)
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

    /// #489: in one-shot CLI mode no editor consumes a proactive
    /// `publishDiagnostics`, so `did_open_impl` must NOT schedule the synthetic
    /// diagnostic task — that avoids the wasted downstream pull and the
    /// abort-vs-`didClose` race. The LSP-mode control below proves the assertion
    /// discriminates (the task IS scheduled when not in CLI mode), so a gate that
    /// suppressed it unconditionally would fail the control.
    #[tokio::test]
    async fn cli_mode_did_open_does_not_schedule_synthetic_diagnostic_task() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_md::LANGUAGE.into());
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        });
        server.mark_cli_mode();

        let uri = Url::parse("file:///test/cli_no_synthetic.md").unwrap();
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");
        server
            .did_open_impl(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: lsp_uri,
                    language_id: "markdown".to_string(),
                    version: 1,
                    text: "# hi\n".to_string(),
                },
            })
            .await;

        assert!(
            !server.synthetic_diagnostics.has_active_task(&uri),
            "CLI-mode didOpen must not schedule a synthetic diagnostic task (#489)"
        );
    }

    /// Control for the test above: in LSP mode (the default) `did_open_impl` DOES
    /// schedule the synthetic diagnostic task. Without this, a no-op gate would let
    /// the CLI-suppression test pass vacuously.
    #[tokio::test]
    async fn lsp_mode_did_open_schedules_synthetic_diagnostic_task() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_md::LANGUAGE.into());
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        });

        let uri = Url::parse("file:///test/lsp_synthetic.md").unwrap();
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");
        server
            .did_open_impl(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: lsp_uri,
                    language_id: "markdown".to_string(),
                    version: 1,
                    text: "# hi\n".to_string(),
                },
            })
            .await;

        // The present-parser open parse now runs off the ingress ticket (#6), so the
        // synthetic diagnostic is scheduled by the spawned task once the tree lands —
        // asynchronously after `did_open_impl` returns.
        wait_until(|| server.synthetic_diagnostics.has_active_task(&uri)).await;
        assert!(
            server.synthetic_diagnostics.has_active_task(&uri),
            "LSP-mode didOpen schedules the synthetic diagnostic task"
        );
    }
}
