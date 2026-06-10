//! End-to-end test for Lua folding ranges in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for folding range:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Folding range request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms line numbers
//!
//! Run with: `cargo test --test e2e_lsp_lua_folding_range --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH
//! (it advertises `foldingRangeProvider: true`).

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: folding ranges from the injected Lua region are translated to
/// host document lines.
#[test]
fn e2e_folding_range_translated_to_host_lines() {
    if skip_if_lua_ls_unavailable() {
        return;
    }
    let (mut client, _config_dir) = create_lua_configured_client();

    // Lines (0-based):           host
    // 0: # Test Document
    // 1: (blank)
    // 2: ```lua
    // 3: local function greet(name)   <- virtual line 0
    // 4:   print("hello")
    // 5:   print(name)
    // 6: end                          <- virtual line 3
    // 7: ```
    let markdown_content = r#"# Test Document

```lua
local function greet(name)
  print("hello")
  print(name)
end
```
"#;

    let markdown_uri = "file:///test_folding_range.md";

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

    // Poll: lua-ls spawns lazily, so early requests may come back empty until
    // the downstream server is ready.
    let mut ranges = Vec::new();
    for _ in 0..60 {
        let response = client.send_request(
            "textDocument/foldingRange",
            json!({
                "textDocument": { "uri": markdown_uri }
            }),
        );

        if let Some(error) = response.get("error") {
            panic!("foldingRange returned an error: {:?}", error);
        }
        if let Some(result) = response.get("result")
            && let Some(items) = result.as_array()
            && !items.is_empty()
        {
            ranges = items.clone();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    println!("Folding ranges: {:?}", ranges);
    assert!(
        !ranges.is_empty(),
        "lua-ls supports foldingRange; expected at least one range for the function block"
    );

    // The Lua region spans host lines 3..=6; virtual coordinates would be 0..=3.
    // A range starting below line 3 means the virtual->host translation was lost.
    for range in &ranges {
        let start_line = range["startLine"].as_u64().expect("startLine present");
        let end_line = range["endLine"].as_u64().expect("endLine present");
        assert!(
            (3..=6).contains(&start_line),
            "startLine should be in host coordinates within the code block (expected 3..=6, got {})",
            start_line
        );
        assert!(
            (3..=6).contains(&end_line),
            "endLine should be in host coordinates within the code block (expected 3..=6, got {})",
            end_line
        );
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: folding range for markdown without code blocks returns null.
#[test]
fn e2e_folding_range_no_injections_returns_null() {
    if skip_if_lua_ls_unavailable() {
        return;
    }
    let (mut client, _config_dir) = create_lua_configured_client();

    let markdown_content = r#"# Test Document

Just some plain text without any code blocks.
"#;

    let markdown_uri = "file:///test_folding_no_injections.md";

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

    let response = client.send_request(
        "textDocument/foldingRange",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    assert!(
        response.get("error").is_none(),
        "Should not return error for markdown without injections"
    );
    let result = response.get("result");
    assert!(
        result.is_some() && result.unwrap().is_null(),
        "foldingRange for markdown without injections should return null"
    );

    // Clean shutdown
    shutdown_client(&mut client);
}
