//! Single-writer actor that drains the order queue into downstream stdin.
//!
//! Graceful shutdown is a 3-phase protocol: caller signals `stop_tx`; writer
//! drains the queue and signals `idle_tx`; writer then sends itself on a
//! dedicated `writer_tx` so the caller can perform the LSP `shutdown`/`exit`
//! sequence with the returned writer in hand.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::outbound_message::OutboundMessage;
use crate::lsp::bridge::connection::SplitConnectionWriter;

use super::ResponseRouter;

/// Queue capacity for outbound messages per ls-bridge-message-ordering.
///
/// This bounds memory usage and provides backpressure. With 256 slots and
/// typical message sizes, this uses approximately 32KB per connection.
pub(crate) const OUTBOUND_QUEUE_CAPACITY: usize = 256;

/// Handle to a running Writer Task, managing its lifetime via RAII.
///
/// Drives the 3-phase graceful shutdown protocol (stop → idle → reclaim writer)
/// via the oneshot channels below.
pub(crate) struct WriterTaskHandle {
    /// Held to prevent the task from becoming fully detached.
    ///
    /// Shutdown is coordinated via the oneshot channels, not by awaiting this
    /// handle; keeping it just guards against accidental detachment if refactored.
    _join_handle: tokio::task::JoinHandle<()>,
    /// For signaling graceful stop
    stop_tx: Option<oneshot::Sender<()>>,
    /// For receiving idle confirmation
    idle_rx: Option<oneshot::Receiver<()>>,
    /// For receiving writer back (independent of panic state)
    writer_rx: Option<oneshot::Receiver<SplitConnectionWriter>>,
    /// For coordinating with reader task
    cancel_token: CancellationToken,
}

impl WriterTaskHandle {
    /// Initiate graceful shutdown and wait to reclaim the writer.
    ///
    /// Runs the 3-phase protocol (stop → await idle/queue-drained → reclaim
    /// writer). Returns `None` if the writer task panicked or the channels were
    /// already consumed.
    pub(crate) async fn stop_and_reclaim(&mut self) -> Option<SplitConnectionWriter> {
        // Phase 1: Send stop signal
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }

        // Phase 2: Wait for idle confirmation
        if let Some(idle_rx) = self.idle_rx.take() {
            // If this fails, writer task exited abnormally
            let _ = idle_rx.await;
        }

        // Phase 3: Receive writer back
        if let Some(writer_rx) = self.writer_rx.take() {
            writer_rx.await.ok()
        } else {
            None
        }
    }

    /// Cancel the writer task without graceful shutdown.
    ///
    /// Used when we need to force-stop the connection (e.g., on reader error).
    /// Does not wait for queue drain or writer return.
    #[cfg(test)]
    fn cancel(&self) {
        self.cancel_token.cancel();
    }
}

impl Drop for WriterTaskHandle {
    fn drop(&mut self) {
        // Ensure writer task is cancelled if handle is dropped without explicit shutdown
        self.cancel_token.cancel();
    }
}

/// Spawn a writer task that writes messages from the queue to stdin.
pub(crate) fn spawn_writer_task(
    writer: SplitConnectionWriter,
    rx: mpsc::Receiver<OutboundMessage>,
    router: Arc<ResponseRouter>,
) -> WriterTaskHandle {
    let cancel_token = CancellationToken::new();
    let token_clone = cancel_token.clone();

    // Create channels for 3-phase shutdown protocol
    let (stop_tx, stop_rx) = oneshot::channel();
    let (idle_tx, idle_rx) = oneshot::channel();
    let (writer_tx, writer_rx) = oneshot::channel();

    let join_handle = tokio::spawn(async move {
        writer_loop(writer, rx, router, token_clone, stop_rx, idle_tx, writer_tx).await;
    });

    WriterTaskHandle {
        _join_handle: join_handle,
        stop_tx: Some(stop_tx),
        idle_rx: Some(idle_rx),
        writer_rx: Some(writer_rx),
        cancel_token,
    }
}

/// Fail every request still sitting in the outbound queue.
fn fail_queued(rx: &mut mpsc::Receiver<OutboundMessage>, router: &ResponseRouter, reason: &str) {
    while let Ok(msg) = rx.try_recv() {
        if let OutboundMessage::Tracked { request_id, .. } = msg {
            router.fail_request(request_id, reason);
        }
    }
}

