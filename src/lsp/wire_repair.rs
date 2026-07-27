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
//! preserves every subsequent LSP position exactly. Two places the guarantee
//! weakens, both requiring input that is already broken: byte-granular
//! corruption that cuts *inside* a WTF-8 triplet (no well-defined UTF-16
//! width — repair is best-effort), and clients that send out-of-range
//! positions (which document sync clamps: the seam then describes a
//! coordinate the document never had, and a reassembly there can add one
//! unit of drift to the pre-existing desync). The server survives both;
//! positions self-heal on the next full sync.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream};

/// Buffer between the pump and the server's codec. Sized so multi-hundred-KB
/// didChange bursts stream through without a wakeup round-trip per window;
/// correctness does not depend on the size (write_all applies backpressure).
const PUMP_BUFFER_SIZE: usize = 1 << 20;

/// Wraps the server's input stream, repairing malformed JSON-RPC frame
/// bodies in flight so the downstream codec never sees them.
///
/// Frames that need no repair are forwarded byte-for-byte. If framing is
/// impossible (missing `Content-Length`), the remaining stream is handed
/// through untouched.
pub fn repair_inbound_frames(
    inner: impl AsyncRead + Send + Unpin + 'static,
) -> impl AsyncRead + Send + Unpin + 'static {
    // A duplex (not a split simplex) on purpose: dropping a DuplexStream
    // closes it, so the reader sees EOF on ANY pump exit — including a
    // panic, which must surface as "stdin closed" rather than a server
    // that silently stops reading forever.
    let (pump_side, server_side) = tokio::io::duplex(PUMP_BUFFER_SIZE);
    tokio::spawn(forward_frames(inner, pump_side));
    server_side
}

/// Longest header-block terminator (`\r\n\r\n`); shorter mixed forms exist.
const HEADER_TERMINATOR_MAX: usize = 4;

/// Finds the earliest header-block terminator in `buf`, returning the offset
/// just past it. httparse (the codec's header parser) accepts `\r\n` and
/// bare `\n` per line independently, so the terminator is any two adjacent
/// line breaks: `\r\n\r\n`, `\r\n\n`, `\n\r\n`, or `\n\n`.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < buf.len() {
        let nl = buf[i..].iter().position(|&b| b == b'\n')?;
        let after = i + nl + 1;
        match buf.get(after) {
            Some(&b'\n') => return Some(after + 1),
            Some(&b'\r') if buf.get(after + 1) == Some(&b'\n') => return Some(after + 2),
            // Anything else (including a trailing `\r` that may yet complete
            // to `\r\n` once more bytes arrive): keep scanning / wait.
            _ => i = after,
        }
    }
    None
}

async fn forward_frames(mut input: impl AsyncRead + Send + Unpin, mut output: DuplexStream) {
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK_SIZE);
    let mut repairer = FrameRepairer::default();
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
            let _ = tokio::io::copy(&mut input, &mut output).await;
            return;
        };
        // `content_len` is attacker/bug-controlled; a wrapped add would make
        // an inverted slice below and panic the pump.
        let Some(frame_end) = header_end.checked_add(content_len) else {
            if output.write_all(&buf).await.is_err() {
                return;
            }
            let _ = tokio::io::copy(&mut input, &mut output).await;
            return;
        };
        // Repair needs the whole body in memory (and a second copy lives in
        // the downstream codec's buffer), so monster frames would double
        // peak memory. Stream them instead, through a byte-length-preserving
        // stage-1 repair (Content-Length stays valid); only seam reassembly
        // is skipped at this scale.
        const LARGE_FRAME_BYPASS: usize = 64 * 1024 * 1024;
        if content_len > LARGE_FRAME_BYPASS && buf.len() < frame_end {
            // An edit this size gets no seam inspection; drop all state.
            repairer.forget_seams();
            if output.write_all(&buf[..header_end]).await.is_err() {
                return;
            }
            let mut stream = StreamRepair::default();
            let mut scratch: Vec<u8> = Vec::with_capacity(READ_CHUNK_SIZE + 16);
            let mut tail: Vec<u8> = buf.split_off(header_end);
            buf.clear();
            let mut body_remaining = content_len;
            loop {
                // The read may run past the frame into the next one; only
                // the frame's own bytes go through the stream repair.
                let take = tail.len().min(body_remaining);
                scratch.clear();
                stream.feed(&tail[..take], &mut scratch);
                if output.write_all(&scratch).await.is_err() {
                    return;
                }
                body_remaining -= take;
                if body_remaining == 0 {
                    scratch.clear();
                    stream.finish(&mut scratch);
                    if output.write_all(&scratch).await.is_err() {
                        return;
                    }
                    buf = tail[take..].to_vec(); // over-read: next frame's bytes
                    break;
                }
                tail.clear();
                if !read_more(&mut input, &mut tail).await {
                    // Truncated final frame: flush what the repair withheld.
                    scratch.clear();
                    stream.finish(&mut scratch);
                    let _ = output.write_all(&scratch).await;
                    return;
                }
            }
            continue;
        }
        while buf.len() < frame_end {
            if !read_more(&mut input, &mut buf).await {
                let _ = output.write_all(&buf).await; // truncated final frame
                return;
            }
        }
        let forwarded = match repairer.process(&buf[header_end..frame_end]) {
            None => output.write_all(&buf[..frame_end]).await,
            Some(body) => {
                log::warn!(
                    "repaired malformed inbound frame ({} -> {} bytes)",
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
        // Per spec `exit` is the last message; tower-lsp stops reading right
        // after it (clients may keep stdin open forever) and the pump does
        // the same so no fresh non-cancellable stdin read is parked. This
        // detection is best-effort (an escape-spelled method name or a
        // whitespace-padded frame can evade it): the hard guarantee against
        // a stalled shutdown is main's `runtime.shutdown_background()`,
        // which never waits for a parked stdin read.
        if is_exit_notification(&buf[header_end..frame_end]) {
            return;
        }
        buf.drain(..frame_end);
        // A single huge frame (full-text sync) must not pin its buffer for
        // the rest of the session.
        const BUF_RETAIN_LIMIT: usize = 4 * READ_CHUNK_SIZE;
        if buf.capacity() > BUF_RETAIN_LIMIT && buf.len() < BUF_RETAIN_LIMIT {
            buf.shrink_to(BUF_RETAIN_LIMIT);
        }
    }
}

/// Is this body the `exit` notification, as real clients spell it? Bodies
/// above a tiny bound or without the literal method text skip the parse;
/// evasive spellings are caught by main's shutdown_background instead.
fn is_exit_notification(body: &[u8]) -> bool {
    const MAX_EXIT_FRAME: usize = 256;
    if body.len() > MAX_EXIT_FRAME || !body.windows(4).any(|w| w == b"exit") {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|msg| {
            msg.get("method")
                .and_then(|m| m.as_str())
                .map(|m| m == "exit")
        })
        .unwrap_or(false)
}

/// Reads until `buf` contains a full header block, returning the offset just
/// past the terminator, or `None` on EOF.
async fn fill_until_header_end(
    input: &mut (impl AsyncRead + Unpin),
    buf: &mut Vec<u8>,
) -> Option<usize> {
    let mut searched: usize = 0;
    loop {
        let from = searched.saturating_sub(HEADER_TERMINATOR_MAX - 1);
        if let Some(end) = find_header_end(&buf[from..]) {
            return Some(from + end);
        }
        searched = buf.len();
        if !read_more(input, buf).await {
            return None;
        }
    }
}

/// Read granularity: large enough that a megabyte-scale didChange needs few
/// blocking-pool round-trips through `tokio::io::stdin`, small enough that
/// the post-frame drain tail stays cheap.
const READ_CHUNK_SIZE: usize = 64 * 1024;

async fn read_more(input: &mut (impl AsyncRead + Unpin), buf: &mut Vec<u8>) -> bool {
    // read_buf appends into spare capacity directly — no intermediate copy.
    buf.reserve(READ_CHUNK_SIZE);
    matches!(input.read_buf(buf).await, Ok(n) if n > 0)
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    // Last occurrence wins, mirroring the downstream codec's header loop —
    // the pump and the codec must frame duplicate-header streams identically
    // or a rewrite would corrupt previously-parseable input.
    let mut length = None;
    for line in headers.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let Some((name, value)) = line.trim_end_matches('\r').split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().ok();
        }
    }
    length
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

