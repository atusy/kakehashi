//! LSP client for E2E tests.
//!
//! Provides a simple LSP client that communicates with kakehashi binary
//! via stdin/stdout using JSON-RPC 2.0 protocol.

// These methods are shared across multiple test binaries but not all tests use every method.
// Allow dead_code to suppress per-binary warnings.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Project-local persistent data directory used by every spawned
/// `kakehashi` binary in this test process.
///
/// Mirrors the `cfg(test)` redirection done inside the lib (see
/// `kakehashi::install::default_data_dir`). Lives under `deps/`
/// (already gitignored). Parser/query installs persist across runs to
/// avoid re-downloading.
///
/// The expensive, idempotent setup (dir creation + parser/query install) is
/// cached once per process via `OnceLock`. The transient crash-recovery files
/// (`parsing_in_progress`, `failed_parsers`), however, are cleared on **every**
/// call — i.e. before every client spawn — not just the first: an E2E binary
/// spawns several clients in one process, and a client that shuts down
/// mid-parse leaves those files behind, which would otherwise poison later
/// clients in the same binary.
///
/// We always set `KAKEHASHI_DATA_DIR` on the spawned binary to this
/// path — the binary itself runs without `cfg(test)`, so it cannot
/// auto-redirect like lib unit tests do.
fn test_data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        // Shares the same path and install logic as the lib-side
        // `kakehashi::install::test_data_dir` so unit tests and
        // E2E-spawned binaries reuse one cached parser/query install.
        let dir = kakehashi::install::test_support::test_data_dir_path();
        let _ = std::fs::create_dir_all(&dir);
        let _ = kakehashi::install::test_support::ensure_test_languages_installed(&dir);
        dir
    });
    // Clear crash-recovery state before every spawn, not just the first.
    let _ = std::fs::remove_file(dir.join("parsing_in_progress"));
    let _ = std::fs::remove_file(dir.join("failed_parsers"));
    dir.as_path()
}

/// LSP client for communicating with kakehashi binary.
///
/// Handles JSON-RPC 2.0 message framing with Content-Length headers,
/// request/response matching, and server-initiated notifications.
///
/// Server stdout is drained by a dedicated background thread that frames
/// messages and pushes them onto an `mpsc` channel. Reads therefore become
/// `recv_timeout` with **exact** deadlines — a notification-wait against an
/// alive-but-silent server returns at its timeout instead of blocking on a
/// quiet pipe (see issue #385).
pub struct LspClient {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Framed messages from the background reader thread, in arrival order.
    /// A `Disconnected` recv means the reader thread ended (clean EOF).
    rx: Receiver<ReaderEvent>,
    /// Handle to the background reader thread, joined on drop after the child
    /// is killed (which closes stdout and unblocks the thread).
    reader: Option<JoinHandle<()>>,
    stderr: Option<std::process::ChildStderr>,
    request_id: i64,
}

/// Message pushed from the background reader thread to the client.
///
/// A clean EOF is signaled out-of-band by the thread dropping its [`Sender`],
/// which surfaces to the client as a `Disconnected` recv.
enum ReaderEvent {
    /// A complete, parsed JSON-RPC message.
    Message(Value),
    /// A framing/parse failure with a precise diagnostic. The reader thread
    /// terminates after sending this.
    Error(String),
}

/// Background reader thread body: frame messages off `reader` and forward them
/// over `tx` until EOF, a framing error, or the consumer goes away.
fn reader_loop(mut reader: BufReader<ChildStdout>, tx: Sender<ReaderEvent>) {
    loop {
        match read_framed_message(&mut reader) {
            ReadOutcome::Message(value) => {
                if tx.send(ReaderEvent::Message(value)).is_err() {
                    return; // consumer dropped the receiver
                }
            }
            // Clean EOF: drop tx so the consumer observes `Disconnected`.
            ReadOutcome::Eof => return,
            ReadOutcome::Error(e) => {
                let _ = tx.send(ReaderEvent::Error(e));
                return;
            }
        }
    }
}

/// Builder for creating LspClient instances with custom configuration.
///
/// Supports setting environment variables, removing inherited env vars,
/// and passing CLI arguments to the kakehashi binary.
pub struct LspClientBuilder {
    args: Vec<String>,
    envs: Vec<(String, String)>,
    env_removes: Vec<String>,
}

