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
use helpers::lsp_polling::poll_for_semantic_tokens;
use kakehashi::text::convert_utf16_to_byte_in_line;
use serde_json::json;

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

/// Get token type name from index (full mapping of LEGEND_TYPES from `legend.rs`).
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
        20 => "event",
        21 => "modifier",
        22 => "decorator",
        _ => "unknown",
    }
}

/// Build a JSON snapshot of decoded tokens with human-readable text slices.
///
/// Each token entry includes: line, start, length, type name, and the source text
/// that the token covers. This makes before/after diffs highly informative.
fn build_token_snapshot(tokens: &[DecodedToken], content: &str) -> Vec<serde_json::Value> {
    let lines: Vec<&str> = content.lines().collect();
    tokens
        .iter()
        .map(|t| {
            let text = lines
                .get(t.line as usize)
                .and_then(|line| {
                    let start_byte = convert_utf16_to_byte_in_line(line, t.start as usize)?;
                    let end_byte =
                        convert_utf16_to_byte_in_line(line, (t.start + t.length) as usize)?;
                    line.get(start_byte..end_byte)
                })
                .unwrap_or("<out-of-range>");
            json!({
                "line": t.line,
                "start": t.start,
                "length": t.length,
                "type": token_type_name(t.token_type),
                "text": text,
            })
        })
        .collect()
}

/// E2E test: blockquote multiline injection with full captureMappings.
///
/// Uses a comprehensive captureMappings config (matching typical user configs) with
/// both heading and code block examples in a single document.
#[test]
fn test_blockquote_multiline_support_with_full_config() {
    let mut client = LspClient::new();

    // User's EXACT captureMappings config
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
                        "formats": ["relative"],
                        "multilineTokenSupport": true
                    }
                }
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.heading": "class",
                            "markup.heading.1": "class",
                            "markup.heading.2": "class",
                            "markup.heading.3": "class",
                            "markup.heading.4": "class",
                            "markup.heading.5": "class",
                            "markup.heading.6": "class",
                            "markup.italic": "keyword",
                            "markup.link.label": "keyword",
                            "markup.link.url": "",
                            "markup.link": "",
                            "markup.list.checked": "property",
                            "markup.list.unchecked": "property",
                            "markup.list": "property",
                            "markup.math": "",
                            "markup.quote": "keyword",
                            "markup.raw.block": "string",
                            "markup.raw": "string",
                            "markup.strikethrough": "keyword.deprecated",
                            "markup.strong": "keyword",
                            "markup.underline": "keyword"
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Both examples in one document, matching user's screenshot
    let content = "> # foo\n> bar\n\n> ```py\n> \"\"\"\n>   foo\n> \"\"\"\n> ```\n";
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

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

    // Lines:
    //   0: "> # foo"
    //   1: "> bar"
    //   2: ""
    //   3: "> ```py"
    //   4: "> """
    //   5: ">   foo"
    //   6: "> """
    //   7: "> ```"

    // Check code block content lines (4-6): `> ` prefix should have a host token
    for line_num in 4..=6 {
        let line_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.line == line_num as u32)
            .collect();
        assert!(
            !line_tokens.is_empty(),
            "Line {} should have tokens. All: {:?}",
            line_num,
            tokens
        );

        // The `> ` prefix should have a host "keyword" token at col 0
        let has_keyword_prefix = line_tokens
            .iter()
            .any(|t| t.start == 0 && token_type_name(t.token_type) == "keyword");
        assert!(
            has_keyword_prefix,
            "Line {} should have a 'keyword' token at col 0 for `> ` prefix. Tokens: {:?}",
            line_num, line_tokens
        );

        // No injection "string" tokens should cover the `> ` prefix (col 0-1)
        let bad_tokens: Vec<_> = line_tokens
            .iter()
            .filter(|t| t.start == 0 && token_type_name(t.token_type) == "string")
            .collect();
        assert!(
            bad_tokens.is_empty(),
            "Line {} should NOT have 'string' tokens at col 0 (prefix leak). Bad: {:?}",
            line_num,
            bad_tokens
        );
    }
}

