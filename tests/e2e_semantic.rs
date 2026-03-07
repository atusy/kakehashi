//! End-to-end tests for semantic tokens.
//!
//! Verifies that semantic token highlighting works correctly for both
//! plain files and files with injections.
//!
//! Based on tests/test_lsp_semantic.lua which tests:
//! - Lua file: keyword token at line 0, col 1 ("local")
//! - Markdown file: injected Lua keyword at line 6, col 1 ("local")
//!
//! Unlike other E2E tests, we decode the semantic tokens for validation
//! because the LSP encoding is complex (delta-encoded positions).
//!
//! Run with: `cargo test --test e2e_semantic --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lsp_polling::poll_for_semantic_tokens;
use helpers::test_fixtures::{
    create_selection_range_lua_fixture, create_selection_range_md_fixture,
};
use serde_json::{Value, json};

/// Represents a decoded semantic token with absolute positions.
///
/// This is defined locally to avoid coupling tests to internal implementation.
#[derive(Debug, Clone, PartialEq)]
struct DecodedToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    token_modifiers_bitset: u32,
}

/// Decode LSP semantic tokens from delta-encoded to absolute positions.
///
/// Semantic tokens are encoded as 5-element tuples (delta_line, delta_start, length, token_type, modifiers).
/// This function converts them to absolute line/column positions for easier validation.
fn decode_semantic_tokens(data: &[u32]) -> Vec<DecodedToken> {
    let mut result = Vec::new();
    let mut current_line = 0u32;
    let mut current_col = 0u32;

    // Process tokens in chunks of 5
    for chunk in data.chunks_exact(5) {
        let delta_line = chunk[0];
        let delta_start = chunk[1];
        let length = chunk[2];
        let token_type = chunk[3];
        let token_modifiers_bitset = chunk[4];

        current_line += delta_line;
        if delta_line > 0 {
            current_col = delta_start;
        } else {
            current_col += delta_start;
        }

        result.push(DecodedToken {
            line: current_line,
            start: current_col,
            length,
            token_type,
            token_modifiers_bitset,
        });
    }

    result
}

/// Get token type name from index based on LEGEND_TYPES order.
///
/// Matches src/analysis/semantic.rs:LEGEND_TYPES
fn token_type_name(index: u32) -> &'static str {
    match index {
        0 => "comment",
        1 => "keyword",
        2 => "string",
        3 => "number",
        4 => "regexp",
        5 => "operator",
        6 => "namespace",
        7 => "type",
        8 => "struct",
        9 => "class",
        10 => "interface",
        11 => "enum",
        12 => "enumMember",
        13 => "typeParameter",
        14 => "function",
        15 => "method",
        16 => "macro",
        17 => "variable",
        18 => "parameter",
        19 => "property",
        20 => "label",
        21 => "decorator",
        _ => "unknown",
    }
}

/// Test semantic tokens for a plain Lua file.
///
/// Based on test_lsp_semantic.lua test for assets/example.lua
/// Verifies that "local" keyword at line 0, col 0 is tokenized as keyword.
#[test]
fn test_semantic_tokens_lua_keyword() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "dynamicRegistration": false,
                        "requests": {
                            "full": true
                        },
                        "tokenTypes": ["keyword", "variable", "function"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

    // Verify response
    assert!(
        response.get("result").is_some(),
        "Semantic tokens response should have result: {:?}",
        response
    );

    let result = response.get("result").unwrap();

    // Extract token data
    let data = result
        .get("data")
        .expect("Result should have data field")
        .as_array()
        .expect("Data should be array");

    let data_u32: Vec<u32> = data.iter().map(|v| v.as_u64().unwrap() as u32).collect();

    // Decode tokens
    let tokens = decode_semantic_tokens(&data_u32);

    assert!(!tokens.is_empty(), "Should have at least one token");

    // Find keyword token at line 0, col 0 (the "local" keyword)
    let keyword_token = tokens.iter().find(|t| t.line == 0 && t.start == 0);

    assert!(
        keyword_token.is_some(),
        "Should find token at line 0, col 0: {:?}",
        tokens
    );

    let token = keyword_token.unwrap();
    let type_name = token_type_name(token.token_type);

    assert_eq!(
        type_name, "keyword",
        "Token at line 0, col 0 should be keyword type, got: {}",
        type_name
    );
}

