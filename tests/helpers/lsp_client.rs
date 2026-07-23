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
/// cached once per process via `OnceLock`. The directory is treated as
/// read-only after install so independent test processes can share it; the
/// transient crash-recovery files that the server would otherwise write here
/// are redirected to a per-spawn [`isolated_state_dir`].
///
/// We always set `KAKEHASHI_DATA_DIR` on the spawned binary to this
/// path — the binary itself runs without `cfg(test)`, so it cannot
/// auto-redirect like lib unit tests do.
fn test_data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        // Shares the same path and install logic as the lib-side
        // `kakehashi::install::test_data_dir` so unit tests and
        // E2E-spawned binaries reuse one cached parser/query install.
        let dir = kakehashi::install::test_support::test_data_dir_path();
        // Fail fast with a clear message on a broken setup (permissions, disk
        // full, corrupt cache) rather than letting every later test fail with a
        // murky downstream error. Matches the CLI helpers' `data_dir()`.
        std::fs::create_dir_all(&dir).expect("create shared test data dir");
        kakehashi::install::test_support::ensure_test_languages_installed(&dir)
            .expect("install test parsers/queries into the shared data dir");
        dir
    })
    .as_path()
}

/// A fresh, unique directory for ONE spawned server's crash-recovery state
/// (`parsing_in_progress`, `failed_parsers`), passed via `KAKEHASHI_STATE_DIR`.
///
/// Kept OUT of the shared [`test_data_dir`] so servers never poison each other:
/// the server's `FailedParserRegistry::init()` reads a leftover
/// `parsing_in_progress` as a crash and marks that parser failed. A per-SPAWN
/// (not per-process) dir is what makes the test suite safe under both
/// cross-process AND intra-process concurrency (parallel test threads in one
/// binary): no two concurrently-running servers ever share these files, so a
/// server that persists state on a shutdown-while-parsing can't be read as a
/// crash by another — and no pre-spawn clearing is needed. The expensive
/// parser/query install stays shared (read-only) in `test_data_dir`.
///
/// All per-spawn dirs live under one base [`tempfile::TempDir`] parked in a
/// `static` and — like [`isolated_config_dir`] — intentionally never dropped
/// (statics aren't), so the small tree is left in the OS temp dir when the
/// process exits (one tree per test process; the same accepted leak).
fn isolated_state_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static BASE: OnceLock<tempfile::TempDir> = OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let base = BASE
        .get_or_init(|| tempfile::tempdir().expect("create base dir for KAKEHASHI_STATE_DIR"))
        .path();
    let dir = base.join(format!("spawn-{}", SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).expect("create per-spawn KAKEHASHI_STATE_DIR");
    dir
}

/// Empty `XDG_CONFIG_HOME` for the spawned server, so it never reads the
/// developer's real `~/.config/kakehashi`. Inherited user config changes the
/// server's behavior under test — e.g. a host-bridging entry flips the
/// willSave/willSaveWaitUntil capability gate and breaks the
/// `initialize_capabilities` snapshot — so isolation must hold regardless of
/// how the test is invoked (`make test_e2e`, a bare `cargo test`, or the
/// pre-commit gate). Doing it here, rather than in the Makefile, is the only
/// place that covers all three.
///
/// The directory is left empty: the server's `$XDG_CONFIG_HOME/kakehashi`
/// lookup simply finds nothing and falls back to built-in defaults.
///
/// Uses a unique per-process [`tempfile::TempDir`] rather than a fixed path so
/// the isolation is deterministic: a fixed shared path could already hold stale
/// contents from a prior or concurrent run (re-introducing the very pollution
/// this guards against) or collide on permissions across users. The `TempDir`
/// is parked in a `static` so it outlives every spawned server for the whole
/// test process; it is intentionally never dropped (statics aren't), so no
/// cleanup can race a still-running server.
fn isolated_config_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().expect("create temp dir for XDG_CONFIG_HOME isolation"))
        .path()
}

