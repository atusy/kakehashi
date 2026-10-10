//! E2E tests for `kakehashi/virtualDocument/prepare`: a real
//! `tsudoi-language-server` (the published npm package, run under Deno)
//! prepares markdown's lua virtual documents, and the `echo-document` mock
//! (`tests/bin/mock_formatter.rs`) reports what reached it.
//!
//! The tsudoi hook dedents content by its common indentation, deletes the
//! blank lines at the document's edges, and replaces every gap with a
//! placeholder line (`--` unless a test needs another). The mock answers hover with the text
//! it holds, ranged over the hovered line in its own coordinates, and
//! formats by uppercasing — so each test proves both directions: the
//! downstream server sees the prepared text, and its positions and edits map
//! back onto the host.
//!
//! Skipped (with a `SKIP:` line) when `deno` is not on PATH, except under
//! `KAKEHASHI_E2E_REQUIRE_DENO` (CI's `prepare-e2e` job). The first run
//! downloads the pinned package into Deno's cache, so requests retry while
//! the peer starts.

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// The published tsudoi build the hook runs on (CI pre-caches this exact
/// specifier; keep `.github/workflows/ci.yaml` in sync).
const TSUDOI: &str = "npm:@atusy/tsudoi-language-server@0.1.0-alpha.2/cli";

/// The prepare hook, as a tsudoi config: dedent content, delete its edge
/// blank lines, fill gaps with `PLACEHOLDER` (substituted per test).
const TSUDOI_CONFIG: &str = r#"
export default async () => ({
  methods: {
    initialize: async (context: any) => ({
      ...context.preparedResult,
      capabilities: {
        ...context.preparedResult.capabilities,
        experimental: { kakehashi: { virtualDocumentPrepare: true } },
      },
    }),
  },
  customMethods: {
    "kakehashi/virtualDocument/prepare": (_context: any, params: any) =>
      Promise.resolve({ result: prepare(params.segments) }),
  },
});

function prepare(segments: { type: string; content: string }[]) {
  // Lets a test exercise a peer that fails: an error response.
  if (segments.some((segment) => segment.content.includes("FAIL"))) {
    throw new Error("refusing to prepare");
  }
  // A content segment's first line starts a line when the document starts
  // there or the host text before it (a gap) ended one.
  const startsLine = segments.map((_, index) =>
    index === 0 || segments[index - 1].content.endsWith("\n")
  );
  const lineStarts = (index: number) => {
    const lines = segments[index].content.split("\n");
    return lines
      .map((text, line) => ({ text, line }))
      .filter(({ text, line }) => (line > 0 || startsLine[index]) && text.trim() !== "");
  };
  const indents = segments.flatMap((segment, index) =>
    segment.type === "content"
      ? lineStarts(index).map(({ text }) => text.length - text.trimStart().length)
      : []
  );
  const indent = indents.length > 0 ? Math.min(...indents) : 0;
  // Whole blank lines at the document's edges, as a string syntax dropping
  // them would: from the first segment's start, and before the last
  // segment's end (or before a last line holding only indentation).
  const edgeLines = (index: number) => {
    const lines = segments[index].content.split("\n");
    const last = lines.length - 1;
    const blank = (line: number) => lines[line].trim() === "";
    let leading = 0;
    while (index === 0 && leading < last && blank(leading)) leading++;
    let trailing = last;
    while (
      index === segments.length - 1 && blank(last) && trailing > leading && blank(trailing - 1)
    ) trailing--;
    const whole = (from: number, to: number) => ({
      range: { start: { line: from, character: 0 }, end: { line: to, character: 0 } },
      newText: "",
    });
    return [
      ...(leading > 0 ? [whole(0, leading)] : []),
      ...(trailing < last ? [whole(trailing, last)] : []),
    ];
  };
  return {
    segments: segments.map((segment, index) =>
      segment.type === "gap"
        ? { type: "gap", content: PLACEHOLDER }
        : {
          type: "content",
          changes: [
            ...edgeLines(index),
            ...(indent === 0 ? [] : lineStarts(index).map(({ line }) => ({
              range: {
                start: { line, character: 0 },
                end: { line, character: indent },
              },
              newText: "",
            }))),
          ],
        }
    ),
  };
}
"#;

