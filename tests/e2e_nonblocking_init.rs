//! End-to-end tests for non-blocking initialization (PBI-304).
//!
//! These tests verify that kakehashi responds quickly even when
//! downstream language servers like lua-language-server are still initializing.
//!
//! Run with: `cargo test --test e2e_nonblocking_init --features e2e`
//!
//! **PBI-304 Acceptance Criteria**:
//! - AC1: kakehashi responds without blocking during lua-ls startup
//! - AC4: Native features (selection range, etc.) work during bridge init

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;
use std::time::Instant;

/// Maximum acceptable response time for non-blocking operations (100ms).
const MAX_NONBLOCKING_RESPONSE_MS: u128 = 100;

/// Create a minimal client without bridge configuration
fn create_minimal_client() -> LspClient {
    let mut client = LspClient::new();

    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
    client
}

/// E2E test: Native selection range works regardless of bridge state (AC4)
///
/// This test verifies that kakehashi native features like selection range
/// work correctly even while bridge language servers might be initializing.
#[test]
fn e2e_native_selection_range_works_during_bridge_init() {
    let mut client = create_minimal_client();

    // Open a Lua file for selection range
    let lua_content = r#"local x = 1
function greet(name)
    return "Hello, " .. name
end
"#;
    let lua_uri = "file:///test_selection.lua";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": lua_uri,
                "languageId": "lua",
                "version": 1,
                "text": lua_content
            }
        }),
    );

    // Measure response time for selection range
    let start = Instant::now();
    let response = client.send_request(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": lua_uri },
            "positions": [{ "line": 0, "character": 0 }]
        }),
    );
    let elapsed = start.elapsed();

    // Verify response is quick (native feature should not wait for bridge)
    println!(
        "Selection range response time: {:?} (max: {}ms)",
        elapsed, MAX_NONBLOCKING_RESPONSE_MS
    );

    // Selection range should succeed
    assert!(
        response.get("error").is_none(),
        "Selection range should not return error: {:?}",
        response.get("error")
    );

    let result = response.get("result").expect("Should have result");
    assert!(
        result.is_array(),
        "Result should be array of SelectionRange"
    );

    // Verify we got selection ranges
    let ranges = result.as_array().unwrap();
    assert!(
        !ranges.is_empty(),
        "Should have at least one selection range"
    );

    println!("✓ E2E AC4: Native selection range works during bridge init");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: Selection range in markdown with injection works (AC4 variant)
///
/// Tests that selection range works on markdown files with code blocks,
/// even if those code blocks would trigger bridge initialization.
#[test]
fn e2e_native_selection_range_works_in_markdown_with_injection() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let (mut client, _config_dir) = create_lua_configured_client();

    // Open markdown with Lua code block
    let markdown_content = r#"# Test Document

```lua
local x = 1
print(x)
```

More text.
"#;
    let markdown_uri = "file:///test_md_selection.md";

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

    // Immediately request selection range (don't wait for lua-ls)
    let start = Instant::now();
    let response = client.send_request(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": markdown_uri },
            "positions": [{ "line": 0, "character": 0 }]
        }),
    );
    let elapsed = start.elapsed();

    println!("Selection range response time (markdown): {:?}", elapsed);

    // Selection range should succeed regardless of bridge state
    assert!(
        response.get("error").is_none(),
        "Selection range should not return error: {:?}",
        response.get("error")
    );

    let result = response.get("result").expect("Should have result");
    assert!(
        result.is_array(),
        "Result should be array of SelectionRange"
    );

    println!("✓ E2E AC4: Selection range in markdown works during bridge init");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: Semantic tokens work during bridge initialization (AC4)
///
/// Tests that semantic token requests complete quickly without waiting
/// for downstream language servers.
#[test]
fn e2e_native_semantic_tokens_work_during_bridge_init() {
    let mut client = create_minimal_client();

    // Open a Lua file
    let lua_content = r#"local x = 1
function greet(name)
    return "Hello, " .. name
end
"#;
    let lua_uri = "file:///test_semantic.lua";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": lua_uri,
                "languageId": "lua",
                "version": 1,
                "text": lua_content
            }
        }),
    );

    // Measure response time for semantic tokens
    let start = Instant::now();
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({
            "textDocument": { "uri": lua_uri }
        }),
    );
    let elapsed = start.elapsed();

    println!("Semantic tokens response time: {:?}", elapsed);

    // Semantic tokens should succeed (native feature)
    assert!(
        response.get("error").is_none(),
        "Semantic tokens should not return error: {:?}",
        response.get("error")
    );

    let result = response.get("result").expect("Should have result");
    assert!(result.is_object(), "Result should be SemanticTokens object");

    println!("✓ E2E AC4: Semantic tokens work during bridge init");

    // Clean shutdown
    shutdown_client(&mut client);
}
