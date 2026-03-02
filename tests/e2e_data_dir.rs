//! End-to-end tests for --data-dir and KAKEHASHI_DATA_DIR resolution.
//!
//! Verifies that the effective `searchPaths` in the LSP configuration
//! correctly reflects the --data-dir CLI flag and KAKEHASHI_DATA_DIR
//! environment variable, including their interaction and precedence.
//!
//! Run with: `cargo test --test e2e_data_dir --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;

/// Helper: initialize the LSP client and return the effective searchPaths.
fn get_effective_search_paths(client: &mut LspClient) -> Vec<String> {
    let _init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    let response = client.send_request("kakehashi/internal/effectiveConfiguration", json!({}));

    let result = response
        .get("result")
        .expect("effectiveConfiguration should return a result");
    let settings = result
        .get("settings")
        .expect("result should contain settings");
    let search_paths = settings
        .get("searchPaths")
        .expect("settings should contain searchPaths");

    search_paths
        .as_array()
        .expect("searchPaths should be an array")
        .iter()
        .map(|v| {
            v.as_str()
                .expect("searchPath should be a string")
                .to_string()
        })
        .collect()
}

/// When only --data-dir is set (no env var), searchPaths should resolve to the flag value.
#[test]
fn test_search_paths_with_data_dir_flag_only() {
    let mut client = LspClient::builder()
        .arg("--data-dir")
        .arg("/tmp/e2e-flag-only")
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let paths = get_effective_search_paths(&mut client);

    assert_eq!(
        paths,
        vec!["/tmp/e2e-flag-only"],
        "searchPaths should resolve to --data-dir value"
    );
}

/// When only KAKEHASHI_DATA_DIR env var is set (no --data-dir), searchPaths should resolve to env value.
#[test]
fn test_search_paths_with_env_var_only() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_DATA_DIR", "/tmp/e2e-env-only")
        .build();

    let paths = get_effective_search_paths(&mut client);

    assert_eq!(
        paths,
        vec!["/tmp/e2e-env-only"],
        "searchPaths should resolve to KAKEHASHI_DATA_DIR env var value"
    );
}

/// When both --data-dir flag and KAKEHASHI_DATA_DIR env var are set,
/// the flag should win (it sets env var in-process, overriding the inherited one).
#[test]
fn test_search_paths_flag_overrides_env_var() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_DATA_DIR", "/tmp/e2e-env-value")
        .arg("--data-dir")
        .arg("/tmp/e2e-flag-value")
        .build();

    let paths = get_effective_search_paths(&mut client);

    assert_eq!(
        paths,
        vec!["/tmp/e2e-flag-value"],
        "--data-dir flag should override KAKEHASHI_DATA_DIR env var"
    );
}

/// When neither --data-dir nor KAKEHASHI_DATA_DIR is set,
/// searchPaths should fall back to the platform default containing "kakehashi".
#[test]
fn test_search_paths_falls_back_to_platform_default() {
    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let paths = get_effective_search_paths(&mut client);

    assert_eq!(
        paths.len(),
        1,
        "should have exactly one default search path"
    );
    assert!(
        paths[0].ends_with("kakehashi"),
        "default search path should end with 'kakehashi', got: {}",
        paths[0]
    );
    // Verify it's NOT some test path
    assert!(
        !paths[0].starts_with("/tmp/e2e"),
        "should not be a test path, got: {}",
        paths[0]
    );
}
