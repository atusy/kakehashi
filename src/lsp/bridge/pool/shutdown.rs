//! Shutdown coordination for downstream language servers.
//!
//! This module contains the shutdown-related methods for LanguageServerPool,
//! implementing graceful and forced shutdown per ls-bridge-graceful-shutdown (Graceful Shutdown).

use std::sync::Arc;

use super::{ConnectionState, GlobalShutdownTimeout, LanguageServerPool};

impl LanguageServerPool {
    /// Drains a JoinSet, logging any task panics with the provided context.
    pub(super) async fn drain_join_set(
        join_set: &mut tokio::task::JoinSet<()>,
        task_context: &str,
    ) {
        while let Some(result) = join_set.join_next().await {
            if let Err(e) = result {
                log::error!(
                    target: "kakehashi::bridge",
                    "{} panicked: {}",
                    task_context,
                    e
                );
            }
        }
    }

    /// Graceful shutdown of every downstream connection (ls-bridge-graceful-shutdown), parallel,
    /// under the default 10s `GlobalShutdownTimeout` (ls-bridge-timeout-hierarchy). Per-state:
    /// Ready/Initializing run the LSP shutdown handshake, Failed jumps straight
    /// to Closed (stdin is gone), Closing/Closed are skipped. Concurrent calls
    /// are safe (state machine is monotonic) but only the first does real work.
    pub(crate) async fn shutdown_all(&self) {
        self.shutdown_all_with_timeout(GlobalShutdownTimeout::default())
            .await;
    }

    /// Parallel graceful shutdown under a single global ceiling (ls-bridge-graceful-shutdown).
    /// Ready/Initializing connections run the LSP shutdown in parallel; Failed
    /// ones jump straight to Closed. When `timeout` elapses, survivors are
    /// force-killed (SIGTERM→SIGKILL on Unix) and all enter Closed.
    pub(crate) async fn shutdown_all_with_timeout(&self, timeout: GlobalShutdownTimeout) {
        // Track connections that were skipped for logging (minimize lock duration)
        let mut failed_connections: Vec<super::ConnectionKey> = Vec::new();
        let mut already_closing: Vec<super::ConnectionKey> = Vec::new();

        // Collect handles to shutdown - release lock before async operations
        let handles_to_shutdown: Vec<(super::ConnectionKey, Arc<super::ConnectionHandle>)> = {
            let connections = self.connections.lock().await;
            connections
                .iter()
                .filter_map(|(key, handle)| match handle.state() {
                    ConnectionState::Ready | ConnectionState::Initializing => {
                        Some((key.clone(), Arc::clone(handle)))
                    }
                    ConnectionState::Failed => {
                        failed_connections.push(key.clone());
                        handle.complete_shutdown();
                        None
                    }
                    ConnectionState::Closing | ConnectionState::Closed => {
                        already_closing.push(key.clone());
                        None
                    }
                })
                .collect()
        };

        // Log after releasing lock
        for key in failed_connections {
            log::debug!(
                target: "kakehashi::bridge",
                "Shutting down {} connection (Failed → Closed)",
                key
            );
        }
        for key in already_closing {
            log::debug!(
                target: "kakehashi::bridge",
                "Connection {} already shutting down or closed",
                key
            );
        }

        if handles_to_shutdown.is_empty() {
            return;
        }

        // Spawn graceful shutdown tasks into JoinSet (outside timeout so we can abort on timeout)
        let mut join_set = tokio::task::JoinSet::new();
        for (key, handle) in handles_to_shutdown {
            join_set.spawn(async move {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Performing graceful shutdown for {} connection",
                    key
                );
                if let Err(e) = handle.graceful_shutdown().await {
                    log::warn!(
                        target: "kakehashi::bridge",
                        "Graceful shutdown failed for {}: {}",
                        key, e
                    );
                }
            });
        }

        // Wait for all shutdowns to complete with global timeout
        let graceful_result = tokio::time::timeout(
            timeout.as_duration(),
            Self::drain_join_set(&mut join_set, "Shutdown task"),
        )
        .await;

