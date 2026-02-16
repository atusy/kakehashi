//! Goto implementation method for Kakehashi.

use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp_server::ls_types::{Location, MessageType};

use crate::lsp::bridge::location_link_to_location;

use super::super::Kakehashi;
use super::first_win::{self, FirstWinResult, fan_out};

impl Kakehashi {
    pub(crate) async fn goto_implementation_impl(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "goto_implementation")
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) = match self.subscribe_cancel(&ctx.upstream_request_id) {
            Some((rx, guard)) => (Some(rx), Some(guard)),
            None => (None, None),
        };

        // Fan-out implementation requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool.clone(), |t| async move {
            t.pool
                .send_implementation_request(
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

        // Return the first non-empty implementation response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(&ctx.upstream_request_id);

        match result {
            FirstWinResult::Winner(Some(links)) => {
                if self.supports_implementation_link() {
                    Ok(Some(GotoImplementationResponse::Link(links)))
                } else {
                    let locations: Vec<Location> =
                        links.into_iter().map(location_link_to_location).collect();
                    Ok(Some(GotoImplementationResponse::Array(locations)))
                }
            }
            FirstWinResult::Winner(None) => Ok(None),
            FirstWinResult::NoWinner { errors } => {
                let level = if errors > 0 {
                    MessageType::WARNING
                } else {
                    MessageType::LOG
                };
                self.client
                    .log_message(
                        level,
                        "No implementation response from any bridge server",
                    )
                    .await;
                Ok(None)
            }
            FirstWinResult::Cancelled => Err(Error::request_cancelled()),
        }
    }
}
