//! End-to-end tests for the configuration reload driven by
//! `workspace/didChangeWorkspaceFolders`.
//!
//! The handler picks kakehashi's own configuration root from the current folder
//! list, so a folder change re-reads the project `kakehashi.toml` at the new
//! root. These tests pin which root it picks — including when the client removes
//! the last folder, where the root has to come from the same ladder
//! `initialize` uses.
//!
//! Run with: `cargo test --test e2e_workspace_folder_config --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

/// A directory holding a project config whose `searchPaths` names it.
fn project_dir(marker: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("kakehashi.toml"),
        format!("searchPaths = [\"/{marker}\"]\n"),
    )
    .unwrap();
    dir
}

fn uri_of(dir: &TempDir) -> String {
    format!("file://{}", dir.path().display())
}

fn folder(dir: &TempDir, name: &str) -> serde_json::Value {
    json!({ "uri": uri_of(dir), "name": name })
}

fn query_effective_settings(client: &mut LspClient) -> serde_json::Value {
    client
        .send_request("kakehashi/internal/effectiveConfiguration", json!({}))
        .get("result")
        .expect("should have result")
        .get("settings")
        .expect("should have settings")
        .clone()
}

/// Poll until `searchPaths` matches `expected`, then return the settings.
fn poll_search_paths(client: &mut LspClient, expected: &str, msg: &str) -> serde_json::Value {
    for _ in 0..20 {
        let settings = query_effective_settings(client);
        if settings["searchPaths"] == json!([expected]) {
            return settings;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let settings = query_effective_settings(client);
    assert_eq!(
        settings["searchPaths"],
        json!([expected]),
        "{msg}: {settings}"
    );
    settings
}

/// Removing the last workspace folder must not leave the session rootless.
///
/// `initialize` walks `workspaceFolders` → `rootUri` → `rootPath` → process CWD
/// to choose the configuration root. Emptying the folder list puts the session
/// in exactly the state a client that never sent a folder starts in, so the same
/// ladder has to answer — otherwise the project layer silently disappears and
/// relative paths in the client layers lose the base they were anchored to.
#[test]
fn test_removing_the_last_folder_falls_back_to_root_uri() {
    let folder_dir = project_dir("from-folder");
    let root_uri_dir = project_dir("from-root-uri");

    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": uri_of(&root_uri_dir),
            "workspaceFolders": [folder(&folder_dir, "folder")],
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["searchPaths"],
        json!(["/from-folder"]),
        "precondition: the first workspace folder outranks rootUri: {settings}"
    );

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [folder(&folder_dir, "folder")],
            }
        }),
    );

    poll_search_paths(
        &mut client,
        "/from-root-uri",
        "removing the last folder should fall back to the rootUri project config",
    );
}

/// The primary root follows the client's folder order, not the change event:
/// removing a folder that is not first leaves the configuration root alone.
#[test]
fn test_removing_a_non_primary_folder_keeps_the_root() {
    let primary = project_dir("from-primary");
    let secondary = project_dir("from-secondary");

    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "workspaceFolders": [
                folder(&primary, "primary"),
                folder(&secondary, "secondary"),
            ],
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["searchPaths"],
        json!(["/from-primary"]),
        "precondition: the first folder is the configuration root: {settings}"
    );

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [folder(&secondary, "secondary")],
            }
        }),
    );

    // Nothing should change, so poll for the *stable* value rather than a
    // transition: a wrong root would show up as `/from-secondary` or the
    // programmed defaults.
    let settings = poll_search_paths(
        &mut client,
        "/from-primary",
        "removing a non-primary folder must not move the configuration root",
    );
    assert_eq!(settings["searchPaths"], json!(["/from-primary"]));
}

/// Removing the primary folder promotes the next one in client order.
#[test]
fn test_removing_the_primary_folder_promotes_the_next() {
    let primary = project_dir("from-primary");
    let secondary = project_dir("from-secondary");

    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "workspaceFolders": [
                folder(&primary, "primary"),
                folder(&secondary, "secondary"),
            ],
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["searchPaths"],
        json!(["/from-primary"]),
        "precondition"
    );

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [folder(&primary, "primary")],
            }
        }),
    );

    poll_search_paths(
        &mut client,
        "/from-secondary",
        "removing the primary folder should promote the next one",
    );
}

/// A folder change replaces the project-file layer only. The layers the client
/// supplied — `initializationOptions` and everything pushed since — outrank it
/// and must survive the reload.
#[test]
fn test_folder_change_preserves_client_layers() {
    let primary = project_dir("from-primary");
    let secondary = project_dir("from-secondary");

    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "workspaceFolders": [
                folder(&primary, "primary"),
                folder(&secondary, "secondary"),
            ],
            "initializationOptions": { "diagnosticsDebounceMs": 111 },
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "kakehashi": { "diagnosticsDebounceMs": 222 } } }),
    );
    for _ in 0..20 {
        if query_effective_settings(&mut client)["diagnosticsDebounceMs"] == json!(222) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["diagnosticsDebounceMs"],
        json!(222),
        "precondition: the runtime layer outranks initializationOptions: {settings}"
    );

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [folder(&primary, "primary")],
            }
        }),
    );

    let settings = poll_search_paths(
        &mut client,
        "/from-secondary",
        "the project layer should follow the new root",
    );
    assert_eq!(
        settings["diagnosticsDebounceMs"],
        json!(222),
        "the client layers must survive a project-layer reload: {settings}"
    );
}
