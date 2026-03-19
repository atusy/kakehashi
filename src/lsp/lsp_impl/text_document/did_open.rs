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
            .get_language_for_path(uri.path())
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
                        .maybe_auto_install_language(lang, uri.clone(), text.clone(), false)
                        .await;
                } else {
                    // Notify user that parser is missing and needs manual installation
                    let reason = self.auto_install_disabled_reason();
                    self.notify_parser_missing(lang, &reason).await;
                }
            }
        }

        // Only parse if auto-install was NOT triggered
        // If auto-install was triggered, reload_language_after_install will call parse_document
        // after the parser file is completely written, preventing race condition
        if !skip_parse {
            self.parse_document(
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
        self.process_injections(&uri, false).await;

        // ADR-0020 Phase 2: Trigger synthetic diagnostic push on didOpen
        // This provides proactive diagnostics for clients that don't support pull diagnostics.
        // Note: We use the already-cloned lsp_uri here (it was cloned at the start of the method).
        self.spawn_synthetic_diagnostic_task(uri, lsp_uri);

        // NOTE: No semantic_tokens_refresh() on didOpen.
        // Capable LSP clients should request by themselves.
        // Calling refresh would be redundant and can cause deadlocks with clients
        // like vim-lsp that don't respond to workspace/semanticTokens/refresh requests.

        self.notifier().log_info("file opened!").await;
    }
}
