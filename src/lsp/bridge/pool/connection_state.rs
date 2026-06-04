//! Connection state machine for downstream LSP connections (ls-bridge-message-ordering).

/// LSP-handshake lifecycle. Closed is terminal. Transitions (ls-bridge-message-ordering Operation Gating):
/// Initializing → Ready (init ok) | Failed (timeout/err) | Closing (shutdown mid-init);
/// Ready → Closing (shutdown); Closing → Closed (complete/timeout); Failed → Closed
/// directly (stdin gone, no LSP handshake). Failed connections are evicted from the
/// pool so the next request respawns a fresh server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    /// Server spawned, initialize request sent, awaiting response
    Initializing,
    /// Initialize/initialized handshake complete, ready for requests
    Ready,
    /// Initialization failed (timeout, error, server crash)
    Failed,
    /// Graceful shutdown in progress (LSP shutdown/exit handshake)
    Closing,
    /// Connection terminated (terminal state)
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::pool::test_helpers::*;

    /// Test that Ready state transitions to Closing on shutdown signal.
    ///
    /// ls-bridge-message-ordering: Ready → Closing transition occurs when shutdown is initiated.
    /// This is the graceful shutdown path for active connections.
    #[tokio::test]
    async fn ready_to_closing_transition() {
        let handle = create_handle_with_state(ConnectionState::Ready).await;

        // Verify initial state
        assert_eq!(
            handle.state(),
            ConnectionState::Ready,
            "Should start in Ready state"
        );

        // Trigger shutdown - should transition to Closing
        handle.begin_shutdown();

        // Verify transition
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "Ready + shutdown signal = Closing"
        );
    }

    /// Test that Initializing state transitions to Closing on shutdown signal.
    ///
    /// ls-bridge-graceful-shutdown: When shutdown is initiated during initialization, abort init
    /// and proceed directly to shutdown. This handles cases where editor closes
    /// during slow server startup.
    #[tokio::test]
    async fn initializing_to_closing_transition() {
        let handle = create_handle_with_state(ConnectionState::Initializing).await;

        // Verify initial state
        assert_eq!(
            handle.state(),
            ConnectionState::Initializing,
            "Should start in Initializing state"
        );

        // Trigger shutdown - should transition to Closing
        handle.begin_shutdown();

        // Verify transition
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "Initializing + shutdown signal = Closing"
        );
    }

    /// Test that Closing state transitions to Closed on completion.
    ///
    /// ls-bridge-message-ordering: Closing → Closed transition occurs when LSP shutdown/exit
    /// handshake completes or times out. This is the terminal state for
    /// graceful shutdown.
    #[tokio::test]
    async fn closing_to_closed_transition() {
        let handle = create_handle_with_state(ConnectionState::Closing).await;

        // Verify initial state
        assert_eq!(
            handle.state(),
            ConnectionState::Closing,
            "Should start in Closing state"
        );

        // Complete shutdown - should transition to Closed
        handle.complete_shutdown();

        // Verify transition
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "Closing + completion = Closed"
        );
    }

    /// Test that Failed state transitions directly to Closed (bypassing Closing).
    ///
    /// ls-bridge-graceful-shutdown: Failed connections cannot perform LSP shutdown/exit handshake
    /// because stdin is unavailable. They go directly to Closed state.
    #[tokio::test]
    async fn failed_to_closed_direct_transition() {
        let handle = create_handle_with_state(ConnectionState::Failed).await;

        // Verify initial state
        assert_eq!(
            handle.state(),
            ConnectionState::Failed,
            "Should start in Failed state"
        );

        // Direct shutdown completion - bypasses Closing state
        handle.complete_shutdown();

        // Verify transition
        assert_eq!(
            handle.state(),
            ConnectionState::Closed,
            "Failed + completion = Closed (bypasses Closing)"
        );
    }
}
