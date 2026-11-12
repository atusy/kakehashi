//! E2E: a region an edit makes kakehashi mint anew leaves no virtual document
//! behind.
//!
//! A region's identity is kept across edits by predicting where its node
//! lands. When the reparsed tree puts the node elsewhere, the region is minted
//! under a new id and opened under a new virtual URI; the document opened under
//! the old id must be closed. A server that reads every open document together
//! (gopls) otherwise sees the same code twice and reports each declaration as
//! a redeclaration.
//!
//! Uses the `mock-lsp-formatter` binary in `open-documents` mode: its hover
//! names the asked document's text and every URI the process holds open.
//!
//! Run with: `cargo test --features e2e --test e2e e2e_reminted_region_close::`

use std::time::{Duration, Instant};

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Path to the mock downstream server binary (built by Cargo for E2E runs).
fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client() -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("reminted_region_close.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf8 path"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-open": { "cmd": [mock_bin(), "open-documents"], "languages": ["lua"] }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// What the downstream server holds, as its hover at `position` reports it:
/// the text of the document the position maps into, and every open URI.
fn downstream_state(
    client: &mut LspClient,
    uri: &str,
    position: (u32, u32),
) -> Option<(String, Vec<String>)> {
    let response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": position.0, "character": position.1 },
        }),
    );
    assert!(
        response.get("error").is_none(),
        "hover must not error: {response:?}"
    );
    let contents = response.pointer("/result/contents")?.as_str()?;
    let state: Value = serde_json::from_str(contents).ok()?;
    let text = state.get("text")?.as_str()?.to_string();
    let open = state
        .get("open")?
        .as_array()?
        .iter()
        .filter_map(|uri| uri.as_str().map(str::to_string))
        .collect();
    Some((text, open))
}

/// Wait until the server holds the region as `text` in its only open document.
fn expect_single_document(client: &mut LspClient, uri: &str, position: (u32, u32), text: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last = None;
    while Instant::now() < deadline {
        last = downstream_state(client, uri, position);
        if let Some((held, open)) = &last
            && held == text
            && open.len() == 1
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the server must hold {text:?} in its only open document, last saw {last:?}");
}

fn shutdown(client: &mut LspClient) {
    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// A line inserted where a fence's content ends (a line pasted above the
/// closing fence, Vim's `O` on it) grows the content node at its end, which
/// the identity prediction reads as an edit after the node.
#[test]
fn e2e_line_inserted_above_the_closing_fence_leaves_one_virtual_document() {
    let (mut client, _config_dir) = init_client();
    let uri = "file:///test_reminted_region_insert.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Title\n\n```lua\nlocal x = 1\n```\n"
            }
        }),
    );
    expect_single_document(&mut client, uri, (3, 6), "local x = 1\n");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 4, "character": 0 },
                    "end": { "line": 4, "character": 0 }
                },
                "text": "local y = 2\n"
            }]
        }),
    );

    expect_single_document(&mut client, uri, (3, 6), "local x = 1\nlocal y = 2\n");
    shutdown(&mut client);
}

/// A whole-document replacement that moves the fence into a list item: the
/// text-diff prediction keeps the old region where no node is any more.
#[test]
fn e2e_whole_document_replacement_leaves_one_virtual_document() {
    let (mut client, _config_dir) = init_client();
    let uri = "file:///test_reminted_region_replace.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Title\n\n```lua\n      local x = 1\n```\n\nafter\n"
            }
        }),
    );
    expect_single_document(&mut client, uri, (3, 12), "      local x = 1\n");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "text": "# Title\n\n- item\n\n  ```lua\n  local x = 1\n  ```\n\nafter\n"
            }]
        }),
    );

    expect_single_document(&mut client, uri, (5, 8), "local x = 1\n");
    shutdown(&mut client);
}
