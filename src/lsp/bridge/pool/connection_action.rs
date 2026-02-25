//! Connection action decision logic for the language server pool.
//!
//! This module extracts pure decision logic from `get_or_create_connection_with_timeout`,
//! enabling unit testing without spawning real processes.

use std::io;

use super::ConnectionState;

/// Maximum consecutive panics before giving up on a server.
///
/// After this many consecutive handshake task panics for a server,
/// we stop retrying and return an error to prevent infinite retry loops.
pub(super) const MAX_CONSECUTIVE_PANICS: u32 = 3;

/// Bridge-specific errors that can be matched by type.
///
/// This enum provides type-safe error handling for bridge connection operations,
/// avoiding fragile string comparison. Each variant converts to an appropriate
/// `io::Error` while preserving the ability to match on specific error conditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeError {
    /// Server is currently initializing; request should wait or retry later.
    Initializing,
    /// Server is closing and cannot accept new requests.
    Closing,
    /// Server disabled after repeated handshake failures.
    Disabled,
    // === ADR-0015 Single-Writer Loop variants ===
    /// Request queue is full; request rejected with REQUEST_FAILED.
    ///
    /// Per ADR-0015 Section 3, when the bounded order queue (capacity 256)
    /// is full, requests are rejected with this error. Notifications are
    /// dropped instead (with WARN logging).
    QueueFull,
    /// Writer channel closed; connection is being torn down.
    ///
    /// This occurs when the writer task has exited (normally or via panic)
    /// and the channel is no longer accepting messages.
    ChannelClosed,
    /// Payload could not be serialized to JSON.
    ///
    /// This indicates a bug in the caller (e.g., an invalid `Serialize` impl).
    /// It is distinct from channel errors and should not trigger reconnection.
    SerializationFailed,
}

impl BridgeError {
    /// Check if this error indicates the server is still initializing.
    ///
    /// This enables callers to decide whether to wait for the server
    /// instead of failing immediately.
    pub(crate) fn is_initializing(&self) -> bool {
        matches!(self, BridgeError::Initializing)
    }

