//! End-to-end tests for LSP protocol conformance.
//!
//! These tests verify that kakehashi correctly implements the core LSP
//! protocol lifecycle: spawning, initialization handshake, and clean shutdown.
//! They test the server itself, not any specific language features or bridge
//! functionality.
//!
//! Run with: `cargo test --test e2e_lsp_protocol --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;

#[test]
fn test_spawn_binary_starts_process() {
    // Creating an LspClient should spawn the kakehashi binary and
    // establish LSP communication channels without panicking.
    //
    // If the binary fails to start (e.g., missing executable, spawn error),
    // LspClient::new() should surface that as a failure, causing this test
    // to fail.
    let _client = LspClient::new();
}

#[test]
fn test_initialize_returns_capabilities() {
    let mut client = LspClient::new();

    let response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );

    let capabilities = response
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .expect("InitializeResult should have capabilities");

    insta::assert_json_snapshot!("initialize_capabilities", capabilities);
}

#[test]
fn test_shutdown_terminates_cleanly() {
    let mut client = LspClient::new();

    // Initialize handshake
    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Verify server is running
    assert!(
        client.is_running(),
        "Server should be running before shutdown"
    );

    // Send shutdown request (server should acknowledge but stay running)
    // Note: LSP shutdown takes no params
    let shutdown_response = client.send_request("shutdown", json!(null));
    assert!(
        shutdown_response.get("result").is_some(),
        "Shutdown should return a result: {:?}",
        shutdown_response
    );

    // Server should still be running after shutdown (waiting for exit notification)
    assert!(
        client.is_running(),
        "Server should still be running after shutdown, waiting for exit notification"
    );

    // Send exit notification (server should terminate)
    client.send_notification("exit", json!(null));

    // Close stdin to signal EOF - this helps tower-lsp's server to exit
    client.close_stdin();

    // Wait for process to exit (up to 2 seconds)
    let exit_status = client
        .wait_for_exit(std::time::Duration::from_secs(2))
        .expect("Server should have exited after exit notification");

    // Verify clean exit (exit code 0)
    assert!(
        exit_status.success(),
        "Server should exit cleanly with code 0, got: {:?}",
        exit_status
    );
}