/// LSP client for communicating with kakehashi binary.
///
/// Handles JSON-RPC 2.0 message framing with Content-Length headers,
/// request/response matching, and server-initiated notifications.
///
/// Server stdout is drained by a dedicated background thread that frames
/// messages and pushes them onto an `mpsc` channel. Reads therefore become
/// `recv_timeout`, so waits honor their deadline (modulo scheduler jitter)
/// instead of blocking indefinitely — a notification-wait against an
/// alive-but-silent server returns at its timeout rather than hanging on a
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
        cmd.env("XDG_CONFIG_HOME", isolated_config_dir());
        cmd.env("KAKEHASHI_STATE_DIR", isolated_state_dir());
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
/// Reads every header line until the blank separator line, capturing
/// `Content-Length` wherever it appears — so the order of headers and the
/// presence of others (e.g. `Content-Type`) does not matter — then reads
/// exactly that many body bytes. Both `\r\n` and bare `\n` line endings are
/// accepted.
///
/// Blocks until a full message is available. Returns [`ReadOutcome::Eof`] only
/// for a clean stream end at a message boundary (EOF before any header of the
/// next message); a stream that ends mid-message, a malformed/missing
/// `Content-Length`, an oversized/zero length, a truncated body, or invalid
/// JSON all become [`ReadOutcome::Error`] so the caller — or the background
/// reader thread — can surface the exact failure rather than a generic
/// disconnect.
fn read_framed_message<R: BufRead>(reader: &mut R) -> ReadOutcome {
    const MAX_HEADERS: u32 = 100;
    const MAX_MESSAGE_SIZE: usize = 100 * 1024 * 1024; // 100MB limit

    let mut header = String::new();
    let mut header_count = 0u32;
    let mut content_length: Option<usize> = None;

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

        if bytes_read == 0 {
            // EOF. Clean only at a true message boundary — i.e. before any
            // header line of the next message. If we have already consumed one
            // or more header lines (whether or not Content-Length was among
            // them), the stream was truncated mid-message.
            return if header_count == 0 {
                ReadOutcome::Eof
            } else {
                ReadOutcome::Error(
                    "Server closed connection prematurely mid-message (incomplete header block)"
                        .to_string(),
                )
            };
        }
        header_count += 1;

        // Blank line: headers are done, the body follows.
        if header == "\r\n" || header == "\n" {
            let Some(len) = content_length else {
                return ReadOutcome::Error(
                    "Missing Content-Length header before message body".to_string(),
                );
            };

            let mut body = vec![0u8; len];
            if let Err(e) = std::io::Read::read_exact(reader, &mut body) {
                return ReadOutcome::Error(format!("Failed to read body: {e}"));
            }

            return match serde_json::from_slice(&body) {
                Ok(value) => ReadOutcome::Message(value),
                Err(e) => ReadOutcome::Error(format!("Failed to parse JSON-RPC message body: {e}")),
            };
        }

        // LSP header names are case-insensitive (HTTP-style framing), so match
        // the field name accordingly. Lines without a colon, or other headers
        // (e.g. Content-Type), are ignored.
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("Content-Length") {
            continue;
        }

        let len: usize = match value.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                return ReadOutcome::Error(format!(
                    "Invalid Content-Length value: {:?}",
                    value.trim()
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
        content_length = Some(len);
    }
}

/// A server→client request's `(id, params)` plus the watched notifications
/// (method, params) collected while waiting for it — the result of
/// [`LspClient::wait_for_server_request_watching`].
pub(crate) type WatchedServerRequest = (Value, Value, Vec<(String, Value)>);
type TwoWatchedServerRequestResponse = (Value, Vec<(Value, Value)>, Vec<(Value, Value)>);

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
            .env("XDG_CONFIG_HOME", isolated_config_dir())
            .env("KAKEHASHI_STATE_DIR", isolated_state_dir())
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

    /// Shut the server down and collect stderr through EOF.
    ///
    /// Unlike [`Self::drain_stderr`], this cannot block waiting for the first
    /// byte and does not truncate output at 4 KiB. It is intended for tests
    /// that assert logging behavior, including the failure mode where the
    /// expected log is absent.
    pub fn shutdown_and_collect_stderr(&mut self) -> String {
        use std::io::Read;
        use std::time::{Duration, Instant};

        let _ = self.send_request("shutdown", Value::Null);
        self.send_notification("exit", Value::Null);
        self.close_stdin();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    self.kill();
                    break;
                }
            }
        }

        let Some(mut stderr) = self.stderr.take() else {
            return String::new();
        };
        let mut output = String::new();
        let _ = stderr.read_to_string(&mut output);
        output
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
        self.receive_response_for_id_watching_server_requests_inner(expected_id, None, None)
            .0
    }

    fn receive_response_for_id_watching_server_requests_inner(
        &mut self,
        expected_id: i64,
        watched_method: Option<&str>,
        second_watched_method: Option<&str>,
    ) -> TwoWatchedServerRequestResponse {
        const MAX_MESSAGES: u32 = 1000;
        const TIMEOUT: Duration = Duration::from_secs(30);

        let start_time = Instant::now();
        let mut message_count = 0u32;
        let mut watched = Vec::new();
        let mut second_watched = Vec::new();

        loop {
            // Check message count threshold
            if message_count >= MAX_MESSAGES {
                panic!(
                    "Exceeded maximum message threshold ({MAX_MESSAGES}) waiting for response with id {expected_id}"
                );
            }

            // Bound each wait by the *remaining* overall budget so the
            // documented 30s timeout stays accurate even across many
            // intervening notifications — without this, a single blocking read
            // could itself wait a full 30s and overshoot the budget.
            let remaining = TIMEOUT.saturating_sub(start_time.elapsed());
            if remaining.is_zero() {
                panic!(
                    "Timeout waiting for response with id {expected_id}. Elapsed: {:?}",
                    start_time.elapsed()
                );
            }

            let message = match self.rx.recv_timeout(remaining) {
                Ok(ReaderEvent::Message(value)) => value,
                Ok(ReaderEvent::Error(e)) => panic!("{e}"),
                Err(RecvTimeoutError::Timeout) => panic!(
                    "Timeout waiting for response with id {expected_id}. Elapsed: {:?}",
                    start_time.elapsed()
                ),
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "Server closed connection prematurely while waiting for response with id {expected_id}"
                ),
            };
            message_count += 1;

            // Check if this is a response to our request
            if let Some(id) = message.get("id") {
                // Preserve watched server-to-client requests instead of silently
                // discarding them while a synchronous client request is in flight.
                if let Some(method) = message.get("method").and_then(Value::as_str) {
                    if Some(method) == watched_method {
                        watched.push((
                            id.clone(),
                            message.get("params").cloned().unwrap_or(Value::Null),
                        ));
                    } else if Some(method) == second_watched_method {
                        second_watched.push((
                            id.clone(),
                            message.get("params").cloned().unwrap_or(Value::Null),
                        ));
                    }
                    continue;
                }
                // Response should match our request id
                if id.as_i64() == Some(expected_id) {
                    return (message, watched, second_watched);
                }
            }
            // Otherwise it's a notification like window/logMessage, skip it
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

    /// Receive a client-request response while retaining selected notifications
    /// that arrived before it. Useful for initialization tests, where the normal
    /// synchronous response helper intentionally discards notifications.
    pub(crate) fn receive_response_for_id_watching_notifications(
        &mut self,
        expected_id: i64,
        watched_methods: &[&str],
    ) -> (Value, Vec<(String, Value)>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut watched = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for response {expected_id}"
            );
            let message = match self.rx.recv_timeout(remaining) {
                Ok(ReaderEvent::Message(value)) => value,
                Ok(ReaderEvent::Error(error)) => panic!("{error}"),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out waiting for response {expected_id}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("server closed before the expected response {expected_id}")
                }
            };
            if message.get("id").and_then(Value::as_i64) == Some(expected_id)
                && message.get("method").is_none()
            {
                return (message, watched);
            }
            if let (Some(id), Some(method)) = (
                message.get("id").cloned(),
                message.get("method").and_then(Value::as_str),
            ) {
                if method == "window/showMessageRequest" {
                    // This request is explicitly permitted during initialize;
                    // null means the user dismissed it. Reply immediately so
                    // a server awaiting the choice can finish initialization.
                    self.send_response(id, Value::Null);
                    continue;
                }
                panic!(
                    "unexpected server request {method} while waiting for response {expected_id}"
                );
            }
            if message.get("id").is_none()
                && let Some(method) = message.get("method").and_then(Value::as_str)
                && watched_methods.contains(&method)
            {
                watched.push((
                    method.to_string(),
                    message.get("params").cloned().unwrap_or(Value::Null),
                ));
            }
        }
    }

    /// Receive one client-request response while preserving server requests of
    /// `watched_method` that arrived before it. This prevents exact-sequence E2E
    /// assertions from losing concurrent server requests in the ordinary
    /// synchronous response loop.
    pub(crate) fn receive_response_for_id_watching_server_requests(
        &mut self,
        expected_id: i64,
        watched_method: &str,
    ) -> (Value, Vec<(Value, Value)>) {
        let (response, watched, _) = self.receive_response_for_id_watching_server_requests_inner(
            expected_id,
            Some(watched_method),
            None,
        );
        (response, watched)
    }

    /// Receive a response while preserving two independently significant
    /// server-request methods that race with it.
    pub(crate) fn receive_response_for_id_watching_two_server_requests(
        &mut self,
        expected_id: i64,
        watched_method: &str,
        second_watched_method: &str,
    ) -> TwoWatchedServerRequestResponse {
        self.receive_response_for_id_watching_server_requests_inner(
            expected_id,
            Some(watched_method),
            Some(second_watched_method),
        )
    }

    /// Wait for a notification with a specific method name.
    ///
    /// Returns the notification params if received within the timeout, or None
    /// if the timeout elapses (or the server disconnects) without it. Skips
    /// other notifications and server-to-client requests while waiting. Panics
    /// if the stream is malformed (a framing/parse error is surfaced, not
    /// hidden behind the `Option`).
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

    /// Wait for one server request while preserving requests of another method
    /// that precede it on the editor-facing connection.
    pub(crate) fn wait_for_server_request_watching_server_requests(
        &mut self,
        expected_method: &str,
        watched_method: &str,
        timeout: Duration,
    ) -> Option<(Value, Vec<(Value, Value)>)> {
        let start_time = Instant::now();
        let mut watched = Vec::new();

        loop {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            if remaining.is_zero() {
                return None;
            }

            let message = self.try_receive_message(remaining)?;
            let method = message.get("method").and_then(Value::as_str);
            if let Some(id) = message.get("id").cloned() {
                if method == Some(watched_method) {
                    watched.push((id, message.get("params").cloned().unwrap_or(Value::Null)));
                    continue;
                }
                if method == Some(expected_method) {
                    return Some((id, watched));
                }
            }
        }
    }

    /// Wait for the first notification whose method is in `methods` AND whose
    /// params satisfy `predicate`, preserving arrival order across methods.
    ///
    /// Returns `(method, params)` for the first match, or None on timeout (or
    /// server disconnect). Unlike `wait_for_notification`, this can observe the
    /// ORDER of several notification methods (e.g. prove showMessage arrives
    /// before a later logMessage). Non-matching messages are skipped. Panics if
    /// the stream is malformed (a framing/parse error is surfaced, not hidden
    /// behind the `Option`).
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

    /// Wait for the first server→client **request** (a message carrying both an
    /// `id` and a `method`) whose method is `expected_method`, returning its
    /// `(id, params)` (`params` is `Value::Null` for a param-less request like
    /// `workspace/diagnostic/refresh`). Returns None on timeout or disconnect.
    ///
    /// `wait_for_notification*` deliberately skip messages with an `id`, so they
    /// can't observe a server-initiated request; this is the counterpart that does.
    /// The `id` is returned so the caller can ack via [`Self::send_response`] —
    /// needed when the server single-flights on the ack (e.g. the workspace
    /// diagnostic refresh, #497): the next refresh won't fire until this one is
    /// answered.
    pub(crate) fn wait_for_server_request(
        &mut self,
        expected_method: &str,
        timeout: Duration,
    ) -> Option<(Value, Value)> {
        let start_time = Instant::now();

        loop {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            if remaining.is_zero() {
                return None;
            }

            let message = self.try_receive_message(remaining)?;

            // A server→client request carries both an `id` and a `method`. The
            // message is discarded right after, so destructure it and `remove` the
            // fields (one lookup each, moved rather than cloned). A message with no
            // `id` (a notification) fails the inner `remove` and is skipped.
            if let Value::Object(mut map) = message
                && map.get("method").and_then(|m| m.as_str()) == Some(expected_method)
                && let Some(id) = map.remove("id")
            {
                let params = map.remove("params").unwrap_or(Value::Null);
                return Some((id, params));
            }
            // A response or notification, or a different request — skip and keep
            // waiting within the deadline.
        }
    }

    /// Like [`Self::wait_for_server_request`], but also collects every
    /// notification whose method is in `watch` seen while waiting (in arrival
    /// order). Lets a test assert something did NOT precede the awaited request
    /// — the pipe is FIFO, so anything the server enqueued earlier is in the
    /// collected list — without resorting to a flaky negative timeout.
    pub(crate) fn wait_for_server_request_watching(
        &mut self,
        expected_method: &str,
        timeout: Duration,
        watch: &[&str],
    ) -> Option<WatchedServerRequest> {
        let start_time = Instant::now();
        let mut watched = Vec::new();

        loop {
            let remaining = timeout.saturating_sub(start_time.elapsed());
            if remaining.is_zero() {
                return None;
            }

            let message = self.try_receive_message(remaining)?;

            let Value::Object(mut map) = message else {
                continue;
            };
            let method = map.get("method").and_then(|m| m.as_str());
            match method {
                Some(method) if method == expected_method && map.contains_key("id") => {
                    let id = map.remove("id").expect("present: checked above");
                    let params = map.remove("params").unwrap_or(Value::Null);
                    return Some((id, params, watched));
                }
                Some(method) if !map.contains_key("id") && watch.contains(&method) => {
                    watched.push((
                        method.to_string(),
                        map.remove("params").unwrap_or(Value::Null),
                    ));
                }
                _ => {} // other request/response/notification — skip
            }
        }
    }

    /// Send a JSON-RPC success response for a server→client request `id` (e.g. to
    /// ack a `workspace/diagnostic/refresh`).
    pub(crate) fn send_response(&mut self, id: Value, result: Value) {
        self.send_message(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    /// Try to receive a message within `timeout`, returning None if none arrives.
    ///
    /// Backed by the background reader thread's channel, so the deadline is
    /// honored (modulo scheduler jitter): against an alive-but-silent server
    /// this blocks for about `timeout` and then returns None, rather than
    /// hanging on a quiet pipe. A disconnect (reader thread ended at EOF) also
    /// returns None — no further messages can arrive. A framing/parse error
    /// panics with its diagnostic.
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

    /// The canonical framing: a single `Content-Length` header followed by the
    /// blank line and body.
    #[test]
    fn read_framed_message_parses_basic_frame() {
        let mut input = &b"Content-Length: 13\r\n\r\n{\"jsonrpc\":1}"[..];
        match read_framed_message(&mut input) {
            ReadOutcome::Message(v) => assert_eq!(v, json!({"jsonrpc": 1})),
            other => panic!("expected Message, got {:?}", outcome_label(&other)),
        }
    }

    /// Header order is irrelevant and unrelated headers (e.g. `Content-Type`)
    /// must not be mistaken for the blank separator line. This is the case
    /// Gemini flagged: a header after `Content-Length` previously got consumed
    /// as the blank line and corrupted the body read.
    #[test]
    fn read_framed_message_handles_reordered_and_extra_headers() {
        let mut input = &b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
            Content-Length: 13\r\n\
            \r\n\
            {\"jsonrpc\":1}"[..];
        match read_framed_message(&mut input) {
            ReadOutcome::Message(v) => assert_eq!(v, json!({"jsonrpc": 1})),
            other => panic!("expected Message, got {:?}", outcome_label(&other)),
        }
    }

    /// LSP header names are case-insensitive, so `content-length` must parse.
    #[test]
    fn read_framed_message_matches_content_length_case_insensitively() {
        let mut input = &b"content-length: 13\r\n\r\n{\"jsonrpc\":1}"[..];
        match read_framed_message(&mut input) {
            ReadOutcome::Message(v) => assert_eq!(v, json!({"jsonrpc": 1})),
            other => panic!("expected Message, got {:?}", outcome_label(&other)),
        }
    }

    /// Bare `\n` line endings (no `\r`) are accepted as header terminators.
    #[test]
    fn read_framed_message_accepts_lf_only_line_endings() {
        let mut input = &b"Content-Length: 13\n\n{\"jsonrpc\":1}"[..];
        match read_framed_message(&mut input) {
            ReadOutcome::Message(v) => assert_eq!(v, json!({"jsonrpc": 1})),
            other => panic!("expected Message, got {:?}", outcome_label(&other)),
        }
    }

    /// Two messages back to back are framed independently, so the reader thread
    /// can loop over a continuous stream.
    #[test]
    fn read_framed_message_reads_consecutive_frames() {
        let mut input =
            &b"Content-Length: 7\r\n\r\n{\"a\":1}Content-Length: 7\r\n\r\n{\"b\":2}"[..];
        assert!(
            matches!(read_framed_message(&mut input), ReadOutcome::Message(v) if v == json!({"a": 1}))
        );
        assert!(
            matches!(read_framed_message(&mut input), ReadOutcome::Message(v) if v == json!({"b": 2}))
        );
    }

    /// A clean EOF at a message boundary is `Eof`, not an error.
    #[test]
    fn read_framed_message_clean_eof_at_boundary() {
        let mut input = &b""[..];
        assert!(matches!(read_framed_message(&mut input), ReadOutcome::Eof));
    }

    /// EOF after a header but before the body is a truncated stream, not a
    /// clean boundary.
    #[test]
    fn read_framed_message_premature_eof_is_error() {
        let mut input = &b"Content-Length: 99\r\n\r\n"[..];
        assert!(matches!(
            read_framed_message(&mut input),
            ReadOutcome::Error(_)
        ));
    }

    /// EOF after a partial header block that has NOT yet included
    /// Content-Length is still a mid-message truncation, not a clean boundary.
    #[test]
    fn read_framed_message_eof_after_partial_headers_is_error() {
        let mut input = &b"Content-Type: application/vscode-jsonrpc\r\n"[..];
        assert!(matches!(
            read_framed_message(&mut input),
            ReadOutcome::Error(_)
        ));
    }

    fn outcome_label(o: &ReadOutcome) -> &'static str {
        match o {
            ReadOutcome::Message(_) => "Message",
            ReadOutcome::Eof => "Eof",
            ReadOutcome::Error(_) => "Error",
        }
    }
}
