//! `kakehashi/bridge/peer/request`: downstream → kakehashi → peer request.

use std::borrow::Cow;

use serde::Deserialize;
use tower_lsp_server::jsonrpc;

use crate::lsp::bridge::actor::{RouterCleanupGuard, ServerRequestDeps, send_server_response};
use crate::lsp::bridge::protocol::JsonRpcNotification;

const METHOD: &str = "kakehashi/bridge/peer/request";
const DENIED_METHODS: &[&str] = &[
    "initialize",
    "initialized",
    "shutdown",
    "exit",
    "$/cancelRequest",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerRequestParams {
    id: String,
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

fn request_failed(reason: &'static str, message: impl Into<String>) -> jsonrpc::Error {
    jsonrpc::Error {
        code: jsonrpc::ErrorCode::ServerError(-32803),
        message: Cow::Owned(format!("bridge/peer: {}", message.into())),
        data: Some(serde_json::json!({ "reason": reason })),
    }
}

fn validate_params(params: &PeerRequestParams) -> jsonrpc::Result<()> {
    if params.method.is_empty() {
        return Err(jsonrpc::Error::invalid_params("method must not be empty"));
    }
    if DENIED_METHODS.contains(&params.method.as_str()) {
        return Err(request_failed(
            "methodDenied",
            format!(
                "method '{}' is reserved for connection lifecycle",
                params.method
            ),
        ));
    }
    if params
        .params
        .as_ref()
        .is_some_and(|value| !value.is_object() && !value.is_array())
    {
        return Err(jsonrpc::Error::invalid_params(
            "inner params must be an object, array, or omitted",
        ));
    }
    Ok(())
}

/// Start an arbitrary request against one discovered peer without blocking the
/// originating connection's reader loop.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    id: jsonrpc::Id,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let response_tx = deps.response_tx.clone();
    let server_prefix = server_prefix.to_string();
    let params = match PeerRequestParams::deserialize(&message["params"]) {
        Ok(params) => params,
        Err(error) => {
            tokio::spawn(async move {
                let response = jsonrpc::Response::from_error(
                    id,
                    jsonrpc::Error::invalid_params(format!("Invalid params: {error}")),
                );
                send_server_response(&response_tx, response, &server_prefix, METHOD).await;
            });
            return;
        }
    };

    if let Err(error) = validate_params(&params) {
        tokio::spawn(async move {
            let response = jsonrpc::Response::from_error(id, error);
            send_server_response(&response_tx, response, &server_prefix, METHOD).await;
        });
        return;
    }

    let Some(peer) = deps
        .peer_directory
        .resolve(&deps.connection_key, &params.id)
    else {
        tokio::spawn(async move {
            let response = jsonrpc::Response::from_error(
                id,
                request_failed(
                    "unknownPeer",
                    "peer is absent, is the caller, or is not running",
                ),
            );
            send_server_response(&response_tx, response, &server_prefix, METHOD).await;
        });
        return;
    };

    let (downstream_id, response_rx) = match peer.register_request() {
        Ok(registered) => registered,
        Err(error) => {
            tokio::spawn(async move {
                let response = jsonrpc::Response::from_error(
                    id,
                    request_failed("forwardFailed", error.to_string()),
                );
                send_server_response(&response_tx, response, &server_prefix, METHOD).await;
            });
            return;
        }
    };
    let mut router_guard = RouterCleanupGuard::new(peer.clone().router().clone(), downstream_id);
    // Register before the inner send: a $/cancelRequest arriving immediately
    // after the outer request must not fall into a send/register gap.
    let connection_id = deps.progress_connection_id;
    let registry = deps.inbound_request_registry.clone();
    let (cancel, generation) = registry.register(connection_id, id.clone());
    if let Err(error) = peer.send_request_value(params.method, params.params, downstream_id) {
        registry.unregister(connection_id, &id, generation);
        tokio::spawn(async move {
            let response = jsonrpc::Response::from_error(
                id,
                request_failed("forwardFailed", error.to_string()),
            );
            send_server_response(&response_tx, response, &server_prefix, METHOD).await;
        });
        return;
    }

    tokio::spawn(async move {
        let body = tokio::select! {
            response = peer.wait_for_response(downstream_id, response_rx) => {
                router_guard.disarm();
                match response {
                    Ok(response) => normalize_response(response),
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                        Err(request_failed("requestTimeout", error.to_string()))
                    }
                    Err(error) => Err(request_failed("connectionLost", error.to_string())),
                }
            }
            _ = cancel.cancelled() => {
                let should_notify = peer.router().cancel_and_remove(downstream_id);
                router_guard.disarm();
                if should_notify {
                    peer.send_notification(JsonRpcNotification::new(
                        "$/cancelRequest",
                        serde_json::json!({ "id": downstream_id.as_i64() }),
                    ));
                }
                Err(jsonrpc::Error::request_cancelled())
            }
        };
        registry.unregister(connection_id, &id, generation);
        let response = match body {
            Ok(result) => jsonrpc::Response::from_ok(id, result),
            Err(error) => jsonrpc::Response::from_error(id, error),
        };
        send_server_response(&response_tx, response, &server_prefix, METHOD).await;
    });
}

