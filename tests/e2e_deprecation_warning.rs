//! E2E test for the one-per-session `rootMarkers` deprecation notice.
//!
//! The notice is surfaced by `initialize` and `workspace/didChangeConfiguration`
//! sharing a single session-scoped claim guard, so it fires at most once even
//! when config keeps carrying the deprecated key. This drives it through
//! didChangeConfiguration (whose notifications, unlike a warning emitted during
//! the `initialize` request, are observable by the test client) to prove the
//! warn-path works and the guard suppresses the repeat. The guard and detectors
//! are also covered in isolation by unit tests.

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
