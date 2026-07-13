//! `workspace/diagnostic/refresh` server-request handler.
//!
//! Inbound (downstream → bridge → editor). A downstream server asks the client
//! to re-pull diagnostics; the bridge forwards the request upstream so the
//! editor triggers a fresh diagnostic pull, then acks the downstream with
//! `null`.

use log::debug;
use tower_lsp_server::jsonrpc;

use crate::lsp::bridge::actor::ServerRequestDeps;

/// Handle a `workspace/diagnostic/refresh` request, returning the JSON-RPC body
/// the dispatcher wraps in a response.
pub(in crate::lsp::bridge) fn handle(
    server_prefix: &str,
    deps: &ServerRequestDeps,
) -> jsonrpc::Result<serde_json::Value> {
    debug!(
        target: "kakehashi::bridge::reader",
        "{}Acknowledging workspace/diagnostic/refresh",
        server_prefix
    );
    let _ = deps;
    Ok(serde_json::Value::Null)
}
