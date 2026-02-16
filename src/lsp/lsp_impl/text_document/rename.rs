//! Rename method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{MessageType, RenameParams, WorkspaceEdit};

use super::super::Kakehashi;
use super::first_win::{self, fan_out};

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
        let position = ctx.position;
        let mut join_set = fan_out(&ctx, pool, |t| {
            let new_name = new_name.clone();
            async move {
                t.pool
                    .send_rename_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
                        &t.injection_language,
                        &t.region_id,
                        t.region_start_line,
                        &t.virtual_content,
                        &new_name,
                        t.upstream_id,
                    )
                    .await
            }
        });

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
