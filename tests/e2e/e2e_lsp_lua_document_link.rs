//! End-to-end test for Lua document link in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for document link:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Document link request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms coordinates
//!
//! Run with: `cargo test --features e2e --test e2e e2e_lsp_lua_document_link::`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//! **Note**: lua-ls may not support documentLink (returns method not found), so
//! this test mainly verifies the kakehashi infrastructure is wired correctly.

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn document_links_with_retry(client: &mut LspClient, uri: &str) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/documentLink",
            json!({ "textDocument": { "uri": uri } }),
        );
        assert!(
            response.get("error").is_none(),
            "unexpected response: {response}"
        );
        if let Some(links) = response["result"].as_array()
            && !links.is_empty()
        {
            return links.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for a document link");
}

#[test]
fn e2e_document_link_resolve_round_trips_to_virtual_origin() {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("document_link_resolve.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock-document-link": {
                    "cmd": [mock_formatter_bin(), "document-link-resolve"],
                    "languages": ["lua"]
                }
            }}
        }),
    );
    assert_eq!(
        init["result"]["capabilities"]["documentLinkProvider"]["resolveProvider"],
        json!(true)
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_document_link_resolve.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "# Test\n\n```lua\nlocal x = 1\n```\n"
        }}),
    );

    let links = document_links_with_retry(&mut client, uri);
    let link = &links[0];
    assert_eq!(link["range"]["start"]["line"], 3);
    assert!(link.get("target").is_none() || link["target"].is_null());
    assert_eq!(link["data"]["kakehashi"]["origin"], "mock-document-link");

    let response = client.send_request("documentLink/resolve", link.clone());
    assert!(
        response.get("error").is_none(),
        "unexpected response: {response}"
    );
    let resolved = &response["result"];
    assert_eq!(resolved["tooltip"], "mock resolved:link-1");
    assert_eq!(
        resolved["target"],
        resolved["data"]["kakehashi"]["inner"]["uri"]
    );
    assert_eq!(resolved["range"], link["range"]);

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "text": "shift\n"
            }]
        }),
    );
    let stale_response = client.send_request("documentLink/resolve", link.clone());
    assert!(
        stale_response.get("error").is_none(),
        "unexpected stale response: {stale_response}"
    );
    let stale = &stale_response["result"];
    assert!(stale.get("target").is_none() || stale["target"].is_null());
    assert_eq!(stale["data"]["kakehashi"]["origin"], "mock-document-link");

    shutdown_client(&mut client);
}

#[test]
fn e2e_document_link_resolve_round_trips_to_host_origin() {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("host_document_link_resolve.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-document-link": {
                        "cmd": [mock_formatter_bin(), "document-link-resolve"],
                        "languages": ["markdown"]
                    }
                },
                "languages": {
                    "markdown": { "bridge": { "_self": { "enabled": true } } }
                }
            }
        }),
    );
    assert_eq!(
        init["result"]["capabilities"]["documentLinkProvider"]["resolveProvider"],
        json!(true)
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_host_document_link_resolve.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "# Host link\n"
        }}),
    );

    let links = document_links_with_retry(&mut client, uri);
    let link = &links[0];
    assert_eq!(link["range"]["start"]["line"], 0);
    assert_eq!(link["data"]["kakehashi"]["host_layer"], true);

    let response = client.send_request("documentLink/resolve", link.clone());
    assert!(
        response.get("error").is_none(),
        "unexpected response: {response}"
    );
    let resolved = &response["result"];
    assert_eq!(resolved["tooltip"], "mock resolved:link-1");
    assert_eq!(resolved["target"], uri);
    assert_eq!(resolved["range"], link["range"]);
    assert_eq!(resolved["data"]["kakehashi"]["host_layer"], true);

    shutdown_client(&mut client);
}

/// E2E test: document link request is handled without error
#[test]
fn e2e_document_link_request_handled() {
    let (mut client, _config_dir) = create_lua_configured_client();

    // Open markdown document with Lua code block containing require statement
    let markdown_content = r#"# Test Document

```lua
local json = require("cjson")
local data = json.decode('{"key": "value"}')
print(data.key)
```

More text.
"#;

    let markdown_uri = "file:///test_document_link.md";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    // Give lua-ls time to process
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Request document link for the file
    let link_response = client.send_request(
        "textDocument/documentLink",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!("Document link response: {:?}", link_response);

    // The request should complete without crashing
    // lua-ls may return:
    // - null (no links found)
    // - [] (empty array)
    // - array of DocumentLink objects
    // - error (method not supported by lua-ls)

    // All of these are valid - the important thing is kakehashi handled the request
    assert!(
        link_response.get("id").is_some(),
        "Response should have id field"
    );

    // Check that we didn't get an internal error from kakehashi itself
    if let Some(error) = link_response.get("error") {
        // Method not found (-32601) from downstream is acceptable
        // Internal errors from kakehashi would be different codes
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != -32601 {
            // -32601 is "method not found" which is OK (lua-ls doesn't support documentLink)
            panic!("Unexpected error: {:?}", error);
        }
        println!("E2E: lua-ls returned method not found (expected - documentLink not supported)");
    } else {
        // Got a result (null or array) - either is fine
        let result = link_response.get("result");
        if let Some(r) = result {
            if r.is_array() {
                let links = r.as_array().unwrap();
                println!("E2E: Got {} document links", links.len());

                // If we got links, verify they have been transformed to host coordinates
                for link in links {
                    if let Some(range) = link.get("range") {
                        let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                        // Links should be in host coordinates (inside the code block, line 3+)
                        assert!(
                            start_line >= 2,
                            "Link line should be in host coordinates (expected >= 2, got {})",
                            start_line
                        );
                    }
                }
            } else if r.is_null() {
                println!("E2E: Got null result (no links found)");
            }
        }
    }

    println!("E2E: Document link request completed successfully");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: document link for markdown file without code blocks returns null
#[test]
fn e2e_document_link_no_injections_returns_null() {
    let (mut client, _config_dir) = create_lua_configured_client();

    // Open markdown document WITHOUT code blocks
    let markdown_content = r#"# Test Document

Just some plain text without any code blocks.

More text here.
"#;

    let markdown_uri = "file:///test_no_injections.md";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    // Request document link
    let link_response = client.send_request(
        "textDocument/documentLink",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!(
        "Document link (no injections) response: {:?}",
        link_response
    );

    // Should return null since there are no injection regions
    assert!(
        link_response.get("error").is_none(),
        "Should not return error for markdown without injections"
    );

    let result = link_response.get("result");
    assert!(
        result.is_some() && result.unwrap().is_null(),
        "Document link for markdown without injections should return null"
    );

    println!("E2E: Document link correctly returns null for markdown without code blocks");

    // Clean shutdown
    shutdown_client(&mut client);
}
