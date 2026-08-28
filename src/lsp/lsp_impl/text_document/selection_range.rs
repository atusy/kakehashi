//! Selection range method for Kakehashi.

use futures::FutureExt;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    PartialResultParams, Position, SelectionRange, SelectionRangeParams, TextDocumentIdentifier,
    Uri, WorkDoneProgressParams,
};

use crate::analysis::handle_selection_range;
use crate::config::settings::LayerSource;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::current_upstream_id;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

use super::super::{Kakehashi, uri_to_url};

const METHOD: &str = "textDocument/selectionRange";

struct HostSelectionPass {
    results: Vec<Option<SelectionRange>>,
    incarnation: u64,
    content_version: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectionLayer {
    Virt,
    Host,
    Native,
}

impl HostSelectionPass {
    fn matches_snapshot(&self, incarnation: u64, content_version: u64) -> bool {
        self.incarnation == incarnation && self.content_version == content_version
    }

    fn into_complete(self) -> Option<Vec<SelectionRange>> {
        self.results.into_iter().collect()
    }
}

fn settle_timed_out_selection(
    host: Option<Vec<SelectionRange>>,
    has_stale_snapshot: bool,
) -> Result<Option<Vec<SelectionRange>>> {
    if host.is_some() || !has_stale_snapshot {
        Ok(host)
    } else {
        Err(crate::error::content_modified_error())
    }
}

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

fn normalize_position(text: &str, position: Position) -> Option<Position> {
    let mapper = crate::text::PositionMapper::new(text);
    mapper.byte_to_position(mapper.position_to_byte_clamped(position))
}

fn selection_chain_is_valid(selection: &SelectionRange, position: Position, text: &str) -> bool {
    let mapper = crate::text::PositionMapper::new(text);
    let document_end = mapper.byte_to_position(text.len());
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
        if child.is_none()
            && !((range.start == position && range.end == position)
                || (range.start <= position
                    && (position < range.end
                        || (Some(position) == document_end && position == range.end))))
        {
            return false;
        }
        child = Some(range);
        current = selection.parent.as_deref();
    }
    true
}