/// E2E test: multiline Python string in blockquote preserves host prefix tokens.
///
/// When a Python `"""..."""` triple-quoted string spans multiple lines inside a
/// blockquote, the injection tokens on continuation lines must NOT start before
/// column 2 (the `> ` prefix). Each content line should have a host token at
/// col 0 (the `> ` prefix) and injection tokens starting at col 2 or later.
#[test]
fn test_blockquote_multiline_injection_token_prefix() {
    let mut client = LspClient::new();

    // captureMappings: markup.quote → "keyword" so the `> ` prefix produces host tokens,
    // markup.raw.block and markup.raw → "string" for the fenced code block itself.
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
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.quote": "keyword",
                            "markup.raw.block": "string",
                            "markup.raw": "string"
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Python triple-quoted string spanning 3 lines inside a blockquote:
    //   Line 0: > ```py
    //   Line 1: > """
    //   Line 2: >   foo
    //   Line 3: > """
    //   Line 4: > ```
    let content = "> ```py\n> \"\"\"\n>   foo\n> \"\"\"\n> ```\n";
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

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

    // Lines 1-3 each should have a host token at col 0 (the `> ` prefix, mapped to "keyword")
    for line_num in 1..=3 {
        let line_tokens: Vec<_> = tokens.iter().filter(|t| t.line == line_num).collect();
        assert!(
            !line_tokens.is_empty(),
            "Line {} should have tokens. All: {:?}",
            line_num,
            tokens
        );

        let has_prefix_token = line_tokens
            .iter()
            .any(|t| t.start == 0 && token_type_name(t.token_type) == "keyword");
        assert!(
            has_prefix_token,
            "Line {} should have a host 'keyword' token at col 0 for `> ` prefix. Tokens: {:?}",
            line_num, line_tokens
        );
    }

    // No injection tokens should start before col 2 on lines 1-3
    // (injection tokens have depth >= 1, but LSP doesn't expose depth,
    // so we check that no "string" tokens start at col 0 or 1)
    for line_num in 1..=3 {
        let line_tokens: Vec<_> = tokens.iter().filter(|t| t.line == line_num).collect();
        let bad_injection_tokens: Vec<_> = line_tokens
            .iter()
            .filter(|t| t.start < 2 && token_type_name(t.token_type) == "string")
            .collect();
        assert!(
            bad_injection_tokens.is_empty(),
            "Line {} should have no injection 'string' tokens before col 2. Bad tokens: {:?}",
            line_num,
            bad_injection_tokens
        );
    }
}

/// E2E test: heading in blockquote preserves host tokens on all lines.
///
/// Tests `> # foo\n> bar\n` — the heading on line 0 creates a markdown_inline
/// injection, and the continuation line should still have proper host tokens.
#[test]
fn test_blockquote_heading_host_token_preservation() {
    let mut client = LspClient::new();

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
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.heading.1": "class",
                            "markup.quote": "keyword"
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Content: blockquote with heading
    //   Line 0: > # foo
    //   Line 1: > bar
    let content = "> # foo\n> bar\n";
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

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

    // Line 0 should have tokens (heading and/or blockquote prefix)
    let line0_tokens: Vec<_> = tokens.iter().filter(|t| t.line == 0).collect();
    assert!(
        !line0_tokens.is_empty(),
        "Line 0 should have tokens. All: {:?}",
        tokens
    );

    // Line 1 should have a host token at col 0 (the `> ` prefix, mapped to "keyword")
    let line1_tokens: Vec<_> = tokens.iter().filter(|t| t.line == 1).collect();
    assert!(
        !line1_tokens.is_empty(),
        "Line 1 should have tokens. All: {:?}",
        tokens
    );

    // Without multilineTokenSupport, the heading is single-line (trailing newline case),
    // so it doesn't extend to line 1. The blockquote `markup.quote → "keyword"` covers
    // the entire line including the `> ` prefix.
    let line1_has_prefix = line1_tokens.iter().any(|t| t.start == 0);
    assert!(
        line1_has_prefix,
        "Line 1 should have a host token at col 0 for `> ` prefix. Tokens: {:?}",
        line1_tokens
    );
}

