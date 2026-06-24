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
//! - `will-save` — advertises `hoverProvider` + a `textDocumentSync` Options
//!   block with `willSave`, `willSaveWaitUntil`, and `save` true. Records every
//!   `textDocument/willSave` (count + last reason + last URI) and
//!   `textDocument/didSave` (count + last URI), and answers
//!   `textDocument/willSaveWaitUntil` with a save-time edit echoing the
//!   requested URI (only for documents synced via `didOpen`). `hover` returns
//!   the recorded state as a JSON string (`{will,reason,willUri,did,didUri}`),
//!   so a test can prove the notifications reached this server carrying the URI
//!   it knows — the host URI for a host server, the *virtual* URI for a virt
//!   server. Used by `tests/e2e_host_bridge.rs` to prove host- AND virt-bridge
//!   willSave/didSave forwarding (#357).
//! - `will-save-slow` — like `will-save`, but sleeps 8s before answering
//!   `willSaveWaitUntil`, past kakehashi's 5s save budget. Lets the test prove
//!   the bridge times out and returns null near 5s instead of hanging the save
//!   on the 30s request timeout (#357 Q3).
//! - `will-save-incapable` — like `will-save` (records + reports save state via
//!   hover) but advertises NEITHER `willSave` nor `save`, so the bridge's
//!   per-server capability gate must skip it and its counts stay zero (#357).
//! - `notify` — right after answering `initialize`, emits a
//!   `window/showMessage` followed by a `window/logMessage` notification.
//!   Used by `tests/e2e_window_notifications.rs` to prove the bridge forwards
//!   both window/* notifications unconditionally (#378). showMessage is sent
//!   FIRST, and the bridge preserves order, so the test asserts showMessage
//!   arrives ahead of logMessage.
//! - `workspace-folders` — advertises `workspace.workspaceFolders.{supported,
//!   changeNotifications}` + `hoverProvider`; records the `initialize`-time
//!   workspace folders and every `workspace/didChangeWorkspaceFolders`
//!   addition, then answers `textDocument/hover` with the sorted list of folder
//!   URIs it currently knows (not gated on `didOpen` — the folder set is the
//!   subject under test, populated by initialize + didChangeWorkspaceFolders).
//!   Used by
//!   `tests/e2e_shared_instance.rs` (#391) to prove the shared-instance opt-in
//!   grows one downstream process's folder set across roots.
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
    // `workspace-folders` mode: every folder URI this server has been told
    // about, via `initialize` params and `workspace/didChangeWorkspaceFolders`.
    let mut workspace_folders: Vec<String> = Vec::new();
    // `will-save` mode: counts + last-seen URI for the willSave/didSave
    // notifications, reported back via hover so the test can prove they arrived
    // and carried the right document URI (the virtual URI for a virt server,
    // the host URI for a host server) (#357).
    let mut will_save_count: usize = 0;
    let mut last_will_save_reason: i64 = 0;
    let mut last_will_save_uri: Option<String> = None;
    let mut did_save_count: usize = 0;
    let mut last_did_save_uri: Option<String> = None;

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
                    "will-save" | "will-save-slow" => json!({
                        "hoverProvider": true,
                        "textDocumentSync": {
                            "openClose": true,
                            "change": 1,
                            "willSave": true,
                            "willSaveWaitUntil": true,
                            "save": { "includeText": false }
                        }
                    }),
                    // Records willSave/didSave like `will-save`, but advertises
                    // NEITHER save flag — so the bridge's per-server capability
                    // gate must skip it (its hover state stays at zero) (#357).
                    "will-save-incapable" => json!({
                        "hoverProvider": true,
                        "textDocumentSync": 1
                    }),
                    "workspace-folders" => json!({
                        "hoverProvider": true,
                        "textDocumentSync": 1,
                        "workspace": {
                            "workspaceFolders": {
                                "supported": true,
                                "changeNotifications": true
                            }
                        }
                    }),
                    // Like `workspace-folders` but does NOT advertise the
                    // workspaceFolders capability, so a `preferSharedInstance`
                    // opt-in must fall back to per-root instances (#391).
                    "workspace-folders-incapable" => json!({
                        "hoverProvider": true,
                        "textDocumentSync": 1
                    }),
                    _ => json!({
                        "documentFormattingProvider": true,
                        "textDocumentSync": 1
                    }),
                };
                // Record the initialize-time workspace folders so the
                // `workspace-folders` mode can prove the first root is known
                // before any didChangeWorkspaceFolders arrives.
                if let Some(folders) = message
                    .pointer("/params/workspaceFolders")
                    .and_then(Value::as_array)
                {
                    for folder in folders {
                        if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                            workspace_folders.push(uri.to_string());
                        }
                    }
                }
                respond(&mut writer, id, json!({ "capabilities": capabilities }));
                if mode == "notify" {
                    notify(
                        &mut writer,
                        "window/showMessage",
                        json!({ "type": 2, "message": "mock show line" }),
                    );
                    notify(
                        &mut writer,
                        "window/logMessage",
                        json!({ "type": 3, "message": "mock log line" }),
                    );
                }
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
                    // `diagnostics-push` mode: spontaneously push one diagnostic on
                    // the virtual line 0 (no pull). The bridge translates it to host
                    // coordinates and publishes it to the editor (#427).
                    if mode == "diagnostics-push" {
                        notify(
                            &mut writer,
                            "textDocument/publishDiagnostics",
                            push_diagnostics(uri, true),
                        );
                    }
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
                    // `diagnostics-push`: a follow-up push with an EMPTY list clears
                    // this source's contribution (#427).
                    if mode == "diagnostics-push" {
                        notify(
                            &mut writer,
                            "textDocument/publishDiagnostics",
                            push_diagnostics(uri, false),
                        );
                    }
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
            "textDocument/willSave" => {
                // Notification (no id): record receipt + reason + URI so a later
                // hover can prove the bridge forwarded it carrying the document
                // URI this server knows (host or virtual) (#357).
                will_save_count += 1;
                if let Some(reason) = message.pointer("/params/reason").and_then(Value::as_i64) {
                    last_will_save_reason = reason;
                }
                last_will_save_uri = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "textDocument/didSave" => {
                // Notification (no id): record receipt + URI for the hover probe
                // (#357). didSave carries no text (includeText:false), so just
                // accepting it is the property under test.
                did_save_count += 1;
                last_did_save_uri = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "textDocument/willSaveWaitUntil" => {
                // `will-save-slow` stalls past kakehashi's 5s save budget so the
                // test can exercise the bridge's timeout-drop path: kakehashi
                // must return null near 5s (not wait the 30s request timeout).
                if mode == "will-save-slow" {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                }
                // Answer with a save-time edit echoing the requested URI — but
                // only for documents synced via didOpen, so a successful edit
                // proves the host document was opened and the REAL URI was
                // forwarded verbatim (#357).
                let result = message
                    .pointer("/params/textDocument/uri")
                    .and_then(Value::as_str)
                    .filter(|uri| documents.contains_key(*uri))
                    .map(|uri| {
                        json!([{
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 }
                            },
                            "newText": format!("willsave-edit:{uri}\n")
                        }])
                    })
                    .unwrap_or(Value::Null);
                respond(&mut writer, id, result);
            }
            "workspace/didChangeWorkspaceFolders" => {
                // Notification (no id): record every added folder URI so a
                // later hover can prove the bridge announced the new root.
                if let Some(added) = message
                    .pointer("/params/event/added")
                    .and_then(Value::as_array)
                {
                    for folder in added {
                        if let Some(uri) = folder.get("uri").and_then(Value::as_str) {
                            workspace_folders.push(uri.to_string());
                        }
                    }
                }
            }
            "textDocument/hover" => {
                let result = if mode.starts_with("will-save") {
                    // Report the recorded willSave/didSave state as a JSON string
                    // so the test can prove the notifications reached this server
                    // and carried the document URI it knows (#357).
                    let state = json!({
                        "will": will_save_count,
                        "reason": last_will_save_reason,
                        "willUri": last_will_save_uri,
                        "did": did_save_count,
                        "didUri": last_did_save_uri,
                    });
                    json!({ "contents": state.to_string() })
                } else if mode.starts_with("workspace-folders") {
                    // Echo the sorted, de-duplicated set of folder URIs this
                    // single process currently knows. Not gated on didOpen: the
                    // folder set is the subject under test, and it is populated
                    // by initialize + didChangeWorkspaceFolders, not didOpen.
                    let mut folders = workspace_folders.clone();
                    folders.sort();
                    folders.dedup();
                    json!({ "contents": format!("folders:{}", folders.join(",")) })
                } else {
                    message
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                        .filter(|uri| documents.contains_key(*uri))
                        .map(|uri| json!({ "contents": format!("mock-hover:{uri}") }))
                        .unwrap_or(Value::Null)
                };
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

/// Send a JSON-RPC notification (server-initiated, no `id`).
fn notify<W: Write>(writer: &mut W, method: &str, params: Value) {
    let body = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
    let _ = write!(writer, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = writer.flush();
}

/// Build `textDocument/publishDiagnostics` params for `uri` (`diagnostics-push`
/// mode, #427): one diagnostic on virtual line 0 when `present`, or an empty list
/// (clearing this source's contribution) otherwise. The bridge translates the
/// virtual range to host coordinates before publishing to the editor.
fn push_diagnostics(uri: &str, present: bool) -> Value {
    let diagnostics = if present {
        json!([{
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 7 }
            },
            "severity": 1,
            "source": "mock-push",
            "message": format!("mock-push-diag:{uri}")
        }])
    } else {
        json!([])
    };
    json!({ "uri": uri, "diagnostics": diagnostics })
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
