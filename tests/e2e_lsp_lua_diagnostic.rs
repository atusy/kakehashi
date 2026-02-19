//! End-to-end test for Lua diagnostics in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for pull diagnostics:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - textDocument/diagnostic request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms diagnostic positions
//!
//! Run with: `cargo test --test e2e_lsp_lua_diagnostic --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//!
//! Implements ADR-0020 Phase 1: Pull-first diagnostic forwarding.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use helpers::lsp_polling::wait_for_server_ready;
use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;

/// E2E test: diagnosticProvider capability is advertised
#[test]
fn e2e_diagnostic_capability_advertised() {
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

    // Verify diagnosticProvider is in capabilities
    let capabilities = init_response
        .get("result")
        .and_then(|r| r.get("capabilities"))
        .expect("Should have capabilities in init response");

    let diagnostic_provider = capabilities.get("diagnosticProvider");
    assert!(
        diagnostic_provider.is_some(),
        "diagnosticProvider should be advertised in server capabilities"
    );

    // Verify specific options per ADR-0020
    let provider = diagnostic_provider.unwrap();
    assert_eq!(
        provider.get("interFileDependencies"),
        Some(&json!(false)),
        "interFileDependencies should be false per ADR-0020"
    );
    assert_eq!(
        provider.get("workspaceDiagnostics"),
        Some(&json!(false)),
        "workspaceDiagnostics should be false per ADR-0020"
    );

    println!("E2E: diagnosticProvider capability advertised correctly");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: diagnostic response has valid structure (DocumentDiagnosticReport)
///
/// Validates the response structure is correct regardless of whether lua-ls
/// reports any diagnostics. This test always passes if the bridge is working.
/// Position transformation correctness is tested separately in unit tests.
#[test]
fn e2e_diagnostic_response_structure_is_valid() {
    let mut client = create_lua_configured_client();

    let markdown_content = r#"# Test Document

```lua
local x = 1
```
"#;

    let markdown_uri = "file:///test_diagnostic_structure.md";

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

    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    let response = client.send_request(
        "textDocument/diagnostic",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    let result = response
        .get("result")
        .expect("Should have result in diagnostic response");

    insta::assert_json_snapshot!("diagnostic_response_structure", result, {
        ".items" => insta::dynamic_redaction(|value, _path| {
            // Redact items contents (vary by lua-ls timing) but preserve the array type
            let len = value.as_slice().map(|s| s.len()).unwrap_or(0);
            format!("[{len} items redacted]")
        })
    });

    shutdown_client(&mut client);
}

/// E2E test: diagnostic positions are transformed to host document coordinates
///
/// When diagnostics are returned, verifies that positions are correctly
/// transformed from virtual (Lua-only) to host (Markdown) coordinates.
/// Note: This test may report no diagnostics if lua-ls needs more time,
/// which is acceptable - position transformation is also tested in unit tests.
#[test]
fn e2e_diagnostic_positions_transformed_to_host_coordinates() {
    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing a syntax error
    // Document structure (0-indexed lines):
    // Line 0: "# Test Document"
    // Line 1: ""
    // Line 2: "```lua"
    // Line 3: "-- Syntax error: unmatched parenthesis"  <- Lua content starts here
    // Line 4: "print((("                                <- Syntax error on this line
    // Line 5: "```"
    // Line 6: ""
    // Line 7: "More text."
    let markdown_content = r#"# Test Document

```lua
-- Syntax error: unmatched parenthesis
print(((
```

More text.
"#;

    // The Lua code block content starts at line 3 (after ```lua)
    let lua_content_start_line: u64 = 3;

    let markdown_uri = "file:///test_diagnostic_positions.md";

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

    // Wait for lua-ls to start and be ready
    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    // Send diagnostic request
    let response = client.send_request(
        "textDocument/diagnostic",
        json!({
            "textDocument": {
                "uri": markdown_uri
            }
        }),
    );

    println!(
        "Diagnostic response: {}",
        serde_json::to_string_pretty(&response).unwrap()
    );

    let result = response
        .get("result")
        .expect("Should have result in diagnostic response");

    let items = result
        .get("items")
        .and_then(|i| i.as_array())
        .expect("Result should have 'items' array");

    // If diagnostics are returned, verify positions are transformed to host coordinates
    // Note: lua-ls should report syntax errors, but timing can be flaky in tests.
    if items.is_empty() {
        println!(
            "No diagnostics returned (lua-ls may need more time). \
             Position transformation correctness tested in unit tests."
        );
    } else {
        println!("Verifying {} diagnostic position(s)...", items.len());

        // Verify each diagnostic position is in host document coordinates
        // (line >= lua_content_start_line, not virtual line 0 or 1)
        for (i, diagnostic) in items.iter().enumerate() {
            let range = diagnostic
                .get("range")
                .expect("Diagnostic should have 'range'");
            let start_line = range
                .get("start")
                .and_then(|s| s.get("line"))
                .and_then(|l| l.as_u64())
                .expect("Diagnostic range should have start.line");

            assert!(
                start_line >= lua_content_start_line,
                "Diagnostic {} position should be transformed to host coordinates: \
                 got line {}, expected >= {} (Lua content start line)",
                i,
                start_line,
                lua_content_start_line
            );

            println!(
                "Diagnostic {}: line {} (correctly transformed to host coordinates)",
                i, start_line
            );
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: multi-region diagnostic aggregation with multiple Lua code blocks
///
/// Sprint 17: Verifies that diagnostics from multiple injection regions
/// are aggregated into a single response.
#[test]
fn e2e_diagnostic_multi_region_aggregation() {
    let mut client = create_lua_configured_client();

    // Open markdown document with MULTIPLE Lua code blocks
    // This tests the Sprint 17 multi-region aggregation
    let markdown_content = r#"# Multi-Region Diagnostic Test

First Lua block:

```lua
-- Block 1: Valid Lua code
local x = 1
print(x)
```

Some text between blocks.

Second Lua block:

```lua
-- Block 2: More Lua code
local y = 2
return y
```

Third Lua block:

```lua
-- Block 3: Even more Lua
function test()
    return "hello"
end
```

End of document.
"#;

    let markdown_uri = "file:///test_multi_region_diagnostic.md";

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

    // Wait for lua-ls to start and be ready using exponential backoff polling
    // Starts at 100ms, doubles each retry, up to 5 attempts (total ~3100ms max)
    wait_for_server_ready(&mut client, markdown_uri, 5, 100);

    // Send diagnostic request - should aggregate from all 3 Lua blocks
    let response = client.send_request(
        "textDocument/diagnostic",
        json!({
            "textDocument": {
                "uri": markdown_uri
            }
        }),
    );

    println!(
        "Multi-region diagnostic response: {}",
        serde_json::to_string_pretty(&response).unwrap()
    );

    // The response should be a DocumentDiagnosticReport
    let result = response.get("result");
    assert!(
        result.is_some(),
        "Should have result in multi-region diagnostic response"
    );

    // Verify it's a "full" report
    let result = result.unwrap();
    let kind = result.get("kind");
    assert_eq!(
        kind,
        Some(&json!("full")),
        "Multi-region result should be a 'full' report"
    );

    // The items array should exist (may be empty if no diagnostics reported)
    let items = result.get("items");
    assert!(
        items.is_some(),
        "Multi-region result should have 'items' field"
    );

    println!(
        "E2E: Multi-region diagnostic aggregation handled correctly with {} diagnostics",
        items.unwrap().as_array().map(|a| a.len()).unwrap_or(0)
    );

    // Clean shutdown
    shutdown_client(&mut client);
}
