//! Goto type definition method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::Location;
use tower_lsp_server::ls_types::request::{GotoTypeDefinitionParams, GotoTypeDefinitionResponse};

use crate::lsp::bridge::location_link_to_location;

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

impl Kakehashi {
    pub(crate) async fn goto_type_definition_impl(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Result<Option<GotoTypeDefinitionResponse>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "goto_type_definition")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());

        // Fan-out type definition requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_type_definition_request(
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

        // Return the first non-empty type definition response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());

        result
            .handle(&self.client, "type definition", None, |value| match value {
                Some(links) => {
                    if self.supports_type_definition_link() {
                        Ok(Some(GotoTypeDefinitionResponse::Link(links)))
                    } else {
                        let locations: Vec<Location> =
                            links.into_iter().map(location_link_to_location).collect();
                        Ok(Some(GotoTypeDefinitionResponse::Array(locations)))
                    }
                }
                None => Ok(None),
            })
            .await
    }
}
