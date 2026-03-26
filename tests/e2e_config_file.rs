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
use std::time::Duration;
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

/// Query effective settings on an already-initialized client (no init handshake).
///
/// Use this after sending `didChangeConfiguration` on a client that was already initialized
/// via `get_effective_settings` or `get_effective_settings_with_init_options`.
fn query_effective_settings(client: &mut LspClient) -> serde_json::Value {
    let response = client.send_request("kakehashi/internal/effectiveConfiguration", json!({}));
    response
        .get("result")
        .expect("should have result")
        .get("settings")
        .expect("should have settings")
        .clone()
}

/// Poll effective settings until `predicate` returns true, or panic after timeout.
///
/// Use after `didChangeConfiguration` to avoid fixed sleeps. Polls every 100ms for
/// up to 20 attempts (2s total), which is more robust on slow CI machines than a
/// single `sleep(500ms)`.
fn poll_effective_settings(
    client: &mut LspClient,
    predicate: impl Fn(&serde_json::Value) -> bool,
    msg: &str,
) -> serde_json::Value {
    for _ in 0..20 {
        let settings = query_effective_settings(client);
        if predicate(&settings) {
            return settings;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let settings = query_effective_settings(client);
    assert!(predicate(&settings), "{}", msg);
    settings
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

/// --config-file with non-existent path: the file load fails with an error,
/// and the configuration layer is skipped, so effective settings fall back to defaults.
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

// ── Config Merging Tests ──────────────────────────────────────────────

/// Deep merge: languages from two config files combine at the language level.
#[test]
fn test_config_file_deep_merge_languages_from_two_files() {
    let dir = TempDir::new().unwrap();

    let first = dir.path().join("first.toml");
    std::fs::write(
        &first,
        "[languages.python]\nparser = \"/first/python.so\"\n",
    )
    .unwrap();

    let second = dir.path().join("second.toml");
    std::fs::write(
        &second,
        "[languages.python]\nqueries = [{ path = \"/second/highlights.scm\", kind = \"highlights\" }]\n",
    )
    .unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(first.to_str().unwrap())
        .arg("--config-file")
        .arg(second.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    let python = &settings["languages"]["python"];

    assert_eq!(
        python["parser"],
        json!("/first/python.so"),
        "parser from first file should be preserved"
    );
    assert!(
        python["queries"].is_array(),
        "queries from second file should be present"
    );
    assert_eq!(
        python["queries"][0]["path"],
        json!("/second/highlights.scm"),
        "queries path from second file should be present"
    );
}

/// Deep merge: captureMappings from config file + initializationOptions.
#[test]
fn test_config_file_deep_merge_capture_mappings_with_init_options() {
    let dir = TempDir::new().unwrap();

    let config = dir.path().join("capture.toml");
    std::fs::write(
        &config,
        "[captureMappings._.highlights]\n\"variable.builtin\" = \"from.file\"\n",
    )
    .unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings_with_init_options(
        &mut client,
        json!({
            "captureMappings": {
                "_": {
                    "highlights": {
                        "function.builtin": "from.init"
                    }
                }
            }
        }),
    );

    let highlights = &settings["captureMappings"]["_"]["highlights"];

    assert_eq!(
        highlights["variable.builtin"],
        json!("from.file"),
        "variable.builtin from config file should be preserved"
    );
    assert_eq!(
        highlights["function.builtin"],
        json!("from.init"),
        "function.builtin from initializationOptions should be added"
    );
}

/// Config file sets languageServers, initializationOptions overrides a field.
#[test]
fn test_config_file_language_servers_with_init_options_partial_override() {
    let dir = TempDir::new().unwrap();

    let config = dir.path().join("servers.toml");
    std::fs::write(
        &config,
        "[languageServers.myserver]\ncmd = [\"old-cmd\"]\nlanguages = [\"rust\"]\n",
    )
    .unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    // Note: BridgeServerConfig requires that both `cmd` and `languages` be present in the
    // JSON for deserialization (they are not `Option` and have no `#[serde(default)]`).
    // We pass `languages: []` so deserialization succeeds; the merge logic then keeps the
    // config file's non-empty `languages` because the overlay's is empty.
    let settings = get_effective_settings_with_init_options(
        &mut client,
        json!({
            "languageServers": {
                "myserver": {
                    "cmd": ["new-cmd"],
                    "languages": []
                }
            }
        }),
    );

    let myserver = &settings["languageServers"]["myserver"];

    assert_eq!(
        myserver["cmd"],
        json!(["new-cmd"]),
        "cmd should be overridden by initializationOptions"
    );
    assert_eq!(
        myserver["languages"],
        json!(["rust"]),
        "languages should be inherited from config file (overlay was empty)"
    );
}

// ── didChangeConfiguration Tests ──────────────────────────────────────

/// didChangeConfiguration updates autoInstall over config-file value.
#[test]
fn test_did_change_configuration_updates_auto_install() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("base.toml");
    std::fs::write(&config, "autoInstall = false\n").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    assert_eq!(settings["autoInstall"], json!(false), "precondition");

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "autoInstall": true } }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["autoInstall"] == json!(true),
        "didChangeConfiguration should update autoInstall to true",
    );
    assert_eq!(settings["autoInstall"], json!(true));
}

