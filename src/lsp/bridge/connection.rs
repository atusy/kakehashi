//! Async connection to downstream language server processes via stdio.
//!
//! Reader and writer are separated so the reader can move to a dedicated
//! Reader Task (ls-bridge-message-ordering) for non-blocking response routing.

use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

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
        let mut saw_header = false;
        // First non-LSP-header line seen this frame, kept (truncated) so the
        // genuine framing error below can QUOTE the offending bytes — when a
        // downstream prints an error to STDOUT (observed: basedpyright), that
        // text is the crash reason, and the frame that trips over it is the
        // only place it is still readable.
        let mut stray_line: Option<String> = None;

        // Read headers until empty line
        loop {
            let mut line = String::new();
            let bytes_read = self.stdout.read_line(&mut line).await?;

            // `read_line` returning 0 is EOF, which would otherwise be
            // indistinguishable from the end-of-headers empty line and
            // misreport a dead process as "missing Content-Length header" —
            // sending crash triage down a protocol-desync path.
            if bytes_read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    if saw_header {
                        "downstream closed stdout mid-headers (truncated frame)"
                    } else {
                        "downstream closed stdout (EOF)"
                    },
                ));
            }

            // Trim CRLF/LF endings
            let trimmed = line.trim_end_matches(['\r', '\n']);

            if trimmed.is_empty() {
                // Only a newline-TERMINATED empty line ends the headers. A
                // lone '\r' (or bare partial) without its '\n' can only mean
                // the stream ended mid-separator — `read_line` returns a
                // newline-less line exclusively at EOF — so fall through to
                // the next read, which reports the EOF instead of letting a
                // dying gasp masquerade as a complete (and then
                // Content-Length-less) header block.
                if line.ends_with('\n') {
                    break; // Empty line = end of headers
                }
                saw_header = true;
                continue;
            }
            saw_header = true;

            if let Some(value) = trimmed.strip_prefix("Content-Length: ") {
                content_length = Some(value.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length value")
                })?);
            } else if stray_line.is_none() {
                // Remember the first non-Content-Length line for the framing
                // error's quote. Legitimate blocks always carry
                // Content-Length, so this is only ever REPORTED for a failed
                // block — where any such line (including ones that merely
                // look header-shaped, like a JSON fragment after a length
                // mismatch or "Error: …") IS the evidence. Real spare headers
                // (Content-Type) on healthy frames are still ignored.
                let mut quoted = trimmed.to_string();
                const MAX_QUOTE: usize = 300;
                if quoted.len() > MAX_QUOTE {
                    let mut end = MAX_QUOTE;
                    while !quoted.is_char_boundary(end) {
                        end -= 1;
                    }
                    quoted.truncate(end);
                    quoted.push('…');
                }
                stray_line = Some(quoted);
            }
        }

        let content_length = content_length.ok_or_else(|| match stray_line {
            Some(stray) => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("missing Content-Length header (stray stdout line: {stray:?})"),
            ),
            None => io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header"),
        })?;

        // Read exact body bytes. Name a mid-body EOF like the header-side
        // classifications (tokio's generic "early eof" otherwise breaks the
        // uniform crash-triage reading this reader's errors now have).
        let mut body = vec![0u8; content_length];
        self.stdout.read_exact(&mut body).await.map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "downstream closed stdout mid-body (truncated frame)",
                )
            } else {
                e
            }
        })?;

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
    stderr: Option<ChildStderr>,  // Option to support taking for the drain task
}

/// Writer half of a split connection.
///
/// Owns the child process and writer. Dropping this kills the child process.
pub(crate) struct SplitConnectionWriter {
    child: Child,
    writer: BridgeWriter,
}

impl SplitConnectionWriter {
    /// Child process id, exposed so tests can assert the process really dies.
    #[cfg(test)]
    pub(crate) fn child_id(&self) -> Option<u32> {
        self.child.id()
    }

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
            // Piped (not null) so a crashing downstream's dying words are
            // observable: the pool spawns [`drain_downstream_stderr`] on it.
            // The drain never stops reading (it only stops LOGGING past its
            // cap), so a chatty child cannot block on a full stderr pipe.
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("bridge: failed to capture stdin"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("bridge: failed to capture stdout"))?;

