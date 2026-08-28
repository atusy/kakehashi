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

#[test]
fn e2e_native_selection_range_defaults_overlong_positions() {
    let mut client = LspClient::new();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
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

    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 0, "character": 999 }]
        }),
    );
    let range = &response["result"][0]["range"];
    let start = (
        range["start"]["line"].as_u64().expect("start line"),
        range["start"]["character"]
            .as_u64()
            .expect("start character"),
    );
    let end = (
        range["end"]["line"].as_u64().expect("end line"),
        range["end"]["character"].as_u64().expect("end character"),
    );
    let defaulted = (0, 12);
    assert!(
        (start == defaulted && end == defaulted) || (start <= defaulted && defaulted < end),
        "native range must contain the defaulted line-end position: {response:?}"
    );

    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 999, "character": 999 }]
        }),
    );
    let range = &response["result"][0]["range"];
    let start = (
        range["start"]["line"].as_u64().expect("start line"),
        range["start"]["character"]
            .as_u64()
            .expect("start character"),
    );
    let end = (
        range["end"]["line"].as_u64().expect("end line"),
        range["end"]["character"].as_u64().expect("end character"),
    );
    let eof = (
        content.matches('\n').count() as u64,
        content
            .rsplit('\n')
            .next()
            .expect("at least one line")
            .encode_utf16()
            .count() as u64,
    );
    assert!(
        start <= eof && eof <= end,
        "native range must contain the defaulted EOF position: {response:?}"
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
priorities = ["host"]

[languages.markdown.bridge._self]
enabled = true
"#,
    )
    .expect("write config");
    let events = tempfile::TempDir::new().expect("events");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("MOCK_LSP_CANCEL_DIR", events.path().to_string_lossy())
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
    assert!(
        events
            .path()
            .join("selection-range-host.request.json")
            .exists(),
        "the host document must reach the downstream server"
    );
}

#[test]
fn e2e_empty_host_is_not_retried_before_native_fallback() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("selection_range_host_empty.toml");
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
    let events = tempfile::TempDir::new().expect("events");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("MOCK_LSP_CANCEL_DIR", events.path().to_string_lossy())
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "languageServers": {
                    "mock-selection-range-empty": {
                        "cmd": [mock_bin(), "selection-range-empty"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///selection_range_host_empty.md";
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

    let marker = events.path().join("selection-range-empty.request.json");
    poll_until(100, 50, || {
        let _ = client.send_request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": uri },
                "positions": [{ "line": 0, "character": 1 }]
            }),
        );
        marker.exists().then_some(())
    })
    .expect("empty host server should become ready");
    std::fs::write(
        events.path().join("selection-range-empty.request.count"),
        "0",
    )
    .expect("reset request count");

    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 0, "character": 1 }]
        }),
    );
    assert!(response.pointer("/result/0/range").is_some());
    assert_eq!(
        std::fs::read_to_string(events.path().join("selection-range-empty.request.count"))
            .expect("request count"),
        "1",
        "the empty highest-priority host layer must not be dispatched twice"
    );
}

#[test]
fn e2e_host_first_reuses_mixed_results_per_position() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("selection_range_host_mixed.toml");
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
    let events = tempfile::TempDir::new().expect("events");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("MOCK_LSP_CANCEL_DIR", events.path().to_string_lossy())
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "languageServers": {
                    "mock-selection-range-mixed": {
                        "cmd": [mock_bin(), "selection-range-mixed-empty"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///selection_range_host_mixed.md";
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

    let marker = events
        .path()
        .join("selection-range-mixed-empty.request.json");
    poll_until(100, 50, || {
        let _ = client.send_request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": uri },
                "positions": [{ "line": 0, "character": 1 }]
            }),
        );
        marker.exists().then_some(())
    })
    .expect("mixed host server should become ready");
    std::fs::write(
        events
            .path()
            .join("selection-range-mixed-empty.request.count"),
        "0",
    )
    .expect("reset request count");

    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [
                { "line": 0, "character": 1 },
                { "line": 0, "character": 3 }
            ]
        }),
    );
    assert_eq!(
        response.pointer("/result/1/range/start/character"),
        Some(&json!(3))
    );
    assert_eq!(
        response.pointer("/result/1/range/end/character"),
        Some(&json!(4))
    );
    assert_eq!(
        std::fs::read_to_string(
            events
                .path()
                .join("selection-range-mixed-empty.request.count")
        )
        .expect("request count"),
        "2",
        "each host position must be dispatched once and its result reused"
    );
}

