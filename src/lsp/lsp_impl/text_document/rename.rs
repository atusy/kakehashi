//! Rename method for Kakehashi.

use std::sync::Arc;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{MessageType, RenameParams, WorkspaceEdit};

use super::super::Kakehashi;
use super::first_win;

impl Kakehashi {
    pub(crate) async fn rename_impl(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let new_name = params.new_name;

        let Some(ctx) = self
            .resolve_bridge_contexts(&lsp_uri, position, "rename")
            .await
        else {
            return Ok(None);
        };

        // Fan-out rename requests to all matching servers
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
            let new_name = new_name.clone();

            join_set.spawn(async move {
                pool.send_rename_request(
                    &server_name,
                    &server_config,
                    &uri,
                    position,
                    &injection_language,
                    &region_id,
                    region_start_line,
                    &virtual_content,
                    &new_name,
                    upstream_id,
                )
                .await
            });
        }

        // Return the first non-null rename response
        let result = first_win::first_win(&mut join_set, |opt| opt.is_some()).await;
        match result {
            Some(workspace_edit) => Ok(workspace_edit),
            None => {
                self.client
                    .log_message(
                        MessageType::LOG,
                        "No rename response from any bridge server",
                    )
                    .await;
                Ok(None)
            }
        }
    }
}
