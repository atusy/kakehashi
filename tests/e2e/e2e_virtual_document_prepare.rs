//! E2E tests for `kakehashi/virtualDocument/prepare`: the `prepare-peer`
//! mock (`tests/bin/mock_formatter.rs`) prepares markdown's lua virtual
//! documents, and the `echo-document` mock reports what reached it.
//!
//! The peer dedents content by its common indentation, deletes the blank
//! lines at the document's edges, and replaces every gap with a placeholder
//! line (`--` unless a test needs another). The echo mock answers hover with
//! the text it holds, ranged over the hovered line in its own coordinates —
//! so each test proves both directions: the downstream server sees the
//! prepared text, and its positions map back onto the host.

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Combine every lua fence of a markdown document into one virtual document,
/// so the host text between fences becomes a gap.
const COMBINED_QUERY: &str = r#"
(fenced_code_block
  (info_string (language) @injection.language)
  (code_fence_content) @injection.content
  (#set! injection.combined))
"#;

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// How a test starts kakehashi: the peer preparing markdown's lua documents
/// for an echo mock.
struct Setup {
    /// Swap in [`COMBINED_QUERY`].
    combined: bool,
    /// What the peer replaces each gap with.
    placeholder: &'static str,
    /// The echo mock's mode.
    echo: &'static str,
    /// Also bridge python to the echo mock, unprepared.
    unprepared_python: bool,
    /// The editor's capabilities.
    capabilities: Value,
    /// The prepare method's `priorities` for markdown's lua documents.
    prepare_priorities: Value,
    /// Have both mocks log every message they receive to `wire.log` in the
    /// test's directory.
    wire_log: bool,
    /// `KAKEHASHI_EXPERIMENTAL`, which the prepare layer requires.
    experimental: bool,
    /// Bridge markdown itself to a `slow-empty` host server and give the
    /// formatting methods' layers this strategy, the host layer ranked as
    /// it says: `preferred` ranks host first, `concatenated` runs virt then
    /// host.
    slow_host: Option<&'static str>,
    /// Format markdown's lua documents with the concatenated pipeline (the
    /// echo mock its one step).
    concatenated: bool,
    /// The mock mode of the `slow_host` server: `slow-empty`, or
    /// `empty-on-release` to answer only once the test creates `release`
    /// in its directory.
    host_mode: &'static str,
    /// Also bridge lua to an `on-type` mock, ranked below the echo mock for
    /// on-type formatting.
    second_on_type: bool,
    /// Have the echo mock answer color presentations only once the test
    /// creates `release` in its directory.
    color_after_release: bool,
    /// Have the echo mock answer renames only once the test creates
    /// `release` in its directory.
    rename_after_release: bool,
    /// Have the echo mock answer code actions only once the test creates
    /// `release` in its directory.
    code_action_after_release: bool,
    /// Have the echo mock ask to apply its save edit only once it has read
    /// the document's next change.
    apply_edit_after_change: bool,
    /// Also bridge lua to a `code-action` mock, ranked below the echo mock
    /// for code actions under `preferred`.
    second_code_action: bool,
    /// Rank the echo mock alone under a `preferred` code action walk.
    echo_code_action_preferred: bool,
    /// Have the echo mock answer linked editing ranges only once the test
    /// creates `release` in its directory.
    linked_after_release: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            combined: false,
            placeholder: "--\n",
            echo: "echo-document",
            unprepared_python: false,
            capabilities: json!({}),
            prepare_priorities: json!(["peer"]),
            wire_log: false,
            experimental: true,
            slow_host: None,
            concatenated: false,
            host_mode: "slow-empty",
            second_on_type: false,
            color_after_release: false,
            rename_after_release: false,
            code_action_after_release: false,
            apply_edit_after_change: false,
            second_code_action: false,
            echo_code_action_preferred: false,
            linked_after_release: false,
        }
    }
}

impl Setup {
    fn start(self) -> (LspClient, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let config_path = dir.path().join("kakehashi.toml");
        std::fs::write(&config_path, "").expect("write kakehashi config");
        // The peer prepares lua and answers nothing else: the prepare method
        // names it, over the `_` method entry that leaves every other method
        // to the echo mock.
        let mut markdown = json!({ "bridge": { "lua": { "aggregation": {
            "kakehashi/virtualDocument/prepare": { "priorities": self.prepare_priorities },
            "_": { "priorities": ["echo"] }
        } } } });
        if self.unprepared_python {
            markdown["bridge"]["python"] = json!({});
        }
        if self.second_on_type {
            markdown["bridge"]["lua"]["aggregation"]["textDocument/onTypeFormatting"] =
                json!({ "priorities": ["echo", "second"] });
        }
        if self.second_code_action {
            markdown["bridge"]["lua"]["aggregation"]["textDocument/codeAction"] =
                json!({ "strategy": "preferred", "priorities": ["echo", "second"] });
        }
        if self.echo_code_action_preferred {
            markdown["bridge"]["lua"]["aggregation"]["textDocument/codeAction"] =
                json!({ "strategy": "preferred", "priorities": ["echo"] });
        }
        if self.concatenated {
            markdown["bridge"]["lua"]["aggregation"]["textDocument/formatting"] =
                json!({ "strategy": "concatenated", "priorities": ["echo"] });
        }
        if let Some(strategy) = self.slow_host {
            markdown["bridge"]["_self"] = json!({ "enabled": true });
            let priorities = if strategy == "preferred" {
                json!(["host", "virt"])
            } else {
                json!(["virt", "host"])
            };
            markdown["layers"] = json!({ "aggregation": {
                "textDocument/formatting": { "strategy": strategy, "priorities": priorities },
                "textDocument/onTypeFormatting": { "strategy": strategy, "priorities": priorities }
            } });
        }
        if self.combined {
            let query_path = dir.path().join("combined-injections.scm");
            std::fs::write(&query_path, COMBINED_QUERY).expect("write combined query");
            markdown["queries"] = json!([{
                "path": query_path.to_str().expect("UTF-8 query path"),
                "kind": "injections"
            }]);
        }
        let mut builder = LspClient::builder()
            .arg("--config-file")
            .arg(config_path.to_str().expect("UTF-8 config path"))
            .env(
                "KAKEHASHI_EXPERIMENTAL",
                if self.experimental { "true" } else { "false" },
            )
            .env("MOCK_PREPARE_PLACEHOLDER", self.placeholder)
            .env(
                "MOCK_RELEASE_FILE",
                dir.path()
                    .join("release")
                    .to_str()
                    .expect("UTF-8 release path"),
            );
        if self.color_after_release {
            builder = builder.env("MOCK_COLOR_AFTER_RELEASE", "1");
        }
        if self.rename_after_release {
            builder = builder.env("MOCK_RENAME_AFTER_RELEASE", "1");
        }
        if self.code_action_after_release {
            builder = builder.env("MOCK_CODE_ACTION_AFTER_RELEASE", "1");
        }
        if self.linked_after_release {
            builder = builder.env("MOCK_LINKED_AFTER_RELEASE", "1");
        }
        if self.apply_edit_after_change {
            builder = builder.env("MOCK_APPLY_EDIT_AFTER_CHANGE", "1");
        }
        if self.wire_log {
            builder = builder.env(
                "MOCK_LSP_WIRE_LOG",
                dir.path()
                    .join("wire.log")
                    .to_str()
                    .expect("UTF-8 wire log path"),
            );
        }
        let mut client = builder.build();
        let mut servers = json!({
            "peer": {
                "cmd": [mock_bin(), "prepare-peer"],
                "languages": ["lua"]
            },
            "echo": {
                "cmd": [mock_bin(), self.echo],
                "languages": if self.unprepared_python {
                    json!(["lua", "python"])
                } else {
                    json!(["lua"])
                }
            }
        });
        if self.second_on_type {
            servers["second"] = json!({
                "cmd": [mock_bin(), "on-type"],
                "languages": ["lua"]
            });
        }
        if self.second_code_action {
            servers["second"] = json!({
                "cmd": [mock_bin(), "code-action"],
                "languages": ["lua"]
            });
        }
        if self.slow_host.is_some() {
            servers["slowhost"] = json!({
                "cmd": [mock_bin(), self.host_mode],
                "languages": ["markdown"]
            });
        }
        client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": self.capabilities,
                "workspaceFolders": null,
                "initializationOptions": {
                    "languageServers": servers,
                    "languages": { "markdown": markdown }
                }
            }),
        );
        client.send_notification("initialized", json!({}));
        (client, dir)
    }
}

