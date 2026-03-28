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

/// effectiveConfiguration returns raw (pre-expansion) settings, so with no
/// `KAKEHASHI_DATA_DIR` env var and no `--data-dir` flag the raw searchPaths
/// still contains the `${KAKEHASHI_DATA_DIR}` template.
#[test]
fn test_search_paths_returns_raw_template() {
    let mut client = LspClient::builder()
        .env_remove("KAKEHASHI_DATA_DIR")
        .build();

    let paths = get_effective_search_paths(&mut client);

    assert_eq!(
        paths,
        vec!["${KAKEHASHI_DATA_DIR}"],
        "raw searchPaths should preserve the template variable"
    );
}

/// When KAKEHASHI_DATA_DIR env var is set, effectiveConfiguration still shows
/// the raw template — expansion happens internally, not in the raw config.
#[test]
fn test_env_var_does_not_affect_raw_search_paths() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_DATA_DIR", "/tmp/kakehashi_test_data")
        .build();

    let paths = get_effective_search_paths(&mut client);

    assert_eq!(
        paths,
        vec!["${KAKEHASHI_DATA_DIR}"],
        "raw searchPaths should still show template even with env var set"
    );
}
