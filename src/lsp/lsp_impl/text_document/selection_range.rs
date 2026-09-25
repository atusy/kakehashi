//! Selection range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{SelectionRange, SelectionRangeParams};

use crate::analysis::handle_selection_range;

use super::super::{Kakehashi, uri_to_url};

/// The explicit-action bounded wait (parse-snapshot ADR §3): `selectionRange`
/// is keyboard-triggered expand/shrink — a silent no-op on a consciously
/// triggered action is jarring, and the request is not per-keystroke, so it
/// may briefly wait for the in-flight parse to land before falling back to
/// `ContentModified` (or `null` while a reload's reparse is pending). The same bound as the formatting verbs', but its own
/// loop rather than `wait_for_explicit_action_snapshot`: a document with no
/// snapshot yet also waits only this long, not the first-parse backstop.
const SELECTION_RANGE_WAIT: std::time::Duration =
    crate::lsp::lsp_impl::snapshot_read::EXPLICIT_ACTION_WAIT;

impl Kakehashi {
    pub(crate) async fn selection_range_impl(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let lsp_uri = params.text_document.uri;
        let positions = params.positions;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in selectionRange: {}", lsp_uri.as_str());
            return Ok(None);
        };

        // Get language for document
        let Some(language_name) = self.document_language(&uri) else {
            return Ok(None);
        };

        // Ensure language is loaded (handles race condition with didOpen)
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
        if !load_result.success {
            return Ok(None);
        }

        // Resolve the latest parse snapshot, waiting briefly (bounded) for a
        // *current* one — this reader's coordinates are authored against the
        // live text, so a trailing snapshot cannot answer it (ADR §3
        // staleness-reject, with the explicit-action wait). This replaces the
        // former reader on-demand parse: readers never parse inline.
        let deadline = tokio::time::Instant::now() + SELECTION_RANGE_WAIT;
        let snapshot = loop {
            // Subscribe BEFORE checking (lost-wakeup guard, see
            // snapshot_for_tokens), then re-resolve per iteration
            // (per-request re-resolution rule): a close/reopen between
            // wakeups is observed here, never served.
            let Some(mut receiver) = self.documents.subscribe_snapshots(&uri) else {
                return Ok(None);
            };
            let Some(view) = self.documents.latest_snapshot(&uri) else {
                // Unregistered or closed.
                return Ok(None);
            };
            let current = view
                .slot
                .snapshot
                .as_ref()
                .filter(|snapshot| snapshot.parsed_version == view.content_version);
            match current {
                Some(snapshot) if !snapshot.awaiting_reparse => {
                    break std::sync::Arc::clone(snapshot);
                }
                _ => {
                    // No snapshot yet (first parse in flight), trailing an
                    // edit, or a reload placeholder whose reparse is still
                    // queued: wait for the next publish, bounded by the
                    // deadline.
                    let wait = tokio::time::timeout_at(deadline, receiver.changed()).await;
                    match wait {
                        // A publish (or close) landed — loop and re-resolve.
                        Ok(Ok(())) => continue,
                        // Channel closed: the document is gone.
                        Ok(Err(_)) => return Ok(None),
                        // Deadline passed. A stale snapshot exists → the
                        // coordinates can't be answered: ContentModified. No
                        // parse of the current text yet — the first parse or
                        // a reload's reparse still running — → the
                        // pre-snapshot behavior: null (the coordinates are
                        // the live text's, there is just no tree for them).
                        Err(_elapsed) => {
                            return if current.is_none() && view.slot.snapshot.is_some() {
                                Err(crate::error::content_modified_error())
                            } else {
                                Ok(None)
                            };
                        }
                    }
                }
            }
        };

        // A completed parse that produced no tree cannot produce selection
        // ranges (see `ParseSnapshot` for the causes). The reload placeholder
        // never reaches here: the wait above settles for its reparse.
        if snapshot.tree.is_none() {
            return Ok(None);
        }
        let expected_version = snapshot.parsed_version;
        let expected_incarnation = snapshot.incarnation;
        let expected_settings_generation = self.cache.semantic_token_generation();

        // Run the synchronous injection-aware walk as one work-unit on the
        // compute pool against the snapshot's consistent (text, tree). The
        // walk uses a TRANSIENT parser pool: holding the shared parser-pool
        // mutex across the whole injection walk would block any concurrent
        // parse work-unit's brief acquire/release on it — pinning a second
        // compute thread for the walk's duration. Parser construction is
        // cheap (the grammars are already registered), and selectionRange is
        // a user-triggered, infrequent read, so per-request parsers beat
        // cross-request reuse here.
        let language = std::sync::Arc::clone(&self.language);
        let result = self
            .compute_pool
            .run(None, move || {
                let mut pool = language.create_document_parser_pool();
                handle_selection_range(
                    &snapshot.text,
                    snapshot.tree.as_ref(),
                    snapshot.language.as_deref(),
                    &positions,
                    &language,
                    &mut pool,
                )
            })
            .await;

