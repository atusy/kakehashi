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
    create_lua_configured_client_with_workspace, shutdown_client, skip_if_lua_ls_unavailable,
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

    let (mut client, workspace_dir, _config_dir) = create_lua_configured_client_with_workspace();

    // Phase 1: Open markdown document with Lua code block
    // Write the file to a real workspace directory so lua-ls can index it.
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

    let md_path = workspace_dir.path().join("test_incremental_sync.md");
    std::fs::write(&md_path, initial_content).expect("Failed to write markdown file");
    let markdown_uri = format!("file://{}", md_path.display());

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

    // Phase 2: Poll for hover to establish the virtual document.
    // lua-ls may return null during workspace loading (non-deterministic), so a null
    // result is not a failure — it just means we can't verify ULID stability in Phase 4.
    // Line 4: "print(foo)", character 0 is inside the Lua injection region.
    let hover_before = poll_for_hover(&mut client, &markdown_uri, 4, 0, 20, 500);

    println!(
        "Phase 2: Hover before incremental change: {:?}",
        hover_before
    );

    if hover_before.is_none() {
        eprintln!(
            "Note: lua-ls returned null for hover (may still be loading). \
             Skipping ULID stability check, but verifying no error on incremental edit path."
        );
    }

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

    // Phase 4: Verify hover doesn't error after incremental edit.
    // If Phase 2 got content, we verify ULID stability (same virtual document URI).
    // If Phase 2 was null, we still verify the incremental edit path doesn't crash.
    let hover_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 0 }
        }),
    );

    println!(
        "Phase 4: Hover after incremental change: {:?}",
        hover_response
    );

    assert!(
        hover_response.get("error").is_none(),
        "Hover should not return error after incremental edit: {:?}",
        hover_response.get("error")
    );

    if hover_before.is_some() {
        // lua-ls was ready before the edit — verify it still works (ULID stability).
        let hover_after = poll_for_hover(&mut client, &markdown_uri, 4, 0, 5, 500);
        assert!(
            hover_after.is_some(),
            "Hover should return content after incremental edit when lua-ls was ready before. \
             A persistent null result indicates ULID over-invalidation."
        );
        println!("E2E: ULID stability verified — hover works after incremental edit!");
    } else {
        println!(
            "E2E: Incremental edit processed without error (lua-ls not yet ready for ULID check)."
        );
    }

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

    let (mut client, workspace_dir, _config_dir) = create_lua_configured_client_with_workspace();

    // Open markdown with two Lua blocks
    // Write to a real workspace directory so lua-ls can index virtual documents.
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

    let md_path = workspace_dir.path().join("test_multi_incremental.md");
    std::fs::write(&md_path, initial_content).expect("Failed to write markdown file");
    let markdown_uri = format!("file://{}", md_path.display());

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

    // Poll for hover on first Lua block to establish its virtual document.
    // lua-ls may return null during workspace loading (non-deterministic).
    // Line 4: "print(a)", character 0 is inside the first Lua injection region.
    let hover1 = poll_for_hover(&mut client, &markdown_uri, 4, 0, 20, 500);
    let lua_ls_ready = hover1.is_some();
    if lua_ls_ready {
        println!("Established first Lua block virtual document");
    } else {
        eprintln!("Note: lua-ls returned null for first block hover (may still be loading).");
    }

    // Poll for hover on second Lua block to establish its virtual document.
    // Line 11: "print(b)", character 0 is inside the second Lua injection region.
    if lua_ls_ready {
        let hover2 = poll_for_hover(&mut client, &markdown_uri, 11, 0, 20, 500);
        if hover2.is_some() {
            println!("Established second Lua block virtual document");
        } else {
            eprintln!("Note: lua-ls returned null for second block hover.");
        }
    }

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

    // Verify hover doesn't error after multiple incremental edits.
    // Line 4: "print(a)" - unchanged (edit 2 modified line 3, not line 4).
    let hover1_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 0 }
        }),
    );
    assert!(
        hover1_response.get("error").is_none(),
        "First block hover should not error after incremental edits: {:?}",
        hover1_response.get("error")
    );

    // Verify second Lua block hover doesn't error at new position (line 12).
    // Edit 1 inserted a line after "Middle text." (line 7), shifting lines 8+ by 1.
    // "print(b)" was at line 11, now at line 12.
    let hover2_response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 12, "character": 0 }
        }),
    );
    assert!(
        hover2_response.get("error").is_none(),
        "Second block hover should not error at shifted position: {:?}",
        hover2_response.get("error")
    );

    if lua_ls_ready {
        // lua-ls was ready before edits — verify ULID stability (content still available).
        let hover1_after = poll_for_hover(&mut client, &markdown_uri, 4, 0, 5, 500);
        assert!(
            hover1_after.is_some(),
            "First Lua block hover should return content after edits when lua-ls was ready before. \
             A persistent null result indicates ULID over-invalidation."
        );
        println!("First Lua block ULID stability verified");

        let hover2_after = poll_for_hover(&mut client, &markdown_uri, 12, 0, 5, 500);
        assert!(
            hover2_after.is_some(),
            "Second Lua block hover should return content at new position (line 12). \
             A persistent null result indicates ULID over-invalidation or incorrect position shift."
        );
        println!("Second Lua block ULID stability verified at shifted position");
        println!(
            "E2E: Multiple incremental edits maintain correct region positions (full verification)"
        );
    } else {
        println!(
            "E2E: Multiple incremental edits processed without error (lua-ls not yet ready for ULID check)."
        );
    }

    shutdown_client(&mut client);
}
