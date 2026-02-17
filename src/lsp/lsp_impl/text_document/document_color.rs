//! Document color method for Kakehashi.

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorInformation, DocumentColorParams, MessageType};

use crate::config::settings::BridgeServerConfig;
use crate::language::InjectionResolver;
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::bridge::UpstreamId;

use super::super::{Kakehashi, uri_to_url};
use super::first_win;

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
        let result = collect_colors_with_cancel(outer_join_set, cancel_rx).await;

        // Clean up stale upstream registry entries left by aborted inner tasks.
        // This MUST run on both success and cancel paths — do NOT use `?` above,
        // or the cancel Err would propagate early and skip this cleanup.
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        result
    }
}

/// Collect document colors from all regions, aborting immediately if cancelled.
///
/// Uses `tokio::select!` with biased mode to prioritize cancel handling.
/// When cancelled:
/// - Returns `RequestCancelled` error immediately
/// - Drops the JoinSet, which aborts all spawned outer tasks (cascading to inner tasks)
///
/// When all regions complete:
/// - Returns aggregated colors from all successful regions, or empty Vec if no colors
///
/// If `cancel_rx` is `None`, cancel handling is disabled (graceful degradation
/// when subscription failed due to `AlreadySubscribedError`).
async fn collect_colors_with_cancel(
    mut join_set: JoinSet<Vec<ColorInformation>>,
    cancel_rx: Option<crate::lsp::request_id::CancelReceiver>,
) -> Result<Vec<ColorInformation>> {
    let mut all_colors: Vec<ColorInformation> = Vec::new();

    // Handle None case: no cancel support, just collect results
    let Some(cancel_rx) = cancel_rx else {
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(colors) => all_colors.extend(colors),
                Err(join_err) => {
                    log::warn!("document_color region task panicked: {join_err}");
                }
            }
        }
        return Ok(all_colors);
    };

    // Pin the cancel receiver for use in select!
    tokio::pin!(cancel_rx);

    loop {
        tokio::select! {
            // Biased: check cancel first to ensure immediate abort on cancellation
            biased;

            // Cancel notification received - abort immediately
            _ = &mut cancel_rx => {
                log::debug!(
                    target: "kakehashi::document_color",
                    "documentColor request cancelled, aborting {} remaining tasks",
                    join_set.len()
                );
                // JoinSet dropped here, aborting all spawned tasks
                return Err(tower_lsp_server::jsonrpc::Error::request_cancelled());
            }

            // Next task completed - collect result
            result = join_set.join_next() => {
                match result {
                    Some(Ok(colors)) => {
                        all_colors.extend(colors);
                    }
                    Some(Err(join_err)) => {
                        log::warn!("document_color region task panicked: {join_err}");
                    }
                    None => {
                        // All tasks completed - return aggregated results
                        break;
                    }
                }
            }
        }
    }

    Ok(all_colors)
}

/// Race all capable servers for a single injection region, returning the first
/// non-empty document color response.
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
) -> Vec<ColorInformation> {
    let mut join_set: JoinSet<std::io::Result<Vec<ColorInformation>>> = JoinSet::new();

    for config in configs {
        let pool = Arc::clone(&pool);
        let uri = uri.clone();
        let injection_language = injection_language.clone();
        let region_id = region_id.clone();
        let virtual_content = virtual_content.clone();
        let upstream_id = upstream_id.clone();
        let server_name = config.server_name.clone();
        let server_config: Arc<BridgeServerConfig> = config.config;

        join_set.spawn(async move {
            pool.send_document_color_request(
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
    let result = first_win::first_win(&mut join_set, |colors| !colors.is_empty(), None).await;

    match result {
        first_win::FirstWinResult::Winner(colors) => colors,
        first_win::FirstWinResult::NoWinner { .. } | first_win::FirstWinResult::Cancelled => {
            Vec::new()
        }
    }
}