/// Streaming, byte-length-preserving stage-1 repair for frames too large to
/// buffer: lone surrogate escapes become `�` (6 -> 6 bytes), WTF-8
/// surrogate triplets become U+FFFD (3 -> 3 bytes), and other invalid UTF-8
/// bytes become `?` (1 -> 1 byte). Content-Length therefore never changes
/// and no whole-body buffer is needed. Pair reassembly (stage 2) and the
/// batch scanner's string-context tracking are deliberately skipped at this
/// scale: an escaped-backslash false positive rewrites six literal
/// characters into six other literal characters, preserving geometry.
#[derive(Default)]
struct StreamRepair {
    /// Bytes withheld at a chunk boundary because they could be the prefix
    /// of a pattern that needs lookahead (at most [`Self::MAX_CARRY`]).
    carry: Vec<u8>,
}

impl StreamRepair {
    /// A high surrogate escape plus all but the last byte of its potential
    /// low escape: `\uXXXX\uYYY`.
    const MAX_CARRY: usize = 11;

    fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) {
        let mut work = std::mem::take(&mut self.carry);
        work.extend_from_slice(chunk);
        let consumed = repair_stream_into(&work, out, false);
        debug_assert!(work.len() - consumed <= Self::MAX_CARRY);
        self.carry = work[consumed..].to_vec();
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        let work = std::mem::take(&mut self.carry);
        repair_stream_into(&work, out, true);
    }
}

/// Core of [`StreamRepair`]: repairs `work` into `out`, returning how many
/// bytes were consumed. Unless `at_end`, stops short of the tail when it
/// could be a pattern prefix needing more input. Every rule is
/// byte-length-preserving.
fn repair_stream_into(work: &[u8], out: &mut Vec<u8>, at_end: bool) -> usize {
    const PAIR_LEN: usize = 12; // \uXXXX\uYYYY
    let mut i = 0;
    while i < work.len() {
        let Some(rel) = work[i..].iter().position(|&b| b == b'\\' || b >= 0x80) else {
            out.extend_from_slice(&work[i..]);
            return work.len();
        };
        out.extend_from_slice(&work[i..i + rel]);
        i += rel;
        let b = work[i];
        if b == b'\\' {
            if !at_end && i + PAIR_LEN > work.len() {
                return i; // withhold: could be a pair straddling the chunk
            }
            match work.get(i + 1) {
                Some(b'u') => match parse_hex4(&work[i + 2..]) {
                    Some(unit) if is_high_surrogate(unit) => {
                        if low_surrogate_escape(&work[i + 6..]).is_some() {
                            out.extend_from_slice(&work[i..i + PAIR_LEN]);
                            i += PAIR_LEN;
                        } else {
                            out.extend_from_slice(b"\\uFFFD");
                            i += 6;
                        }
                    }
                    Some(unit) if is_low_surrogate(unit) => {
                        out.extend_from_slice(b"\\uFFFD");
                        i += 6;
                    }
                    Some(_) => {
                        out.extend_from_slice(&work[i..i + 6]);
                        i += 6;
                    }
                    None => {
                        let take = 2.min(work.len() - i);
                        out.extend_from_slice(&work[i..i + take]);
                        i += take;
                    }
                },
                // Pairwise skip keeps backslash parity (`\\` copies whole),
                // so a later `\uD...` after an escaped backslash is plain
                // text and never reaches the branch above.
                Some(_) => {
                    out.extend_from_slice(&work[i..i + 2]);
                    i += 2;
                }
                None => {
                    out.push(b);
                    i += 1;
                }
            }
        } else {
            let len = match b {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => {
                    out.push(b'?'); // stray continuation or invalid lead
                    i += 1;
                    continue;
                }
            };
            if !at_end && i + len > work.len() {
                return i; // withhold: partial multi-byte sequence
            }
            let seq = &work[i..(i + len).min(work.len())];
            if seq.len() == len && std::str::from_utf8(seq).is_ok() {
                out.extend_from_slice(seq);
                i += len;
            } else if seq.len() >= 3
                && seq[0] == 0xED
                && (0xA0..=0xBF).contains(&seq[1])
                && (0x80..=0xBF).contains(&seq[2])
            {
                out.extend_from_slice("\u{FFFD}".as_bytes()); // 3 -> 3 bytes
                i += 3;
            } else {
                out.push(b'?');
                i += 1;
            }
        }
    }
    i
}

/// A lone high surrogate seen at the end of a didChange insertion, waiting
/// for the matching low surrogate at the start of the next chunk.
struct PendingHigh {
    unit: u16,
    /// UTF-16 position just past the inserted `�` seam.
    line: u64,
    character: u64,
    /// Frame index after which this seam is dropped. Chunk continuations
    /// arrive within a handful of frames; an abandoned seam must not force
    /// the inspection slow path for the rest of the session.
    expires_at: u64,
}

/// How many frames a seam stays alive. The field pattern interleaves at most
/// ~10 read-only frames between chunks; 32 is generous headroom.
const SEAM_TTL_FRAMES: u64 = 32;

/// Per-connection frame processor: repairs lone surrogates (stage 1) and
/// reassembles surrogate pairs split across adjacent didChange chunks
/// (stage 2).
///
/// Stage 2 works because the repair can be expressed entirely on the *next*
/// frame: an insertion of `<low>rest` at the seam position P becomes a
/// replacement of `[P-1, P)` (the `�` left by stage 1) with `<pair>rest`.
/// Both sides agree on every position throughout — a lone surrogate and
/// U+FFFD are one UTF-16 unit each — so the rewrite never shifts geometry,
/// and if the continuation never arrives the stage-1 result simply stands.
#[derive(Default)]
struct FrameRepairer {
    pending: std::collections::HashMap<String, PendingHigh>,
    /// Monotonic count of processed frames, for seam expiry.
    frame_index: u64,
}