/// Name the servers preparing markdown's lua documents from now on.
fn set_prepare_priorities(client: &mut LspClient, priorities: Value) {
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "kakehashi": { "languages": { "markdown": { "bridge": { "lua": {
            "aggregation": {
                "kakehashi/virtualDocument/prepare": { "priorities": priorities }
            }
        } } } } } } }),
    );
}

fn open(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1, "text": text }
        }),
    );
}

fn change(client: &mut LspClient, uri: &str, version: i32, text: &str) {
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }]
        }),
    );
}

fn hover(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
    client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }),
    )
}

/// Hover until the peer and the echo mock are up.
fn hover_with_retry(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
    for _ in 0..300 {
        let response = hover(client, uri, line, character);
        if !response["result"].is_null() {
            return response["result"].clone();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for a hover through the prepared document");
}

fn hover_text(hover: &Value) -> String {
    match &hover["contents"] {
        Value::String(text) => text.clone(),
        contents => contents["value"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| panic!("unexpected hover contents: {contents}")),
    }
}

/// The first `echo-document` diagnostic published for `uri` whose message
/// is `message`.
fn wait_for_echo_diagnostic(client: &mut LspClient, uri: &str, message: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let params = client
            .wait_for_notification("textDocument/publishDiagnostics", remaining)
            .unwrap_or_else(|| panic!("no echo-document diagnostic {message:?} for {uri}"));
        if params["uri"] != uri {
            continue;
        }
        if let Some(diagnostic) = params["diagnostics"].as_array().and_then(|items| {
            items
                .iter()
                .find(|d| d["source"] == "echo-document" && d["message"] == message)
        }) {
            return diagnostic.clone();
        }
    }
}

/// The text of the next `echo-document <event>: ` log line.
fn wait_for_echo_log(client: &mut LspClient, event: &str) -> String {
    let marker = format!("echo-document {event}: ");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let params = client
            .wait_for_notification("window/logMessage", remaining)
            .unwrap_or_else(|| panic!("no `{marker}` log"));
        // Relayed with the server's name as a prefix.
        if let Some((_, text)) = params["message"]
            .as_str()
            .and_then(|message| message.split_once(marker.as_str()))
        {
            return text.to_string();
        }
    }
}

#[test]
fn downstream_sees_the_dedented_document_and_maps_back() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/dedent.md";
    let text = "# t\n\n```lua\n  local x = 1\n  print(x)\n```\n";
    open(&mut client, uri, text);

    // Host (3, 4) sits in `local`, two columns into the dedented line.
    let hover = hover_with_retry(&mut client, uri, 3, 4);
    assert_eq!(hover_text(&hover), "local x = 1\nprint(x)\n");
    // The mock's whole-line range (0, 0)-(0, 11) lands after the host indent.
    assert_eq!(
        hover["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 13 }
        })
    );
}

/// The completion items, inlay hints or color presentations a request got,
/// whichever shape its result takes.
fn result_items(response: &Value) -> Vec<Value> {
    match &response["result"] {
        Value::Array(items) => items.clone(),
        Value::Object(list) => list["items"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[test]
fn edits_on_a_prepared_document_map_back_reindented() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/edits.md";
    let text = "# t\n\n```lua\n  local x = 1\n  pr\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // Completion at the end of `pr` (P (1, 2)): the inserted second line
    // regains the host indent, and the line added above the first lands
    // after the host's indent, followed by it.
    let response = client.send_request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 4, "character": 4 } }),
    );
    let items = result_items(&response);
    let item = items
        .iter()
        .find(|item| item["label"] == "echo-edit")
        .unwrap_or_else(|| panic!("no echo-edit item: {response}"));
    assert_eq!(
        item["textEdit"],
        json!({
            "range": {
                "start": { "line": 4, "character": 4 },
                "end": { "line": 4, "character": 4 }
            },
            "newText": "int(x)\n  print(y)"
        })
    );
    assert_eq!(
        item["additionalTextEdits"],
        json!([{
            "range": {
                "start": { "line": 3, "character": 2 },
                "end": { "line": 3, "character": 2 }
            },
            "newText": "-- c\n  "
        }])
    );
    // A caret in the removed indent gets none, not even an item without an
    // edit, which the client would insert at the caret unmapped: the server
    // would see it at the line's content.
    let response = client.send_request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 4, "character": 1 } }),
    );
    assert!(result_items(&response).is_empty(), "{response}");

    // The hint's edit at P (0, 0) lands after the host indent.
    let response = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 5, "character": 0 }
            }
        }),
    );
    let hints = result_items(&response);
    assert_eq!(hints.len(), 1, "{response}");
    assert_eq!(hints[0]["position"], json!({ "line": 3, "character": 3 }));
    assert_eq!(
        hints[0]["textEdits"],
        json!([{
            "range": {
                "start": { "line": 3, "character": 2 },
                "end": { "line": 3, "character": 2 }
            },
            "newText": "existing "
        }])
    );

    // A presentation keyed to the request range over `x` maps back onto it,
    // as does one the server places there itself (P (0, 6)-(0, 7)).
    let range = json!({
        "start": { "line": 3, "character": 8 },
        "end": { "line": 3, "character": 9 }
    });
    let response = client.send_request(
        "textDocument/colorPresentation",
        json!({
            "textDocument": { "uri": uri },
            "color": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 },
            "range": range
        }),
    );
    let presentations = result_items(&response);
    assert_eq!(presentations.len(), 2, "{response}");
    assert_eq!(
        presentations[0]["textEdit"],
        json!({ "range": range, "newText": "#ff0000" })
    );
    assert_eq!(
        presentations[1]["textEdit"],
        json!({ "range": range, "newText": "#00ff00" })
    );
}

#[test]
fn an_incomplete_list_dropped_whole_still_asks_to_requery() {
    // Every item is a snippet inserting a variable, which a prepared
    // document cannot carry: no item comes back, but the ask to re-query does, as the next keystroke
    // may yield items that map.
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/requery.md";
    let text = "# t\n\n```lua\n  local SNIPPETS = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let response = client.send_request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 7 } }),
    );
    assert_eq!(
        response["result"],
        json!({ "isIncomplete": true, "items": [] }),
        "{response}"
    );
}

#[test]
fn a_snippet_completion_on_a_prepared_document_keeps_its_tabstops() {
    // A one-line snippet keeps its tabstops. `do\n\t${1:body}\n$0end` is
    // refused whatever the editor declares: snippet engines indent a
    // snippet's later lines by the caret line's, and turn its tabs into
    // spaces, whatever the item asks.
    for capabilities in [
        json!({ "textDocument": { "completion": { "completionItem": {
            "snippetSupport": true,
            "insertTextModeSupport": { "valueSet": [1, 2] }
        }}}}),
        json!({ "textDocument": { "completion": { "completionItem": {
            "snippetSupport": true
        }}}}),
    ] {
        let (mut client, _dir) = Setup {
            capabilities,
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/snippet.md";
        let text = "# t\n\n```lua\n  local SNIPPET_EDIT = 1\n```\n";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        let response = client.send_request(
            "textDocument/completion",
            json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 24 } }),
        );
        let at = json!({
            "start": { "line": 3, "character": 24 },
            "end": { "line": 3, "character": 24 }
        });
        assert_eq!(
            response["result"]["items"],
            json!([{"label": "call", "insertTextFormat": 2, "textEdit": {
                "range": at, "newText": "f(${1:x})$0"
            }}]),
            "{response}"
        );
    }
}

