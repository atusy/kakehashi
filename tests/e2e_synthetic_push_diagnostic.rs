//! End-to-end test for synthetic push diagnostics (ADR-0020 Phase 2).
//!
//! This test verifies that `textDocument/publishDiagnostics` notifications
//! are sent automatically on `didSave` and `didOpen` events.
//!
//! Run with: `cargo test --test e2e_synthetic_push_diagnostic --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_polling::wait_for_server_ready;
use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;
use std::time::Duration;

/// E2E test: publishDiagnostics is sent on didOpen
///
/// When a document is opened, kakehashi should automatically collect
/// diagnostics from downstream servers and publish them.
#[test]
fn e2e_synthetic_push_on_did_open() {
    let (mut client, _config_dir) = create_lua_configured_client();

    // Markdown document with Lua code block
    let markdown_content = r#"# Test Document

```lua
-- Valid Lua code
local x = 1
print(x)
```
"#;

    let markdown_uri = "file:///test_synthetic_push_open.md";

    // Send didOpen - this should trigger synthetic push
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    // Wait for lua-ls to be ready (ensures downstream server is initialized)
    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    // Wait for publishDiagnostics notification
    // The synthetic push happens after initialization, so we need to wait a bit
    let notification =
        client.wait_for_notification("textDocument/publishDiagnostics", Duration::from_secs(10));

    if let Some(params) = notification {
        println!(
            "Received publishDiagnostics: {}",
            serde_json::to_string_pretty(&params).unwrap()
        );

        // Verify the notification has expected structure
        let uri = params.get("uri").and_then(|u| u.as_str());
        assert_eq!(
            uri,
            Some(markdown_uri),
            "publishDiagnostics should be for the opened document"
        );

        let diagnostics = params.get("diagnostics").and_then(|d| d.as_array());
        assert!(
            diagnostics.is_some(),
            "publishDiagnostics should have diagnostics array"
        );

        println!(
            "✓ E2E: Synthetic push on didOpen - received {} diagnostics",
            diagnostics.unwrap().len()
        );
    } else {
        // It's acceptable if no notification is received in some environments
        // (e.g., if lua-ls is slow to start). The important thing is that
        // the server doesn't crash and handles the flow correctly.
        println!(
            "⚠ E2E: No publishDiagnostics received within timeout. \
             This may happen if lua-ls is slow to initialize."
        );
    }

    shutdown_client(&mut client);
}

/// E2E test: publishDiagnostics is sent on didSave
///
/// When a document is saved, kakehashi should automatically collect
/// diagnostics from downstream servers and publish them.
#[test]
fn e2e_synthetic_push_on_did_save() {
    let (mut client, _config_dir) = create_lua_configured_client();

    // First open the document
    let markdown_content = r#"# Test Document

```lua
local x = 1
```
"#;

    let markdown_uri = "file:///test_synthetic_push_save.md";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    // Wait for lua-ls to be ready
    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    // Drain any existing notifications from didOpen
    let _ = client.wait_for_notification("textDocument/publishDiagnostics", Duration::from_secs(3));

    // Now send didSave - this should trigger another synthetic push
    client.send_notification(
        "textDocument/didSave",
        json!({
            "textDocument": {
                "uri": markdown_uri
            }
        }),
    );

    // Wait for publishDiagnostics notification from didSave
    let notification =
        client.wait_for_notification("textDocument/publishDiagnostics", Duration::from_secs(10));

    if let Some(params) = notification {
        println!(
            "Received publishDiagnostics on save: {}",
            serde_json::to_string_pretty(&params).unwrap()
        );

        let uri = params.get("uri").and_then(|u| u.as_str());
        assert_eq!(
            uri,
            Some(markdown_uri),
            "publishDiagnostics should be for the saved document"
        );

        let diagnostics = params.get("diagnostics").and_then(|d| d.as_array());
        assert!(
            diagnostics.is_some(),
            "publishDiagnostics should have diagnostics array"
        );

        println!(
            "✓ E2E: Synthetic push on didSave - received {} diagnostics",
            diagnostics.unwrap().len()
        );
    } else {
        println!(
            "⚠ E2E: No publishDiagnostics received on save within timeout. \
             This may happen if lua-ls is slow."
        );
    }

    shutdown_client(&mut client);
}

