//! E2E tests for `kakehashi/virtualDocument/prepare`: a real
//! `tsudoi-language-server` (the published npm package, run under Deno)
//! prepares markdown's lua virtual documents, and the `echo-document` mock
//! (`tests/bin/mock_formatter.rs`) reports what reached it.
//!
//! The tsudoi hook dedents content by its common indentation and replaces
//! every gap with a placeholder line (`--` unless a test needs another). The mock answers hover with the text
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

/// The published tsudoi build the hook runs on.
const TSUDOI: &str = "npm:@atusy/tsudoi-language-server@0.1.0-alpha.2/cli";

/// The prepare hook, as a tsudoi config: dedent content, fill gaps with
/// `PLACEHOLDER` (substituted per test).
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
  return {
    segments: segments.map((segment, index) =>
      segment.type === "gap"
        ? { type: "gap", content: PLACEHOLDER }
        : {
          type: "content",
          changes: indent === 0 ? [] : lineStarts(index).map(({ line }) => ({
            range: {
              start: { line, character: 0 },
              end: { line, character: indent },
            },
            newText: "",
          })),
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
    let dir = tempfile::TempDir::new().expect("temp dir");
    let tsudoi_config = dir.path().join("tsudoi.config.ts");
    let hook = format!(
        "const PLACEHOLDER = {};\n{TSUDOI_CONFIG}",
        serde_json::to_string(placeholder).expect("placeholder as a JS string")
    );
    std::fs::write(&tsudoi_config, hook).expect("write tsudoi config");
    let config_path = dir.path().join("kakehashi.toml");
    std::fs::write(&config_path, "").expect("write kakehashi config");
    let mut markdown = json!({ "bridge": { "lua": { "prepare": "tsudoi" } } });
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
                        "languages": []
                    },
                    "echo": {
                        "cmd": [mock_formatter_bin(), "echo-document"],
                        "languages": ["lua"]
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
    let edits = response["result"].as_array().cloned().unwrap_or_default();
    assert_eq!(apply_edits(text, &edits), text, "{response}");
}
