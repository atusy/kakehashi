//! E2E tests for bridged `textDocument/codeAction` (#568), using the
//! `mock-lsp-formatter` test binary in `code-action` mode (one edit-carrying
//! quickfix + one bare Command action) and `code-action-lazy` mode (one
//! resolve-only action).
//!
//! Proves end-to-end:
//! - `codeActionProvider{resolveProvider}` is advertised upstream.
//! - The request range is translated host→virtual, the returned edit is
//!   re-keyed to the host URI with host-translated ranges, and the title
//!   carries the `"{title} — {server}"` suffix.
//! - A bare Command action surfaces as a disabled placeholder for a client
//!   with `disabledSupport`, and is dropped for a client without it.
//! - A lazy action is enveloped and resolved via `codeAction/resolve` for a
//!   resolve-capable client, and eager-resolved downstream (edit inline) for a
//!   client lacking resolve/data support (PR 4).

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client(client_capabilities: Value) -> (LspClient, Value, tempfile::TempDir) {
    init_client_mode("code-action", client_capabilities)
}

fn init_client_mode(
    mock_mode: &str,
    client_capabilities: Value,
) -> (LspClient, Value, tempfile::TempDir) {
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
                "mock-codeaction": { "cmd": [bin, mock_mode], "languages": ["lua"] }
            }}
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, init_response, config_dir)
}

/// Client capabilities with codeActionLiteralSupport (required for the
/// bridge to advertise codeActionProvider at all — it can only produce
/// CodeAction literals), optionally with disabledSupport.
fn literal_support_caps(disabled_support: bool) -> Value {
    let mut code_action = json!({
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } }
    });
    if disabled_support {
        code_action["disabledSupport"] = json!(true);
    }
    json!({ "textDocument": { "codeAction": code_action } })
}

/// Literal support PLUS dataSupport + resolveSupport — the client can hold a
/// lazy action's routing envelope and issue codeAction/resolve (PR 4).
fn resolve_support_caps() -> Value {
    json!({ "textDocument": { "codeAction": {
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } },
        "disabledSupport": true,
        "dataSupport": true,
        "resolveSupport": { "properties": ["edit"] }
    }}})
}

fn assert_advertised(init_response: &Value) {
    // PR 4 advertises the Options form so `resolveProvider` can be declared.
    assert_eq!(
        init_response["result"]["capabilities"]["codeActionProvider"]["resolveProvider"],
        json!(true),
        "codeActionProvider{{resolveProvider}} must be advertised for literal-support clients (#568)"
    );
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
    for _ in 0..300 {
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
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("textDocument/codeAction never returned actions");
}

#[test]
fn code_action_edit_is_host_translated_and_suffixed() {
    // No disabledSupport: the bare Command action must be dropped entirely.
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(false));
    assert_advertised(&init_response);
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
fn whole_document_range_reaches_the_injection_region() {
    // Save-time flows (editor.codeActionsOnSave) request with a range that
    // starts at (0,0), OUTSIDE any injection — the bridge must still find
    // the overlapped region instead of skipping the virt layer.
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(false));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": MARKDOWN_URI },
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 5, "character": 0 }
                    },
                    "context": { "diagnostics": [] }
                }),
            );
            match response["result"].as_array() {
                Some(actions) if !actions.is_empty() => Some(actions.clone()),
                _ => {
                    std::thread::sleep(Duration::from_millis(50));
                    None
                }
            }
        })
        .expect("whole-document codeAction never returned actions");

    assert_eq!(actions[0]["title"], "Replace with fixed — mock-codeaction");
    // The edit still lands on the fence content line in host coordinates.
    assert_eq!(
        actions[0]["edit"]["changes"][MARKDOWN_URI][0]["range"]["start"]["line"],
        3
    );

    shutdown(&mut client);
}

#[test]
fn code_action_not_advertised_without_literal_support() {
    // A client without codeActionLiteralSupport only understands Command[]
    // responses, which the bridge cannot produce — the capability must be
    // withheld so the client never asks.
    let (mut client, init_response, _config_dir) = init_client(json!({}));
    assert_eq!(
        init_response["result"]["capabilities"]["codeActionProvider"],
        Value::Null,
        "codeActionProvider must be withheld without literal support"
    );
    shutdown(&mut client);
}

#[test]
fn command_action_surfaces_as_disabled_with_disabled_support() {
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(true));
    assert_advertised(&init_response);
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

/// Issue codeAction over the lua fence line for the given client, retrying
/// while the result is null (cold downstream still handshaking).
fn code_action_over_fence(client: &mut LspClient) -> Vec<Value> {
    for _ in 0..300 {
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
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("textDocument/codeAction never returned actions");
}

#[test]
fn lazy_action_is_resolved_via_code_action_resolve() {
    // A resolve-capable client gets the lazy action back with a routing
    // envelope; codeAction/resolve then materializes the edit, re-keyed to the
    // host document with the suffix preserved.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];
    assert_eq!(lazy["title"], "Lazy organize imports — mock-codeaction");
    assert!(
        lazy["edit"].is_null(),
        "the action is still lazy (no edit yet), got: {lazy:?}"
    );
    // The routing envelope must be present under the kakehashi data key.
    assert_eq!(lazy["data"]["kakehashi"]["origin"], "mock-codeaction");
    assert_eq!(
        lazy["data"]["kakehashi"]["original_title"], "Lazy organize imports",
        "the unsuffixed title is stored for downstream title matching"
    );

    let resolved = client.send_request("codeAction/resolve", lazy.clone());
    let resolved = &resolved["result"];
    assert_eq!(
        resolved["title"], "Lazy organize imports — mock-codeaction",
        "the suffix is re-applied after resolve"
    );
    let edits = &resolved["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "the resolved edit must be re-keyed to the host URI, got: {resolved:?}"
    );
    // Virtual line 0 = host line 3 (fence content line).
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    assert_eq!(edits[0]["newText"], "organized");

    shutdown(&mut client);
}

#[test]
fn lazy_action_is_eager_resolved_without_resolve_support() {
    // A client WITHOUT resolveSupport/dataSupport cannot resolve a lazy
    // action, so the bridge eager-resolves it downstream: the codeAction
    // response already carries the materialized, host-translated edit.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy", literal_support_caps(false));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "got: {actions:?}");
    let action = &actions[0];
    assert_eq!(action["title"], "Lazy organize imports — mock-codeaction");
    let edits = &action["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "eager-resolve must materialize the edit inline, got: {action:?}"
    );
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    assert_eq!(edits[0]["newText"], "organized");
    assert!(
        action["data"].is_null(),
        "a client without dataSupport must not receive a routing envelope"
    );

    shutdown(&mut client);
}