impl LspClientBuilder {
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            envs: Vec::new(),
            env_removes: Vec::new(),
        }
    }

    /// Add a CLI argument to pass to the kakehashi binary.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Set an environment variable for the spawned process.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// Remove an environment variable from the spawned process.
    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_removes.push(key.into());
        self
    }

    /// Build and spawn the LspClient.
    pub fn build(self) -> LspClient {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kakehashi"));
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Order matters for isolation. Apply removals to the *inherited*
        // environment first, then set the isolation default, then explicit
        // per-test values. This way `.env_remove("KAKEHASHI_DATA_DIR")` (used
        // by tests that exercise the "no env var" config path) drops only the
        // developer's inherited value — the default below still keeps the
        // spawned binary off the real platform data dir. Were removals applied
        // last, they would strip the isolation default and the binary would
        // fall back to the developer/platform data dir.
        for key in &self.env_removes {
            cmd.env_remove(key);
        }
        cmd.env("KAKEHASHI_DATA_DIR", test_data_dir());
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }

        let child = cmd.spawn().expect("Failed to spawn kakehashi binary");
        LspClient::from_child(child)
    }
}

/// Outcome of reading one Content-Length-framed message from the server.
enum ReadOutcome {
    /// A complete, parsed JSON-RPC message.
    Message(Value),
    /// Clean end-of-stream at a message boundary (server closed stdout).
    Eof,
    /// Protocol/framing failure carrying a precise, human-readable diagnostic.
    Error(String),
}

/// Read a single Content-Length-framed JSON-RPC message from `reader`.
///
/// Blocks until a full message is available. Returns [`ReadOutcome::Eof`] only
/// for a clean stream end at a message boundary (the next header read yields 0
/// bytes); every malformed header, oversized/zero `Content-Length`, truncated
/// body, or invalid JSON becomes [`ReadOutcome::Error`] so the caller — or the
/// background reader thread — can surface the exact failure rather than a
/// generic disconnect.
fn read_framed_message(reader: &mut BufReader<ChildStdout>) -> ReadOutcome {
    const MAX_HEADERS: u32 = 100;
    const MAX_MESSAGE_SIZE: usize = 100 * 1024 * 1024; // 100MB limit

    let mut header = String::new();
    let mut header_count = 0u32;

    loop {
        if header_count >= MAX_HEADERS {
            return ReadOutcome::Error(format!(
                "Exceeded maximum header count ({MAX_HEADERS}) - server may be sending malformed headers"
            ));
        }

        header.clear();
        let bytes_read = match reader.read_line(&mut header) {
            Ok(n) => n,
            Err(e) => return ReadOutcome::Error(format!("Failed to read header line: {e}")),
        };

        // EOF while awaiting the next header is a clean shutdown at a message
        // boundary.
        if bytes_read == 0 {
            return ReadOutcome::Eof;
        }
        header_count += 1;

        if header == "\r\n" {
            continue;
        }

        let Some(rest) = header.strip_prefix("Content-Length:") else {
            // Unknown header line; ignore and keep reading (mirrors prior behavior).
            continue;
        };

        let len: usize = match rest.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                return ReadOutcome::Error(format!(
                    "Invalid Content-Length value: {:?}",
                    rest.trim()
                ));
            }
        };

        // Validate Content-Length to prevent excessive allocations.
        if len > MAX_MESSAGE_SIZE {
            return ReadOutcome::Error(format!(
                "Content-Length {len} exceeds maximum allowed size {MAX_MESSAGE_SIZE}"
            ));
        }
        if len == 0 {
            return ReadOutcome::Error(
                "Invalid Content-Length: 0 - message body cannot be empty".to_string(),
            );
        }

        // Read the blank line separating headers from the body.
        let mut empty = String::new();
        let bytes_read = match reader.read_line(&mut empty) {
            Ok(n) => n,
            Err(e) => return ReadOutcome::Error(format!("Failed to read empty line: {e}")),
        };
        if bytes_read == 0 {
            return ReadOutcome::Error(
                "Server closed connection prematurely after header".to_string(),
            );
        }

        let mut body = vec![0u8; len];
        if let Err(e) = std::io::Read::read_exact(reader, &mut body) {
            return ReadOutcome::Error(format!("Failed to read body: {e}"));
        }

        return match serde_json::from_slice(&body) {
            Ok(value) => ReadOutcome::Message(value),
            Err(e) => ReadOutcome::Error(format!("Failed to parse response: {e}")),
        };
    }
}

impl LspClient {
    /// Spawn the kakehashi binary and create a new LSP client.
    pub fn new() -> Self {
        Self::with_debug(false)
    }

    /// Return a builder for configuring the LspClient before spawning.
    pub fn builder() -> LspClientBuilder {
        LspClientBuilder::new()
    }

    /// Spawn the kakehashi binary with optional debug logging.
    pub fn with_debug(debug: bool) -> Self {
        // `CARGO_BIN_EXE_kakehashi` is set by Cargo's test harness for integration tests
        // and points to the built `kakehashi` binary, so we don't hardcode its path here.
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kakehashi"));
        cmd.env("KAKEHASHI_DATA_DIR", test_data_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if debug {
            cmd.env("RUST_LOG", "debug");
        }

        let child = cmd.spawn().expect("Failed to spawn kakehashi binary");
        Self::from_child(child)
    }

    /// Wire up a spawned child: take its stdio handles and start the background
    /// reader thread that drains stdout into the message channel.
    fn from_child(mut child: Child) -> Self {
        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = BufReader::new(child.stdout.take().expect("Failed to get stdout"));
        let stderr = child.stderr.take();

        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || reader_loop(stdout, tx));

        Self {
            child,
            stdin: Some(stdin),
            rx,
            reader: Some(reader),
            stderr,
            request_id: 0,
        }
    }