/// Apply LSP `TextEdit`s to `text`, fixtures being ASCII (a UTF-16 column
/// is a byte), end-first.
fn apply_edits(text: &str, edits: &[Value]) -> String {
    let offset = |position: &Value| -> usize {
        let line = position["line"].as_u64().unwrap() as usize;
        let character = position["character"].as_u64().unwrap() as usize;
        let start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
        let content = text.split_inclusive('\n').nth(line).unwrap_or("");
        assert!(
            character <= content.trim_end_matches('\n').len(),
            "column {character} past line {line}"
        );
        start + character
    };
    let mut spans: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|edit| {
            (
                offset(&edit["range"]["start"]),
                offset(&edit["range"]["end"]),
                edit["newText"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    spans.sort_by_key(|&(start, end, _)| std::cmp::Reverse((start, end)));
    let mut text = text.to_string();
    for (start, end, new_text) in spans {
        text.replace_range(start..end, &new_text);
    }
    text
}

#[test]
fn formatting_on_a_prepared_document_maps_back_reindented() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/format.md";
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // The echo mock formats the prepared (dedented) text, uppercasing it
    // and breaking the line: the line it creates regains the indentation
    // the peer removed.
    for method in ["textDocument/formatting", "textDocument/rangeFormatting"] {
        let response = client.send_request(
            method,
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 5, "character": 0 }
                },
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        );
        let edits = response["result"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("{method}: {response}"));
        assert_eq!(
            apply_edits(text, &edits),
            "# t\n\n```lua\n  LOCAL X = 1\n  LOCAL Y = 2\n```\n",
            "{method}: {response}"
        );
    }
}

#[test]
fn on_type_formatting_on_a_prepared_document_maps_back_reindented() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/on-type.md";
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // `;` typed after `1`: the echo mock answers for the whole prepared
    // text, its line broken; the new line regains the removed indent.
    let response = client.send_request(
        "textDocument/onTypeFormatting",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 14 },
            "ch": ";",
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let edits = response["result"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{response}"));
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  LOCAL X = 1\n  LOCAL Y = 2\n```\n",
        "{response}"
    );
}

#[test]
fn an_on_type_answer_its_map_refuses_leaves_the_request_to_the_next_server() {
    let (mut client, _dir) = Setup {
        second_on_type: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/on-type-refused.md";
    let text = "# t\n\n```lua\n  local x = 1; -- OVERLAP\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // The echo mock, ranked first, answers with overlapping edits: its map
    // refuses them, which is no answer, so the `on-type` mock ranked below
    // answers instead (uppercasing), its edits mapped back reindented. No
    // answer (null) only while that server starts; the first answer given
    // is checked as it comes (an empty list would be the refusal winning).
    let mut edits = None;
    for _ in 0..100 {
        let response = client.send_request(
            "textDocument/onTypeFormatting",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 3, "character": 14 },
                "ch": ";",
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        if !response["result"].is_null() {
            edits = response["result"].as_array().cloned();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let edits = edits.expect("an on-type answer");
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  LOCAL X = 1; -- OVERLAP\n```\n"
    );
}

#[test]
fn on_type_formatting_with_the_caret_in_a_removed_indent_is_answered() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/on-type-indent.md";
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // The caret inside the indent the peer removed has no place of its own
    // in the prepared text: it is sent as its line's start there, and the
    // answer maps back as from anywhere else.
    let response = client.send_request(
        "textDocument/onTypeFormatting",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 1 },
            "ch": ";",
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let edits = response["result"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{response}"));
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  LOCAL X = 1\n  LOCAL Y = 2\n```\n",
        "{response}"
    );
}

#[test]
fn a_selection_of_only_what_the_peer_removed_is_not_range_formatted() {
    let (mut client, dir) = Setup {
        wire_log: true,
        ..Setup::default()
    }
    .start();
    let wire = dir.path().join("wire.log");
    let sent = || {
        std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("textDocument/rangeFormatting\t"))
            .count()
    };
    // Each selection on its own document: (text, selection, reaches the
    // server). One holding nothing the prepared text has (the removed
    // indent, or a removed blank line) is empty there: no request, as for a
    // selection only in a host prefix. One holding a character of the code
    // is sent.
    let one_line = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    let blank_first = "# t\n\n```lua\n\n  local x = 1; local y = 2\n```\n";
    for (index, (text, (start, end), reaches)) in [
        (one_line, ((3, 0), (3, 2)), false),
        (one_line, ((3, 1), (3, 2)), false),
        (blank_first, ((3, 0), (4, 0)), false),
        (one_line, ((3, 0), (3, 3)), true),
    ]
    .into_iter()
    .enumerate()
    {
        let uri = format!("file:///prepare/range-removed-{index}.md");
        let code_line = if text == blank_first { 4 } else { 3 };
        open(&mut client, &uri, text);
        hover_with_retry(&mut client, &uri, code_line, 4);
        let before = sent();
        let response = client.send_request(
            "textDocument/rangeFormatting",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": start.0, "character": start.1 },
                    "end": { "line": end.0, "character": end.1 }
                },
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        );
        assert!(response.get("error").is_none(), "{index}: {response}");
        assert_eq!(sent() - before, usize::from(reaches), "{index}: {response}");
        let edits = response["result"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            apply_edits(text, &edits) != text,
            reaches,
            "{index}: {response}"
        );
    }
}

#[test]
fn the_concatenated_pipeline_formats_a_prepared_document() {
    let (mut client, _dir) = Setup {
        concatenated: true,
        ..Setup::default()
    }
    .start();
    // The second fence's leading blank line is deleted by the peer: the
    // host keeps it.
    for (index, text, formatted) in [
        (
            0,
            "# t\n\n```lua\n  local x = 1; local y = 2\n```\n",
            "# t\n\n```lua\n  LOCAL X = 1\n  LOCAL Y = 2\n```\n",
        ),
        (
            1,
            "# t\n\n```lua\n\n  local x = 1; local y = 2\n```\n",
            "# t\n\n```lua\n\n  LOCAL X = 1\n  LOCAL Y = 2\n```\n",
        ),
    ] {
        let uri = format!("file:///prepare/pipeline-{index}.md");
        open(&mut client, &uri, text);
        hover_with_retry(&mut client, &uri, 3 + index, 4);
        let response = client.send_request(
            "textDocument/formatting",
            formatting_params("textDocument/formatting", &uri),
        );
        let edits = response["result"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("{response}"));
        assert_eq!(apply_edits(text, &edits), formatted, "{response}");
    }
}

/// The params of `method` over the whole of `uri`, typed at host (3, 14)
/// for on-type formatting.
fn formatting_params(method: &str, uri: &str) -> Value {
    let mut params = json!({
        "textDocument": { "uri": uri },
        "options": { "tabSize": 2, "insertSpaces": true }
    });
    match method {
        "textDocument/rangeFormatting" => {
            params["range"] = json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 5, "character": 0 }
            });
        }
        "textDocument/onTypeFormatting" => {
            params["position"] = json!({ "line": 3, "character": 14 });
            params["ch"] = json!(";");
        }
        _ => {}
    }
    params
}

