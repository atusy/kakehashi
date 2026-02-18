//! Find references method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Location, ReferenceParams};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_first_win;

impl Kakehashi {
    pub(crate) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "references")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out references requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_first_win(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_references_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
                        &t.injection_language,
                        &t.region_id,
                        t.region_start_line,
                        &t.virtual_content,
                        include_declaration,
                        t.upstream_id,
                    )
                    .await
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result.handle(&self.client, "references", None, Ok).await
    }
}
