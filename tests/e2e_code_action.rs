//! E2E tests for bridged `textDocument/codeAction` (#568, edit-carrying
//! stage), using the `mock-lsp-formatter` test binary in `code-action` mode
//! (one edit-carrying quickfix + one bare Command action).
//!
//! Proves end-to-end:
//! - `codeActionProvider` is advertised upstream.
//! - The request range is translated host→virtual, the returned edit is
//!   re-keyed to the host URI with host-translated ranges, and the title
//!   carries the `"{title} — {server}"` suffix.
//! - A bare Command action surfaces as a disabled placeholder for a client
//!   with `disabledSupport`, and is dropped for a client without it.

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client(client_capabilities: Value) -> (LspClient, tempfile::TempDir) {
    let bin = mock_formatter_bin();
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("code_action.toml");
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
            "capabilities": client_capabilities,
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock-codeaction": { "cmd": [bin, "code-action"], "languages": ["lua"] }
            }}
        }),
    );
    assert_eq!(
        init_response["result"]["capabilities"]["codeActionProvider"],
        json!(true),
        "codeActionProvider must be advertised (#568)"
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// Markdown host: the lua fence content sits on host line 3.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_code_action.md";

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

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
    std::thread::sleep(Duration::from_millis(1000));
}

/// Issue `textDocument/codeAction` over the lua fence line, retrying while
/// the result is null (cold downstream still handshaking).
fn code_action_with_retry(client: &mut LspClient) -> Vec<Value> {
    for _ in 0..20 {
        let response = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": MARKDOWN_URI },
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 3, "character": 5 }
                },
                "context": { "diagnostics": [] }
            }),
        );
        if let Some(actions) = response["result"].as_array()
            && !actions.is_empty()
        {
            return actions.clone();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    panic!("textDocument/codeAction never returned actions");
}

#[test]
fn code_action_edit_is_host_translated_and_suffixed() {
    // No disabledSupport: the bare Command action must be dropped entirely.
    let (mut client, _config_dir) = init_client(json!({}));
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);

    assert_eq!(
        actions.len(),
        1,
        "the Command action must be dropped without disabledSupport, got: {actions:?}"
    );
    let action = &actions[0];
    assert_eq!(action["title"], "Replace with fixed — mock-codeaction");
    assert_eq!(action["kind"], "quickfix");
    let edits = &action["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "edit must be re-keyed to the host URI, got: {action:?}"
    );
    // Virtual line 0 = host line 3 (fence content line).
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    assert_eq!(edits[0]["newText"], "fixed");

    shutdown(&mut client);
}

#[test]
fn command_action_surfaces_as_disabled_with_disabled_support() {
    let (mut client, _config_dir) = init_client(json!({
        "textDocument": { "codeAction": { "disabledSupport": true } }
    }));
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);

    assert_eq!(actions.len(), 2, "got: {actions:?}");
    let disabled: Vec<&Value> = actions
        .iter()
        .filter(|a| a["disabled"].is_object())
        .collect();
    assert_eq!(disabled.len(), 1, "exactly one disabled placeholder");
    assert_eq!(disabled[0]["title"], "Run mock command — mock-codeaction");
    assert!(
        disabled[0]["command"].is_null() && disabled[0]["edit"].is_null(),
        "a disabled placeholder must not carry an executable payload"
    );

    shutdown(&mut client);
}
