//! Shared parsing orchestration for host documents.

use tree_sitter::InputEdit;
use url::Url;

use super::Kakehashi;

/// Timeout for spawn_blocking parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl Kakehashi {
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
    pub(super) async fn parse_with_pool<T, F>(
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
        // Checkout parser from pool (brief lock)
        let parser = {
            let mut pool = self.parser_pool.lock().await;
            pool.acquire(language_name)
        };

        let parser = parser?;

        // Parse in spawn_blocking with timeout to avoid blocking tokio worker thread
        // and prevent infinite hangs on pathological input
        let result = tokio::time::timeout(
            PARSE_TIMEOUT,
            tokio::task::spawn_blocking(move || parse_fn(parser)),
        )
        .await;

        // Handle timeout vs successful completion
        match result {
            Ok(Ok((parser, value))) => {
                // Return parser to pool (brief lock)
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
                // Parser is lost in the task (panicked or cancelled)
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
                // Parser is lost in the still-running blocking task
                None
            }
        }
    }

    pub(super) async fn parse_document(
        &self,
        uri: Url,
        text: String,
        language_id: Option<&str>,
        edits: Vec<InputEdit>,
    ) {
        let parse_generation = self.documents.mark_parse_started(&uri);
        let mut events = Vec::new();

        // ADR-0005: Detection fallback chain via LanguageCoordinator
        // Host document: token is None (no code fence identifier)
        let language_name = self
            .language
            .detect_language(uri.path(), &text, None, language_id);

        if let Some(language_name) = language_name {
            // Check if this parser has previously crashed
            if self.auto_install.is_parser_failed(&language_name) {
                log::warn!(
                    target: "kakehashi::crash_recovery",
                    "Skipping parsing for '{}' - parser previously crashed",
                    language_name
                );
                // Store document without parsing
                self.documents
                    .insert(uri.clone(), text, Some(language_name), None);
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
                self.notifier().log_language_events(&events).await;
                return;
            }

            // Ensure language is loaded
            let load_result = self.language.ensure_language_loaded(&language_name);
            events.extend(load_result.events.clone());

            // Parse the document with crash detection via parse_with_pool
            // Get old tree for incremental parsing before entering the closure
            // For edits: get edited tree (after tree.edit() applied)
            // For full parse: get current tree as-is
            let (base_tree, pre_edit_tree) = if !edits.is_empty() {
                let edited = self.documents.get_edited_tree(&uri, &edits);
                // Clone for storage - we need to keep the edited tree for changed_ranges()
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
                    // Record that we're about to parse (for crash detection)
                    let _ = auto_install.begin_parsing(&language_name_clone);

                    let parse_result = parser.parse(&text_clone, base_tree.as_ref());

                    // Parsing succeeded without crash - clear the state for this language
                    let _ = auto_install.end_parsing(&language_name_clone);

                    // Return both parse result and edited tree for proper changed_ranges support
                    (parser, parse_result.map(|tree| (tree, pre_edit_tree)))
                })
                .await;

            // Store the parsed document
            if let Some((tree, pre_edit_tree)) = parsed_tree {
                // Populate InjectionMap with injection regions for targeted cache invalidation
                self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.region_id_tracker(),
                );

                if let Some(edited_tree) = pre_edit_tree {
                    // Use the new method that preserves the edited tree for changed_ranges()
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

        // Store unparsed document
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
    pub(super) fn get_language_for_document(&self, uri: &Url) -> Option<String> {
        let path = uri.path();

        // Get the document's language_id and content if available
        let (language_id, content) = self
            .documents
            .get(uri)
            .map(|doc| {
                (
                    doc.language_id().map(|s| s.to_string()),
                    doc.text().to_string(),
                )
            })
            .unwrap_or((None, String::new()));

        // ADR-0005: Unified detection chain with alias resolution at each step
        // Priority: languageId → heuristics (first-line, filename) → extension
        // Host document: token=None (no code fence identifier)
        self.language
            .detect_language(path, &content, None, language_id.as_deref())
    }
}