/// didChangeConfiguration preserves config-file settings not mentioned in the update.
#[test]
fn test_did_change_configuration_preserves_config_file_settings() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("base.toml");
    std::fs::write(&config, "searchPaths = [\"/from-file\"]\n").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    assert_eq!(
        settings["searchPaths"],
        json!(["/from-file"]),
        "precondition"
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "autoInstall": false } }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["autoInstall"] == json!(false),
        "didChangeConfiguration should apply autoInstall=false",
    );
    assert_eq!(
        settings["searchPaths"],
        json!(["/from-file"]),
        "searchPaths from config file should be preserved"
    );
}

/// didChangeConfiguration adds languageServers to a config-file that only had languages.
#[test]
fn test_did_change_configuration_adds_language_servers_to_config_file_base() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("languages.toml");
    std::fs::write(
        &config,
        "[languages.python]\nparser = \"/path/to/python.so\"\n",
    )
    .unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    assert!(
        settings["languages"]["python"]["parser"].is_string(),
        "precondition: python language should be present"
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languageServers": {
                    "pyright": {
                        "cmd": ["pyright-langserver", "--stdio"],
                        "languages": ["python"]
                    }
                }
            }
        }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["languageServers"]["pyright"].is_object(),
        "languageServers from didChangeConfiguration should be present",
    );
    assert_eq!(
        settings["languages"]["python"]["parser"],
        json!("/path/to/python.so"),
        "languages from config file should be preserved after didChangeConfiguration"
    );
    assert_eq!(
        settings["languageServers"]["pyright"]["cmd"],
        json!(["pyright-langserver", "--stdio"]),
        "languageServer cmd should match didChangeConfiguration"
    );
}

/// didChangeConfiguration overrides config-file + initializationOptions layering.
#[test]
fn test_did_change_configuration_overrides_config_file_and_init_options() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("base.toml");
    std::fs::write(&config, "autoInstall = false\n").unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    // initializationOptions overrides config file: autoInstall becomes true
    let settings =
        get_effective_settings_with_init_options(&mut client, json!({ "autoInstall": true }));
    assert_eq!(
        settings["autoInstall"],
        json!(true),
        "precondition: initializationOptions should override config file"
    );

    // didChangeConfiguration overrides everything: autoInstall back to false
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "autoInstall": false } }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["autoInstall"] == json!(false),
        "didChangeConfiguration should override both config file and initializationOptions",
    );
    assert_eq!(settings["autoInstall"], json!(false));
}

/// didChangeConfiguration preserves implicit wildcard inheritance for existing languages.
#[test]
fn test_did_change_configuration_updates_wildcard_bridge_defaults_for_existing_languages() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("base.toml");
    std::fs::write(
        &config,
        r#"
[languages._.bridge.python.aggregation._]
priorities = ["pyright"]

[languages.r]
"#,
    )
    .unwrap();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    assert_eq!(
        settings["languages"]["r"]["bridge"],
        json!(null),
        "precondition: inherited wildcard bridge settings should remain implicit in raw effective settings"
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "_": {
                        "bridge": {
                            "python": {
                                "aggregation": {
                                    "_": {
                                        "priorities": ["ruff"]
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| {
            s["languages"]["_"]["bridge"]["python"]["aggregation"]["_"]["priorities"]
                == json!(["ruff"])
        },
        "didChangeConfiguration should update wildcard bridge priorities",
    );
    assert_eq!(
        settings["languages"]["r"]["bridge"],
        json!(null),
        "existing languages with implicit wildcard inheritance should not materialize copied wildcard bridge settings after reload"
    );
}
