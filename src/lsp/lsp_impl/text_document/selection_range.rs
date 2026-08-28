//! Selection range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    PartialResultParams, Position, SelectionRange, SelectionRangeParams, TextDocumentIdentifier,
    Uri, WorkDoneProgressParams,
};

use crate::analysis::handle_selection_range;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

use super::super::{Kakehashi, uri_to_url};

const METHOD: &str = "textDocument/selectionRange";

fn parse_single_host_selection_range(
    value: serde_json::Value,
    position: Position,
    document_end: Position,
) -> Option<SelectionRange> {
    let mut ranges = parse_host_verbatim::<Vec<SelectionRange>>(value)?;
    if ranges.len() != 1 {
        return None;
    }
    let selection = ranges.pop().expect("length checked");
    selection_chain_is_valid(&selection, position, document_end).then_some(selection)
}

fn selection_chain_is_valid(
    selection: &SelectionRange,
    position: Position,
    document_end: Position,
) -> bool {
    let mut child = None;
    let mut current = Some(selection);
    while let Some(selection) = current {
        let range = selection.range;
        if range.start > range.end
            || range.end > document_end
            || child.is_some_and(|child: tower_lsp_server::ls_types::Range| {
                range.start > child.start || range.end < child.end
            })
        {
            return false;
        }
        if child.is_none() && !(range.start <= position && position <= range.end) {
            return false;
        }
        child = Some(range);
        current = selection.parent.as_deref();
    }
    true
}

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

        // A resolved-but-tree-less snapshot cannot produce selection ranges. See
        // `ParseSnapshot` for the causes — they include a settings-reload
        // placeholder that reads as current, so this is not only a failure path:
        // the wait above breaks on that placeholder and answers `null` instead
        // of settling for its reparse (#923).
        if snapshot.tree.is_none() {
            return Ok(None);
        }
        let expected_version = snapshot.parsed_version;
        let expected_incarnation = snapshot.incarnation;
        let expected_settings_generation = self.cache.semantic_token_generation();
        let document_end = crate::text::PositionMapper::new(&snapshot.text)
            .byte_to_position(snapshot.text.len())
            .unwrap_or_default();

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
        let native_positions = positions.clone();
        let result = self
            .compute_pool
            .run(None, move || {
                let mut pool = language.create_document_parser_pool();
                handle_selection_range(
                    &snapshot.text,
                    snapshot.tree.as_ref(),
                    snapshot.language.as_deref(),
                    &native_positions,
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

        let native_ranges = result.unwrap_or_default();
        if native_ranges.len() != positions.len() {
            return Ok(None);
        }

        // SelectionRange is position-aligned: choosing one winning layer for
        // the whole array would misroute multi-cursor requests whose positions
        // belong to different virtual regions. Resolve preferred layers once
        // per position and preserve the editor's input order.
        let mut selected = Vec::with_capacity(positions.len());
        for (index, position) in positions.into_iter().enumerate() {
            let raw_params = serde_json::to_value(SelectionRangeParams {
                text_document: TextDocumentIdentifier {
                    uri: lsp_uri.clone(),
                },
                positions: vec![position],
                // One upstream progress token cannot be forwarded to multiple
                // independent downstream requests without token collisions.
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .unwrap_or(serde_json::Value::Null);
            let virt = self.selection_range_virt_layer(&lsp_uri, position);
            let native = std::future::ready(Ok(native_ranges.get(index).cloned()));
            let result = self
                .walk_layers_with_native(
                    &lsp_uri,
                    METHOD,
                    METHOD,
                    raw_params,
                    virt,
                    native,
                    |value| parse_single_host_selection_range(value, position, document_end),
                    |_| true,
                )
                .await?;
            let Some(result) = result else {
                // The protocol requires one result for every input position;
                // never surface a shorter, index-shifted response.
                return Ok(None);
            };
            selected.push(result);
        }

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

        Ok(Some(selected))
    }

    async fn selection_range_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
    ) -> Result<Option<SelectionRange>> {
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };
        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let result = dispatch_preferred(
            &ctx.document,
            pool,
            |task| async move {
                task.pool
                    .send_selection_range_request(
                        &task.server_name,
                        &task.server_config,
                        &task.uri,
                        position,
                        task.region_end(),
                        &task.injection_language,
                        &task.region_id,
                        task.offset,
                        &task.virtual_content,
                        task.upstream_id,
                    )
                    .await
            },
            |result| result.is_some(),
            cancel_rx,
        )
        .await;
        result
            .handle(&self.notifier(), "selectionRange", None, Ok)
            .await
    }
}
