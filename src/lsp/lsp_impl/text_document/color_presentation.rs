//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorPresentation, ColorPresentationParams};

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let color = params.color;

        // Use resolve_bridge_contexts() for fan-out to ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, range.start, "colorPresentation")
            .await
        else {
            return Ok(Vec::new());
        };

        // Convert Color to JSON Value for bridge (shared across all tasks)
        let color_json = serde_json::to_value(color).unwrap_or_default();

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(&ctx.upstream_request_id);

        // Fan-out color presentation requests to all matching servers
        let pool = self.bridge.pool_arc();
        let mut join_set = fan_out(&ctx, pool.clone(), |t| {
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
        });

        // Return the first non-empty color presentation response
        let result = first_win::first_win(
            &mut join_set,
            |presentations| !presentations.is_empty(),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(&ctx.upstream_request_id);

        result
            .handle(&self.client, "color presentation", Vec::new(), Ok)
            .await
    }
}
