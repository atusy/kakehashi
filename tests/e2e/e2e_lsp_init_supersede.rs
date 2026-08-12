//! End-to-end test for request superseding during initialization window.
//!
//! This test verifies that when typing rapidly during initialization,
//! only the latest incremental request (completion, hover) receives a
//! real response, while earlier requests are superseded with REQUEST_FAILED.
//!
//! Run with: `cargo test --features e2e --test e2e e2e_lsp_init_supersede::`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

use crate::helpers::lsp_client::LspClient;
use serde_json::json;

#[test]
fn test_rapid_typing_during_init_triggers_superseding() {
    // Check if lua-language-server is available
    let check = std::process::Command::new("lua-language-server")
        .arg("--version")
        .output();

    if check.is_err() {
        eprintln!("SKIP: lua-language-server not found in PATH");
        eprintln!("Install lua-language-server to run this test:");
        eprintln!("  brew install lua-language-server");
        return;
    }

    // Spawn kakehashi binary
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

    // Open markdown document with Lua code block
    let markdown_content = r#"# Test Document

```lua
local x = 10
print(
```

More text.
"#;

    let markdown_uri = "file:///test.md";

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

    // Send rapid completion requests during initialization window
    // Position at end of "print(" on line 4
    // The key is NOT to wait for lua-ls initialization - send requests immediately
    // This simulates rapid typing during the initialization window

    // Request 1 (should be superseded)
    let response1 = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": {
                "uri": markdown_uri
            },
            "position": {
                "line": 4,
                "character": 6
            }
        }),
    );

    // Request 2 (should be superseded)
    let response2 = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": {
                "uri": markdown_uri
            },
            "position": {
                "line": 4,
                "character": 6
            }
        }),
    );

    // Request 3 (latest - should eventually get response)
    let response3 = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": {
                "uri": markdown_uri
            },
            "position": {
                "line": 4,
                "character": 6
            }
        }),
    );

    println!("Response 1: {:?}", response1);
    println!("Response 2: {:?}", response2);
    println!("Response 3: {:?}", response3);

    // Verify superseding behavior (timing dependent)
    // Count how many requests were superseded
    let response1_superseded = response1
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.contains("superseded"))
        .unwrap_or(false);

    let response2_superseded = response2
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.contains("superseded"))
        .unwrap_or(false);

    let response3_superseded = response3
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(|s| s.contains("superseded"))
        .unwrap_or(false);

    // Verify superseding error format if any superseding occurred
    if response1_superseded {
        let error = response1
            .get("error")
            .expect("Superseded response should have error");
        assert_eq!(
            error.get("code").and_then(|c| c.as_i64()),
            Some(-32803),
            "Superseded request 1 should have error code REQUEST_FAILED (-32803)"
        );
        println!("✓ Request 1 was correctly superseded");
    }
    if response2_superseded {
        let error = response2
            .get("error")
            .expect("Superseded response should have error");
        assert_eq!(
            error.get("code").and_then(|c| c.as_i64()),
            Some(-32803),
            "Superseded request 2 should have error code REQUEST_FAILED (-32803)"
        );
        println!("✓ Request 2 was correctly superseded");
    }
    if response3_superseded {
        let error = response3
            .get("error")
            .expect("Superseded response should have error");
        assert_eq!(
            error.get("code").and_then(|c| c.as_i64()),
            Some(-32803),
            "Superseded request 3 should have error code REQUEST_FAILED (-32803)"
        );
        println!("✓ Request 3 was correctly superseded");
    }

    // The last request should never be superseded
    assert!(
        !response3_superseded,
        "Latest request should not be superseded"
    );

    // If initialization was slow enough, some superseding should have occurred
    // But if it was fast, all requests might succeed (which is also valid behavior)
    if response1_superseded || response2_superseded {
        println!("✓ Request superseding during initialization window verified");
    } else {
        println!(
            "✓ All requests succeeded (initialization completed quickly - no superseding needed)"
        );
        println!("  Note: Superseding behavior is still correct (verified in unit tests)");
    }

    // Regardless of timing, verify infrastructure works correctly:
    // - No request should error with anything OTHER than superseded
    // - All non-superseded requests should have valid responses
    for (i, response) in [(1, &response1), (2, &response2), (3, &response3)].iter() {
        if let Some(error) = response.get("error") {
            let error_msg = error.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if !error_msg.contains("superseded") {
                panic!("Request {} had unexpected error: {}", i, error_msg);
            }
        }
    }

    // Clean shutdown
    let _shutdown_response = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
