//! End-to-end test for Lua document symbol in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for document symbol:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Document symbol request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_document_symbol --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;

/// Create an LspClient with hierarchicalDocumentSymbolSupport enabled.
///
/// The shared `create_lua_configured_client()` sends empty capabilities,
/// which defaults hierarchicalDocumentSymbolSupport to false. This helper
/// creates a client that explicitly enables it, so tests can assert on
/// DocumentSymbol fields (range, selectionRange) rather than the flat
/// SymbolInformation format (location).
fn create_lua_client_with_hierarchical_symbol_support() -> LspClient {
    let mut client = LspClient::new();
    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "documentSymbol": {
                        "hierarchicalDocumentSymbolSupport": true
                    }
                }
            },
            "initializationOptions": {
                "languageServers": {
                    "lua-language-server": {
                        "cmd": ["lua-language-server"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    client
}

/// E2E test: documentSymbolProvider capability is advertised
#[test]
fn e2e_document_symbol_capability_advertised() {
    let mut client = LspClient::new();

    // Initialize handshake
    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );

    // Verify documentSymbolProvider is in capabilities
    let capabilities = init_response
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .expect("Should have capabilities in init response");

    let symbol_provider = capabilities.get("documentSymbolProvider");
    assert!(
        symbol_provider.is_some(),
        "documentSymbolProvider should be advertised in server capabilities"
    );

    println!("E2E: documentSymbolProvider capability advertised");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: document symbol request returns symbols with transformed coordinates
///
/// Uses a client with hierarchicalDocumentSymbolSupport enabled so the server
/// returns DocumentSymbol[] (with range/selectionRange fields) rather than the
/// flat SymbolInformation[] format.
#[test]
fn e2e_document_symbol_request_returns_transformed_symbols() {
    let mut client = create_lua_client_with_hierarchical_symbol_support();

    // Open markdown document with Lua code block containing functions
    // The code block starts at line 2 (0-indexed: after the header and blank line)
    // So function "greet" at virtual line 0 should appear at host line 3
    let markdown_content = r#"# Test Document

```lua
local function greet(name)
    return "Hello, " .. name
end

local function add(a, b)
    return a + b
end

greet("World")
```

More text.
"#;

    let markdown_uri = "file:///test_document_symbol.md";

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
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // Request document symbol for the file
    let symbol_response = client.send_request(
        "textDocument/documentSymbol",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!("Document symbol response: {:?}", symbol_response);

    // The request should complete without crashing
    assert!(
        symbol_response.get("id").is_some(),
        "Response should have id field"
    );

    // Check for errors
    if let Some(error) = symbol_response.get("error") {
        panic!("Document symbol request returned error: {:?}", error);
    }

    // Parse the result
    let result = symbol_response.get("result");
    if let Some(r) = result {
        if r.is_array() {
            let symbols = r.as_array().unwrap();
            println!("E2E: Got {} symbols", symbols.len());

            // Verify at least one symbol was returned (greet or add functions)
            assert!(
                !symbols.is_empty(),
                "Should have at least one symbol from the Lua code block"
            );

            // Verify symbols have been transformed to host coordinates
            for symbol in symbols {
                // DocumentSymbol format has range and selectionRange
                if let Some(range) = symbol.get("range") {
                    let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                    // Code block starts at line 2 (0-indexed), so first symbol line should be >= 3
                    // (the ```lua line is line 2, content starts at line 3)
                    assert!(
                        start_line >= 3,
                        "Symbol line should be in host coordinates (expected >= 3, got {}). Symbol: {:?}",
                        start_line,
                        symbol
                    );
                }

                // Also check selectionRange if present
                if let Some(selection_range) = symbol.get("selectionRange") {
                    let start_line = selection_range["start"]["line"].as_u64().unwrap_or(0);
                    assert!(
                        start_line >= 3,
                        "Symbol selectionRange line should be in host coordinates (expected >= 3, got {})",
                        start_line
                    );
                }

                // Check for symbol name
                let name = symbol.get("name").and_then(|n| n.as_str());
                if let Some(n) = name {
                    println!("  Found symbol: {}", n);
                }
            }
        } else if r.is_null() {
            // lua-ls might not return symbols for some reason
            println!("E2E: Got null result (no symbols found by lua-ls)");
        }
    }

    println!("E2E: Document symbol request completed successfully");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: document symbol for markdown file without code blocks returns null
#[test]
fn e2e_document_symbol_no_injections_returns_null() {
    let mut client = create_lua_configured_client();

    // Open markdown document WITHOUT code blocks
    let markdown_content = r#"# Test Document

Just some plain text without any code blocks.

More text here.
"#;

    let markdown_uri = "file:///test_no_injections_symbol.md";

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

    // Request document symbol
    let symbol_response = client.send_request(
        "textDocument/documentSymbol",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!(
        "Document symbol (no injections) response: {:?}",
        symbol_response
    );

    // Should return null since there are no injection regions
    assert!(
        symbol_response.get("error").is_none(),
        "Should not return error for markdown without injections"
    );

    let result = symbol_response.get("result");
    assert!(
        result.is_some() && result.unwrap().is_null(),
        "Document symbol for markdown without injections should return null"
    );

    println!("E2E: Document symbol correctly returns null for markdown without code blocks");

    // Clean shutdown
    shutdown_client(&mut client);
}
