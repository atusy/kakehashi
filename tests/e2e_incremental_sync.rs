//! End-to-end test for incremental sync (TextDocumentSyncKind::Incremental).
//!
//! This test verifies that when didChange uses incremental sync (with range),
//! the apply_edits path (Phase 4) correctly processes the edit and updates
//! region tracking without over-invalidating ULIDs.
//!
//! Run with: `cargo test --test e2e_incremental_sync --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_polling::poll_for_hover;
use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: incremental sync (range-based didChange) works correctly
///
/// This test verifies Phase 4's apply_edits path:
/// 1. Open a markdown document with a Lua block
/// 2. Trigger hover to establish virtual document
/// 3. Send incremental didChange (with range, not full text)
/// 4. Verify hover still works (no error — region tracking correct, ULID preserved)
///
/// The key difference from e2e_didchange_forwarding is using incremental sync
/// with `range` instead of full document replacement.
#[test]
fn e2e_incremental_sync_preserves_region_tracking() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let (mut client, _config_dir) = create_lua_configured_client();

    // Phase 1: Open markdown document with Lua code block
    let markdown_uri = "file:///test_incremental_sync.md";
    // Line numbers (0-indexed):
    // 0: "# Test Document"
    // 1: ""
    // 2: "```lua"
    // 3: "local foo = 1"
    // 4: "print(foo)"    ← hover target: inside injection region
    // 5: "```"
    // 6: ""
    // 7: "More text."
    let initial_content =
        "# Test Document\n\n```lua\nlocal foo = 1\nprint(foo)\n```\n\nMore text.\n";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": initial_content
            }
        }),
    );

    // Give lua-ls time to initialize
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Phase 2: Poll for hover to establish the virtual document and confirm lua-ls is
    // indexed. A non-null result proves (a) the bridge routes to the Lua injection
    // region, and (b) lua-ls has the virtual document open — a prerequisite for
    // detecting ULID over-invalidation in Phase 4.
    // Line 4: "print(foo)", character 0 is inside the Lua injection region.
    let hover_before = poll_for_hover(&mut client, markdown_uri, 4, 0, 20, 500);

    println!(
        "Phase 2: Hover before incremental change: {:?}",
        hover_before
    );

    assert!(
        hover_before.is_some(),
        "Initial hover should return content after retries. \
         This indicates lua-ls may not be ready or there's a bridge routing issue."
    );

    // Phase 3: Send incremental didChange
    // Insert " = 2" at end of line 3 ("local foo = 1" → "local foo = 1 = 2").
    // Line 4 ("print(foo)") is unaffected — this is an INCREMENTAL change with range.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "version": 2
            },
            "contentChanges": [
                {
                    "range": {
                        "start": { "line": 3, "character": 13 },
                        "end": { "line": 3, "character": 13 }
                    },
                    "text": " = 2"
                }
            ]
        }),
    );

    println!("Phase 3: Sent incremental didChange (insert at end of line 3)");
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Phase 4: Verify hover still works on `print` at line 4 (unchanged by the edit).
    // Using poll_for_hover to detect ULID over-invalidation:
    // - If ULID preserved: bridge sends to the same virtual document URI → lua-ls
    //   (already indexed in Phase 2) returns non-null hover promptly.
    // - If ULID changed: bridge sends to an unknown virtual document URI → lua-ls
    //   consistently returns null → poll exhausts retries → assertion fails.
    // A short retry window (5 × 500 ms) is sufficient because lua-ls is already
    // indexed; only a brief re-indexing delay is expected after the incremental edit.
    let hover_after = poll_for_hover(&mut client, markdown_uri, 4, 0, 5, 500);

    println!("Phase 4: Hover after incremental change: {:?}", hover_after);

    assert!(
        hover_after.is_some(),
        "Hover should return content after incremental edit. \
         A persistent null result (after lua-ls was confirmed ready in Phase 2) \
         indicates ULID over-invalidation: the bridge sent to a different virtual \
         document URI that lua-ls has not opened."
    );

    println!("E2E: Incremental sync preserves region tracking - hover works after edit!");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: multiple incremental edits maintain correct region positions
