//! End-to-end test for semantic tokens in blockquote-wrapped code blocks.
//!
//! Verifies that `> ```lua` code blocks produce correct injection tokens
//! on all content lines, and that `> ` prefixes don't leak into the injection
//! parser or suppress host tokens.
//!
//! Run with: `cargo test --features e2e --test e2e_semantic_blockquote`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;
use std::time::Duration;

/// Decoded semantic token with absolute positions.
#[derive(Debug, Clone, PartialEq)]
struct DecodedToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
}

/// Decode delta-encoded LSP semantic tokens to absolute positions.
fn decode_semantic_tokens(data: &[u32]) -> Vec<DecodedToken> {
    let mut result = Vec::new();
    let mut current_line = 0u32;
    let mut current_col = 0u32;

    for chunk in data.chunks_exact(5) {
        let delta_line = chunk[0];
        let delta_start = chunk[1];
        let length = chunk[2];
        let token_type = chunk[3];

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
        });
    }

    result
}

/// Get token type name from index (partial mapping of LEGEND_TYPES).
///
/// This covers only 9 of the 23 entries in `LEGEND_TYPES` (see `legend.rs`) —
/// the subset asserted in this test. Unmapped indices return `"other"`.
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
        17 => "variable",
        _ => "other",
    }
}

/// E2E test: blockquote-wrapped fenced code blocks produce consistent tokens.
///
/// Tests that both content lines in a blockquote Lua code block:
/// 1. Have identical token sequences (same types, same columns)
/// 2. Have `keyword` tokens for `local` at the correct column (after `> `)
/// 3. Have no host `string` tokens leaking inside the injection region
#[test]
fn test_blockquote_injection_tokens() {
    let mut client = LspClient::new();

    // Initialize with default captureMappings
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": ["keyword", "variable", "string", "number", "operator"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Create a temp markdown file with blockquote code block
    let content = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
    let temp_file = tempfile::Builder::new()
        .suffix(".md")
        .tempfile()
        .expect("Failed to create temp file");
    std::fs::write(temp_file.path(), content).expect("Failed to write temp file");
    let uri = url::Url::from_file_path(temp_file.path())
        .expect("Failed to construct URI")
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

    // Give server time to load parsers and process
    std::thread::sleep(Duration::from_millis(1000));

    // Request semantic tokens
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({
            "textDocument": { "uri": uri }
        }),
    );

    let result = response
        .get("result")
        .expect("Should have result in response");
    let data = result
        .get("data")
        .expect("Result should have data")
        .as_array()
        .expect("Data should be array");
    let data_u32: Vec<u32> = data.iter().map(|v| v.as_u64().unwrap() as u32).collect();
    let tokens = decode_semantic_tokens(&data_u32);

    assert!(!tokens.is_empty(), "Should have semantic tokens");

    // Lines:
    //   0: "> ```lua"
    //   1: "> local x = 1"
    //   2: "> local y = 2"
    //   3: "> ```"

    let line1_tokens: Vec<_> = tokens.iter().filter(|t| t.line == 1).collect();
    let line2_tokens: Vec<_> = tokens.iter().filter(|t| t.line == 2).collect();

    // Both content lines should have tokens
    assert!(
        !line1_tokens.is_empty(),
        "Line 1 should have tokens. All: {:?}",
        tokens
    );
    assert!(
        !line2_tokens.is_empty(),
        "Line 2 should have tokens. All: {:?}",
        tokens
    );

    // Both lines should have `keyword` for `local` at col 2 (after `> `)
    let line1_keywords: Vec<_> = line1_tokens
        .iter()
        .filter(|t| token_type_name(t.token_type) == "keyword")
        .collect();
    let line2_keywords: Vec<_> = line2_tokens
        .iter()
        .filter(|t| token_type_name(t.token_type) == "keyword")
        .collect();

    assert!(
        !line1_keywords.is_empty(),
        "Line 1 should have keyword for 'local'. Tokens: {:?}",
        line1_tokens
    );
    assert!(
        !line2_keywords.is_empty(),
        "Line 2 should have keyword for 'local'. Tokens: {:?}",
        line2_tokens
    );

    // Keyword should be at absolute column 2 (after `> ` prefix)
    assert_eq!(
        line1_keywords[0].start, 2,
        "Keyword 'local' should start at column 2 (after `> `)"
    );
    // Keyword columns should match (both after `> `)
    assert_eq!(
        line1_keywords[0].start, line2_keywords[0].start,
        "Keyword columns should match on both lines"
    );

    // Token sequences (col, len, type) should be identical on both lines
    let line1_sig: Vec<_> = line1_tokens
        .iter()
        .map(|t| (t.start, t.length, t.token_type))
        .collect();
    let line2_sig: Vec<_> = line2_tokens
        .iter()
        .map(|t| (t.start, t.length, t.token_type))
        .collect();
    assert_eq!(
        line1_sig, line2_sig,
        "Both content lines should produce identical token sequences"
    );

    // No host `string` tokens inside the injection region (after col 2)
    let string_type = 2u32;
    let line1_string_leaks: Vec<_> = line1_tokens
        .iter()
        .filter(|t| t.token_type == string_type && t.start >= 2)
        .collect();
    let line2_string_leaks: Vec<_> = line2_tokens
        .iter()
        .filter(|t| t.token_type == string_type && t.start >= 2)
        .collect();

    assert!(
        line1_string_leaks.is_empty(),
        "No host string tokens should leak inside injection on line 1. Leaks: {:?}",
        line1_string_leaks
    );
    assert!(
        line2_string_leaks.is_empty(),
        "No host string tokens should leak inside injection on line 2. Leaks: {:?}",
        line2_string_leaks
    );

    // The `> ` prefix at col 0 should HAVE a host `string` token (token splitting preserves it)
    let line1_prefix: Vec<_> = line1_tokens
        .iter()
        .filter(|t| t.start == 0 && t.token_type == string_type)
        .collect();
    let line2_prefix: Vec<_> = line2_tokens
        .iter()
        .filter(|t| t.start == 0 && t.token_type == string_type)
        .collect();
    assert!(
        !line1_prefix.is_empty(),
        "Line 1 should have host `string` for `> ` prefix. Line 1 tokens: {:?}",
        line1_tokens
    );
    assert!(
        !line2_prefix.is_empty(),
        "Line 2 should have host `string` for `> ` prefix. Line 2 tokens: {:?}",
        line2_tokens
    );
}
