//! End-to-end tests for --config-file CLI flag.
//!
//! Verifies that explicit config file paths override default config
//! locations, merge in order, and interact correctly with initializationOptions.
//!
//! Run with: `cargo test --test e2e_config_file --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;
use tempfile::TempDir;

/// Initialize LSP client and return the effective configuration settings.
fn get_effective_settings(client: &mut LspClient) -> serde_json::Value {
    get_effective_settings_with_init_options(client, json!(null))
}

/// Initialize LSP client with custom initializationOptions and return effective settings.
fn get_effective_settings_with_init_options(
    client: &mut LspClient,
    init_options: serde_json::Value,
) -> serde_json::Value {
    let mut params = json!({
        "processId": std::process::id(),
        "rootUri": null,
        "capabilities": {}
    });
    if !init_options.is_null() {
        params["initializationOptions"] = init_options;
    }
    let _init = client.send_request("initialize", params);
    client.send_notification("initialized", json!({}));

    let response = client.send_request("kakehashi/internal/effectiveConfiguration", json!({}));
    response
        .get("result")
        .expect("should have result")
        .get("settings")
        .expect("should have settings")
        .clone()
}

/// --config-file with a single valid file overrides default config locations.
#[test]
fn test_config_file_single_file_overrides_defaults() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("custom.toml");
    std::fs::write(&config_path, "autoInstall = false\n").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);

    assert_eq!(
        settings["autoInstall"],
        json!(false),
        "autoInstall should be overridden by the config file"
    );
}

/// --config-file with two files merges in order (later wins).
#[test]
fn test_config_file_two_files_merge_in_order() {
    let dir = TempDir::new().unwrap();

    let first = dir.path().join("first.toml");
    std::fs::write(&first, "autoInstall = false\nsearchPaths = [\"/first\"]\n").unwrap();

    let second = dir.path().join("second.toml");
    std::fs::write(&second, "autoInstall = true\n").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(first.to_str().unwrap())
        .arg("--config-file")
        .arg(second.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);

    // autoInstall: second file (true) should override first file (false)
    assert_eq!(
        settings["autoInstall"],
        json!(true),
        "second config file should override first"
    );

    // searchPaths: first file sets it, second doesn't override → inherited from first
    assert_eq!(
        settings["searchPaths"],
        json!(["/first"]),
        "searchPaths from first file should be inherited"
    );
}

/// --config-file with non-existent path: missing file is silently skipped in the
/// merge (returns None), so effective settings fall back to programmed defaults.
#[test]
fn test_config_file_nonexistent_falls_back_to_defaults() {
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg("/nonexistent/kakehashi-test-config.toml")
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);

    // The missing file produces None in the merge, so only defaults apply
    assert_eq!(
        settings["autoInstall"],
        json!(true),
        "missing config file should fall back to default autoInstall=true"
    );
}

/// --config-file with an empty temp file gives defaults only (test isolation pattern).
#[test]
fn test_config_file_empty_file_gives_defaults() {
    let dir = TempDir::new().unwrap();
    let empty_config = dir.path().join("empty.toml");
    std::fs::write(&empty_config, "").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(empty_config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);

    // With an empty config file, only programmed defaults apply
    assert_eq!(
        settings["autoInstall"],
        json!(true),
        "empty config file should leave autoInstall at default (true)"
    );
}

/// --config-file + initializationOptions: initializationOptions still wins.
#[test]
fn test_config_file_with_init_options_override() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("base.toml");
    std::fs::write(&config_path, "autoInstall = false\n").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings =
        get_effective_settings_with_init_options(&mut client, json!({ "autoInstall": true }));

    assert_eq!(
        settings["autoInstall"],
        json!(true),
        "initializationOptions should override --config-file"
    );
}
