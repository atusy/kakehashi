//! Async connection to downstream language server processes via stdio.
//!
//! Reader and writer are separated so the reader can move to a dedicated
//! Reader Task (ls-bridge-message-ordering) for non-blocking response routing.

use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Writer handle for sending LSP messages to downstream language server.
///
/// Wraps `ChildStdin` to provide LSP message framing (Content-Length header).
pub(crate) struct BridgeWriter {
    stdin: ChildStdin,
}

impl BridgeWriter {
    /// Write a JSON-RPC message to the downstream language server.
    ///
    /// Formats with the LSP `Content-Length: <length>\r\n\r\n<json>` framing.
    pub(crate) async fn write_message(
        &mut self,
        message: &impl serde::Serialize,
    ) -> io::Result<()> {
        let body = serde_json::to_string(message)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(body.as_bytes()).await?;
        self.stdin.flush().await?;

        Ok(())
    }
}

/// Reader handle for receiving LSP messages from downstream language server.
///
/// Wraps `BufReader<ChildStdout>` to provide LSP message parsing. Used by the
/// Reader Task (ls-bridge-message-ordering) for non-blocking response routing via ResponseRouter.
pub(crate) struct BridgeReader {
    stdout: BufReader<ChildStdout>,
}

impl BridgeReader {
    /// Create a new BridgeReader from a ChildStdout.
    pub(crate) fn new(stdout: ChildStdout) -> Self {
        Self {
            stdout: BufReader::new(stdout),
        }
    }
}

impl BridgeReader {
    /// Read the raw bytes of an LSP message body from stdout.
    ///
    /// Parses headers until empty line, extracts Content-Length, and returns the body bytes.
    /// Handles multiple headers and different header orders per LSP spec.
    async fn read_message_bytes(&mut self) -> io::Result<Vec<u8>> {
        use tokio::io::AsyncReadExt;

        let mut content_length: Option<usize> = None;

        // Read headers until empty line
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).await?;

            // Trim CRLF/LF endings
            let trimmed = line.trim_end_matches(['\r', '\n']);

            if trimmed.is_empty() {
                break; // Empty line = end of headers
            }

