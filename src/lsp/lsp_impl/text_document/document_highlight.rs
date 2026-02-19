//! Document highlight method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentHighlight, DocumentHighlightParams};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;

impl Kakehashi {
    pub(crate) async fn document_highlight_impl(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "textDocument/documentHighlight")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out document highlight requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_document_highlight_request(
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
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result
            .handle(&self.client, "document highlight", None, Ok)
            .await
    }
}
