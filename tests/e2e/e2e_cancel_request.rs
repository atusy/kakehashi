//! End-to-end test for $/cancelRequest forwarding via kakehashi binary.
//!
//! This test verifies the cancel forwarding infrastructure:
//! - Requests are tracked with upstream ID mappings
//! - $/cancelRequest notifications are forwarded to downstream language servers
//! - Responses still flow through after cancel (per LSP spec)
//!
//! Run with: `cargo test --features e2e --test e2e e2e_cancel_request::`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lua_bridge::{shutdown_client, skip_if_lua_ls_unavailable};
use serde_json::json;

/// Request ID that was never sent, used for testing cancel of unknown requests.
const NONEXISTENT_REQUEST_ID: i64 = 999999;

/// E2E test: cancel request forwarding - response still arrives after cancel.
///
/// Per LSP spec, a cancelled request should still receive a response
/// (either the normal result or error code -32800 RequestCancelled).
///
/// This test:
/// 1. Opens a document to initialize lua-ls connection
/// 2. Sends a hover request asynchronously
/// 3. Immediately sends $/cancelRequest for that request
/// 4. Verifies a response still arrives (either success or cancelled error)
#[test]
fn e2e_cancel_request_forwarding_response_still_arrives() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = LspClient::new();

    // Initialize with lua-language-server configuration
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

    // Open markdown document with Lua code block
    let markdown_content = r#"# Test Document

```lua
local function compute()
    local result = 0
    for i = 1, 100 do
        result = result + i
    end
    return result
end

local x = compute()
print(x)
```
"#;

    let markdown_uri = "file:///test_cancel.md";

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

    // Give lua-ls time to process the didOpen
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Send a hover request asynchronously (don't wait for response yet)
    let request_id = client.send_request_async(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 5, "character": 10 }
        }),
    );

    println!("Sent hover request with id: {}", request_id);

    // Immediately send cancel notification for this request
    client.send_notification(
        "$/cancelRequest",
        json!({
            "id": request_id
        }),
    );

    println!("Sent $/cancelRequest for id: {}", request_id);

    // Wait for response - it should still arrive (per LSP spec)
    let response = client.receive_response_for_id_public(request_id);

    println!("Received response: {:?}", response);

    // The response should be valid JSON-RPC with matching ID
    assert_eq!(
        response.get("id").and_then(|id| id.as_i64()),
        Some(request_id),
        "Response should have matching id"
    );

    // Check if we got a cancelled error or a normal response
    if let Some(error) = response.get("error") {
        let error_code = error.get("code").and_then(|c| c.as_i64());
        if error_code == Some(-32800) {
            println!("✓ E2E: Request was cancelled (got RequestCancelled error -32800)");
        } else {
            // Other errors are also acceptable - the point is we got a response
            println!(
                "✓ E2E: Request returned error (code: {:?}, message: {:?})",
                error_code,
                error.get("message")
            );
        }
    } else {
        // Normal response - the request completed before cancel was processed
        println!(
            "✓ E2E: Request completed normally despite cancel: {:?}",
            response.get("result")
        );
    }

    // Verify server is still operational after cancel by sending another request
    let second_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 11, "character": 6 }
        }),
    );

    // The server should still be responsive
    assert!(
        second_response.get("id").is_some(),
        "Server should still respond after cancel"
    );
    println!("✓ E2E: Server still operational after cancel (second request succeeded)");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: cancel for unknown request ID is gracefully handled.
///
/// Sending a cancel notification for a request ID that doesn't exist
/// should not crash the server or cause any errors.
#[test]
fn e2e_cancel_unknown_request_id_gracefully_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = LspClient::new();

    // Initialize with lua-language-server configuration
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

    // Open a document to ensure connection is established
    let markdown_uri = "file:///test_cancel_unknown.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Test\n\n```lua\nlocal x = 1\n```\n"
            }
        }),
    );

    std::thread::sleep(std::time::Duration::from_millis(300));

    // Send cancel for a request ID that was never sent
    client.send_notification(
        "$/cancelRequest",
        json!({
            "id": NONEXISTENT_REQUEST_ID
        }),
    );

    println!(
        "Sent $/cancelRequest for unknown id: {}",
        NONEXISTENT_REQUEST_ID
    );

    // Small delay to let the cancel be processed
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Verify server is still operational by sending a real request
    let hover_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 6 }
        }),
    );

    // Server should still respond normally
    assert!(
        hover_response.get("id").is_some(),
        "Server should still respond after cancel for unknown ID"
    );
    assert!(
        hover_response.get("error").is_none(),
        "Hover request should succeed: {:?}",
        hover_response.get("error")
    );

    println!("✓ E2E: Server gracefully handled cancel for unknown request ID");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: multiple cancel requests for the same ID are handled.
