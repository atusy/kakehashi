//! Shared parsing orchestration for host documents.

use tree_sitter::InputEdit;
use url::Url;

use super::Kakehashi;
use super::coordinator::ParseCoordinator;

impl Kakehashi {
    fn parse_coordinator(&self) -> ParseCoordinator<'_> {
        ParseCoordinator::new(self)
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
        self.parse_coordinator()
            .parse_with_pool(language_name, uri, text_len, parse_fn)
            .await
    }

    pub(super) async fn parse_document(
        &self,
        uri: Url,
        text: String,
        language_id: Option<&str>,
        edits: Vec<InputEdit>,
    ) {
        self.parse_coordinator()
            .parse_document(uri, text, language_id, edits)
            .await;
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
        self.parse_coordinator().get_language_for_document(uri)
    }
}
