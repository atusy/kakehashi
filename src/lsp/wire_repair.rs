//! Repair of malformed inbound JSON-RPC bodies.
//!
//! Some clients chunk large `didChange` insertions at fixed code-unit
//! boundaries and can split a UTF-16 surrogate pair across two messages,
//! producing JSON with lone surrogate escapes (e.g. `\uD83D` with no low
//! surrogate). serde_json rejects such bodies, and tokio-util's `FramedRead`
//! fuses after a decode error, so a single malformed frame kills the whole
//! server (tokio-rs/tokio#3976). Repairing the body before the codec sees it
//! keeps the server alive.
//!
//! A lone surrogate is one UTF-16 code unit and so is U+FFFD, so replacing it
//! preserves every subsequent LSP position exactly.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, SimplexStream, WriteHalf};

/// Wraps the server's input stream, repairing malformed JSON-RPC frame
/// bodies in flight so the downstream codec never sees them.
///
/// Frames that need no repair are forwarded byte-for-byte. If framing is
/// impossible (missing `Content-Length`), the remaining stream is handed
/// through untouched.
pub fn repair_inbound_frames(
    inner: impl AsyncRead + Send + Unpin + 'static,
) -> impl AsyncRead + Send + Unpin + 'static {
    let (reader, writer) = tokio::io::simplex(64 * 1024);
    tokio::spawn(pump_frames(inner, writer));
    reader
}

const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

async fn pump_frames(input: impl AsyncRead + Send + Unpin, mut output: WriteHalf<SimplexStream>) {
    forward_frames(input, &mut output).await;
    // A dropped split-WriteHalf does NOT close the underlying SimplexStream
    // (the ReadHalf keeps it alive); EOF must be signalled explicitly.
    let _ = output.shutdown().await;
}

async fn forward_frames(
    mut input: impl AsyncRead + Send + Unpin,
    output: &mut WriteHalf<SimplexStream>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let Some(header_end) = fill_until_header_end(&mut input, &mut buf).await else {
            // EOF (or write-side gone) before a complete header: flush and stop.
            let _ = output.write_all(&buf).await;
            return;
        };
        let Some(content_len) = parse_content_length(&buf[..header_end]) else {
            // Framing is impossible without Content-Length; degrade to a
            // transparent passthrough and let the downstream codec decide.
            if output.write_all(&buf).await.is_err() {
                return;
            }
            let _ = tokio::io::copy(&mut input, output).await;
            return;
        };
        let frame_end = header_end + content_len;
        while buf.len() < frame_end {
            if !read_more(&mut input, &mut buf).await {
                let _ = output.write_all(&buf).await; // truncated final frame
                return;
            }
        }
        let forwarded = match repair_frame_body(&buf[header_end..frame_end]) {
            None => output.write_all(&buf[..frame_end]).await,
            Some(body) => {
                log::warn!(
                    "repaired lone surrogates in inbound frame ({} -> {} bytes)",
                    content_len,
                    body.len()
                );
                let headers = rewrite_content_length(&buf[..header_end], body.len());
                match output.write_all(&headers).await {
                    Ok(()) => output.write_all(body.as_bytes()).await,
                    Err(e) => Err(e),
                }
            }
        };
        if forwarded.is_err() {
            return; // reader side is gone; nothing left to do
        }
        buf.drain(..frame_end);
    }
}

/// Reads until `buf` contains a full header block, returning the offset just
/// past the terminating `\r\n\r\n`, or `None` on EOF.
async fn fill_until_header_end(
    input: &mut (impl AsyncRead + Unpin),
    buf: &mut Vec<u8>,
) -> Option<usize> {
    let mut searched: usize = 0;
    loop {
        let from = searched.saturating_sub(HEADER_TERMINATOR.len() - 1);
        if let Some(pos) = find_subslice(&buf[from..], HEADER_TERMINATOR) {
            return Some(from + pos + HEADER_TERMINATOR.len());
        }
        searched = buf.len();
        if !read_more(input, buf).await {
            return None;
        }
    }
}