/// A formatting answer mapped through a prepared document is checked once
/// more as the final answer is chosen: while the host layer still ran, the
/// host was edited, and the whole answer is dropped, without an error.
/// Without the edit, the same request answers with the virt edits.
#[test]
fn a_formatting_answer_the_host_moved_under_is_dropped_as_it_is_chosen() {
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    for (strategy, method) in [
        ("preferred", "textDocument/formatting"),
        ("preferred", "textDocument/rangeFormatting"),
        ("preferred", "textDocument/onTypeFormatting"),
        ("concatenated", "textDocument/formatting"),
    ] {
        let (mut client, _dir) = Setup {
            slow_host: Some(strategy),
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/stale.md";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);

        let response = client.send_request(method, formatting_params(method, uri));
        let edits = response["result"].as_array().cloned().unwrap_or_default();
        assert!(
            !edits.is_empty(),
            "{strategy} {method}, unedited: {response}"
        );
        drop(client);

        // Now a host layer answering only when released. Edit the host
        // (outside the region) once the host layer's request is out, and
        // release it once the edit is in. The request read the host's
        // version as it started, before either layer was sent: whether the
        // virt layer's answer came before the edit or after the release,
        // it is made for the text before the edit, and the answer chosen
        // after the host layer's is checked against the edited host.
        let (mut client, dir) = Setup {
            slow_host: Some(strategy),
            host_mode: "empty-on-release",
            wire_log: true,
            ..Setup::default()
        }
        .start();
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        let id = client.send_request_async(method, formatting_params(method, uri));
        let wire = dir.path().join("wire.log");
        let host_request = format!("{method}\t{uri}");
        let until = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .lines()
            .any(|line| line == host_request)
        {
            assert!(
                Instant::now() < until,
                "{strategy} {method}: host request not sent: {}",
                std::fs::read_to_string(&wire).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        change(&mut client, uri, 2, &format!("# u{}", &text[3..]));
        // A request after the edit is answered once the edit is in.
        hover(&mut client, uri, 0, 0);
        std::fs::write(dir.path().join("release"), "").expect("release the host layer");
        let response = client.receive_response_for_id_public(id);
        assert!(
            response.get("error").is_none(),
            "{strategy} {method}: {response}"
        );
        // Dropped whole: no answer at all, not an empty list some layer
        // gave.
        assert!(
            response["result"].is_null(),
            "{strategy} {method}, edited: {response}"
        );
    }
}

#[test]
fn a_color_presentation_the_host_moved_under_is_dropped() {
    // The echo mock answers the picker only once released. The host is
    // edited (outside the region) while the request is out: its
    // presentation, made for the text before the edit, is dropped.
    let (mut client, dir) = Setup {
        color_after_release: true,
        wire_log: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/color-stale.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let request = json!({
        "textDocument": { "uri": uri },
        "color": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 },
        "range": {
            "start": { "line": 3, "character": 8 },
            "end": { "line": 3, "character": 9 }
        }
    });
    // Unedited, the presentations map back.
    std::fs::write(dir.path().join("release"), "").expect("release");
    let response = client.send_request("textDocument/colorPresentation", request.clone());
    assert!(!result_items(&response).is_empty(), "{response}");
    std::fs::remove_file(dir.path().join("release")).expect("hold again");

    let id = client.send_request_async("textDocument/colorPresentation", request);
    let wire = dir.path().join("wire.log");
    let sent = || {
        std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("textDocument/colorPresentation\t"))
            .count()
    };
    let until = Instant::now() + Duration::from_secs(10);
    while sent() < 2 {
        assert!(Instant::now() < until, "color request not sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    change(&mut client, uri, 2, &format!("# u{}", &text[3..]));
    hover(&mut client, uri, 0, 0);
    std::fs::write(dir.path().join("release"), "").expect("release");
    let response = client.receive_response_for_id_public(id);
    assert!(response.get("error").is_none(), "{response}");
    assert!(result_items(&response).is_empty(), "{response}");
}

#[test]
fn a_formatting_request_cancelled_while_it_waits_ends_at_once() {
    // The host layer answers only once released; the request waits on it,
    // as it may wait on the host settling before its answer is judged, all
    // in the request's own handler. `$/cancelRequest` ends that handler
    // wherever it waits: the response is the cancelled error, at once.
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    for method in [
        "textDocument/formatting",
        "textDocument/rangeFormatting",
        "textDocument/onTypeFormatting",
    ] {
        let (mut client, dir) = Setup {
            slow_host: Some("preferred"),
            host_mode: "empty-on-release",
            wire_log: true,
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/cancelled.md";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        let id = client.send_request_async(method, formatting_params(method, uri));
        let wire = dir.path().join("wire.log");
        let host_request = format!("{method}\t{uri}");
        let until = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .lines()
            .any(|line| line == host_request)
        {
            assert!(Instant::now() < until, "{method}: host request not sent");
            std::thread::sleep(Duration::from_millis(20));
        }
        let cancelled = Instant::now();
        client.send_notification("$/cancelRequest", json!({ "id": id }));
        let response = client.receive_response_for_id_public(id);
        assert_eq!(response["error"]["code"], -32800, "{method}: {response}");
        assert!(
            cancelled.elapsed() < Duration::from_secs(5),
            "{method}: {:?}",
            cancelled.elapsed()
        );
        std::fs::write(dir.path().join("release"), "").expect("release the host layer");
    }
}

#[test]
fn formatting_before_the_document_was_looked_up_waits_for_its_prepared_text() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/format-early.md";
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    open(&mut client, uri, text);

    // At once: the answer is not in yet. The request waits for it and
    // formats the prepared text, its edits mapped back: the line the
    // formatter breaks off regains the indent (unprepared, it would not).
    let response = client.send_request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let edits = response["result"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{response}"));
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  LOCAL X = 1\n  LOCAL Y = 2\n```\n",
        "{response}"
    );
}

#[test]
fn a_rename_maps_back_through_the_prepared_document() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/rename.md";
    let text = "# t\n\n```lua\n  local x = 1\n  return x\n```\n";
    open(&mut client, uri, text);

    // At once, the answer not in yet: the request waits for it, and the
    // rename (the echo mock replaces its first line) is placed after the
    // host's indentation, which the server never saw.
    let response = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 8 },
            "newName": "y"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let edits = response["result"]["changes"][uri]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{response}"));
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  renamed\n  return x\n```\n",
        "{response}"
    );
}

#[test]
fn a_rename_the_host_moved_under_is_answered_as_content_modified() {
    // The echo mock answers renames only once released. The host is
    // edited (outside the region) while the request is out: the rename,
    // made for the text before the edit, is not applied, and the editor is
    // told the content was modified, not that there is nothing to rename.
    let (mut client, dir) = Setup {
        rename_after_release: true,
        wire_log: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/rename-stale.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let request = json!({
        "textDocument": { "uri": uri },
        "position": { "line": 3, "character": 8 },
        "newName": "y"
    });
    let id = client.send_request_async("textDocument/rename", request);
    let wire = dir.path().join("wire.log");
    let until = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&wire)
        .unwrap_or_default()
        .lines()
        .any(|line| line.starts_with("textDocument/rename\t"))
    {
        assert!(Instant::now() < until, "rename request not sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    change(&mut client, uri, 2, &format!("# u{}", &text[3..]));
    hover(&mut client, uri, 0, 0);
    std::fs::write(dir.path().join("release"), "").expect("release");
    let response = client.receive_response_for_id_public(id);
    assert_eq!(response["error"]["code"], -32801, "{response}");
}

#[test]
fn linked_editing_ranges_map_back_through_the_prepared_document() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/linked.md";
    let text = "# t\n\n```lua\n  local x = 1\n  return x\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let response = client.send_request(
        "textDocument/linkedEditingRange",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 8 } }),
    );
    // `x` where the host has it: past each line's indent, which the server
    // never saw.
    assert_eq!(
        response["result"]["ranges"],
        json!([
            { "start": { "line": 3, "character": 8 }, "end": { "line": 3, "character": 9 } },
            { "start": { "line": 4, "character": 9 }, "end": { "line": 4, "character": 10 } }
        ]),
        "{response}"
    );
}

fn code_action_request(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 3, "character": 8 },
            "end": { "line": 3, "character": 8 }
        },
        "context": { "diagnostics": [] }
    })
}

