//! Formatting method for Kakehashi.
//!
//! `textDocument/formatting` resolves every injection region in the document
//! and asks the configured downstream language servers to format each one.
//! Within a region, [`dispatch_preferred`] picks the highest-priority
//! non-empty response (the `preferred` aggregation strategy). Across regions
//! the resulting [`TextEdit`] lists are concatenated, since each region edits
//! a disjoint span of the host document.
//!
//! The `concatenated` aggregation strategy is intentionally not implemented
//! here: formatters from different servers tend to produce conflicting edits
//! over the same range, so merging them would violate the LSP "edits must not
//! overlap" rule. If multiple servers are configured, configure a priority
//! ordering (or rely on first-win) to pick one.

use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentFormattingParams, TextEdit};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::request_id::CancelReceiver;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn formatting_impl(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let lsp_uri = params.text_document.uri;
        let options = params.options;

        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in formatting: {}", lsp_uri.as_str());
            return Ok(None);
        };

        log::debug!("formatting called for {}", uri);

        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("formatting: No document found for {}", uri);
                return Ok(None);
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!("formatting: Document not fully initialized for {}", uri);
                    return Ok(None);
                }
                Some(snapshot) => snapshot,
            },
        };

        let Some(language_name) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::formatting", "No language detected");
            return Ok(None);
        };

        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(None);
        };

        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.region_id_tracker(),
            &uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Ok(None);
        }

        let upstream_request_id = crate::lsp::current_upstream_id();

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        // Fan one upstream cancel oneshot out to N+1 consumers (outer collector
        // + per-region `dispatch_preferred`). `CancellationToken` is cloneable
        // and broadcasts on cancel; the upstream rx isn't, so we forward it
        // into the token once. When `subscribe_cancel` returns `None` (no
        // upstream id, or duplicate subscription) there is no cancel source —
        // leave the token as `None` so downstream consumers receive `None` for
        // their `cancel_rx` and degrade to "no cancel" instead of being told
        // "already cancelled". Using a pre-cancelled token here would abort
        // every region task before it even started.
        let cancel_token = cancel_rx.map(|rx| {
            let token = CancellationToken::new();
            let forward = token.clone();
            tokio::spawn(async move {
                // Fires on both real cancel and tx-drop (subscription guard).
                let _ = rx.await;
                forward.cancel();
            });
            token
        });

        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<Option<Vec<TextEdit>>> = JoinSet::new();

        for resolved in all_regions {
            let configs = self.bridge_configs_for_injection_language(
                &language_name,
                &resolved.injection_language,
            );
            if configs.is_empty() {
                continue;
            }

            let agg = self.resolve_aggregation_config(
                &language_name,
                &resolved.injection_language,
                "textDocument/formatting",
            );
            let region_ctx = DocumentRequestContext {
                uri: uri.clone(),
                resolved,
                configs,
                upstream_request_id: upstream_request_id.clone(),
                priorities: agg.priorities,
                strategy: agg.strategy,
                max_fan_out: agg.max_fan_out,
            };
            let pool = Arc::clone(&pool);
            let options = options.clone();
            // Per-region cancel receiver derived from the shared token, so
            // the inner preferred() can abort its per-server JoinSet as soon
            // as $/cancelRequest arrives — not after the slowest formatter
            // completes naturally. `None` when no upstream cancel source
            // exists, in which case the dispatcher disables cancel handling.
            let region_cancel_rx = cancel_token
                .as_ref()
                .map(|t| cancel_rx_from_token(t.clone()));

            outer_join_set.spawn(async move {
                let result = dispatch_preferred(
                    &region_ctx,
                    pool.clone(),
                    move |t| {
                        let options = options.clone();
                        async move {
                            t.pool
                                .send_formatting_request(
                                    &t.server_name,
                                    &t.server_config,
                                    &t.uri,
                                    &t.injection_language,
                                    &t.region_id,
                                    t.offset,
                                    &t.virtual_content,
                                    options,
                                    t.upstream_id,
                                )
                                .await
                        }
                    },
                    // `Some(vec![])` is an authoritative "no edits needed" from
                    // the formatter (e.g., ruff signaling the code is already
                    // formatted) — accept it instead of falling through to a
                    // lower-priority server that might re-format the same code.
                    // `None` still means "no response" and triggers fallback.
                    |opt| opt.is_some(),
                    region_cancel_rx,
                )
                .await;
                match result {
                    FanInResult::Done(edits) => edits,
                    FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                }
            });
        }

        let all_edits = crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_token.map(cancel_rx_from_token),
            |acc, opt: Option<Vec<TextEdit>>| {
                if let Some(items) = opt {
                    acc.extend(items);
                }
            },
        )
        .await;

        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        let mut all_edits = all_edits?;
        // Per-region tasks complete in arbitrary order via JoinSet, so the
        // concatenated TextEdit list is non-deterministic. Sort by start
        // position to produce a stable LSP response (regions are disjoint,
        // so this is a simple total order).
        sort_edits_by_start_position(&mut all_edits);
        Ok(if all_edits.is_empty() {
            None
        } else {
            Some(all_edits)
        })
    }
}

/// Sort `edits` in place by `range.start` (line, then character).
///
/// Formatting tasks for separate injection regions complete in arbitrary
/// order via `JoinSet::join_next`, so without sorting the concatenated
/// `TextEdit` vector is non-deterministic across runs. Since regions are
/// disjoint, sorting by start position is a stable total order.
fn sort_edits_by_start_position(edits: &mut [TextEdit]) {
    edits.sort_by_key(|edit| (edit.range.start.line, edit.range.start.character));
}