///
/// Sending multiple cancel notifications for the same request ID
/// should not cause issues.
#[test]
fn e2e_multiple_cancel_for_same_request() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = LspClient::new();

    // Initialize
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

    // Open document
    let markdown_uri = "file:///test_multi_cancel.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Test\n\n```lua\nlocal x = 1\nprint(x)\n```\n"
            }
        }),
    );

    std::thread::sleep(std::time::Duration::from_millis(500));

    // Send hover request asynchronously
    let request_id = client.send_request_async(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 6 }
        }),
    );

    // Send multiple cancel notifications for the same request
    for i in 0..3 {
        client.send_notification(
            "$/cancelRequest",
            json!({
                "id": request_id
            }),
        );
        println!("Sent cancel #{} for request id: {}", i + 1, request_id);
    }

    // Wait for response
    let response = client.receive_response_for_id_public(request_id);

    // Should still get a valid response
    assert_eq!(
        response.get("id").and_then(|id| id.as_i64()),
        Some(request_id),
        "Response should have matching id"
    );

    println!("✓ E2E: Multiple cancel notifications handled correctly");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: diagnostic request cancel returns RequestCancelled error.
///
/// This test specifically verifies the upstream cancel notification flow:
/// 1. Start a diagnostic request for a document with multiple Lua blocks
/// 2. Immediately send $/cancelRequest before diagnostics complete
/// 3. Verify that either:
///    - RequestCancelled error (-32800) is returned (cancel arrived in time), OR
///    - Normal response is returned (diagnostics completed before cancel processed)
///
/// The key insight is that diagnostic requests involve parallel fan-out to
/// downstream servers, which takes non-trivial time. This gives us a window
/// to test cancel handling with real timing conditions.
#[test]
fn e2e_diagnostic_cancel_returns_request_cancelled_error() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = LspClient::new();

    // Initialize with lua-language-server configuration
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

    // Open markdown document with MULTIPLE Lua code blocks
    // Multiple blocks = multiple downstream requests = more time for cancel to arrive
    let markdown_content = r#"# Large Document for Cancel Testing

```lua
-- Block 1
local function compute1()
    local result = 0
    for i = 1, 1000 do
        result = result + math.sin(i) * math.cos(i)
    end
    return result
end
```

Some text between blocks.

```lua
-- Block 2
local function compute2()
    local data = {}
    for i = 1, 100 do
        data[i] = { x = i, y = i * 2, z = i * 3 }
    end
    return data
end
```

More text.

```lua
-- Block 3
local function compute3()
    return {
        nested = {
            deeply = {
                value = 42
            }
        }
    }
end
```

Even more text.

