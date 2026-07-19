//! Document color method for Kakehashi.

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorInformation, DocumentColorParams};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn document_color_impl(
        &self,
        params: DocumentColorParams,
    ) -> Result<Vec<ColorInformation>> {
        // Experimental (KAKEHASHI_EXPERIMENTAL=true): without the opt-in the
        // capability is not advertised, so answer a compliant empty result to
        // any client that calls regardless.
        if !self.experimental_enabled() {
            return Ok(Vec::new());
        }
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in documentColor: {}", lsp_uri.as_str());
            return Ok(vec![]);
        };

        log::debug!("documentColor called for {}", uri);

        // Ensure a fresh tree before snapshotting: `didChange` clears the tree and
        // reparses off-ingress, so without this documentColor (virt-only) returns
        // empty for the whole reparse window after every edit.
        self.ensure_document_parsed(&uri).await;

        // Get document snapshot (minimizes lock duration)
        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("documentColor: No document found for {}", uri);
                return Ok(Vec::new());
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!("documentColor: Document not fully initialized for {}", uri);
                    return Ok(Vec::new());
                }
                Some(snapshot) => snapshot,
            },
            // doc automatically dropped here, lock released
        };

        // Get the language for this document
        let Some(language_name) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::document_color", "No language detected");
            return Ok(Vec::new());
        };

        if !self.virt_layer_enabled(&language_name, "textDocument/documentColor") {
            log::debug!(
                target: "kakehashi::document_color",
                "virt layer disabled for {} via layers.aggregation priorities",
                language_name
            );
            return Ok(Vec::new());
        }

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(Vec::new());
        };

        // Collect all injection regions
        let all_regions = match self
            .documents
            .current_resolved_regions(&uri, self.cache.semantic_token_generation())
        {
            Some(regions) => regions,
            None => std::sync::Arc::new(InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                &uri,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
                snapshot.incarnation(),
            )),
        };

        // Compare the fused worker derivation only for a reader that already
        // consumes every injection. Issuing this RPC from the parse hot path
        // can overtake queued edits on the document lane and makes the shadow
        // observation causally stale under a typing burst.
        if let Some(view) = self.documents.latest_snapshot(&uri)
            && view.slot.current_incarnation == snapshot.incarnation()
            && view.slot.snapshot.as_ref().is_some_and(|current| {
                current.incarnation == snapshot.incarnation()
                    && current.parsed_version == view.content_version
            })
            && let Some(worker_regions) = self
                .tree_worker_shadow
                .injection_regions(
                    &uri,
                    snapshot.incarnation(),
                    view.content_version,
                    self.cache.semantic_token_generation(),
                )
                .await
        {
            self.tree_worker_shadow.compare_injection_regions(
                &uri,
                view.content_version,
                &all_regions,
                &worker_regions,
            );
        }

        if all_regions.is_empty() {
            return Ok(Vec::new());
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = crate::lsp::current_upstream_id();

        // Subscribe to cancel notifications so we can abort early on $/cancelRequest.
        // _cancel_guard ensures automatic unsubscribe when this scope exits.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<(usize, Option<Vec<ColorInformation>>)> = JoinSet::new();

        // Drop servers already known (a live, `Ready` connection) NOT to support
        // this method before the per-region fan-out spawns their tasks
        // (capability-prefilter-fanout). One pool query for the whole request.
        let incapable_servers = self
            .incapable_virt_servers(
                &language_name,
                all_regions.iter().map(|r| r.injection_language.as_str()),
                "textDocument/documentColor",
            )
            .await;

        for (region_index, resolved) in all_regions.iter().enumerate() {
            // Get ALL bridge server configs for this injection language
            let mut configs = self.bridge_configs_for_injection_language(
                &language_name,
                &resolved.injection_language,
            );
            if !incapable_servers.is_empty() {
                configs.retain(|c| !incapable_servers.contains(&c.server_name));
            }
            if configs.is_empty() {
                continue;
            }

            let agg = self.resolve_aggregation_config(
                &language_name,
                &resolved.injection_language,
                "textDocument/documentColor",
            );
            let region_ctx = DocumentRequestContext {
                uri: uri.clone(),
                resolved: resolved.clone(),
                configs,
                upstream_request_id: upstream_request_id.clone(),
                priorities: agg.priorities,
                strategy: agg.strategy,
                max_fan_out: agg.max_fan_out,
                client_progress_token: None,
            };
            let pool = Arc::clone(&pool);

            outer_join_set.spawn(async move {
                let result = dispatch_preferred(
                    &region_ctx,
                    pool.clone(),
                    |t| async move {
                        t.pool
                            .send_document_color_request(
                                &t.server_name,
                                &t.server_config,
                                &t.uri,
                                &t.injection_language,
                                &t.region_id,
                                t.offset,
                                &t.virtual_content,
                                t.upstream_id,
                            )
                            .await
                    },
                    |colors| !colors.is_empty(),
                    None,
                )
                .await;
                let colors = match result {
                    FanInResult::Done(colors) => Some(colors),
                    FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                };
                (region_index, colors)
            });
        }

        // RAII sweep of stale upstream registry entries: fires after the
        // collection settles or the whole request future is dropped — after
        // the JoinSet is drained/dropped, so cancel forwarding remains intact
        // for all in-flight downstream requests. Nothing else shares the id
        // (virt-only handler, no layer race, no outer sweep).
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            pool.clone(),
            upstream_request_id.clone(),
        );
        // Collect results, aborting early if $/cancelRequest arrives.
        let completion_order_items =
            crate::lsp::aggregation::region::collect_region_results_with_cancel(
                outer_join_set,
                cancel_rx,
                |acc, region_items: (usize, Option<Vec<ColorInformation>>)| acc.push(region_items),
            )
            .await?;
        Ok(
            crate::lsp::lsp_impl::whole_document::flatten_ordered_region_items(
                completion_order_items,
            ),
        )
    }
}