#[test]
fn code_actions_map_back_through_the_prepared_document() {
    // Lazy actions resolved by the editor, and for an editor that cannot,
    // within the request: either way through the prepared document's map.
    for resolves in [true, false] {
        let (mut client, _dir) = Setup {
            capabilities: if resolves {
                json!({ "textDocument": { "codeAction": {
                    "dataSupport": true,
                    "resolveSupport": { "properties": ["edit"] }
                } } })
            } else {
                json!({})
            },
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/action.md";
        let text = "# t\n\n```lua\n  local x = 1\n```\n";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        let response = client.send_request("textDocument/codeAction", code_action_request(uri));
        let actions = response["result"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("{response}"));
        let action = |title: &str| {
            actions
                .iter()
                .find(|action| {
                    action["title"]
                        .as_str()
                        .is_some_and(|t| t.starts_with(title))
                })
                .cloned()
                .unwrap_or_else(|| panic!("no {title}: {response}"))
        };
        let edited = |action: &Value| {
            let edits = action["edit"]["changes"][uri]
                .as_array()
                .cloned()
                .unwrap_or_else(|| panic!("{action}"));
            apply_edits(text, &edits)
        };
        assert_eq!(edited(&action("echo-fix")), "# t\n\n```lua\n  fixed\n```\n");
        let wrap = action("echo-wrap");
        let wrap = if resolves {
            assert!(wrap.get("edit").is_none(), "{wrap}");
            let resolved = client.send_request("codeAction/resolve", wrap);
            resolved["result"].clone()
        } else {
            wrap
        };
        assert_eq!(
            edited(&wrap),
            "# t\n\n```lua\n  do\n    local x = 1\n  end\n```\n",
            "resolves: {resolves}"
        );
    }
}

#[test]
fn a_prepared_answer_of_refused_actions_does_not_win_a_preferred_walk() {
    // The echo mock, ranked first, answers one action whose edit the map
    // refuses: disabled for this editor, it is no answer, and the server
    // ranked below is asked, its action offered.
    let (mut client, _dir) = Setup {
        capabilities: json!({ "textDocument": { "codeAction": { "disabledSupport": true } } }),
        second_code_action: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/action-refused.md";
    let text = "# t\n\n```lua\n  local x = 1 -- REFUSE\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let response = client.send_request("textDocument/codeAction", code_action_request(uri));
    let titles: Vec<String> = response["result"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{response}"))
        .iter()
        .filter_map(|action| action["title"].as_str().map(str::to_string))
        .collect();
    assert!(
        titles
            .iter()
            .any(|title| title.starts_with("Replace with fixed")),
        "{response}"
    );
    assert!(
        !titles.iter().any(|title| title.starts_with("echo-refused")),
        "{response}"
    );
}

#[test]
fn a_prepared_answer_of_refused_actions_answers_a_preferred_walk_nothing_else_answers() {
    // The echo mock, alone under `preferred`, answers one action whose edit
    // the map refuses: no server ranked below answers, so it is offered
    // disabled, for this editor to show why.
    let (mut client, _dir) = Setup {
        capabilities: json!({ "textDocument": { "codeAction": { "disabledSupport": true } } }),
        echo_code_action_preferred: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/action-refused-alone.md";
    let text = "# t\n\n```lua\n  local x = 1 -- REFUSE\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let response = client.send_request("textDocument/codeAction", code_action_request(uri));
    let actions = response["result"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{response}"));
    let refused = actions
        .iter()
        .find(|action| {
            action["title"]
                .as_str()
                .is_some_and(|title| title.starts_with("echo-refused"))
        })
        .unwrap_or_else(|| panic!("{response}"));
    assert!(refused["disabled"]["reason"].is_string(), "{response}");
    assert!(refused.get("edit").is_none(), "{response}");
}

#[test]
fn code_actions_the_host_moved_under_are_dropped() {
    // The echo mock answers code actions only once released. The host is
    // edited (outside the region) while the request is out: the actions,
    // made for the text before the edit, are not offered.
    let (mut client, dir) = Setup {
        code_action_after_release: true,
        wire_log: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/action-stale.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let id = client.send_request_async("textDocument/codeAction", code_action_request(uri));
    let wire = dir.path().join("wire.log");
    let until = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&wire)
        .unwrap_or_default()
        .lines()
        .any(|line| line.starts_with("textDocument/codeAction\t"))
    {
        assert!(Instant::now() < until, "code action request not sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    change(&mut client, uri, 2, &format!("# u{}", &text[3..]));
    hover(&mut client, uri, 0, 0);
    std::fs::write(dir.path().join("release"), "").expect("release");
    let response = client.receive_response_for_id_public(id);
    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"].is_null(), "{response}");
}

#[test]
fn a_rename_range_the_host_moved_under_is_no_answer() {
    // The echo mock answers prepareRename (the word at the position) only
    // once released. Unedited, the word maps where the host has it; the
    // host edited above the region while the request is out, its range,
    // for the text before the edit, is no answer.
    for moved in [false, true] {
        let (mut client, dir) = Setup {
            rename_after_release: true,
            wire_log: true,
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/prepare-rename.md";
        let text = "# t\n\n```lua\n  local x = 1\n```\n";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        let id = client.send_request_async(
            "textDocument/prepareRename",
            json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 8 } }),
        );
        let wire = dir.path().join("wire.log");
        let until = Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&wire)
            .unwrap_or_default()
            .lines()
            .any(|line| line.starts_with("textDocument/prepareRename\t"))
        {
            assert!(Instant::now() < until, "prepareRename not sent");
            std::thread::sleep(Duration::from_millis(20));
        }
        if moved {
            change(&mut client, uri, 2, &format!("# u\n{text}"));
            hover(&mut client, uri, 0, 0);
        }
        std::fs::write(dir.path().join("release"), "").expect("release");
        let response = client.receive_response_for_id_public(id);
        assert!(response.get("error").is_none(), "{response}");
        if moved {
            assert!(response["result"].is_null(), "{response}");
        } else {
            assert_eq!(
                response["result"],
                json!({
                    "start": { "line": 3, "character": 8 },
                    "end": { "line": 3, "character": 9 }
                }),
                "{response}"
            );
        }
    }
}

#[test]
fn linked_editing_ranges_the_host_moved_under_are_no_answer() {
    // The echo mock answers linked ranges only once released: the host
    // edited above the region while the request is out, its ranges, for
    // the text before the edit, are no answer.
    let (mut client, dir) = Setup {
        linked_after_release: true,
        wire_log: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/linked-moved.md";
    let text = "# t\n\n```lua\n  local x = 1\n  return x\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    let id = client.send_request_async(
        "textDocument/linkedEditingRange",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 8 } }),
    );
    let wire = dir.path().join("wire.log");
    let until = Instant::now() + Duration::from_secs(10);
    while !std::fs::read_to_string(&wire)
        .unwrap_or_default()
        .lines()
        .any(|line| line.starts_with("textDocument/linkedEditingRange\t"))
    {
        assert!(Instant::now() < until, "linkedEditingRange not sent");
        std::thread::sleep(Duration::from_millis(20));
    }
    change(&mut client, uri, 2, &format!("# u\n{text}"));
    hover(&mut client, uri, 0, 0);
    std::fs::write(dir.path().join("release"), "").expect("release");
    let response = client.receive_response_for_id_public(id);
    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"].is_null(), "{response}");
}

#[test]
fn downstream_sees_edge_blank_lines_deleted_and_the_host_keeps_them() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/edges.md";
    let text = "# t\n\n```lua\n\n  #!/usr/bin/env lua\n  print(1)\n\n```\n";
    open(&mut client, uri, text);

    // The shebang is the first line downstream.
    let answer = hover_with_retry(&mut client, uri, 4, 4);
    assert_eq!(hover_text(&answer), "#!/usr/bin/env lua\nprint(1)\n");
    assert_eq!(
        answer["range"],
        json!({
            "start": { "line": 4, "character": 2 },
            "end": { "line": 4, "character": 20 }
        })
    );
    // Downstream has no deleted line: a hover there is not answered for
    // the next one.
    let response = hover(&mut client, uri, 3, 0);
    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"].is_null(), "{response}");
}

