//! End-to-end tests for selectionRange using direct LSP communication with kakehashi binary.
//!
//! These tests verify that selection range requests work correctly with kakehashi native
//! implementation (NOT through the bridge - bridge support is not yet implemented).
//!
//! Selection range allows expanding/shrinking text selection based on syntax tree structure.
//! This is particularly useful for features like "smart select" or "expand region".
//!
//! Based on tests/test_lsp_select.lua which tests:
//! - Plain Lua files (no injection)
//! - Markdown files with injections (YAML frontmatter, code blocks, nested injections)
//!
//! Run with: `cargo test --features e2e --test e2e e2e_selection_range::`

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lsp_polling::poll_until;
use crate::helpers::sanitization::sanitize_selection_range_response;
use crate::helpers::test_fixtures::{
    create_selection_range_lua_fixture, create_selection_range_md_fixture,
};
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// Request `textDocument/selectionRange` once the document's parse has landed.
///
/// `didOpen` schedules parsing asynchronously and the server answers `null`
/// until it completes, so the fixed `sleep(100ms)` these call sites used was a
/// latent flake: under parallel load the parse routinely takes longer, and the
/// `result.as_array().unwrap()` that follows then panics on `None` with no
/// indication of why. Poll for the array instead — correct at any load, and
/// still instant when the parse is quick.
fn selection_range_when_parsed(client: &mut LspClient, request: Value) -> Value {
    poll_until(100, 50, || {
        let response = client.send_request("textDocument/selectionRange", request.clone());
        response
            .get("result")
            .filter(|r| r.is_array())
            .map(|_| response.clone())
    })
    .unwrap_or_else(|| panic!("textDocument/selectionRange never returned an array for {request}"))
}

/// Helper function to extract text from a range in content.
///
/// Converts LSP Position (line, character) to byte offsets and extracts the substring.
fn extract_range_text(content: &str, range: &Value) -> String {
    let start = range.get("start").unwrap();
    let end = range.get("end").unwrap();

    let start_line = start.get("line").unwrap().as_u64().unwrap() as usize;
    let start_char = start.get("character").unwrap().as_u64().unwrap() as usize;
    let end_line = end.get("line").unwrap().as_u64().unwrap() as usize;
    let end_char = end.get("character").unwrap().as_u64().unwrap() as usize;

    let lines: Vec<&str> = content.lines().collect();

    if start_line == end_line {
        // Single line range
        if let Some(line) = lines.get(start_line) {
            let chars: Vec<char> = line.chars().collect();
            let end_idx = end_char.min(chars.len());
            let start_idx = start_char.min(end_idx);
            return chars[start_idx..end_idx].iter().collect();
        }
    } else {
        // Multi-line range
        let mut result = String::new();
        for (i, line) in lines.iter().enumerate().skip(start_line) {
            if i > end_line {
                break;
            }
            if i == start_line {
                let chars: Vec<char> = line.chars().collect();
                result.push_str(&chars[start_char..].iter().collect::<String>());
                result.push('\n');
            } else if i == end_line {
                let chars: Vec<char> = line.chars().collect();
                let end_idx = end_char.min(chars.len());
                result.push_str(&chars[..end_idx].iter().collect::<String>());
            } else {
                result.push_str(line);
                result.push('\n');
            }
        }
        return result;
    }

    String::new()
}

