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

use crate::config::settings::LayerSource;
use crate::lsp::bridge::location_link_to_location;

use super::super::{Kakehashi, uri_to_url};
use crate::lsp::aggregation::server::{dispatch_host_preferred, dispatch_preferred};

const METHOD: &str = "textDocument/definition";

impl Kakehashi {
    pub(crate) async fn goto_definition_impl(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in definition: {}", lsp_uri.as_str());
            return Ok(None);
        };
        let Some(host_language) = self.document_language(&uri) else {
            return Ok(None);
        };

        let layer_cfg = self.resolve_layer_config(&host_language, METHOD);
        for layer in &layer_cfg.order {
            let links = match layer {
                LayerSource::Virt => self.definition_virt_layer(&lsp_uri, position).await?,
                LayerSource::Host => self.definition_host_layer(&lsp_uri, position).await?,
                // No native definition implementation: empty contributor.
                LayerSource::Native => None,
            };
            if let Some(links) = links
                && !links.is_empty()
            {
                return Ok(Some(self.definition_response(links)));
            }
        }
        Ok(None)
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn definition_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
    ) -> Result<Option<Vec<LocationLink>>> {
        let Some(ctx) = self.resolve_bridge_contexts(lsp_uri, position, METHOD) else {
            return Ok(None);
        };

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
                    )
                    .await
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());
        result.handle(&self.client, "definition", None, Ok).await
    }

    /// Host layer: bridge the host document itself (real URI, responses
    /// verbatim — host-document-bridge).
    async fn definition_host_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
    ) -> Result<Option<Vec<LocationLink>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();
        let result = dispatch_host_preferred(
            &ctx,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_host_definition_request(
                        &t.server_name,
                        &t.server_config,
                        &crate::lsp::bridge::HostDocument {
                            uri: &t.uri,
                            language_id: &t.language_id,
                            text: &t.text,
                        },
                        position,
                        t.upstream_id,
                    )
                    .await
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
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
