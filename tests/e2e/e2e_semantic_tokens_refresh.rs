//! E2E tests for semantic tokens refresh capability checking.
//!
//! Verifies LSP @since 3.16.0 compliance: server only sends workspace/semanticTokens/refresh
//! when client declares refreshSupport: true.
//!
//! NOTE: The SemanticTokensRefresh event is only emitted during DYNAMIC language loading
//! (first time a language is loaded). This means these tests may not trigger a refresh
//! if the language is already loaded. The core capability checking logic is tested
//! thoroughly via unit tests in lsp_impl.rs (7 test cases).
//!
//! NOTE: Tests that verify ABSENCE of refresh requests require non-blocking I/O with
//! true timeout support. The current LspClient uses blocking BufReader which cannot
//! properly timeout. These tests are marked #[ignore] until async I/O is implemented.
//!
//! Run with: `cargo test --features e2e --test e2e e2e_semantic_tokens_refresh::`
//! Run ignored tests: `cargo test --features e2e --test e2e e2e_semantic_tokens_refresh:: -- --ignored`

use crate::helpers::lsp_client::LspClient;
use serde_json::json;

/// Initialize with specific capabilities for testing.
fn initialize_with_capabilities(client: &mut LspClient, capabilities: serde_json::Value) {
    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": capabilities,
        }),
    );
    client.send_notification("initialized", json!({}));
}

/// Test that server works correctly when capability IS supported.
///
/// This test verifies that the server initializes and operates correctly when the
/// refresh capability is declared. It doesn't verify that a refresh is actually sent,
/// because the SemanticTokensRefresh event is only emitted during dynamic language loading.
///
/// The core capability checking logic is tested via unit tests.
#[test]
fn test_server_operates_with_refresh_capability_enabled() {
    let mut client = LspClient::new();

    initialize_with_capabilities(
        &mut client,
        json!({
            "workspace": {
                "semanticTokens": {
                    "refreshSupport": true
                }
            }
        }),
    );

    // Open a file
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///test.lua",
                "languageId": "lua",
                "version": 1,
                "text": "local x = 1"
            }
        }),
    );

    // Verify server is operational
    let hover = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///test.lua" },
            "position": { "line": 0, "character": 0 }
        }),
    );

    // Server should respond without error (result may be null for hover)
    assert!(
        hover.get("error").is_none(),
        "Server should respond without error when refresh capability is enabled"
    );
}

/// Test that server works correctly when capability is NOT supported.
///
/// This test verifies that the server initializes and operates correctly when the
/// refresh capability is NOT declared.
#[test]
fn test_server_operates_with_refresh_capability_disabled() {
    let mut client = LspClient::new();

    initialize_with_capabilities(
        &mut client,
        json!({
            "workspace": {
                "semanticTokens": {
                    "refreshSupport": false
                }
            }
        }),
    );

    // Open a file
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///test.lua",
                "languageId": "lua",
                "version": 1,
                "text": "local x = 1"
            }
        }),
    );

    // Verify server is operational
    let hover = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///test.lua" },
            "position": { "line": 0, "character": 0 }
        }),
    );

    // Server should respond without error (result may be null for hover)
    assert!(
        hover.get("error").is_none(),
        "Server should respond without error when refresh capability is disabled"
    );
}

/// Test that server works correctly with empty capabilities.
#[test]
fn test_server_operates_with_empty_capabilities() {
    let mut client = LspClient::new();

    // Empty capabilities - no workspace.semanticTokens at all
    initialize_with_capabilities(&mut client, json!({}));

    // Open a file
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///test.lua",
                "languageId": "lua",
                "version": 1,
                "text": "local x = 1"
            }
        }),
    );

    // Verify server is operational
    let hover = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": "file:///test.lua" },
            "position": { "line": 0, "character": 0 }
        }),
    );

    // Server should respond without error (result may be null for hover)
    assert!(
        hover.get("error").is_none(),
        "Server should respond without error with empty capabilities"
    );
}

// NOTE: The following tests verify ABSENCE of refresh requests.
// They require non-blocking I/O with true timeout support.
// Currently blocked on BufReader's blocking read_line().
// Uncomment when async I/O is implemented in LspClient.

/*
#[test]
fn test_refresh_not_sent_when_capability_false() {
    // Test that server does NOT send refresh when capability is false
    // Requires timeout-based receive to verify absence of message
}

#[test]
fn test_refresh_not_sent_when_capability_missing() {
    // Test that server does NOT send refresh when capability is missing
}

#[test]
fn test_refresh_not_sent_when_workspace_null() {
    // Test that server does NOT send refresh when workspace is null
}

#[test]
fn test_refresh_sent_when_capability_true() {
    // Test that server DOES send refresh when capability is true
    // Requires triggering dynamic language loading
}
*/
