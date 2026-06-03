//! Outbound message types for the writer loop.
//!
//! This module provides the `OutboundMessage` enum that represents messages
//! to be sent to a downstream language server via the single-writer loop
//! (ls-bridge-message-ordering).
//!
//! All messages pass through the unified order queue to ensure FIFO ordering.
//! The writer task consumes these and writes to stdin.

use crate::lsp::bridge::protocol::RequestId;

/// Message to be sent to a downstream language server.
///
/// All messages pass through the unified order queue to ensure FIFO ordering
/// per ls-bridge-message-ordering. The writer task consumes these and writes to stdin.
///
/// The variants reflect whether the ResponseRouter tracks the message:
/// - `Untracked`: fire-and-forget (notifications, server-request responses)
/// - `Tracked`: request_id registered with ResponseRouter for cleanup on failure
#[derive(Debug)]
pub(crate) enum OutboundMessage {
    /// Untracked message (no ResponseRouter entry to clean up on failure).
    ///
    /// Used for notifications and server-request responses — anything that
    /// doesn't have a corresponding oneshot waiter in the ResponseRouter.
    /// If the queue is full, untracked messages are dropped with WARN logging.
    Untracked(serde_json::Value),

    /// Tracked request (response expected, registered with ResponseRouter).
    ///
    /// The request_id must be registered with ResponseRouter BEFORE queuing.
    /// If the queue is full, the request is rejected with REQUEST_FAILED
    /// and the router entry is cleaned up.
    Tracked {
        /// The JSON-RPC request payload
        payload: serde_json::Value,
        /// Request ID for correlation (already registered with router)
        request_id: RequestId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outbound_message_untracked_debug_format() {
        let untracked = OutboundMessage::Untracked(json!({"method": "textDocument/didChange"}));
        let debug_str = format!("{:?}", untracked);
        assert!(
            debug_str.contains("Untracked"),
            "Debug format should contain 'Untracked': {}",
            debug_str
        );
    }

    #[test]
    fn outbound_message_tracked_debug_format() {
        let tracked = OutboundMessage::Tracked {
            payload: json!({"method": "textDocument/hover", "id": 42}),
            request_id: RequestId::new(42),
        };
        let debug_str = format!("{:?}", tracked);
        assert!(
            debug_str.contains("Tracked"),
            "Debug format should contain 'Tracked': {}",
            debug_str
        );
        assert!(
            debug_str.contains("request_id"),
            "Debug format should contain 'request_id': {}",
            debug_str
        );
    }
}
