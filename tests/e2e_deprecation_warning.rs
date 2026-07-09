//! E2E tests for one-per-session deprecation notices.
//!
//! `rootMarkers` shares a session-scoped guard between `initialize` and
//! `workspace/didChangeConfiguration`; this drives the observable didChange path
//! to prove the guard suppresses repeats. The unwrapped didChangeConfiguration
//! notice is didChange-only, and is covered separately below. The guard and
//! detectors are also covered in isolation by unit tests.

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(15);

/// The deprecation popup: a `window/showMessage` naming the moved key.
fn is_deprecation_notice(params: &Value) -> bool {
    params["message"]
        .as_str()
        .is_some_and(|m| m.contains("rootMarkers") && m.contains("deprecated"))
}

fn is_unwrapped_didchange_deprecation_notice(params: &Value) -> bool {
    params["message"].as_str().is_some_and(|m| {
        m.contains("unwrapped") && m.contains("didChangeConfiguration") && m.contains("deprecated")
    })
}

/// The `didChangeConfiguration` success log ("Configuration updated!"). The
/// handler emits the claim-gated popup *before* this log, so seeing this log
/// with no preceding popup is a positive proof that no popup fired — no flaky
/// negative-timeout wait.
fn is_config_updated(params: &Value) -> bool {
    params["message"]
        .as_str()
        .is_some_and(|m| m.contains("Configuration updated"))
}

fn config_with_root_markers() -> Value {
    json!({
        "settings": {
            "languageServers": {
                "x": { "cmd": ["true"], "languages": ["lua"], "rootMarkers": [".git"] }
            }
        }
    })
}

fn flat_didchange_config(auto_install: bool) -> Value {
    json!({ "settings": { "autoInstall": auto_install } })
}

fn wrapped_didchange_config(auto_install: bool) -> Value {
    json!({ "settings": { "kakehashi": { "autoInstall": auto_install } } })
}

fn query_effective_settings(client: &mut LspClient) -> Value {
    client
        .send_request("kakehashi/internal/effectiveConfiguration", json!({}))
        .get("result")
        .expect("should have result")
        .get("settings")
        .expect("should have settings")
        .clone()
}

#[test]
fn e2e_root_markers_deprecation_warns_once_across_didchange() {
    let config_dir = tempfile::TempDir::new().expect("temp config dir");
    let config_path = config_dir.path().join("kakehashi.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 temp path"))
        .build();

    // Clean initialize (no rootMarkers) leaves the once-per-session slot free.
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // First config carrying `rootMarkers` → the deprecation popup fires.
    client.send_notification(
        "workspace/didChangeConfiguration",
        config_with_root_markers(),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage"], TIMEOUT, is_deprecation_notice)
        .expect("first rootMarkers config should surface the deprecation popup");
    assert_eq!(method, "window/showMessage");
    assert_eq!(
        params["type"].as_i64(),
        Some(2),
        "deprecation notice should be MessageType::WARNING"
    );
    // Drain this reconfig's "Configuration updated!" log so the buffer is clean
    // before the second reconfig — otherwise the ordering assertion below could
    // match this log instead of the second one.
    client
        .wait_for_notification_where(&["window/logMessage"], TIMEOUT, is_config_updated)
        .expect("first reconfig should log a config-updated message");

    // Second config still carrying `rootMarkers` → the guard has latched, so no
    // second popup precedes the config-updated log.
    client.send_notification(
        "workspace/didChangeConfiguration",
        config_with_root_markers(),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage", "window/logMessage"], TIMEOUT, |p| {
            is_deprecation_notice(p) || is_config_updated(p)
        })
        .expect("second reconfig should log a config-updated message");
    assert_eq!(
        method, "window/logMessage",
        "no second deprecation popup should fire; got: {params:?}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_unwrapped_didchange_deprecation_warns_once_and_ignores_unrelated_settings() {
    let config_dir = tempfile::TempDir::new().expect("temp config dir");
    let config_path = config_dir.path().join("kakehashi.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 temp path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Unrelated editor/server settings are ignored as an empty update and must
    // not claim the flat-shape deprecation slot.
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "gopls": { "usePlaceholders": true } } }),
    );

    // Canonical wrapped settings are applied without the flat-shape
    // deprecation popup.
    client.send_notification(
        "workspace/didChangeConfiguration",
        wrapped_didchange_config(false),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage", "window/logMessage"], TIMEOUT, |p| {
            is_unwrapped_didchange_deprecation_notice(p) || is_config_updated(p)
        })
        .expect("wrapped reconfig should log a config-updated message");
    assert_eq!(
        method, "window/logMessage",
        "wrapped didChange config must not trigger the flat-shape deprecation popup; got: {params:?}"
    );
    assert_eq!(
        query_effective_settings(&mut client)["autoInstall"],
        json!(false),
        "wrapped didChange config should update the effective runtime settings"
    );

    // The first actual flat kakehashi runtime setting still gets the warning,
    // proving the unrelated payload above did not consume the once-per-session
    // guard and the wrapped payload above did not warn.
    client.send_notification(
        "workspace/didChangeConfiguration",
        flat_didchange_config(true),
    );
    let (method, params) = client
        .wait_for_notification_where(
            &["window/showMessage"],
            TIMEOUT,
            is_unwrapped_didchange_deprecation_notice,
        )
        .expect("first flat didChange config should surface the deprecation popup");
    assert_eq!(method, "window/showMessage");
    assert_eq!(
        params["type"].as_i64(),
        Some(2),
        "deprecation notice should be MessageType::WARNING"
    );
    client
        .wait_for_notification_where(&["window/logMessage"], TIMEOUT, is_config_updated)
        .expect("first flat reconfig should log a config-updated message");
    assert_eq!(
        query_effective_settings(&mut client)["autoInstall"],
        json!(true),
        "flat didChange config should still update the effective runtime settings"
    );

    // A second flat payload is still accepted, but the deprecation popup does
    // not repeat before the successful update log.
    client.send_notification(
        "workspace/didChangeConfiguration",
        flat_didchange_config(false),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage", "window/logMessage"], TIMEOUT, |p| {
            is_unwrapped_didchange_deprecation_notice(p) || is_config_updated(p)
        })
        .expect("second flat reconfig should log a config-updated message");
    assert_eq!(
        method, "window/logMessage",
        "no second flat-shape deprecation popup should fire; got: {params:?}"
    );
    assert_eq!(
        query_effective_settings(&mut client)["autoInstall"],
        json!(false),
        "second flat didChange config should still update the effective runtime settings"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