fn normalize_response(response: serde_json::Value) -> jsonrpc::Result<serde_json::Value> {
    if response.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err(request_failed(
            "malformedResponse",
            "peer returned a response without jsonrpc: 2.0",
        ));
    }
    if let Some(reason) = response
        .pointer("/error/data/kakehashiBridgeFailure")
        .and_then(serde_json::Value::as_str)
        .and_then(|reason| match reason {
            "connectionLost" => Some("connectionLost"),
            "requestTimeout" => Some("requestTimeout"),
            _ => None,
        })
    {
        let message = response
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("peer connection failed");
        return Err(request_failed(reason, message));
    }
    match (response.get("result"), response.get("error")) {
        (Some(result), None) => Ok(serde_json::json!({ "result": result })),
        (None, Some(error)) => serde_json::from_value::<jsonrpc::Error>(error.clone())
            .and_then(serde_json::to_value)
            .map(|error| serde_json::json!({ "error": error }))
            .map_err(|_| {
                request_failed(
                    "malformedResponse",
                    "peer returned a malformed JSON-RPC error object",
                )
            }),
        _ => Err(request_failed(
            "malformedResponse",
            "peer returned an invalid JSON-RPC response envelope",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downstream_result_and_error_are_wrapped_without_internal_ids() {
        assert_eq!(
            normalize_response(serde_json::json!({
                "jsonrpc": "2.0", "id": 91, "result": null
            }))
            .unwrap(),
            serde_json::json!({ "result": null })
        );
        assert_eq!(
            normalize_response(serde_json::json!({
                "jsonrpc": "2.0", "id": 92,
                "error": { "code": -32601, "message": "missing" }
            }))
            .unwrap(),
            serde_json::json!({
                "error": { "code": -32601, "message": "missing" }
            })
        );
    }

    #[test]
    fn lifecycle_methods_are_denied_with_a_machine_readable_reason() {
        let params = PeerRequestParams {
            id: "denols".to_string(),
            method: "shutdown".to_string(),
            params: None,
        };
        let error = validate_params(&params).unwrap_err();
        assert_eq!(error.code, jsonrpc::ErrorCode::ServerError(-32803));
        assert_eq!(
            error.data,
            Some(serde_json::json!({ "reason": "methodDenied" }))
        );
    }

    #[test]
    fn bridge_failures_remain_outer_request_failures() {
        for reason in ["connectionLost", "requestTimeout"] {
            let error = normalize_response(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 91,
                "error": {
                    "code": -32603,
                    "message": "bridge transport failed",
                    "data": { "kakehashiBridgeFailure": reason }
                }
            }))
            .unwrap_err();
            assert_eq!(error.code, jsonrpc::ErrorCode::ServerError(-32803));
            assert_eq!(error.data, Some(serde_json::json!({ "reason": reason })));
        }
    }

    #[test]
    fn malformed_downstream_error_is_not_relayed() {
        let error = normalize_response(serde_json::json!({
            "jsonrpc": "2.0", "id": 92,
            "error": { "message": "missing code" }
        }))
        .unwrap_err();
        assert_eq!(
            error.data,
            Some(serde_json::json!({ "reason": "malformedResponse" }))
        );
    }
}
