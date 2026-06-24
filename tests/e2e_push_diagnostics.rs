//! E2E (#427): a downstream virt-server's **spontaneous** `publishDiagnostics`
//! push reaches the editor in **host** coordinates, and a follow-up empty push
//! clears that source's contribution.
//!
//! The push path already has strong unit coverage (merge, transform, reader
//! routing); this proves the integrated flow end-to-end with a real downstream
//! that pushes on its own (not in response to a pull).
//!
//! Uses the `mock-lsp-formatter` binary in `diagnostics-push` mode: it pushes one
//! diagnostic on **virtual line 0** of each opened virtual doc, and pushes an empty
//! list on `didChange`.
//!
//! No-resurrection-after-close (a late push dropped once the editor closed the doc)
//! is covered by the `#421` push-accept-guard unit tests; an e2e for it would be a
//! flaky negative-timeout assertion, so it is intentionally left out here.

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Path to the mock downstream server binary (built by Cargo for E2E runs).
fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const MD_URI: &str = "file:///test_push_diagnostics.md";

/// `local x = 1` (the lua fence content) is host line 3 (0-indexed):
/// `0:"# Test"  1:""  2:"```lua"  3:"local x = 1"  4:"```"`. So the injected
/// virtual document's line 0 maps to host line 3.
const MD_TEXT: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const HOST_LINE: i64 = 3;

fn init_client() -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("push_diagnostics.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf8 path"))
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
                    "mock-push": { "cmd": [mock_bin(), "diagnostics-push"], "languages": ["lua"] }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// True if `params` is a `publishDiagnostics` for the host doc that contains the
/// mock's pushed diagnostic.
fn has_pushed_diag(params: &Value) -> bool {
    params["uri"] == json!(MD_URI)
        && params["diagnostics"]
            .as_array()
            .is_some_and(|ds| ds.iter().any(is_mock_push))
}

/// True if `params` is a `publishDiagnostics` for the host doc that NO LONGER
/// contains the mock's pushed diagnostic (cleared).
fn cleared_host_diag(params: &Value) -> bool {
    params["uri"] == json!(MD_URI)
        && params["diagnostics"]
            .as_array()
            .is_some_and(|ds| !ds.iter().any(is_mock_push))
}

fn is_mock_push(d: &Value) -> bool {
    d["message"]
        .as_str()
        .is_some_and(|m| m.contains("mock-push-diag"))
}

#[test]
fn e2e_push_diagnostic_reaches_editor_in_host_coords_then_clears() {
    let (mut client, _config_dir) = init_client();

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MD_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MD_TEXT
            }
        }),
    );

    // The mock pushes a diagnostic on virtual line 0 of the lua region; the bridge
    // translates it to host coordinates and publishes it for the host document.
    let (_method, params) = client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            has_pushed_diag,
        )
        .expect("editor should receive the spontaneously pushed diagnostic for the host doc");

    let pushed = params["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .find(|d| is_mock_push(d))
        .expect("the mock-push diagnostic should be present");
    assert_eq!(
        pushed["range"]["start"]["line"],
        json!(HOST_LINE),
        "virtual line 0 must be translated to host line {HOST_LINE} (the lua fence content line)"
    );

    // A follow-up empty push (the mock pushes `[]` on didChange) clears this
    // source's contribution for the host doc.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": MD_TEXT }]
        }),
    );

    let cleared = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        cleared_host_diag,
    );
    assert!(
        cleared.is_some(),
        "an empty push should clear the source's diagnostic for the host doc"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
