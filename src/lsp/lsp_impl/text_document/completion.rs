//! Completion method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CompletionParams, CompletionResponse};

use super::super::Kakehashi;
use crate::lsp::aggregation::fan_in::first_win;
use crate::lsp::aggregation::fan_out::fan_out;

impl Kakehashi {
    pub(crate) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        // Use shared preamble to resolve injection context with ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "completion")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());

        // Fan-out completion requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_completion_request(
                    &t.server_name,
                    &t.server_config,
                    &t.uri,
                    position,
                    &t.injection_language,
                    &t.region_id,
                    t.region_start_line,
                    &t.virtual_content,
                    t.upstream_id,
                )
                .await
        });

        // Return the first non-empty completion response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(list) if !list.items.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());

        result
            .handle(&self.client, "completion", None, |v| {
                Ok(v.map(CompletionResponse::List))
            })
            .await
    }
}
