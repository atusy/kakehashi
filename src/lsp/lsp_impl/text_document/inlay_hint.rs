//! Inlay hint method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the requested range, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams, NumberOrString, Range, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/inlayHint";

impl Kakehashi {
    pub(crate) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        // Move (not clone) the token out — `params` is consumed below.
        let work_done_token = params.work_done_progress_params.work_done_token;
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        let virt = self.inlay_hint_virt_layer(&lsp_uri, range, work_done_token);
        self.walk_layers(
            &lsp_uri,
            METHOD,
            METHOD,
            raw_params,
            virt,
            parse_host_verbatim::<Vec<InlayHint>>,
            |hints: &Vec<InlayHint>| !hints.is_empty(),
        )
        .await
    }

    /// Virt layer: bridge the injection region under the requested range.
    async fn inlay_hint_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        client_progress_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<InlayHint>>> {
        let Some(mut ctx) = self.resolve_bridge_contexts_for_range(lsp_uri, range, METHOD) else {
            return Ok(None);
        };
        ctx.document.client_progress_token = client_progress_token;

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
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                        t.client_progress_token,
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
