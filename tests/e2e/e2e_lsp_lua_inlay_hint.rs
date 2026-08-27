//! End-to-end test for Lua inlay hints in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for inlay hints:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Inlay hint request with range in Lua block
//! - kakehashi detects injection, translates range, spawns lua-ls
//! - Inlay hints received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --features e2e --test e2e e2e_lsp_lua_inlay_hint::`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_mock_inlay_hint_client(
    mode: &str,
    language: &str,
    host: bool,
    cancel_dir: Option<&std::path::Path>,
) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("inlay_hint_resolve.toml");
    std::fs::write(&config_path, "").expect("write config");
    let builder = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"));
    let builder = match cancel_dir {
        Some(dir) => builder.env("MOCK_LSP_CANCEL_DIR", dir.to_string_lossy()),
        None => builder,
    };
    let mut client = builder.build();
    let mut initialization_options = json!({ "languageServers": {
        "mock-inlay-hint": {
            "cmd": [mock_formatter_bin(), mode],
            "languages": [language]
        }
    }});
    if host {
        initialization_options["languages"] = json!({
            (language): { "bridge": { "_self": { "enabled": true } } }
        });
    }
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": initialization_options
        }),
    );
    assert_eq!(
        init["result"]["capabilities"]["inlayHintProvider"]["resolveProvider"],
        json!(true)
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

fn inlay_hints_with_retry(
    client: &mut LspClient,
    uri: &str,
    start_line: u64,
    end_line: u64,
) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/inlayHint",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": start_line, "character": 0 },
                    "end": { "line": end_line, "character": 0 }
                }
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        if let Some(hints) = response["result"].as_array()
            && !hints.is_empty()
        {
            return hints.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for inlay hints");
}

/// E2E test: inlay hint request is handled without error
#[test]
fn e2e_inlay_hint_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let (mut client, _config_dir) = create_lua_configured_client();

    // Open markdown document with Lua code block
    // lua-language-server provides type hints for variables
    let markdown_content = r#"# Test Document

```lua
local function add(a, b)
    local result = a + b
    return result
end

local sum = add(1, 2)
print(sum)
```

More text.
"#;

    let markdown_uri = "file:///test_inlay_hint.md";

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

    // Request inlay hints for the Lua code block area
    // The code block is at lines 2-11 (0-indexed: line 2 is "```lua", line 11 is "```")
    // Content starts at line 3
    let inlay_hint_response = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": markdown_uri },
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 11, "character": 0 }
            }
        }),
    );

    println!("Inlay hint response: {:?}", inlay_hint_response);

    // Verify no error
    assert!(
        inlay_hint_response.get("error").is_none(),
        "Inlay hint should not return error: {:?}",
        inlay_hint_response.get("error")
    );

    let result = inlay_hint_response
        .get("result")
        .expect("Inlay hint should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading or if no hints available
        println!("Note: lua-ls returned null (may still be loading or no hints available)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // InlayHint[] format
        let hints = result.as_array().unwrap();
        println!("Inlay hints found: {} items", hints.len());

        // If hints are returned, verify coordinates are in host document range
        for hint in hints {
            if let Some(position) = hint.get("position") {
                let line = position["line"].as_u64().unwrap_or(0);
                println!("  - Hint at line {}", line);
                // The hints should be in the Lua code block area (lines 3-10)
                assert!(
                    (2..=12).contains(&line),
                    "Hint line should be in host coordinates (expected 2-12, got {})",
                    line
                );
            }
        }
        println!("E2E: Inlay hint returns hints with host coordinates");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

#[test]
fn e2e_inlay_hint_resolve_round_trips_to_virtual_origin() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_resolve.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "# Test\n\n```lua\nlocal x = 1\n```\n"
        }}),
    );

    let hint = inlay_hints_with_retry(&mut client, uri, 3, 5).remove(0);
    assert_eq!(hint["position"], json!({ "line": 3, "character": 1 }));
    assert_eq!(hint["data"]["kakehashi"]["origin"], "mock-inlay-hint");

    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    let resolved = &response["result"];
    assert_eq!(resolved["tooltip"], "mock resolved:hint-1");
    assert_eq!(resolved["position"], hint["position"]);
    assert_eq!(
        resolved["data"]["kakehashi"]["inner"]["receivedPosition"],
        json!({ "line": 0, "character": 1 })
    );
    assert_eq!(
        resolved["textEdits"][0]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );

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
    let stale = client.send_request("inlayHint/resolve", hint);
    assert!(stale.get("error").is_none(), "{stale}");
    assert!(stale["result"].get("tooltip").is_none());

    shutdown_client(&mut client);
}

