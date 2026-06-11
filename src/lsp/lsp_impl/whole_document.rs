//! Shared fan-out for whole-document bridged requests.
//!
//! documentLink, foldingRange, and codeLens all follow the same shape: no
//! position parameter, so the request fans out to *every* injection region,
//! uses the preferred strategy within each region, and concatenates the
//! per-region results. This module hosts that shape once; the per-method
//! handlers supply only the LSP method name and the downstream send call.

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::Uri;

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{FanInResult, FanOutTask, dispatch_preferred};

use super::bridge_context::DocumentRequestContext;
use super::{Kakehashi, uri_to_url};

impl Kakehashi {
    /// Fan out a whole-document bridged request to all injection regions.
    ///
    /// Within a region the preferred strategy picks one server's result;
    /// across regions results are concatenated (regions are disjoint, so this
    /// is safe for any item kind). Returns `None` when the document has no
    /// injection regions, no configured servers, or every region came back
    /// empty — mirroring the per-method handlers this was extracted from.
    pub(super) async fn whole_document_preferred_fan_out<T, F, Fut>(
        &self,
        lsp_uri: &Uri,
        method_name: &'static str,
        send: F,
    ) -> Result<Option<Vec<T>>>
    where
        T: Send + 'static,
        F: Fn(FanOutTask) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = io::Result<Option<Vec<T>>>> + Send + 'static,
    {
        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!("Invalid URI in {}: {}", method_name, lsp_uri.as_str());
            return Ok(None);
        };

        log::debug!("{} called for {}", method_name, uri);

        // Tower-LSP runs requests concurrently, so a whole-document request can
        // arrive before didOpen/didChange has finished parsing. Wait briefly
        // for any in-flight parse to land a tree before snapshotting, matching
        // the read-handler pattern (semantic tokens, rangeFormatting, node
        // lookups); otherwise an otherwise-valid request would degrade to
        // `Ok(None)` purely due to a parse race.
        self.documents
            .wait_for_parse_completion(&uri, std::time::Duration::from_millis(200))
            .await;

        // Get document snapshot (minimizes lock duration)
        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("{}: No document found for {}", method_name, uri);
                return Ok(None);
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!(
                        "{}: Document not fully initialized for {}",
                        method_name,
                        uri
                    );
                    return Ok(None);
                }
                Some(snapshot) => snapshot,
            },
            // doc automatically dropped here, lock released
        };

        // Get the language for this document
        let Some(language_name) = self.document_language(&uri) else {
            log::debug!("{}: No language detected", method_name);
            return Ok(None);
        };

        if !self.virt_layer_enabled(&language_name, method_name) {
            log::debug!(
                "{}: virt layer disabled for {} via layers.order",
                method_name,
                language_name
            );
            return Ok(None);
        }

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(None);
        };

        // Collect all injection regions
        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.node_tracker(),
            &uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Ok(None);
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = crate::lsp::current_upstream_id();

        // Subscribe to cancel notifications so we can abort early on $/cancelRequest.
        // _cancel_guard ensures automatic unsubscribe when this scope exits.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<Option<Vec<T>>> = JoinSet::new();

        for resolved in all_regions {
            // Get ALL bridge server configs for this injection language
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
                method_name,
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
            let send = send.clone();

            outer_join_set.spawn(async move {
                let result = dispatch_preferred(
                    &region_ctx,
                    pool.clone(),
                    send,
                    |opt| matches!(opt, Some(v) if !v.is_empty()),
                    None,
                )
                .await;
                match result {
                    FanInResult::Done(items) => items,
                    FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                }
            });
        }

        // Collect results, aborting early if $/cancelRequest arrives.
        let all_items = crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_rx,
            |acc, opt: Option<Vec<T>>| {
                if let Some(items) = opt {
                    acc.extend(items);
                }
            },
        )
        .await;

        // Clean up stale upstream registry entries once all region tasks have completed
        // (or been aborted via JoinSet drop). Must happen after the JoinSet is drained
        // so cancel forwarding remains intact for all in-flight downstream requests.
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        let all_items = all_items?;
        Ok(if all_items.is_empty() {
            None
        } else {
            Some(all_items)
        })
    }
}