            if let Some(value) = trimmed.strip_prefix("Content-Length: ") {
                content_length = Some(value.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length value")
                })?);
            }
            // Other headers (Content-Type, etc.) are silently ignored per LSP spec
        }

        let content_length = content_length.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
        })?;

        // Read exact body bytes
        let mut body = vec![0u8; content_length];
        self.stdout.read_exact(&mut body).await?;

        Ok(body)
    }

    /// Read and parse a JSON-RPC message from the downstream language server.
    ///
    /// Parses the Content-Length header and reads the JSON body.
    pub(crate) async fn read_message(&mut self) -> io::Result<serde_json::Value> {
        let body = self.read_message_bytes().await?;
        serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Async connection to a downstream language server process.
///
/// `split()` separates the writer (stays for serialized request sending) from
/// the reader (moves to a dedicated Reader Task for non-blocking response
/// routing) after initialization (ls-bridge-message-ordering).
pub(crate) struct AsyncBridgeConnection {
    child: Option<Child>,         // Option to support taking for split()
    writer: Option<BridgeWriter>, // Option to support taking for split()
    reader: Option<BridgeReader>, // Option to support taking for Reader Task
}

/// Writer half of a split connection.
///
/// Owns the child process and writer. Dropping this kills the child process.
pub(crate) struct SplitConnectionWriter {
    child: Child,
    writer: BridgeWriter,
}

impl SplitConnectionWriter {
    /// Write a JSON-RPC message to the child process stdin.
    pub(crate) async fn write_message(
        &mut self,
        message: &impl serde::Serialize,
    ) -> io::Result<()> {
        self.writer.write_message(message).await
    }

    /// Force-kill the child process with platform-appropriate escalation.
    ///
    /// Unix escalates SIGTERM→SIGKILL with a 2s grace period; Windows has no
    /// SIGTERM equivalent so it terminates immediately via `start_kill()` and
    /// relies on the LSP shutdown/exit handshake for cleanup (ls-bridge-graceful-shutdown).
    pub(crate) async fn force_kill_with_escalation(&mut self) {
        #[cfg(unix)]
        {
            self.force_kill_with_escalation_unix().await;
        }

        #[cfg(not(unix))]
        {
            self.force_kill_with_escalation_general().await;
        }
    }

    /// Unix-specific force-kill with SIGTERM→SIGKILL escalation and a 2s grace period.
    #[cfg(unix)]
    async fn force_kill_with_escalation_unix(&mut self) {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        use std::time::Duration;

        const SIGTERM_WAIT: Duration = Duration::from_secs(2);

        let Some(pid) = self.child.id() else {
            log::debug!(
                target: "kakehashi::bridge",
                "force_kill_with_escalation: child process already exited"
            );
            return;
        };

        let nix_pid = Pid::from_raw(pid as i32);

        // Step 1: Send SIGTERM
        log::debug!(
            target: "kakehashi::bridge",
            "Sending SIGTERM to process {}",
            pid
        );

        if let Err(e) = kill(nix_pid, Signal::SIGTERM) {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to send SIGTERM to process {}: {}",
                pid, e
            );
            // If SIGTERM fails, try SIGKILL directly via start_kill()
            if let Err(kill_err) = self.child.start_kill() {
                log::error!(
                    target: "kakehashi::bridge",
                    "Failed to send SIGTERM to process {}, and fallback SIGKILL also failed: {}",
                    pid, kill_err
                );
            } else {
                // Wait for process to be reaped after fallback SIGKILL
                match self.child.wait().await {
                    Ok(status) => {
                        log::debug!(
                            target: "kakehashi::bridge",
                            "Process {} terminated after fallback SIGKILL with status: {}",
                            pid, status
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            target: "kakehashi::bridge",
                            "Error waiting for process {} after fallback SIGKILL: {}",
                            pid, e
                        );
                    }
                }
            }
            return;
        }

        // Step 2: Wait for process to exit with timeout
        let wait_result = tokio::time::timeout(SIGTERM_WAIT, self.child.wait()).await;

        match wait_result {
            Ok(Ok(status)) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Process {} terminated after SIGTERM with status: {}",
                    pid, status
                );
                return;
            }
            Ok(Err(e)) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Error waiting for process {} after SIGTERM: {}",
                    pid, e
                );
            }
            Err(_) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Process {} did not respond to SIGTERM within {:?}, escalating to SIGKILL",
                    pid, SIGTERM_WAIT
                );
            }
        }

        // Step 3: Send SIGKILL
        log::debug!(
            target: "kakehashi::bridge",
            "Sending SIGKILL to process {}",
            pid
        );

        if let Err(e) = kill(nix_pid, Signal::SIGKILL) {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to send SIGKILL to process {}: {}",
                pid, e
            );
        }

        // Wait for process to be reaped after SIGKILL
        match self.child.wait().await {
            Ok(status) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Process {} terminated after SIGKILL with status: {}",
                    pid, status
                );
            }
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Error waiting for process {} after SIGKILL: {}",
                    pid, e
                );
            }
        }
    }

    /// General (non-Unix) force-kill via direct process termination.
    ///
    /// No graceful period: these platforms lack a SIGTERM equivalent.
    #[cfg(not(unix))]
    async fn force_kill_with_escalation_general(&mut self) {
        let Some(pid) = self.child.id() else {
            log::debug!(
                target: "kakehashi::bridge",
                "force_kill_with_escalation: child process already exited"
            );
            return;
        };

        log::debug!(
            target: "kakehashi::bridge",
            "Terminating process {} (direct termination, no grace period)",
            pid
        );

        if let Err(e) = self.child.start_kill() {
            log::error!(
                target: "kakehashi::bridge",
                "Failed to terminate process {}: {}",
                pid, e
            );
            return;
        }

        // Wait for process to be reaped
        match self.child.wait().await {
            Ok(status) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Process {} terminated with status: {}",
                    pid, status
                );
            }
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Error waiting for process {} after termination: {}",
                    pid, e
                );
            }
        }
    }
}

impl Drop for SplitConnectionWriter {
    fn drop(&mut self) {
        // Kill the child process to prevent orphans.
        if let Err(e) = self.child.start_kill() {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to kill child process: {}",
                e
            );
        } else {
            log::debug!(
                target: "kakehashi::bridge",
                "Killed child process {:?}",
                self.child.id()
            );
        }
    }
}