/// Combine every lua fence of a markdown document into one virtual document,
/// so the host text between fences becomes a gap.
const COMBINED_QUERY: &str = r#"
(fenced_code_block
  (info_string (language) @injection.language)
  (code_fence_content) @injection.content
  (#set! injection.combined))
"#;

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// `true` (and a `SKIP:` line) when Deno is missing — unless
/// `KAKEHASHI_E2E_REQUIRE_DENO` is set (CI), where a missing Deno fails.
fn skip_if_deno_unavailable() -> bool {
    let available = std::process::Command::new("deno")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !available {
        assert!(
            std::env::var_os("KAKEHASHI_E2E_REQUIRE_DENO").is_none(),
            "deno is not on PATH but KAKEHASHI_E2E_REQUIRE_DENO is set"
        );
        eprintln!("SKIP: deno is not on PATH; tsudoi cannot run");
    }
    !available
}

/// Start kakehashi with tsudoi preparing markdown's lua documents for the
/// `echo-document` mock. `combined` swaps in [`COMBINED_QUERY`]; gaps are
/// replaced with `placeholder`.
fn init_client(combined: bool, placeholder: &str) -> (LspClient, tempfile::TempDir) {
    init_client_with(combined, placeholder, json!({}))
}

/// [`init_client`] with extra `bridge.lua` settings merged in.
fn init_client_with(
    combined: bool,
    placeholder: &str,
    lua_bridge: Value,
) -> (LspClient, tempfile::TempDir) {
    init_client_bridging(combined, placeholder, lua_bridge, false)
}

/// [`init_client_with`], also bridging python to the mock unprepared when
/// `unprepared_python`.
fn init_client_bridging(
    combined: bool,
    placeholder: &str,
    lua_bridge: Value,
    unprepared_python: bool,
) -> (LspClient, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let tsudoi_config = dir.path().join("tsudoi.config.ts");
    let hook = format!(
        "const PLACEHOLDER = {};\n{TSUDOI_CONFIG}",
        serde_json::to_string(placeholder).expect("placeholder as a JS string")
    );
    std::fs::write(&tsudoi_config, hook).expect("write tsudoi config");
    let config_path = dir.path().join("kakehashi.toml");
    std::fs::write(&config_path, "").expect("write kakehashi config");
    // tsudoi prepares lua and answers nothing else: the prepare method names
    // it, over the `_` method entry that leaves every other method to echo.
    let mut markdown = json!({ "bridge": { "lua": { "aggregation": {
        "kakehashi/virtualDocument/prepare": { "priorities": ["tsudoi"] },
        "_": { "priorities": ["echo"] }
    } } } });
    if let Value::Object(extra) = lua_bridge {
        for (key, value) in extra {
            match (key.as_str(), value) {
                ("aggregation", Value::Object(methods)) => {
                    for (method, config) in methods {
                        markdown["bridge"]["lua"]["aggregation"][method] = config;
                    }
                }
                (_, value) => markdown["bridge"]["lua"][key] = value,
            }
        }
    }
    if unprepared_python {
        markdown["bridge"]["python"] = json!({});
    }
    if combined {
        let query_path = dir.path().join("combined-injections.scm");
        std::fs::write(&query_path, COMBINED_QUERY).expect("write combined query");
        markdown["queries"] = json!([{
            "path": query_path.to_str().expect("UTF-8 query path"),
            "kind": "injections"
        }]);
    }

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("KAKEHASHI_EXPERIMENTAL", "true")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "tsudoi": {
                        "cmd": [
                            "deno", "run", "-A", TSUDOI,
                            "--config", tsudoi_config.to_str().expect("UTF-8 tsudoi config"),
                        ],
                        "languages": ["lua"]
                    },
                    "echo": {
                        "cmd": [mock_formatter_bin(), "echo-document"],
                        "languages": if unprepared_python { json!(["lua", "python"]) } else { json!(["lua"]) }
                    }
                },
                "languages": { "markdown": markdown }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, dir)
}