        // Handle timeout: abort remaining tasks and force-kill connections
        if graceful_result.is_err() {
            log::warn!(
                target: "kakehashi::bridge",
                "Global shutdown timeout ({:?}) expired, force-killing remaining connections",
                timeout.as_duration()
            );

            // Abort still-running graceful shutdown tasks to avoid duplicate logs and wasted work.
            // Note: force_kill is idempotent (returns early if process exited), so any race is harmless.
            join_set.abort_all();

            self.force_kill_all().await;
        }
    }

    /// Post-timeout fallback: terminate every non-closed connection in
    /// parallel and mark it Closed (ls-bridge-graceful-shutdown). Unix uses SIGTERM→SIGKILL with
    /// a 2s grace period; Windows uses `TerminateProcess` directly.
    ///
    /// Each kill goes through `graceful_shutdown` with a short (3s) cap so a
    /// hung writer task can't stall this path — on expiry we mark Closed
    /// anyway and let the OS reap the orphan (ls-bridge-message-ordering).
    async fn force_kill_all(&self) {
        /// Timeout for force-kill attempts.
        ///
        /// This is deliberately short since we're already past the global timeout.
        /// 3 seconds allows for:
        /// - Writer task to respond to stop signal (~100ms typical)
        /// - SIGTERM -> SIGKILL escalation on Unix (2s grace)
        /// - Some buffer for slow systems
        const FORCE_KILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        // Collect handles to force-kill (minimize lock duration - no logging inside lock)
        let handles_with_info: Vec<(
            super::ConnectionKey,
            ConnectionState,
            Arc<super::ConnectionHandle>,
        )> = {
            let connections = self.connections.lock().await;
            connections
                .iter()
                .filter_map(|(key, handle)| {
                    let state = handle.state();
                    if state != ConnectionState::Closed {
                        Some((key.clone(), state, Arc::clone(handle)))
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Force-kill all connections in parallel.
        // Using JoinSet for parallel execution ensures O(1) force-kill time for N connections.
        let mut join_set = tokio::task::JoinSet::new();
        for (key, state, handle) in handles_with_info {
            log::debug!(
                target: "kakehashi::bridge",
                "Force-killing {} connection (state: {:?})",
                key,
                state
            );
            join_set.spawn(async move {
                // Spawn graceful_shutdown in a separate task to contain potential panics.
                // This ensures complete_shutdown() is always called on timeout OR panic.
                let handle_for_shutdown = Arc::clone(&handle);
                let shutdown_task =
                    tokio::spawn(async move { handle_for_shutdown.graceful_shutdown().await });
                let shutdown_abort = shutdown_task.abort_handle();

                match tokio::time::timeout(FORCE_KILL_TIMEOUT, shutdown_task).await {
                    Ok(Ok(_)) => {
                        // Graceful shutdown completed successfully.
                        // graceful_shutdown() calls complete_shutdown() internally.
                    }
                    Ok(Err(join_error)) => {
                        // The graceful_shutdown task panicked.
                        log::error!(
                            target: "kakehashi::bridge",
                            "Panic during force-kill for {} connection, marking as closed: {}",
                            key,
                            join_error
                        );
                        handle.complete_shutdown();
                    }
                    Err(_) => {
                        // The graceful_shutdown task timed out. Abort it:
                        // graceful_shutdown has no internal timeout (its
                        // response wait is unbounded by design), so dropping
                        // the JoinHandle alone would leave it running detached
                        // — still holding the reclaimed writer and thus the
                        // child process — until runtime shutdown. Aborting
                        // drops the future and the writer's Drop kills the
                        // child, same as abort_all() in the first phase.
                        shutdown_abort.abort();
                        log::warn!(
                            target: "kakehashi::bridge",
                            "Force-kill timeout for {} connection, marking as closed",
                            key
                        );
                        handle.complete_shutdown();
                    }
                }
            });
        }

        // Wait for all force-kills to complete
        Self::drain_join_set(&mut join_set, "Force-kill task").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::lsp::bridge::pool::test_helpers::{create_handle_with_state_and_pid, process_stat};
    #[cfg(unix)]
    use std::time::Duration;

    /// A sink server never answers the shutdown request, so its
    /// graceful_shutdown blocks forever on the response wait and force-kill
    /// hits the timeout arm. That arm must abort the still-running shutdown
    /// task: aborting drops the reclaimed writer, whose Drop kills the child.
    /// Without the abort the task lingers detached, the writer stays alive,
    /// and the child survives until process exit.
    ///
    /// Unix-only: probes child liveness via `ps`, like the other shutdown
    /// lifecycle tests.
    #[cfg(unix)]
    #[tokio::test]
    async fn force_kill_timeout_aborts_shutdown_task_and_kills_child() {
        let (handle, pid) = create_handle_with_state_and_pid(ConnectionState::Ready).await;
        let pool = LanguageServerPool::new();
        pool.connections
            .lock()
            .await
            .insert(super::super::ConnectionKey::for_server("test"), handle);

        pool.force_kill_all().await;

        // Dead means reaped (None) or zombie (Z…); allow a short grace for
        // the kill to land after the abort.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match process_stat(pid) {
                Err(e) => {
                    eprintln!("Skipping child-liveness assertion: ps unavailable ({e})");
                    return;
                }
                Ok(None) => break,
                Ok(Some(stat)) if stat.starts_with('Z') => break,
                Ok(Some(stat)) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "sink child {pid} still running (stat {stat}) after force_kill_all"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}
