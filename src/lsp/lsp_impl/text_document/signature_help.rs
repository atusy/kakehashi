//! Signature help method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{SignatureHelp, SignatureHelpParams};

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

impl Kakehashi {
    pub(crate) async fn signature_help_impl(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Use shared preamble to resolve injection context with ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "signature_help")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());

        // Fan-out signature help requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_signature_help_request(
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

        // Return the first non-null signature help response
        let result = first_win::first_win(&mut join_set, |opt| opt.is_some(), cancel_rx).await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
        result
            .handle(&self.client, "signature help", None, Ok)
            .await
    }
}
