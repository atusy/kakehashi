//! Document color method for Kakehashi.

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorInformation, DocumentColorParams, MessageType};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_first_win;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn document_color_impl(
        &self,
        params: DocumentColorParams,
    ) -> Result<Vec<ColorInformation>> {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in documentColor: {}", lsp_uri.as_str());
            return Ok(vec![]);
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("documentColor called for {}", uri),
            )
            .await;

        // Get document snapshot (minimizes lock duration)
        let (snapshot, missing_message) = match self.documents.get(&uri) {
            None => (None, Some("No document found")),
            Some(doc) => match doc.snapshot() {
                None => (None, Some("Document not fully initialized")),
                Some(snapshot) => (Some(snapshot), None),
            },
            // doc automatically dropped here, lock released
        };
        if let Some(message) = missing_message {
            self.client.log_message(MessageType::INFO, message).await;
            return Ok(Vec::new());
        }
        let snapshot = snapshot.expect("snapshot set when missing_message is None");

        // Get the language for this document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            log::debug!(target: "kakehashi::document_color", "No language detected");
            return Ok(Vec::new());
        };

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.get_injection_query(&language_name) else {
            return Ok(Vec::new());
        };

        // Collect all injection regions
        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.region_id_tracker(),
            &uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Ok(Vec::new());
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = super::super::bridge_context::current_upstream_id();

        // Subscribe to cancel notifications so we can abort early on $/cancelRequest.
        // _cancel_guard ensures automatic unsubscribe when this scope exits.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<Vec<ColorInformation>> = JoinSet::new();

        for resolved in all_regions {
            // Get ALL bridge server configs for this injection language
            let configs = self
                .get_all_bridge_configs_for_language(&language_name, &resolved.injection_language);
            if configs.is_empty() {
                continue;
            }

            let region_ctx = DocumentRequestContext {
                uri: uri.clone(),
                resolved,
                configs,
                upstream_request_id: upstream_request_id.clone(),
            };
            let pool = Arc::clone(&pool);

            outer_join_set.spawn(async move {
                let result = dispatch_first_win(
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
                                t.region_start_line,
                                &t.virtual_content,
                                t.upstream_id,
                            )
                            .await
                    },
                    |colors| !colors.is_empty(),
                    None,
                )
                .await;
                pool.unregister_all_for_upstream_id(region_ctx.upstream_request_id.as_ref());
                match result {
                    FanInResult::Done(colors) => colors,
                    FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
                }
            });
        }

        // Collect results, aborting early if $/cancelRequest arrives.
        crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_rx,
            |acc, items: Vec<ColorInformation>| acc.extend(items),
        )
        .await
    }
}
