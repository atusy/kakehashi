//! Typed JSON-RPC 2.0 message wrappers for LSP communication.
//!
//! These structs replace raw `serde_json::Value` construction in protocol
//! builders, enabling compile-time validation of message structure.

use serde::Serialize;

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
}
