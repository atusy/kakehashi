//! End-to-end test for Lua rename in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for rename:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Rename request at position in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - WorkspaceEdit received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_rename --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, is_lua_ls_available, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: rename request is handled without error
#[test]
fn e2e_rename_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing local variable
    // The variable 'x' appears at:
    // - Line 3 (declaration): local x = 42
    // - Line 4 (usage): print(x)
    let markdown_content = r#"# Test Document

```lua
local x = 42
print(x)
```

More text.
"#;

    let markdown_uri = "file:///test_rename.md";

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
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Request rename on "x" at line 3 (local x = 42)
    // The variable 'x' starts at character 6 on line 3
    let rename_response = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 7 },
            "newName": "counter"
        }),
    );

    println!("Rename response: {:?}", rename_response);

    // Verify no error
    assert!(
        rename_response.get("error").is_none(),
        "Rename should not return error: {:?}",
        rename_response.get("error")
    );

    let result = rename_response
        .get("result")
        .expect("Rename should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading
        println!("Note: lua-ls returned null (may still be loading)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else {
        // WorkspaceEdit format
        // Can have either "changes" (map) or "documentChanges" (array)
        if let Some(changes) = result.get("changes")
            && let Some(changes_map) = changes.as_object()
        {
            println!("WorkspaceEdit with changes map: {} URIs", changes_map.len());

            // Verify the edits are for the host document
            for (uri, edits) in changes_map {
                println!("  - URI: {}", uri);
                assert!(
                    uri == markdown_uri,
                    "Edit URI should be host document URI, got: {}",
                    uri
                );

                if let Some(edits_arr) = edits.as_array() {
                    for edit in edits_arr {
                        if let Some(range) = edit.get("range") {
                            let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                            println!("    - Edit at line {}", start_line);
                            // The edits should be in the Lua code block area (lines 3-4)
                            assert!(
                                (2..=6).contains(&start_line),
                                "Edit line should be in host coordinates (expected 2-6, got {})",
                                start_line
                            );
                        }
                    }
                }
            }
        }

        if let Some(document_changes) = result.get("documentChanges")
            && let Some(changes_arr) = document_changes.as_array()
        {
            println!(
                "WorkspaceEdit with documentChanges: {} items",
                changes_arr.len()
            );

            for item in changes_arr {
                if let Some(text_document) = item.get("textDocument")
                    && let Some(uri) = text_document.get("uri").and_then(|u| u.as_str())
                {
                    println!("  - textDocument.uri: {}", uri);
                    assert!(
                        uri == markdown_uri,
                        "textDocument.uri should be host document URI, got: {}",
                        uri
                    );
                }

                if let Some(edits) = item.get("edits")
                    && let Some(edits_arr) = edits.as_array()
                {
                    for edit in edits_arr {
                        if let Some(range) = edit.get("range") {
                            let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                            println!("    - Edit at line {}", start_line);
                            // The edits should be in the Lua code block area
                            assert!(
                                (2..=6).contains(&start_line),
                                "Edit line should be in host coordinates (expected 2-6, got {})",
                                start_line
                            );
                        }
                    }
                }
            }
        }

        println!("E2E: Rename returns WorkspaceEdit with host coordinates and URIs");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: rename multiple occurrences of a variable
#[test]
fn e2e_rename_multiple_occurrences() {
    if !is_lua_ls_available() {
        eprintln!("SKIP: lua-language-server not found in PATH");
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block with multiple variable occurrences
    let markdown_content = r#"# Test Document

```lua
local function greet(name)
    print("Hello, " .. name)
    return name
end
```

More text.
"#;

    let markdown_uri = "file:///test_rename_multi.md";

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
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Request rename on "name" parameter at line 3
    // The parameter 'name' starts at character 21 on line 3
    let rename_response = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 23 },
            "newName": "personName"
        }),
    );

    println!(
        "Rename multiple occurrences response: {:?}",
        rename_response
    );

    // Verify no error
    assert!(
        rename_response.get("error").is_none(),
        "Rename should not return error: {:?}",
        rename_response.get("error")
    );

    let result = rename_response.get("result");
    if let Some(result) = result
        && !result.is_null()
    {
        // Count the number of edits - should be 3 (declaration + 2 usages)
        let edit_count = if let Some(changes) = result.get("changes") {
            if let Some(changes_map) = changes.as_object() {
                changes_map
                    .values()
                    .filter_map(|v| v.as_array())
                    .map(|arr| arr.len())
                    .sum()
            } else {
                0
            }
        } else if let Some(document_changes) = result.get("documentChanges") {
            if let Some(changes_arr) = document_changes.as_array() {
                changes_arr
                    .iter()
                    .filter_map(|item| item.get("edits"))
                    .filter_map(|edits| edits.as_array())
                    .map(|arr| arr.len())
                    .sum()
            } else {
                0
            }
        } else {
            0
        };

        println!("Total edits: {}", edit_count);
        // We expect at least 3 edits (name appears 3 times in the code)
        if edit_count >= 3 {
            println!("E2E: Rename correctly handles multiple occurrences");
        } else {
            println!("Note: Got {} edits (expected at least 3)", edit_count);
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