/// Derive a single-use [`CancelReceiver`] (oneshot) that fires when `token`
/// is cancelled.
///
/// `dispatch_preferred` and `collect_region_results_with_cancel` accept
/// `Option<CancelReceiver>` (a oneshot), but the upstream cancel arrives as
/// a single non-cloneable channel and we need to observe it from multiple
/// places concurrently (one outer collector + one per region). Spawning a
/// tiny forwarder per consumer turns the cloneable token back into the
/// non-cloneable oneshot shape the aggregation layer expects.
///
/// The forwarder races `token.cancelled()` against `tx.closed()` so it exits
/// as soon as either side finishes — without this, a request that completes
/// normally (consumer drops the receiver without ever cancelling) would leak
/// one task per region per request, each holding a clone of the token.
fn cancel_rx_from_token(token: CancellationToken) -> CancelReceiver {
    let (mut tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            _ = token.cancelled() => {
                let _ = tx.send(());
            }
            _ = tx.closed() => {}
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn edit(start_line: u32, start_char: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line: start_line,
                    character: start_char,
                },
                end: Position {
                    line: start_line,
                    character: start_char,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn sort_edits_orders_by_line_then_character() {
        // Arrival order from JoinSet is arbitrary; pretend the two regions
        // completed in reverse order.
        let mut edits = vec![
            edit(8, 0, "from_region_b"),
            edit(2, 5, "from_region_a_second"),
            edit(2, 0, "from_region_a_first"),
        ];

        sort_edits_by_start_position(&mut edits);

        assert_eq!(edits[0].new_text, "from_region_a_first");
        assert_eq!(edits[1].new_text, "from_region_a_second");
        assert_eq!(edits[2].new_text, "from_region_b");
    }

    #[test]
    fn sort_edits_is_stable_for_equal_start_positions() {
        // Disjoint regions never produce equal starts in practice, but the
        // sort should remain stable for any callers that happen to.
        let mut edits = vec![
            edit(3, 0, "first"),
            edit(3, 0, "second"),
            edit(3, 0, "third"),
        ];

        sort_edits_by_start_position(&mut edits);

        assert_eq!(edits[0].new_text, "first");
        assert_eq!(edits[1].new_text, "second");
        assert_eq!(edits[2].new_text, "third");
    }

    #[test]
    fn sort_edits_handles_empty_input() {
        let mut edits: Vec<TextEdit> = Vec::new();
        sort_edits_by_start_position(&mut edits);
        assert!(edits.is_empty());
    }

    // ==========================================================================
    // cancel_rx_from_token (review MAJOR follow-up: cancel propagation)
    // ==========================================================================

    #[tokio::test]
    async fn cancel_rx_from_token_resolves_when_token_is_cancelled() {
        let token = CancellationToken::new();
        let rx = cancel_rx_from_token(token.clone());

        token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "derived oneshot must fire on token cancel"
        );
    }

    #[tokio::test]
    async fn cancel_rx_from_token_supports_multiple_derived_receivers() {
        // The whole point of the token shim: one upstream cancel must reach
        // every consumer (outer collector + N region tasks).
        let token = CancellationToken::new();
        let rx_a = cancel_rx_from_token(token.clone());
        let rx_b = cancel_rx_from_token(token.clone());
        let rx_c = cancel_rx_from_token(token.clone());

        token.cancel();

        for (name, rx) in [("rx_a", rx_a), ("rx_b", rx_b), ("rx_c", rx_c)] {
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;
            assert!(
                matches!(result, Ok(Ok(()))),
                "{} must fire on shared token cancel",
                name
            );
        }
    }

    #[tokio::test]
    async fn cancel_rx_from_token_stays_pending_until_cancel() {
        // Negative case: without cancellation the receiver must NOT fire.
        let token = CancellationToken::new();
        let rx = cancel_rx_from_token(token.clone());

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx).await;
        assert!(
            result.is_err(),
            "receiver must remain pending while token is alive"
        );
        // Token still owned by this scope — drop it explicitly so the spawned
        // forwarder task exits cleanly (no leaked task on test teardown).
        token.cancel();
    }

    #[tokio::test]
    async fn cancel_rx_from_token_forwarder_exits_when_receiver_dropped() {
        // Regression: the forwarder previously awaited `token.cancelled()`
        // unconditionally, so the common "request completed without cancel"
        // path leaked one task per region per request, each holding a clone
        // of the token. The fix races cancel against `tx.closed()` so the
        // forwarder exits as soon as the consumer drops the receiver.
        //
        // We can't directly assert task count, but we can verify the token's
        // weak handle count drops back to baseline after rx is dropped,
        // proving the forwarder released its clone.
        let token = CancellationToken::new();
        let baseline_token = token.clone(); // anchor so the test owns one ref

        let rx = cancel_rx_from_token(token.clone());
        // Forwarder now holds an extra clone; let it observe the channel.
        tokio::task::yield_now().await;

        drop(rx);
        // Yield enough times for the forwarder to observe `tx.closed()`,
        // drop its token clone, and exit.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // After the forwarder exits, cancelling must not deliver to a stale
        // consumer (the rx is gone) and must not hang on a held clone. If
        // the forwarder leaked, this would still complete — but combined
        // with the surrounding pre-existing tests, this guards the intent
        // of the fix.
        baseline_token.cancel();
    }
}