        let stderr = child.stderr.take();

        Ok(Self {
            child: Some(child),
            writer: Some(BridgeWriter { stdin }),
            reader: Some(BridgeReader::new(stdout)),
            stderr,
        })
    }

    /// Take the child's stderr for the drain task (None after the first take,
    /// or if capture failed at spawn).
    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
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

/// Cap on stderr lines LOGGED per connection; the drain keeps reading past it
/// (a full pipe would otherwise block the child) but stops the log noise.
const STDERR_LOG_MAX_LINES: usize = 500;
/// Cap on a single logged stderr line's bytes (truncated at a char boundary).
const STDERR_LOG_MAX_LINE_BYTES: usize = 2000;

/// Forward a downstream process's stderr into kakehashi's log, bounded (see
/// the constants above). This is the crash-triage channel: a downstream that
/// dies (e.g. a node heap OOM) writes its reason here, which `Stdio::null()`
/// used to discard while the reader could only report the resulting EOF.
pub(crate) async fn drain_downstream_stderr(stderr: ChildStderr, server_name: String) {
    let (logged, total) = drain_stderr_lines(stderr, |line| {
        log::warn!(target: "kakehashi::bridge::stderr", "[{server_name}] {line}");
    })
    .await;
    if total > logged {
        log::warn!(
            target: "kakehashi::bridge::stderr",
            "[{server_name}] (suppressed {} further stderr lines)",
            total - logged
        );
    }
}

