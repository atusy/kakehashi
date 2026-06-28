//! Goto type definition method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and no coordinate translation. The first layer producing a non-empty
//! result wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::request::{GotoTypeDefinitionParams, GotoTypeDefinitionResponse};
use tower_lsp_server::ls_types::{Location, LocationLink, Position, Uri};

use crate::lsp::bridge::{location_link_to_location, normalize_host_goto_result};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;

const METHOD: &str = "textDocument/typeDefinition";

impl Kakehashi {
    pub(crate) async fn goto_type_definition_impl(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Result<Option<GotoTypeDefinitionResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let work_done_token = params.work_done_progress_params.work_done_token;

        let virt = self.type_definition_virt_layer(&lsp_uri, position, work_done_token);
        let links = self
            .walk_layers(
                &lsp_uri,
                METHOD,
                METHOD,
                raw_params,
                virt,
                normalize_host_goto_result,
                |links: &Vec<LocationLink>| !links.is_empty(),
            )
            .await?;
        Ok(links.map(|links| self.type_definition_response(links)))
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn type_definition_virt_layer(
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

        // Fan-out type definition requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_type_definition_request(
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
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());
        result
            .handle(&self.client, "type definition", None, Ok)
            .await
    }

    /// Shape the winning links per the client's `LocationLink` support.
    fn type_definition_response(&self, links: Vec<LocationLink>) -> GotoTypeDefinitionResponse {
        if self.settings_manager.supports_type_definition_link() {
            GotoTypeDefinitionResponse::Link(links)
        } else {
            let locations: Vec<Location> =
                links.into_iter().map(location_link_to_location).collect();
            GotoTypeDefinitionResponse::Array(locations)
        }
    }
}
