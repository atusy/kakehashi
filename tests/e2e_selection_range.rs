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
//! Run with: `cargo test --test e2e_selection_range --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lsp_polling::poll_for_selection_range;
use helpers::sanitization::sanitize_selection_range_response;
use helpers::test_fixtures::{
    create_selection_range_lua_fixture, create_selection_range_md_fixture,
};
use serde_json::{Value, json};

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
    let positions = vec![json!({"line": 0, "character": 0})];
    let response = poll_for_selection_range(&mut client, &uri, &positions, 20, 100)
        .expect("Should receive selection range");

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

    let positions = vec![json!({"line": 0, "character": 0})];
    let response = poll_for_selection_range(&mut client, &uri, &positions, 20, 100)
        .expect("Should receive selection range");

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
    let positions = vec![json!({"line": 1, "character": 0})];
    let response = poll_for_selection_range(&mut client, &uri, &positions, 20, 100)
        .expect("Should receive selection range");

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
    let response = client.send_request(
        "textDocument/selectionRange",
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

    let positions = vec![json!({"line": 0, "character": 0})];
    let response = poll_for_selection_range(&mut client, &uri, &positions, 20, 100)
        .expect("Should receive selection range");

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
    let positions = vec![
        json!({"line": 0, "character": 0}), // "local" on line 1
        json!({"line": 2, "character": 0}), // "function" on line 3
    ];
    let response = poll_for_selection_range(&mut client, &uri, &positions, 20, 100)
        .expect("Should receive selection range");

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
