//! Rename method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{RenameParams, WorkspaceEdit};

use super::super::Kakehashi;
use crate::lsp::aggregation::aggregate::dispatch_first_win;

impl Kakehashi {
    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "rename")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out rename requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_first_win(
            &ctx.document,
            pool.clone(),
            |t| {
                let new_name = new_name.clone();
                async move {
                    t.pool
                        .send_rename_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            position,
                            &t.injection_language,
                            &t.region_id,
                            t.region_start_line,
                            &t.virtual_content,
                            &new_name,
                            t.upstream_id,
                        )
                        .await
                }
            },
            |opt| opt.is_some(),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());
        result.handle(&self.client, "rename", None, Ok).await
    }
}