/// The main writer loop - writes messages from queue to stdin.
///
/// Handles four shutdown paths: graceful stop drains the queue and returns the
/// writer; a dropped stop sender (handle dropped without stop_and_reclaim)
/// fails queued requests and exits; cancellation drains and fails pending
/// requests without returning the writer; channel close fails all pending and
/// exits.
async fn writer_loop(
    mut writer: SplitConnectionWriter,
    mut rx: mpsc::Receiver<OutboundMessage>,
    router: Arc<ResponseRouter>,
    cancel_token: CancellationToken,
    mut stop_rx: oneshot::Receiver<()>,
    idle_tx: oneshot::Sender<()>,
    writer_tx: oneshot::Sender<SplitConnectionWriter>,
) {
    loop {
        tokio::select! {
            biased;

            // Priority 1: Check for graceful stop signal
            result = &mut stop_rx => {
                if result.is_ok() {
                    log::debug!(
                        target: "kakehashi::bridge::writer",
                        "Writer task received stop signal, draining queue"
                    );
                    // Drain remaining messages (best-effort write)
                    while let Ok(msg) = rx.try_recv() {
                        match write_or_cancelled(&mut writer, &msg, &cancel_token).await {
                            None => {
                                log::debug!(
                                    target: "kakehashi::bridge::writer",
                                    "Writer task cancelled during shutdown drain"
                                );
                                if let OutboundMessage::Tracked { request_id, .. } = msg {
                                    router.fail_request(request_id, "connection closing");
                                }
                                fail_queued(&mut rx, &router, "connection closing");
                                // Don't send idle/writer: dropping the writer
                                // kills the hung child.
                                return;
                            }
                            Some(Err(e)) => {
                                log::warn!(
                                    target: "kakehashi::bridge::writer",
                                    "Write error during drain: {}",
                                    e
                                );
                                // Fail the request if write failed
                                if let OutboundMessage::Tracked { request_id, .. } = msg {
                                    router.fail_request(request_id, "write error during shutdown");
                                }
                            }
                            Some(Ok(())) => {}
                        }
                    }
                    // Signal idle (queue drained)
                    let _ = idle_tx.send(());
                    // Return writer for LSP shutdown sequence
                    let _ = writer_tx.send(writer);
                    return;
                }
                // Err: stop sender dropped without a stop signal, i.e. the
                // WriterTaskHandle was dropped without stop_and_reclaim().
                // The completed oneshot must not be polled again (it panics),
                // so treat this as forced shutdown like the cancellation arm.
                log::debug!(
                    target: "kakehashi::bridge::writer",
                    "Writer stop channel closed, shutting down"
                );
                fail_queued(&mut rx, &router, "connection closing");
                return;
            }

            // Priority 2: Check for cancellation
            _ = cancel_token.cancelled() => {
                log::debug!(
                    target: "kakehashi::bridge::writer",
                    "Writer task cancelled, shutting down"
                );
                // Drain remaining queued requests and fail them
                fail_queued(&mut rx, &router, "connection closing");
                // Don't send idle/writer - caller is force-cancelling
                return;
            }

            // Priority 3: Process messages from queue
            msg = rx.recv() => {
                match msg {
                    Some(outbound) => {
                        match write_or_cancelled(&mut writer, &outbound, &cancel_token).await {
                            None => {
                                log::debug!(
                                    target: "kakehashi::bridge::writer",
                                    "Writer task cancelled during write, shutting down"
                                );
                                if let OutboundMessage::Tracked { request_id, .. } = &outbound {
                                    router.fail_request(*request_id, "connection closing");
                                }
                                fail_queued(&mut rx, &router, "connection closing");
                                // Exiting drops the writer, whose Drop kills
                                // the hung child.
                                return;
                            }
                            Some(Err(e)) => {
                                log::warn!(
                                    target: "kakehashi::bridge::writer",
                                    "Write error: {}, failing request",
                                    e
                                );
                                // Clean up request from router if write failed
                                if let OutboundMessage::Tracked { request_id, .. } = &outbound {
                                    router.fail_request(*request_id, "write error");
                                }
                                // Note: Connection will transition to Failed via reader task
                                // when it detects the write error (broken pipe, etc.)
                            }
                            Some(Ok(())) => {}
                        }
                    }
                    None => {
                        // Channel closed - all senders dropped
                        log::debug!(
                            target: "kakehashi::bridge::writer",
                            "Writer channel closed, cleaning up router entries"
                        );
                        // Fail any requests that were queued but not yet written
                        router.fail_all("bridge: writer channel closed");
                        return;
                    }
                }
            }
        }
    }
}

