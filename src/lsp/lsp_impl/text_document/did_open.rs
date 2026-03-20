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

        // ADR-0020 Phase 2: Trigger synthetic diagnostic push on didOpen
        // This provides proactive diagnostics for clients that don't support pull diagnostics.
        // Note: We use the already-cloned lsp_uri here (it was cloned at the start of the method).
        self.diagnostic_scheduler()
            .spawn_synthetic_diagnostic_task(uri, lsp_uri);

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
    use crate::config::settings::BridgeServerConfig;
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
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        server
            .language
            .register_language_for_test("markdown", tree_sitter_md::LANGUAGE.into());
        server
            .language
            .register_language_for_test("rust", tree_sitter_rust::LANGUAGE.into());

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
            .register_injection_query_for_test("markdown", injection_query);

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
}
