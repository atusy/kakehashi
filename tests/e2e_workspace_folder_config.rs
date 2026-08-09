//! End-to-end tests for the configuration reload driven by
//! `workspace/didChangeWorkspaceFolders`.
//!
//! The handler picks kakehashi's own configuration root from the current folder
//! list, so a folder change re-reads the project `kakehashi.toml` at the new
//! root. These tests pin which root it picks — including when the client removes
//! the last folder, where the root comes from the rungs `initialize` resolved
//! below `workspaceFolders`, and where the ladder deliberately stops before the
//! process working directory.
//!
//! Run with: `cargo test --test e2e_workspace_folder_config --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lsp_polling::poll_until;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

/// A directory holding a project config whose `searchPaths` names it.
fn project_dir(marker: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    write_marker(&dir, marker);
    dir
}

/// Repoint a project config at a new marker, so the only way to observe it is a
/// reload that reads that directory again.
fn write_marker(dir: &TempDir, marker: &str) {
    std::fs::write(
        dir.path().join("kakehashi.toml"),
        format!("searchPaths = [\"/{marker}\"]\n"),
    )
    .unwrap();
}

/// A canonical `file://` URI. Built via `Url::from_file_path` rather than string
/// formatting so the URI is RFC-canonical and percent-encoded — a `TMPDIR`
/// containing a space would otherwise produce something kakehashi's
/// `url::Url::parse` rejects.
fn uri_of(dir: &TempDir) -> String {
    url::Url::from_file_path(dir.path())
        .expect("valid file URI")
        .to_string()
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

/// Poll until `searchPaths` equals `expected`.
///
/// Every caller polls for a value that differs from the one in effect when the
/// notification was sent, so this is a barrier on the reload rather than a
/// sample that can pass before the handler has run.
fn poll_search_paths(client: &mut LspClient, expected: serde_json::Value, msg: &str) {
    let settled = poll_until(20, 100, || {
        let settings = query_effective_settings(client);
        (settings["searchPaths"] == expected).then_some(settings)
    });
    assert!(
        settled.is_some(),
        "{msg}; last seen: {}",
        query_effective_settings(client)["searchPaths"]
    );
}

/// Removing the last workspace folder must not leave the session rootless when
/// the client named another root.
///
/// `initialize` walks `workspaceFolders` → `rootUri` → `rootPath`, and an empty
/// folder list deliberately does not suppress those deprecated fields
/// (`client_root`). Emptying the list through a notification reaches the same
/// state, so the same rungs answer — otherwise the project layer silently
/// disappears and relative paths in the client layers lose their base.
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
        json!(["/from-root-uri"]),
        "removing the last folder should fall back to the rootUri project config",
    );
}

/// The common client shape: `rootUri` names the same directory as the only
/// workspace folder (Neovim and single-folder VS Code both send this).
///
/// Removing that folder restores the root to the directory the client just
/// closed, so its `kakehashi.toml` stays in effect. That follows from the rung
/// order rather than being chosen for this case, and it matches `client_root`'s
/// existing refusal to let an empty folder list suppress `rootUri` — pinned here
/// because this shape is the one real clients produce.
#[test]
fn test_removing_the_last_folder_keeps_a_root_uri_that_names_it() {
    let dir = project_dir("from-shared-root");

    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": uri_of(&dir),
            "workspaceFolders": [folder(&dir, "root")],
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
    assert_eq!(
        query_effective_settings(&mut client)["searchPaths"],
        json!(["/from-shared-root"]),
        "precondition"
    );

    // Repoint the config so the assertion below can only be satisfied by a
    // reload that read this directory again, not by the settings already in
    // effect.
    write_marker(&dir, "from-shared-root-reloaded");
    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [folder(&dir, "root")],
            }
        }),
    );

    poll_search_paths(
        &mut client,
        json!(["/from-shared-root-reloaded"]),
        "a rootUri naming the removed folder keeps that directory as the root",
    );
}