#[test]
fn downstream_sees_gaps_replaced_and_lines_map_back() {
    let (mut client, _dir) = Setup {
        combined: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/combined.md";
    let text = "```lua\nlocal a = 1\n```\n\ntext\n\n```lua\nprint(a)\n```\n";
    open(&mut client, uri, text);

    // The host text between the fences — five lines — is one gap, which the
    // peer replaced with a single `--` line.
    let answer = hover_with_retry(&mut client, uri, 7, 1);
    assert_eq!(hover_text(&answer), "local a = 1\n--\nprint(a)\n");
    // Prepared line 2 is host line 7.
    assert_eq!(
        answer["range"],
        json!({
            "start": { "line": 7, "character": 0 },
            "end": { "line": 7, "character": 8 }
        })
    );
}

#[test]
fn an_edit_is_prepared_before_requests_read_it() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/edit.md";
    let text = "# t\n\n```lua\n  local x = 1\n  print(x)\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // Insert a line above `print(x)`, shifting it down, and hover it at once:
    // the answer must come from the newly prepared text, not the one the
    // server held before the edit.
    // The peer takes two seconds over the edited text: the hover races a
    // pending answer.
    let edited = "# t\n\n```lua\n  local x = 1\n  x = x + 1 -- SLOW\n  print(x)\n```\n";
    change(&mut client, uri, 2, edited);
    // No answer may come from the text the server held before the edit:
    // a request either waits for the new prepared text or gives no answer
    // (retried here, as a loaded machine can push the sync past a single
    // request's budget). A stale answer fails at once.
    let answer = (0..300)
        .find_map(|_| {
            let answer = hover(&mut client, uri, 5, 4)["result"].clone();
            if answer.is_null() {
                std::thread::sleep(Duration::from_millis(100));
                return None;
            }
            assert_eq!(
                hover_text(&answer),
                "local x = 1\nx = x + 1 -- SLOW\nprint(x)\n",
                "an answer from the text before the edit"
            );
            Some(answer)
        })
        .expect("no hover after the edit");
    assert_eq!(
        answer["range"],
        json!({
            "start": { "line": 5, "character": 2 },
            "end": { "line": 5, "character": 10 }
        })
    );
}

#[test]
fn an_unprepared_document_answers_promptly_beside_a_prepared_one() {
    let (mut client, _dir) = Setup {
        unprepared_python: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/mixed.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n\n```python\nx = 1\n```\n";
    open(&mut client, uri, text);
    // Something is prepared, and both documents are up.
    hover_with_retry(&mut client, uri, 3, 4);
    assert_eq!(
        hover_text(&hover_with_retry(&mut client, uri, 7, 0)),
        "x = 1\n"
    );

    // An edit to the unprepared fence: its requests check the text its
    // server holds too, which must not hold them up beyond that send.
    let edited = "# t\n\n```lua\n  local x = 1\n```\n\n```python\nx = 1\ny = 2\n```\n";
    change(&mut client, uri, 2, edited);
    // The first request answers well within the budget a stuck wait would
    // use up (5 s) — possibly with nothing, under load, but never with the
    // text before the edit.
    let started = Instant::now();
    let first = hover(&mut client, uri, 8, 0)["result"].clone();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(4500),
        "the unprepared request waited {elapsed:?}"
    );
    let answer = std::iter::once(first)
        .chain((0..300).map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            hover(&mut client, uri, 8, 0)["result"].clone()
        }))
        .find(|answer| !answer.is_null())
        .expect("no hover after the edit");
    assert_eq!(
        hover_text(&answer),
        "x = 1\ny = 2\n",
        "an answer from the text before the edit"
    );
}

#[test]
fn range_formatting_does_not_wait_for_regions_outside_its_range() {
    let (mut client, _dir) = Setup {
        unprepared_python: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/range-elsewhere.md";
    let fences = |lua: &str| {
        format!(
            "# t\n\n```lua\n{lua}1\n```\n\n```lua\n{lua}2\n```\n\n```lua\n{lua}3\n```\n\n```python\nx = 1\n```\n"
        )
    };
    open(&mut client, uri, &fences("  x"));
    // Both documents are up.
    hover_with_retry(&mut client, uri, 3, 2);
    assert_eq!(
        hover_text(&hover_with_retry(&mut client, uri, 15, 0)),
        "x = 1\n"
    );

    // Each lua fence's new text takes its peer 2 s to answer, one after
    // another. The python fence alone is selected: its formatting waits for
    // none of them.
    change(&mut client, uri, 2, &fences("SLOW"));
    let started = Instant::now();
    let response = client.send_request(
        "textDocument/rangeFormatting",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 15, "character": 0 },
                "end": { "line": 15, "character": 5 }
            },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    let elapsed = started.elapsed();
    assert!(response.get("error").is_none(), "{response}");
    let edits = response["result"].as_array().cloned().unwrap_or_default();
    assert!(
        !edits.is_empty(),
        "the python fence is formatted: {response}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "range formatting waited {elapsed:?} for regions it does not format"
    );
}

#[test]
fn without_experimental_features_formatting_answers_as_unprepared() {
    // With experimental features off, the formatting methods answer as on
    // any unprepared document: the edits' ranges translated, their text as
    // the server wrote it
    // (the line the formatter breaks off regains no indent).
    let (mut client, _dir) = Setup {
        experimental: false,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/flag-off-format.md";
    let text = "# t\n\n```lua\n  local x = 1; local y = 2\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);
    for method in [
        "textDocument/formatting",
        "textDocument/rangeFormatting",
        "textDocument/onTypeFormatting",
    ] {
        let response = client.send_request(method, formatting_params(method, uri));
        assert_eq!(
            response["result"],
            json!([{
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 4, "character": 0 }
                },
                "newText": "  LOCAL X = 1\nLOCAL Y = 2\n"
            }]),
            "{method}"
        );
    }
}

#[test]
fn without_experimental_features_snippet_completions_go_as_the_server_sent_them() {
    // Nothing is prepared: a snippet keeps its text and insert mode
    // whatever the editor declares, its range translated only.
    for capabilities in [
        json!({ "textDocument": { "completion": { "completionItem": {
            "snippetSupport": true,
            "insertTextModeSupport": { "valueSet": [1, 2] }
        }}}}),
        json!({ "textDocument": { "completion": { "completionItem": {
            "snippetSupport": true
        }}}}),
    ] {
        let (mut client, _dir) = Setup {
            capabilities,
            experimental: false,
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/flag-off-snippet.md";
        let text = "# t\n\n```lua\n  local SNIPPET_EDIT = 1\n```\n";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        let response = client.send_request(
            "textDocument/completion",
            json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 24 } }),
        );
        let at = json!({
            "start": { "line": 3, "character": 24 },
            "end": { "line": 3, "character": 24 }
        });
        assert_eq!(
            response["result"]["items"],
            json!([
                {"label": "call", "insertTextFormat": 2, "textEdit": {
                    "range": at, "newText": "f(${1:x})$0"
                }},
                {"label": "block", "insertTextFormat": 2, "textEdit": {
                    "range": at, "newText": "do\n\t${1:body}\n$0end"
                }}
            ]),
            "{response}"
        );
    }
}

