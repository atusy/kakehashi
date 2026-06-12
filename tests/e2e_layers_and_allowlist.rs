//! E2E tests for the aggregation-priorities-wildcard allowlist semantics and
//! the cross-layer-aggregation `layers` gate, over the full LSP protocol with
//! a real downstream server (`mock-lsp-formatter`).
//!
//! Both tests use the same deterministic transition pattern: warm up until
//! formatting *succeeds* (proving the downstream server is ready and the
//! bridge path works), then flip the configuration via
//! `workspace/didChangeConfiguration` and assert formatting goes null —
//! ruling out "server not ready yet" as an alternative explanation for the
//! null.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_layers_allowlist.md";

/// Spawn kakehashi with the mock formatter configured for lua injections and
/// complete the handshake plus didOpen.
///
/// An empty `--config-file` isolates the test from any user-level
/// configuration; all settings flow through `initializationOptions` and
/// `workspace/didChangeConfiguration`.
fn init_warm_client() -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("empty.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

    let bin = mock_formatter_bin();
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
            "initializationOptions": {
                "languageServers": {
                    "mock-upper": { "cmd": [bin, "upper"], "languages": ["lua"] },
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

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
    (client, config_dir)
}

fn send_formatting(client: &mut LspClient) -> Value {
    let response = client.send_request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": MARKDOWN_URI },
            "options": { "tabSize": 4, "insertSpaces": true },
        }),
    );
    assert!(
        response.get("error").is_none(),
        "formatting must not surface a top-level error; got: {:?}",
        response.get("error")
    );
    response["result"].clone()
}

/// Poll until formatting yields a non-empty edit list (downstream warm).
fn wait_until_formatting_succeeds(client: &mut LspClient) {
    for _ in 0..300 {
        let result = send_formatting(client);
        if result.as_array().is_some_and(|edits| !edits.is_empty()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("formatting never succeeded — downstream server did not warm up");
}

/// Poll until formatting yields null/empty (the config flip has applied).
fn wait_until_formatting_is_null(client: &mut LspClient, context: &str) {
    for _ in 0..300 {
        let result = send_formatting(client);
        let is_null_or_empty =
            result.is_null() || result.as_array().is_some_and(|edits| edits.is_empty());
        if is_null_or_empty {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("formatting still produced edits — {context}");
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// aggregation-priorities-wildcard: `priorities` is an allowlist. Naming only
/// a server that is not configured for the region must exclude the configured
/// server entirely — under the old preference semantics, mock-upper would
/// have kept formatting as the implicit fallback.
#[test]
fn e2e_priorities_allowlist_excludes_unlisted_servers() {
    let (mut client, _config_dir) = init_warm_client();
    wait_until_formatting_succeeds(&mut client);

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "markdown": {
                        "bridge": {
                            "lua": {
                                "aggregation": {
                                    "textDocument/formatting": { "priorities": ["ghost"] }
                                }
                            }
                        }
                    }
                }
            }
        }),
    );

    wait_until_formatting_is_null(
        &mut client,
        "priorities = [\"ghost\"] must exclude the unlisted mock-upper (allowlist), \
         not fall back to it",
    );
    shutdown(&mut client);
}

/// cross-layer-aggregation: omitting "virt" from `layers.order` disables
/// injection bridging for the method even though a working downstream server
/// is configured.
#[test]
fn e2e_layers_order_without_virt_disables_bridging() {
    let (mut client, _config_dir) = init_warm_client();
    wait_until_formatting_succeeds(&mut client);

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "markdown": {
                        "layers": {
                            "aggregation": {
                                "textDocument/formatting": { "order": ["native"] }
                            }
                        }
                    }
                }
            }
        }),
    );

    wait_until_formatting_is_null(
        &mut client,
        "layers.order = [\"native\"] must gate off the virt bridge for formatting",
    );
    shutdown(&mut client);
}
