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
//! - `options-echo` — advertises `documentFormattingProvider`; replaces the
//!   text with a line echoing the received `FormattingOptions` (`tabSize`,
//!   `insertSpaces`), so tests can assert the options a client sent actually
//!   reach the downstream server (e.g. `kakehashi format --tab-size`).
//! - `fail-request` — advertises `documentFormattingProvider`, handshakes
//!   normally, then answers every formatting request with a JSON-RPC error.
//!   Exercises the request-time failure path (vs. a server that never
//!   starts), which `kakehashi format` must report instead of exiting 0.
//! - `malformed` — advertises `documentFormattingProvider`, handshakes
//!   normally, then answers formatting with a JSON-RPC *success* whose
//!   `result` is not a `TextEdit[]`. Exercises the malformed-payload
//!   request-failure path.
//! - `code-lens` — advertises `codeLensProvider` with `resolveProvider`;
//!   answers `textDocument/codeLens` with one UNRESOLVED lens (data only) and
//!   `codeLens/resolve` by materializing a command that echoes the lens data.
//!   Used by `tests/e2e_code_lens_resolve.rs` (#355).
//! - `diagnostics` — advertises `diagnosticProvider`; answers
//!   `textDocument/diagnostic` with a full report carrying one diagnostic
//!   that echoes the requested URI, but only for documents it received via
//!   `didOpen`. Used to prove cross-layer diagnostic aggregation merges the
//!   host layer in (cross-layer-aggregation).
//! - `on-type` — advertises `documentOnTypeFormattingProvider` with `}` and
//!   `;` as triggers; answers `textDocument/onTypeFormatting` with the
//!   uppercasing whole-document edit for ANY typed character (bridge-side
//!   trigger filtering is what `tests/e2e_on_type_formatting.rs` proves).
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
                    "code-lens" => json!({
                        "codeLensProvider": { "resolveProvider": true },
                        "textDocumentSync": 1
                    }),
                    "diagnostics" => json!({
                        "diagnosticProvider": {
                            "interFileDependencies": false,
                            "workspaceDiagnostics": false
                        },
                        "textDocumentSync": 1
                    }),
                    "on-type" => json!({
                        "documentOnTypeFormattingProvider": {
                            "firstTriggerCharacter": "}",
                            "moreTriggerCharacter": [";"]
                        },
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
            "textDocument/codeLens" => {
                // One UNRESOLVED lens (data only, no command) on the first
                // line of the (virtual) document — the rust-analyzer shape
                // that motivates codeLens/resolve support (#355).
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .filter(|uri| documents.contains_key(*uri))
                    .map(|_| {
                        json!([{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 5 }
                            },
                            "data": { "mock": "lens-1" }
                        }])
                    })
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            "textDocument/diagnostic" => {
                // One deterministic diagnostic echoing the requested URI —
                // but only for documents this server actually received via
                // didOpen, so the test also proves the host document was
                // synced before the pull.
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .filter(|uri| documents.contains_key(*uri))
                    .map(|uri| {
                        json!({
                            "kind": "full",
                            "items": [{
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 1 }
                                },
                                "severity": 2,
                                "message": format!("mock-diagnostic:{uri}")
                            }]
                        })
                    })
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            "codeLens/resolve" => {
                // Materialize the command, echoing the lens's own data back so
                // the test can prove the downstream data round-tripped through
                // the bridge envelope.
                let data = message
                    .pointer("/params/data")
                    .cloned()
                    .unwrap_or(Value::Null);
                let range = message
                    .pointer("/params/range")
                    .cloned()
                    .unwrap_or(Value::Null);
                respond(
                    &mut writer,
                    id,
                    json!({
                        "range": range,
                        "command": {
                            "title": format!("mock resolved:{}", data["mock"].as_str().unwrap_or("?")),
                            "command": "mock.codelens"
                        },
                        "data": data
                    }),
                );
            }
            "textDocument/onTypeFormatting" => {
                // Answer with the whole-document transformation REGARDLESS of
                // the typed character: the bridge is supposed to filter
                // undeclared triggers before the request ever reaches this
                // server, so a null upstream result for an undeclared char
                // proves bridge-side filtering, not mock refusal.
                let options = message.pointer("/params/options").cloned();
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| documents.get(uri))
                    .map(|text| whole_document_edit(text, &mode, options.as_ref()))
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            "textDocument/formatting" | "textDocument/rangeFormatting" => {
                if mode == "fail-request" {
                    // Healthy handshake, broken request: exercises the
                    // request-time failure path (vs. a server that never
                    // starts), which clients must not read as "no edits".
                    respond_error(&mut writer, id, -32603, "mock formatter request failure");
                    continue;
                }
                if mode == "malformed" {
                    // JSON-RPC success whose result is not TextEdit[]: a
                    // protocol-invalid formatter that must count as a request
                    // failure, not as "no edits".
                    respond(&mut writer, id, json!("not-a-textedit-array"));
                    continue;
                }
                let options = message.pointer("/params/options").cloned();
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|uri| documents.get(uri))
                    .map(|text| whole_document_edit(text, &mode, options.as_ref()))
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
fn whole_document_edit(text: &str, mode: &str, options: Option<&Value>) -> Value {
    let new_text = match mode {
        "append" => {
            // Keep the trailing newline shape so the host document's closing
            // fence stays on its own line when the edit is applied.
            match text.strip_suffix('\n') {
                Some(stripped) => format!("{stripped}\n-- mock-marker\n"),
                None => format!("{text}\n-- mock-marker"),
            }
        }
        "options-echo" => {
            let tab_size = options
                .and_then(|o| o.get("tabSize"))
                .map(Value::to_string)
                .unwrap_or_else(|| "missing".to_string());
            let insert_spaces = options
                .and_then(|o| o.get("insertSpaces"))
                .map(Value::to_string)
                .unwrap_or_else(|| "missing".to_string());
            format!("-- tabSize={tab_size} insertSpaces={insert_spaces}\n")
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

/// Send a JSON-RPC error response (`fail-request` mode).
fn respond_error<W: Write>(writer: &mut W, id: Option<Value>, code: i64, message: &str) {
    let Some(id) = id else {
        return;
    };
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string();
    let _ = write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = writer.flush();
}
