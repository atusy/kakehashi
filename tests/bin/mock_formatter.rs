//! Minimal mock LSP formatter for E2E tests of the concatenated formatting
//! pipeline (`tests/e2e_concatenated_formatting.rs`).
//!
//! Speaks just enough LSP over stdio to participate in the bridge: it answers
//! `initialize` with formatting capabilities, tracks document text via
//! `didOpen`/`didChange`/`didClose`, and answers formatting requests with a
//! single whole-document replacement edit whose content is a deterministic
//! transformation of the tracked text. The transformation is selected by the
//! first CLI argument so a chain of two instances can prove serial pipeline
//! order end-to-end:
//!
//! - `upper` — advertises `documentFormattingProvider`; uppercases the text.
//! - `append` — advertises `documentFormattingProvider`; appends a
//!   `-- mock-marker` line (lowercase, so a later `upper` step would be
//!   detectable).
//! - `range-upper` — advertises ONLY `documentRangeFormattingProvider`;
//!   uppercases the text. Exercises the pipeline's capability-based
//!   whole-region rangeFormatting fallback (concatenated-formatting-pipeline
//!   Decision point 3.2).
//! - `definition` — advertises `definitionProvider` + `hoverProvider`;
//!   answers definition with a fixed Location that **echoes the requested
//!   URI** (and hover with the URI in the contents), but only for documents
//!   it received via `didOpen`. Used by `tests/e2e_host_bridge.rs` to prove
//!   the host bridge forwards the real client URI and returns the response
//!   verbatim (host-document-bridge).
//!
//! Only built for E2E runs (`required-features = ["e2e"]` in Cargo.toml).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "upper".to_string());
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    let mut documents: HashMap<String, String> = HashMap::new();

    while let Some(message) = read_message(&mut reader) {
        let method = message
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        let id = message.get("id").cloned();

        match method {
            "initialize" => {
                let capabilities = match mode.as_str() {
                    "range-upper" => json!({
                        "documentRangeFormattingProvider": true,
                        "textDocumentSync": 1
                    }),
                    "definition" => json!({
                        "definitionProvider": true,
                        "hoverProvider": true,
                        "textDocumentSync": 1
                    }),
                    _ => json!({
                        "documentFormattingProvider": true,
                        "textDocumentSync": 1
                    }),
                };
                respond(&mut writer, id, json!({ "capabilities": capabilities }));
            }
            "shutdown" => respond(&mut writer, id, Value::Null),
            "exit" => break,
            "textDocument/didOpen" => {
                if let (Some(uri), Some(text)) = (
                    message
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str),
                    message
                        .pointer("/params/textDocument/text")
                        .and_then(Value::as_str),
                ) {
                    documents.insert(uri.to_string(), text.to_string());
                }
            }
            "textDocument/didChange" => {
                // Full-sync only (textDocumentSync: 1): the last content
                // change carries the complete new text.
                if let (Some(uri), Some(text)) = (
                    message
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str),
                    message
                        .pointer("/params/contentChanges")
                        .and_then(Value::as_array)
                        .and_then(|changes| changes.last())
                        .and_then(|change| change.get("text"))
                        .and_then(Value::as_str),
                ) {
                    documents.insert(uri.to_string(), text.to_string());
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                {
                    documents.remove(uri);
                }
            }
            "textDocument/definition" => {
                // Echo the requested URI back in a fixed Location — but only
                // for documents this server actually received via didOpen,
                // so the test also proves the host document was synced.
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .filter(|uri| documents.contains_key(*uri))
                    .map(|uri| {
                        json!({
                            "uri": uri,
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 4 }
                            }
                        })
                    })
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            "textDocument/hover" => {
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .filter(|uri| documents.contains_key(*uri))
                    .map(|uri| json!({ "contents": format!("mock-hover:{uri}") }))
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            "textDocument/formatting" | "textDocument/rangeFormatting" => {
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| documents.get(uri))
                    .map(|text| whole_document_edit(text, &mode))
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            _ => {
                // Unknown REQUESTS get a null result so the client never
                // hangs; notifications are ignored.
                if id.is_some() {
                    respond(&mut writer, id, Value::Null);
                }
            }
        }
    }
}

/// Apply the mode's transformation and wrap it in a single whole-document
/// `TextEdit[]`. The end position stays within the document's real line
/// count (the bridge drops edits past the virtual EOF).
fn whole_document_edit(text: &str, mode: &str) -> Value {
    let new_text = match mode {
        "append" => {
            // Keep the trailing newline shape so the host document's closing
            // fence stays on its own line when the edit is applied.
            match text.strip_suffix('\n') {
                Some(stripped) => format!("{stripped}\n-- mock-marker\n"),
                None => format!("{text}\n-- mock-marker"),
            }
        }
        // "upper" and "range-upper"
        _ => text.to_uppercase(),
    };

    let end_line = text.matches('\n').count();
    let last_line_start = text.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end_character = text[last_line_start..].encode_utf16().count();

    json!([{
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": end_line, "character": end_character }
        },
        "newText": new_text
    }])
}

/// Read one Content-Length-framed JSON-RPC message; `None` on EOF or framing
/// errors (the main loop then exits).
fn read_message<R: BufRead>(reader: &mut R) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; content_length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Send a JSON-RPC success response for `id` (no-op for notifications).
fn respond<W: Write>(writer: &mut W, id: Option<Value>, result: Value) {
    let Some(id) = id else {
        return;
    };
    let body = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
    let _ = write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = writer.flush();
}
