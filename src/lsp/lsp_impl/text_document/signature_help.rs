//! Signature help method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Position, SignatureHelp, SignatureHelpParams, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/signatureHelp";

impl Kakehashi {
    pub(crate) async fn signature_help_impl(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let virt = self.signature_help_virt_layer(&lsp_uri, position);
        self.walk_layers(
            &lsp_uri,
            METHOD,
            METHOD,
            raw_params,
            virt,
            parse_host_verbatim::<SignatureHelp>,
            // An empty-but-shaped response ({ signatures: [] }) is "nothing
            // here" — fall through to the next layer instead of letting it
            // win, consistent with the host fan-in's empty-shape handling.
            |help: &SignatureHelp| !help.signatures.is_empty(),
        )
        .await
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn signature_help_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
    ) -> Result<Option<SignatureHelp>> {
        // Use shared preamble to resolve injection context with ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out signature help requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_signature_help_request(
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
            |opt| opt.is_some(),
            cancel_rx,
        )
        .await;
        result
            .handle(&self.notifier(), "signature help", None, Ok)
            .await
    }
}
