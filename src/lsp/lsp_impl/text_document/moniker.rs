//! Moniker method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Moniker, MonikerParams};

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

impl Kakehashi {
    pub(crate) async fn moniker_impl(&self, params: MonikerParams) -> Result<Option<Vec<Moniker>>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "moniker")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(&ctx.upstream_request_id);

        // Fan-out moniker requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_moniker_request(
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

        // Return the first non-empty moniker response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(&ctx.upstream_request_id);

        result.handle(&self.client, "moniker", None, Ok).await
    }
}