/// Test semantic tokens for markdown with Lua injection.
///
/// Based on test_lsp_semantic.lua test for assets/example.md
/// Verifies that "local" keyword in injected Lua code block is tokenized.
#[test]
fn test_semantic_tokens_markdown_injection() {
    let mut client = LspClient::new();

    // Initialize server
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": ["keyword", "variable"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                }
            }
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

    // Verify response
    assert!(
        response.get("result").is_some(),
        "Semantic tokens response should have result"
    );

    let result = response.get("result").unwrap();

    // Extract token data
    let data = result
        .get("data")
        .expect("Result should have data field")
        .as_array()
        .expect("Data should be array");

    let data_u32: Vec<u32> = data.iter().map(|v| v.as_u64().unwrap() as u32).collect();

    // Decode tokens
    let tokens = decode_semantic_tokens(&data_u32);

    assert!(!tokens.is_empty(), "Should have tokens for markdown file");

    // The markdown file has a Lua code block at line 6 (0-indexed) with "local xyz = 12345"
    // Find keyword token at line 6 (the "local" keyword in injected Lua)
    let keyword_tokens: Vec<_> = tokens
        .iter()
        .filter(|t| t.line == 6 && token_type_name(t.token_type) == "keyword")
        .collect();

    assert!(
        !keyword_tokens.is_empty(),
        "Should find keyword token in injected Lua code block at line 6. All tokens: {:?}",
        tokens
            .iter()
            .map(|t| (t.line, t.start, token_type_name(t.token_type)))
            .collect::<Vec<_>>()
    );
}

/// Test semantic tokens snapshot with decoded representation.
///
/// Captures decoded token structure for deterministic testing.
#[test]
fn test_semantic_tokens_snapshot() {
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

    let result = response.get("result").unwrap();
    let data = result.get("data").unwrap().as_array().unwrap();
    let data_u32: Vec<u32> = data.iter().map(|v| v.as_u64().unwrap() as u32).collect();

    // Decode tokens for snapshot
    let tokens = decode_semantic_tokens(&data_u32);

    // Convert to JSON-serializable format with token type names
    let snapshot_tokens: Vec<Value> = tokens
        .iter()
        .map(|t| {
            json!({
                "line": t.line,
                "start": t.start,
                "length": t.length,
                "type": token_type_name(t.token_type),
                "modifiers": t.token_modifiers_bitset
            })
        })
        .collect();

    // Capture snapshot of decoded tokens
    insta::assert_json_snapshot!("semantic_tokens_lua_decoded", snapshot_tokens);
}

/// Test that semantic tokens include resultId for incremental updates.
#[test]
fn test_semantic_tokens_result_id() {
    let mut client = LspClient::new();

    // Initialize
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create and open test file
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

    let result = response.get("result").unwrap();

    // resultId should be present for incremental updates
    assert!(
        result.get("resultId").is_some(),
        "Semantic tokens should include resultId for incremental updates: {:?}",
        result
    );
}

/// Test semantic tokens for markdown with inline content (bold/italic).
///
/// Verifies that markdown_inline injection works and bold text gets
/// semantic tokens (tests the @markup.strong capture).
#[test]
fn test_semantic_tokens_markdown_inline_bold() {
    // Use debug mode to capture server logs
    let mut client = LspClient::with_debug(true);

    // Initialize server with custom capture mapping for markup.strong -> keyword
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": ["keyword", "variable"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                }
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.strong": "keyword",
                            "markup.heading.1": "class"
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create markdown file with bold text (uses markdown_inline injection)
    let content = r#"# Heading

**bold text**
"#;
    let temp_file = tempfile::Builder::new()
        .suffix(".md")
        .tempfile()
        .expect("Failed to create temp file");
    std::fs::write(temp_file.path(), content).expect("Failed to write temp file");
    let uri = url::Url::from_file_path(temp_file.path())
        .expect("Failed to construct file URI")
        .to_string();

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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

    // Verify response
    assert!(
        response.get("result").is_some(),
        "Semantic tokens response should have result"
    );

    let result = response.get("result").unwrap();
    let data = result
        .get("data")
        .expect("Result should have data field")
        .as_array()
        .expect("Data should be array");

    let data_u32: Vec<u32> = data.iter().map(|v| v.as_u64().unwrap() as u32).collect();
    let tokens = decode_semantic_tokens(&data_u32);

    // Print all tokens for debugging
    let token_info: Vec<_> = tokens
        .iter()
        .map(|t| {
            format!(
                "line={} start={} len={} type={}",
                t.line,
                t.start,
                t.length,
                token_type_name(t.token_type)
            )
        })
        .collect();
    eprintln!("All tokens: {:?}", token_info);

    // Drain and print server stderr for debugging
    let stderr_output = client.drain_stderr();
    if !stderr_output.is_empty() {
        eprintln!("Server stderr:\n{}", stderr_output);
    }

    // The bold text is on line 2 (0-indexed)
    // We expect to see a token for the bold text from markdown_inline injection
    // The capture @markup.strong should be mapped (by default it's suppressed with "")
    let line2_tokens: Vec<_> = tokens.iter().filter(|t| t.line == 2).collect();

    // We must have tokens on line 2 for markdown_inline injection to be working
    assert!(
        !line2_tokens.is_empty(),
        "Should have tokens on line 2 for **bold text** from markdown_inline injection. \
         All tokens: {:?}",
        token_info
    );
}
