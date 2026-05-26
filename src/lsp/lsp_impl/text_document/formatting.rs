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

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentFormattingParams, TextEdit};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

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
                    None,
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
            cancel_rx,
            |acc, opt: Option<Vec<TextEdit>>| {
                if let Some(items) = opt {
                    acc.extend(items);
                }
            },
        )
        .await;

        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        let all_edits = all_edits?;
        Ok(if all_edits.is_empty() {
            None
        } else {
            Some(all_edits)
        })
    }
}
