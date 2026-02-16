//! Shared test utilities for pool and bridge module tests.
//!
//! This module provides common test helpers used across pool and bridge submodule tests.
//! Helpers avoid duplication and ensure consistent test setup.
//!
//! Import from submodule tests via `use crate::lsp::bridge::pool::test_helpers::*;`

use std::sync::Arc;

use url::Url;

// Re-export types needed by tests
pub(in crate::lsp::bridge) use crate::config::settings::BridgeServerConfig;

use crate::lsp::bridge::actor::{ResponseRouter, spawn_reader_task};
use crate::lsp::bridge::connection::AsyncBridgeConnection;
use crate::lsp::bridge::pool::{ConnectionHandle, ConnectionState};

// Test ULID constants - valid 26-char alphanumeric strings matching ULID format.
// Using realistic ULIDs ensures tests reflect actual runtime behavior.
pub(in crate::lsp::bridge) const TEST_ULID_LUA_0: &str = "01JPMQ8ZYYQA1W3AVPW4JDRZFR";
pub(in crate::lsp::bridge) const TEST_ULID_LUA_1: &str = "01JPMQ8ZYYQA1W3AVPW4JDRZFS";
pub(in crate::lsp::bridge) const TEST_ULID_PYTHON_0: &str = "01JPMQ8ZYYQA1W3AVPW4JDRZFT";

/// Check if lua-language-server is available. Returns false and logs skip message if not.
///
/// Use at the beginning of tests that require a real LSP server:
/// ```ignore
/// if !lua_ls_available() { return; }
/// ```
pub(in crate::lsp::bridge) fn lua_ls_available() -> bool {
    if std::process::Command::new("lua-language-server")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("Skipping test: lua-language-server not found");
        false
    } else {
        true
    }
}

/// Create a BridgeServerConfig for lua-language-server.
pub(in crate::lsp::bridge) fn lua_ls_config() -> BridgeServerConfig {
    BridgeServerConfig {
        cmd: vec!["lua-language-server".to_string()],
        languages: vec!["lua".to_string()],
        initialization_options: None,
        workspace_type: None,
    }
}

/// Create a BridgeServerConfig for a mock server that discards input.
/// Useful for testing timeout behavior or when no response is expected.
pub(in crate::lsp::bridge) fn devnull_config() -> BridgeServerConfig {
    devnull_config_for_language("lua")
}

/// Create a BridgeServerConfig for a mock server with a specific language.
pub(in crate::lsp::bridge) fn devnull_config_for_language(language: &str) -> BridgeServerConfig {
    BridgeServerConfig {
        cmd: vec![
            "sh".to_string(),
            "-c".to_string(),
            "cat > /dev/null".to_string(),
        ],
        languages: vec![language.to_string()],
        initialization_options: None,
        workspace_type: None,
    }
}

/// Helper function to convert url::Url to tower_lsp_server::ls_types::Uri for tests.
pub(in crate::lsp::bridge) fn url_to_uri(url: &Url) -> tower_lsp_server::ls_types::Uri {
    crate::lsp::lsp_impl::url_to_uri(url).expect("test URL should convert to URI")
}

/// Create a test host URI with the given name.
pub(in crate::lsp::bridge) fn test_host_uri(name: &str) -> Url {
    Url::parse(&format!("file:///test/{}.md", name)).unwrap()
}

/// Create a ConnectionHandle with a specific initial state for testing.
///
/// Spawns a `cat > /dev/null` sink process to get a working connection,
/// then sets the desired state. Uses a sink (not echo) because echo servers
/// return raw requests which include both `"id"` and `"method"` fields —
/// these are correctly classified as ServerRequest rather than Response,
/// causing shutdown handshakes to hang.
pub async fn create_handle_with_state(state: ConnectionState) -> Arc<ConnectionHandle> {
    // Create a mock server process (sink — discards all input, no output)
    let mut conn = AsyncBridgeConnection::spawn(vec![
        "sh".to_string(),
        "-c".to_string(),
        "cat > /dev/null".to_string(),
    ])
    .await
    .expect("should spawn sink process");

    // Split connection and spawn reader task (new architecture)
    let (writer, reader) = conn.split();
    let router = Arc::new(ResponseRouter::new());
    let reader_handle = spawn_reader_task(reader, Arc::clone(&router));

    let handle = Arc::new(ConnectionHandle::new(writer, router, reader_handle));
    handle.set_state(state);
    handle
}