/// A client that named no root besides its folders gets no project layer when
/// the last one goes — never the directory the server was launched from.
///
/// The working directory is `initialize`'s last resort so a session opened with
/// no workspace can still find a configuration. Reaching it from a folder change
/// would migrate an established session to whatever `kakehashi.toml` sits in the
/// launch directory, which may name parser libraries to load.
#[test]
fn test_removing_the_last_folder_without_a_named_root_drops_the_project_layer() {
    let folder_dir = project_dir("from-folder");
    let launch_dir = project_dir("from-launch-dir");

    let mut client = LspClient::builder()
        .current_dir(launch_dir.path())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "workspaceFolders": [folder(&folder_dir, "folder")],
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
    assert_eq!(
        query_effective_settings(&mut client)["searchPaths"],
        json!(["/from-folder"]),
        "precondition: the folder outranks the launch directory"
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

    // Back to the programmed default, which the launch directory's config would
    // have replaced had the ladder reached it.
    poll_search_paths(
        &mut client,
        json!(["${KAKEHASHI_DATA_DIR}"]),
        "an unrooted session must not adopt the launch directory's config",
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

    // Repoint the primary's config before the change. Polling for the value
    // already in effect would return on the first sample — passing even if the
    // notification were dropped — so the reload has to produce a value that did
    // not exist when it was sent.
    write_marker(&primary, "from-primary-reloaded");
    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [],
                "removed": [folder(&secondary, "secondary")],
            }
        }),
    );

    poll_search_paths(
        &mut client,
        json!(["/from-primary-reloaded"]),
        "removing a non-primary folder must reload from the unchanged primary root",
    );
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
    assert_eq!(
        query_effective_settings(&mut client)["searchPaths"],
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
        json!(["/from-secondary"]),
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
    let pushed = poll_until(20, 100, || {
        let settings = query_effective_settings(&mut client);
        (settings["diagnosticsDebounceMs"] == json!(222)).then_some(settings)
    });
    assert!(
        pushed.is_some(),
        "precondition: the runtime layer outranks initializationOptions"
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
        json!(["/from-secondary"]),
        "the project layer should follow the new root",
    );
    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["diagnosticsDebounceMs"],
        json!(222),
        "the client layers must survive a project-layer reload: {settings}"
    );
}

/// `--config-file` skips the whole reload transaction, but a folder change
/// must still publish the new root: it is what anchors a later relative-path
/// push. This branch has no settings-level effect of its own to poll for —
/// unlike the general path, it never reloads — so it can't be proven via
/// `poll_search_paths` the way the tests above are. The barrier here is the
/// forced `workspace/diagnostic/refresh` request instead: it is sent only
/// after `set_root_path`, in the same synchronous function body, so seeing it
/// arrive is proof the root already moved before the push below reads it.
#[test]
fn test_config_file_folder_change_anchors_a_later_push_to_the_new_root() {
    let config_dir = TempDir::new().unwrap();
    let config_path = config_dir.path().join("override.toml");
    std::fs::write(&config_path, "autoInstall = false\n").unwrap();

    let folder_a = TempDir::new().unwrap();
    let folder_b = TempDir::new().unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "workspaceFolders": [folder(&folder_a, "a")],
            "capabilities": {
                "workspace": { "diagnostics": { "refreshSupport": true } }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({
            "event": {
                "added": [folder(&folder_b, "b")],
                "removed": [folder(&folder_a, "a")],
            }
        }),
    );

    // Proves `set_root_path(folder_b)` already ran: the config-file branch
    // sends this request only after that call, in the same function body.
    let (refresh_id, _) = client
        .wait_for_server_request("workspace/diagnostic/refresh", Duration::from_secs(15))
        .expect("a folder change under --config-file must still nudge a pull-mode editor");
    client.send_response(refresh_id, json!(null));

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "kakehashi": { "searchPaths": ["./libs"] } } }),
    );

    let expected = folder_b.path().join("libs");
    let settled = poll_until(20, 100, || {
        let settings = query_effective_settings(&mut client);
        (settings["searchPaths"] == json!([expected.to_str().unwrap()])).then_some(settings)
    });
    assert!(
        settled.is_some(),
        "a relative path pushed after the folder change must anchor to the new root \
         (folder_b), not the session's original root (folder_a) or the launch \
         directory; last seen: {}",
        query_effective_settings(&mut client)["searchPaths"]
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