/// E2E test: heading in blockquote with multilineTokenSupport preserves prefix type.
///
/// When `multilineTokenSupport: true`, the `atx_heading` node spans lines 0-1 and
/// gets emitted as a single multiline token. Without proper prefix width handling at
/// the HOST level, `split_multiline_tokens` starts continuation lines at col 0,
/// causing the heading "class" token (priority 100) to overwrite `markup.quote`
/// "keyword" (priority 90) on the `> ` prefix of line 1.
///
/// The correct behavior: line 1 col 0 should be "keyword" (from `markup.quote`),
/// because the heading content doesn't include the `> ` prefix.
#[test]
fn test_blockquote_heading_multiline_support_prefix_type() {
    let mut client = LspClient::new();

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
                        "formats": ["relative"],
                        "multilineTokenSupport": true
                    }
                }
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.heading.1": "class",
                            "markup.quote": "keyword"
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    // Content: blockquote with heading
    //   Line 0: > # foo
    //   Line 1: > bar
    let content = "> # foo\n> bar\n";
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

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

    // Line 1 tokens
    let line1_tokens: Vec<_> = tokens.iter().filter(|t| t.line == 1).collect();
    assert!(
        !line1_tokens.is_empty(),
        "Line 1 should have tokens. All: {:?}",
        tokens
    );

    // The `> ` prefix on line 1 MUST be "keyword" (from markup.quote, priority 90),
    // NOT "class" (from markup.heading.1, priority 100).
    // The heading content should not extend into the `> ` prefix region.
    let prefix_token = line1_tokens
        .iter()
        .find(|t| t.start == 0)
        .expect("Line 1 should have a token at col 0");
    assert_eq!(
        token_type_name(prefix_token.token_type),
        "keyword",
        "Line 1 col 0 should be 'keyword' (markup.quote), not 'class' (heading). \
         The heading multiline token should not cover the `> ` prefix. Tokens: {:?}",
        line1_tokens
    );
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

    // Initialize with explicit captureMappings to ensure deterministic output
    // regardless of user's ~/.config/kakehashi/kakehashi.toml settings.
    //
    // `markup.quote → "string"` makes the `block_quote` node produce a `string`
    // token, enabling us to verify that `finalize_tokens` correctly SPLITS the
    // multiline host token at the injection boundary (preserving the `> ` prefix
    // at col=0 as a `string` while suppressing the col=2+ portion inside injection).
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
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.quote": "string"
                        }
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

    let response = poll_for_semantic_tokens(&mut client, &uri, 20, 200)
        .expect("Should receive semantic tokens");

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

// ─── Snapshot tests for observing token output before/after fixes ────────────