/// Race a write against the cancel token, returning `None` when cancelled.
///
/// A hung child that stops reading stdin leaves the kernel pipe buffer full
/// and the write parked forever; the writer loop's select arms cannot preempt
/// a write that already began, so the write itself must observe cancellation
/// or a cancelled writer task leaks itself, the writer, and the child.
/// Cancelling mid-frame may leave a partial message in the pipe — acceptable
/// because every cancellation path tears the connection down.
async fn write_or_cancelled(
    writer: &mut SplitConnectionWriter,
    msg: &OutboundMessage,
    cancel_token: &CancellationToken,
) -> Option<std::io::Result<()>> {
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => None,
        result = write_message(writer, msg) => Some(result),
    }
}

/// Write a single outbound message to the downstream server.
async fn write_message(
    writer: &mut SplitConnectionWriter,
    msg: &OutboundMessage,
) -> std::io::Result<()> {
    match msg {
        OutboundMessage::Untracked(payload) => writer.write_message(payload).await,
        OutboundMessage::Tracked { payload, .. } => writer.write_message(payload).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::connection::AsyncBridgeConnection;
    use crate::lsp::bridge::protocol::RequestId;
    use serde_json::json;
    use std::time::Duration;

    /// A child that never reads stdin leaves the kernel pipe buffer full and
    /// the in-flight write parked forever. The select arms can't preempt a
    /// write that already began, so the write itself must race the cancel
    /// token; exiting drops the writer, whose Drop kills the child. Without
    /// that race a cancelled writer task leaks itself, the writer, and the
    /// child process.
    #[tokio::test]
    async fn cancel_reclaims_writer_blocked_on_full_stdin_pipe() {
        use crate::lsp::bridge::pool::test_helpers::process_stat;

        let mut conn = AsyncBridgeConnection::spawn(vec!["sleep".to_string(), "30".to_string()])
            .await
            .expect("should spawn sleep process");
        let (writer, _reader) = conn.split();
        let pid = writer.child_id().expect("child should have a pid");
        let router = Arc::new(ResponseRouter::new());
        let (tx, rx) = mpsc::channel(16);
        let handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        // Far larger than the kernel pipe buffer so the write parks mid-frame.
        let huge = json!({"method": "blocked", "params": {"data": "x".repeat(4 * 1024 * 1024)}});
        tx.send(OutboundMessage::Untracked(huge)).await.unwrap();

        // Let the writer task pick the message up and block on the full pipe.
        tokio::time::sleep(Duration::from_millis(200)).await;

        handle.cancel();

        // Dead means reaped (None) or zombie (Z…).
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
                        "child {pid} still alive (stat {stat}): cancelled writer task \
                         failed to exit while blocked on a full stdin pipe"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Test that writer task maintains FIFO order.
    #[tokio::test]
    async fn writer_loop_maintains_fifo_order() {
        // Create an echo server (cat echoes everything back)
        let mut conn = AsyncBridgeConnection::spawn(vec!["cat".to_string()])
            .await
            .expect("should spawn cat process");

        let (writer, _reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let (tx, rx) = mpsc::channel(16);

        let _handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        // Queue messages in order
        let msg1 = json!({"id": 1, "method": "test1"});
        let msg2 = json!({"id": 2, "method": "test2"});
        let msg3 = json!({"id": 3, "method": "test3"});

        tx.send(OutboundMessage::Untracked(msg1.clone()))
            .await
            .unwrap();
        tx.send(OutboundMessage::Untracked(msg2.clone()))
            .await
            .unwrap();
        tx.send(OutboundMessage::Untracked(msg3.clone()))
            .await
            .unwrap();

        // Give writer time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Note: We can't easily verify order without a mock writer, but this
        // ensures the basic flow works. Integration tests verify actual order.
    }

    /// Test that writer task handles graceful shutdown.
    #[tokio::test]
    async fn writer_task_graceful_shutdown() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, _reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let (tx, rx) = mpsc::channel(16);

        let mut handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        // Send a notification
        tx.send(OutboundMessage::Untracked(json!({"method": "test"})))
            .await
            .unwrap();

        // Give writer time to process
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Gracefully stop and reclaim writer
        let reclaimed = handle.stop_and_reclaim().await;
        assert!(
            reclaimed.is_some(),
            "Should reclaim writer on graceful shutdown"
        );
    }

    /// Dropping WriterTaskHandle without stop_and_reclaim() drops stop_tx,
    /// completing stop_rx with Err. The loop must exit on that branch: a
    /// completed oneshot panics if polled again on the next select! iteration.
    #[tokio::test]
    async fn writer_loop_exits_cleanly_when_stop_sender_dropped() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, _reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        // Keep tx alive so the loop cannot exit via the channel-closed path.
        let (_tx, rx) = mpsc::channel(16);

        let cancel_token = CancellationToken::new();
        let (stop_tx, stop_rx) = oneshot::channel();
        let (idle_tx, _idle_rx) = oneshot::channel();
        let (writer_tx, _writer_rx) = oneshot::channel();

        let handle = tokio::spawn(writer_loop(
            writer,
            rx,
            Arc::clone(&router),
            cancel_token,
            stop_rx,
            idle_tx,
            writer_tx,
        ));
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Simulate handle drop without graceful shutdown: sender dropped
        // without sending.
        drop(stop_tx);

        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        let join = result.expect("writer_loop should exit promptly after stop sender drop");
        assert!(
            join.is_ok(),
            "writer_loop must not panic when stop sender is dropped: {:?}",
            join.err()
        );
    }

    /// Test that writer task cancellation fails pending requests.
    #[tokio::test]
    async fn writer_task_cancel_fails_pending() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, _reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let (tx, rx) = mpsc::channel(16);

        let handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        // Register a request with the router
        let request_id = RequestId::new(42);
        let response_rx = router.register(request_id).expect("should register");

        // Queue the request but cancel before it's processed
        tx.send(OutboundMessage::Tracked {
            payload: json!({"id": 42, "method": "test"}),
            request_id,
        })
        .await
        .unwrap();

        // Cancel immediately
        handle.cancel();

        // Give the writer task time to process the cancellation
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The pending request should receive an error response
        let response = tokio::time::timeout(Duration::from_millis(100), response_rx)
            .await
            .expect("should not timeout")
            .expect("should receive response");

        assert!(
            response.get("error").is_some(),
            "Should receive error response: {:?}",
            response
        );
    }

    /// Test that channel close fails all pending.
    #[tokio::test]
    async fn writer_channel_close_fails_all() {
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ])
        .await
        .expect("should spawn process");

        let (writer, _reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let (tx, rx) = mpsc::channel(16);

        let _handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        // Register a request
        let request_id = RequestId::new(99);
        let response_rx = router.register(request_id).expect("should register");

        // Drop the sender to close the channel
        drop(tx);

        // The pending request should receive an error response
        let response = tokio::time::timeout(Duration::from_millis(100), response_rx)
            .await
            .expect("should not timeout")
            .expect("should receive response");

        assert!(
            response.get("error").is_some(),
            "Should receive error on channel close: {:?}",
            response
        );
    }

    /// Test that write errors during normal operation fail the pending request.
    ///
    /// When a write to stdin fails (e.g., broken pipe because process exited),
    /// the writer task should:
    /// 1. Log the error at WARN level
    /// 2. Call router.fail_request() to deliver error response to waiter
    ///
    /// This test spawns a process that exits immediately, causing writes to fail.
    #[tokio::test]
    async fn writer_write_error_fails_request() {
        // Spawn a process that exits immediately - subsequent writes will fail
        let mut conn = AsyncBridgeConnection::spawn(vec![
            "sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(), // Exit immediately
        ])
        .await
        .expect("should spawn process");

        let (writer, _reader) = conn.split();
        let router = Arc::new(ResponseRouter::new());
        let (tx, rx) = mpsc::channel(16);

        let _handle = spawn_writer_task(writer, rx, Arc::clone(&router));

        // Give the process time to exit
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Register a request with the router
        let request_id = RequestId::new(123);
        let response_rx = router.register(request_id).expect("should register");

        // Queue the request - the write should fail (broken pipe)
        tx.send(OutboundMessage::Tracked {
            payload: json!({"id": 123, "method": "test"}),
            request_id,
        })
        .await
        .unwrap();

        // The pending request should receive an error response from fail_request
        let response = tokio::time::timeout(Duration::from_millis(200), response_rx)
            .await
            .expect("should not timeout")
            .expect("should receive response");

        assert!(
            response.get("error").is_some(),
            "Should receive error response on write failure: {:?}",
            response
        );

        // Verify the error message mentions write error
        let error_msg = response["error"]["message"]
            .as_str()
            .expect("should have error message");
        assert!(
            error_msg.contains("write error"),
            "Error should mention write error: {}",
            error_msg
        );
    }
}