/// The testable core of [`drain_downstream_stderr`]: read lines until EOF,
/// emit at most [`STDERR_LOG_MAX_LINES`] of them (truncated to
/// [`STDERR_LOG_MAX_LINE_BYTES`] at a char boundary), keep DRAINING past the
/// cap, and return `(emitted, total)` line counts.
async fn drain_stderr_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    mut emit: impl FnMut(String),
) -> (usize, usize) {
    let mut lines = BufReader::new(reader).lines();
    let mut total = 0usize;
    let mut emitted = 0usize;
    while let Ok(Some(mut line)) = lines.next_line().await {
        total += 1;
        if emitted >= STDERR_LOG_MAX_LINES {
            continue;
        }
        if line.len() > STDERR_LOG_MAX_LINE_BYTES {
            let mut end = STDERR_LOG_MAX_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
            line.push('…');
        }
        emit(line);
        emitted += 1;
    }
    (emitted, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_emits_lines_truncated_at_char_boundaries() {
        let (mut tx, rx) = tokio::io::duplex(64 * 1024);
        let long = format!("{}あ", "x".repeat(STDERR_LOG_MAX_LINE_BYTES - 1));
        let payload = format!("first\n{long}\nlast\n");
        tokio::io::AsyncWriteExt::write_all(&mut tx, payload.as_bytes())
            .await
            .unwrap();
        drop(tx);

        let mut seen = Vec::new();
        let (emitted, total) = drain_stderr_lines(rx, |line| seen.push(line)).await;
        assert_eq!((emitted, total), (3, 3));
        assert_eq!(seen[0], "first");
        assert!(seen[1].ends_with('…'), "over-long line is truncated");
        assert!(
            seen[1].len() <= STDERR_LOG_MAX_LINE_BYTES + '…'.len_utf8(),
            "truncation respects the byte cap"
        );
        assert_eq!(seen[2], "last");
    }

    #[tokio::test]
    async fn drain_keeps_reading_past_the_log_cap() {
        let (mut tx, rx) = tokio::io::duplex(64 * 1024);
        let writer = tokio::spawn(async move {
            for i in 0..(STDERR_LOG_MAX_LINES + 50) {
                tokio::io::AsyncWriteExt::write_all(&mut tx, format!("line{i}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });

        let (emitted, total) = drain_stderr_lines(rx, |_| {}).await;
        writer.await.unwrap();
        assert_eq!(emitted, STDERR_LOG_MAX_LINES);
        assert_eq!(
            total,
            STDERR_LOG_MAX_LINES + 50,
            "draining continues past the cap"
        );
    }

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

    /// A downstream that exits cleanly closes stdout; the reader must report
    /// that as EOF, not as a framing error. The old message ("missing
    /// Content-Length header") sent crash triage down the wrong path: it read
    /// as protocol desync when the process had simply died (observed with a
    /// basedpyright crash under load — and every clean shutdown logged the
    /// same misleading warning).
    #[tokio::test]
    async fn read_message_reports_eof_when_downstream_closes_stdout() {
        // `true` writes nothing and exits: a clean close before any message.
        let cmd = vec!["true".to_string()];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        let err = conn
            .read_message()
            .await
            .expect_err("EOF must surface as an error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("closed stdout"),
            "EOF must not masquerade as a framing error: {err}"
        );
    }

    /// EOF in the middle of a header block is a truncated frame (the process
    /// died mid-write) — still EOF, but named as truncation.
    #[tokio::test]
    async fn read_message_reports_truncated_frame_on_eof_mid_headers() {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'Content-Length: 10\\r\\n'".to_string(),
        ];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        let err = conn
            .read_message()
            .await
            .expect_err("a truncated header block must surface as an error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("mid-headers"),
            "mid-frame EOF should be named as truncation: {err}"
        );
    }

    /// A dying gasp of a lone `\r` (a separator cut before its `\n`) must not
    /// read as a complete empty line: that resurrects the phantom
    /// "missing Content-Length header" on a 1-byte window of a crashing
    /// process. Only a newline-terminated empty line ends the header block.
    #[tokio::test]
    async fn read_message_reports_eof_on_a_truncated_separator() {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'Content-Length: 10\\r\\n\\r'".to_string(),
        ];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        let err = conn
            .read_message()
            .await
            .expect_err("a truncated separator must surface as an error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("mid-headers"),
            "a separator cut before its newline is a truncated frame, \
             not a framing error: {err}"
        );
    }

    /// A process dying mid-body keeps the UnexpectedEof kind but previously
    /// surfaced tokio's generic "early eof" — name it like the header-side
    /// classifications so crash triage reads uniformly.
    #[tokio::test]
    async fn read_message_names_the_closed_stdout_on_a_truncated_body() {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'Content-Length: 100\\r\\n\\r\\nshort'".to_string(),
        ];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        let err = conn
            .read_message()
            .await
            .expect_err("a truncated body must surface as an error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("mid-body"),
            "mid-body EOF should carry the classification naming: {err}"
        );
    }

    /// A COMPLETE header block (terminated by its empty line) that lacks
    /// Content-Length is the one genuine framing error — the message is kept
    /// for exactly this case.
    #[tokio::test]
    async fn read_message_keeps_framing_error_for_headers_without_content_length() {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'X-Whatever: 1\\r\\n\\r\\n'".to_string(),
        ];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        let err = conn
            .read_message()
            .await
            .expect_err("a header block without Content-Length must fail");
        assert!(
            err.to_string().contains("missing Content-Length header"),
            "genuine framing errors keep the framing message: {err}"
        );
    }

    /// The genuine framing error must QUOTE the stray stdout line that broke
    /// the frame: when a downstream prints an error to STDOUT (observed:
    /// basedpyright emitting unframed output under load), that text IS the
    /// crash reason, and the frame that trips over it is the only place the
    /// evidence is still readable.
    #[tokio::test]
    async fn read_message_quotes_the_stray_line_in_the_framing_error() {
        let cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'Error: downstream exploded\\r\\n\\r\\n'".to_string(),
        ];
        let mut conn = AsyncBridgeConnection::spawn(cmd)
            .await
            .expect("spawn should succeed");

        let err = conn
            .read_message()
            .await
            .expect_err("a header block without Content-Length must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("missing Content-Length header"),
            "the framing classification must be kept: {msg}"
        );
        assert!(
            msg.contains("Error: downstream exploded"),
            "the stray stdout line is the crash evidence and must be quoted: {msg}"
        );
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
