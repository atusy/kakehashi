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

const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

async fn forward_frames(mut input: impl AsyncRead + Send + Unpin, mut output: DuplexStream) {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
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

/// A lone high surrogate seen at the end of a didChange insertion, waiting
/// for the matching low surrogate at the start of the next chunk.
struct PendingHigh {
    unit: u16,
    /// UTF-16 position just past the inserted `�` seam.
    line: u64,
    character: u64,
}

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
}

impl FrameRepairer {
    /// Processes one frame body, returning `None` when it can be forwarded
    /// byte-for-byte.
    fn process(&mut self, body: &[u8]) -> Option<String> {
        // Bodies that are not valid UTF-8 would fuse the downstream codec
        // exactly like a serde failure; decode them first. WTF-8 surrogate
        // triplets become `\uXXXX` escapes so both spellings of a split
        // pair flow through the same repair below.
        let decoded = match std::str::from_utf8(body) {
            Ok(_) => None,
            Err(_) => Some(decode_invalid_utf8(body)),
        };
        let text: &str = match &decoded {
            Some(owned) => owned,
            None => std::str::from_utf8(body).ok()?,
        };
        // Cheap gate: surrogate escapes start with `\ud` or `\uD` (first hex
        // digit of D800..DFFF); bodies without them are forwarded untouched
        // unless a pending seam requires inspecting document notifications.
        let has_surrogate_escape = text
            .as_bytes()
            .windows(3)
            .any(|w| w[0] == b'\\' && w[1] == b'u' && (w[2] == b'd' || w[2] == b'D'));
        let needs_inspection =
            !self.pending.is_empty() && find_subslice(body, b"textDocument/did").is_some();
        if decoded.is_none() && !has_surrogate_escape && !needs_inspection {
            return None;
        }
        let repair = if has_surrogate_escape {
            repair_lone_surrogates(text)
        } else {
            None
        };
        if decoded.is_none() && repair.is_none() && !needs_inspection {
            return None;
        }

        let repaired_text = repair.as_ref().map_or(text, |r| r.repaired.as_str());
        let Ok(mut msg) = serde_json::from_str::<serde_json::Value>(repaired_text) else {
            // Invalid JSON beyond lone surrogates; forward our best repair.
            return Some(repaired_text.to_owned());
        };
        let strings = repair.as_ref().map_or(&[][..], |r| r.strings.as_slice());
        let rewritten = match msg.get("method").and_then(|m| m.as_str()) {
            Some("textDocument/didChange") => self.process_didchange(&mut msg, strings),
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
        } else if repair.is_some() || decoded.is_some() {
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
        let Some(changes) = msg
            .pointer_mut("/params/contentChanges")
            .and_then(|c| c.as_array_mut())
        else {
            return false;
        };
        let mut used = vec![false; strings.len()];
        let mut dirty = false;
        for change in changes.iter_mut() {
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
            let entry = strings
                .iter()
                .enumerate()
                .position(|(k, s)| !used[k] && s.value == text)
                .map(|k| {
                    used[k] = true;
                    &strings[k]
                });

            let start = match change.get("range") {
                None => {
                    // Full-text sync replaces the document: any seam is gone.
                    self.pending.remove(&uri);
                    Some((0, 0))
                }
                Some(range) => range_start_of_insertion(range),
            };
            let is_ranged_insert = change.get("range").is_some() && start.is_some();

            if let (Some((line, character)), Some(entry)) = (start, entry) {
                let consumed = is_ranged_insert
                    && entry.leading_low.is_some()
                    && self.pending.get(&uri).is_some_and(|p| {
                        p.line == line && p.character == character && p.character >= 1
                    });
                if consumed {
                    let Some(p) = self.pending.remove(&uri) else {
                        continue;
                    };
                    let low = entry.leading_low.unwrap_or(0xDC00);
                    let pair = combine_surrogates(p.unit, low);
                    let new_text: String = format!("{}{}", pair, &text['\u{FFFD}'.len_utf8()..]);
                    change["range"]["start"]["character"] =
                        serde_json::Value::from(p.character - 1);
                    if change.get("rangeLength").is_some() {
                        change["rangeLength"] = serde_json::Value::from(1);
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

    fn remember_seam(&mut self, uri: &str, unit: u16, line: u64, character: u64) {
        // Safety valve against pathological clients that open seams on many
        // documents and never continue them.
        if self.pending.len() >= 8 && !self.pending.contains_key(uri) {
            self.pending.clear();
        }
        self.pending.insert(
            uri.to_owned(),
            PendingHigh {
                unit,
                line,
                character,
            },
        );
    }
}

fn document_uri(msg: &serde_json::Value) -> Option<String> {
    msg.pointer("/params/textDocument/uri")
        .and_then(|u| u.as_str())
        .map(str::to_owned)
}

/// Returns the start position when `range` denotes an insertion
/// (start == end), else `None`.
fn range_start_of_insertion(range: &serde_json::Value) -> Option<(u64, u64)> {
    let pos = |which: &str| {
        let p = range.get(which)?;
        Some((p.get("line")?.as_u64()?, p.get("character")?.as_u64()?))
    };
    let start = pos("start")?;
    (start == pos("end")?).then_some(start)
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
pub(crate) struct BodyRepair {
    /// The body with every lone surrogate escape replaced by `�`.
    pub(crate) repaired: String,
    /// Strings that contained a lone surrogate at an edge, in body order.
    pub(crate) strings: Vec<RepairedString>,
}

/// A JSON string that had a lone surrogate as its first or last UTF-16 unit.
///
/// The original code units are recorded here because the repair erases them:
/// they are what lets a later frame reassemble a split surrogate pair.
#[derive(Debug, PartialEq)]
pub(crate) struct RepairedString {
    /// Decoded value after repair — exactly what serde_json will parse from
    /// the repaired body, used to attribute this record to a `text` field.
    pub(crate) value: String,
    /// Lone low surrogate that was the string's first UTF-16 unit.
    pub(crate) leading_low: Option<u16>,
    /// Lone high surrogate that was the string's last UTF-16 unit.
    pub(crate) trailing_high: Option<u16>,
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
pub(crate) fn repair_lone_surrogates(body: &str) -> Option<BodyRepair> {
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
                    i += ch.len_utf8().max(1);
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
                if bad.len() >= 3
                    && bad[0] == 0xED
                    && (0xA0..=0xBF).contains(&bad[1])
                    && (0x80..=0xBF).contains(&bad[2])
                {
                    let unit = (u32::from(bad[0] & 0x0F) << 12)
                        | (u32::from(bad[1] & 0x3F) << 6)
                        | u32::from(bad[2] & 0x3F);
                    out.push_str(&format!("\\u{unit:04X}"));
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
