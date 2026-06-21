//! `client/registerCapability` server-request handler.
//!
//! Inbound (downstream → bridge). A downstream server registers a dynamic
//! capability; the bridge records it in the shared [`DynamicCapabilityRegistry`]
//! and acks with `null`. Param-parse failures (or a missing `params` field)
//! reply with InvalidParams (-32602): a server that can't form its own request
//! is buggy, and the LSP spec allows an error response to any request.
//!
//! [`DynamicCapabilityRegistry`]: crate::lsp::bridge::pool::DynamicCapabilityRegistry

use log::{debug, warn};
use serde::Deserialize;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::RegistrationParams;

use crate::lsp::bridge::actor::ServerRequestDeps;

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
            deps.dynamic_capabilities.register(reg_params.registrations);
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