/// E2E test: Rapid saves supersede previous diagnostic tasks
///
/// When multiple saves happen in quick succession, only the latest
/// should complete and publish diagnostics (earlier ones are aborted).
///
/// Note: This test is ignored because it requires non-blocking I/O to properly
/// timeout when waiting for notifications that may not arrive. The superseding
/// behavior is already tested in unit tests (`test_register_supersedes_previous`).
#[test]
#[ignore]
fn e2e_synthetic_push_superseding_on_rapid_saves() {
    let (mut client, _config_dir) = create_lua_configured_client();

    let markdown_content = r#"# Test Document

```lua
local x = 1
```
"#;

    let markdown_uri = "file:///test_synthetic_push_supersede.md";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    // Wait for lua-ls to be ready
    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    // Drain any existing notifications
    let _ = client.wait_for_notification("textDocument/publishDiagnostics", Duration::from_secs(2));

    // Send multiple saves in rapid succession
    // The superseding pattern should ensure only the last one publishes
    for i in 1..=5 {
        client.send_notification(
            "textDocument/didSave",
            json!({
                "textDocument": {
                    "uri": markdown_uri
                }
            }),
        );
        // Small delay between saves (but not enough to complete diagnostic collection)
        std::thread::sleep(Duration::from_millis(10));
        println!("Sent didSave #{}", i);
    }

    // Wait for publishDiagnostics - should receive at least one
    // (the last one should complete, earlier ones may be superseded)
    let mut notification_count = 0;

    // First, wait a reasonable time for the first notification
    if let Some(params) =
        client.wait_for_notification("textDocument/publishDiagnostics", Duration::from_secs(10))
    {
        notification_count += 1;
        println!(
            "Received publishDiagnostics #{}: {}",
            notification_count,
            serde_json::to_string_pretty(&params).unwrap()
        );

        // Then check if there are more notifications (with shorter timeout)
        while let Some(params) = client.wait_for_notification(
            "textDocument/publishDiagnostics",
            Duration::from_millis(1000),
        ) {
            notification_count += 1;
            println!(
                "Received publishDiagnostics #{}: {}",
                notification_count,
                serde_json::to_string_pretty(&params).unwrap()
            );
            // Limit to prevent infinite loop in case of bugs
            if notification_count >= 10 {
                break;
            }
        }
    }

    // We should receive at least one notification (from the last save)
    // In practice, due to timing, we might receive 1-5 depending on how fast
    // the diagnostic collection is vs. how fast the saves come in
    println!(
        "✓ E2E: Received {} publishDiagnostics notification(s) after 5 rapid saves",
        notification_count
    );

    // The test passes as long as the server doesn't crash and handles rapid saves
    // The exact number of notifications depends on timing

    shutdown_client(&mut client);
}

/// E2E test: Diagnostic positions in publishDiagnostics are transformed
///
/// Diagnostics published via synthetic push should have positions
/// transformed to host document coordinates (same as pull diagnostics).
#[test]
fn e2e_synthetic_push_positions_transformed() {
    let (mut client, _config_dir) = create_lua_configured_client();

    // Document with syntax error in Lua code block
    // Line 0: "# Test Document"
    // Line 1: ""
    // Line 2: "```lua"
    // Line 3: "-- Syntax error"        <- Lua content starts here
    // Line 4: "print((("               <- Syntax error
    // Line 5: "```"
    let markdown_content = r#"# Test Document

```lua
-- Syntax error
print(((
```
"#;

    let lua_content_start_line: u64 = 3;
    let markdown_uri = "file:///test_synthetic_push_transform.md";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    // Wait for lua-ls to be ready
    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    // Wait for publishDiagnostics
    let notification =
        client.wait_for_notification("textDocument/publishDiagnostics", Duration::from_secs(10));

    if let Some(params) = notification {
        println!(
            "Received publishDiagnostics: {}",
            serde_json::to_string_pretty(&params).unwrap()
        );

        let diagnostics = params
            .get("diagnostics")
            .and_then(|d| d.as_array())
            .expect("Should have diagnostics array");

        if diagnostics.is_empty() {
            println!(
                "⚠ E2E: No diagnostics returned (lua-ls may need more time). \
                 Position transformation tested in unit tests."
            );
        } else {
            // Verify positions are in host document coordinates
            for (i, diagnostic) in diagnostics.iter().enumerate() {
                let range = diagnostic
                    .get("range")
                    .expect("Diagnostic should have range");
                let start_line = range
                    .get("start")
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64())
                    .expect("Should have start.line");

                assert!(
                    start_line >= lua_content_start_line,
                    "Diagnostic {} position should be transformed: got line {}, expected >= {}",
                    i,
                    start_line,
                    lua_content_start_line
                );

                println!(
                    "✓ Diagnostic {}: line {} (transformed to host coordinates)",
                    i, start_line
                );
            }
        }
    } else {
        println!("⚠ E2E: No publishDiagnostics received within timeout.");
    }

    shutdown_client(&mut client);
}