///
/// This test verifies that multiple sequential incremental edits correctly
/// update region positions using the running coordinates approach.
#[test]
fn e2e_multiple_incremental_edits_maintain_positions() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let (mut client, _config_dir) = create_lua_configured_client();

    // Open markdown with two Lua blocks
    let markdown_uri = "file:///test_multi_incremental.md";
    // Line numbers (0-indexed):
    // 0:  "# Test"
    // 1:  ""
    // 2:  "```lua"
    // 3:  "local a = 1"
    // 4:  "print(a)"     ← hover target for first block
    // 5:  "```"
    // 6:  ""
    // 7:  "Middle text."
    // 8:  ""
    // 9:  "```lua"
    // 10: "local b = 2"
    // 11: "print(b)"     ← hover target for second block
    // 12: "```"
    let initial_content = "# Test\n\n```lua\nlocal a = 1\nprint(a)\n```\n\nMiddle text.\n\n```lua\nlocal b = 2\nprint(b)\n```\n";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": initial_content
            }
        }),
    );

    // Give lua-ls time to initialize
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Poll for hover on first Lua block to establish its virtual document and confirm
    // lua-ls has indexed it. Non-null result is required as a baseline for detecting
    // ULID over-invalidation after the incremental edits below.
    // Line 4: "print(a)", character 0 is inside the first Lua injection region.
    let hover1 = poll_for_hover(&mut client, markdown_uri, 4, 0, 20, 500);
    assert!(
        hover1.is_some(),
        "First Lua block hover should return content after retries. \
         This indicates lua-ls may not be ready or there's a bridge routing issue."
    );
    println!("Established first Lua block virtual document");

    // Poll for hover on second Lua block to establish its virtual document.
    // Line 11: "print(b)", character 0 is inside the second Lua injection region.
    let hover2 = poll_for_hover(&mut client, markdown_uri, 11, 0, 20, 500);
    assert!(
        hover2.is_some(),
        "Second Lua block hover should return content after retries. \
         This indicates lua-ls may not be ready or there's a bridge routing issue."
    );
    println!("Established second Lua block virtual document");

    // Send multiple incremental edits in sequence.
    // Edit 1: Insert newline after "Middle text." (shifts second Lua block down by 1 line).
    // "Middle text." is at line 7, character 12 (end of "Middle text.")
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "version": 2
            },
            "contentChanges": [
                {
                    "range": {
                        "start": { "line": 7, "character": 12 },
                        "end": { "line": 7, "character": 12 }
                    },
                    "text": "\nExtra line."
                }
            ]
        }),
    );
    println!(
        "Sent edit 1: Insert newline after 'Middle text.' (shifts second block to lines 10-13)"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Edit 2: Insert character in first Lua block (does not shift any lines).
    // Line 3: "local a = 1" (11 chars) → insert "0" at char 11 → "local a = 10"
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "version": 3
            },
            "contentChanges": [
                {
                    "range": {
                        "start": { "line": 3, "character": 11 },
                        "end": { "line": 3, "character": 11 }
                    },
                    "text": "0"
                }
            ]
        }),
    );
    println!("Sent edit 2: Insert '0' making 'local a = 10'");
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Verify first Lua block still works (after both edits).
    // Line 4: "print(a)" - unchanged (edit 2 modified line 3, not line 4).
    // Short retry window: lua-ls is already indexed; a persistent null result
    // indicates ULID over-invalidation for the first block.
    let hover1_after = poll_for_hover(&mut client, markdown_uri, 4, 0, 5, 500);
    assert!(
        hover1_after.is_some(),
        "First Lua block hover should return content after multiple incremental edits. \
         A persistent null result indicates ULID over-invalidation."
    );
    println!("First Lua block hover works after multiple incremental edits");

    // Verify second Lua block still works (now shifted to line 12).
    // Edit 1 inserted a line after "Middle text." (line 7), shifting lines 8+ by 1.
    // "print(b)" was at line 11, now at line 12.
    // Short retry window: ULID stability check — if region positions shifted correctly,
    // the same ULID should resolve to the new line; if over-invalidated, null is persistent.
    let hover2_after = poll_for_hover(&mut client, markdown_uri, 12, 0, 5, 500);
    assert!(
        hover2_after.is_some(),
        "Second Lua block hover should return content at new position (line 12). \
         A persistent null result indicates ULID over-invalidation or incorrect position shift."
    );
    println!("Second Lua block hover works after position shift");

    // All assertions passed
    println!("E2E: Multiple incremental edits maintain correct region positions");

    shutdown_client(&mut client);
}