fn append_containing_selection_ancestors(
    mut selection: SelectionRange,
    mut ancestors: SelectionRange,
) -> (SelectionRange, bool) {
    fn attach_to_tail(selection: &mut SelectionRange, ancestors: SelectionRange) {
        match selection.parent.as_mut() {
            Some(parent) => attach_to_tail(parent, ancestors),
            None => selection.parent = Some(Box::new(ancestors)),
        }
    }
    let mut outer = &selection;
    while let Some(parent) = outer.parent.as_ref() {
        outer = parent;
    }
    let outer = outer.range;
    loop {
        let strictly_contains = ancestors.range.start <= outer.start
            && ancestors.range.end >= outer.end
            && ancestors.range != outer;
        if strictly_contains {
            attach_to_tail(&mut selection, ancestors);
            return (selection, true);
        }
        let Some(parent) = ancestors.parent.take() else {
            break;
        };
        ancestors = *parent;
    }
    (selection, false)
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

async fn race_native_producer<T>(
    native_producer: impl std::future::Future<Output = Result<()>>,
    layer_walk: impl std::future::Future<Output = Result<Option<T>>>,
) -> Result<Option<T>> {
    tokio::pin!(native_producer);
    tokio::pin!(layer_walk);
    tokio::select! {
        biased;
        result = &mut layer_walk => result,
        produced = &mut native_producer => {
            produced?;
            layer_walk.await
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
        let Some(bridge_language) = self.document_bridge_language(&uri) else {
            return Ok(None);
        };
        let parser_language = self.document_language(&uri);
        let expected_settings_generation = self.cache.semantic_token_generation();
        let layer_config = self.resolve_layer_config(&bridge_language, METHOD);
        let has_parse_layer =
            layer_config.allows(LayerSource::Virt) || layer_config.allows(LayerSource::Native);
        let allows_host = layer_config.allows(LayerSource::Host);
        let host_attempted = layer_config.priorities.first() == Some(&LayerSource::Host);
        let mut host_results = None;
        if host_attempted {
            let host = self
                .selection_range_host_pass(
                    &lsp_uri,
                    positions.clone(),
                    expected_settings_generation,
                )
                .await?;
            if let Some(host) = host {
                if host.results.iter().all(Option::is_some) || !has_parse_layer {
                    return Ok(host.into_complete());
                }
                host_results = Some(host);
            } else if !has_parse_layer {
                return Ok(None);
            }
        } else if !has_parse_layer {
            return Ok(None);
        }

        // Ensure language is loaded (handles race condition with didOpen)
        let parser_ready = match parser_language {
            Some(language) => {
                self.language
                    .ensure_language_loaded_async(&language)
                    .await
                    .success
            }
            None => false,
        };
        if !parser_ready {
            return if let Some(host) = host_results.take() {
                Ok(host.into_complete())
            } else if allows_host {
                self.selection_range_host_only(&lsp_uri, positions, expected_settings_generation)
                    .await
            } else {
                Ok(None)
            };
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
                            let host = if let Some(host) = host_results.take() {
                                host.into_complete()
                            } else if allows_host {
                                self.selection_range_host_only(
                                    &lsp_uri,
                                    positions,
                                    expected_settings_generation,
                                )
                                .await?
                            } else {
                                None
                            };
                            return settle_timed_out_selection(host, view.slot.snapshot.is_some());
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
            return if let Some(host) = host_results.take() {
                Ok(host.into_complete())
            } else if allows_host {
                self.selection_range_host_only(&lsp_uri, positions, expected_settings_generation)
                    .await
            } else {
                Ok(None)
            };
        }
        // LSP defaults an overlong character to the line end. Normalize once
        // against the exact snapshot and feed the same defended positions to
        // every layer, including the host request's outbound params.
        let Some(positions) = positions
            .iter()
            .map(|position| normalize_position(&snapshot.text, *position))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let expected_version = snapshot.parsed_version;
        let expected_incarnation = snapshot.incarnation;
        if host_results
            .as_ref()
            .is_some_and(|host| !host.matches_snapshot(expected_incarnation, expected_version))
        {
            return Err(crate::error::content_modified_error());
        }
        let Some(version_cancel) = self.documents.get(&uri).and_then(|document| {
            (document.incarnation() == expected_incarnation
                && document.content_version() == expected_version)
                .then(|| document.version_cancel_token())
        }) else {
            return Err(crate::error::content_modified_error());
        };

        let native_enabled = layer_config.allows(LayerSource::Native);
        let reusable_host_results = host_results.as_ref();
        let (native_tx, native_rx) = tokio::sync::watch::channel(None);
        let language = std::sync::Arc::clone(&self.language);
        let native_positions = positions.clone();
        let native_position_count = native_positions.len();
        let native_snapshot = std::sync::Arc::clone(&snapshot);
        let compute_cancel = cancel_token.clone();
        // Produce the native result concurrently with the bridge walk. When
        // native is disabled this future stays inert; when a higher-priority
        // bridge wins first, dropping it cancels any in-flight blocking work.
        let native_producer = async {
            if !native_enabled {
                std::future::pending::<()>().await;
            }
            let worker_cancel = compute_cancel.clone();
            let mut compute_guard = SelectionComputeCancelGuard::new(compute_cancel.clone());
            let result = self
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
                })
                .await;
            compute_guard.disarm();

            let still_current = self.documents.latest_snapshot(&uri).is_some_and(|view| {
                view.content_version == expected_version
                    && view.slot.current_incarnation == expected_incarnation
                    && view.slot.snapshot.is_some_and(|snapshot| {
                        snapshot.parsed_version == expected_version
                            && snapshot.incarnation == expected_incarnation
                    })
            });
            if !still_current
                || self.cache.semantic_token_generation() != expected_settings_generation
            {
                return Err(crate::error::content_modified_error());
            }
            let result = result.unwrap_or_default();
            if result.len() != native_position_count {
                let _ = native_tx.send(Some(std::sync::Arc::new(Vec::new())));
                return Ok(());
            }
            let _ = native_tx.send(Some(std::sync::Arc::new(result)));
            Ok(())
        };

        // SelectionRange is position-aligned: choosing one winning layer for
        // the whole array would misroute multi-cursor requests whose positions
        // belong to different virtual regions. Resolve preferred layers once
        // per position and preserve the editor's input order.
        let layer_walk = async {
            let priorities = &layer_config.priorities;
            let walks = positions.into_iter().enumerate().map(|(index, position)| {
                let native_rx = native_rx.clone();
                let lsp_uri = lsp_uri.clone();
                async move {
                    let virt = async {
                        self.selection_range_virt_layer(&lsp_uri, position, expected_incarnation)
                            .await
                            .map(|selection| {
                                selection.map(|selection| (selection, SelectionLayer::Virt))
                            })
                    };
                    let cached_host = reusable_host_results
                        .map(|host| &host.results)
                        .and_then(|results| results.get(index))
                        .cloned()
                        .flatten();
                    let host = async {
                        let selection = if reusable_host_results.is_some() {
                            Ok(cached_host)
                        } else {
                            self.selection_range_host_layer(
                                &lsp_uri,
                                position,
                                expected_incarnation,
                                expected_version,
                            )
                            .await
                        }?;
                        Ok(selection.map(|selection| (selection, SelectionLayer::Host)))
                    }
                    .boxed()
                    .shared();
                    let native = async {
                        if !native_enabled {
                            Ok(None)
                        } else {
                            let mut receiver = native_rx.clone();
                            loop {
                                if let Some(ranges) = receiver.borrow().as_ref() {
                                    break Ok(ranges
                                        .get(index)
                                        .cloned()
                                        .map(|selection| (selection, SelectionLayer::Native)));
                                }
                                if receiver.changed().await.is_err() {
                                    break Ok(None);
                                }
                            }
                        }
                    };
                    let result = self
                        .walk_layer_futures(
                            &lsp_uri,
                            METHOD,
                            METHOD,
                            virt,
                            host.clone(),
                            native,
                            |_| true,
                        )
                        .await?;
                    let Some((mut result, source)) = result else {
                        // The protocol requires one result for every input position;
                        // never surface a shorter, index-shifted response.
                        return Ok(None);
                    };
                    if source == SelectionLayer::Virt {
                        let lower_layers = priorities
                            .iter()
                            .skip_while(|source| **source != LayerSource::Virt)
                            .skip(1);
                        for source in lower_layers {
                            let ancestors = match source {
                                LayerSource::Native if native_enabled => {
                                    let mut receiver = native_rx.clone();
                                    loop {
                                        if let Some(ranges) = receiver.borrow().as_ref() {
                                            break ranges.get(index).cloned();
                                        }
                                        if receiver.changed().await.is_err() {
                                            break None;
                                        }
                                    }
                                }
                                LayerSource::Host => {
                                    host.clone().await?.map(|(selection, _)| selection)
                                }
                                _ => None,
                            };
                            if let Some(ancestors) = ancestors {
                                let (extended, appended) =
                                    append_containing_selection_ancestors(result, ancestors);
                                result = extended;
                                if appended {
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Some(result))
                }
            });
            Ok(futures::future::try_join_all(walks)
                .await?
                .into_iter()
                .collect())
        };
        let layer_race = race_native_producer(native_producer, layer_walk);
        let selected = tokio::select! {
            biased;
            _ = version_cancel.cancelled() => {
                compute_cancel.cancel();
                return Err(crate::error::content_modified_error());
            }
            result = layer_race => result?,
        };

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

        Ok(selected)
    }

    async fn selection_range_host_only(
        &self,
        lsp_uri: &Uri,
        positions: Vec<Position>,
        expected_settings_generation: u64,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let Some(pass) = self
            .selection_range_host_pass(lsp_uri, positions, expected_settings_generation)
            .await?
        else {
            return Ok(None);
        };
        Ok(pass.into_complete())
    }

    async fn selection_range_host_pass(
        &self,
        lsp_uri: &Uri,
        positions: Vec<Position>,
        expected_settings_generation: u64,
    ) -> Result<Option<HostSelectionPass>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let expected_incarnation = ctx.incarnation;
        let expected_version = ctx.content_version;
        let Some(version_cancel) = self.documents.get(&ctx.uri).and_then(|document| {
            (document.incarnation() == expected_incarnation
                && document.content_version() == expected_version)
                .then(|| document.version_cancel_token())
        }) else {
            return Err(crate::error::content_modified_error());
        };
        let uri = ctx.uri.clone();
        drop(ctx);

        let request = async {
            let mut selected = Vec::with_capacity(positions.len());
            for position in positions {
                let host = self.selection_range_host_layer(
                    lsp_uri,
                    position,
                    expected_incarnation,
                    expected_version,
                );
                let selection = self
                    .walk_layer_futures(
                        lsp_uri,
                        METHOD,
                        METHOD,
                        std::future::ready(Ok(None)),
                        host,
                        std::future::ready(Ok(None)),
                        |_| true,
                    )
                    .await?;
                selected.push(selection);
            }
            Ok(selected)
        };
        let selected = tokio::select! {
            biased;
            _ = version_cancel.cancelled() => {
                return Err(crate::error::content_modified_error());
            }
            result = request => result?,
        };
        let still_current = self.documents.get(&uri).is_some_and(|document| {
            document.incarnation() == expected_incarnation
                && document.content_version() == expected_version
        });
        if !still_current || self.cache.semantic_token_generation() != expected_settings_generation
        {
            return Err(crate::error::content_modified_error());
        }
        Ok(Some(HostSelectionPass {
            results: selected,
            incarnation: expected_incarnation,
            content_version: expected_version,
        }))
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
        let position = ctx.position;
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
        let documents = std::sync::Arc::clone(&self.documents);
        let lsp_uri = lsp_uri.clone();
        let result = dispatch_host_preferred(
            &ctx,
            self.bridge.pool_arc(),
            move |task: HostFanOutTask| {
                let documents = std::sync::Arc::clone(&documents);
                let lsp_uri = lsp_uri.clone();
                async move {
                    let Some(position) = normalize_position(&task.text, position) else {
                        return Ok(None);
                    };
                    let params = serde_json::to_value(SelectionRangeParams {
                        text_document: TextDocumentIdentifier { uri: lsp_uri },
                        positions: vec![position],
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    })
                    .unwrap_or(serde_json::Value::Null);
                    let host_uri = task.uri.clone();
                    let revision_text_reader: crate::lsp::bridge::HostTextReader =
                        std::sync::Arc::new(move || {
                            documents.get(&host_uri).and_then(|document| {
                                (document.incarnation() == expected_incarnation
                                    && document.content_version() == expected_version)
                                    .then(|| document.text_arc())
                            })
                        });
                    let raw = task
                        .pool
                        .send_host_raw_request_for_revision(
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
                            revision_text_reader,
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

    #[test]
    fn virtual_selection_chain_appends_strictly_containing_host_ancestors() {
        let virtual_chain = SelectionRange {
            range: tower_lsp_server::ls_types::Range::new(Position::new(2, 4), Position::new(2, 8)),
            parent: Some(Box::new(SelectionRange {
                range: tower_lsp_server::ls_types::Range::new(
                    Position::new(2, 2),
                    Position::new(2, 10),
                ),
                parent: None,
            })),
        };
        let host_chain = SelectionRange {
            range: tower_lsp_server::ls_types::Range::new(Position::new(2, 4), Position::new(2, 8)),
            parent: Some(Box::new(SelectionRange {
                range: tower_lsp_server::ls_types::Range::new(
                    Position::new(1, 0),
                    Position::new(3, 0),
                ),
                parent: Some(Box::new(SelectionRange {
                    range: tower_lsp_server::ls_types::Range::new(
                        Position::new(0, 0),
                        Position::new(4, 0),
                    ),
                    parent: None,
                })),
            })),
        };

        let (merged, appended) = append_containing_selection_ancestors(virtual_chain, host_chain);
        assert!(appended);
        let virtual_outer = merged.parent.expect("virtual outer range");
        let host_outer = virtual_outer.parent.expect("containing host range");
        assert_eq!(host_outer.range.start, Position::new(1, 0));
        assert!(host_outer.parent.is_some());
    }

    #[test]
    fn non_containing_chain_does_not_stop_lower_layer_search() {
        let virtual_chain = SelectionRange {
            range: tower_lsp_server::ls_types::Range::new(Position::new(2, 2), Position::new(2, 8)),
            parent: None,
        };
        let bounded_host_chain = SelectionRange {
            range: tower_lsp_server::ls_types::Range::new(Position::new(2, 3), Position::new(2, 7)),
            parent: None,
        };

        let (unchanged, appended) =
            append_containing_selection_ancestors(virtual_chain.clone(), bounded_host_chain);

        assert!(!appended);
        assert_eq!(unchanged, virtual_chain);
    }

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
    async fn higher_priority_layer_drops_a_started_native_producer() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer_started = Arc::clone(&started);
        let producer_dropped = Arc::clone(&dropped);
        let native = async move {
            let _drop_flag = DropFlag(producer_dropped);
            producer_started.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        };
        let layer = async {
            started.notified().await;
            Ok(Some(7_u8))
        };

        assert_eq!(race_native_producer(native, layer).await.unwrap(), Some(7));
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "a winning higher-priority layer must cancel its native competitor"
        );
    }

    #[tokio::test]
    async fn empty_native_producer_still_waits_for_a_bridge_layer() {
        let native = std::future::ready(Ok(()));
        let layer = async {
            tokio::task::yield_now().await;
            Ok(Some(7_u8))
        };

        assert_eq!(race_native_producer(native, layer).await.unwrap(), Some(7));
    }

    #[tokio::test]
    async fn host_layer_walk_survives_an_unavailable_parser() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = url::Url::parse("file:///test/no_parser.hostonly").unwrap();
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).unwrap();
        server.documents.insert(
            uri,
            "word\n".to_string(),
            Some("hostonly".to_string()),
            None,
        );
        assert!(!server.language.has_parser_available("hostonly"));

        let selected = server
            .walk_layer_futures(
                &lsp_uri,
                METHOD,
                METHOD,
                std::future::ready(Ok(None)),
                std::future::ready(Ok(Some(7_u8))),
                std::future::ready(Ok(None)),
                |_| true,
            )
            .await
            .unwrap();
        assert_eq!(selected, Some(7));
    }

    #[tokio::test]
    async fn explicit_host_language_keeps_path_detected_native_selection() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let uri = url::Url::parse("file:///test/host-routed-native.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() { value }\n".to_string(),
            Some("hostonly".to_string()),
            None,
        );
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar");
        let text = "fn main() { value }\n";
        let tree = parser.parse(text, None).expect("rust tree");
        let incarnation = server
            .documents
            .latest_snapshot(&uri)
            .expect("document must be open")
            .slot
            .current_incarnation;
        let published = server.documents.get(&uri).is_some_and(|document| {
            document.publish_snapshot(std::sync::Arc::new(
                crate::document::snapshot::ParseSnapshot {
                    text: std::sync::Arc::from(text),
                    tree: Some(tree),
                    language: Some("rust".to_string()),
                    parsed_version: 0,
                    incarnation,
                    injection_regions: None,
                    bridge_regions: None,
                    resolved_regions: None,
                    layer_trees: std::sync::OnceLock::new(),
                },
            ))
        });
        assert!(published);
        assert_eq!(server.document_language(&uri).as_deref(), Some("rust"));
        assert_eq!(
            server.document_bridge_language(&uri).as_deref(),
            Some("hostonly")
        );

        let result = server
            .selection_range_impl(SelectionRangeParams {
                text_document: TextDocumentIdentifier {
                    uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("LSP URI"),
                },
                positions: vec![Position::new(0, 14)],
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("selectionRange should succeed");
        assert!(
            result.is_some_and(|ranges| ranges.len() == 1),
            "native selection must remain available through the path-detected rust parser"
        );
    }

    #[test]
    fn cached_host_pass_is_bound_to_its_snapshot_lineage() {
        let pass = HostSelectionPass {
            results: Vec::new(),
            incarnation: 4,
            content_version: 9,
        };

        assert!(pass.matches_snapshot(4, 9));
        assert!(!pass.matches_snapshot(4, 10));
        assert!(!pass.matches_snapshot(5, 9));
    }

    #[test]
    fn partial_host_pass_cannot_form_a_protocol_response() {
        let selection = SelectionRange {
            range: tower_lsp_server::ls_types::Range::new(Position::new(0, 0), Position::new(0, 1)),
            parent: None,
        };
        let pass = HostSelectionPass {
            results: vec![Some(selection), None],
            incarnation: 1,
            content_version: 1,
        };

        assert!(pass.into_complete().is_none());
    }

    #[test]
    fn stale_snapshot_keeps_content_modified_when_host_fallback_is_empty() {
        let error = settle_timed_out_selection(None, true)
            .expect_err("a stale snapshot must remain retryable when the host has no result");
        assert_eq!(
            error.code,
            tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32801)
        );
        assert!(settle_timed_out_selection(None, false).unwrap().is_none());
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

    #[test]
    fn host_selection_range_validates_against_the_defaulted_client_position() {
        let text = "abc\n";
        let position = normalize_position(text, Position::new(0, 999)).expect("same-line clamp");
        assert_eq!(position, Position::new(0, 3));
        let value = json!([{
            "range": {
                "start": { "line": 0, "character": 3 },
                "end": { "line": 0, "character": 3 }
            }
        }]);

        assert!(parse_single_host_selection_range(value, position, text).is_some());
    }

    #[test]
    fn selection_position_past_eof_defaults_to_document_end() {
        assert_eq!(
            normalize_position("a\nb", Position::new(99, 99)),
            Some(Position::new(1, 1))
        );
    }

    #[test]
    fn host_selection_range_accepts_a_nonempty_range_ending_at_requested_eof() {
        let text = "abc";
        let position = Position::new(0, 3);
        let value = json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 3 }
            }
        }]);

        assert!(parse_single_host_selection_range(value, position, text).is_some());
    }
}
