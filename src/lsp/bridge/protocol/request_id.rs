//! JSON-RPC request ID type for LSP bridge communication.
//!
//! This module provides a type-safe wrapper for request IDs, preventing
//! confusion with other integer types and enabling compile-time guarantees.

/// JSON-RPC request ID for LSP communication.
///
/// Wraps `i64` to prevent confusion with other integer types (version numbers,
/// line numbers) and to serve as a `pending_requests` HashMap key (ADR-0015).
///
/// LSP allows numeric or string IDs; only numeric IDs are supported since the
/// bridge controls request ID generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RequestId(i64);

impl RequestId {
    /// Create a new RequestId from an i64 value.
    #[inline]
    pub(crate) fn new(id: i64) -> Self {
        Self(id)
    }

    /// Get the underlying i64 value.
    #[inline]
    pub(crate) fn as_i64(self) -> i64 {
        self.0
    }

    /// Extract RequestId from a JSON-RPC message.
    ///
    /// Returns `Some(RequestId)` if the message has a numeric "id" field,
    /// `None` if the field is missing or not a number (e.g., notifications).
    pub(crate) fn from_json(message: &serde_json::Value) -> Option<Self> {
        message.get("id")?.as_i64().map(Self)
    }
}

impl From<i64> for RequestId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<RequestId> for i64 {
    fn from(id: RequestId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_id_new_and_as_i64() {
        let id = RequestId::new(42);
        assert_eq!(id.as_i64(), 42);
    }

    #[test]
    fn request_id_from_i64() {
        let id: RequestId = 123.into();
        assert_eq!(id.as_i64(), 123);
    }

    #[test]
    fn request_id_into_i64() {
        let id = RequestId::new(456);
        let value: i64 = id.into();
        assert_eq!(value, 456);
    }

    #[test]
    fn request_id_equality() {
        let id1 = RequestId::new(1);
        let id2 = RequestId::new(1);
        let id3 = RequestId::new(2);

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn request_id_hash() {
        use std::collections::HashMap;

        let mut map: HashMap<RequestId, &str> = HashMap::new();
        map.insert(RequestId::new(1), "first");
        map.insert(RequestId::new(2), "second");

        assert_eq!(map.get(&RequestId::new(1)), Some(&"first"));
        assert_eq!(map.get(&RequestId::new(2)), Some(&"second"));
        assert_eq!(map.get(&RequestId::new(3)), None);
    }

    #[test]
    fn request_id_from_json_with_numeric_id() {
        let msg = json!({"jsonrpc": "2.0", "id": 42, "result": null});
        let id = RequestId::from_json(&msg);
        assert_eq!(id, Some(RequestId::new(42)));
    }

    #[test]
    fn request_id_from_json_without_id_returns_none() {
        let msg = json!({"jsonrpc": "2.0", "method": "initialized", "params": {}});
        let id = RequestId::from_json(&msg);
        assert_eq!(id, None);
    }

    #[test]
    fn request_id_from_json_with_null_id_returns_none() {
        let msg = json!({"jsonrpc": "2.0", "id": null, "result": null});
        let id = RequestId::from_json(&msg);
        assert_eq!(id, None);
    }
}