#[test]
fn without_experimental_features_nothing_is_prepared() {
    // The peer advertises the request and is configured to prepare lua, but
    // with experimental features off it is never asked: the document goes
    // as is, and edits through it are not closed.
    let (mut client, dir) = Setup {
        wire_log: true,
        experimental: false,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/flag-off.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    let answer = hover_with_retry(&mut client, uri, 3, 4);
    assert_eq!(hover_text(&answer), "  local x = 1\n");
    let response = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 8 },
            "newName": "y"
        }),
    );
    assert!(!response["result"].is_null(), "{response}");
    // Edits translate as ranges only, as with preparation off: no text
    // re-indented.
    let response = client.send_request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 4 } }),
    );
    let items = result_items(&response);
    let item = items
        .iter()
        .find(|item| item["label"] == "echo-edit")
        .unwrap_or_else(|| panic!("no echo-edit item: {response}"));
    assert_eq!(
        item["textEdit"],
        json!({
            "range": {
                "start": { "line": 3, "character": 4 },
                "end": { "line": 3, "character": 4 }
            },
            "newText": "int(x)\nprint(y)"
        })
    );
    assert_eq!(
        item["additionalTextEdits"][0]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );
    assert_eq!(item["additionalTextEdits"][0]["newText"], "-- c\n");
    let response = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 4, "character": 0 }
            }
        }),
    );
    let hints = result_items(&response);
    assert_eq!(hints.len(), 1, "{response}");
    assert_eq!(
        hints[0]["textEdits"][0]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );
    let response = client.send_request(
        "textDocument/colorPresentation",
        json!({
            "textDocument": { "uri": uri },
            "color": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 },
            "range": {
                "start": { "line": 3, "character": 8 },
                "end": { "line": 3, "character": 9 }
            }
        }),
    );
    let presentations = result_items(&response);
    assert_eq!(presentations.len(), 2, "{response}");
    assert_eq!(
        presentations[1]["textEdit"]["range"],
        json!({
            "start": { "line": 3, "character": 6 },
            "end": { "line": 3, "character": 7 }
        })
    );
    let log = std::fs::read_to_string(dir.path().join("wire.log")).expect("wire log");
    assert!(log.contains("textDocument/didOpen\t"), "{log}");
    assert!(!log.contains("kakehashi/virtualDocument/prepare"), "{log}");
}

#[test]
fn a_document_the_peer_cannot_prepare_is_not_bridged() {
    let (mut client, dir) = Setup {
        wire_log: true,
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/failure.md";
    let text = "```lua\nprint(1)\n```\n\n```lua\nprint('FAIL')\n```\n";
    open(&mut client, uri, text);

    // The first block prepares, so the peer and the mock are up…
    let answer = hover_with_retry(&mut client, uri, 1, 1);
    assert_eq!(hover_text(&answer), "print(1)\n");
    // …and the second, whose prepare the peer answers with an error, is never
    // sent downstream: no answer for it, rather than one from unprepared
    // text. Asserted past the prepare timeout (5 s): a request waits for a
    // pending prepare, so by then its failure was decided, and a document
    // let through after it would answer.
    let until = Instant::now() + Duration::from_secs(7);
    while Instant::now() < until {
        let response = hover(&mut client, uri, 5, 1);
        assert!(response.get("error").is_none(), "{response}");
        assert!(response["result"].is_null(), "{response}");
        std::thread::sleep(Duration::from_millis(200));
    }
    // No server was ever sent it: every document opened is the first
    // block's.
    let log = std::fs::read_to_string(dir.path().join("wire.log")).expect("wire log");
    let opened: std::collections::HashSet<&str> = log
        .lines()
        .filter_map(|line| line.strip_prefix("textDocument/didOpen\t"))
        .collect();
    assert_eq!(opened.len(), 1, "{log}");
}

#[test]
fn diagnostics_arrive_once_the_prepared_text_is_sent() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/diagnostics.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);

    // The diagnostic pass that runs on open skips the region while its
    // prepare is pending; the one after the answer lands must publish it,
    // in host coordinates, without any further edit.
    let diagnostic = wait_for_echo_diagnostic(&mut client, uri, "local x = 1\n");
    assert_eq!(
        diagnostic["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 13 }
        })
    );
}

#[test]
fn a_pushed_diagnostic_places_through_the_text_its_server_held() {
    let (mut client, _dir) = Setup {
        echo: "echo-document-push",
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/push.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);

    // Pushed for the prepared text, placed through its map.
    let diagnostic = wait_for_echo_diagnostic(&mut client, uri, "local x = 1\n");
    assert_eq!(
        diagnostic["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 13 }
        })
    );

    // Deeper indentation, prepared again: the push for the new text places
    // through the new map.
    change(&mut client, uri, 2, "# t\n\n```lua\n    local x = 2\n```\n");
    let diagnostic = wait_for_echo_diagnostic(&mut client, uri, "local x = 2\n");
    assert_eq!(
        diagnostic["range"],
        json!({
            "start": { "line": 3, "character": 4 },
            "end": { "line": 3, "character": 15 }
        })
    );
}

fn pull_capabilities() -> Value {
    json!({
        "textDocument": { "diagnostic": { "dynamicRegistration": false } },
        "workspace": { "diagnostics": { "refreshSupport": true } }
    })
}

/// `client`'s answer to `method`, answering each refresh the server asks
/// for meanwhile (an editor always answers; an unanswered one would hold
/// back every later refresh).
fn request_answering_refreshes(client: &mut LspClient, method: &str, params: Value) -> Value {
    let id = client.send_request_async(method, params);
    let (response, refreshes) =
        client.receive_response_for_id_watching_server_requests(id, "workspace/diagnostic/refresh");
    for (id, _) in refreshes {
        client.send_response(id, Value::Null);
    }
    response
}

