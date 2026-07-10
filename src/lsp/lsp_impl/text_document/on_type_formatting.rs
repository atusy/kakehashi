//! `textDocument/onTypeFormatting` handler (#354).
//!
//! Position-based like hover: the typed character sits inside (at most) one
//! injection region, so the virt layer resolves the region under the cursor
//! and fans out per the preferred strategy. The downstream-side trigger
//! filter (the second half of #354's double filter) lives in the bridge
//! method; this handler only routes.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DocumentOnTypeFormattingParams, FormattingOptions, Position, TextEdit, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/onTypeFormatting";

impl Kakehashi {
    pub(crate) async fn on_type_formatting_impl(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let virt =
            self.on_type_formatting_virt_layer(&lsp_uri, position, &params.ch, params.options);
        self.walk_layers(
            &lsp_uri,
            METHOD,
            METHOD,
            raw_params,
            virt,
            parse_host_verbatim::<Vec<TextEdit>>,
            |edits: &Vec<TextEdit>| !edits.is_empty(),
        )
        .await
    }

    /// Virt layer: bridge the injection region under the typed character.
    async fn on_type_formatting_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        ch: &str,
        options: FormattingOptions,
    ) -> Result<Option<Vec<TextEdit>>> {
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            move |t| {
                let ch = ch.to_string();
                let options = options.clone();
                async move {
                    t.pool
                        .send_on_type_formatting_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            position,
                            &ch,
                            options,
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                        )
                        .await
                }
            },
            // Like formatting: `Some(vec![])` is an authoritative "no edits
            // needed" and stops the fan-out; `None` (no provider / trigger not
            // declared / no response) falls through to lower-priority servers.
            |opt| opt.is_some(),
            cancel_rx,
        )
        .await;
        result
            .handle(&self.client, "onTypeFormatting", None, Ok)
            .await
    }
}
