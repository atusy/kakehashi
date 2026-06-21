//! `client/unregisterCapability` server-request handler.
//!
//! Inbound (downstream → bridge). A downstream server unregisters a dynamic
//! capability it previously registered; the bridge drops it from the shared
//! [`DynamicCapabilityRegistry`] and acks with `null`. Param-parse failures (or
//! a missing `params` field) reply with InvalidParams (-32602), mirroring
//! [`register_capability`](super::register_capability).
//!
//! [`DynamicCapabilityRegistry`]: crate::lsp::bridge::pool::DynamicCapabilityRegistry

use log::{debug, warn};
use serde::Deserialize;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::UnregistrationParams;

use crate::lsp::bridge::actor::ServerRequestDeps;

/// Handle a `client/unregisterCapability` request, returning the JSON-RPC body
/// the dispatcher wraps in a response.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) -> jsonrpc::Result<serde_json::Value> {
    let Some(params) = message.get("params") else {
        warn!(
            target: "kakehashi::bridge::reader",
            "{}Request 'client/unregisterCapability' is missing 'params' field",
            server_prefix
        );
        return Err(jsonrpc::Error::invalid_params(
            "Request 'client/unregisterCapability' is missing 'params' field",
        ));
    };

    // Deserialize from the borrowed `params` value to avoid cloning the JSON
    // tree, matching the by-reference pattern used across the bridge handlers.
    match UnregistrationParams::deserialize(params) {
        Ok(unreg_params) => {
            for unreg in &unreg_params.unregisterations {
                debug!(
                    target: "kakehashi::bridge::reader",
                    "{}Unregistered dynamic capability: {} (id={})",
                    server_prefix, unreg.method, unreg.id
                );
            }
            deps.dynamic_capabilities
                .unregister(unreg_params.unregisterations);
            Ok(serde_json::Value::Null)
        }
        Err(e) => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}Failed to parse unregisterCapability params: {}",
                server_prefix, e
            );
            Err(jsonrpc::Error::invalid_params(format!(
                "Invalid params: {e}"
            )))
        }
    }
}
