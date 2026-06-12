//! E2E tests for bridged `textDocument/onTypeFormatting` (#354), using the
//! `mock-lsp-formatter` test binary in `on-type` mode.
//!
//! Proves the config-driven design end-to-end:
//! - `onTypeFormattingTriggers` on a `languageServers` entry makes kakehashi
//!   advertise `documentOnTypeFormattingProvider` (sorted union) at
//!   initialize; no config keeps the capability unadvertised.
//! - A request whose typed character the downstream declares comes back as
//!   host-translated `TextEdit[]`.
//! - A typed character the downstream does NOT declare is filtered by the
//!   bridge (the mock answers ANY character, so a null result proves the
//!   filter, not mock refusal).

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Path to the mock downstream server binary (built by Cargo for E2E runs;
/// `required-features = ["e2e"]`).
fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// Spawn kakehashi with the given `languageServers` map and complete the
/// initialize handshake. Returns the initialize response for capability
/// assertions.
fn init_client(language_servers: Value) -> (LspClient, Value, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("on_type_formatting.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let init_response = client.send_request(
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
    (client, init_response, config_dir)
}

/// Markdown host: the lua fence content sits on host line 3.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_on_type_formatting.md";

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

fn on_type_formatting_params(ch: &str) -> Value {
    json!({
        // Inside the lua fence content ("local x = 1" on host line 3).
        "textDocument": { "uri": MARKDOWN_URI },
        "position": { "line": 3, "character": 11 },
        "ch": ch,
        "options": { "tabSize": 4, "insertSpaces": true },
    })
}

/// Issue `textDocument/onTypeFormatting`, retrying while the result is null
/// (cold downstream servers are still handshaking when the first request
/// arrives; a not-yet-ready server yields null). An authoritative empty edit
/// list (`[]` — server ran, nothing to change) terminates immediately so a
/// misbehaving path fails fast instead of spinning out the retry budget.
fn request_with_retry(client: &mut LspClient, ch: &str) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/onTypeFormatting",
            on_type_formatting_params(ch),
        );
        assert!(
            response.get("error").is_none(),
            "kakehashi must not surface a top-level onTypeFormatting error; got: {:?}",
            response.get("error")
        );
        let result = &response["result"];
        if result.is_null() {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        return result
            .as_array()
            .cloned()
            .expect("non-null onTypeFormatting result must be a TextEdit array");
    }
    panic!("timed out waiting for a non-null onTypeFormatting result");
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_on_type_formatting_advertises_sorted_trigger_union_and_translates_edits() {
    let bin = mock_formatter_bin();
    let (mut client, init_response, _config_dir) = init_client(json!({
        "mock-ontype": {
            "cmd": [bin, "on-type"],
            "languages": ["lua"],
            "onTypeFormattingTriggers": ["}", ";"]
        }
    }));

    // Capability: sorted, deduplicated union of configured triggers.
    let provider = &init_response["result"]["capabilities"]["documentOnTypeFormattingProvider"];
    assert_eq!(
        provider["firstTriggerCharacter"], ";",
        "first trigger is the lexicographically first of the union; got: {provider:?}"
    );
    assert_eq!(
        provider["moreTriggerCharacter"],
        json!(["}"]),
        "remaining triggers follow sorted"
    );

    open_markdown(&mut client);

    // Declared trigger: the mock uppercases the virtual document; the edit
    // must come back in HOST coordinates (fence content is host line 3).
    let edits = request_with_retry(&mut client, "}");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["newText"], "LOCAL X = 1\n");
    assert_eq!(
        edits[0]["range"]["start"]["line"], 3,
        "virtual line 0 must translate to the fence content's host line"
    );

    // Undeclared trigger: the downstream is warm now (previous request
    // succeeded) and would answer any character, so a null result proves the
    // bridge filtered the request before it reached the server. (Strictly,
    // null could also mean the injection region failed to resolve — but the
    // immediately preceding request resolved the same region and returned
    // edits, so that alternative is ruled out for this sequence.)
    let response = client.send_request(
        "textDocument/onTypeFormatting",
        on_type_formatting_params("x"),
    );
    assert!(response.get("error").is_none());
    assert_eq!(
        response["result"],
        Value::Null,
        "a character the downstream does not declare must be filtered by the bridge"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_on_type_formatting_capability_absent_without_config() {
    let bin = mock_formatter_bin();
    let (mut client, init_response, _config_dir) = init_client(json!({
        "mock-ontype": {
            "cmd": [bin, "on-type"],
            "languages": ["lua"]
        }
    }));

    assert_eq!(
        init_response["result"]["capabilities"]["documentOnTypeFormattingProvider"],
        Value::Null,
        "no onTypeFormattingTriggers config -> capability not advertised"
    );

    shutdown(&mut client);
}
