//! Selection range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    PartialResultParams, Position, SelectionRange, SelectionRangeParams, TextDocumentIdentifier,
    Uri, WorkDoneProgressParams,
};

use crate::analysis::handle_selection_range;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::current_upstream_id;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

use super::super::{Kakehashi, uri_to_url};

const METHOD: &str = "textDocument/selectionRange";

fn parse_single_host_selection_range(
    value: serde_json::Value,
    position: Position,
    text: &str,
) -> Option<SelectionRange> {
    let mut ranges = parse_host_verbatim::<Vec<SelectionRange>>(value)?;
    if ranges.len() != 1 {
        return None;
    }
    let selection = ranges.pop().expect("length checked");
    selection_chain_is_valid(&selection, position, text).then_some(selection)
}

fn selection_chain_is_valid(selection: &SelectionRange, position: Position, text: &str) -> bool {
    let mapper = crate::text::PositionMapper::new(text);
    let mut child = None;
    let mut current = Some(selection);
    while let Some(selection) = current {
        let range = selection.range;
        if range.start > range.end
            || mapper.position_to_byte_strict(range.start).is_none()
            || mapper.position_to_byte_strict(range.end).is_none()
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

/// Cancels blocking selection work when its async owner is dropped.
struct SelectionComputeCancelGuard {
    token: crate::cancel::CancelToken,
    armed: bool,
}

impl SelectionComputeCancelGuard {
    fn new(token: crate::cancel::CancelToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SelectionComputeCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

impl Kakehashi {
    pub(crate) async fn selection_range_impl(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let cancel_token = crate::cancel::CancelToken::default();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(current_upstream_id().as_ref());
        let request = self.selection_range_inner(params, cancel_token.clone());
        match cancel_rx {
            Some(mut cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => {
                        cancel_token.cancel();
                        Err(tower_lsp_server::jsonrpc::Error::request_cancelled())
                    },
                    result = request => result,
                }
            }
            None => request.await,
        }
    }

    async fn selection_range_inner(
        &self,
        params: SelectionRangeParams,
        cancel_token: crate::cancel::CancelToken,
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
        let Some(version_cancel) = self.documents.get(&uri).and_then(|document| {
            (document.incarnation() == expected_incarnation
                && document.content_version() == expected_version)
                .then(|| document.version_cancel_token())
        }) else {
            return Err(crate::error::content_modified_error());
        };

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
        let native_snapshot = std::sync::Arc::clone(&snapshot);
        let compute_cancel = cancel_token.clone();
        let worker_cancel = compute_cancel.clone();
        let mut compute_guard = SelectionComputeCancelGuard::new(compute_cancel.clone());
        let compute = self
            .compute_pool
            .run(Some(compute_cancel.clone()), move || {
                let mut pool = language.create_document_parser_pool();
                handle_selection_range(
                    &native_snapshot.text,
                    native_snapshot.tree.as_ref(),
                    native_snapshot.language.as_deref(),
                    &native_positions,
                    &language,
                    &mut pool,
                    &worker_cancel,
                )
            });
        let result = tokio::select! {
            result = compute => result,
            _ = version_cancel.cancelled() => {
                compute_cancel.cancel();
                return Err(crate::error::content_modified_error());
            }
        };
        compute_guard.disarm();

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
            let virt = self.selection_range_virt_layer(&lsp_uri, position, expected_incarnation);
            let host = self.selection_range_host_layer(
                &lsp_uri,
                raw_params,
                position,
                expected_incarnation,
                expected_version,
            );
            let native = std::future::ready(Ok(native_ranges.get(index).cloned()));
            let result = self
                .walk_layer_futures(&lsp_uri, METHOD, METHOD, virt, host, native, |_| true)
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
        expected_incarnation: u64,
    ) -> Result<Option<SelectionRange>> {
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };
        if ctx.incarnation != expected_incarnation {
            return Ok(None);
        }
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
                        expected_incarnation,
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

    #[allow(clippy::too_many_arguments)]
    async fn selection_range_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
        position: Position,
        expected_incarnation: u64,
        expected_version: u64,
    ) -> Result<Option<SelectionRange>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        if ctx.incarnation != expected_incarnation || ctx.content_version != expected_version {
            return Ok(None);
        }
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let result = dispatch_host_preferred(
            &ctx,
            self.bridge.pool_arc(),
            move |task: HostFanOutTask| {
                let params = raw_params.clone();
                async move {
                    let raw = task
                        .pool
                        .send_host_raw_request_for_incarnation(
                            &task.server_name,
                            &task.server_config,
                            &HostDocument {
                                uri: &task.uri,
                                language_id: &task.language_id,
                                text: &task.text,
                            },
                            METHOD,
                            params,
                            task.upstream_id,
                            expected_incarnation,
                        )
                        .await?;
                    Ok(raw.and_then(|raw| {
                        parse_single_host_selection_range(raw.value, position, &task.text)
                    }))
                }
            },
            |result| result.is_some(),
            cancel_rx,
        )
        .await;
        self.host_layer_result(result, METHOD, |won| won).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use tokio::time::sleep;
    use tower_lsp_server::LspService;

    use super::*;
    use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
    use crate::lsp::request_id::CancelForwarder;

    #[tokio::test]
    async fn aborting_selection_compute_owner_cancels_blocking_work() {
        let token = crate::cancel::CancelToken::default();
        let started = Arc::new(tokio::sync::Notify::new());
        let started_by_owner = Arc::clone(&started);
        let owner_token = token.clone();
        let notified = started.notified();
        let owner = tokio::spawn(async move {
            let _guard = SelectionComputeCancelGuard::new(owner_token);
            started_by_owner.notify_one();
            std::future::pending::<()>().await;
        });

        notified.await;
        owner.abort();
        let _ = owner.await;

        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_covers_the_native_snapshot_wait() {
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, pool, cancel_forwarder.clone())
        });
        let server = service.inner();
        let uri = url::Url::parse("file:///selection_range_cancel.lua").expect("test URI");
        server.documents.insert(
            uri.clone(),
            "local value = 1\n".to_string(),
            Some("lua".to_string()),
            None,
        );
        let loaded = server.language.ensure_language_loaded("lua");
        if !loaded.success {
            eprintln!("Skipping: lua language parser not available");
            return;
        }

        let notifier = cancel_forwarder.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(1)).await;
            notifier.notify_cancel(&UpstreamId::Number(71));
        });
        let params = SelectionRangeParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("LSP URI"),
            },
            positions: vec![Position::new(0, 1)],
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let result = crate::lsp::request_id::CURRENT_REQUEST_ID
            .scope(
                Some(tower_lsp_server::jsonrpc::Id::Number(71)),
                server.selection_range_impl(params),
            )
            .await;

        assert_eq!(
            result
                .expect_err("the parked request should be cancelled")
                .code,
            tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled
        );
    }

    #[test]
    fn host_selection_range_rejects_an_overlong_intermediate_line_column() {
        let value = json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 999 }
            }
        }]);
        assert!(parse_single_host_selection_range(value, Position::new(0, 0), "a\nb").is_none());
    }
}