/// Test selection range on a plain Lua file (no injections).
///
/// Based on test_lsp_select.lua test for assets/example.lua
/// Cursor at line 0 (0-indexed), col 0 - the "local" keyword
#[test]
fn test_selection_range_lua_no_injection() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "selectionRange": {
                        "dynamicRegistration": false
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create and open Lua test file
    let (uri, content, _temp_file) = create_selection_range_lua_fixture();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "lua",
                "version": 1,
                "text": content
            }
        }),
    );

    // Request selection range at line 0, col 0 (on "local" keyword)
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": {
                "uri": uri
            },
            "positions": [{
                "line": 0,
                "character": 0
            }]
        }),
    );

    // Verify response
    assert!(
        response.get("result").is_some(),
        "SelectionRange response should have result: {:?}",
        response
    );

    let result = response.get("result").unwrap();
    assert!(result.is_array(), "Result should be an array: {:?}", result);

    let ranges = result.as_array().unwrap();
    assert!(
        !ranges.is_empty(),
        "Should have at least one selection range"
    );

    // Verify first range structure
    let first_range = &ranges[0];
    assert!(
        first_range.get("range").is_some(),
        "SelectionRange should have range field: {:?}",
        first_range
    );

    // Extract text from the range
    let range = first_range.get("range").unwrap();
    let selected_text = extract_range_text(&content, range);

    // At position 0,0, the innermost selection should be "local" keyword
    assert!(
        selected_text.contains("local"),
        "Selected text should contain 'local', got: '{}'",
        selected_text
    );
}

/// Test selection range expansion through parent chain.
///
/// Verifies that the parent field provides progressively larger selections.
#[test]
fn test_selection_range_parent_chain() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create and open Lua test file
    let (uri, content, _temp_file) = create_selection_range_lua_fixture();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "lua",
                "version": 1,
                "text": content
            }
        }),
    );

    // Request selection range
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 0, "character": 0 }]
        }),
    );

    let result = response.get("result").unwrap();
    let ranges = result.as_array().unwrap();
    let first_range = &ranges[0];

    // Verify parent chain exists
    assert!(
        first_range.get("parent").is_some(),
        "SelectionRange should have parent for expansion: {:?}",
        first_range
    );

    // Walk parent chain and verify each level expands
    let mut current = first_range;
    let mut level = 1;
    let mut prev_text_len = 0;

    while let Some(parent) = current.get("parent") {
        let range = parent.get("range").unwrap();
        let text = extract_range_text(&content, range);

        // Each parent should have equal or larger selection
        assert!(
            text.len() >= prev_text_len,
            "Parent level {} should have larger or equal selection than level {}",
            level + 1,
            level
        );

        prev_text_len = text.len();
        current = parent;
        level += 1;

        // Prevent infinite loops
        if level > 20 {
            break;
        }
    }

    // Should have multiple levels of expansion
    assert!(
        level > 1,
        "Should have at least 2 levels of selection expansion"
    );
}

/// Test selection range on markdown with injections.
///
/// Based on test_lsp_select.lua tests for assets/example.md
/// Tests YAML frontmatter expansion, Lua code blocks, and nested injections.
#[test]
fn test_selection_range_markdown_with_injections() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create and open markdown test file
    let (uri, content, _temp_file) = create_selection_range_md_fixture();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": content
            }
        }),
    );

    // Test YAML frontmatter: line 1 (0-indexed), col 0 - "title" keyword
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 1, "character": 0 }]
        }),
    );

    let result = response.get("result").unwrap();
    let ranges = result.as_array().unwrap();
    assert!(!ranges.is_empty(), "Should have selection range for YAML");

    let first_range = &ranges[0];
    let range = first_range.get("range").unwrap();
    let selected_text = extract_range_text(&content, range);

    assert!(
        selected_text.contains("title"),
        "YAML selection should contain 'title', got: '{}'",
        selected_text
    );

    // Test Lua code block: line 6 (0-indexed), col 0 - "local" keyword
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 6, "character": 0 }]
        }),
    );

    let result = response.get("result").unwrap();
    let ranges = result.as_array().unwrap();
    assert!(
        !ranges.is_empty(),
        "Should have selection range for Lua code block"
    );

    let first_range = &ranges[0];
    let range = first_range.get("range").unwrap();
    let selected_text = extract_range_text(&content, range);

    assert!(
        selected_text.contains("local"),
        "Lua code selection should contain 'local', got: '{}'",
        selected_text
    );
}

