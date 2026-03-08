//! End-to-end test for color presentation in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for color presentation:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Color presentation request sent with mock color/range
//! - kakehashi detects injection, spawns lua-ls, and transforms coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_color_presentation --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//!
//! **Note**: colorPresentation is typically called after documentColor returns colors.
//! Since lua-ls doesn't return colors, we use mock values to test the infrastructure.

#![cfg(all(feature = "e2e", feature = "experimental"))]

mod helpers;

use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;

/// E2E test: colorPresentation request is handled without error
///
/// colorPresentation is typically called after documentColor returns colors.
/// Since lua-ls doesn't return colors for Lua code, we use mock values to test
/// that the bridge infrastructure correctly handles the request.
#[test]
fn e2e_color_presentation_request_handled() {
    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block
    let markdown_content = r##"# Test Document

```lua
local color = "#ff0000"
print(color)
```

More text.
"##;

    let markdown_uri = "file:///test_color_presentation.md";

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

    // Send colorPresentation request with mock color and range
    // The range points to line 3 (0-indexed) which is inside the Lua code block
    // In host coordinates: line 3 is `local color = "#ff0000"`
    let color_presentation_response = client.send_request(
        "textDocument/colorPresentation",
        json!({
            "textDocument": { "uri": markdown_uri },
            "color": {
                "red": 1.0,
                "green": 0.0,
                "blue": 0.0,
                "alpha": 1.0
            },
            "range": {
                "start": { "line": 3, "character": 14 },
                "end": { "line": 3, "character": 23 }
            }
        }),
    );

    println!(
        "Color presentation response: {:?}",
        color_presentation_response
    );

    // The request should complete without crashing
    assert!(
        color_presentation_response.get("id").is_some(),
        "Response should have id field"
    );

    // Check for errors
    if let Some(error) = color_presentation_response.get("error") {
        panic!("Color presentation request returned error: {:?}", error);
    }

    // Parse the result - should be an array of ColorPresentation (possibly empty)
    let result = color_presentation_response.get("result");
    assert!(result.is_some(), "Should have result field");

    let r = result.unwrap();
    if r.is_array() {
        let presentations = r.as_array().unwrap();
        println!("E2E: Got {} color presentation(s)", presentations.len());

        // If presentations are returned, verify they have valid structure
        for presentation in presentations {
            // ColorPresentation has a required 'label' field
            assert!(
                presentation.get("label").is_some(),
                "ColorPresentation should have label field"
            );
        }
    } else if r.is_null() {
        // Some servers return null instead of empty array
        println!("E2E: Got null result (acceptable for colorPresentation)");
    }

    println!("E2E: Color presentation request completed successfully");

    // Clean shutdown
    shutdown_client(&mut client);
}
