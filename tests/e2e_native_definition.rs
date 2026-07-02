//! End-to-end tests for the native lexical-resolution layer
//! (lexical-name-resolution ADR) over the real LSP protocol.
//!
//! No bridge server is configured: definition / references / rename answers
//! come from the embedded lua `bindings.scm` and the tree-sitter tree alone.
//!
//! Run with: `cargo test --test e2e_native_definition --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::json;

const LUA_SOURCE: &str = "local greeting = \"hi\"\nprint(greeting)\n";
const URI: &str = "file:///native_bindings_test.lua";

fn client_with_open_doc() -> LspClient {
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
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": URI,
                "languageId": "lua",
                "version": 1,
                "text": LUA_SOURCE
            }
        }),
    );
    client
}

/// Cursor on `greeting` inside `print(greeting)` — line 1, character 8.
fn use_position() -> serde_json::Value {
    json!({ "line": 1, "character": 8 })
}

#[test]
fn native_definition_resolves_without_a_bridge_server() {
    let mut client = client_with_open_doc();

    let response = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": URI },
            "position": use_position()
        }),
    );
    assert!(
        response.get("error").is_none(),
        "definition must not error: {response:?}"
    );
    let result = response.get("result").expect("result field");
    assert!(!result.is_null(), "native layer must answer: {response:?}");
    let location = if result.is_array() {
        result
            .as_array()
            .unwrap()
            .first()
            .expect("one location")
            .clone()
    } else {
        result.clone()
    };
    let range = location
        .get("range")
        .or_else(|| location.get("targetRange"));
    let start = &range.expect("range")["start"];
    assert_eq!(start["line"], 0, "definition is the `local greeting` line");
    assert_eq!(start["character"], 6);
}

#[test]
fn native_definition_resolves_inside_markdown_lua_block() {
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

    // line 3: `local v = 1`; line 4: `print(v)` — inside a lua fence.
    let markdown = "# t\n\n```lua\nlocal v = 1\nprint(v)\n```\n";
    let md_uri = "file:///native_bindings_test.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": md_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown
            }
        }),
    );

    let response = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": md_uri },
            "position": { "line": 4, "character": 6 }
        }),
    );
    assert!(response.get("error").is_none(), "{response:?}");
    let result = &response["result"];
    assert!(
        !result.is_null(),
        "injected-layer cursor must resolve natively: {response:?}"
    );
    let location = if result.is_array() {
        result[0].clone()
    } else {
        result.clone()
    };
    let range = location
        .get("range")
        .or_else(|| location.get("targetRange"))
        .expect("range");
    assert_eq!(range["start"]["line"], 3, "definition in host coordinates");
    assert_eq!(range["start"]["character"], 6);
}

#[test]
fn native_references_and_rename_span_definition_and_use() {
    let mut client = client_with_open_doc();

    let response = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": URI },
            "position": use_position(),
            "context": { "includeDeclaration": true }
        }),
    );
    let refs = response["result"]
        .as_array()
        .unwrap_or_else(|| panic!("references must answer: {response:?}"))
        .clone();
    assert_eq!(refs.len(), 2, "declaration + use: {refs:?}");

    let response = client.send_request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": URI },
            "position": use_position(),
            "newName": "salutation"
        }),
    );
    let changes = &response["result"]["changes"][URI];
    let edits = changes
        .as_array()
        .unwrap_or_else(|| panic!("rename must produce edits: {response:?}"));
    assert_eq!(edits.len(), 2, "both occurrences renamed: {edits:?}");
    assert!(edits.iter().all(|e| e["newText"] == "salutation"));
}