/// Test selection range snapshot for deterministic testing.
///
/// Captures the structure of SelectionRange response for future comparison.
#[test]
fn test_selection_range_snapshot() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create and open Lua test file
    let (uri, content, _temp_file) = create_selection_range_lua_fixture();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "lua",
                "version": 1,
                "text": content
            }
        }),
    );

    // Request selection range
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 0, "character": 0 }]
        }),
    );

    let result = response.get("result").unwrap();

    // Sanitize for snapshot testing
    let sanitized = sanitize_selection_range_response(result);

    // Capture snapshot
    insta::assert_json_snapshot!("selection_range_lua", sanitized);
}

/// Test selection range with multiple positions.
///
/// SelectionRange can accept multiple positions and returns ranges for each.
#[test]
fn test_selection_range_multiple_positions() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create and open Lua test file
    let (uri, _content, _temp_file) = create_selection_range_lua_fixture();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "lua",
                "version": 1,
                "text": _content
            }
        }),
    );

    // Request selection ranges for multiple positions
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [
                { "line": 0, "character": 0 },  // "local" on line 1
                { "line": 2, "character": 0 },  // "function" on line 3
            ]
        }),
    );

    let result = response.get("result").unwrap();
    let ranges = result.as_array().unwrap();

    // Should return one SelectionRange per position
    assert_eq!(
        ranges.len(),
        2,
        "Should return selection range for each position"
    );

    // Both should have valid range and parent
    for (i, range) in ranges.iter().enumerate() {
        assert!(
            range.get("range").is_some(),
            "SelectionRange {} should have range field",
            i
        );
    }
}

#[test]
fn e2e_selection_range_routes_each_position_to_its_virtual_or_native_layer() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("selection_range_virt.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/selectionRange"]
priorities = ["virt", "native"]
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "languageServers": {
                    "mock-selection-range": {
                        "cmd": [mock_bin(), "selection-range-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///selection_range_layers.md";
    let text = "before\n\n> ```lua\n> code\n> ```\n";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": text
            }
        }),
    );

    let response = poll_until(100, 50, || {
        let response = client.send_request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": uri },
                "positions": [
                    { "line": 0, "character": 1 },
                    { "line": 3, "character": 3 }
                ]
            }),
        );
        (response.pointer("/result/1/range/start") == Some(&json!({ "line": 3, "character": 3 })))
            .then_some(response)
    })
    .expect("virtual selection range should become available");
    let ranges = response["result"].as_array().expect("aligned result array");
    assert_eq!(ranges.len(), 2, "one result per requested position");
    assert_eq!(
        ranges[1],
        json!({
            "range": {
                "start": { "line": 3, "character": 3 },
                "end": { "line": 3, "character": 4 }
            },
            "parent": {
                "range": {
                    "start": { "line": 3, "character": 2 },
                    "end": { "line": 3, "character": 6 }
                }
            }
        }),
        "the embedded position should use the rebased virtual-server chain"
    );
    assert!(
        ranges[0]["range"]["start"]["line"].as_u64().unwrap_or(1) == 0,
        "the position outside every injection should retain its native aligned result"
    );
}

#[test]
fn e2e_selection_range_uses_the_host_layer_for_the_real_document() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("selection_range_host.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/selectionRange"]
priorities = ["host", "native"]

[languages.markdown.bridge._self]
enabled = true
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "languageServers": {
                    "mock-selection-range-host": {
                        "cmd": [mock_bin(), "selection-range-host"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///selection_range_host.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "word\n"
            }
        }),
    );

    let response = poll_until(100, 50, || {
        let response = client.send_request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": uri },
                "positions": [{ "line": 0, "character": 1 }]
            }),
        );
        (response.pointer("/result/0/range/start/character") == Some(&json!(1))).then_some(response)
    })
    .expect("host selection range should become available");
    assert_eq!(
        response["result"][0],
        json!({
            "range": {
                "start": { "line": 0, "character": 1 },
                "end": { "line": 0, "character": 2 }
            },
            "parent": {
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 4 }
                }
            }
        })
    );
}