    /// Read stderr output (single read, up to 4KB).
    ///
    /// Note: This may block if no data is available. Call after the server
    /// has had time to produce output. Useful for debugging server behavior.
    pub fn drain_stderr(&mut self) -> String {
        use std::io::Read;
        let Some(stderr) = self.stderr.as_mut() else {
            return String::new();
        };

        let mut buf = [0u8; 4096];
        match stderr.read(&mut buf) {
            Ok(0) => String::new(),
            Ok(n) => String::from_utf8_lossy(&buf[..n]).to_string(),
            Err(_) => String::new(),
        }
    }

    /// Send an LSP request and return the response.
    pub fn send_request(&mut self, method: &str, params: Value) -> Value {
        self.request_id += 1;
        let request_id = self.request_id;

        // Build request - some methods like "shutdown" don't take params
        let mut request = serde_json::Map::new();
        request.insert("jsonrpc".to_string(), json!("2.0"));
        request.insert("id".to_string(), json!(request_id));
        request.insert("method".to_string(), json!(method));

        // Only add params if it's not null
        if !params.is_null() {
            request.insert("params".to_string(), params);
        }

        self.send_message(&Value::Object(request));
        self.receive_response_for_id(request_id)
    }

    /// Send an LSP notification (no response expected).
    pub fn send_notification(&mut self, method: &str, params: Value) {
        // Build notification - some methods don't take params
        let mut notification = serde_json::Map::new();
        notification.insert("jsonrpc".to_string(), json!("2.0"));
        notification.insert("method".to_string(), json!(method));

        // Only add params if it's not null
        if !params.is_null() {
            notification.insert("params".to_string(), params);
        }

        self.send_message(&Value::Object(notification));
    }

    /// Send a JSON-RPC message with Content-Length header.
    fn send_message(&mut self, message: &Value) {
        let body = serde_json::to_string(message).expect("Failed to serialize message");
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        let stdin = self.stdin.as_mut().expect("stdin already closed");
        stdin
            .write_all(header.as_bytes())
            .expect("Failed to write header");
        stdin
            .write_all(body.as_bytes())
            .expect("Failed to write body");
        stdin.flush().expect("Failed to flush stdin");
    }

    /// Receive an LSP response for a specific request id.
    /// Skips server-initiated notifications and requests until finding matching response.
    /// Times out after 30 seconds or 1000 messages to prevent indefinite hangs.
    fn receive_response_for_id(&mut self, expected_id: i64) -> Value {
        const MAX_MESSAGES: u32 = 1000;
        const TIMEOUT: Duration = Duration::from_secs(30);

        let start_time = Instant::now();
        let mut message_count = 0u32;

        loop {
            // Check timeout
            if start_time.elapsed() > TIMEOUT {
                panic!(
                    "Timeout waiting for response with id {}. Elapsed: {:?}",
                    expected_id,
                    start_time.elapsed()
                );
            }

            // Check message count threshold
            if message_count >= MAX_MESSAGES {
                panic!(
                    "Exceeded maximum message threshold ({}) waiting for response with id {}",
                    MAX_MESSAGES, expected_id
                );
            }

            let message = self.receive_message();
            message_count += 1;

            // Check if this is a response to our request
            if let Some(id) = message.get("id") {
                // Server-to-client requests have "method" field, skip them
                if message.get("method").is_some() {
                    continue;
                }
                // Response should match our request id
                if id.as_i64() == Some(expected_id) {
                    return message;
                }
            }
            // Otherwise it's a notification like window/logMessage, skip it
        }
    }

    /// Receive a single LSP message from the background reader thread.
    /// Times out after 30 seconds to prevent indefinite blocking on an
    /// unresponsive (alive but silent) server.
    fn receive_message(&mut self) -> Value {
        const TIMEOUT: Duration = Duration::from_secs(30);
        match self.rx.recv_timeout(TIMEOUT) {
            Ok(ReaderEvent::Message(value)) => value,
            Ok(ReaderEvent::Error(e)) => panic!("{e}"),
            Err(RecvTimeoutError::Timeout) => {
                panic!("Timeout reading LSP message after {TIMEOUT:?}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("Server closed connection prematurely (reader thread ended at EOF)")
            }
        }
    }

    /// Kill the server process.
    fn kill(&mut self) {
        // If the child has already exited, nothing to do.
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                // Child is still running; try to terminate it.
                let _ = self.child.kill();
            }
            Err(_) => {
                // If we can't query status, still attempt to kill as best effort.
                let _ = self.child.kill();
            }
        }

