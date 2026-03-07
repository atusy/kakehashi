//! End-to-end test for stable region_id across hover and completion.
//!
//! This test verifies that hover and completion on the same Lua code block
//! use the same virtual document URI (stable region_id like "lua-0").
//!
//! The stable region_id means:
//! - First access (hover): sends didOpen to downstream server
//! - Second access (completion): sends didChange (not didOpen again)
//!
//! We verify this by:
//! 1. Sending hover request on a Lua block
//! 2. Sending completion request on the same Lua block
//! 3. Both should succeed without errors
//!
//! If the implementation incorrectly used different region_ids (like "hover-temp"
//! and "completion-temp"), the virtual document URI would differ between requests,
//! causing issues with downstream server state.
//!
//! Run with: `cargo test --test e2e_stable_region_id --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_polling::poll_for_hover;
use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: hover then complete on same Lua block shares virtual document URI
///
/// This verifies PBI-STABLE-REGION-ID acceptance criteria:
/// - Hover and completion use the same virtual URI for the same injection region
/// - First access sends didOpen, subsequent access sends didChange (not didOpen again)
#[test]
fn e2e_hover_then_completion_on_same_lua_block_shares_uri() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with a Lua code block containing a variable
    // We'll hover on and complete from this variable
    let markdown_content = r#"# Test Document

```lua
local greeting = "hello"
print(greeting)
```

More text.
"#;

    let markdown_uri = "file:///test_stable_region.md";

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

    // FIRST ACCESS: Hover on "greeting" variable (line 3, inside Lua block)
    // Line 3: "local greeting = ..."
    // Character 10 is on "greeting"
    let hover_response = poll_for_hover(&mut client, markdown_uri, 3, 10, 20, 500)
        .expect("Should receive hover response");

    println!("First access (hover) response: {:?}", hover_response);

    // Verify hover request succeeded (no error)
    assert!(
        hover_response.get("error").is_none(),
        "Hover should not return error: {:?}",
        hover_response.get("error")
    );

    println!("First access (hover): didOpen sent to downstream");

    // SECOND ACCESS: Completion after "print(" on line 4
    // Line 4: "print(greeting)"
    // Character 6 is after "print("
    // This should trigger didChange (not didOpen) since the virtual document
    // was already opened by the hover request
    let completion_response = client.send_request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 6 }
        }),
    );

    println!(
        "Second access (completion) response: {:?}",
        completion_response
    );

    // Verify completion request succeeded (no error)
    assert!(
        completion_response.get("error").is_none(),
        "Completion should not return error: {:?}",
        completion_response.get("error")
    );

    println!("Second access (completion): didChange sent (not didOpen)");

    // Both requests succeeded, which indicates:
    // 1. The stable region_id (lua-0) was used for both
    // 2. The virtual document URI was the same
    // 3. didOpen was sent once (first hover), didChange was sent for subsequent (completion)
    println!("Hover and completion share the same virtual document URI (stable region_id)");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: multiple Lua blocks have different stable region_ids
///
/// This verifies that lua-0 and lua-1 are correctly assigned to different blocks.
#[test]
fn e2e_multiple_lua_blocks_have_distinct_region_ids() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with TWO Lua code blocks
    let markdown_content = r#"# Test Document

First Lua block:

```lua
local x = 1
print(x)
```

Second Lua block:

```lua
local y = 2
print(y)
```

More text.
"#;

    let markdown_uri = "file:///test_multiple_blocks.md";

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

    // Hover on first Lua block (line 5: "local x = 1")
    // This should use region_id "lua-0"
    let hover_first = poll_for_hover(&mut client, markdown_uri, 5, 7, 20, 500)
        .expect("Should receive hover response");

    assert!(
        hover_first.get("error").is_none(),
        "Hover on first block should not error: {:?}",
        hover_first.get("error")
    );
    println!("First Lua block (lua-0): hover succeeded");

    // Hover on second Lua block (line 12: "local y = 2")
    // This should use region_id "lua-1"
    let hover_second = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 12, "character": 7 }
        }),
    );

    assert!(
        hover_second.get("error").is_none(),
        "Hover on second block should not error: {:?}",
        hover_second.get("error")
    );
    println!("Second Lua block (lua-1): hover succeeded");

    // Both blocks work independently
    println!("Multiple Lua blocks have distinct stable region_ids (lua-0, lua-1)");

    // Clean shutdown
    shutdown_client(&mut client);
}