async fn read_more(input: &mut (impl AsyncRead + Unpin), buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 8 * 1024];
    match input.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    for line in headers.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let Some((name, value)) = line.trim_end_matches('\r').split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Rewrites the `Content-Length` value in a header block, preserving every
/// other header byte-for-byte.
fn rewrite_content_length(headers: &[u8], new_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(headers.len() + 8);
    for line in headers.split_inclusive(|&b| b == b'\n') {
        let text = std::str::from_utf8(line).ok();
        let is_content_length = text
            .and_then(|t| t.split_once(':'))
            .is_some_and(|(name, _)| name.eq_ignore_ascii_case("content-length"));
        if is_content_length {
            out.extend_from_slice(format!("Content-Length: {new_len}\r\n").as_bytes());
        } else {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Repairs one frame body, returning `None` when it can be forwarded as-is.
fn repair_frame_body(body: &[u8]) -> Option<String> {
    // Cheap gate: surrogate escapes start with `\ud` or `\uD` (first hex
    // digit of D800..DFFF); bodies without them are forwarded untouched.
    let gate = body
        .windows(3)
        .any(|w| w[0] == b'\\' && w[1] == b'u' && (w[2] == b'd' || w[2] == b'D'));
    if !gate {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    Some(repair_lone_surrogates(text)?.repaired)
}

/// Outcome of repairing one JSON body.
#[derive(Debug, PartialEq)]
pub(crate) struct BodyRepair {
    /// The body with every lone surrogate escape replaced by `�`.
    pub(crate) repaired: String,
}

/// Replaces lone surrogate escapes in `body` with `�`.
///
/// Returns `None` when the body contains no lone surrogates.
pub(crate) fn repair_lone_surrogates(body: &str) -> Option<BodyRepair> {
    const ESCAPE_LEN: usize = 6; // \uXXXX

    let bytes = body.as_bytes();
    let mut repaired = String::new();
    let mut copied = 0; // start of the region not yet copied into `repaired`
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                in_string = !in_string;
                i += 1;
            }
            b'\\' if in_string => match bytes.get(i + 1) {
                Some(b'u') => match parse_hex4(&bytes[i + 2..]) {
                    Some(unit) if is_high_surrogate(unit) => {
                        if starts_with_low_surrogate_escape(&bytes[i + ESCAPE_LEN..]) {
                            i += 2 * ESCAPE_LEN; // valid pair
                        } else {
                            repaired.push_str(&body[copied..i]);
                            repaired.push('\u{FFFD}');
                            i += ESCAPE_LEN;
                            copied = i;
                        }
                    }
                    Some(unit) if is_low_surrogate(unit) => {
                        // A low surrogate reachable here has no preceding high
                        // (a valid pair is consumed as a whole above).
                        repaired.push_str(&body[copied..i]);
                        repaired.push('\u{FFFD}');
                        i += ESCAPE_LEN;
                        copied = i;
                    }
                    Some(_) => i += ESCAPE_LEN,
                    None => i += 2, // malformed \u escape; leave as-is
                },
                Some(_) => i += 2, // \\, \", \n, ... (or malformed escape)
                None => i += 1,
            },
            _ => i += 1,
        }
    }

    if copied == 0 {
        return None;
    }
    repaired.push_str(&body[copied..]);
    Some(BodyRepair { repaired })
}

fn parse_hex4(bytes: &[u8]) -> Option<u16> {
    let hex = bytes.get(..4)?;
    let hex = std::str::from_utf8(hex).ok()?;
    u16::from_str_radix(hex, 16).ok()
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

fn starts_with_low_surrogate_escape(bytes: &[u8]) -> bool {
    bytes.first() == Some(&b'\\')
        && bytes.get(1) == Some(&b'u')
        && parse_hex4(&bytes[2..]).is_some_and(is_low_surrogate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lone_high_surrogate_escape_becomes_replacement_char() {
        let body = r#"{"text":"a\uD83D"}"#;
        let repair = repair_lone_surrogates(body).expect("lone surrogate should trigger repair");
        assert_eq!(repair.repaired, r#"{"text":"a�"}"#);
    }

    #[test]
    fn lone_low_surrogate_escape_becomes_replacement_char() {
        let body = r#"{"text":"\uDE03b"}"#;
        let repair = repair_lone_surrogates(body).expect("lone surrogate should trigger repair");
        assert_eq!(repair.repaired, r#"{"text":"�b"}"#);
    }

    #[test]
    fn valid_surrogate_pair_escape_is_untouched() {
        // Concatenated to keep the high+low escapes adjacent on the wire.
        let body = format!(r#"{{"text":"{}{}"}}"#, r"\uD83D", r"\uDE03");
        assert_eq!(repair_lone_surrogates(&body), None);
    }

    #[test]
    fn clean_body_needs_no_repair() {
        assert_eq!(repair_lone_surrogates(r#"{"text":"plain 😃 text"}"#), None);
    }

    #[test]
    fn escaped_backslash_before_u_is_not_an_escape() {
        // The client sent the literal 6 characters `\uD83D`, not an escape.
        assert_eq!(repair_lone_surrogates(r#"{"text":"\\uD83D"}"#), None);
    }

    #[test]
    fn lowercase_hex_lone_surrogate_is_repaired() {
        let body = r#"{"text":"a\ud83d"}"#;
        let repair = repair_lone_surrogates(body).expect("lone surrogate should trigger repair");
        assert_eq!(repair.repaired, r#"{"text":"a�"}"#);
    }

    #[test]
    fn high_surrogate_followed_by_plain_text_is_lone() {
        let body = r#"{"text":"\uD83Dxyz"}"#;
        let repair = repair_lone_surrogates(body).expect("lone surrogate should trigger repair");
        assert_eq!(repair.repaired, r#"{"text":"�xyz"}"#);
    }

    #[test]
    fn every_lone_surrogate_in_the_body_is_repaired() {
        // Chunked-didChange wire pattern: one change ends with the high half,
        // the next change starts with the low half.
        let body = r#"{"a":"x\uD83D","b":"\uDE03y"}"#;
        let repair = repair_lone_surrogates(body).expect("lone surrogates should trigger repair");
        assert_eq!(repair.repaired, r#"{"a":"x�","b":"�y"}"#);
    }

    #[test]
    fn truncated_escape_at_end_of_body_is_left_alone() {
        assert_eq!(repair_lone_surrogates(r#"{"text":"\uD8"#), None);
    }

    #[test]
    fn quote_escape_does_not_end_the_string() {
        // `\"` stays inside the string; the lone surrogate after it repairs.
        let body = r#"{"text":"say \"hi\" \uD83D"}"#;
        let repair = repair_lone_surrogates(body).expect("lone surrogate should trigger repair");
        assert_eq!(repair.repaired, r#"{"text":"say \"hi\" �"}"#);
    }

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{}", body.len(), body).into_bytes()
    }

    /// Feeds `input` through `repair_inbound_frames` and returns everything
    /// the wrapped reader yields until EOF.
    async fn pump_through(input: Vec<u8>) -> Vec<u8> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut tx, rx) = tokio::io::duplex(1 << 20);
        let mut repaired = repair_inbound_frames(rx);
        let write = async move {
            tx.write_all(&input).await.unwrap();
            drop(tx); // EOF
        };
        let read = async {
            let mut out = Vec::new();
            repaired.read_to_end(&mut out).await.unwrap();
            out
        };
        let (_, out) = tokio::join!(write, read);
        out
    }

    #[tokio::test]
    async fn clean_frame_passes_through_byte_for_byte() {
        let input = frame(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(input.clone()),
        )
        .await
        .expect("pump should finish");
        assert_eq!(out, input);
    }

    #[tokio::test]
    async fn frame_split_across_reads_is_reassembled() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let input = frame(r#"{"method":"x","text":"a\uD83D"}"#);
        let (mut tx, rx) = tokio::io::duplex(1 << 20);
        let mut repaired = repair_inbound_frames(rx);
        let write = async move {
            // Trickle the frame in 5-byte chunks, splitting headers, the
            // escape sequence, everything.
            for chunk in input.chunks(5) {
                tx.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
            drop(tx);
        };
        let read = async {
            let mut out = Vec::new();
            repaired.read_to_end(&mut out).await.unwrap();
            out
        };
        let (_, out) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(write, read)
        })
        .await
        .expect("pump should finish");
        assert_eq!(out, frame(r#"{"method":"x","text":"a�"}"#));
    }

    #[tokio::test]
    async fn extra_headers_survive_repair() {
        let body = r#"{"text":"\uDE03"}"#;
        let with_type = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            body.len(),
            body
        );
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(with_type.into_bytes()),
        )
        .await
        .expect("pump should finish");
        let repaired_body = r#"{"text":"�"}"#;
        let expected = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            repaired_body.len(),
            repaired_body
        );
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[tokio::test]
    async fn later_frames_still_flow_after_a_repair() {
        let bad = frame(r#"{"text":"\uD83D"}"#);
        let good = frame(r#"{"method":"y"}"#);
        let mut input = bad.clone();
        input.extend_from_slice(&good);
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let mut expected = frame(r#"{"text":"�"}"#);
        expected.extend_from_slice(&good);
        assert_eq!(out, expected);
    }

    #[tokio::test]
    async fn stream_without_content_length_degrades_to_passthrough() {
        let input = b"GARBAGE: yes\r\n\r\nleftover bytes".to_vec();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(input.clone()),
        )
        .await
        .expect("pump should finish");
        assert_eq!(out, input);
    }

    /// Regression for the field crash: a didChange whose text chunk was cut
    /// mid-surrogate-pair used to kill the whole server (serde_json rejects
    /// the body, tokio-util's FramedRead fuses after the decode error, and
    /// the read loop ends). With repair in front, the server must keep
    /// answering requests that arrive after the malformed frame.
    #[tokio::test]
    async fn server_survives_didchange_with_split_surrogate_pair() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tower_lsp_server::{LspService, Server};

        let (service, socket) = LspService::new(crate::lsp::Kakehashi::new);
        let (mut client_tx, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, mut client_rx) = tokio::io::duplex(1 << 20);
        let server = tokio::spawn(async move {
            Server::new(repair_inbound_frames(server_in), server_out, socket)
                .serve(service)
                .await;
        });

        let mut input = Vec::new();
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
        ));
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
        ));
        // didChange cut mid-emoji: the text ends in a lone high surrogate.
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///t.md","version":2},"contentChanges":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"text":"abc\uD83D"}]}}"#,
        ));
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}"#,
        ));
        client_tx.write_all(&input).await.unwrap();

        let saw_shutdown_response =
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let mut seen = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = client_rx.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return false;
                    }
                    seen.extend_from_slice(&chunk[..n]);
                    if String::from_utf8_lossy(&seen).contains(r#""id":2"#) {
                        return true;
                    }
                }
            })
            .await
            .expect("server must keep responding after the malformed frame");
        assert!(saw_shutdown_response);
        server.abort();
    }

    #[tokio::test]
    async fn frame_with_lone_surrogate_is_repaired_with_content_length() {
        let body = r#"{"method":"x","text":"a\uD83D"}"#;
        let repaired_body = r#"{"method":"x","text":"a�"}"#;
        let out =
            tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(frame(body)))
                .await
                .expect("pump should finish");
        assert_eq!(out, frame(repaired_body));
    }
}
