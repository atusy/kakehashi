//! An edit that adds or joins lines in a fence whose lines carry a host
//! prefix — a blockquote's `> `, a list item's indentation — reaches the
//! editor with that prefix on every line it adds.
//!
//! Each test applies the edits it is answered with to the host document and
//! compares the whole result, so a prefix missing from one line, doubled on
//! another, or a closing fence pulled onto a content line all fail.

use std::time::Duration;

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lua_bridge::shutdown_client;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const URI: &str = "file:///test_prefixed_region_edits.md";

/// The fence's one content line is host line 3, behind `> `.
const BLOCKQUOTE: &str = "# Test\n\n> ```lua\n> local x = 1\n> ```\n";
/// The fence's one content line is host line 5, behind two spaces.
const LIST: &str = "# Test\n\n- item\n\n  ```lua\n  local x = 1\n  ```\n";

/// kakehashi with one mock downstream for lua, and `text` open as [`URI`].
fn open_with_mock(mode: &str, capabilities: Value, text: &str) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("prefixed_region_edits.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": capabilities,
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock": { "cmd": [mock_bin(), mode], "languages": ["lua"] }
            }}
        }),
    );
    client.send_notification("initialized", json!({}));
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": URI, "languageId": "markdown", "version": 1, "text": text
        }}),
    );
    (client, config_dir)
}

/// Ask until `pick` finds what the test is after: the downstream is still
/// handshaking when the first request arrives.
fn request_until<T>(
    client: &mut LspClient,
    method: &str,
    params: Value,
    pick: impl Fn(&Value) -> Option<T>,
) -> T {
    let mut last = Value::Null;
    for _ in 0..300 {
        let response = client.send_request(method, params.clone());
        assert!(
            response.get("error").is_none(),
            "{method} answered an error: {:?}",
            response.get("error")
        );
        if let Some(found) = pick(&response["result"]) {
            return found;
        }
        last = response["result"].clone();
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{method} never answered what the test waits for; last: {last}");
}

/// Apply LSP `TextEdit`s to `text`. The fixtures are ASCII, so a UTF-16
/// offset is a byte offset. An edit past a line's end, a reversed range or
/// overlapping edits fail the test: they are what a wrong translation looks
/// like.
fn apply_edits(text: &str, edits: &[Value]) -> String {
    let offset = |position: &Value| -> usize {
        let line = position["line"].as_u64().expect("line");
        let character = position["character"].as_u64().expect("character") as usize;
        let mut at = 0usize;
        for (index, host_line) in text.split_inclusive('\n').enumerate() {
            if index as u64 == line {
                let content = host_line.strip_suffix('\n').unwrap_or(host_line);
                assert!(
                    character <= content.len(),
                    "character {character} is past the end of host line {line} ({content:?})"
                );
                return at + character;
            }
            at += host_line.len();
        }
        // The line after a final line break is the document's last, empty line.
        assert!(
            line as usize == text.split_inclusive('\n').count() && character == 0,
            "position {position} is past the end of the host document"
        );
        at
    };
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            let start = offset(&edit["range"]["start"]);
            let end = offset(&edit["range"]["end"]);
            assert!(start <= end, "reversed range: {edit}");
            (start, end, edit["newText"].as_str().expect("newText"))
        })
        .collect();
    spans.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in spans.windows(2) {
        assert!(pair[1].0 >= pair[0].1, "overlapping edits: {pair:?}");
    }
    let mut out = text.to_string();
    for (start, end, new_text) in spans.into_iter().rev() {
        out.replace_range(start..end, new_text);
    }
    out
}

fn formatted(text: &str) -> String {
    let (mut client, _config_dir) = open_with_mock("append", json!({}), text);
    let edits = request_until(
        &mut client,
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": URI },
            "options": { "tabSize": 4, "insertSpaces": true },
        }),
        |result| result.as_array().filter(|edits| !edits.is_empty()).cloned(),
    );
    shutdown_client(&mut client);
    apply_edits(text, &edits)
}

#[test]
fn formatting_that_adds_a_line_to_a_blockquoted_fence_quotes_that_line() {
    assert_eq!(
        formatted(BLOCKQUOTE),
        "# Test\n\n> ```lua\n> local x = 1\n> -- mock-marker\n> ```\n"
    );
}

#[test]
fn formatting_that_adds_a_line_to_a_list_item_fence_indents_that_line() {
    assert_eq!(
        formatted(LIST),
        "# Test\n\n- item\n\n  ```lua\n  local x = 1\n  -- mock-marker\n  ```\n"
    );
}

#[test]
fn a_completion_that_brings_a_line_into_a_list_item_fence_indents_that_line() {
    let (mut client, _config_dir) = open_with_mock("completion-additional-line", json!({}), LIST);

    let item = request_until(
        &mut client,
        "textDocument/completion",
        json!({
            "textDocument": { "uri": URI },
            "position": { "line": 5, "character": 13 }
        }),
        |result| {
            result["items"]
                .as_array()
                .and_then(|items| items.first().cloned())
        },
    );
    shutdown_client(&mut client);

    let edits = item["additionalTextEdits"]
        .as_array()
        .unwrap_or_else(|| panic!("the item lost the line it brings: {item}"));
    assert_eq!(
        apply_edits(LIST, edits),
        "# Test\n\n- item\n\n  ```lua\n  -- import\n  local x = 1\n  ```\n"
    );
}
