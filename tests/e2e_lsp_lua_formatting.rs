//! End-to-end test for Lua textDocument/formatting in Markdown code blocks via
//! the kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for formatting:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - `textDocument/formatting` request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms each returned
//!   TextEdit range from virtual coordinates back to the host document
//!
//! Run with: `cargo test --test e2e_lsp_lua_formatting --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//! **Note**: lua-ls's stylua-style formatting is best-effort; the test
//! tolerates `null` / empty-array responses and only asserts host-coordinate
//! correctness when edits are returned. The bridge swallows downstream
//! errors (e.g., lua-ls's `-32601`) into `FanInResult::NoResult` and
//! surfaces them as a `null` result, so any top-level JSON-RPC `error`
//! on this request indicates a kakehashi routing/handler bug.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;

/// Default LSP `FormattingOptions` body — fixed tab size + spaces. Matches
/// what most editors send out of the box; sufficient to exercise the bridge.
fn default_formatting_options() -> serde_json::Value {
    json!({ "tabSize": 4, "insertSpaces": true })
}

/// E2E test: formatting request through the bridge completes without errors
/// and (if edits are returned) places every edit inside the host code-fence.
#[test]
fn e2e_formatting_request_returns_host_coordinate_edits() {
    let (mut client, _config_dir) = create_lua_configured_client();

    // Deliberately badly-formatted lua inside a markdown code fence — gives
    // the downstream formatter something to rewrite. The fence opens on line
    // 2 of the host doc, so any returned TextEdit must reference line >= 2.
    let markdown_content = "# Test Document\n\n```lua\nlocal  x   =   1\nlocal y=2\n```\n";

    let markdown_uri = "file:///test_formatting.md";

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

    // Give lua-ls time to process the virtual document didOpen.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let format_response = client.send_request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": markdown_uri },
            "options": default_formatting_options(),
        }),
    );

    println!("Formatting response: {:?}", format_response);

    assert!(
        format_response.get("id").is_some(),
        "Response should have id field"
    );

    // The request goes to kakehashi, not lua-ls directly. The bridge
    // converts a downstream `-32601` ("method not found") into a
    // `FanInResult::NoResult` and returns `null` to the editor. So any
    // top-level JSON-RPC error here means kakehashi itself failed to
    // route/handle `textDocument/formatting` — a regression, not a
    // tolerable downstream gap.
    assert!(
        format_response.get("error").is_none(),
        "kakehashi must not surface a top-level error for textDocument/formatting; \
         downstream errors are absorbed by the bridge. Got: {:?}",
        format_response.get("error")
    );

    let result = format_response
        .get("result")
        .expect("Response should carry a result field");

    if result.is_null() {
        println!("E2E: Got null result (formatter signalled no edits)");
    } else {
        let edits = result
            .as_array()
            .expect("formatting result must be null or TextEdit[]");
        println!("E2E: Got {} TextEdit(s)", edits.len());

        // Code fence opens on host line 2 (after "# Test Document\n\n```lua\n").
        // Every returned edit's start must land at or after line 2 — any
        // earlier line would indicate a coordinate-translation bug.
        for edit in edits {
            let start_line = edit["range"]["start"]["line"]
                .as_u64()
                .expect("TextEdit.range.start.line must be a number");
            assert!(
                start_line >= 2,
                "Edit must reference host coordinates inside the code fence \
                 (expected line >= 2, got {}). Full edit: {:?}",
                start_line,
                edit
            );
        }
    }

    shutdown_client(&mut client);
}

/// E2E test: formatting a markdown file with no injection regions returns
/// `null` (per `formatting_impl`'s `all_regions.is_empty()` early-return).
#[test]
fn e2e_formatting_without_injections_returns_null() {
    let (mut client, _config_dir) = create_lua_configured_client();

    let markdown_content = "# Plain markdown\n\nNo code blocks here.\n";
    let markdown_uri = "file:///test_formatting_plain.md";

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

    let format_response = client.send_request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": markdown_uri },
            "options": default_formatting_options(),
        }),
    );

    println!("Formatting (no injections) response: {:?}", format_response);

    assert!(
        format_response.get("error").is_none(),
        "Should not error for markdown without injections"
    );

    let result = format_response
        .get("result")
        .expect("Response should carry a result field");
    assert!(
        result.is_null(),
        "Formatting without injections must return null, got {:?}",
        result
    );

    shutdown_client(&mut client);
}