/// Helper: initialize client with full captureMappings config and multilineTokenSupport.
fn init_client_with_full_config(client: &mut LspClient) {
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
                        "formats": ["relative"],
                        "multilineTokenSupport": true
                    }
                }
            },
            "initializationOptions": {
                "captureMappings": {
                    "_": {
                        "highlights": {
                            "markup.heading": "class",
                            "markup.heading.1": "class",
                            "markup.heading.2": "class",
                            "markup.heading.3": "class",
                            "markup.heading.4": "class",
                            "markup.heading.5": "class",
                            "markup.heading.6": "class",
                            "markup.italic": "keyword",
                            "markup.link.label": "keyword",
                            "markup.link.url": "",
                            "markup.link": "",
                            "markup.list.checked": "property",
                            "markup.list.unchecked": "property",
                            "markup.list": "property",
                            "markup.math": "",
                            "markup.quote": "keyword",
                            "markup.raw.block": "string",
                            "markup.raw": "string",
                            "markup.strikethrough": "keyword.deprecated",
                            "markup.strong": "keyword",
                            "markup.underline": "keyword"
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
}

/// Helper: open a markdown document and return raw delta-encoded `[u32]` data.
///
/// Returns the raw LSP semantic token data without decoding — useful for
/// binary-level snapshot tests that track exact delta-encoded changes.
fn open_and_get_raw_data(client: &mut LspClient, content: &str) -> Vec<u32> {
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

    let response =
        poll_for_semantic_tokens(client, &uri, 20, 200).expect("Should receive semantic tokens");

    let result = response
        .get("result")
        .expect("Should have result in response");
    let data = result
        .get("data")
        .expect("Result should have data")
        .as_array()
        .expect("Data should be array");
    data.iter().map(|v| v.as_u64().unwrap() as u32).collect()
}

/// Helper: open a markdown document and request semantic tokens.
fn open_and_get_tokens(client: &mut LspClient, content: &str) -> Vec<DecodedToken> {
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

    let response =
        poll_for_semantic_tokens(client, &uri, 20, 200).expect("Should receive semantic tokens");

    let result = response
        .get("result")
        .expect("Should have result in response");
    let data = result
        .get("data")
        .expect("Result should have data")
        .as_array()
        .expect("Data should be array");
    let data_u32: Vec<u32> = data.iter().map(|v| v.as_u64().unwrap() as u32).collect();
    decode_semantic_tokens(&data_u32)
}

/// Snapshot: language-less fenced code block inside blockquote.
///
/// A ` ``` ` block without a language specifier produces no injection — the entire
/// `fenced_code_block` node becomes a host-level `markup.raw.block` token.
/// This snapshot captures whether the `> ` prefix is correctly handled.
///
/// ```markdown
/// > ```
/// > this should be string (green)
/// > ```
/// ```
#[test]
fn test_snapshot_blockquote_languageless_codeblock() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> ```\n> this should be string (green)\n> ```\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_languageless_codeblock", snapshot);
}

/// Snapshot: deeply nested injection (blockquote > markdown codeblock > python codeblock).
///
/// Tests the injection chain: host markdown → markdown injection → python injection.
/// The `> ` prefix from the blockquote must not leak into the nested python tokens.
///
/// ```markdown
/// > ``````markdown
/// > ```python
/// > y = 1 + 2
/// > x = f"""
/// >   foo{1 + 2}
/// > """
/// > ```
/// > ``````
/// ```
#[test]
fn test_snapshot_blockquote_nested_markdown_python() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> ``````markdown\n> ```python\n> y = 1 + 2\n> x = f\"\"\"\n>   foo{1 + 2}\n> \"\"\"\n> ```\n> ``````\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_nested_markdown_python", snapshot);
}

/// Snapshot: double-nested blockquote (blockquote > markdown codeblock > blockquote > python).
///
/// Tests the injection chain: host markdown → markdown injection (inner blockquote) → python.
/// The `> > ` prefix from the double blockquote must not leak into the nested python tokens.
/// Specifically, the inner `> ` must not be parsed as a Python comparison operator.
///
/// ```markdown
/// > ``````markdown
/// > > ```python
/// > > y = 1 + 2
/// > > ```
/// > ``````
/// ```
#[test]
fn test_snapshot_blockquote_double_nested_markdown_python() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> ``````markdown\n> > ```python\n> > y = 1 + 2\n> > ```\n> ``````\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_double_nested_markdown_python", snapshot);
}

/// Snapshot: heading in blockquote with multilineTokenSupport.
///
/// The `atx_heading` node in markdown spans from `# foo` to the next line's
/// start (trailing newline). With `multilineTokenSupport: true`, this becomes
/// a multiline token. The bug: `split_multiline_tokens` starts continuation
/// lines at col 0, so the heading's "class" token overwrites the `> ` prefix's
/// "keyword" token on line 1.
///
/// Expected (correct):
/// ```text
/// line 0: [keyword "> "] [class "# foo"]
/// line 1: [keyword "> bar"]
/// ```
///
/// Actual (buggy):
/// ```text
/// line 0: [keyword "> "] [class "# foo"]
/// line 1: [class "> "] [keyword "bar"]   ← heading leaks into prefix
/// ```
#[test]
fn test_snapshot_blockquote_heading_multiline_leak() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> # foo\n> bar\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_heading_multiline_leak", snapshot);
}