impl FrameRepairer {
    /// Processes one frame body, returning `None` when it can be forwarded
    /// byte-for-byte.
    fn process(&mut self, body: &[u8]) -> Option<String> {
        self.frame_index += 1;
        if !self.pending.is_empty() {
            let now = self.frame_index;
            self.pending.retain(|_, seam| seam.expires_at > now);
        }
        // Bodies that are not valid UTF-8 would fuse the downstream codec
        // exactly like a serde failure; decode them first. WTF-8 surrogate
        // triplets become `\uXXXX` escapes so both spellings of a split
        // pair flow through the same repair below.
        let decoded_body;
        let (text, was_decoded): (&str, bool) = match std::str::from_utf8(body) {
            Ok(text) => (text, false),
            Err(_) => {
                decoded_body = decode_invalid_utf8(body);
                (&decoded_body, true)
            }
        };
        // Cheap gate: surrogate escapes start with `\ud` or `\uD` (first hex
        // digit of D800..DFFF); bodies without them are forwarded untouched
        // unless a pending seam requires inspecting document notifications.
        // `str::contains` is memchr-accelerated on the first byte, and `\` is
        // sparse in real bodies, so this beats a scalar windowed scan.
        let has_surrogate_escape = text.contains("\\ud") || text.contains("\\uD");
        // While a seam is pending, EVERY frame must be inspected: matching
        // on a method-name substring would miss legal alternate spellings
        // (e.g. `textDocument\/didChange` from serializers that escape `/`),
        // and a missed invalidation turns a stale seam into a wrong rewrite.
        // Seam windows are rare and short, so the extra parse is cheap.
        let needs_inspection = !self.pending.is_empty();
        if !was_decoded && !has_surrogate_escape && !needs_inspection {
            return None;
        }
        let repair = if has_surrogate_escape {
            repair_lone_surrogates(text)
        } else {
            None
        };
        if !was_decoded && repair.is_none() && !needs_inspection {
            return None;
        }

        let repaired_text = repair.as_ref().map_or(text, |r| r.repaired.as_str());
        let Ok(mut msg) = serde_json::from_str::<serde_json::Value>(repaired_text) else {
            // Invalid JSON beyond lone surrogates: forward our best repair,
            // or the frame byte-for-byte when nothing was changed (an
            // inspection pass alone must not rewrite headers).
            return (repair.is_some() || was_decoded).then(|| repaired_text.to_owned());
        };
        let strings = repair.as_ref().map_or(&[][..], |r| r.strings.as_slice());
        let rewritten = match msg.get("method").and_then(|m| m.as_str()) {
            Some("textDocument/didChange") => {
                if is_applyable_didchange(&msg) {
                    self.process_didchange(&mut msg, strings)
                } else {
                    // The downstream will drop this notification without
                    // applying it (request form, or params that fail typed
                    // validation). Tracking seams against an edit that never
                    // happens would let a stale seam rewrite a live
                    // character later — forget everything instead.
                    self.pending.clear();
                    false
                }
            }
            Some("textDocument/didOpen") | Some("textDocument/didClose") => {
                // The document was replaced or dropped; any recorded seam
                // position no longer describes it.
                if let Some(uri) = document_uri(&msg) {
                    self.pending.remove(&uri);
                }
                false
            }
            _ => false,
        };
        if rewritten {
            match serde_json::to_string(&msg) {
                Ok(serialized) => Some(serialized),
                Err(_) => Some(repaired_text.to_owned()),
            }
        } else if repair.is_some() || was_decoded {
            Some(repaired_text.to_owned())
        } else {
            None
        }
    }

    fn process_didchange(
        &mut self,
        msg: &mut serde_json::Value,
        strings: &[RepairedString],
    ) -> bool {
        let Some(uri) = document_uri(msg) else {
            return false;
        };
        // Scanner records carry no JSON path, so attribution goes by decoded
        // value. That is only sound when no OTHER string in the message (a
        // key, the uri, an extension property) shares a text's value — an
        // equal-valued sibling could donate its record and drive a rewrite
        // that merges unrelated values. Flag such texts and leave them to
        // stage 1.
        let ambiguous = ambiguity_flags(msg, strings);
        let Some(changes) = msg
            .pointer_mut("/params/contentChanges")
            .and_then(|c| c.as_array_mut())
        else {
            return false;
        };
        let mut used = vec![false; strings.len()];
        let mut dirty = false;
        for (index, change) in changes.iter_mut().enumerate() {
            let Some(text) = change
                .get("text")
                .and_then(|t| t.as_str())
                .map(str::to_owned)
            else {
                continue;
            };
            // Attribute the scanner's record to this text field by decoded
            // value, consuming records in body order so identical texts pair
            // with their own occurrence.
            let entry = if ambiguous.get(index).copied().unwrap_or(true) {
                None
            } else {
                strings
                    .iter()
                    .enumerate()
                    .position(|(k, s)| !used[k] && s.value == text)
                    .map(|k| {
                        used[k] = true;
                        &strings[k]
                    })
            };

            let (start, is_ranged) = match change.get("range") {
                None => {
                    // Full-text sync replaces the document: any seam is gone.
                    self.pending.remove(&uri);
                    (Some((0, 0)), false)
                }
                Some(range) => match range_bounds(range) {
                    // Any ranged edit whose start sits at the seam can
                    // consume it: its inserted text begins right where the
                    // low half belongs, whether it replaces something or not.
                    Some((range_start, _)) => (Some(range_start), true),
                    None => (None, false),
                },
            };

            if let (Some((line, character)), Some(entry)) = (start, entry) {
                // Typed validation upstream caps rangeLength at u32; exactly
                // u32::MAX would overflow when the rewrite widens it by one,
                // and the broken field would make tower drop the whole edit.
                let range_length_can_widen = change.get("rangeLength").map_or(true, |v| {
                    v.as_u64().is_some_and(|l| l < u64::from(u32::MAX))
                });
                let seam = if is_ranged && range_length_can_widen && entry.leading_low.is_some() {
                    self.pending
                        .get(&uri)
                        // A recorded seam always sits just past a non-newline
                        // `�`, so character >= 1 holds; the check documents
                        // the invariant that guards the -1 below.
                        .is_some_and(|p| {
                            p.line == line && p.character == character && p.character >= 1
                        })
                        .then(|| self.pending.remove(&uri))
                        .flatten()
                } else {
                    None
                };
                if let (Some(p), Some(low)) = (seam, entry.leading_low) {
                    let pair = combine_surrogates(p.unit, low);
                    let new_text: String = format!("{}{}", pair, &text['\u{FFFD}'.len_utf8()..]);
                    change["range"]["start"]["character"] =
                        serde_json::Value::from(p.character - 1);
                    // The rewrite widens the replaced range by the one `�`
                    // unit in front of it.
                    if let Some(len) = change.get("rangeLength").and_then(|v| v.as_u64()) {
                        change["rangeLength"] = serde_json::Value::from(len + 1);
                    }
                    change["text"] = serde_json::Value::from(new_text.as_str());
                    dirty = true;
                    if let Some(unit) = entry.trailing_high {
                        let (l, c) = advance_utf16(line, character - 1, &new_text);
                        self.remember_seam(&uri, unit, l, c);
                    }
                    continue;
                }
            }

            // Any didChange for this document that does not consume the seam
            // invalidates it: the recorded position may describe stale text.
            self.pending.remove(&uri);
            if let (Some((line, character)), Some(unit)) =
                (start, entry.and_then(|e| e.trailing_high))
            {
                let (l, c) = advance_utf16(line, character, &text);
                self.remember_seam(&uri, unit, l, c);
            }
        }
        dirty
    }

    /// Drops all seam state (used when a frame skips inspection entirely).
    fn forget_seams(&mut self) {
        self.pending.clear();
    }

    fn remember_seam(&mut self, uri: &str, unit: u16, line: u64, character: u64) {
        // Safety valve against pathological clients that open seams on many
        // documents and never continue them. Clearing is purely conservative:
        // a dropped seam can only leave stage-1's U+FFFD standing, never
        // cause a wrong rewrite.
        const MAX_PENDING_SEAMS: usize = 8;
        if self.pending.len() >= MAX_PENDING_SEAMS && !self.pending.contains_key(uri) {
            self.pending.clear();
        }
        self.pending.insert(
            uri.to_owned(),
            PendingHigh {
                unit,
                line,
                character,
                expires_at: self.frame_index + SEAM_TTL_FRAMES,
            },
        );
    }
}

