//! LSP initialize/initialized handshake for downstream servers.
//!
//! This module handles the LSP protocol handshake that establishes a connection
//! with a downstream language server. The handshake follows the LSP specification:
//! 1. Send `initialize` request
//! 2. Wait for `initialize` response
//! 3. Send `initialized` notification
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! The handshake uses `send_request()` and `send_notification()` to queue messages
//! via the channel-based writer task. This ensures all messages go through the
//! unified order queue for consistent FIFO ordering.

use std::io;

use tower_lsp_server::ls_types::{ClientCapabilities, ServerCapabilities, WorkspaceFolder};

use super::ConnectionHandle;
use super::connection_handle::NotificationSendResult;
use crate::lsp::bridge::protocol::{
    RequestId, build_initialize_request, build_initialized_notification,
    parse_initialize_response_capabilities,
};

/// Send `initialize`, await the response, send `initialized`, return the typed
/// `ServerCapabilities`. Invoked by `get_or_create_connection_with_timeout`
/// once the connection has spawned and the reader task is up; goes through
/// the single-writer channel (ls-bridge-message-ordering) for FIFO ordering.
#[allow(clippy::too_many_arguments)]
pub(super) async fn perform_lsp_handshake(
    handle: &ConnectionHandle,
    init_request_id: RequestId,
    init_response_rx: tokio::sync::oneshot::Receiver<serde_json::Value>,
    init_options: Option<serde_json::Value>,
    root_uri: Option<String>,
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    client_capabilities: Option<ClientCapabilities>,
    advertise_configuration: bool,
) -> io::Result<ServerCapabilities> {
    // 1. Build and send initialize request via the single-writer loop
    let init_request = build_initialize_request(
        init_request_id,
        init_options,
        root_uri,
        workspace_folders,
        client_capabilities.as_ref(),
        advertise_configuration,
        // Process-global opt-in, not Kakehashi's per-instance copy: the pool
        // has no Kakehashi in scope, and both derive from the same one-shot
        // env read (the per-instance AtomicBool exists only so unit tests can
        // toggle Kakehashi-level gates; none of them reach this handshake).
        crate::experimental::enabled(),
    );
    handle
        .send_request(init_request, init_request_id)
        .map_err(|e| -> io::Error { e.into() })?;

    // 2. Wait for initialize response via pre-registered receiver
    let response = init_response_rx
        .await
        .map_err(|_| io::Error::other("bridge: initialize response channel closed"))?;

    // 3. Validate response and extract typed capabilities
    let parsed = parse_initialize_response_capabilities(&response)?;
    for dropped in parsed.dropped {
        log::warn!(
            target: "kakehashi::bridge",
            "Downstream {} advertised malformed capability {:?}; ignoring it: {}",
            handle.key(),
            dropped.field,
            dropped.error,
        );
    }
    let capabilities = parsed.capabilities;

    // 4. Send initialized notification via the single-writer loop
    let initialized = build_initialized_notification();
    match handle.send_notification(initialized) {
        NotificationSendResult::Queued => {}
        NotificationSendResult::QueueFull => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge: initialized notification queue full",
            ));
        }
        NotificationSendResult::ChannelClosed => {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bridge: initialized notification channel closed",
            ));
        }
        NotificationSendResult::SerializationFailed => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge: failed to serialize initialized notification",
            ));
        }
    }

    Ok(capabilities)
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::lsp::bridge::actor::{ResponseRouter, spawn_reader_task};
    use crate::lsp::bridge::connection::AsyncBridgeConnection;

    #[tokio::test]
    async fn recovered_capabilities_complete_the_handshake() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("messages");
        let mut connection = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > \"$1\"".to_string(),
            "sh".to_string(),
            output.display().to_string(),
        ])
        .await
        .unwrap();
        let (writer, reader) = connection.split();
        let router = Arc::new(ResponseRouter::new());
        let reader_handle = spawn_reader_task(reader, Arc::clone(&router));
        let handle = ConnectionHandle::new(writer, router, reader_handle);
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        response_tx
            .send(serde_json::json!({
                "result": {
                    "capabilities": {
                        "hoverProvider": 42,
                        "completionProvider": {"triggerCharacters": ["."]}
                    }
                }
            }))
            .unwrap();

        let capabilities = perform_lsp_handshake(
            &handle,
            RequestId::new(1),
            response_rx,
            None,
            None,
            None,
            None,
            false,
        )
        .await
        .expect("recoverable capability errors must not stop the handshake");

        assert!(capabilities.hover_provider.is_none());
        assert!(capabilities.completion_provider.is_some());
        let mut messages = String::new();
        for _ in 0..40 {
            messages = std::fs::read_to_string(&output).unwrap_or_default();
            if messages.contains("\"method\":\"initialized\"") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            messages.contains("\"method\":\"initialized\""),
            "initialized notification was not written: {messages:?}"
        );
    }
}
