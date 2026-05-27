//! End-to-end tests for `kakehashi/node` and `kakehashi/node/text` (ADR-0025 PR-1).
//!
//! Covers the entry-point method (position → NodeInfo) plus the text resolution
//! method (id → current node text) and their interaction with `didChange`:
//!
//! - smallest-at-cursor lookup for named-or-anonymous nodes (host language only)
//! - end-of-document exception (`b == L && L > 0 && e == L`)
//! - empty document and out-of-bounds returning `null`
//! - `kakehashi/node/text` returning the live slice for a tracked node
//! - ULID survival across position-adjusting edits
//! - ULID invalidation when the edit covers the node's START byte
//!
//! Run with: `cargo test --test e2e_kakehashi_node --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Initialize + `initialized` handshake.
fn initialize(client: &mut LspClient) {
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
}

/// Open a markdown document via `textDocument/didOpen`.
fn open_markdown(client: &mut LspClient, uri: &str, text: &str) {
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
}

/// Send `kakehashi/node` and unwrap the `result` field (which may be `null`).
fn request_node(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
    let response = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// ULIDs are 26 uppercase Crockford-base32 characters.
fn assert_ulid_shaped(value: &Value) {
    let s = value.as_str().expect("id should be a string");
    assert_eq!(s.len(), 26, "ULID must be 26 characters, got {:?}", s);
    assert!(
        s.chars().all(|c| c.is_ascii_alphanumeric()),
        "ULID must be alphanumeric, got {:?}",
        s
    );
}

/// `kakehashi/node` on a markdown heading returns a NodeInfo with the heading
/// node's tree-sitter type and a ULID-shaped id.
#[test]
fn test_node_at_heading_returns_node_info() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_heading.md";
    let text = "# Hello\n\nSome paragraph text.\n";
    open_markdown(&mut client, uri, text);

    // Cursor on the word "Hello" inside the ATX heading (line 0, character 4).
    let result = request_node(&mut client, uri, 0, 4);

    assert!(
        !result.is_null(),
        "kakehashi/node should return a NodeInfo for a position inside the heading, got null"
    );

    let id = result.get("id").expect("result must have id field");
    assert_ulid_shaped(id);

    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("result must have a string type field");

    // The smallest node containing "Hello" inside `# Hello` is anonymous text
    // ("hello") or its named ancestor `inline`. Either is acceptable for the
    // entry point — what matters is that the type is non-empty.
    assert!(
        !ty.is_empty(),
        "type field should be the tree-sitter node kind, got empty string"
    );
}
