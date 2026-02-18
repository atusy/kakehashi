//! Goto declaration method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::Location;
use tower_lsp_server::ls_types::request::{GotoDeclarationParams, GotoDeclarationResponse};

use crate::lsp::bridge::location_link_to_location;

use super::super::Kakehashi;
use crate::lsp::aggregation::aggregate::dispatch_first_win;

impl Kakehashi {
    pub(crate) async fn goto_declaration_impl(
        &self,
        params: GotoDeclarationParams,
    ) -> Result<Option<GotoDeclarationResponse>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "goto_declaration")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out declaration requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_first_win(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_declaration_request(
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
            .handle(&self.client, "declaration", None, |value| match value {
                Some(links) => {
                    if self.supports_declaration_link() {
                        Ok(Some(GotoDeclarationResponse::Link(links)))
                    } else {
                        let locations: Vec<Location> =
                            links.into_iter().map(location_link_to_location).collect();
                        Ok(Some(GotoDeclarationResponse::Array(locations)))
                    }
                }
                None => Ok(None),
            })
            .await
    }
}
