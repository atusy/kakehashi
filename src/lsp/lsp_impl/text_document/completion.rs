//! Completion method for Kakehashi.

use std::sync::Arc;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CompletionParams, CompletionResponse, MessageType};

use super::super::Kakehashi;
use super::first_win;

impl Kakehashi {
    pub(crate) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        // Use shared preamble to resolve injection context with ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "completion")
            .await
        else {
            return Ok(None);
        };

        // Fan-out completion requests to all matching servers
        let pool = self.bridge.pool_arc();
        let mut join_set = tokio::task::JoinSet::new();
        let position = ctx.position;
        let region_start_line = ctx.resolved.region.line_range.start;

        for config in ctx.configs {
            let pool = Arc::clone(&pool);
            let uri = ctx.uri.clone();
            let injection_language = ctx.resolved.injection_language.clone();
            let region_id = ctx.resolved.region.region_id.clone();
            let virtual_content = ctx.resolved.virtual_content.clone();
            let upstream_id = ctx.upstream_request_id.clone();
            let server_name = config.server_name.clone();
            let server_config = Arc::new(config.config);

            join_set.spawn(async move {
                pool.send_completion_request(
                    &server_name,
                    &server_config,
                    &uri,
                    position,
                    &injection_language,
                    &region_id,
                    region_start_line,
                    &virtual_content,
                    upstream_id,
                )
                .await
            });
        }

        // Return the first non-empty completion response
        let result = first_win::first_win(
            &mut join_set,
            |opt| matches!(opt, Some(list) if !list.items.is_empty()),
        )
        .await
        .flatten();

        match result {
            Some(completion_list) => Ok(Some(CompletionResponse::List(completion_list))),
            None => {
                self.client
                    .log_message(
                        MessageType::LOG,
                        "No completion response from any bridge server",
                    )
                    .await;
                Ok(None)
            }
        }
    }
}