    /// Get the LSP error code for this error.
    ///
    /// All bridge errors map to REQUEST_FAILED (-32803) per LSP spec,
    /// indicating the request could not be processed but may be retried.
    ///
    /// Note: This is distinct from INTERNAL_ERROR (-32603) used by
    /// `ResponseRouter::fail_all()` for connection failures/panics.
    #[cfg(test)]
    fn lsp_error_code(&self) -> i32 {
        -32803 // REQUEST_FAILED per LSP spec
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Initializing => {
                write!(f, "bridge: downstream server initializing")
            }
            BridgeError::Closing => write!(f, "bridge: connection closing"),
            BridgeError::Disabled => {
                write!(
                    f,
                    "bridge: server disabled after repeated handshake failures"
                )
            }
            BridgeError::QueueFull => write!(f, "bridge: request queue full"),
            BridgeError::ChannelClosed => write!(f, "bridge: writer channel closed"),
            BridgeError::SerializationFailed => {
                write!(f, "bridge: failed to serialize request payload")
            }
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<BridgeError> for io::Error {
    fn from(err: BridgeError) -> Self {
        io::Error::other(err)
    }
}

/// Action to take when requesting a connection for a language.
///
/// This enum represents the decision made based on existing connection state,
/// allowing the logic to be tested in isolation from I/O operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ConnectionAction {
    /// Return the existing connection (state is Ready)
    ReturnExisting,
    /// Spawn a new connection (no connection exists, or previous was Failed/Closed)
    SpawnNew,
    /// Fail fast with typed error (state is Initializing, Closing, or too many panics)
    FailFast(BridgeError),
}

/// Decide what action to take based on existing connection state and panic history.
///
/// This is a pure function that can be unit tested without spawning processes.
///
/// # State-based decisions per ADR-0015 Operation Gating:
/// - `None`: No connection exists → SpawnNew (unless too many panics)
/// - `Initializing`: Init in progress → FailFast (reject concurrent requests)
/// - `Ready`: Connection available → ReturnExisting
/// - `Failed`: Previous attempt failed → SpawnNew (unless too many panics)
/// - `Closing`: Shutdown in progress → FailFast (reject new requests)
/// - `Closed`: Connection terminated → SpawnNew (unless too many panics)
///
/// # Panic Protection
///
/// If `consecutive_panic_count >= MAX_CONSECUTIVE_PANICS`, returns FailFast
/// to prevent infinite retry loops when a handshake consistently panics.
pub(super) fn decide_connection_action(
    state: Option<ConnectionState>,
    consecutive_panic_count: u32,
) -> ConnectionAction {
    // Check for too many consecutive panics first
    if consecutive_panic_count >= MAX_CONSECUTIVE_PANICS {
        return ConnectionAction::FailFast(BridgeError::Disabled);
    }

    match state {
        None => ConnectionAction::SpawnNew,
        Some(ConnectionState::Ready) => ConnectionAction::ReturnExisting,
        Some(ConnectionState::Initializing) => {
            ConnectionAction::FailFast(BridgeError::Initializing)
        }
        Some(ConnectionState::Failed) => ConnectionAction::SpawnNew,
        Some(ConnectionState::Closing) => ConnectionAction::FailFast(BridgeError::Closing),
        Some(ConnectionState::Closed) => ConnectionAction::SpawnNew,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that no existing connection results in SpawnNew action.
    #[test]
    fn no_connection_spawns_new() {
        assert_eq!(
            decide_connection_action(None, 0),
            ConnectionAction::SpawnNew
        );
    }

    /// Test that Ready state returns existing connection.
    #[test]
    fn ready_state_returns_existing() {
        assert_eq!(
            decide_connection_action(Some(ConnectionState::Ready), 0),
            ConnectionAction::ReturnExisting
        );
    }

    /// Test that Initializing state fails fast.
    ///
    /// ADR-0015: Concurrent requests during initialization should fail immediately
    /// rather than block or queue.
    #[test]
    fn initializing_state_fails_fast() {
        let action = decide_connection_action(Some(ConnectionState::Initializing), 0);
        assert_eq!(
            action,
            ConnectionAction::FailFast(BridgeError::Initializing)
        );
    }

    /// Test that Failed state triggers respawn.
    ///
    /// ADR-0015: Failed connections are removed from pool, next request spawns fresh.
    #[test]
    fn failed_state_spawns_new() {
        assert_eq!(
            decide_connection_action(Some(ConnectionState::Failed), 0),
            ConnectionAction::SpawnNew
        );
    }

    /// Test that Closing state fails fast.
    ///
    /// ADR-0017: Connections in graceful shutdown reject new requests.
    #[test]
    fn closing_state_fails_fast() {
        let action = decide_connection_action(Some(ConnectionState::Closing), 0);
        assert_eq!(action, ConnectionAction::FailFast(BridgeError::Closing));
    }

    /// Test that Closed state triggers respawn.
    ///
    /// Terminated connections should be replaced on next request.
    #[test]
    fn closed_state_spawns_new() {
        assert_eq!(
            decide_connection_action(Some(ConnectionState::Closed), 0),
            ConnectionAction::SpawnNew
        );
    }

    /// Test that too many consecutive panics stops retry attempts.
    ///
    /// Prevents infinite retry loops when handshake consistently panics.
    #[test]
    fn too_many_panics_fails_fast() {
        // At the threshold, should fail fast
        let action = decide_connection_action(None, MAX_CONSECUTIVE_PANICS);
        assert_eq!(action, ConnectionAction::FailFast(BridgeError::Disabled));

        // Above the threshold, should also fail fast
        let action =
            decide_connection_action(Some(ConnectionState::Failed), MAX_CONSECUTIVE_PANICS + 1);
        assert_eq!(action, ConnectionAction::FailFast(BridgeError::Disabled));
    }

    /// Test that below panic threshold allows normal retry.
    #[test]
    fn below_panic_threshold_allows_retry() {
        // Just below the threshold, should still spawn new
        let action =
            decide_connection_action(Some(ConnectionState::Failed), MAX_CONSECUTIVE_PANICS - 1);
        assert_eq!(action, ConnectionAction::SpawnNew);
    }

    // ========================================
    // ADR-0015 Single-Writer Loop error tests
    // ========================================

    /// Test QueueFull error has correct LSP error code.
    #[test]
    fn queue_full_error_has_correct_lsp_code() {
        assert_eq!(BridgeError::QueueFull.lsp_error_code(), -32803);
    }

    /// Test ChannelClosed error has correct LSP error code.
    #[test]
    fn channel_closed_error_has_correct_lsp_code() {
        assert_eq!(BridgeError::ChannelClosed.lsp_error_code(), -32803);
    }

    /// Test QueueFull display message.
    #[test]
    fn queue_full_display_message() {
        assert_eq!(
            format!("{}", BridgeError::QueueFull),
            "bridge: request queue full"
        );
    }

    /// Test ChannelClosed display message.
    #[test]
    fn channel_closed_display_message() {
        assert_eq!(
            format!("{}", BridgeError::ChannelClosed),
            "bridge: writer channel closed"
        );
    }

    /// Test that all error variants have the same LSP error code (REQUEST_FAILED).
    #[test]
    fn all_errors_use_request_failed_code() {
        // All bridge errors should map to REQUEST_FAILED (-32803)
        assert_eq!(BridgeError::Initializing.lsp_error_code(), -32803);
        assert_eq!(BridgeError::Closing.lsp_error_code(), -32803);
        assert_eq!(BridgeError::Disabled.lsp_error_code(), -32803);
        assert_eq!(BridgeError::QueueFull.lsp_error_code(), -32803);
        assert_eq!(BridgeError::ChannelClosed.lsp_error_code(), -32803);
        assert_eq!(BridgeError::SerializationFailed.lsp_error_code(), -32803);
    }
}
