//! End-to-end test for Lua inlay hints in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for inlay hints:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Inlay hint request with range in Lua block
//! - kakehashi detects injection, translates range, spawns lua-ls
//! - Inlay hints received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_inlay_hint --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: inlay hint request is handled without error
#[test]
fn e2e_inlay_hint_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block
    // lua-language-server provides type hints for variables
    let markdown_content = r#"# Test Document

```lua
local function add(a, b)
    local result = a + b
    return result
end

local sum = add(1, 2)
print(sum)
```

More text.
"#;

    let markdown_uri = "file:///test_inlay_hint.md";

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

    // Give lua-ls time to process
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // Request inlay hints for the Lua code block area
    // The code block is at lines 2-11 (0-indexed: line 2 is "```lua", line 11 is "```")
    // Content starts at line 3
    let inlay_hint_response = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": markdown_uri },
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 11, "character": 0 }
            }
        }),
    );

    println!("Inlay hint response: {:?}", inlay_hint_response);

    // Verify no error
    assert!(
        inlay_hint_response.get("error").is_none(),
        "Inlay hint should not return error: {:?}",
        inlay_hint_response.get("error")
    );

    let result = inlay_hint_response
        .get("result")
        .expect("Inlay hint should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading or if no hints available
        println!("Note: lua-ls returned null (may still be loading or no hints available)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // InlayHint[] format
        let hints = result.as_array().unwrap();
        println!("Inlay hints found: {} items", hints.len());

        // If hints are returned, verify coordinates are in host document range
        for hint in hints {
            if let Some(position) = hint.get("position") {
                let line = position["line"].as_u64().unwrap_or(0);
                println!("  - Hint at line {}", line);
                // The hints should be in the Lua code block area (lines 3-10)
                assert!(
                    (2..=12).contains(&line),
                    "Hint line should be in host coordinates (expected 2-12, got {})",
                    line
                );
            }
        }
        println!("E2E: Inlay hint returns hints with host coordinates");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
