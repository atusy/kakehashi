//! E2E tests for forwarding `window/logMessage` / `window/showMessage`
//! notifications from bridged downstream servers to the editor (#378), using
//! the `mock-lsp-formatter` test binary in `notify` mode (which emits a
//! showMessage followed by a logMessage right after `initialize`).
//!
//! `window/logMessage` obeys one live workspace policy for both downstream and
//! kakehashi-originated messages. `window/showMessage` remains unaffected. The mock sends
//! showMessage BEFORE logMessage and the bridge preserves notification order
//! end-to-end (single reader task -> single forwarding loop), so the test can
//! assert showMessage arrives first, then logMessage — no flaky
//! negative-timeout assertion needed.

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Path to the mock downstream server binary (built by Cargo for E2E runs;
/// `required-features = ["e2e"]`).
fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// Spawn kakehashi with the given `languageServers` map and complete the
/// initialize handshake.
fn init_client(initialization_options: Value) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("window_notifications.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

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
            "initializationOptions": initialization_options
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// Markdown host with a lua fence: opening it makes kakehashi spawn the
/// bridged server configured for lua, whose `initialize` reply triggers the
/// mock's notifications.
fn open_markdown(client: &mut LspClient) {
    open_markdown_with(client, "file:///test_window_notifications.md", "lua");
}

fn open_markdown_with(client: &mut LspClient, uri: &str, fence_language: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": format!("# Test\n\n```{fence_language}\nvalue = 1\n```\n")
            }
        }),
    );
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// Matches only notifications forwarded from the mock downstream server,
/// ignoring kakehashi's own window/logMessage output.
///
/// Reliable because only the bridge's forwarding path prepends
/// `[kakehashi:<server>]` (reader.rs `prefixed_message`); kakehashi's own
/// log/show messages never contain that marker.
fn from_mock(params: &Value) -> bool {
    params["message"]
        .as_str()
        .is_some_and(|m| m.contains("[kakehashi:mock-notify]"))
}

#[test]
fn e2e_forwards_window_show_and_log_messages() {
    let bin = mock_formatter_bin();
    let (mut client, _config_dir) = init_client(json!({
        "languageServers": {
            "mock-notify": { "cmd": [bin, "notify"], "languages": ["lua"] }
        }
    }));

    open_markdown(&mut client);

    // The bridge forwards both window/* notifications with no per-server gate.
    // The mock emits showMessage BEFORE logMessage and the bridge preserves
    // order end-to-end, so showMessage must arrive first.
    let (show_method, show_params) = client
        .wait_for_notification_where(
            &["window/showMessage", "window/logMessage"],
            Duration::from_secs(15),
            from_mock,
        )
        .expect("the mock's window/showMessage should be forwarded");
    assert_eq!(
        show_method, "window/showMessage",
        "showMessage is forwarded first; got params: {show_params:?}"
    );
    assert_eq!(
        show_params["message"].as_str(),
        Some("[kakehashi:mock-notify] mock show line"),
        "showMessage should be forwarded verbatim with the server-name prefix"
    );
    assert_eq!(
        show_params["type"].as_i64(),
        Some(2),
        "MessageType::WARNING should pass through unchanged"
    );

    let (log_method, log_params) = client
        .wait_for_notification_where(
            &["window/showMessage", "window/logMessage"],
            Duration::from_secs(15),
            from_mock,
        )
        .expect("the mock's window/logMessage should be forwarded");
    assert_eq!(
        log_method, "window/logMessage",
        "logMessage follows the showMessage; got params: {log_params:?}"
    );
    assert_eq!(
        log_params["message"].as_str(),
        Some("[kakehashi:mock-notify] mock log line"),
        "logMessage should be forwarded verbatim with the server-name prefix"
    );
    assert_eq!(
        log_params["type"].as_i64(),
        Some(3),
        "MessageType::INFO should pass through unchanged"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_global_log_policy_gates_both_origins_and_updates_live() {
    let bin = mock_formatter_bin();
    let (mut client, _config_dir) = init_client(json!({
        "features": {
            "window/logMessage": { "logLevel": "off" }
        },
        "languageServers": {
            "mock-muted": { "cmd": [bin, "notify"], "languages": ["lua"] },
            "mock-live": { "cmd": [bin, "notify"], "languages": ["python"] }
        }
    }));

    open_markdown_with(
        &mut client,
        "file:///test_window_notifications_muted.md",
        "lua",
    );
    let (method, _) = client
        .wait_for_notification_where(
            &["window/showMessage", "window/logMessage"],
            Duration::from_secs(15),
            |params| {
                params["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("[kakehashi:mock-muted]"))
            },
        )
        .expect("showMessage must remain visible while logMessage is off");
    assert_eq!(method, "window/showMessage");
    assert!(
        client
            .wait_for_notification_where(
                &["window/logMessage"],
                Duration::from_millis(300),
                |params| params["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("[kakehashi:mock-muted]")),
            )
            .is_none(),
        "downstream info log should be suppressed by the global off policy"
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "kakehashi": {
                    "features": {
                        "window/logMessage": { "logLevel": "info" }
                    }
                }
            }
        }),
    );
    client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            params["message"]
                .as_str()
                .is_some_and(|message| message.contains("Configuration updated"))
        })
        .expect("kakehashi's own info log should use the newly applied policy");

    open_markdown_with(
        &mut client,
        "file:///test_window_notifications_live.md",
        "python",
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(15), |params| {
            params["message"]
                .as_str()
                .is_some_and(|message| message.contains("[kakehashi:mock-live]"))
        })
        .expect("subsequent downstream info log should use the live policy");
    assert_eq!(method, "window/logMessage");
    assert_eq!(params["type"].as_i64(), Some(3));

    shutdown(&mut client);
}