fn open(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1, "text": text }
        }),
    );
}

/// Hover until the peer and the mock are up.
fn hover_with_retry(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
    for _ in 0..600 {
        let response = client.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        );
        if !response["result"].is_null() {
            return response["result"].clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("timed out waiting for a hover through the prepared document");
}

/// Format until the peer and the mock are up.
fn format_with_retry(client: &mut LspClient, uri: &str) -> Vec<Value> {
    for _ in 0..600 {
        let response = client.send_request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        );
        if let Some(edits) = response["result"].as_array()
            && !edits.is_empty()
        {
            return edits.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("timed out waiting for formatting through the prepared document");
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

/// Apply host `TextEdit`s to `text` (ASCII fixtures: UTF-16 = bytes).
fn apply_edits(text: &str, edits: &[Value]) -> String {
    let offset = |position: &Value| -> usize {
        let line = position["line"].as_u64().unwrap() as usize;
        let character = position["character"].as_u64().unwrap() as usize;
        let start: usize = text.split_inclusive('\n').take(line).map(str::len).sum();
        start + character
    };
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            (
                offset(&edit["range"]["start"]),
                offset(&edit["range"]["end"]),
                edit["newText"].as_str().unwrap(),
            )
        })
        .collect();
    spans.sort_by_key(|span| std::cmp::Reverse(span.0));
    let mut output = text.to_string();
    for (start, end, new_text) in spans {
        output.replace_range(start..end, new_text);
    }
    output
}

#[test]
fn downstream_sees_the_dedented_document_and_maps_back() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(false, "--\n");
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

    // Formatting the dedented text keeps the host's indentation.
    let edits = format_with_retry(&mut client, uri);
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  LOCAL X = 1\n  PRINT(X)\n```\n"
    );
}

#[test]
fn downstream_sees_edge_blank_lines_deleted_and_the_host_keeps_them() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(false, "--\n");
    let uri = "file:///prepare/edges.md";
    let text = "# t\n\n```lua\n\n  #!/usr/bin/env lua\n  print(1)\n\n```\n";
    open(&mut client, uri, text);

    // The shebang is the first line downstream.
    let hover = hover_with_retry(&mut client, uri, 4, 4);
    assert_eq!(hover_text(&hover), "#!/usr/bin/env lua\nprint(1)\n");
    assert_eq!(
        hover["range"],
        json!({
            "start": { "line": 4, "character": 2 },
            "end": { "line": 4, "character": 20 }
        })
    );

    // Formatting keeps the host's blank lines and indentation.
    let edits = format_with_retry(&mut client, uri);
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n\n  #!/USR/BIN/ENV LUA\n  PRINT(1)\n\n```\n"
    );
}

#[test]
fn downstream_sees_gaps_replaced_and_lines_map_back() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(true, "--\n");
    let uri = "file:///prepare/combined.md";
    let text = "```lua\nlocal a = 1\n```\n\ntext\n\n```lua\nprint(a)\n```\n";
    open(&mut client, uri, text);

    // The host text between the fences — five lines — is one gap, which the
    // peer replaced with a single `--` line.
    let hover = hover_with_retry(&mut client, uri, 7, 1);
    assert_eq!(hover_text(&hover), "local a = 1\n--\nprint(a)\n");
    // Prepared line 2 is host line 7.
    assert_eq!(
        hover["range"],
        json!({
            "start": { "line": 7, "character": 0 },
            "end": { "line": 7, "character": 8 }
        })
    );
}

