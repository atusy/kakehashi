//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorPresentation, ColorPresentationParams};

use super::super::Kakehashi;
use crate::lsp::aggregation::aggregate::dispatch_aggregation;

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let color = params.color;

        let Some(ctx) = self
            .resolve_bridge_contexts_for_range(&lsp_uri, range, "colorPresentation")
            .await
        else {
            return Ok(Vec::new());
        };

        // Convert Color to JSON Value for bridge (shared across all tasks)
        let color_json = serde_json::to_value(color).unwrap_or_default();

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out color presentation requests to all matching servers
        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let result = dispatch_aggregation(
            &ctx.document,
            pool.clone(),
            |t| {
                let color_json = color_json.clone();
                async move {
                    t.pool
                        .send_color_presentation_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            range,
                            &color_json,
                            &t.injection_language,
                            &t.region_id,
                            t.region_start_line,
                            &t.virtual_content,
                            t.upstream_id,
                        )
                        .await
                }
            },
            |presentations| !presentations.is_empty(),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result
            .handle(&self.client, "color presentation", Vec::new(), Ok)
            .await
    }
}
