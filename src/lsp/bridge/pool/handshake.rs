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
    validate_initialize_response,
};

/// Send `initialize`, await the response, send `initialized`, return the typed
/// `ServerCapabilities`. Invoked by `get_or_create_connection_with_timeout`
/// once the connection has spawned and the reader task is up; goes through
/// the single-writer channel (ls-bridge-message-ordering) for FIFO ordering.
pub(super) async fn perform_lsp_handshake(
    handle: &ConnectionHandle,
    init_request_id: RequestId,
    init_response_rx: tokio::sync::oneshot::Receiver<serde_json::Value>,
    init_options: Option<serde_json::Value>,
    root_uri: Option<String>,
    workspace_folders: Option<Vec<WorkspaceFolder>>,
    client_capabilities: Option<ClientCapabilities>,
) -> io::Result<ServerCapabilities> {
    // 1. Build and send initialize request via the single-writer loop
    let init_request = build_initialize_request(
        init_request_id,
        init_options,
        root_uri,
        workspace_folders,
        client_capabilities.as_ref(),
    );
    handle
        .send_request(init_request, init_request_id)
        .map_err(|e| -> io::Error { e.into() })?;

    // 2. Wait for initialize response via pre-registered receiver
    let response = init_response_rx
        .await
        .map_err(|_| io::Error::other("bridge: initialize response channel closed"))?;

    // 3. Validate response and extract typed capabilities
    validate_initialize_response(&response)?;
    let caps_value = response
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .cloned()
        .unwrap_or_default();
    let capabilities = serde_json::from_value::<ServerCapabilities>(caps_value).unwrap_or_default();

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