fn pull(client: &mut LspClient, uri: &str) -> Value {
    let response = request_answering_refreshes(
        client,
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

fn echo_items(report: &Value) -> Vec<Value> {
    report["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["source"] == "echo-document")
        .cloned()
        .collect()
}

#[test]
fn an_editor_pull_while_preparing_waits_for_the_prepared_text() {
    let (mut client, _dir) = Setup {
        capabilities: pull_capabilities(),
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/pull.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // The peer takes two seconds over this text: the pull waits for it, and
    // its server is asked about the prepared text, placed back through it.
    change(
        &mut client,
        uri,
        2,
        "# t\n\n```lua\n  local x = 2 -- SLOW\n```\n",
    );
    // Pulled again while nothing is reported (a loaded runner can push the
    // answer past one pull's budget), never with the text before.
    let deadline = Instant::now() + Duration::from_secs(30);
    let items = loop {
        let started = Instant::now();
        let report = pull(&mut client, uri);
        let items = echo_items(&report);
        assert!(
            items.iter().all(|item| item["message"] != "local x = 1\n"),
            "a diagnostic of the text before the edit: {report}"
        );
        if !items.is_empty() {
            break items;
        }
        // Empty only after waiting out the answer's budget (5 s).
        assert!(
            started.elapsed() >= Duration::from_secs(4),
            "the pull did not wait for the prepared text: {report}"
        );
        assert!(Instant::now() < deadline, "{report}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["message"], "local x = 2 -- SLOW\n");
    assert_eq!(
        items[0]["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 21 }
        })
    );
}

#[test]
fn an_editor_pull_folding_pushes_while_preparing_is_refreshed_once_the_text_is_sent() {
    let (mut client, _dir) = Setup {
        echo: "echo-document-push",
        capabilities: pull_capabilities(),
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/pull-push.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    // Up, the push for the prepared text folded.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !echo_items(&pull(&mut client, uri))
        .iter()
        .any(|d| d["message"] == "local x = 1\n")
    {
        assert!(Instant::now() < deadline, "the first push was never folded");
        std::thread::sleep(Duration::from_millis(100));
    }

    // A push-only server: a pull folds what it pushed, and cannot wait for
    // it. While the peer takes two seconds over the edited text, a pull
    // answers without the region and owes the editor a refresh once the
    // prepared text is sent.
    change(
        &mut client,
        uri,
        2,
        "# t\n\n```lua\n  local x = 2 -- SLOW\n```\n",
    );
    pull(&mut client, uri);
    let (id, _) = client
        .wait_for_server_request("workspace/diagnostic/refresh", Duration::from_secs(30))
        .expect("no refresh once the prepared text was sent");
    client.send_response(id, Value::Null);

    // The re-pull folds the push for the prepared text, in host coordinates.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let report = pull(&mut client, uri);
        if let Some(item) = echo_items(&report)
            .into_iter()
            .find(|d| d["message"] == "local x = 2 -- SLOW\n")
        {
            assert_eq!(item["range"]["start"], json!({ "line": 3, "character": 2 }));
            break;
        }
        assert!(Instant::now() < deadline, "{report}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_save_while_preparing_reaches_the_server_with_the_prepared_text() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/save.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // Edit and save at once: the peer takes two seconds over the edited
    // region, which is held meanwhile, and the save must still reach the
    // server — after the new prepared text, which it describes.
    change(
        &mut client,
        uri,
        2,
        "# t\n\n```lua\n  local x = 2 -- SLOW\n```\n",
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert_eq!(
        wait_for_echo_log(&mut client, "saved"),
        "local x = 2 -- SLOW\n"
    );
}

#[test]
fn a_show_document_selection_maps_through_the_prepared_text() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/show.md";
    let text = "# t\n\n```lua\n  local x = 1 -- SHOW\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // On save the echo mock asks to show its document with its first line
    // selected, in the prepared text's coordinates.
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    let (id, params) = client
        .wait_for_server_request("window/showDocument", Duration::from_secs(30))
        .expect("no showDocument");
    client.send_response(id, json!({ "success": true }));
    assert_eq!(params["uri"], uri);
    assert_eq!(
        params["selection"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 21 }
        })
    );
}

/// Save `uri`.
fn save(client: &mut LspClient, uri: &str) {
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
}

/// Answer the echo mock's applyEdit with `applied`, returning the
/// request's params (`None` when the editor is never asked) and what the
/// mock logged it was told.
fn answer_apply_edit(client: &mut LspClient, applied: bool) -> (Option<Value>, String) {
    let marker = "echo-document applied: ";
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut asked = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = client
            .try_receive_message(remaining)
            .unwrap_or_else(|| panic!("no `{marker}` log"));
        if message["method"] == "workspace/applyEdit" {
            client.send_response(message["id"].clone(), json!({ "applied": applied }));
            asked = Some(message["params"].clone());
        } else if message["method"] == "window/logMessage"
            && let Some((_, told)) = message["params"]["message"]
                .as_str()
                .and_then(|text| text.split_once(marker))
        {
            return (asked, told.to_string());
        }
    }
}

#[test]
fn an_edit_a_server_computed_for_a_prepared_text_maps_back_through_it() {
    let (mut client, _dir) = Setup {
        capabilities: json!({ "workspace": { "applyEdit": true } }),
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/apply.md";
    let text = "# t\n\n```lua\n  local x = 1 -- EDIT\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // On save the echo mock asks to apply an edit to its first line: the
    // editor is asked to apply it where the host has that line, after its
    // indentation, and the server told what the editor answered.
    save(&mut client, uri);
    let (asked, told) = answer_apply_edit(&mut client, true);
    let asked = asked.expect("the editor is asked");
    let edits = asked["edit"]["changes"][uri]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("{asked}"));
    assert_eq!(apply_edits(text, &edits), "# t\n\n```lua\n  edited\n```\n");
    assert_eq!(told, "true");
}

#[test]
fn an_edit_for_a_text_the_host_moved_from_is_refused() {
    // The echo mock's edit on save is pinned to the version it holds.
    // Unmoved, it applies. Moved: the mock asks only once it has read the
    // region's next change, its edit still for the text saved; the edit is
    // refused, the editor never asked, the server told it was not applied.
    for moved in [false, true] {
        let (mut client, _dir) = Setup {
            capabilities: json!({ "workspace": { "applyEdit": true } }),
            apply_edit_after_change: moved,
            ..Setup::default()
        }
        .start();
        let uri = "file:///prepare/apply-moved.md";
        let text = "# t\n\n```lua\n  local x = 1 -- EDIT VERSIONED\n```\n";
        open(&mut client, uri, text);
        hover_with_retry(&mut client, uri, 3, 4);
        save(&mut client, uri);
        if moved {
            change(&mut client, uri, 2, &text.replace("local x", "local y"));
        }
        let (asked, told) = answer_apply_edit(&mut client, true);
        assert_eq!(asked.is_some(), !moved, "moved: {moved}: {asked:?}");
        assert_eq!(told, (!moved).to_string(), "moved: {moved}");
    }
}

#[test]
fn gaining_a_peer_prepares_an_open_document() {
    let (mut client, _dir) = Setup {
        prepare_priorities: json!([]),
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/gain.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    // No peer: sent as is.
    let answer = hover_with_retry(&mut client, uri, 3, 4);
    assert_eq!(hover_text(&answer), "  local x = 1\n");

    // A peer from now on: the open document is prepared, without an edit,
    // and once it is, no answer comes from the text as is.
    set_prepare_priorities(&mut client, json!(["peer"]));
    let deadline = Instant::now() + Duration::from_secs(30);
    let answer = loop {
        let answer = hover(&mut client, uri, 3, 4)["result"].clone();
        if !answer.is_null() && hover_text(&answer) == "local x = 1\n" {
            break answer;
        }
        assert!(Instant::now() < deadline, "never prepared: {answer}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        answer["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 13 }
        })
    );
    for _ in 0..10 {
        let answer = hover(&mut client, uri, 3, 4)["result"].clone();
        assert_eq!(hover_text(&answer), "local x = 1\n");
    }
}

#[test]
fn losing_the_peer_sends_an_open_document_as_is() {
    let (mut client, _dir) = Setup {
        echo: "echo-document-push",
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/lose.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    let diagnostic = wait_for_echo_diagnostic(&mut client, uri, "local x = 1\n");
    assert_eq!(
        diagnostic["range"]["start"],
        json!({ "line": 3, "character": 2 })
    );

    // No peer from now on: the open document goes out as is, and what its
    // server computed for the prepared text is never placed through the
    // offset alone.
    set_prepare_priorities(&mut client, json!([]));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let params = client
            .wait_for_notification("textDocument/publishDiagnostics", remaining)
            .expect("the text as is was never diagnosed");
        let items = params["diagnostics"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for item in &items {
            if item["message"] == "local x = 1\n" {
                assert_eq!(
                    item["range"]["start"],
                    json!({ "line": 3, "character": 2 }),
                    "a prepared text's diagnostic placed through the offset alone"
                );
            }
        }
        if let Some(item) = items.iter().find(|d| d["message"] == "  local x = 1\n") {
            assert_eq!(
                item["range"],
                json!({
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 3, "character": 13 }
                })
            );
            break;
        }
    }
    let answer = hover_with_retry(&mut client, uri, 3, 4);
    assert_eq!(hover_text(&answer), "  local x = 1\n");
}

#[test]
fn a_reopened_prepared_document_is_prepared_again() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/reopen.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    open(&mut client, uri, text);
    let answer = hover_with_retry(&mut client, uri, 3, 4);
    assert_eq!(hover_text(&answer), "local x = 1\n");
    assert_eq!(
        answer["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 13 }
        })
    );
}