/// Will the downstream server actually APPLY this didChange? Seam state must
/// only ever track edits the document receives: tower-lsp drops didChange
/// requests-with-id and notifications whose params fail typed
/// deserialization, so the pump validates against the same typed shape.
fn is_applyable_didchange(msg: &serde_json::Value) -> bool {
    use tower_lsp_server::ls_types::DidChangeTextDocumentParams;

    msg.get("id").is_none()
        && msg.get("params").is_some_and(|params| {
            serde_json::from_value::<DidChangeTextDocumentParams>(params.clone()).is_ok()
        })
}

/// For each contentChange, whether its `text` value also appears as some
/// OTHER string (or object key) anywhere in the message — which would make
/// value-based record attribution ambiguous.
fn ambiguity_flags(msg: &serde_json::Value, strings: &[RepairedString]) -> Vec<bool> {
    let Some(changes) = msg
        .pointer("/params/contentChanges")
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    let text_values: Vec<*const serde_json::Value> = changes
        .iter()
        .filter_map(|c| c.get("text"))
        .map(|t| t as *const _)
        .collect();
    fn text_of(c: &serde_json::Value) -> Option<&str> {
        c.get("text").and_then(|t| t.as_str())
    }
    changes
        .iter()
        .map(|change| {
            let Some(text) = text_of(change) else {
                return false;
            };
            if other_string_equals(msg, text, &text_values) {
                return true;
            }
            // Body-order pairing of equal-valued texts is only sound when
            // every occurrence contributed a record; a text whose U+FFFD is
            // literal content contributes none, and a record from an
            // equal-valued sibling change would land on it.
            let texts_with_value = changes.iter().filter(|c| text_of(c) == Some(text)).count();
            let records_with_value = strings.iter().filter(|s| s.value == text).count();
            records_with_value > 0 && records_with_value != texts_with_value
        })
        .collect()
}

/// Does any string in `value` equal `needle`, excluding the values whose
/// addresses are listed in `exclude` (the contentChange text fields)?
fn other_string_equals(
    value: &serde_json::Value,
    needle: &str,
    exclude: &[*const serde_json::Value],
) -> bool {
    match value {
        serde_json::Value::String(s) => {
            s == needle && !exclude.contains(&std::ptr::from_ref(value))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .any(|v| other_string_equals(v, needle, exclude)),
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, v)| key == needle || other_string_equals(v, needle, exclude)),
        _ => false,
    }
}

fn document_uri(msg: &serde_json::Value) -> Option<String> {
    // Seam keys must be exactly as coarse as the downstream document keys,
    // or an edit through an alternate spelling of the same document would
    // miss invalidation and leave a stale seam. Same normalization as the
    // ingress gate.
    msg.pointer("/params/textDocument/uri")
        .and_then(|u| u.as_str())
        .map(crate::lsp::ingress_order::normalize_uri)
}

/// Returns the start position when `range` denotes an insertion
/// (start == end), else `None`.
/// Extracts the validated (start, end) positions of a change range.
fn range_bounds(range: &serde_json::Value) -> Option<((u64, u64), (u64, u64))> {
    // LSP positions are u32; anything larger cannot describe the downstream
    // document (tower-lsp rejects the whole message) and would overflow the
    // u64 position arithmetic in advance_utf16. Treat it as non-tracking.
    const MAX_POSITION: u64 = u32::MAX as u64;
    let pos = |which: &str| {
        let p = range.get(which)?;
        let line = p.get("line")?.as_u64()?;
        let character = p.get("character")?.as_u64()?;
        (line <= MAX_POSITION && character <= MAX_POSITION).then_some((line, character))
    };
    Some((pos("start")?, pos("end")?))
}

/// Advances an LSP UTF-16 position across `text` (newlines: \n, \r\n, \r).
fn advance_utf16(line: u64, character: u64, text: &str) -> (u64, u64) {
    let (mut l, mut c) = (line, character);
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => {
                l += 1;
                c = 0;
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                l += 1;
                c = 0;
            }
            _ => c += ch.len_utf16() as u64,
        }
    }
    (l, c)
}

/// Outcome of repairing one JSON body.
#[derive(Debug, PartialEq)]
struct BodyRepair {
    /// The body with every lone surrogate escape replaced by `�`.
    repaired: String,
    /// Strings that contained a lone surrogate at an edge, in body order.
    strings: Vec<RepairedString>,
}

/// A JSON string that had a lone surrogate as its first or last UTF-16 unit.
///
/// The original code units are recorded here because the repair erases them:
/// they are what lets a later frame reassemble a split surrogate pair.
#[derive(Debug, PartialEq)]
struct RepairedString {
    /// Decoded value after repair — exactly what serde_json will parse from
    /// the repaired body, used to attribute this record to a `text` field.
    value: String,
    /// Lone low surrogate that was the string's first UTF-16 unit.
    leading_low: Option<u16>,
    /// Lone high surrogate that was the string's last UTF-16 unit.
    trailing_high: Option<u16>,
}

/// Decodes one JSON string literal while the scanner walks it.
#[derive(Default)]
struct StringDecode {
    value: String,
    utf16_len: usize,
    leading_low: Option<u16>,
    last_lone: Option<(usize, u16)>,
}

impl StringDecode {
    fn push_char(&mut self, ch: char) {
        self.value.push(ch);
        self.utf16_len += ch.len_utf16();
    }

    fn push_escape(&mut self, escaped: u8) {
        let ch = match escaped {
            b'n' => '\n',
            b't' => '\t',
            b'r' => '\r',
            b'b' => '\u{0008}',
            b'f' => '\u{000C}',
            // `\"`, `\\`, `\/` decode to themselves; anything else is
            // invalid JSON that serde will reject later — the decoded
            // value is then never consulted, so a best-effort identity
            // mapping is fine.
            other => other as char,
        };
        self.push_char(ch);
    }

    fn push_lone_surrogate(&mut self, unit: u16) {
        if self.utf16_len == 0 && is_low_surrogate(unit) {
            self.leading_low = Some(unit);
        }
        self.last_lone = Some((self.utf16_len, unit));
        self.push_char('\u{FFFD}');
    }

    fn finish(self) -> Option<RepairedString> {
        let trailing_high = self.last_lone.and_then(|(idx, unit)| {
            (idx + 1 == self.utf16_len && is_high_surrogate(unit)).then_some(unit)
        });
        if self.leading_low.is_none() && trailing_high.is_none() {
            return None;
        }
        Some(RepairedString {
            value: self.value,
            leading_low: self.leading_low,
            trailing_high,
        })
    }
}