#[test]
fn formatting_a_combined_document_leaves_its_gaps_alone() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(true, "--\n");
    let uri = "file:///prepare/combined-format.md";
    let text = "```lua\nlocal a = 1\n```\n\ntext\n\n```lua\nprint(a)\n```\n";
    open(&mut client, uri, text);

    // The mock uppercases the whole prepared document, `--` placeholder
    // included (it is unchanged by uppercasing); only the two fences'
    // content may change in the host.
    let edits = format_with_retry(&mut client, uri);
    assert_eq!(
        apply_edits(text, &edits),
        "```lua\nLOCAL A = 1\n```\n\ntext\n\n```lua\nPRINT(A)\n```\n"
    );
}

#[test]
fn formatting_that_would_rewrite_a_gap_is_refused() {
    if skip_if_deno_unavailable() {
        return;
    }
    // Uppercasing turns this placeholder into `-- GAP`: an edit to host-owned
    // text, which must fail the request rather than reach the host.
    let (mut client, _dir) = init_client(true, "-- gap\n");
    let uri = "file:///prepare/combined-refused.md";
    let text = "```lua\nlocal a = 1\n```\n\ntext\n\n```lua\nprint(a)\n```\n";
    open(&mut client, uri, text);
    // Wait until the prepared document is served before formatting once.
    let hover = hover_with_retry(&mut client, uri, 7, 1);
    assert_eq!(hover_text(&hover), "local a = 1\n-- gap\nprint(a)\n");

    let response = client.send_request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 2, "insertSpaces": true }
        }),
    );
    // The request fails, or answers no edits; it never edits the host.
    let failed = response.get("error").is_some();
    let edits = response["result"].as_array().cloned().unwrap_or_default();
    assert!(
        failed || response["result"].is_null() || edits.is_empty(),
        "{response}"
    );
}

#[test]
fn an_edit_is_prepared_before_requests_read_it() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(false, "--\n");
    let uri = "file:///prepare/edit.md";
    let text = "# t\n\n```lua\n  local x = 1\n  print(x)\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // Insert a line above `print(x)`, shifting it down, and hover it at once:
    // the answer must come from the newly prepared text, not the one the
    // server held before the edit.
    let edited = "# t\n\n```lua\n  local x = 1\n  x = x + 1\n  print(x)\n```\n";
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": edited }]
        }),
    );
    // No answer may come from the text the server held before the edit:
    // a request either waits for the new prepared text or gives no answer
    // (retried here, as a loaded machine can push the sync past a single
    // request's budget). A stale answer fails at once.
    let hover = (0..600)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": 5, "character": 4 }
                }),
            );
            let hover = response["result"].clone();
            if hover.is_null() {
                std::thread::sleep(std::time::Duration::from_millis(100));
                return None;
            }
            assert_eq!(
                hover_text(&hover),
                "local x = 1\nx = x + 1\nprint(x)\n",
                "an answer from the text before the edit"
            );
            Some(hover)
        })
        .expect("no hover after the edit");
    assert_eq!(hover_text(&hover), "local x = 1\nx = x + 1\nprint(x)\n");
    assert_eq!(
        hover["range"],
        json!({
            "start": { "line": 5, "character": 2 },
            "end": { "line": 5, "character": 10 }
        })
    );
}

#[test]
fn an_unprepared_document_answers_promptly_beside_a_prepared_one() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client_bridging(false, "--\n", json!({}), true);
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
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": edited }]
        }),
    );
    let hover = |client: &mut LspClient| {
        client.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 8, "character": 0 }
            }),
        )["result"]
            .clone()
    };
    // The first request answers well within the sync budget
    // (`PREPARE_TIMEOUT`, 5 s) a stuck wait would use up — possibly with
    // nothing, under load, but never with the text before the edit.
    let started = std::time::Instant::now();
    let first = hover(&mut client);
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "the unprepared request waited {elapsed:?}"
    );
    let answer = std::iter::once(first)
        .chain((0..600).map(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            hover(&mut client)
        }))
        .find(|hover| !hover.is_null())
        .expect("no hover after the edit");
    assert_eq!(
        hover_text(&answer),
        "x = 1\ny = 2\n",
        "an answer from the text before the edit"
    );
}

