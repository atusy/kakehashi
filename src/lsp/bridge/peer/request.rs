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
    params: OptionalParams,
}

#[derive(Default)]
enum OptionalParams {
    #[default]
    Missing,
    Present(serde_json::Value),
}

impl<'de> Deserialize<'de> for OptionalParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::Present)
    }
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
    if matches!(
        &params.params,
        OptionalParams::Present(value) if !value.is_object() && !value.is_array()
    ) {
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

    let connection_id = deps.progress_connection_id;
    let registry = deps.inbound_request_registry.clone();
    let Some((cancel, generation, permit)) = registry.try_register_peer(connection_id, id.clone())
    else {
        tokio::spawn(async move {
            let response = jsonrpc::Response::from_error(
                id,
                request_failed(
                    "tooManyRequests",
                    "too many peer requests are awaiting responses",
                ),
            );
            send_server_response(&response_tx, response, &server_prefix, METHOD).await;
        });
        return;
    };

    let (downstream_id, response_rx) = match peer.register_peer_request() {
        Ok(registered) => registered,
        Err(error) => {
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
    };
    let mut router_guard = RouterCleanupGuard::new(peer.clone().router().clone(), downstream_id);
    // Register before the inner send: a $/cancelRequest arriving immediately
    // after the outer request must not fall into a send/register gap.
    let inner_params = match params.params {
        OptionalParams::Missing => None,
        OptionalParams::Present(value) => Some(value),
    };
    if let Err(error) = peer.send_request_value(params.method, inner_params, downstream_id) {
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

    let deadline = tokio::time::Instant::now() + super::super::pool::REQUEST_TIMEOUT;
    tokio::spawn(async move {
        let mut permit = Some(permit);
        let body = tokio::select! {
            response = peer.wait_for_response_until(downstream_id, response_rx, deadline) => {
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
                let cancellation = peer.router().cancel_peer(downstream_id);
                let should_notify = cancellation.unwrap_or(false);
                if cancellation.is_some() {
                    router_guard.disarm();
                }
                if should_notify {
                    let outcome = peer.send_notification(JsonRpcNotification::new(
                        "$/cancelRequest",
                        serde_json::json!({ "id": downstream_id.as_i64() }),
                    ));
                    if outcome != super::super::pool::NotificationSendResult::Queued {
                        log::warn!(
                            target: "kakehashi::bridge::peer",
                            "Could not queue peer cancellation for request {}: {:?}",
                            downstream_id.as_i64(),
                            outcome
                        );
                    }
                    let router = peer.router().clone();
                    let peer = peer.clone();
                    let permit = permit.take();
                    tokio::spawn(async move {
                        tokio::time::sleep_until(deadline).await;
                        if router.expire_peer_cancel(downstream_id) {
                            peer.fail_if_ready();
                        }
                        drop(permit);
                    });
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
    use crate::lsp::bridge::pool::{
        ConnectionKey, ConnectionState, test_helpers::create_handle_with_key,
    };

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
            params: OptionalParams::Missing,
        };
        let error = validate_params(&params).unwrap_err();
        assert_eq!(error.code, jsonrpc::ErrorCode::ServerError(-32803));
        assert_eq!(
            error.data,
            Some(serde_json::json!({ "reason": "methodDenied" }))
        );
    }

    #[test]
    fn explicit_null_params_are_invalid_but_omission_is_allowed() {
        let omitted = PeerRequestParams::deserialize(&serde_json::json!({
            "id": "peer",
            "method": "custom/request"
        }))
        .unwrap();
        validate_params(&omitted).unwrap();

        let explicit_null = PeerRequestParams::deserialize(&serde_json::json!({
            "id": "peer",
            "method": "custom/request",
            "params": null
        }))
        .unwrap();
        let error = validate_params(&explicit_null).unwrap_err();
        assert_eq!(error.code, jsonrpc::ErrorCode::InvalidParams);
    }

    #[test]
    fn downstream_error_data_cannot_impersonate_a_bridge_failure() {
        assert_eq!(
            normalize_response(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 91,
                "error": {
                    "code": -32603,
                    "message": "target error",
                    "data": { "kakehashiBridgeFailure": "connectionLost" }
                }
            }))
            .unwrap(),
            serde_json::json!({
                "error": {
                    "code": -32603,
                    "message": "target error",
                    "data": { "kakehashiBridgeFailure": "connectionLost" }
                }
            })
        );
    }

    #[tokio::test]
    async fn router_transport_failures_are_out_of_band() {
        let peer =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("oxfmt"))
                .await;
        let (request_id, response_rx) = peer.register_peer_request().unwrap();
        assert!(peer.router().fail_request(request_id, "write error"));

        let error = peer
            .wait_for_response(request_id, response_rx)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
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