/// Replaces lone surrogate escapes in `body` with `�`.
///
/// Returns `None` when the body contains no lone surrogates.
fn repair_lone_surrogates(body: &str) -> Option<BodyRepair> {
    const ESCAPE_LEN: usize = 6; // \uXXXX

    let bytes = body.as_bytes();
    let mut repaired = String::new();
    let mut copied = 0; // start of the region not yet copied into `repaired`
    let mut changed = false;
    let mut strings = Vec::new();
    let mut cur: Option<StringDecode> = None; // Some while inside a string literal
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                match cur.take() {
                    None => cur = Some(StringDecode::default()),
                    Some(decode) => strings.extend(decode.finish()),
                }
                i += 1;
            }
            b'\\' if cur.is_some() => {
                let decode = cur.as_mut().expect("checked by match guard");
                match bytes.get(i + 1) {
                    Some(b'u') => match parse_hex4(&bytes[i + 2..]) {
                        Some(unit) if is_high_surrogate(unit) => {
                            if let Some(low) = low_surrogate_escape(&bytes[i + ESCAPE_LEN..]) {
                                decode.push_char(combine_surrogates(unit, low));
                                i += 2 * ESCAPE_LEN; // valid pair
                            } else {
                                repaired.push_str(&body[copied..i]);
                                repaired.push('\u{FFFD}');
                                i += ESCAPE_LEN;
                                copied = i;
                                changed = true;
                                decode.push_lone_surrogate(unit);
                            }
                        }
                        Some(unit) if is_low_surrogate(unit) => {
                            // A low surrogate reachable here has no preceding
                            // high (a valid pair is consumed as a whole above).
                            repaired.push_str(&body[copied..i]);
                            repaired.push('\u{FFFD}');
                            i += ESCAPE_LEN;
                            copied = i;
                            changed = true;
                            decode.push_lone_surrogate(unit);
                        }
                        Some(unit) => {
                            decode.push_char(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
                            i += ESCAPE_LEN;
                        }
                        None => i += 2, // malformed \u escape; leave as-is
                    },
                    Some(&escaped) if escaped.is_ascii() => {
                        decode.push_escape(escaped);
                        i += 2;
                    }
                    Some(_) => {
                        // `\` before a multi-byte char is invalid JSON, but
                        // the advance must still land on a char boundary or
                        // the slices below panic mid-character.
                        let ch = body[i + 1..].chars().next().unwrap_or('\u{FFFD}');
                        decode.push_char(ch);
                        i += 1 + ch.len_utf8();
                    }
                    None => i += 1,
                }
            }
            _ => {
                if let Some(decode) = cur.as_mut() {
                    let ch = body[i..].chars().next().unwrap_or('\u{FFFD}');
                    decode.push_char(ch);
                    i += ch.len_utf8();
                } else {
                    i += 1;
                }
            }
        }
    }

    if !changed {
        return None;
    }
    repaired.push_str(&body[copied..]);
    Some(BodyRepair { repaired, strings })
}

/// Decodes a body that is not valid UTF-8. WTF-8-encoded surrogates (the
/// raw-byte spelling of a lone surrogate: ED A0..BF 80..BF) become `\uXXXX`
/// escapes so the scanner treats both spellings of a split pair uniformly;
/// any other invalid sequence becomes U+FFFD.
fn decode_invalid_utf8(body: &[u8]) -> String {
    let mut out = String::with_capacity(body.len() + 8);
    let mut rest = body;
    loop {
        match std::str::from_utf8(rest) {
            Ok(tail) => {
                out.push_str(tail);
                return out;
            }
            Err(err) => {
                let (valid, bad) = rest.split_at(err.valid_up_to());
                if let Ok(valid) = std::str::from_utf8(valid) {
                    out.push_str(valid);
                }
                let is_wtf8_surrogate = bad.len() >= 3
                    && bad[0] == 0xED
                    && (0xA0..=0xBF).contains(&bad[1])
                    && (0x80..=0xBF).contains(&bad[2]);
                if is_wtf8_surrogate {
                    // An odd run of backslashes before the insertion point
                    // would swallow the escape's own backslash and forge the
                    // literal text `\uXXXX` — fall back to one U+FFFD there
                    // (one UTF-16 unit either way, so geometry holds).
                    let trailing = out.bytes().rev().take_while(|&b| b == b'\\').count();
                    if trailing % 2 == 0 {
                        let unit = (u32::from(bad[0] & 0x0F) << 12)
                            | (u32::from(bad[1] & 0x3F) << 6)
                            | u32::from(bad[2] & 0x3F);
                        out.push_str(&format!("\\u{unit:04X}"));
                    } else {
                        out.push('\u{FFFD}');
                    }
                    rest = &bad[3..];
                } else {
                    out.push('\u{FFFD}');
                    let skip = err.error_len().unwrap_or(bad.len()).max(1);
                    rest = &bad[skip.min(bad.len())..];
                }
            }
        }
    }
}

/// Combines a surrogate pair into the character it encodes.
fn combine_surrogates(high: u16, low: u16) -> char {
    let code = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
    // High/low ranges guarantee a valid supplementary code point.
    char::from_u32(code).unwrap_or('\u{FFFD}')
}