#[test]
fn a_document_the_peer_cannot_prepare_is_not_bridged() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(false, "--\n");
    let uri = "file:///prepare/failure.md";
    let text = "```lua\nprint(1)\n```\n\n```lua\nprint('FAIL')\n```\n";
    open(&mut client, uri, text);

    // The first block prepares, so the peer and the mock are up…
    let hover = hover_with_retry(&mut client, uri, 1, 1);
    assert_eq!(hover_text(&hover), "print(1)\n");
    // …and the second, whose prepare the peer answers with an error, is never
    // sent downstream: no answer for it, rather than one from unprepared
    // text. Asserted past the prepare timeout (5 s): a request waits for a
    // pending prepare, so by then its failure was decided, and a document
    // let through after it would answer.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(7);
    while std::time::Instant::now() < until {
        let response = client.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 5, "character": 1 }
            }),
        );
        assert!(response["result"].is_null(), "{response}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[test]
fn diagnostics_arrive_once_the_prepared_text_is_sent() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(false, "--\n");
    let uri = "file:///prepare/diagnostics.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);

    // The diagnostic pass that runs on open skips the region while its
    // prepare is pending; the one after the answer lands must publish it,
    // in host coordinates, without any further edit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let params = client
            .wait_for_notification("textDocument/publishDiagnostics", remaining)
            .expect("no diagnostics were published for the prepared region");
        let Some(diagnostic) = params["diagnostics"]
            .as_array()
            .and_then(|items| items.iter().find(|d| d["source"] == "echo-document"))
        else {
            continue;
        };
        assert_eq!(diagnostic["message"], "local x = 1\n");
        assert_eq!(
            diagnostic["range"],
            json!({
                "start": { "line": 3, "character": 2 },
                "end": { "line": 3, "character": 13 }
            })
        );
        return;
    }
}

#[test]
fn the_concatenated_formatting_pipeline_maps_back_through_the_prepared_map() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client_with(
        false,
        "--\n",
        json!({
            "aggregation": {
                "textDocument/formatting": { "strategy": "concatenated", "priorities": ["echo"] }
            }
        }),
    );
    let uri = "file:///prepare/pipeline.md";
    let text = "# t\n\n```lua\n  local x = 1\n  print(x)\n```\n";
    open(&mut client, uri, text);

    let edits = format_with_retry(&mut client, uri);
    assert_eq!(
        apply_edits(text, &edits),
        "# t\n\n```lua\n  LOCAL X = 1\n  PRINT(X)\n```\n"
    );
}

#[test]
fn a_save_while_preparing_reaches_the_server_with_the_prepared_text() {
    if skip_if_deno_unavailable() {
        return;
    }
    let (mut client, _dir) = init_client(false, "--\n");
    let uri = "file:///prepare/save.md";
    let text = "# t\n\n```lua\n  local x = 1\n```\n";
    open(&mut client, uri, text);
    hover_with_retry(&mut client, uri, 3, 4);

    // Edit and save at once: the edited region is held while the peer
    // prepares it, and the save must still reach the server — after the new
    // prepared text, which it describes.
    let edited = "# t\n\n```lua\n  local x = 2\n```\n";
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": edited }]
        }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let params = client
            .wait_for_notification("window/logMessage", remaining)
            .expect("the server never received the save");
        // Relayed with the server's name as a prefix.
        if let Some((_, saved)) = params["message"]
            .as_str()
            .and_then(|message| message.split_once("echo-document saved: "))
        {
            assert_eq!(saved, "local x = 2\n");
            break;
        }
    }
    // The save's diagnostic pass ran while the region was held; the one after
    // its prepared text was sent must still publish it.
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let params = client
            .wait_for_notification("textDocument/publishDiagnostics", remaining)
            .expect("no diagnostics for the saved prepared text");
        if params["diagnostics"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|d| d["source"] == "echo-document" && d["message"] == "local x = 2\n")
        }) {
            return;
        }
    }
}
