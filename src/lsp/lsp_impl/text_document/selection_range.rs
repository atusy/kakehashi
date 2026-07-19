//! Selection range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{SelectionRange, SelectionRangeParams};

use crate::analysis::handle_selection_range;

use super::super::{Kakehashi, uri_to_url};

/// The explicit-action bounded wait (parse-snapshot ADR §3): `selectionRange`
/// is keyboard-triggered expand/shrink — a silent no-op on a consciously
/// triggered action is jarring, and the request is not per-keystroke, so it
/// may briefly wait for the in-flight parse to land before falling back to
/// `ContentModified`.
const SELECTION_RANGE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

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
            match &view.slot.snapshot {
                Some(snapshot) if snapshot.parsed_version == view.content_version => {
                    break std::sync::Arc::clone(snapshot);
                }
                _ => {
                    // No snapshot yet (first parse in flight) or trailing an
                    // edit: wait for the next publish, bounded by the deadline.
                    let wait = tokio::time::timeout_at(deadline, receiver.changed()).await;
                    match wait {
                        // A publish (or close) landed — loop and re-resolve.
                        Ok(Ok(())) => continue,
                        // Channel closed: the document is gone.
                        Ok(Err(_)) => return Ok(None),
                        // Deadline passed. A stale snapshot exists → the
                        // coordinates can't be answered: ContentModified. No
                        // snapshot at all (first parse still running) → the
                        // pre-snapshot behavior: null.
                        Err(_elapsed) => {
                            return if view.slot.snapshot.is_some() {
                                Err(crate::error::content_modified_error())
                            } else {
                                Ok(None)
                            };
                        }
                    }
                }
            }
        };

        // A resolved-but-tree-less snapshot (no parser installed / crashed
        // grammar) cannot produce selection ranges.
        if snapshot.tree.is_none() {
            return Ok(None);
        }
        let expected_version = snapshot.parsed_version;
        let expected_incarnation = snapshot.incarnation;
        let expected_settings_generation = self.cache.semantic_token_generation();

        let worker_positions = positions
            .iter()
            .map(|position| crate::tree_worker::WirePosition {
                line: position.line,
                character: position.character,
            })
            .collect();
        let worker = self.tree_worker_shadow.selection_ranges(
            &uri,
            expected_incarnation,
            expected_version,
            expected_settings_generation,
            worker_positions,
        );

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
        let authoritative = self.compute_pool.run(None, move || {
            let mut pool = language.create_document_parser_pool();
            handle_selection_range(
                &snapshot.text,
                snapshot.tree.as_ref(),
                snapshot.language.as_deref(),
                &positions,
                &language,
                &mut pool,
            )
        });
        let (worker, result) = tokio::join!(worker, authoritative);

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

        if let (Some(worker), Some(authoritative)) = (worker, result.as_ref()) {
            let worker = worker
                .ranges
                .into_iter()
                .map(selection_range_from_wire)
                .collect::<Vec<_>>();
            if &worker != authoritative {
                log::debug!(
                    target: "kakehashi::tree_worker_shadow",
                    "host-only selectionRange mismatch uri={} version={} authoritative={:?} worker={:?}",
                    uri,
                    expected_version,
                    authoritative,
                    worker,
                );
            }
        }

        // None = the work-unit panicked (logged by the pool); serve the
        // no-result fallback rather than an error.
        Ok(result)
    }
}

fn selection_range_from_wire(range: crate::tree_worker::WireSelectionRange) -> SelectionRange {
    SelectionRange {
        range: tower_lsp_server::ls_types::Range::new(
            position_from_wire(range.range.start),
            position_from_wire(range.range.end),
        ),
        parent: range
            .parent
            .map(|parent| Box::new(selection_range_from_wire(*parent))),
    }
}

fn position_from_wire(
    position: crate::tree_worker::WirePosition,
) -> tower_lsp_server::ls_types::Position {
    tower_lsp_server::ls_types::Position::new(position.line, position.character)
}
