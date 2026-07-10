//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorPresentation, ColorPresentationParams};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        // Experimental (KAKEHASHI_EXPERIMENTAL=true): without the opt-in the
        // capability is not advertised, so answer a compliant empty result to
        // any client that calls regardless.
        if !self.experimental_enabled() {
            return Ok(Vec::new());
        }
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let color = params.color;

        let Some(ctx) = self
            .resolve_bridge_contexts_for_range(&lsp_uri, range, "textDocument/colorPresentation")
            .await
        else {
            return Ok(Vec::new());
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out color presentation requests to all matching servers
        let pool = self.bridge.pool_arc();
        // RAII sweep: virt-only handler with no layer race or outer sweep —
        // clean stale registry entries on every exit, including a dropped
        // request future (dispatch_preferred aborts losers without joining).
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard {
            pool: pool.clone(),
            id: ctx.document.upstream_request_id.clone(),
        };
        let range = ctx.range;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_color_presentation_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        range,
                        color,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
            |presentations| !presentations.is_empty(),
            cancel_rx,
        )
        .await;

        result
            .handle(&self.client, "color presentation", Vec::new(), Ok)
            .await
    }
}