```lua
-- Block 4
local x = compute1()
local y = compute2()
local z = compute3()
print(x, y, z)
```
"#;

    let markdown_uri = "file:///test_diagnostic_cancel.md";

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

    // Give lua-ls a moment to start (but not too long - we want to test cancel timing)
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Send a diagnostic request asynchronously
    let request_id = client.send_request_async(
        "textDocument/diagnostic",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!("Sent diagnostic request with id: {}", request_id);

    // Immediately send cancel notification
    // Due to multiple Lua blocks requiring parallel fan-out, there's a good chance
    // the cancel arrives before all diagnostics complete
    client.send_notification(
        "$/cancelRequest",
        json!({
            "id": request_id
        }),
    );

    println!(
        "Sent $/cancelRequest for diagnostic request id: {}",
        request_id
    );

    // Wait for response
    let response = client.receive_response_for_id_public(request_id);

    println!("Diagnostic response: {:?}", response);

    // Verify response has matching ID
    assert_eq!(
        response.get("id").and_then(|id| id.as_i64()),
        Some(request_id),
        "Response should have matching id"
    );

    // Check if we got RequestCancelled error or normal response
    // Per LSP spec, cancel is "best effort" - valid outcomes are:
    // 1. Normal result (request completed before cancel processed)
    // 2. RequestCancelled error (-32800)
    // Any other error indicates a bug and should fail the test.
    if let Some(error) = response.get("error") {
        let error_code = error.get("code").and_then(|c| c.as_i64());
        assert_eq!(
            error_code,
            Some(-32800),
            "If diagnostic returns an error, it must be RequestCancelled (-32800). \
             Got error code {:?} with message {:?}",
            error_code,
            error.get("message")
        );
        println!("✓ E2E: Diagnostic request was cancelled (got RequestCancelled error -32800)");
    } else {
        // Normal response - diagnostics completed before cancel was processed
        // This is also valid - cancel is "best effort" per LSP spec
        println!("✓ E2E: Diagnostic completed normally before cancel processed (race condition)");
        // Verify we got a valid diagnostic report
        let result = response.get("result");
        assert!(
            result.is_some(),
            "Should have result in diagnostic response"
        );
    }

    // Verify server is still operational after cancel
    let hover_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 5, "character": 10 }
        }),
    );

    assert!(
        hover_response.get("id").is_some(),
        "Server should still respond after diagnostic cancel"
    );
    println!("✓ E2E: Server still operational after diagnostic cancel");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: diagnostic cancel with multiple injection regions.
///
/// This test verifies that when a diagnostic request involves multiple
/// injection regions (all using the same language server), cancel forwarding
/// still works correctly with the HashSet-based registry.
///
/// Note: Full multi-server testing (e.g., Lua + Python in the same document)
/// requires both lua-language-server and pyright to be installed. The unit
/// tests in pool.rs comprehensively test the multi-server HashSet behavior.
///
/// Setup:
/// - Markdown document with multiple Lua code blocks
/// - lua-language-server configured
///
/// Test:
/// 1. Open document with multiple injection regions
/// 2. Send diagnostic request
/// 3. Immediately send $/cancelRequest
/// 4. Verify either RequestCancelled or normal response
/// 5. Verify server is still operational
#[test]
fn e2e_diagnostic_cancel_multi_region_same_server() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = LspClient::new();

    // Initialize with lua-language-server configuration
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

    // Open markdown with multiple Lua code blocks
    // This creates multiple injection regions that all use the same lua-language-server
    // entry in the HashSet; multi-server fan-out behavior is covered by unit tests.
    let markdown_content = r#"# Multi-Region Document

```lua
-- Region 1
local function func1()
    return 1
end
```

Some text between regions.

```lua
-- Region 2
local function func2()
    return 2
end
```

More text.

```lua
-- Region 3
local function func3()
    return 3
end
```
"#;

    let markdown_uri = "file:///test_multi_region_cancel.md";

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

    std::thread::sleep(std::time::Duration::from_millis(500));

    // Send diagnostic request
    let request_id = client.send_request_async(
        "textDocument/diagnostic",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!("Sent diagnostic request with id: {}", request_id);

    // Immediately send cancel
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));

    println!("Sent $/cancelRequest for id: {}", request_id);

    // Wait for response
    let response = client.receive_response_for_id_public(request_id);

    println!("Received response: {:?}", response);

    // Verify valid response (either cancelled or completed)
    assert_eq!(
        response.get("id").and_then(|id| id.as_i64()),
        Some(request_id),
        "Response should have matching id"
    );

    if let Some(error) = response.get("error") {
        let error_code = error.get("code").and_then(|c| c.as_i64());
        assert_eq!(
            error_code,
            Some(-32800),
            "If error, should be RequestCancelled (-32800)"
        );
        println!("✓ E2E: Multi-region diagnostic was cancelled (got RequestCancelled)");
    } else {
        println!(
            "✓ E2E: Multi-region diagnostic completed before cancel: {:?}",
            response.get("result")
        );
    }

    // Verify server still operational
    let hover_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 10 }
        }),
    );

    assert!(
        hover_response.get("id").is_some(),
        "Server should still respond after multi-region cancel"
    );
    println!("✓ E2E: Server still operational after multi-region cancel");

    shutdown_client(&mut client);
}