#[test]
fn e2e_inlay_hint_resolve_accepts_offset_frontmatter_region() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "yaml", false, None);
    let uri = "file:///test_inlay_hint_resolve_frontmatter.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "---\ntitle: test\n---\n"
        }}),
    );

    let hint = inlay_hints_with_retry(&mut client, uri, 1, 2).remove(0);
    assert_eq!(hint["position"], json!({ "line": 1, "character": 1 }));
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["tooltip"], "mock resolved:hint-1");
    assert_eq!(response["result"]["position"], hint["position"]);
    assert_eq!(
        response["result"]["data"]["kakehashi"]["inner"]["receivedPosition"],
        json!({ "line": 0, "character": 1 })
    );
    assert_eq!(
        response["result"]["textEdits"][0]["range"]["start"],
        json!({ "line": 1, "character": 0 })
    );

    shutdown_client(&mut client);
}

#[test]
fn e2e_inlay_hint_resolve_round_trips_to_host_origin() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", true, None);
    let uri = "file:///test_inlay_hint_resolve.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "local x = 1\n"
        }}),
    );

    let hint = inlay_hints_with_retry(&mut client, uri, 0, 1).remove(0);
    assert_eq!(hint["position"], json!({ "line": 0, "character": 1 }));
    assert_eq!(hint["data"]["kakehashi"]["host_layer"], true);
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["tooltip"], "mock resolved:hint-1");
    assert_eq!(response["result"]["position"], hint["position"]);
    assert_eq!(
        response["result"]["data"]["kakehashi"]["inner"]["receivedPosition"],
        hint["position"]
    );

    shutdown_client(&mut client);
}

#[test]
fn e2e_virtual_inlay_hint_from_replaced_producer_stays_unresolved() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_replacement.md";
    let open = |client: &mut LspClient| {
        client.send_notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Test\n\n```lua\nlocal x = 1\n```\n"
            }}),
        );
    };
    open(&mut client);
    let old_hint = inlay_hints_with_retry(&mut client, uri, 3, 5).remove(0);

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "languageServers": {
            "mock-inlay-hint": {
                "cmd": [mock_formatter_bin(), "inlay-hint-resolve-replacement"],
                "languages": ["lua"]
            }
        }}}),
    );
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    open(&mut client);
    let replacement_hint = inlay_hints_with_retry(&mut client, uri, 3, 5).remove(0);
    assert_eq!(
        old_hint["data"]["kakehashi"]["connection_key"],
        replacement_hint["data"]["kakehashi"]["connection_key"]
    );
    assert_ne!(
        old_hint["data"]["kakehashi"]["connection_generation"],
        replacement_hint["data"]["kakehashi"]["connection_generation"]
    );
    let replacement = client.send_request("inlayHint/resolve", replacement_hint);
    assert_eq!(
        replacement["result"]["tooltip"],
        "replacement resolved:hint-1"
    );

    let old_data = old_hint["data"].clone();
    let stale = client.send_request("inlayHint/resolve", old_hint);
    assert!(stale.get("error").is_none(), "{stale}");
    assert!(stale["result"].get("tooltip").is_none());
    assert_eq!(stale["result"]["data"], old_data);

    shutdown_client(&mut client);
}

#[test]
fn e2e_host_inlay_hint_resolve_cancel_targets_downstream_request() {
    let cancel_dir = tempfile::TempDir::new().expect("cancel dir");
    let request_file = cancel_dir
        .path()
        .join("inlay-hint-slow-resolve.request.json");
    let cancel_file = cancel_dir
        .path()
        .join("inlay-hint-slow-resolve.cancel.json");
    let (mut client, _config_dir) = init_mock_inlay_hint_client(
        "inlay-hint-slow-resolve",
        "lua",
        true,
        Some(cancel_dir.path()),
    );
    let uri = "file:///test_host_inlay_hint_cancel.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "local x = 1\n"
        }}),
    );
    let hint = inlay_hints_with_retry(&mut client, uri, 0, 1).remove(0);

    let request_id = client.send_request_async("inlayHint/resolve", hint);
    let started =
        client.wait_for_notification("window/logMessage", std::time::Duration::from_secs(10));
    assert!(
        started.as_ref().is_some_and(|params| params["message"]
            .as_str()
            .is_some_and(|message| message.contains("inlay-hint-resolve-started"))),
        "resolve must reach the downstream sent-state barrier: {started:?}"
    );
    assert!(request_file.exists(), "downstream request must be recorded");
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["error"]["code"], -32800, "{response}");
    let forwarded = (0..200).any(|_| {
        if cancel_file.exists() {
            true
        } else {
            std::thread::sleep(std::time::Duration::from_millis(50));
            false
        }
    });
    assert!(forwarded, "cancel must reach the downstream server");
    let request_event: Value =
        serde_json::from_slice(&std::fs::read(&request_file).expect("read request event"))
            .expect("parse request event");
    let cancel_event: Value =
        serde_json::from_slice(&std::fs::read(&cancel_file).expect("read cancel event"))
            .expect("parse cancel event");
    assert_eq!(cancel_event["params"]["id"], request_event["id"]);

    shutdown_client(&mut client);
}