impl AsyncBridgeConnection {
    /// Spawn a new language server process with stdio pipes connected.
    pub(crate) async fn spawn(cmd: Vec<String>) -> io::Result<Self> {
        let (program, args) = cmd.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "command must not be empty")
        })?;

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("bridge: failed to capture stdin"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("bridge: failed to capture stdout"))?;

        Ok(Self {
            child: Some(child),
            writer: Some(BridgeWriter { stdin }),
            reader: Some(BridgeReader::new(stdout)),
        })
    }

    /// Split into a `SplitConnectionWriter` (holds the child process) and a
    /// `BridgeReader` (goes to the Reader Task).
    ///
    /// Panics if called more than once (components already taken).
    pub(crate) fn split(&mut self) -> (SplitConnectionWriter, BridgeReader) {
        let reader = self
            .reader
            .take()
            .expect("split() called after reader was already taken");

        let child = self
            .child
            .take()
            .expect("split() called after child was already taken");

        let writer_inner = self
            .writer
            .take()
            .expect("split() called after writer was already taken");

        let writer = SplitConnectionWriter {
            child,
            writer: writer_inner,
        };

        (writer, reader)
    }

    /// Write a JSON-RPC message to the child process stdin.
    ///
    /// Delegates to internal `BridgeWriter`.
    /// Returns error if the writer has been taken (via split()).
    #[cfg(test)]
    pub(crate) async fn write_message(&mut self, message: &serde_json::Value) -> io::Result<()> {
        match &mut self.writer {
            Some(writer) => writer.write_message(message).await,
            None => Err(io::Error::other("bridge: writer has been taken")),
        }
    }

    /// Read and parse a JSON-RPC message from the child process stdout.
    ///
    /// Delegates to internal `BridgeReader`.
    /// Returns None if the reader has been taken (via split()).
    #[cfg(test)]
    pub(crate) async fn read_message(&mut self) -> io::Result<serde_json::Value> {
        match &mut self.reader {
            Some(reader) => reader.read_message().await,
            None => Err(io::Error::other("bridge: reader has been taken")),
        }
    }
}

impl Drop for AsyncBridgeConnection {
    fn drop(&mut self) {
        // Kill the child process to prevent orphans (None if split() was called).
        if let Some(ref mut child) = self.child {
            if let Err(e) = child.start_kill() {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Failed to kill child process: {}",
                    e
                );
            } else {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Killed child process {:?}",
                    child.id()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_creates_child_process_with_stdio() {
        // Use `cat` as a simple test process that echoes stdin to stdout
        let cmd = vec!["cat".to_string()];
        let _conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        // If spawn succeeded, we have a valid connection
    }

    #[tokio::test]
    async fn read_message_parses_content_length_and_body() {
        use serde_json::json;

        // Use `cat` to echo what we write back to us
        let cmd = vec!["cat".to_string()];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        // Write a JSON-RPC response message
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "capabilities": {} }
        });

        conn.write_message(&response)
            .await
            .expect("write should succeed");

        // Read it back using the reader task's parsing logic
        let parsed = conn.read_message().await.expect("read should succeed");

        // Verify the parsed message matches what we sent
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert!(parsed["result"].is_object());
    }

    /// Integration test: Initialize lua-language-server and verify response
    #[tokio::test]
    async fn initialize_lua_language_server() {
        use serde_json::json;

        // Skip test if lua-language-server is not available
        if std::process::Command::new("lua-language-server")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("Skipping test: lua-language-server not found");
            return;
        }

        let cmd = vec!["lua-language-server".to_string()];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("should spawn lua-language-server");

        // Send initialize request
        let init_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {}
            }
        });

        conn.write_message(&init_request)
            .await
            .expect("should write initialize request");

        // Read initialize response (may need to skip notifications)
        let response = loop {
            let msg = conn.read_message().await.expect("should read message");
            if msg.get("id").is_some() {
                break msg;
            }
            // Skip notifications
        };

        // Verify the response indicates successful initialization
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert!(response["result"].is_object(), "should have result object");
        assert!(
            response["result"]["capabilities"].is_object(),
            "should have capabilities"
        );

        // Send initialized notification
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        conn.write_message(&initialized)
            .await
            .expect("should write initialized notification");
    }
}
