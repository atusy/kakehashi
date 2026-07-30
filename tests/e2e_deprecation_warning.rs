//! E2E tests for the one-per-session config deprecation notices
//! (`rootMarkers`, the top-level `autoInstall`, and the unwrapped
//! `didChangeConfiguration` shape).
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

fn section_wrapped_config_with_root_markers() -> Value {
    json!({
        "settings": {
            "kakehashi": {
                "languageServers": {
                    "x": { "cmd": ["true"], "languages": ["lua"], "rootMarkers": [".git"] }
                }
            }
        }
    })
}

/// The top-level-`autoInstall` notice, distinct from the `rootMarkers` one.
fn is_auto_install_deprecation_notice(params: &Value) -> bool {
    params["message"]
        .as_str()
        .is_some_and(|m| m.contains("autoInstall") && m.contains("deprecated"))
}

/// The canonical replacement spelling, which must NOT warn.
fn canonical_per_language_auto_install(auto_install: bool) -> Value {
    json!({
        "settings": {
            "kakehashi": {
                "languages": { "_": { "autoInstall": auto_install } }
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
fn e2e_root_markers_deprecation_warns_for_section_wrapped_didchange() {
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

    client.send_notification(
        "workspace/didChangeConfiguration",
        section_wrapped_config_with_root_markers(),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage"], TIMEOUT, is_deprecation_notice)
        .expect("section-wrapped rootMarkers config should surface the deprecation popup");
    assert_eq!(method, "window/showMessage");
    assert_eq!(
        params["type"].as_i64(),
        Some(2),
        "deprecation notice should be MessageType::WARNING"
    );
    client
        .wait_for_notification_where(&["window/logMessage"], TIMEOUT, is_config_updated)
        .expect("section-wrapped rootMarkers config should still be applied successfully");

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

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "gopls": { "usePlaceholders": true } } }),
    );

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

#[test]
fn e2e_auto_install_deprecation_warns_once_and_spares_the_canonical_key() {
    let config_dir = tempfile::TempDir::new().expect("temp config dir");
    let config_path = config_dir.path().join("kakehashi.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 temp path"))
        .build();

    // Clean initialize (no `autoInstall` anywhere) leaves the slot free.
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

    // The canonical per-language spelling must NOT warn — it is what the notice
    // tells people to write, so a name-only detector would warn about its own
    // migration target. Positive proof: the config-updated log arrives with no
    // popup ahead of it.
    client.send_notification(
        "workspace/didChangeConfiguration",
        canonical_per_language_auto_install(false),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage", "window/logMessage"], TIMEOUT, |p| {
            is_auto_install_deprecation_notice(p) || is_config_updated(p)
        })
        .expect("canonical reconfig should log a config-updated message");
    assert_eq!(
        method, "window/logMessage",
        "`[languages._] autoInstall` must not trigger the deprecation popup; got: {params:?}"
    );
    assert_eq!(
        query_effective_settings(&mut client)["languages"]["_"]["autoInstall"],
        json!(false),
        "canonical key should still reach the effective runtime settings"
    );

    // The deprecated top-level spelling → the popup fires.
    client.send_notification(
        "workspace/didChangeConfiguration",
        wrapped_didchange_config(true),
    );
    let (method, params) = client
        .wait_for_notification_where(
            &["window/showMessage"],
            TIMEOUT,
            is_auto_install_deprecation_notice,
        )
        .expect("top-level autoInstall should surface the deprecation popup");
    assert_eq!(method, "window/showMessage");
    assert_eq!(
        params["type"].as_i64(),
        Some(2),
        "deprecation notice should be MessageType::WARNING"
    );
    client
        .wait_for_notification_where(&["window/logMessage"], TIMEOUT, is_config_updated)
        .expect("first top-level reconfig should log a config-updated message");

    // Still carrying it → the guard has latched, so no second popup precedes
    // the config-updated log.
    client.send_notification(
        "workspace/didChangeConfiguration",
        wrapped_didchange_config(false),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage", "window/logMessage"], TIMEOUT, |p| {
            is_auto_install_deprecation_notice(p) || is_config_updated(p)
        })
        .expect("second reconfig should log a config-updated message");
    assert_eq!(
        method, "window/logMessage",
        "no second autoInstall deprecation popup should fire; got: {params:?}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_auto_install_deprecation_claimed_at_initialize_suppresses_the_didchange_notice() {
    // The other autoInstall test starts from a CLEAN initialize, so it proves
    // only the didChangeConfiguration half. This one starts from a config FILE
    // carrying the deprecated key, exercising initialize-time DETECTION of it.
    let config_dir = tempfile::TempDir::new().expect("temp config dir");
    let config_path = config_dir.path().join("kakehashi.toml");
    std::fs::write(&config_path, "autoInstall = false\n").expect("write config");

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

    // The initialize-time popup is not observable by this client (it is emitted
    // inside the `initialize` request, before its response — see the module
    // doc), so prove it by its EFFECT: it consumed the session's only slot.
    // Pushing the same deprecated key now yields the config-updated log with no
    // popup ahead of it. Scope, precisely: this pins that initialize DETECTED
    // the file's key and claimed the slot — delete the whole `if` block at
    // `lifecycle.rs` and the popup appears here (verified). It does NOT pin the
    // `show_warning` call: dropping only that line leaves the claim, and this
    // test still passes, because `initialize`'s own notifications are discarded
    // while the client awaits the response. The sibling test pins the popup
    // itself, on the didChange path.
    client.send_notification(
        "workspace/didChangeConfiguration",
        wrapped_didchange_config(false),
    );
    let (method, params) = client
        .wait_for_notification_where(&["window/showMessage", "window/logMessage"], TIMEOUT, |p| {
            is_auto_install_deprecation_notice(p) || is_config_updated(p)
        })
        .expect("reconfig should log a config-updated message");
    assert_eq!(
        method, "window/logMessage",
        "initialize must have claimed the session's only slot; got: {params:?}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
