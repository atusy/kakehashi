//! Typed JSON-RPC 2.0 message wrappers for LSP communication.
//!
//! These structs replace raw `serde_json::Value` construction in protocol
//! builders, enabling compile-time validation of message structure.

use log::warn;
use serde::Serialize;

/// Detect a JSON-RPC error response so transformers can short-circuit.
///
/// Per JSON-RPC 2.0 a response object MUST NOT contain both `result` and
/// `error`; when `error` is present the `result` is meaningless. Returns `true`
/// (after logging a warning tagged with `method`) when the response carries a
/// non-null `error`, signalling the caller to abandon `result` parsing and
/// return its own "no result" value.
///
/// A literal `"error": null` is treated as *no error*: some servers include the
/// null field alongside a valid `result`, and short-circuiting on it would drop
/// good results.
pub(crate) fn response_has_jsonrpc_error(response: &serde_json::Value, method: &str) -> bool {
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for {method}: {error}");
        true
    } else {
        false
    }
}

/// A JSON-RPC 2.0 request message (expects a response).
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcRequest<P> {
    jsonrpc: &'static str,
    id: i64,
    method: &'static str,
    params: P,
}

impl<P> JsonRpcRequest<P> {
    pub(crate) fn new(id: i64, method: &'static str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// A JSON-RPC 2.0 notification message (no response expected).
#[derive(Debug, Serialize)]
pub(crate) struct JsonRpcNotification<P> {
    jsonrpc: &'static str,
    method: &'static str,
    params: P,
}

impl<P> JsonRpcNotification<P> {
    pub(crate) fn new(method: &'static str, params: P) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_correctly() {
        let req = JsonRpcRequest::new(
            42,
            "textDocument/hover",
            serde_json::json!({"key": "value"}),
        );
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
        assert_eq!(json["method"], "textDocument/hover");
        assert_eq!(json["params"]["key"], "value");
    }

    #[test]
    fn notification_serializes_correctly() {
        let notif = JsonRpcNotification::new("initialized", serde_json::json!({}));
        let json = serde_json::to_value(&notif).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "initialized");
        assert!(json.get("id").is_none());
    }

    #[test]
    fn request_with_unit_params_serializes_null() {
        let req = JsonRpcRequest::new(1, "shutdown", ());
        let json = serde_json::to_value(&req).unwrap();
        assert!(json["params"].is_null());
    }

    #[test]
    fn response_with_error_is_detected() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "Method not found"},
        });
        assert!(response_has_jsonrpc_error(&response, "textDocument/hover"));
    }

    #[test]
    fn response_without_error_is_not_flagged() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"contents": "value"},
        });
        assert!(!response_has_jsonrpc_error(&response, "textDocument/hover"));
    }

    #[test]
    fn response_with_null_error_alongside_result_is_not_flagged() {
        // Some servers send `"error": null` next to a valid result; this MUST NOT
        // be treated as an error, otherwise good results get dropped.
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"contents": "value"},
            "error": null,
        });
        assert!(!response_has_jsonrpc_error(&response, "textDocument/hover"));
    }
}
