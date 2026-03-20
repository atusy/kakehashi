use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use tower_lsp_server::Client;
use tree_sitter::InputEdit;
use url::Url;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::settings_manager::SettingsManager;

/// Timeout for spawn_blocking parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) struct ParseCoordinator<'a> {
    client: Client,
    language: &'a std::sync::Arc<LanguageCoordinator>,
    parser_pool: &'a tokio::sync::Mutex<DocumentParserPool>,
    documents: &'a DocumentStore,
    cache: &'a CacheCoordinator,
    settings_manager: &'a SettingsManager,
    auto_install: &'a AutoInstallManager,
    bridge: &'a BridgeCoordinator,
}

impl<'a> ParseCoordinator<'a> {
    pub(crate) fn new(server: &'a Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: &server.language,
            parser_pool: &server.parser_pool,
            documents: &server.documents,
            cache: &server.cache,
            settings_manager: &server.settings_manager,
            auto_install: &server.auto_install,
            bridge: &server.bridge,
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

    pub(crate) async fn parse_document(
        &self,
        uri: Url,
        text: String,
        language_id: Option<&str>,
        edits: Vec<InputEdit>,
    ) {
        let parse_generation = self.documents.mark_parse_started(&uri);
        let mut events = Vec::new();

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
                self.documents
                    .insert(uri.clone(), text, Some(language_name), None);
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
                self.notifier().log_language_events(&events).await;
                return;
            }

            let load_result = self.language.ensure_language_loaded(&language_name);
            events.extend(load_result.events);

            let (base_tree, pre_edit_tree) = if !edits.is_empty() {
                let edited = self.documents.get_edited_tree(&uri, &edits);
                let for_store = edited.clone();
                (edited, for_store)
            } else {
                let tree = self.documents.get(&uri).and_then(|doc| doc.tree().cloned());
                (tree, None)
            };

            let text_clone = text.clone();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();

            let parsed_tree = self
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    let _ = auto_install.begin_parsing(&language_name_clone);
                    let parse_result = parser.parse(&text_clone, base_tree.as_ref());
                    let _ = auto_install.end_parsing(&language_name_clone);
                    (parser, parse_result.map(|tree| (tree, pre_edit_tree)))
                })
                .await;

            if let Some((tree, pre_edit_tree)) = parsed_tree {
                self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    self.language,
                    self.bridge.region_id_tracker(),
                );

                if let Some(edited_tree) = pre_edit_tree {
                    self.documents.update_document_with_edited_tree(
                        uri.clone(),
                        text,
                        tree,
                        edited_tree,
                    );
                } else {
                    self.documents.insert(
                        uri.clone(),
                        text,
                        Some(language_name.clone()),
                        Some(tree),
                    );
                }

                self.documents
                    .mark_parse_finished(&uri, parse_generation, true);
                self.notifier().log_language_events(&events).await;
                return;
            }
        }

        self.documents.insert(uri.clone(), text, None, None);
        self.documents
            .mark_parse_finished(&uri, parse_generation, false);
        self.notifier().log_language_events(&events).await;
    }

    /// Get the language for a document using the full detection chain.
    ///
    /// Uses LanguageCoordinator::detect_language() which implements
    /// the fallback chain (ADR-0005): languageId → heuristics
    /// (token, path-derived token, first-line content) with alias
    /// resolution at each step.
    ///
    /// This ensures aliases are resolved (e.g., "rmd" → "markdown") even when
    /// the document is accessed before didOpen fully completes (race condition).
    pub(crate) fn get_language_for_document(&self, uri: &Url) -> Option<String> {
        let path = uri.path();

        if let Some(doc) = self.documents.get(uri) {
            self.language
                .detect_language(path, doc.text(), None, doc.language_id())
        } else {
            self.language.detect_language(path, "", None, None)
        }
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        ClientNotifier::new(
            self.client.clone(),
            self.settings_manager.client_capabilities_lock(),
        )
    }
}
