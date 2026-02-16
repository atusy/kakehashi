//! Find references method for Kakehashi.

use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::{Location, MessageType, ReferenceParams};

use super::super::Kakehashi;
use super::first_win::{self, FirstWinResult, fan_out};

impl Kakehashi {
    pub(crate) async fn references_impl(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let include_declaration = params.context.include_declaration;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "references")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = match self.subscribe_cancel(&ctx.upstream_request_id) {
            Some((rx, guard)) => (Some(rx), Some(guard)),
            None => (None, None),
        };

        // Fan-out references requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_references_request(
                    &t.server_name,
                    &t.server_config,
                    &t.uri,
                    position,
                    &t.injection_language,
                    &t.region_id,
                    t.region_start_line,
                    &t.virtual_content,
                    include_declaration,
                    t.upstream_id,
                )
                .await
        });

        // Return the first non-empty references response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(&ctx.upstream_request_id);

        match result {
            FirstWinResult::Winner(locations) => Ok(locations),
            FirstWinResult::NoWinner { errors } => {
                let level = if errors > 0 {
                    MessageType::WARNING
                } else {
                    MessageType::LOG
                };
                self.client
                    .log_message(level, "No references response from any bridge server")
                    .await;
                Ok(None)
            }
            FirstWinResult::Cancelled => Err(Error::request_cancelled()),
        }
    }
}
