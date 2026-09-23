//! `client/registerCapability` server-request handler.
//!
//! Inbound (downstream → bridge). A downstream server registers a dynamic
//! capability; the bridge records it in the shared [`DynamicCapabilityRegistry`]
//! and acks with `null`. A shared instance registering
//! `workspace/didChangeWorkspaceFolders` also asks the pool to consolidate the
//! roots diverted away from it while it looked incapable (#968). Param-parse failures (or a missing `params` field)
//! reply with InvalidParams (-32602): a server that can't form its own request
//! is buggy, and the LSP spec allows an error response to any request.
//!
//! [`DynamicCapabilityRegistry`]: crate::lsp::bridge::pool::DynamicCapabilityRegistry

use log::{debug, warn};
use serde::Deserialize;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::RegistrationParams;

use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamRequest};
use crate::lsp::bridge::protocol::DID_CHANGE_WORKSPACE_FOLDERS_METHOD;

/// Handle a `client/registerCapability` request, returning the JSON-RPC body
/// the dispatcher wraps in a response.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) -> jsonrpc::Result<serde_json::Value> {
    let Some(params) = message.get("params") else {
        warn!(
            target: "kakehashi::bridge::reader",
            "{}Request 'client/registerCapability' is missing 'params' field",
            server_prefix
        );
        return Err(jsonrpc::Error::invalid_params(
            "Request 'client/registerCapability' is missing 'params' field",
        ));
    };

    // Deserialize from the borrowed `params` value to avoid cloning the JSON
    // tree, matching the by-reference pattern used across the bridge handlers.
    match RegistrationParams::deserialize(params) {
        Ok(reg_params) => {
            for reg in &reg_params.registrations {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Registered dynamic capability: {} (id={})",
                    server_prefix, reg.method, reg.id
                );
            }
            let folder_changes_on_shared = deps.connection_key.is_shared()
                && reg_params
                    .registrations
                    .iter()
                    .any(|reg| reg.method == DID_CHANGE_WORKSPACE_FOLDERS_METHOD);
            deps.dynamic_capabilities.register(reg_params.registrations);
            // Registered BEFORE signalling, so the pool's capability re-check
            // sees it. A per-root registration signals nothing: those are the
            // processes a consolidation would retire (#968).
            if folder_changes_on_shared
                && deps
                    .upstream_request_tx
                    .send(UpstreamRequest::ConsolidateSharedInstance {
                        server: deps.connection_key.server().to_owned(),
                    })
                    .is_err()
            {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Shared-instance consolidation not queued (forwarding loop gone)",
                    server_prefix
                );
            }
            Ok(serde_json::Value::Null)
        }
        Err(e) => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}Failed to parse registerCapability params: {}",
                server_prefix, e
            );
            Err(jsonrpc::Error::invalid_params(format!(
                "Invalid params: {e}"
            )))
        }
    }
}