        // Reap the process to avoid leaving a zombie. Ignore errors in Drop path.
        let _ = self.child.wait();
    }

    /// Close stdin to signal EOF (for shutdown testing).
    pub(crate) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Get the current request ID counter (for cancel testing).
    pub(crate) fn current_request_id(&self) -> i64 {
        self.request_id
    }

    /// Send an LSP request without waiting for response.
    ///
    /// Returns the request ID used. Caller is responsible for receiving the response
    /// via `receive_response_for_id_public()`.
    pub(crate) fn send_request_async(&mut self, method: &str, params: Value) -> i64 {
        self.request_id += 1;
        let request_id = self.request_id;

        let mut request = serde_json::Map::new();
        request.insert("jsonrpc".to_string(), json!("2.0"));
        request.insert("id".to_string(), json!(request_id));
        request.insert("method".to_string(), json!(method));

        if !params.is_null() {
            request.insert("params".to_string(), params);
        }

        self.send_message(&Value::Object(request));
        request_id
    }

    /// Receive response for a specific request ID (public version).
    pub(crate) fn receive_response_for_id_public(&mut self, expected_id: i64) -> Value {
        self.receive_response_for_id(expected_id)
    }

    /// Wait for a notification with a specific method name.
    ///
    /// Returns the notification params if received within the timeout,
    /// or None if timeout occurs without receiving the expected notification.
    /// Skips other notifications and server-to-client requests while waiting.
    pub(crate) fn wait_for_notification(
        &mut self,
        expected_method: &str,
        timeout: Duration,
    ) -> Option<Value> {
        let start_time = Instant::now();

        loop {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            if remaining.is_zero() {
                return None;
            }

            // Block up to the *remaining* deadline. None means the deadline
            // elapsed or the server disconnected — either way, give up.
            let message = self.try_receive_message(remaining)?;

            // Notifications have no "id" field and must match the expected method.
            if message.get("id").is_none()
                && message.get("method").and_then(|m| m.as_str()) == Some(expected_method)
            {
                return message.get("params").cloned();
            }
            // Otherwise it's a response or server-to-client request; skip it
            // and keep waiting within the deadline.
        }
    }

    /// Wait for the first notification whose method is in `methods` AND whose
    /// params satisfy `predicate`, preserving arrival order across methods.
    ///
    /// Returns `(method, params)` for the first match, or None on timeout.
    /// Unlike `wait_for_notification`, this can observe the ORDER of several
    /// notification methods (e.g. prove showMessage arrives before a later
    /// logMessage). Non-matching messages are skipped.
    pub(crate) fn wait_for_notification_where(
        &mut self,
        methods: &[&str],
        timeout: Duration,
        predicate: impl Fn(&Value) -> bool,
    ) -> Option<(String, Value)> {
        let start_time = Instant::now();

        loop {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            if remaining.is_zero() {
                return None;
            }

            // Block up to the *remaining* deadline. None means the deadline
            // elapsed or the server disconnected — either way, give up.
            let message = self.try_receive_message(remaining)?;

            if message.get("id").is_none()
                && let Some(method) = message.get("method").and_then(|m| m.as_str())
                && methods.contains(&method)
            {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                if predicate(&params) {
                    return Some((method.to_string(), params));
                }
            }
        }
    }

    /// Try to receive a message within `timeout`, returning None if none arrives.
    ///
    /// Backed by the background reader thread's channel, so the deadline is
    /// exact: against an alive-but-silent server this blocks for precisely
    /// `timeout` and then returns None, rather than hanging on a quiet pipe.
    /// A disconnect (reader thread ended at EOF) also returns None — no further
    /// messages can arrive. A framing/parse error panics with its diagnostic.
    fn try_receive_message(&mut self, timeout: Duration) -> Option<Value> {
        match self.rx.recv_timeout(timeout) {
            Ok(ReaderEvent::Message(value)) => Some(value),
            Ok(ReaderEvent::Error(e)) => panic!("{e}"),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Check if the server process is still running.
    pub(crate) fn is_running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("Error checking child status")
            .is_none()
    }

    /// Wait for the process to exit with a timeout.
    /// Returns the exit status if the process exited, or None if timeout occurred.
    pub(crate) fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    if start.elapsed() > timeout {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return None,
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Kill first: closing the child's stdout unblocks the reader thread's
        // pending read, so the subsequent join returns promptly.
        self.kill();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsp_client_can_initialize() {
        let mut client = LspClient::new();

        let response = client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {}
            }),
        );

        assert!(response.get("result").is_some());
        assert!(response["result"].get("capabilities").is_some());
    }
}
