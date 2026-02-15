//! Document highlight method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentHighlight, DocumentHighlightParams, MessageType};

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

impl Kakehashi {
    pub(crate) async fn document_highlight_impl(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "document_highlight")
            .await
        else {
            return Ok(None);
        };

        // Fan-out document highlight requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool, |t| async move {
            t.pool
                .send_document_highlight_request(
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

        // Return the first non-empty document highlight response
        let result =
            first_win::first_win(&mut join_set, |opt| matches!(opt, Some(v) if !v.is_empty()))
                .await;
        match result {
            Some(highlights) => Ok(highlights),
            None => {
                self.client
                    .log_message(
                        MessageType::LOG,
                        "No document highlight response from any bridge server",
                    )
                    .await;
                Ok(None)
            }
        }
    }
}
