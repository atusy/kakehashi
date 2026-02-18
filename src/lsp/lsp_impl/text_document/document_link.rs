//! Document link method for Kakehashi.

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentLink, DocumentLinkParams, MessageType};

use crate::config::settings::BridgeServerConfig;
use crate::language::InjectionResolver;
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::bridge::UpstreamId;

use super::super::{Kakehashi, uri_to_url};
use crate::lsp::aggregation::fan_in::first_win::{self, FirstWinResult};

impl Kakehashi {
    pub(crate) async fn document_link_impl(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in documentLink: {}", lsp_uri.as_str());
            return Ok(None);
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("documentLink called for {}", uri),
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
            return Ok(None);
        }
        let snapshot = snapshot.expect("snapshot set when missing_message is None");

        // Get the language for this document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            log::debug!(target: "kakehashi::document_link", "No language detected");
            return Ok(None);
        };

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.get_injection_query(&language_name) else {
            return Ok(None);
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
            return Ok(None);
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = super::super::bridge_context::current_upstream_id();

        // Subscribe to cancel notifications so we can abort early on $/cancelRequest.
        // _cancel_guard ensures automatic unsubscribe when this scope exits.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<Option<Vec<DocumentLink>>> = JoinSet::new();

        for resolved in all_regions {
            // Get ALL bridge server configs for this injection language
            let configs = self
                .get_all_bridge_configs_for_language(&language_name, &resolved.injection_language);
            if configs.is_empty() {
                continue;
            }

            // Move owned fields into the spawned task (Arc/Url still need clone)
            let pool = Arc::clone(&pool);
            let uri = uri.clone();
            let upstream_id = upstream_request_id.clone();
            let injection_language = resolved.injection_language;
            let region_id = resolved.region.region_id;
            let region_start_line = resolved.region.line_range.start;
            let virtual_content = resolved.virtual_content;

            outer_join_set.spawn(async move {
                race_servers_for_region(
                    pool,
                    configs,
                    uri,
                    injection_language,
                    region_id,
                    region_start_line,
                    virtual_content,
                    upstream_id,
                )
                .await
            });
        }

        // Collect results, aborting early if $/cancelRequest arrives.
        let all_links = crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_rx,
            |acc, opt: Option<Vec<DocumentLink>>| {
                if let Some(items) = opt {
                    acc.extend(items);
                }
            },
        )
        .await;

        // Clean up stale upstream registry entries left by aborted inner tasks.
        // This MUST run on both success and cancel paths — do NOT use `?` above,
        // or the cancel Err would propagate early and skip this cleanup.
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        let all_links = all_links?;
        Ok(if all_links.is_empty() {
            None
        } else {
            Some(all_links)
        })
    }
}

/// Race all capable servers for a single injection region, returning the first
/// non-empty document link response.
///
/// Uses `first_win()` to take the first server that returns a non-empty result,
/// aborting the remaining in-flight requests.
#[allow(clippy::too_many_arguments)]
async fn race_servers_for_region(
    pool: Arc<LanguageServerPool>,
    configs: Vec<crate::lsp::bridge::ResolvedServerConfig>,
    uri: url::Url,
    injection_language: String,
    region_id: String,
    region_start_line: u32,
    virtual_content: String,
    upstream_id: Option<UpstreamId>,
) -> Option<Vec<DocumentLink>> {
    let mut join_set: JoinSet<std::io::Result<Option<Vec<DocumentLink>>>> = JoinSet::new();

    for config in configs {
        let pool: Arc<LanguageServerPool> = Arc::clone(&pool);
        let uri = uri.clone();
        let injection_language = injection_language.clone();
        let region_id = region_id.clone();
        let virtual_content = virtual_content.clone();
        let upstream_id = upstream_id.clone();
        let server_name = config.server_name.clone();
        let server_config: Arc<BridgeServerConfig> = config.config;

        join_set.spawn(async move {
            pool.send_document_link_request(
                &server_name,
                &server_config,
                &uri,
                &injection_language,
                &region_id,
                region_start_line,
                &virtual_content,
                upstream_id,
            )
            .await
        });
    }

    // First non-empty response wins; no cancel support at inner level
    let result = first_win::first_win(
        &mut join_set,
        |opt| matches!(opt, Some(v) if !v.is_empty()),
        None,
    )
    .await;

    match result {
        FirstWinResult::Winner(links) => links,
        FirstWinResult::NoWinner { .. } | FirstWinResult::Cancelled => None,
    }
}