#[test]
fn e2e_selection_range_falls_back_to_native_for_incapable_or_empty_virtual_servers() {
    fn run(mode: Option<&str>, event_dir: &std::path::Path) -> Value {
        let config_dir = tempfile::TempDir::new().expect("config dir");
        let config_path = config_dir.path().join("selection_range_fallback.toml");
        let priorities = if mode.is_some() {
            r#"[languages.markdown.layers.aggregation."textDocument/selectionRange"]
priorities = ["virt", "native"]
"#
        } else {
            r#"[languages.markdown.layers.aggregation."textDocument/selectionRange"]
priorities = ["native"]
"#
        };
        std::fs::write(&config_path, priorities).expect("write config");
        let mut client = LspClient::builder()
            .arg("--config-file")
            .arg(config_path.to_str().expect("UTF-8 config path"))
            .env("MOCK_LSP_CANCEL_DIR", event_dir.to_string_lossy())
            .build();
        let language_servers = mode.map_or_else(
            || json!({}),
            |mode| {
                json!({
                    "mock-selection-range-fallback": {
                        "cmd": [mock_bin(), mode],
                        "languages": ["lua"]
                    }
                })
            },
        );
        client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {},
                "initializationOptions": { "languageServers": language_servers }
            }),
        );
        client.send_notification("initialized", json!({}));
        let uri = "file:///selection_range_fallback.md";
        client.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "markdown",
                    "version": 1,
                    "text": "```lua\nlocal value = 1\n```\n"
                }
            }),
        );
        selection_range_when_parsed(
            &mut client,
            json!({
                "textDocument": { "uri": uri },
                "positions": [{ "line": 1, "character": 7 }]
            }),
        )["result"]
            .clone()
    }

    let native_events = tempfile::TempDir::new().expect("native events");
    let expected_native = run(None, native_events.path());
    for mode in ["selection-range-disabled", "selection-range-empty"] {
        let events = tempfile::TempDir::new().expect("events");
        let actual = run(Some(mode), events.path());
        assert_eq!(
            actual, expected_native,
            "{mode} must preserve the exact native selection hierarchy"
        );
        let marker = events.path().join(format!("{mode}.request.json"));
        if mode == "selection-range-disabled" {
            assert!(
                !marker.exists(),
                "an incapable server must not be dispatched"
            );
        } else {
            assert!(marker.exists(), "the capable empty server should be tried");
        }
    }
}

#[test]
fn e2e_selection_range_skips_non_contiguous_combined_injections() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("selection_range_combined.toml");
    std::fs::write(&config_path, "").expect("write config");
    let query_path = config_dir.path().join("combined-injections.scm");
    std::fs::write(
        &query_path,
        r#"
        (fenced_code_block
          (info_string (language) @injection.language)
          (code_fence_content) @injection.content
          (#set! injection.combined)
          (#set! injection.include-children))
        "#,
    )
    .expect("write injection query");
    let events = tempfile::TempDir::new().expect("events");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("MOCK_LSP_CANCEL_DIR", events.path().to_string_lossy())
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
                },
                "languages": {
                    "markdown": {
                        "queries": [{
                            "path": query_path.to_str().expect("UTF-8 query path"),
                            "kind": "injections"
                        }],
                        "layers": { "aggregation": {
                            "textDocument/selectionRange": {
                                "priorities": ["virt", "native"]
                            }
                        }}
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let warmup_uri = "file:///selection_range_combined_warmup.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": warmup_uri,
                "languageId": "markdown",
                "version": 1,
                "text": "```lua\ncode\n```\n"
            }
        }),
    );
    let marker = events.path().join("selection-range-virt.request.json");
    poll_until(100, 50, || {
        let _ = client.send_request(
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": warmup_uri },
                "positions": [{ "line": 1, "character": 1 }]
            }),
        );
        marker.exists().then_some(())
    })
    .expect("a contiguous virtual request should reach the ready server");
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": warmup_uri } }),
    );
    std::fs::remove_file(&marker).expect("clear warmup marker");
    let uri = "file:///selection_range_combined.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "```lua\nfirst\n```\ngap\n```lua\nsecond\n```\n"
            }
        }),
    );
    let response = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 5, "character": 2 }]
        }),
    );
    assert!(
        response["result"].is_array(),
        "native fallback should answer"
    );
    assert!(
        !marker.exists(),
        "a non-contiguous combined virtual document must not be dispatched"
    );
}

#[test]
fn e2e_selection_range_rejects_a_virtual_response_after_content_changes() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("selection_range_stale.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/selectionRange"]
priorities = ["virt"]
"#,
    )
    .expect("write config");
    let events = tempfile::TempDir::new().expect("events");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("MOCK_LSP_CANCEL_DIR", events.path().to_string_lossy())
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_CHANGE_DIR",
            events.path().to_string_lossy(),
        )
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
                        "cmd": [mock_bin(), "selection-range-delayed"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///selection_range_stale.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "```lua\nold!\n```\n"
            }
        }),
    );

    // Polling establishes the parse/virtual document before starting the one
    // deliberately delayed request.
    std::fs::write(
        events.path().join("selection-range-delayed.release"),
        b"release",
    )
    .expect("release warmup");
    let _ = selection_range_when_parsed(
        &mut client,
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 1, "character": 1 }]
        }),
    );
    std::fs::remove_file(events.path().join("selection-range-delayed.release"))
        .expect("re-arm delay");
    let marker = events.path().join("selection-range-delayed.request.json");
    std::fs::remove_file(&marker).expect("clear warmup marker");
    let request_id = client.send_request_async(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 1, "character": 1 }]
        }),
    );
    assert!(
        (0..60).any(|_| {
            if marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the delayed request should reach the virtual server"
    );
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "```lua\nchanged\n```\n" }]
        }),
    );
    assert!(
        (0..60).any(|_| {
            if events.path().join("changed").exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "didChange should be applied while the response is delayed"
    );
    std::fs::write(
        events.path().join("selection-range-delayed.release"),
        b"release",
    )
    .expect("release delayed response");
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(-32801),
        "a response authored against old content must be rejected: {response:?}"
    );
}
