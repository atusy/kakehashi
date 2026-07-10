//! Find references method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Location, Position, ReferenceParams, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/references";

impl Kakehashi {
    pub(crate) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;
        let work_done_token = params.work_done_progress_params.work_done_token;

        let virt =
            self.references_virt_layer(&lsp_uri, position, include_declaration, work_done_token);
        let native = self.native_bindings_answer(&lsp_uri, position, |ctx| {
            super::native_bindings::native_references(ctx, &lsp_uri, include_declaration)
        });
        self.walk_layers_with_native(
            &lsp_uri,
            METHOD,
            METHOD,
            raw_params,
            virt,
            native,
            parse_host_verbatim::<Vec<Location>>,
            |locations: &Vec<Location>| !locations.is_empty(),
        )
        .await
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn references_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        include_declaration: bool,
        client_progress_token: Option<tower_lsp_server::ls_types::NumberOrString>,
    ) -> Result<Option<Vec<Location>>> {
        let Some(mut ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };
        // Aggregate the fanned-out servers' progress onto the editor's token
        // (ls-bridge-client-progress).
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out references requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
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
                        t.offset,
                        &t.virtual_content,
                        include_declaration,
                        t.upstream_id,
                        t.client_progress_token,
                    )
                    .await
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;

        result.handle(&self.client, "references", None, Ok).await
    }
}
