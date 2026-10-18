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
            .env("MOCK_PREPARE_PLACEHOLDER", self.placeholder);
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
        client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": self.capabilities,
                "workspaceFolders": null,
                "initializationOptions": {
                    "languageServers": {
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
                    },
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
    // A caret in the removed indent gets none: the server would see it at
    // the line's content.
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

    // A presentation keyed to the request range over `x` maps back onto it.
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
    assert_eq!(presentations.len(), 1, "{response}");
    assert_eq!(
        presentations[0]["textEdit"],
        json!({ "range": range, "newText": "#ff0000" })
    );
}

#[test]
fn formatting_is_not_offered_on_a_prepared_document() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/format.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // The echo mock would uppercase the document; no edit is mapped back
    // through a prepared one. Refused up front, not by a wait for a text the
    // server never holds (which would take the 5 s budget).
    for method in ["textDocument/formatting", "textDocument/rangeFormatting"] {
        let started = Instant::now();
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
        assert!(response.get("error").is_none(), "{method}: {response}");
        let edits = response["result"].as_array().cloned().unwrap_or_default();
        assert!(edits.is_empty(), "{method}: {response}");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "{method} took {elapsed:?}"
        );
    }
}

#[test]
fn formatting_before_the_document_was_looked_up_is_refused_once_it_is_prepared() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/format-early.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);

    // At once, and again while a hover may be sending the prepared text
    // before the lifecycle pass looked the region up: each waits for the
    // answer, and is refused up front rather than by waiting out the
    // budget for a text its server never holds.
    for attempt in 0..10 {
        let started = Instant::now();
        let response = client.send_request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        let edits = response["result"].as_array().cloned().unwrap_or_default();
        assert!(edits.is_empty(), "{response}");
        // The first may wait for the peer to start and answer.
        let elapsed = started.elapsed();
        assert!(
            attempt == 0 || elapsed < Duration::from_millis(2500),
            "formatting took {elapsed:?}"
        );
        let _ = hover(&mut client, uri, 3, 4);
    }
}

#[test]
fn a_rename_before_the_document_was_looked_up_is_refused_once_it_is_prepared() {
    let (mut client, _dir) = Setup::default().start();
    let uri = "file:///prepare/rename.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);

    // At once: the answer is not in yet. The request waits for it, finds
    // the document prepared, and is refused — the echo mock would rename.
    for _ in 0..20 {
        let response = client.send_request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 3, "character": 8 },
                "newName": "y"
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert!(response["result"].is_null(), "{response}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The document was bridged meanwhile.
    hover_with_retry(&mut client, uri, 3, 4);
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

#[test]
fn an_edit_a_server_computed_for_a_prepared_text_is_refused() {
    let (mut client, _dir) = Setup {
        capabilities: json!({ "workspace": { "applyEdit": true } }),
        ..Setup::default()
    }
    .start();
    let uri = "file:///prepare/apply.md";
    let text = "# t\n\n```lua\n  local x = 1 -- EDIT\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // On save the echo mock asks to apply an edit to its first line: no edit
    // is mapped back through a prepared document, so the editor is never
    // asked, and the server is told it was not applied.
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    let marker = "echo-document applied: ";
    let deadline = Instant::now() + Duration::from_secs(30);
    let applied = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let message = client
            .try_receive_message(remaining)
            .unwrap_or_else(|| panic!("no `{marker}` log"));
        assert_ne!(
            message["method"], "workspace/applyEdit",
            "the editor was asked to apply an edit: {message}"
        );
        if message["method"] == "window/logMessage"
            && let Some((_, applied)) = message["params"]["message"]
                .as_str()
                .and_then(|text| text.split_once(marker))
        {
            break applied.to_string();
        }
    };
    assert_eq!(applied, "false");
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