        let still_current = self.documents.latest_snapshot(&uri).is_some_and(|view| {
            view.content_version == expected_version
                && view.slot.current_incarnation == expected_incarnation
                && view.slot.snapshot.is_some_and(|snapshot| {
                    snapshot.parsed_version == expected_version
                        && snapshot.incarnation == expected_incarnation
                })
        });
        if !still_current || self.cache.semantic_token_generation() != expected_settings_generation
        {
            return Err(crate::error::content_modified_error());
        }

        // None = the work-unit panicked (logged by the pool); serve the
        // no-result fallback rather than an error.
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{Position, TextDocumentIdentifier};
    use url::Url;

    use super::*;
    use crate::document::LanguageCheck;
    use crate::document::snapshot::ParseSnapshot;

    const TEXT: &str = "fn main() { let x = 1; }";

    fn rust_tree(text: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    /// A rust document with a published tree and its parser registered, so
    /// the handler passes its language gate.
    fn server_with_parsed_doc(uri: &Url) -> LspService<Kakehashi> {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        server.documents.insert(
            uri.clone(),
            TEXT.to_string(),
            Some("rust".to_string()),
            Some(rust_tree(TEXT)),
        );
        service
    }

    fn params(uri: &Url) -> SelectionRangeParams {
        SelectionRangeParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(uri).unwrap(),
            },
            positions: vec![Position::new(0, 16)],
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    /// A completed parse of the live version (with or without a tree),
    /// published through `install_parse` — the placeholder's replacement.
    fn install_current_parse(server: &Kakehashi, uri: &Url, tree: Option<tree_sitter::Tree>) {
        let view = server.documents.latest_snapshot(uri).unwrap();
        let installed = server.documents.install_parse(
            uri,
            LanguageCheck::Record,
            Arc::new(ParseSnapshot {
                text: Arc::from(TEXT),
                tree,
                language: Some("rust".to_string()),
                parsed_version: view.content_version,
                incarnation: view.slot.current_incarnation,
                injection_regions: None,
                regions: None,
                layer_trees: Arc::new(std::sync::OnceLock::new()),
                awaiting_reparse: false,
            }),
        );
        assert!(installed.published, "the reparse must land");
    }

    /// A settings reload replaces the tree with a version-current placeholder
    /// until its reparse lands. An expand-selection issued in that window
    /// must settle for the reparse, not answer `null` off the placeholder.
    #[tokio::test]
    async fn waits_for_the_reparse_behind_a_reload_placeholder() {
        let uri = Url::parse("file:///reload_placeholder.rs").unwrap();
        let service = server_with_parsed_doc(&uri);
        let server = service.inner();
        server.documents.invalidate_all_parses();

        let reparse = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            install_current_parse(server, &uri, Some(rust_tree(TEXT)));
        };
        let (result, ()) = tokio::join!(server.selection_range_impl(params(&uri)), reparse);

        let ranges = result
            .expect("the reparse is current")
            .expect("the reparse's tree answers the request");
        assert_eq!(ranges.len(), 1);
        assert!(
            ranges[0].parent.is_some(),
            "a tree-less walk answers one parentless range; the reparse's tree nests"
        );
    }

    /// A completed parse that produced no tree is a final answer: `null` at
    /// once, not after the wait meant for the placeholder.
    #[tokio::test]
    async fn answers_null_at_once_for_a_completed_tree_less_parse() {
        let uri = Url::parse("file:///tree_less_parse.rs").unwrap();
        let service = server_with_parsed_doc(&uri);
        let server = service.inner();
        server.documents.invalidate_all_parses();
        install_current_parse(server, &uri, None);

        let result = tokio::time::timeout(
            SELECTION_RANGE_WAIT / 2,
            server.selection_range_impl(params(&uri)),
        )
        .await
        .expect("a completed parse needs no wait");

        assert!(matches!(result, Ok(None)));
    }

    /// A reparse still queued at the deadline leaves the live text without a
    /// tree, not the request's coordinates stale: `null`, as before the first
    /// parse, rather than `ContentModified`.
    #[tokio::test(start_paused = true)]
    async fn answers_null_when_the_reparse_outlasts_the_wait() {
        let uri = Url::parse("file:///slow_reparse.rs").unwrap();
        let service = server_with_parsed_doc(&uri);
        let server = service.inner();
        server.documents.invalidate_all_parses();

        let result = server.selection_range_impl(params(&uri)).await;

        assert!(matches!(result, Ok(None)), "{result:?}");
    }
}
