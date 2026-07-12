//! End-to-end tests for --config-file CLI flag.
//!
//! Verifies that explicit config file paths override default config
//! locations, merge in order, and interact correctly with initializationOptions.
//!
//! Run with: `cargo test --test e2e_config_file --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};
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

fn is_config_updated(params: &Value) -> bool {
    params["message"]
        .as_str()
        .is_some_and(|m| m.contains("Configuration updated"))
}

fn contains_unknown_config_key_warning(params: &Value, key: &str) -> bool {
    params["message"].as_str().is_some_and(|m| {
        params["type"].as_i64() == Some(2)
            && m.contains("unknown")
            && m.contains(&format!("`{key}`"))
            && m.contains("workspace/didChangeConfiguration")
    })
}

fn is_unknown_config_key_warning(params: &Value) -> bool {
    contains_unknown_config_key_warning(params, "autoInstal")
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

/// --config-file with non-existent path fails initialization instead of silently
/// falling back to defaults.
#[test]
fn test_config_file_nonexistent_fails_initialization() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("missing.toml");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );

    let error = response
        .get("error")
        .expect("missing explicit config file should reject initialize");
    assert_eq!(error["code"], json!(-32602));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("Config file not found")),
        "initialize error should identify the missing config file: {error}"
    );
}

#[test]
fn test_config_file_invalid_toml_fails_initialization() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("invalid.toml");
    std::fs::write(&config_path, "searchPaths = [\"/unterminated\"\n").unwrap();
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );

    let error = response
        .get("error")
        .expect("invalid explicit config file should reject initialize");
    assert_eq!(error["code"], json!(-32602));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("Failed to parse")),
        "initialize error should identify the parse failure: {error}"
    );
}

#[test]
fn test_config_file_invalid_path_expansion_fails_initialization() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("invalid-path.toml");
    std::fs::write(
        &config_path,
        "searchPaths = [\"$KAKEHASHI_TEST_UNDEFINED/path\"]\n",
    )
    .unwrap();
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .env_remove("KAKEHASHI_TEST_UNDEFINED")
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );

    let error = response
        .get("error")
        .expect("invalid explicit path expansion should reject initialize");
    assert_eq!(error["code"], json!(-32602));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("Path expansion failed")),
        "initialize error should identify the expansion failure: {error}"
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

/// didChangeConfiguration accepts section-wrapped kakehashi settings.
#[test]
fn test_did_change_configuration_accepts_kakehashi_section() {
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
        json!({ "settings": { "kakehashi": { "autoInstall": true } } }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["autoInstall"] == json!(true),
        "didChangeConfiguration should accept kakehashi section-wrapped settings",
    );
    assert_eq!(settings["autoInstall"], json!(true));
}

/// didChangeConfiguration warns on unknown keys instead of reporting success.
#[test]
fn test_did_change_configuration_warns_on_unknown_keys() {
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
        json!({ "settings": { "autoInstal": true } }),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            is_unknown_config_key_warning(params) || is_config_updated(params)
        })
        .expect("didChangeConfiguration should log a warning or success");
    assert_eq!(method, "window/logMessage");
    assert!(
        is_unknown_config_key_warning(&params),
        "unknown key should warn instead of reporting success; got: {params:?}"
    );
    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "unknown-key-only update must not also log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["autoInstall"],
        json!(false),
        "unknown-key-only update should leave settings unchanged"
    );
}

/// didChangeConfiguration warns and does not apply mixed section/root payloads.
#[test]
fn test_did_change_configuration_warns_on_section_sibling_keys() {
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
        json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": true
                },
                "autoInstal": false
            }
        }),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            is_unknown_config_key_warning(params) || is_config_updated(params)
        })
        .expect("didChangeConfiguration should log a section-sibling warning or success");
    assert_eq!(method, "window/logMessage");
    assert!(
        is_unknown_config_key_warning(&params),
        "section sibling key should warn instead of reporting success; got: {params:?}"
    );
    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "section-sibling update must not also log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["autoInstall"],
        json!(false),
        "section-sibling update should leave settings unchanged"
    );
}

/// didChangeConfiguration reports valid kakehashi sibling keys as mixed format.
#[test]
fn test_did_change_configuration_warns_on_valid_section_sibling_keys() {
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
        json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": true
                },
                "autoInstall": false
            }
        }),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            contains_unknown_config_key_warning(params, "autoInstall") || is_config_updated(params)
        })
        .expect("didChangeConfiguration should log a mixed-format warning or success");
    assert_eq!(method, "window/logMessage");
    assert!(
        contains_unknown_config_key_warning(&params, "autoInstall"),
        "mixed-format sibling key should warn instead of reporting success; got: {params:?}"
    );
    assert!(
        params["message"]
            .as_str()
            .is_some_and(|message| message.contains("mixed-format")),
        "known sibling key should be described as mixed-format; got: {params:?}"
    );
    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "mixed-format update must not also log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["autoInstall"],
        json!(false),
        "mixed-format update should leave settings unchanged"
    );
}

/// didChangeConfiguration ignores unrelated editor settings beside kakehashi.
#[test]
fn test_did_change_configuration_ignores_unrelated_section_siblings() {
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
        json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": true
                },
                "editor": {
                    "tabSize": 2
                },
                "files": {
                    "trimTrailingWhitespace": true
                }
            }
        }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["autoInstall"] == json!(true),
        "unrelated section siblings should not block kakehashi section updates",
    );
    assert_eq!(settings["autoInstall"], json!(true));
}

