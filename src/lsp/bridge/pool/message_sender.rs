//! Message sender abstraction for downstream language server communication.
//!
//! This module provides the `MessageSender` trait for channel-based sending
//! per ADR-0015 (single-writer loop pattern).
//!
//! Implementations:
//! - `mpsc::Sender<OutboundMessage>`: Channel-based sends
//! - `ConnectionHandleSender`: Convenience wrapper around `Arc<ConnectionHandle>`
//!
//! This abstraction allows `ensure_document_opened()` and other shared functions to
//! work with different sender types without code duplication.

use std::io;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::connection_handle::{ConnectionHandle, NotificationSendResult};
use crate::lsp::bridge::actor::OutboundMessage;
use crate::lsp::bridge::protocol::JsonRpcNotification;

/// Abstraction for sending messages to a downstream language server.
///
/// This trait provides a unified interface for channel-based message sending
/// per ADR-0015 (single-writer loop pattern).
///
/// # Error Handling
///
/// The `send_notification` method returns `io::Result<()>`:
/// - `ErrorKind::BrokenPipe` if the channel is closed
/// - `ErrorKind::WouldBlock` if the channel is full (non-blocking backpressure)
pub(crate) trait MessageSender: Send {
    /// Send a notification to the downstream language server.
    ///
    /// Accepts typed `JsonRpcNotification<P>` to ensure callers provide
    /// well-formed JSON-RPC 2.0 notification envelopes at compile time.
    /// Serialization to wire format happens internally.
    ///
    /// Notifications are fire-and-forget (no response expected). If the send
    /// fails (e.g., queue full or channel closed), the notification is lost
    /// but an error is returned.
    fn send_notification<P: serde::Serialize + Send>(
        &mut self,
        notification: JsonRpcNotification<P>,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send;
}

// Implementation for channel-based sends
impl MessageSender for mpsc::Sender<OutboundMessage> {
    async fn send_notification<P: serde::Serialize + Send>(
        &mut self,
        notification: JsonRpcNotification<P>,
    ) -> io::Result<()> {
        let payload = serde_json::to_value(notification)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Use try_send for non-blocking backpressure per ADR-0015
        self.try_send(OutboundMessage::Untracked(payload))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "bridge: notification queue full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "bridge: writer channel closed")
                }
            })
    }
}

/// Wrapper around Arc<ConnectionHandle> for use with MessageSender trait.
///
/// This wrapper provides a convenient way to use ConnectionHandle with
/// `ensure_document_opened()` and other functions that take a generic `MessageSender`.
pub(crate) struct ConnectionHandleSender<'a>(pub(crate) &'a Arc<ConnectionHandle>);

// Implementation for ConnectionHandle wrapper
//
// Maps NotificationSendResult to io::ErrorKind per the trait contract:
// - Queued -> Ok(())
// - QueueFull -> WouldBlock (temporary backpressure, caller may retry)
// - ChannelClosed -> BrokenPipe (terminal failure)
// - SerializationFailed -> InvalidData (bug in caller)
impl MessageSender for ConnectionHandleSender<'_> {
    async fn send_notification<P: serde::Serialize + Send>(
        &mut self,
        notification: JsonRpcNotification<P>,
    ) -> io::Result<()> {
        match self.0.send_notification(notification) {
            NotificationSendResult::Queued => Ok(()),
            NotificationSendResult::QueueFull => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge: notification queue full",
            )),
            NotificationSendResult::ChannelClosed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bridge: notification channel closed",
            )),
            NotificationSendResult::SerializationFailed => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge: failed to serialize notification payload",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper to create a test notification.
    fn test_notification(method: &'static str) -> JsonRpcNotification<serde_json::Value> {
        JsonRpcNotification::new(method, json!({}))
    }

    /// Test that mpsc::Sender implements MessageSender correctly (non-blocking).
    #[tokio::test]
    async fn channel_sender_sends_notification() {
        let (tx, mut rx) = mpsc::channel::<OutboundMessage>(16);
        let mut sender: mpsc::Sender<OutboundMessage> = tx;

        let notification = test_notification("textDocument/didChange");
        let expected = serde_json::to_value(&notification).unwrap();
        sender
            .send_notification(notification)
            .await
            .expect("should send notification");

        // Verify the message was queued
        let msg = rx.recv().await.expect("should receive message");
        match msg {
            OutboundMessage::Untracked(received) => {
                assert_eq!(received, expected);
            }
            _ => panic!("Expected Untracked variant"),
        }
    }

    /// Test that channel returns WouldBlock when full.
    #[tokio::test]
    async fn channel_sender_returns_would_block_when_full() {
        // Create a channel with capacity 1
        let (tx, _rx) = mpsc::channel::<OutboundMessage>(1);
        let mut sender: mpsc::Sender<OutboundMessage> = tx;

        // Fill the channel
        sender
            .send_notification(test_notification("first"))
            .await
            .expect("first send should succeed");

        // Second send should fail with WouldBlock
        let result = sender.send_notification(test_notification("second")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(err.to_string().contains("queue full"));
    }

    /// Test that channel returns BrokenPipe when closed.
    #[tokio::test]
    async fn channel_sender_returns_broken_pipe_when_closed() {
        let (tx, rx) = mpsc::channel::<OutboundMessage>(16);
        let mut sender: mpsc::Sender<OutboundMessage> = tx;

        // Drop the receiver to close the channel
        drop(rx);

        let result = sender.send_notification(test_notification("test")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert!(err.to_string().contains("channel closed"));
    }
}