/// Snapshot: Python multiline string in blockquote with multilineTokenSupport.
///
/// A Python `"""..."""` triple-quoted string spans 3 lines inside a blockquote.
/// The bug: when the multiline injection token is split, continuation lines
/// start at col 0 instead of col 2 (after `> `), causing the injection's
/// "string" token to overwrite the host's "keyword" token for the `> ` prefix.
///
/// Expected (correct):
/// ```text
/// line 0: [keyword "> "] [string "```"] [variable "py"]
/// line 1: [keyword "> "] [string "\"\"\""]
/// line 2: [keyword "> "] [string "  foo"]
/// line 3: [keyword "> "] [string "\"\"\""]
/// line 4: [keyword "> "] [string "```"]
/// ```
///
/// Actual (buggy): lines 2 has no keyword prefix — string covers col 0.
#[test]
fn test_snapshot_blockquote_python_multiline_string_leak() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> ```py\n> \"\"\"\n>   foo\n> \"\"\"\n> ```\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_python_multiline_string_leak", snapshot);
}

// ─── Binary-level snapshot tests for tracking raw delta-encoded data ─────────

/// Binary snapshot: nested injection (blockquote > markdown > python).
///
/// Captures the raw delta-encoded `[u32]` data for the nested injection case.
/// This makes the exact impact of fixes visible in snapshot diffs without
/// needing to decode the tokens.
#[test]
fn test_snapshot_blockquote_nested_markdown_python_binary() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> ``````markdown\n> ```python\n> y = 1 + 2\n> x = f\"\"\"\n>   foo{1 + 2}\n> \"\"\"\n> ```\n> ``````\n";
    let data = open_and_get_raw_data(&mut client, content);

    insta::assert_json_snapshot!("blockquote_nested_markdown_python_binary", data);
}

/// Snapshot: triple-nested injection (blockquote > markdown > lua).
///
/// Tests the injection chain: host markdown → markdown injection → lua injection.
/// After the fix, `sub_select_included_ranges` should compose recursively
/// to skip `> ` prefixes at each nesting depth.
///
/// ```markdown
/// > `````markdown
/// > ````lua
/// > local x = 1
/// > ````
/// > `````
/// ```
#[test]
fn test_snapshot_blockquote_triple_nested_injection() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> `````markdown\n> ````lua\n> local x = 1\n> ````\n> `````\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_triple_nested_injection", snapshot);
}

/// Snapshot: heading inside double-nested blockquote (>> > markdown injection > heading).
///
/// Tests the injection chain: host markdown → markdown injection (via `>> `````markdown`)
/// → inner blockquote (`> `) → heading (`# foo`).
///
/// The heading `# foo\n` in markdown's tree-sitter grammar includes the trailing newline,
/// making it a multiline token. When `split_multiline_tokens` processes this inside the
/// nested injection, the continuation line should NOT have the heading "class" token leak
/// into the `>> > ` prefix region.
///
/// ```markdown
/// >> `````markdown
/// >> > # foo
/// >> > bar
/// >> `````
/// ```
#[test]
fn test_snapshot_blockquote_double_nested_heading_leak() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = ">> `````markdown\n>> > # foo\n>> > bar\n>> `````\n";
    let tokens = open_and_get_tokens(&mut client, content);
    let snapshot = build_token_snapshot(&tokens, content);

    insta::assert_json_snapshot!("blockquote_double_nested_heading_leak", snapshot);
}

/// Binary snapshot: triple-nested injection (blockquote > markdown > lua).
///
/// Raw delta-encoded `[u32]` data for the triple-nested case.
#[test]
fn test_snapshot_blockquote_triple_nested_injection_binary() {
    let mut client = LspClient::new();
    init_client_with_full_config(&mut client);

    let content = "> `````markdown\n> ````lua\n> local x = 1\n> ````\n> `````\n";
    let data = open_and_get_raw_data(&mut client, content);

    insta::assert_json_snapshot!("blockquote_triple_nested_injection_binary", data);
}