fn parse_hex4(bytes: &[u8]) -> Option<u16> {
    let hex = bytes.get(..4)?;
    // from_str_radix alone would also accept a sign (`+123`), which JSON
    // escapes never contain.
    if !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let hex = std::str::from_utf8(hex).ok()?;
    u16::from_str_radix(hex, 16).ok()
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

fn low_surrogate_escape(bytes: &[u8]) -> Option<u16> {
    if bytes.first() == Some(&b'\\') && bytes.get(1) == Some(&b'u') {
        parse_hex4(&bytes[2..]).filter(|&unit| is_low_surrogate(unit))
    } else {
        None
    }
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

    /// A raw backslash directly before a WTF-8 surrogate must not have the
    /// escape spelling appended after it — `\` + `\uD83D` reads back as an
    /// escaped backslash plus literal text, silently forging content.
    #[test]
    fn wtf8_after_raw_backslash_becomes_replacement_char() {
        let mut body = br#"{"a":"x\"#.to_vec();
        body.extend([0xED, 0xA0, 0xBD]);
        body.extend(br#""}"#);
        assert_eq!(decode_invalid_utf8(&body), "{\"a\":\"x\\\u{FFFD}\"}");
    }

    #[test]
    fn wtf8_after_escaped_backslash_keeps_escape_spelling() {
        let mut body = br#"{"a":"x\\"#.to_vec();
        body.extend([0xED, 0xA0, 0xBD]);
        body.extend(br#""}"#);
        assert_eq!(decode_invalid_utf8(&body), r#"{"a":"x\\\uD83D"}"#);
    }

    #[test]
    fn truncated_wtf8_tail_terminates_with_replacement_char() {
        assert_eq!(decode_invalid_utf8(b"a\xE3\x81"), "a\u{FFFD}");
    }

    #[test]
    fn hex_with_sign_is_not_an_escape() {
        assert_eq!(parse_hex4(b"+123"), None);
        assert_eq!(parse_hex4(b"D83D"), Some(0xD83D));
    }

    /// `\` followed by a raw multi-byte char used to advance the scanner
    /// into the middle of that char and panic on a non-boundary slice —
    /// which killed the pump task and hung the whole server.
    #[test]
    fn backslash_before_multibyte_char_does_not_panic() {
        let body = "{\"a\":\"\\é\\uD800\"}";
        let repair = repair_lone_surrogates(body).expect("lone surrogate should trigger repair");
        assert_eq!(repair.repaired, "{\"a\":\"\\é\u{FFFD}\"}");
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

    /// A Content-Length near usize::MAX used to wrap the frame-end addition
    /// and panic the pump on an inverted slice, permanently hanging the
    /// server (the reader never sees EOF). It must degrade to passthrough.
    #[tokio::test]
    async fn overflowing_content_length_degrades_to_passthrough() {
        let input = b"Content-Length: 18446744073709551615\r\n\r\n{}".to_vec();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(input.clone()),
        )
        .await
        .expect("pump must not hang");
        assert_eq!(out, input);
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

    /// Splits `bytes` back into LSP frames and parses each body as JSON.
    fn parse_frames(mut bytes: &[u8]) -> Vec<serde_json::Value> {
        let mut frames = Vec::new();
        while !bytes.is_empty() {
            let text = std::str::from_utf8(bytes).unwrap();
            let header_end = text.find("\r\n\r\n").unwrap() + 4;
            let len = parse_content_length(&bytes[..header_end]).unwrap();
            frames.push(serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap());
            bytes = &bytes[header_end + len..];
        }
        frames
    }

    fn didchange_body(uri: &str, version: u32, changes: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{uri}","version":{version}}},"contentChanges":[{changes}]}}}}"#,
        )
    }

    fn didchange_frame(uri: &str, version: u32, changes: &str) -> Vec<u8> {
        frame(&didchange_body(uri, version, changes))
    }

    fn insert_at(line: u32, character: u32, text: &str) -> String {
        format!(
            r#"{{"range":{{"start":{{"line":{line},"character":{character}}},"end":{{"line":{line},"character":{character}}}}},"rangeLength":0,"text":"{text}"}}"#,
        )
    }

    /// The client cut an emoji in half across two didChange chunks. The
    /// second chunk is an insertion at exactly the end of the first chunk's
    /// text and starts with the matching low surrogate, so the pair can be
    /// reassembled: the second edit is rewritten to replace the U+FFFD left
    /// by the first repair with the complete character.
    #[tokio::test]
    async fn split_pair_across_adjacent_didchange_chunks_is_reassembled() {
        let mut input = Vec::new();
        // "a" + high half: ends at (0,2) in UTF-16 units.
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        // Continuation: low half + "b" inserted at (0,2).
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");

        let frames = parse_frames(&out);
        assert_eq!(frames.len(), 2);
        let first = &frames[0]["params"]["contentChanges"][0];
        assert_eq!(first["text"], "a\u{FFFD}");
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(
            second["range"],
            serde_json::json!({"start":{"line":0,"character":1},"end":{"line":0,"character":2}}),
            "rewritten edit must replace the U+FFFD seam"
        );
        assert_eq!(second["rangeLength"], 1);
        assert_eq!(second["text"], "😃b");
    }

    #[tokio::test]
    async fn followup_at_different_position_keeps_replacement_chars() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        // Not the seam position: no reassembly, both halves stay U+FFFD.
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 7, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(second["text"], "\u{FFFD}b");
        assert_eq!(
            second["range"]["start"],
            serde_json::json!({"line":0,"character":7})
        );
    }

    #[tokio::test]
    async fn seam_survives_interleaved_requests_for_other_methods() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        // Read-only traffic between the chunks must not drop the seam.
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///t.md"},"position":{"line":0,"character":1}}}"#,
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(frames[2]["params"]["contentChanges"][0]["text"], "😃b");
    }

    /// Some serializers legally escape `/` in strings, spelling the method
    /// `textDocument\/didChange`. Seam invalidation must not depend on the
    /// spelling: a missed invalidation would leave a stale seam that later
    /// rewrites a live character (document corruption).
    #[tokio::test]
    async fn escaped_slash_didchange_still_invalidates_the_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        // Same document edited via the escaped method spelling: geometry
        // shifted, the seam must die.
        input.extend_from_slice(&frame(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument\/didChange","params":{{"textDocument":{{"uri":"file:\/\/\/t.md","version":2}},"contentChanges":[{}]}}}}"#,
            insert_at(0, 0, "hello"),
        )));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            3,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[2]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// An inspection pass (pending seam) over a frame the pump cannot even
    /// parse must not rewrite anything — not even the header casing.
    #[tokio::test]
    async fn inspection_of_unparseable_frame_forwards_byte_for_byte() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        let garbage = b"content-length: 8\r\n\r\nnot json".to_vec();
        input.extend_from_slice(&garbage);
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let tail = &out[out.len() - garbage.len()..];
        assert_eq!(tail, garbage.as_slice(), "garbage frame must stay verbatim");
    }

    #[tokio::test]
    async fn didclose_between_chunks_clears_the_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///t.md"}}}"#,
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[2]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    #[tokio::test]
    async fn full_text_sync_between_chunks_clears_the_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            r#"{"text":"fresh full content"}"#,
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            3,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[2]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    #[tokio::test]
    async fn multiline_chunk_tracks_seam_across_newlines() {
        let mut input = Vec::new();
        // "x\ny" + high half: seam ends at line 1, character 2.
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"x\ny\uD83D"),
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(1, 2, r"\uDE03"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(second["text"], "😃");
        assert_eq!(
            second["range"],
            serde_json::json!({"start":{"line":1,"character":1},"end":{"line":1,"character":2}})
        );
    }

    /// Both halves arriving inside ONE didChange message: change[0] opens
    /// the seam, change[1] (positions relative to the doc after change[0])
    /// consumes it.
    #[tokio::test]
    async fn intra_message_split_pair_is_reassembled() {
        let changes = format!(
            "{},{}",
            insert_at(0, 0, r"a\uD83D"),
            insert_at(0, 2, r"\uDE03b")
        );
        let input = didchange_frame("file:///t.md", 1, &changes);
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[0]["params"]["contentChanges"][1];
        assert_eq!(second["text"], "😃b");
        assert_eq!(
            second["range"],
            serde_json::json!({"start":{"line":0,"character":1},"end":{"line":0,"character":2}})
        );
    }

    /// Two changes whose repaired texts are the identical string must each
    /// pair with their own scanner record, in body order: record[0] is a
    /// trailing high, record[1] a leading low — swapping them would leave
    /// the pair unassembled.
    #[tokio::test]
    async fn identical_repaired_values_attribute_in_body_order() {
        let changes = format!(
            "{},{}",
            insert_at(0, 0, r"\uD83D"),
            insert_at(0, 1, r"\uDE03")
        );
        let input = didchange_frame("file:///t.md", 1, &changes);
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[0]["params"]["contentChanges"][1];
        assert_eq!(second["text"], "😃");
        assert_eq!(
            second["range"],
            serde_json::json!({"start":{"line":0,"character":0},"end":{"line":0,"character":1}})
        );
    }

    /// A lowercase `content-length` falling into the passthrough branch
    /// would reintroduce the original server death for that client.
    #[tokio::test]
    async fn lowercase_content_length_header_is_recognized() {
        let body = r#"{"text":"\uD83D"}"#;
        let input = format!("content-length: {}\r\n\r\n{}", body.len(), body).into_bytes();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        assert_eq!(out, frame(r#"{"text":"�"}"#));
    }

    #[tokio::test]
    async fn seam_survives_didchange_for_other_document() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///a.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        input.extend_from_slice(&didchange_frame("file:///b.md", 1, &insert_at(0, 0, "x")));
        input.extend_from_slice(&didchange_frame(
            "file:///a.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(frames[2]["params"]["contentChanges"][0]["text"], "😃b");
    }

    /// After `exit` the client may legally keep stdin open while waiting for
    /// the process to die; the pump must stop reading (EOF downstream)
    /// instead of parking another non-cancellable stdin read that would
    /// stall runtime shutdown.
    #[tokio::test]
    async fn pump_stops_after_exit_even_with_stdin_open() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut tx, rx) = tokio::io::duplex(1 << 20);
        let mut repaired = repair_inbound_frames(rx);
        let exit = frame(r#"{"jsonrpc":"2.0","method":"exit"}"#);
        tx.write_all(&exit).await.unwrap();
        // tx deliberately kept alive: no EOF from the "client".
        let mut out = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            repaired.read_to_end(&mut out),
        )
        .await
        .expect("pump must stop reading after exit")
        .unwrap();
        assert_eq!(out, exit);
        drop(tx);
    }

    /// Frames beyond the large-frame bypass are streamed (repair buffering
    /// would double peak memory) through the byte-length-preserving stage-1
    /// repair, and framing must resume cleanly on the next frame.
    #[tokio::test]
    async fn monster_frame_streams_through_with_length_preserving_repair() {
        let mut body = String::with_capacity(65 * 1024 * 1024);
        body.push_str(r#"{"pad":""#);
        body.push_str(&"a".repeat(64 * 1024 * 1024));
        body.push_str(r#"","x":"\uD83D"}"#);
        let mut input = frame(&body);
        input.extend_from_slice(&frame(r#"{"text":"\uD83D"}"#));
        let out = tokio::time::timeout(std::time::Duration::from_secs(60), pump_through(input))
            .await
            .expect("pump should finish");
        let repaired_body = body.replace(r"\uD83D", "\\uFFFD");
        assert_eq!(
            repaired_body.len(),
            body.len(),
            "repair is length-preserving"
        );
        let mut expected = frame(&repaired_body);
        expected.extend_from_slice(&frame(r#"{"text":"�"}"#));
        assert!(
            out == expected,
            "monster frame must stream through repaired and the next frame must repair too"
        );
    }

    fn stream_repair_all(input: &[u8], chunk: usize) -> Vec<u8> {
        let mut stream = StreamRepair::default();
        let mut out = Vec::new();
        for piece in input.chunks(chunk) {
            stream.feed(piece, &mut out);
        }
        stream.finish(&mut out);
        out
    }

    #[test]
    fn stream_repair_preserves_pairs_and_replaces_lone_surrogates() {
        let input = format!(r#"{{"a":"x{}{}y\uD83Dz"}}"#, r"\uD83D", r"\uDE03");
        let expected = format!(r#"{{"a":"x{}{}y{}z"}}"#, r"\uD83D", r"\uDE03", "\\uFFFD");
        for chunk in [1, 2, 3, 5, 7, 11, 64] {
            let out = stream_repair_all(input.as_bytes(), chunk);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                expected,
                "chunk size {chunk}"
            );
        }
    }

    #[test]
    fn stream_repair_replaces_wtf8_triplets_and_invalid_bytes() {
        let mut input = b"a".to_vec();
        input.extend([0xED, 0xA0, 0xBD]); // WTF-8 U+D83D
        input.push(0xFF);
        input.extend(b"b\xC3\xA9c"); // valid é must survive
        let mut expected = b"a".to_vec();
        expected.extend("\u{FFFD}".as_bytes());
        expected.extend(b"?b\xC3\xA9c");
        for chunk in [1, 2, 3, 64] {
            let out = stream_repair_all(&input, chunk);
            assert_eq!(out.len(), input.len(), "chunk size {chunk}");
            assert_eq!(out, expected, "chunk size {chunk}");
        }
    }

    #[test]
    fn stream_repair_keeps_escaped_backslash_text() {
        let input = br#"{"a":"\\uD83D"}"#;
        for chunk in [1, 4, 64] {
            assert_eq!(stream_repair_all(input, chunk), input.to_vec());
        }
    }

    /// Seam keys must match downstream document identity: an edit through an
    /// alternate spelling of the same URI (here a dot segment) must still
    /// invalidate the seam.
    #[tokio::test]
    async fn alternate_uri_spelling_still_invalidates_the_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///tmp/./t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///tmp/t.md",
            2,
            &insert_at(0, 0, "x"),
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///tmp/./t.md",
            3,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[2]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// A replacement's inserted text still lands at range.start, so a
    /// trailing lone high in a replacement opens a consumable seam.
    #[tokio::test]
    async fn replacement_with_trailing_high_opens_a_seam() {
        let mut input = Vec::new();
        let replacement = r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":5}},"text":"abc\uD83D"}"#;
        input.extend_from_slice(&didchange_frame("file:///t.md", 1, replacement));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 4, r"\uDE03rest"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(second["text"], "😃rest");
        assert_eq!(
            second["range"],
            serde_json::json!({"start":{"line":0,"character":3},"end":{"line":0,"character":4}})
        );
    }

    /// An abandoned seam expires after a bounded number of frames so it
    /// cannot force the inspection slow path for the rest of the session.
    #[tokio::test]
    async fn abandoned_seam_expires_after_ttl_frames() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        for n in 0..40 {
            input.extend_from_slice(&frame(&format!(
                r#"{{"jsonrpc":"2.0","id":{n},"method":"textDocument/hover","params":{{"textDocument":{{"uri":"file:///t.md"}},"position":{{"line":0,"character":0}}}}}}"#,
            )));
        }
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[41]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// A didChange the downstream drops (typed validation fails: version is
    /// a string) must not create seam state — the edit never happens, so a
    /// later continuation at its coordinates must stay U+FFFD.
    #[tokio::test]
    async fn invalid_didchange_shape_does_not_open_a_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&frame(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"file:///t.md","version":"bad"}},"contentChanges":[{}]}}}}"#,
            insert_at(0, 0, r"a\uD83D"),
        )));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[1]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// Equal text values where only one occurrence contributed a scanner
    /// record: body-order pairing would hand the record to the literal-FFFD
    /// text sitting at the seam. Both must stay unrewritten.
    #[tokio::test]
    async fn duplicate_texts_with_missing_record_block_reassembly() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        let changes = format!(
            r#"{{"range":{{"start":{{"line":0,"character":2}},"end":{{"line":0,"character":2}}}},"text":"{}z"}},{}"#,
            '\u{FFFD}',
            insert_at(9, 0, r"\uDE03z"),
        );
        input.extend_from_slice(&didchange_frame("file:///t.md", 2, &changes));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let first = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(first["text"], "\u{FFFD}z", "literal U+FFFD must survive");
        assert_eq!(
            first["range"]["start"],
            serde_json::json!({"line":0,"character":2})
        );
    }

    /// A record from an unrelated string (here an extension property) whose
    /// decoded value equals a text field must not drive a rewrite: the text's
    /// U+FFFD here is literal client content, not a repaired surrogate.
    #[tokio::test]
    async fn equal_valued_sibling_string_blocks_reassembly() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        input.extend_from_slice(&frame(&format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"file:///t.md","version":2}},"ext":"\uDE03z","contentChanges":[{{"range":{{"start":{{"line":0,"character":2}},"end":{{"line":0,"character":2}}}},"text":"{}z"}}]}}}}"#,
            '\u{FFFD}'
        )));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let change = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(change["text"], "\u{FFFD}z", "literal U+FFFD must survive");
        assert_eq!(
            change["range"]["start"],
            serde_json::json!({"line":0,"character":2})
        );
    }

    /// LSP positions are u32; a u64-sized character used to overflow the
    /// seam position arithmetic (debug panic → dead pump) or wrap in
    /// release, recording a bogus seam for a message the downstream rejects.
    #[tokio::test]
    async fn position_beyond_u32_never_tracks_a_seam() {
        let mut input = Vec::new();
        let big = r#"{"range":{"start":{"line":0,"character":18446744073709551615},"end":{"line":0,"character":18446744073709551615}},"text":"a\uD83D"}"#;
        input.extend_from_slice(&didchange_frame("file:///t.md", 1, big));
        // Release-mode wrap would have put the seam at character 1.
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 1, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump must not die");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[1]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// A REPLACEMENT starting exactly at the seam consumes it too: its
    /// inserted low half is adjacent to the high half in the client's
    /// buffer, so widening the range by the `�` unit is geometry-exact.
    #[tokio::test]
    async fn replacement_at_seam_position_consumes_the_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        let replacement = r#"{"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":3}},"rangeLength":1,"text":"\uDE03b"}"#;
        input.extend_from_slice(&didchange_frame("file:///t.md", 2, replacement));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(second["text"], "😃b");
        assert_eq!(
            second["range"],
            serde_json::json!({"start":{"line":0,"character":1},"end":{"line":0,"character":3}})
        );
        assert_eq!(second["rangeLength"], 2, "widened by the seam unit");
    }

    #[tokio::test]
    async fn truncated_final_frame_is_flushed_verbatim() {
        let input = b"Content-Length: 100\r\n\r\n{\"partial".to_vec();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(input.clone()),
        )
        .await
        .expect("pump should finish");
        assert_eq!(out, input);
    }

    /// A full-text sync whose text ends in a lone high opens a seam measured
    /// from (0,0); the next chunk consumes it like any other.
    #[tokio::test]
    async fn full_sync_trailing_high_opens_a_consumable_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame("file:///t.md", 1, r#"{"text":"a\uD83D"}"#));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(frames[1]["params"]["contentChanges"][0]["text"], "😃b");
    }

    #[test]
    fn advance_utf16_counts_crlf_and_lone_cr_as_one_break() {
        assert_eq!(advance_utf16(0, 0, "x\r\ny"), (1, 1));
        assert_eq!(advance_utf16(0, 0, "x\ry"), (1, 1));
        assert_eq!(advance_utf16(0, 0, "😃"), (0, 2));
    }

    /// The rewrite must not inject the deprecated rangeLength key when the
    /// client omitted it.
    #[tokio::test]
    async fn rewrite_does_not_inject_absent_range_length() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        let continuation = r#"{"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":2}},"text":"\uDE03b"}"#;
        input.extend_from_slice(&didchange_frame("file:///t.md", 2, continuation));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(second["text"], "😃b");
        assert!(second.get("rangeLength").is_none());
    }

    #[tokio::test]
    async fn didopen_between_chunks_clears_the_seam() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        input.extend_from_slice(&frame(
            r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.md","languageId":"markdown","version":1,"text":"fresh"}}}"#,
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[2]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// Overflowing the seam valve wipes stale seams: a continuation for an
    /// early document must fall back to stage-1 U+FFFD.
    #[tokio::test]
    async fn seam_valve_overflow_drops_stale_seams() {
        let mut input = Vec::new();
        for n in 0..9 {
            input.extend_from_slice(&didchange_frame(
                &format!("file:///doc{n}.md"),
                1,
                &insert_at(0, 0, r"a\uD83D"),
            ));
        }
        input.extend_from_slice(&didchange_frame(
            "file:///doc0.md",
            2,
            &insert_at(0, 2, r"\uDE03b"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(
            frames[9]["params"]["contentChanges"][0]["text"],
            "\u{FFFD}b"
        );
    }

    /// The field pattern: middle chunks both start with a low half and end
    /// with a high half, so one frame consumes a seam and opens the next.
    #[tokio::test]
    async fn chain_of_three_chunks_reassembles_every_pair() {
        let mut input = Vec::new();
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            1,
            &insert_at(0, 0, r"a\uD83D"),
        ));
        // Consumes the first seam (at 0,2), ends with another high half.
        // Inserted 3 units (low + b + high) -> next seam at 0,5.
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            2,
            &insert_at(0, 2, r"\uDE03b\uD83D"),
        ));
        input.extend_from_slice(&didchange_frame(
            "file:///t.md",
            3,
            &insert_at(0, 5, r"\uDE03c"),
        ));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        let second = &frames[1]["params"]["contentChanges"][0];
        assert_eq!(second["text"], "😃b\u{FFFD}");
        let third = &frames[2]["params"]["contentChanges"][0];
        assert_eq!(third["text"], "😃c");
        assert_eq!(
            third["range"],
            serde_json::json!({"start":{"line":0,"character":4},"end":{"line":0,"character":5}})
        );
    }

    fn frame_bytes(body: &[u8]) -> Vec<u8> {
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// Replaces `placeholder` in `body` with raw (possibly invalid) bytes.
    fn splice_raw(body: &str, placeholder: &str, raw: &[u8]) -> Vec<u8> {
        let pos = body.find(placeholder).expect("placeholder present");
        let mut out = body.as_bytes()[..pos].to_vec();
        out.extend_from_slice(raw);
        out.extend_from_slice(&body.as_bytes()[pos + placeholder.len()..]);
        out
    }

    /// A client that ships lone surrogates as raw WTF-8 bytes instead of
    /// escapes produces a body that is not valid UTF-8; the codec's own
    /// from_utf8 would error and fuse the stream just like a serde failure.
    #[tokio::test]
    async fn raw_wtf8_lone_surrogate_is_repaired() {
        // ED A0 BD = WTF-8 for U+D83D (high half of the emoji).
        let body = splice_raw(r#"{"method":"x","text":"a@@"}"#, "@@", &[0xED, 0xA0, 0xBD]);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(frame_bytes(&body)),
        )
        .await
        .expect("pump should finish");
        assert_eq!(out, frame(r#"{"method":"x","text":"a�"}"#));
    }

    /// Raw WTF-8 chunks pair up exactly like escape-spelled ones.
    #[tokio::test]
    async fn raw_wtf8_split_pair_is_reassembled() {
        let body1 = splice_raw(
            &didchange_body("file:///t.md", 1, &insert_at(0, 0, "a@@")),
            "@@",
            &[0xED, 0xA0, 0xBD], // U+D83D
        );
        let body2 = splice_raw(
            &didchange_body("file:///t.md", 2, &insert_at(0, 2, "@@b")),
            "@@",
            &[0xED, 0xB8, 0x83], // U+DE03
        );
        let mut input = frame_bytes(&body1);
        input.extend_from_slice(&frame_bytes(&body2));
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let frames = parse_frames(&out);
        assert_eq!(frames[1]["params"]["contentChanges"][0]["text"], "😃b");
    }

    #[tokio::test]
    async fn arbitrary_invalid_bytes_become_replacement_chars() {
        let body = splice_raw(r#"{"method":"x","text":"a@@b"}"#, "@@", &[0xFF]);
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pump_through(frame_bytes(&body)),
        )
        .await
        .expect("pump should finish");
        assert_eq!(out, frame(r#"{"method":"x","text":"a�b"}"#));
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

    /// httparse (the codec's header parser) accepts bare-LF header blocks;
    /// the pump must not hang buffering forever on input the codec accepts.
    #[tokio::test]
    async fn bare_lf_header_block_is_framed_and_repaired() {
        let body = r#"{"text":"\uD83D"}"#;
        let input = format!("Content-Length: {}\n\n{}", body.len(), body).into_bytes();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let repaired = r#"{"text":"�"}"#;
        // The CL line is canonicalized to CRLF; the bare-LF blank line that
        // terminates the block is preserved byte-for-byte.
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Content-Length: {}\r\n\n{}", repaired.len(), repaired)
        );
    }

    /// httparse also accepts a bare-LF header line followed by a CRLF blank
    /// line (`\n\r\n`); missing it would withhold the frame and then frame a
    /// later jumbo block against the wrong Content-Length.
    #[tokio::test]
    async fn mixed_lf_crlf_terminator_is_framed_and_repaired() {
        let body = r#"{"text":"\uD83D"}"#;
        let input = format!("Content-Length: {}\n\r\n{}", body.len(), body).into_bytes();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let repaired = r#"{"text":"�"}"#;
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("Content-Length: {}\r\n\r\n{}", repaired.len(), repaired)
        );
    }

    /// The downstream codec's header loop takes the LAST Content-Length; the
    /// pump must frame identically or a rewrite desyncs the stream.
    #[tokio::test]
    async fn duplicate_content_length_takes_the_last_value() {
        let body = r#"{"text":"\uD83D"}"#;
        let input = format!(
            "Content-Length: 5\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), pump_through(input))
            .await
            .expect("pump should finish");
        let repaired = r#"{"text":"�"}"#;
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "Content-Length: {len}\r\nContent-Length: {len}\r\n\r\n{repaired}",
                len = repaired.len()
            )
        );
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
