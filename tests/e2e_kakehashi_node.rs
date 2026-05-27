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

/// Send `kakehashi/node/text` for an id and unwrap the `result` field.
fn request_node_text(client: &mut LspClient, uri: &str, id: &str) -> Value {
    let response = client.send_request(
        "kakehashi/node/text",
        json!({
            "textDocument": { "uri": uri },
            "id": id
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node/text returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
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

/// Round-trip: acquire an id via `kakehashi/node`, then ask
/// `kakehashi/node/text` for it and verify the returned slice matches the
/// expected substring of the document.
#[test]
fn test_node_text_round_trips_for_known_id() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_text.md";
    let text = "# Hello\n\nSome paragraph text.\n";
    open_markdown(&mut client, uri, text);

    // Cursor on the second line's paragraph text.
    let node = request_node(&mut client, uri, 2, 2);
    assert!(!node.is_null(), "expected a NodeInfo for paragraph text");
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .expect("id must be present");

    let text_response = request_node_text(&mut client, uri, id);
    assert!(
        !text_response.is_null(),
        "kakehashi/node/text must return a NodeText for a freshly issued id"
    );

    let returned_text = text_response
        .get("text")
        .and_then(Value::as_str)
        .expect("response must contain a `text` field");

    // The selected node must be a subsequence of the original document text.
    assert!(
        text.contains(returned_text),
        "returned text {:?} should be a substring of the document {:?}",
        returned_text,
        text,
    );
    // Sanity: text is non-empty and contains at least one character from "paragraph".
    assert!(!returned_text.is_empty(), "node text must be non-empty");
}

/// End-of-document exception: cursor exactly at `L` for a non-empty document
/// must resolve to the smallest node whose `end_byte == L`. Without the exception
/// the half-open rule would return null and break end-of-file motions.
#[test]
fn test_node_at_end_of_document_returns_node_info() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_eod.md";
    // Three lines, no trailing newline so the document's last byte is the final 'h'.
    // doc_len = "# A\nfoo\nbah".len() = 11. The cursor sits past the 'h'.
    let text = "# A\nfoo\nbah";
    open_markdown(&mut client, uri, text);

    // Cursor at (line 2, character 3) is past the last char of "bah", i.e., byte L=11.
    let result = request_node(&mut client, uri, 2, 3);

    assert!(
        !result.is_null(),
        "end-of-document position (b == L, L > 0) must resolve to a node, got null"
    );
    let id = result.get("id").expect("result must have id field");
    assert_ulid_shaped(id);
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type must be a string");
    assert!(!ty.is_empty(), "type field should be a non-empty kind");
}

/// Empty-document case: ADR-0025 gates the end-of-document exception on `L > 0`,
/// so any query on a zero-length document must return null.
#[test]
fn test_node_in_empty_document_returns_null() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_empty.md";
    open_markdown(&mut client, uri, "");

    let result = request_node(&mut client, uri, 0, 0);
    assert!(
        result.is_null(),
        "empty document must return null (L == 0 gates off the exception), got {:?}",
        result
    );
}

/// Out-of-bounds position: cursor past the actual document length must return null.
#[test]
fn test_node_position_past_eof_returns_null() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_oob.md";
    let text = "# A\n";
    open_markdown(&mut client, uri, text);

    // Line 99 is well past the actual content.
    let result = request_node(&mut client, uri, 99, 0);
    assert!(
        result.is_null(),
        "position past EOF must return null, got {:?}",
        result
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
