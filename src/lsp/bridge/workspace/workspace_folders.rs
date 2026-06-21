//! `workspace/workspaceFolders` server-request handler.
//!
//! Inbound (downstream → bridge). The bridge advertises the
//! `workspace.workspaceFolders` capability, so a downstream may pull the folders
//! it serves; the bridge answers from this connection's current
//! [`WorkspaceFolderSet`](super::folder_set::WorkspaceFolderSet) (the
//! `WorkspaceFolder[] | null` response), which a `preferSharedInstance`
//! connection grows over time (#391).

use log::debug;
use tower_lsp_server::jsonrpc;

use crate::lsp::bridge::actor::ServerRequestDeps;

/// Handle a `workspace/workspaceFolders` request, returning the JSON-RPC body
/// the dispatcher wraps in a response.
pub(in crate::lsp::bridge) fn handle(
    server_prefix: &str,
    deps: &ServerRequestDeps,
) -> jsonrpc::Result<serde_json::Value> {
    debug!(
        target: "kakehashi::bridge::reader",
        "{}Answering workspace/workspaceFolders from the connection's current folders",
        server_prefix
    );
    Ok(serde_json::to_value(deps.workspace_folders.snapshot()).unwrap_or(serde_json::Value::Null))
}
