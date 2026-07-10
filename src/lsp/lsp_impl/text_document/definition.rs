//! Goto definition method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and no coordinate translation. The first layer producing a non-empty
//! result wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Location, LocationLink, Position, Uri,
};

use crate::lsp::bridge::{location_link_to_location, normalize_host_goto_result};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;

const METHOD: &str = "textDocument/definition";

impl Kakehashi {
    pub(crate) async fn goto_definition_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let work_done_token = params.work_done_progress_params.work_done_token;

        let virt = self.definition_virt_layer(&lsp_uri, position, work_done_token);
        let native = self.native_bindings_answer(&lsp_uri, position, |ctx| {
            super::native_bindings::native_definition(ctx, &lsp_uri)
        });
        let links = self
            .walk_layers_with_native(
                &lsp_uri,
                METHOD,
                METHOD,
                raw_params,
                virt,
                native,
                normalize_host_goto_result,
                |links: &Vec<LocationLink>| !links.is_empty(),
            )
            .await?;
        Ok(links.map(|links| self.definition_response(links)))
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn definition_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        client_progress_token: Option<tower_lsp_server::ls_types::NumberOrString>,
    ) -> Result<Option<Vec<LocationLink>>> {
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

        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_definition_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
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
        result.handle(&self.client, "definition", None, Ok).await
    }

    /// Shape the winning links per the client's `LocationLink` support.
    fn definition_response(&self, links: Vec<LocationLink>) -> GotoDefinitionResponse {
        if self.settings_manager.supports_definition_link() {
            GotoDefinitionResponse::Link(links)
        } else {
            let locations: Vec<Location> =
                links.into_iter().map(location_link_to_location).collect();
            GotoDefinitionResponse::Array(locations)
        }
    }
}
