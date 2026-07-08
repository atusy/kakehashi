//! `workspace/executeCommand` method for Kakehashi (#568 PR 6).
//!
//! A `Command` the bridge surfaced in a code action is executed here: the
//! origin server + host document are encoded in the command NAME, so the pool
//! decodes them and routes the request back to that server (see
//! [`dispatch_execute_command`](crate::lsp::bridge::pool::LanguageServerPool)).
//! The server's result is relayed verbatim; a command the bridge didn't mint,
//! or any downstream failure, yields a null result (fail soft).

use serde_json::Value;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::ExecuteCommandParams;

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn execute_command_impl(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();
        let pool = self.bridge.pool_arc();
        Ok(pool
            .dispatch_execute_command(params, &settings, upstream_id)
            .await)
    }
}
