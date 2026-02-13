//! Inlay hint method for Kakehashi.

use std::sync::Arc;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams, MessageType};

use super::super::Kakehashi;
use super::first_win;

impl Kakehashi {
    pub(crate) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        // Use range.start position to find the injection region
        // Note: This is a simplification - for range spanning multiple regions,
        // we'd need to aggregate results from all regions. For now, we use start position.
        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, range.start, "inlay_hint")
            .await
        else {
            return Ok(None);
        };

        // Fan-out inlay hint requests to all matching servers
        let pool = self.bridge.pool_arc();
        let mut join_set = tokio::task::JoinSet::new();
        let region_start_line = ctx.resolved.region.line_range.start;

        for config in ctx.configs {
            let pool = Arc::clone(&pool);
            let uri = ctx.uri.clone();
            let injection_language = ctx.resolved.injection_language.clone();
            let region_id = ctx.resolved.region.region_id.clone();
            let virtual_content = ctx.resolved.virtual_content.clone();
            let upstream_id = ctx.upstream_request_id.clone();
            let server_name = config.server_name.clone();
            let server_config = Arc::new(config.config);

            join_set.spawn(async move {
                pool.send_inlay_hint_request(
                    &server_name,
                    &server_config,
                    &uri,
                    range,
                    &injection_language,
                    &region_id,
                    region_start_line,
                    &virtual_content,
                    upstream_id,
                )
                .await
            });
        }

        // Return the first non-empty inlay hint response
        let result =
            first_win::first_win(&mut join_set, |opt| matches!(opt, Some(v) if !v.is_empty()))
                .await;
        match result {
            Some(hints) => Ok(hints),
            None => {
                self.client
                    .log_message(
                        MessageType::LOG,
                        "No inlay hint response from any bridge server",
                    )
                    .await;
                Ok(None)
            }
        }
    }
}
