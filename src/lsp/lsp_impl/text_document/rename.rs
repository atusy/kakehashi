//! Rename method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{NumberOrString, Position, RenameParams, Uri, WorkspaceEdit};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/rename";

impl Kakehashi {
    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token.clone();
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        let virt = self.rename_virt_layer(&lsp_uri, position, &new_name, work_done_token);
        self.walk_layers(
            &lsp_uri,
            METHOD,
            METHOD,
            raw_params,
            virt,
            parse_host_verbatim::<WorkspaceEdit>,
            |_| true,
        )
        .await
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn rename_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        new_name: &str,
        client_progress_token: Option<NumberOrString>,
    ) -> Result<Option<WorkspaceEdit>> {
        let Some(mut ctx) = self.resolve_bridge_contexts(lsp_uri, position, METHOD) else {
            return Ok(None);
        };
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out rename requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| {
                let new_name = new_name.to_string();
                async move {
                    t.pool
                        .send_rename_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            position,
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            &new_name,
                            t.upstream_id,
                            t.client_progress_token,
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
