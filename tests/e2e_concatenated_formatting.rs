//! E2E tests for the concatenated formatting pipeline
//! (concatenated-formatting-pipeline): a real multi-server chain over the
//! full LSP protocol, using the `mock-lsp-formatter` test binary as the
//! downstream servers (see `tests/bin/mock_formatter.rs`).
//!
//! The chain proves *serial* semantics end-to-end: the `append` step's marker
//! stays lowercase only if it ran AFTER the `upper` step — i.e. each server
//! formatted the previous server's output, not the original region text.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Path to the mock downstream formatter binary (built by Cargo for E2E runs;
/// `required-features = ["e2e"]`).
fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// Spawn kakehashi with `config_toml` as its config file and the given
/// `languageServers` map, and complete the initialize handshake.
fn init_client(config_toml: &str, language_servers: Value) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("concatenated.toml");
    std::fs::write(&config_path, config_toml).expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": language_servers }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// Issue `textDocument/formatting`, retrying while the result is null.
///
/// Cold downstream servers are still handshaking when the first request
/// arrives; the pipeline treats a not-yet-ready server as a failed step
/// (skip-and-continue), which yields a null result. Retrying until the
/// servers are Ready keeps the test free of timing assumptions.
fn request_formatting_with_retry(client: &mut LspClient, uri: &str) -> Option<Vec<Value>> {
    for _ in 0..30 {
        let response = client.send_request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 4, "insertSpaces": true },
            }),
        );
        assert!(
            response.get("error").is_none(),
            "kakehashi must not surface a top-level formatting error; got: {:?}",
            response.get("error")
        );
        if let Some(edits) = response.get("result").and_then(Value::as_array)
            && !edits.is_empty()
        {
            return Some(edits.clone());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    None
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// Markdown host: the lua fence content sits on host line 3.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_concatenated_formatting.md";

fn open_markdown(client: &mut LspClient) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MARKDOWN_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MARKDOWN
            }
        }),
    );
    // Give kakehashi time to parse the host document and resolve injections.
    std::thread::sleep(std::time::Duration::from_millis(1000));
}

#[test]
fn e2e_concatenated_formatting_chains_two_servers_in_priority_order() {
    let bin = mock_formatter_bin();
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge.lua.aggregation."textDocument/formatting"]
strategy = "concatenated"
priorities = ["mock-upper", "mock-append"]
"#,
        json!({
            "mock-upper": { "cmd": [bin, "upper"], "languages": ["lua"] },
            "mock-append": { "cmd": [bin, "append"], "languages": ["lua"] },
        }),
    );

    open_markdown(&mut client);

    let edits = request_formatting_with_retry(&mut client, MARKDOWN_URI)
        .expect("concatenated pipeline must produce a region replacement edit");

    // The pipeline collapses the whole chain into ONE region-replacement edit
    // (Decision point 4), never one edit per server.
    assert_eq!(
        edits.len(),
        1,
        "expected a single region replacement edit, got: {edits:?}"
    );

    let new_text = edits[0]["newText"]
        .as_str()
        .expect("replacement edit must carry newText");

    // Step 1 (upper) ran: the original lua got uppercased.
    assert!(
        new_text.contains("LOCAL X = 1"),
        "upper step must have formatted the region; newText: {new_text:?}"
    );
    // Step 2 (append) ran ON STEP 1'S OUTPUT: its marker is present and still
    // lowercase. Had the order been reversed, upper would have shouted the
    // marker too — this is the serial-semantics proof.
    assert!(
        new_text.contains("-- mock-marker"),
        "append step must have added its marker; newText: {new_text:?}"
    );
    assert!(
        !new_text.contains("MOCK-MARKER"),
        "marker must stay lowercase: append runs after upper, each server \
         formats the previous server's output; newText: {new_text:?}"
    );

    // The replacement targets the fence content in host coordinates
    // (content starts at host line 3, column 0).
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    assert_eq!(edits[0]["range"]["start"]["character"], 0);

    shutdown(&mut client);
}

#[test]
fn e2e_concatenated_formatting_falls_back_to_range_formatting_for_range_only_server() {
    // The mock advertises ONLY documentRangeFormattingProvider, so the
    // pipeline must fall back to whole-region rangeFormatting (Decision
    // point 3.2) — sending textDocument/formatting would get no provider.
    let bin = mock_formatter_bin();
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge.lua.aggregation."textDocument/formatting"]
strategy = "concatenated"
priorities = ["mock-range-upper"]
"#,
        json!({
            "mock-range-upper": { "cmd": [bin, "range-upper"], "languages": ["lua"] },
        }),
    );

    open_markdown(&mut client);

    let edits = request_formatting_with_retry(&mut client, MARKDOWN_URI)
        .expect("range-only server must still format via the whole-region fallback");

    assert_eq!(edits.len(), 1, "expected one region replacement edit");
    let new_text = edits[0]["newText"]
        .as_str()
        .expect("replacement edit must carry newText");
    assert!(
        new_text.contains("LOCAL X = 1"),
        "range-only server must have formatted the region through the \
         rangeFormatting fallback; newText: {new_text:?}"
    );

    shutdown(&mut client);
}