/// didChangeConfiguration ignores unrelated flat settings while applying kakehashi keys.
#[test]
fn test_did_change_configuration_filters_unrelated_flat_settings() {
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
        json!({
            "settings": {
                "autoInstall": true,
                "gopls": {
                    "staticcheck": true
                },
                "editor": {
                    "tabSize": 2
                }
            }
        }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["autoInstall"] == json!(true),
        "unrelated flat settings should not block kakehashi updates",
    );
    assert_eq!(settings["autoInstall"], json!(true));
}

/// didChangeConfiguration ignores unrelated flat settings without reporting success.
#[test]
fn test_did_change_configuration_skips_unrelated_flat_settings() {
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
        json!({
            "settings": {
                "gopls": {
                    "staticcheck": true
                },
                "editor": {
                    "tabSize": 2
                }
            }
        }),
    );

    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "unrelated flat settings must not log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["autoInstall"],
        json!(false),
        "unrelated flat settings should leave kakehashi settings unchanged"
    );
}

/// didChangeConfiguration does not report success for empty kakehashi sections.
#[test]
fn test_did_change_configuration_skips_empty_kakehashi_section() {
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
        json!({
            "settings": {
                "kakehashi": {},
                "editor": {
                    "tabSize": 2
                }
            }
        }),
    );

    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "empty kakehashi section must not log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert_eq!(
        settings["autoInstall"],
        json!(false),
        "empty kakehashi section should leave settings unchanged"
    );
}

/// didChangeConfiguration warns and does not apply nested languageServers typos.
#[test]
fn test_did_change_configuration_warns_on_nested_unknown_keys() {
    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    assert!(settings["languageServers"].get("pyright").is_none());

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languageServers": {
                    "pyright": {
                        "cmd": ["pyright-langserver", "--stdio"],
                        "languages": ["python"],
                        "rootMarker": [".git"]
                    }
                }
            }
        }),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            contains_unknown_config_key_warning(params, "languageServers.pyright.rootMarker")
                || is_config_updated(params)
        })
        .expect("didChangeConfiguration should log a nested-key warning or success");
    assert_eq!(method, "window/logMessage");
    assert!(
        contains_unknown_config_key_warning(&params, "languageServers.pyright.rootMarker"),
        "nested unknown key should warn instead of reporting success; got: {params:?}"
    );
    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "nested unknown-key update must not also log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert!(
        settings["languageServers"].get("pyright").is_none(),
        "nested unknown-key update should leave languageServers unchanged"
    );
}

/// didChangeConfiguration warns and does not apply nested languages typos.
#[test]
fn test_did_change_configuration_warns_on_nested_language_unknown_keys() {
    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let settings = get_effective_settings(&mut client);
    assert!(settings["languages"].get("python").is_none());

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "python": {
                        "parseer": "/tmp/parser.so"
                    }
                }
            }
        }),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            contains_unknown_config_key_warning(params, "languages.python.parseer")
                || is_config_updated(params)
        })
        .expect("didChangeConfiguration should log a nested language-key warning or success");
    assert_eq!(method, "window/logMessage");
    assert!(
        contains_unknown_config_key_warning(&params, "languages.python.parseer"),
        "nested language unknown key should warn instead of reporting success; got: {params:?}"
    );
    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "nested language unknown-key update must not also log success; got: {unexpected_success:?}"
    );

    let settings = query_effective_settings(&mut client);
    assert!(
        settings["languages"].get("python").is_none(),
        "nested language unknown-key update should leave languages unchanged"
    );
}

/// didChangeConfiguration warns and does not apply nested captureMappings typos.
#[test]
fn test_did_change_configuration_warns_on_nested_capture_mapping_unknown_keys() {
    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let initial_capture_mappings = get_effective_settings(&mut client)["captureMappings"].clone();

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "captureMappings": {
                    "_": {
                        "highlight": {
                            "variable.builtin": "keyword"
                        }
                    }
                }
            }
        }),
    );

    let (method, params) = client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(5), |params| {
            contains_unknown_config_key_warning(params, "captureMappings._.highlight")
                || is_config_updated(params)
        })
        .expect("didChangeConfiguration should log a nested capture mapping warning or success");
    assert_eq!(method, "window/logMessage");
    assert!(
        contains_unknown_config_key_warning(&params, "captureMappings._.highlight"),
        "nested capture mapping unknown key should warn instead of reporting success; got: {params:?}"
    );
    let unexpected_success = client.wait_for_notification_where(
        &["window/logMessage"],
        Duration::from_millis(250),
        is_config_updated,
    );
    assert!(
        unexpected_success.is_none(),
        "nested capture mapping unknown-key update must not also log success; got: {unexpected_success:?}"
    );

    let updated_capture_mappings = query_effective_settings(&mut client)["captureMappings"].clone();
    assert_eq!(
        updated_capture_mappings, initial_capture_mappings,
        "nested capture mapping unknown-key update should leave captureMappings unchanged",
    );
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

/// didChangeConfiguration preserves explicit overrides even when they match inherited values.
#[test]
fn test_did_change_configuration_preserves_explicit_matching_override() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("base.toml");
    std::fs::write(
        &config,
        r#"
[languages._.bridge.python]
enabled = true

[languages.r.bridge.python]
enabled = true
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
        settings["languages"]["r"]["bridge"]["python"]["enabled"],
        json!(true),
        "precondition: explicit override should remain visible in raw effective settings",
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "_": {
                        "bridge": {
                            "python": {
                                "enabled": false
                            }
                        }
                    }
                }
            }
        }),
    );

    let settings = poll_effective_settings(
        &mut client,
        |s| s["languages"]["_"]["bridge"]["python"]["enabled"] == json!(false),
        "didChangeConfiguration should update wildcard bridge enabled",
    );
    assert_eq!(
        settings["languages"]["r"]["bridge"]["python"]["enabled"],
        json!(true),
        "explicit override should remain after wildcard updates",
    );
}
