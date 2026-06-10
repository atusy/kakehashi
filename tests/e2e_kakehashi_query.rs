//! End-to-end tests for `kakehashi/query` (query-execution-protocol).
//!
//! Exercises the full LSP round-trip a treesitter-context-style client would
//! make: open a document, send a tree-sitter query, and get back matches whose
//! captures carry an inline LSP `Range` and a trackable `NodeInfo`.
//!
//! Markdown headings stand in for `@context` here — they are the host-language
//! analog of nvim-treesitter-context's sticky headers, and need no injected
//! grammar (v1 is host-layer only).
//!
//! Covered:
//! - capturing nodes with correct names and inline ranges
//! - result `NodeInfo` ids resolving back through `kakehashi/node/*` (composition)
//! - a malformed query producing a JSON-RPC error (not null, not empty)
//! - a query mixing valid and invalid patterns running the valid one and
//!   reporting the invalid one in `skipped`
//! - an unknown document returning `null`
//!
//! Run with: `cargo test --test e2e_kakehashi_query --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Initialize + `initialized` handshake.
fn initialize(client: &mut LspClient) {
    client.send_request(
        "initialize",
        json!({ "processId": std::process::id(), "rootUri": null, "capabilities": {} }),
    );
    client.send_notification("initialized", json!({}));
}

/// Open a markdown document via `textDocument/didOpen`.
fn open_markdown(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1, "text": text }
        }),
    );
}

/// Send `kakehashi/query`, returning the raw JSON-RPC response (so tests can
/// inspect either `result` or `error`).
fn query_response(client: &mut LspClient, uri: &str, query: &str) -> Value {
    client.send_request(
        "kakehashi/query",
        json!({ "textDocument": { "uri": uri }, "query": query }),
    )
}

/// Send `kakehashi/query` and unwrap a successful `result`.
fn query_result(client: &mut LspClient, uri: &str, query: &str) -> Value {
    let response = query_response(client, uri, query);
    assert!(
        response.get("error").is_none(),
        "kakehashi/query returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// Document with two headings — the things a "sticky context" feature renders.
const DOC: &str = "# Title\n\nintro text\n\n## Section A\n\nbody text\n";

#[test]
fn query_captures_headings_with_ranges() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///query_headings.md";
    open_markdown(&mut client, uri, DOC);

    let result = query_result(&mut client, uri, "(atx_heading) @context");

    let matches = result
        .get("matches")
        .and_then(Value::as_array)
        .expect("result.matches must be an array");
    assert_eq!(matches.len(), 2, "two headings -> two matches: {matches:?}");

    // First match: the `# Title` heading on line 0.
    let first = &matches[0];
    let captures = first
        .get("captures")
        .and_then(Value::as_array)
        .expect("match.captures must be an array");
    assert_eq!(captures.len(), 1, "one @context capture per heading");

    let capture = &captures[0];
    assert_eq!(
        capture.get("name").and_then(Value::as_str),
        Some("context"),
        "capture name without the '@'"
    );

    // Inline range: no follow-up call needed to learn where the heading is.
    let start_line = capture
        .pointer("/range/start/line")
        .and_then(Value::as_u64)
        .expect("capture.range.start.line must be present");
    assert_eq!(start_line, 0, "# Title starts on line 0");

    // The capture carries a trackable NodeInfo.
    let node = capture.get("node").expect("capture.node must be present");
    assert_eq!(
        node.get("kind").and_then(Value::as_str),
        Some("atx_heading")
    );
    assert!(
        node.get("id").and_then(Value::as_str).is_some(),
        "NodeInfo must carry a ULID id"
    );

    // Second match should be the `## Section A` heading on line 4.
    let second_start = matches[1]
        .pointer("/captures/0/range/start/line")
        .and_then(Value::as_u64)
        .expect("second match range");
    assert_eq!(second_start, 4, "## Section A starts on line 4");
}

#[test]
fn query_result_nodes_are_trackable() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///query_trackable.md";
    open_markdown(&mut client, uri, DOC);

    let result = query_result(&mut client, uri, "(atx_heading) @context");
    let id = result
        .pointer("/matches/0/captures/0/node/id")
        .and_then(Value::as_str)
        .expect("first capture must have a node id")
        .to_string();

    // The query result composes with the node accessors: feed the id straight
    // back into kakehashi/node/kind.
    let response = client.send_request(
        "kakehashi/node/kind",
        json!({ "textDocument": { "uri": uri }, "id": id }),
    );
    let kind = response
        .pointer("/result/kind")
        .and_then(Value::as_str)
        .expect("kakehashi/node/kind must resolve the queried node");
    assert_eq!(kind, "atx_heading");
}

#[test]
fn malformed_query_returns_error() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///query_bad.md";
    open_markdown(&mut client, uri, DOC);

    // Unbalanced parens cannot be split into patterns at all.
    let response = query_response(&mut client, uri, "(atx_heading @context");
    assert!(
        response.get("error").is_some(),
        "a malformed query must surface a JSON-RPC error, got: {response:?}"
    );
    assert!(
        response.get("result").is_none(),
        "an errored request must not also carry a result"
    );
}

#[test]
fn mixed_valid_and_invalid_patterns_skip_the_invalid() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///query_mixed.md";
    open_markdown(&mut client, uri, DOC);

    // One valid pattern plus one referencing a node kind absent from the
    // markdown grammar: tolerant compilation keeps the good one.
    let query = "(atx_heading) @context\n(no_such_markdown_node) @bad";
    let result = query_result(&mut client, uri, query);

    let matches = result.get("matches").and_then(Value::as_array).unwrap();
    assert_eq!(matches.len(), 2, "the valid heading pattern still runs");

    let skipped = result
        .get("skipped")
        .and_then(Value::as_array)
        .expect("result.skipped must be an array");
    assert_eq!(
        skipped.len(),
        1,
        "the invalid pattern is reported in skipped"
    );
    assert!(
        skipped[0].get("reason").and_then(Value::as_str).is_some(),
        "a skipped pattern carries a reason"
    );
}

#[test]
fn query_on_unknown_document_returns_null() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let result = query_result(
        &mut client,
        "file:///never_opened.md",
        "(atx_heading) @context",
    );
    assert_eq!(
        result,
        Value::Null,
        "unknown document -> null, not an error"
    );
}
