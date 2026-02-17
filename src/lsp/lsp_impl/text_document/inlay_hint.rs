//! Inlay hint method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams};

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

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

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(&ctx.upstream_request_id);

        // Fan-out inlay hint requests to all matching servers
        let pool = self.bridge.pool_arc();
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_inlay_hint_request(
                    &t.server_name,
                    &t.server_config,
                    &t.uri,
                    range,
                    &t.injection_language,
                    &t.region_id,
                    t.region_start_line,
                    &t.virtual_content,
                    t.upstream_id,
                )
                .await
        });

        // Return the first non-empty inlay hint response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(&ctx.upstream_request_id);

        result
            .handle(&self.client, "inlay hint", None, Ok)
            .await
    }
}
