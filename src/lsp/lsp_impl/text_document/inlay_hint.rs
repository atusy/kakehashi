//! Inlay hint method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;

impl Kakehashi {
    pub(crate) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        let Some(ctx) = self
            .resolve_bridge_contexts_for_range(&lsp_uri, range, "inlay_hint")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out inlay hint requests to all matching servers
        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
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
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result.handle(&self.client, "inlay hint", None, Ok).await
    }
}
