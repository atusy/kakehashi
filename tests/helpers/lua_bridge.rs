//! Lua bridge helpers for E2E tests.
//!
//! Provides reusable initialization and verification patterns for tests that
//! interact with lua-language-server via the kakehashi bridge.

// These functions are shared across multiple test binaries but not all tests use every function.
// Allow dead_code to suppress per-binary warnings.
#![allow(dead_code)]

use super::lsp_client::LspClient;
use serde_json::json;

/// Check if lua-language-server is available and working.
///
/// # Returns
/// `true` if `lua-language-server --version` runs successfully, `false` otherwise.
pub fn is_lua_ls_available() -> bool {
    std::process::Command::new("lua-language-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Print skip message and return early guard for tests requiring lua-ls.
///
/// # Returns
/// `true` if lua-ls is not available or not working (test should skip), `false` if available (test can run).
///
/// # Example
/// ```
/// if skip_if_lua_ls_unavailable() {
///     return;
/// }
/// // ... test code that requires lua-ls
/// ```
pub fn skip_if_lua_ls_unavailable() -> bool {
    if !is_lua_ls_available() {
        eprintln!("SKIP: lua-language-server is not available or failed to run successfully");
        eprintln!("Install lua-language-server to run this test:");
        eprintln!("  brew install lua-language-server");
        true
    } else {
        false
    }
}

/// Create an LspClient initialized with lua-language-server configuration.
///
/// This helper encapsulates the common initialization pattern for Lua bridge tests:
/// - Spawn kakehashi binary with an empty config file for test isolation
/// - Send initialize request with lua-language-server bridge configuration
/// - Send initialized notification to complete handshake
///
/// Test isolation: Uses `--config-file` with an empty temp file to prevent
/// the user's `~/.config/kakehashi/kakehashi.toml` from interfering with tests.
/// For example, a user config with `[languages._.bridge.python]` would create a
/// restrictive bridge filter that blocks lua, causing all bridge tests to fail.
///
/// # Returns
/// A tuple of an initialized `LspClient` and the `TempDir` holding the config.
/// Callers must keep the `TempDir` alive (e.g., `let (_client, _config_dir) = ...`)
/// so the temp directory is not deleted while the server is running.
pub fn create_lua_configured_client() -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = config_dir.path().join("empty.toml");
    std::fs::write(&config_path, "").expect("Failed to write empty config");
    let config_path_str = config_path
        .to_str()
        .expect("temp path should be valid UTF-8")
        .to_string();

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(&config_path_str)
        .build();

    // Initialize handshake with language server configuration
    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "languageServers": {
                    "lua-language-server": {
                        "cmd": ["lua-language-server"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// Perform clean LSP shutdown sequence.
///
/// Sends shutdown request followed by exit notification as required by LSP protocol.
pub fn shutdown_client(client: &mut LspClient) {
    let _shutdown_response = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// Verify hover response contains meaningful content.
///
/// Handles all valid hover content formats per LSP spec:
/// - String content
/// - MarkedString array
/// - MarkupContent object with value field
///
/// # Arguments
/// * `result` - The `result` field from a hover response
///
/// # Returns
/// `true` if the result contains non-empty content, `false` otherwise.
pub fn verify_hover_has_content(result: &serde_json::Value) -> bool {
    if result.is_null() {
        return false;
    }

    let contents = match result.get("contents") {
        Some(c) => c,
        None => return false,
    };

    if contents.is_string() {
        !contents.as_str().unwrap().is_empty()
    } else if contents.is_array() {
        !contents.as_array().unwrap().is_empty()
    } else if contents.is_object() {
        contents
            .get("value")
            .map(|v| !v.as_str().unwrap_or("").is_empty())
            .unwrap_or(false)
    } else {
        false
    }
}
