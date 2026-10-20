//! End-to-end tests for inlay hints through the kakehashi binary.
//!
//! The first test drives `textDocument/inlayHint` for a Lua block in Markdown
//! through a real lua-language-server (skipped when it is not in PATH) and
//! checks the request succeeds, with any hints inside the block's host
//! lines. The other tests drive
//! `inlayHint/resolve` through the mock server's `inlay-hint-*` modes (see
//! `tests/bin/mock_formatter.rs`) and between them cover both the injection
//! and host layers: routing back to the exact producer, coordinate reversal,
//! freshness gates, cancellation, and the edit guard.
//!
//! Run with: `cargo test --features e2e --test e2e e2e_lsp_lua_inlay_hint::`

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
    init_mock_inlay_hint_client_with_capabilities(mode, language, host, cancel_dir, json!({}))
}

fn init_mock_inlay_hint_client_with_capabilities(
    mode: &str,
    language: &str,
    host: bool,
    cancel_dir: Option<&std::path::Path>,
    capabilities: Value,
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
            "capabilities": capabilities,
            "workspaceFolders": null,
            "initializationOptions": initialization_options
        }),
    );
    assert_eq!(
        init["result"]["capabilities"]["inlayHintProvider"]["resolveProvider"],
        json!(true)
    );
    assert_eq!(
        init["result"]["capabilities"]["executeCommandProvider"]["commands"],
        json!([])
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

fn init_combined_marker_inlay_hint_client(
    marker_dir: &std::path::Path,
) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("inlay_hint_combined.toml");
    std::fs::write(&config_path, "").expect("write config");
    let query_path = config_dir.path().join("combined-injections.scm");
    std::fs::write(
        &query_path,
        r#"
        (fenced_code_block
          (info_string (language) @injection.language)
          (code_fence_content) @injection.content
          (#set! injection.combined)
          (#set! injection.include-children))
        "#,
    )
    .expect("write combined injection query");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env("MOCK_LSP_CANCEL_DIR", marker_dir.to_string_lossy())
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
                    "mock-inlay-hint": {
                        "cmd": [mock_formatter_bin(), "inlay-hint-marker-resolve"],
                        "languages": ["lua"]
                    }
                },
                "languages": {
                    "markdown": {
                        "queries": [{
                            "path": query_path.to_str().expect("UTF-8 query path"),
                            "kind": "injections"
                        }]
                    }
                }
            }
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

fn inlay_hints_once(
    client: &mut LspClient,
    uri: &str,
    start_line: u64,
    end_line: u64,
) -> Vec<Value> {
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
    response["result"]
        .as_array()
        .unwrap_or_else(|| panic!("expected inlay hints: {response}"))
        .clone()
}

/// Block until the mock resolver reports `inlay-hint-resolve-started`, the
/// sent-state barrier the delayed and slow resolver modes raise on receipt.
/// kakehashi forwards its own `window/logMessage` lines too (query loading,
/// for one), so the first log message on the wire is not necessarily the
/// mock's; skip anything else until the deadline.
fn wait_for_resolve_started(client: &mut LspClient) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let Some(params) = client.wait_for_notification("window/logMessage", remaining) else {
            panic!("resolve never reached the downstream sent-state barrier");
        };
        if params["message"]
            .as_str()
            .is_some_and(|message| message.contains("inlay-hint-resolve-started"))
        {
            return;
        }
    }
}

/// A label-part command that left the bridge carries the routing prefix
/// `workspace/executeCommand` decodes, with the downstream'"'"'s own name last.
fn assert_routed_command(command: &Value, downstream_name: &str) {
    let command = command.as_str().expect("a routed command name");
    assert!(
        command.starts_with("kakehashi|") && command.ends_with(&format!("|{downstream_name}")),
        "expected a routed form of {downstream_name}, got {command}"
    );
}

fn resolve_request_observation(resolved: &Value) -> Value {
    serde_json::from_str(
        resolved["label"][0]["tooltip"]
            .as_str()
            .expect("mock resolve observation tooltip"),
    )
    .expect("parse mock resolve observation")
}

fn wait_for_injected_node(client: &mut LspClient, uri: &str, line: u64) {
    for _ in 0..300 {
        let response = client.send_request(
            "kakehashi/node",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": 1 },
                "injection": true
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        if !response["result"].is_null() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for current injected parse");
}

#[test]
fn inlay_hint_commands_are_registered_without_code_action_support() {
    for host in [false, true] {
        let (mut client, _config_dir) = init_mock_inlay_hint_client_with_capabilities(
            "inlay-hint-resolve",
            "lua",
            host,
            None,
            json!({"workspace": {"executeCommand": {"dynamicRegistration": true}}}),
        );
        let (uri, language, text, start, end) = if host {
            ("file:///registered-hint.lua", "lua", "local x = 1\n", 0, 1)
        } else {
            (
                "file:///registered-hint.md",
                "markdown",
                "# Test\n\n```lua\nlocal x = 1\n```\n",
                3,
                5,
            )
        };
        client.send_notification(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": language, "version": 1, "text": text,
            }}),
        );
        let (registration_id, params) = client
            .wait_for_server_request(
                "client/registerCapability",
                std::time::Duration::from_secs(5),
            )
            .expect("hint producer must register its command names");
        let commands: Vec<&str> = params["registrations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["method"] == "workspace/executeCommand")
            .flat_map(|entry| entry["registerOptions"]["commands"].as_array().unwrap())
            .filter_map(Value::as_str)
            .collect();
        client.send_response(registration_id, json!(null));
        let hint = inlay_hints_with_retry(&mut client, uri, start, end).remove(0);
        let resolved = client.send_request("inlayHint/resolve", hint.clone());
        assert!(resolved.get("error").is_none(), "{resolved}");
        for (item, expected) in [(&hint, "mock.hint"), (&resolved["result"], "mock.resolved")] {
            let command = &item["label"][0]["command"];
            let name = command["command"].as_str().expect("hint command name");
            assert!(
                commands.contains(&name),
                "surfaced command must be registered: {name}"
            );
            let response = client.send_request(
                "workspace/executeCommand",
                json!({
                    "command": name,
                    "arguments": command["arguments"].as_array().cloned().unwrap_or_default(),
                }),
            );
            assert!(response.get("error").is_none(), "{response}");
            assert_eq!(response["result"]["executed"], expected);
        }
        shutdown_client(&mut client);
    }
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
    assert_eq!(
        hint["textEdits"][0]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );
    assert_eq!(hint["label"][0]["location"]["uri"], uri);
    assert_eq!(
        hint["label"][0]["location"]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );
    assert_routed_command(&hint["label"][0]["command"]["command"], "mock.hint");

    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    let resolved = &response["result"];
    let observed = resolve_request_observation(resolved);
    assert_eq!(resolved["tooltip"], "mock resolved:hint-1");
    assert_eq!(resolved["position"], hint["position"]);
    assert_eq!(
        observed["receivedPosition"],
        json!({ "line": 0, "character": 1 })
    );
    assert_eq!(
        observed["receivedTextEdit"]["start"],
        json!({ "line": 0, "character": 0 })
    );
    assert!(
        observed["receivedLocation"]["uri"]
            .as_str()
            .is_some_and(|uri| uri.contains("kakehashi-virtual-uri-"))
    );
    assert_eq!(
        observed["receivedLocation"]["range"]["start"],
        json!({ "line": 0, "character": 0 })
    );
    assert_eq!(observed["receivedCommand"], "mock.hint");
    assert_eq!(
        resolved["data"]["kakehashi"]["inner"],
        hint["data"]["kakehashi"]["inner"]
    );
    assert_eq!(
        resolved["textEdits"][0]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );
    assert_eq!(resolved["label"][0]["location"]["uri"], uri);
    assert_eq!(
        resolved["label"][0]["location"]["range"]["start"],
        json!({ "line": 3, "character": 0 })
    );
    assert_routed_command(&resolved["label"][0]["command"]["command"], "mock.resolved");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 3, "character": 6 },
                    "end": { "line": 3, "character": 7 }
                },
                "text": "y"
            }]
        }),
    );
    let stale = client.send_request("inlayHint/resolve", hint.clone());
    assert!(stale.get("error").is_none(), "{stale}");
    assert!(stale["result"].get("tooltip").is_none());
    assert_eq!(stale["result"]["position"], hint["position"]);

    shutdown_client(&mut client);
}

#[test]
fn e2e_inlay_hint_resolve_discards_response_after_same_shape_edit() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-delayed-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_resolve_didchange_during_wait.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\nlocal x = 1\n```\n"
        }}),
    );
    let hint = inlay_hints_with_retry(&mut client, uri, 1, 3).remove(0);

    let request_id = client.send_request_async("inlayHint/resolve", hint.clone());
    wait_for_resolve_started(&mut client);
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 1, "character": 6 },
                    "end": { "line": 1, "character": 7 }
                },
                "text": "y"
            }]
        }),
    );
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["result"], hint);

    shutdown_client(&mut client);
}

/// The bridge releases the host lifecycle once the resolve is enqueued, so a
/// close and reopen can complete while the downstream is still answering.
/// The reopened document restarts its content version at the value the hint
/// was minted with, so the host incarnation is the only stamp left to
/// refuse the reply.
#[test]
fn e2e_inlay_hint_resolve_discards_response_after_reopen_during_wait() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-reopen-delayed-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_resolve_reopen_during_wait.md";
    let open = json!({ "textDocument": {
        "uri": uri,
        "languageId": "markdown",
        "version": 1,
        "text": "```lua\nlocal x = 1\n```\n"
    }});
    client.send_notification("textDocument/didOpen", open.clone());
    let hint = inlay_hints_with_retry(&mut client, uri, 1, 3).remove(0);
    assert_eq!(hint["data"]["kakehashi"]["content_version"], json!(0));

    let request_id = client.send_request_async("inlayHint/resolve", hint.clone());
    wait_for_resolve_started(&mut client);
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    client.send_notification("textDocument/didOpen", open);
    // The mock answers the parked resolve when the reopened document asks for
    // hints again, so the reply is evaluated against the reopened
    // incarnation. This request is only that trigger; its own reply is not
    // awaited, so it cannot swallow the resolve reply.
    let _trigger = client.send_request_async(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 1, "character": 0 },
                "end": { "line": 3, "character": 0 }
            }
        }),
    );
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["result"], hint, "{response}");

    // The reopened document mints under a new incarnation but the same
    // content version, so the incarnation is what refused the reply above.
    let reopened = inlay_hints_with_retry(&mut client, uri, 1, 3).remove(0);
    assert_eq!(
        reopened["data"]["kakehashi"]["content_version"],
        hint["data"]["kakehashi"]["content_version"]
    );
    assert_ne!(
        reopened["data"]["kakehashi"]["incarnation"],
        hint["data"]["kakehashi"]["incarnation"]
    );

    shutdown_client(&mut client);
}

/// A lazily materialized edit passes the same region guard as an eager one;
/// when it is refused the eager edit set the hint already carried stays.
#[test]
fn e2e_inlay_hint_resolve_drops_an_escaping_lazy_edit_and_keeps_the_eager_one() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-escaping-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_resolve_escaping_edit.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\nlocal x = 1\n```\n"
        }}),
    );
    let hint = inlay_hints_with_retry(&mut client, uri, 1, 3).remove(0);
    assert_eq!(hint["textEdits"][0]["newText"], "existing ");

    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["tooltip"], "mock resolved:hint-1");
    assert_eq!(
        response["result"]["textEdits"], hint["textEdits"],
        "the escaping lazy edit must be dropped in favour of the eager set"
    );

    shutdown_client(&mut client);
}

#[test]
fn e2e_inlay_hint_resolve_rejects_live_non_contiguous_region_before_dispatch() {
    let marker_dir = tempfile::TempDir::new().expect("marker dir");
    let marker = marker_dir
        .path()
        .join("inlay-hint-marker-resolve.request.json");
    let (mut client, _config_dir) = init_combined_marker_inlay_hint_client(marker_dir.path());
    let uri = "file:///test_inlay_hint_resolve_combined.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\nlocal x = 1\n```\n"
        }}),
    );
    let mut hint = inlay_hints_with_retry(&mut client, uri, 1, 3).remove(0);
    // The content version counts text mutations since didOpen, so a hint
    // minted before any edit carries 0. Inlay hints carry edits and are never
    // minted for a non-contiguous region, so the stamps below are forged from
    // that baseline rather than re-minted.
    assert_eq!(hint["data"]["kakehashi"]["content_version"], json!(0));

    // Positive control for the forged stamp: after one in-fence edit the
    // region is unchanged and contiguous, so the hint stamped with the live
    // version must reach the downstream.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "```lua\nlocal y = 1\n```\n" }]
        }),
    );
    hint["data"]["kakehashi"]["content_version"] = json!(1);
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"]["tooltip"], "mock resolved:hint-1",
        "a live stamp on an unchanged contiguous region must resolve: {response}"
    );
    assert!(
        marker.exists(),
        "the control resolve must reach the downstream"
    );
    std::fs::remove_file(&marker).expect("clear the request marker");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 3 },
            "contentChanges": [{
                "text": "```lua\nlocal x = 1\n```\n\nprose\n\n```lua\nlocal y = 2\n```\n"
            }]
        }),
    );
    wait_for_injected_node(&mut client, uri, 7);

    let current = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 7, "character": 1 }
        }),
    );
    assert!(current.get("error").is_none(), "{current}");
    let current: Value = serde_json::from_str(
        current["result"]["contents"]
            .as_str()
            .expect("combined hover observation"),
    )
    .expect("parse combined hover observation");
    let region_id = hint["data"]["kakehashi"]["region_id"]
        .as_str()
        .expect("enveloped region id");
    assert!(
        current["uri"]
            .as_str()
            .is_some_and(|uri| uri.contains(&format!("kakehashi-virtual-uri-{region_id}.lua"))),
        "the edited combined region must retain the exact producer region id: {current}"
    );
    assert_eq!(
        7 - current["position"]["line"].as_u64().expect("virtual line"),
        hint["data"]["kakehashi"]["offset"]["line"]
            .as_u64()
            .expect("enveloped line offset")
    );
    assert_eq!(
        1 - current["position"]["character"]
            .as_u64()
            .expect("virtual character"),
        hint["data"]["kakehashi"]["offset"]["column"]
            .as_u64()
            .expect("enveloped column offset")
    );

    // The stamp is now one edit behind: that is refused before dispatch,
    // whatever the region looks like.
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], hint);
    assert!(
        !marker.exists(),
        "a content-stale resolve must not reach the downstream"
    );

    // With the live stamp (two edits), the only thing between the request and
    // the downstream is the geometry guard: the once-single combined capture
    // is now non-contiguous.
    hint["data"]["kakehashi"]["content_version"] = json!(2);
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], hint);
    assert!(
        !marker.exists(),
        "a non-contiguous region must not reach the downstream"
    );

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
        resolve_request_observation(&response["result"])["receivedPosition"],
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
    assert_routed_command(&hint["label"][0]["command"]["command"], "mock.hint");
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["tooltip"], "mock resolved:hint-1");
    assert_eq!(response["result"]["position"], hint["position"]);
    let observed = resolve_request_observation(&response["result"]);
    assert_eq!(observed["receivedPosition"], hint["position"]);
    assert_eq!(observed["receivedTextEdit"], hint["textEdits"][0]["range"]);
    assert_eq!(observed["receivedLocation"], hint["label"][0]["location"]);
    assert_eq!(observed["receivedCommand"], "mock.hint");
    assert_routed_command(
        &response["result"]["label"][0]["command"]["command"],
        "mock.resolved",
    );

    shutdown_client(&mut client);
}

fn assert_virtual_inlay_hint_replacement_fails_soft(change_pool_key: bool) {
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

    let mut server = json!({
        "cmd": [mock_formatter_bin(), "inlay-hint-resolve-replacement"],
        "languages": ["lua"]
    });
    if change_pool_key {
        server["preferSharedInstance"] = json!(true);
    }
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "languageServers": {
            "mock-inlay-hint": server
        }}}),
    );
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    open(&mut client);
    let replacement_hint = inlay_hints_with_retry(&mut client, uri, 3, 5).remove(0);
    let old_envelope = &old_hint["data"]["kakehashi"];
    let replacement_envelope = &replacement_hint["data"]["kakehashi"];
    if change_pool_key {
        assert_ne!(
            old_envelope["connection_key"],
            replacement_envelope["connection_key"]
        );
        assert_eq!(
            old_envelope["connection_generation"],
            replacement_envelope["connection_generation"]
        );
    } else {
        assert_eq!(
            old_envelope["connection_key"],
            replacement_envelope["connection_key"]
        );
        assert_ne!(
            old_envelope["connection_generation"],
            replacement_envelope["connection_generation"]
        );
    }
    let replacement = client.send_request("inlayHint/resolve", replacement_hint.clone());
    assert_eq!(
        replacement["result"]["tooltip"],
        "replacement resolved:hint-1"
    );

    // Keep CURRENT incarnation + region geometry, replacing only producer
    // identity. This isolates the key/generation gates from freshness checks.
    let mut stale_hint = replacement_hint;
    stale_hint["data"]["kakehashi"]["connection_key"] = old_envelope["connection_key"].clone();
    stale_hint["data"]["kakehashi"]["connection_generation"] =
        old_envelope["connection_generation"].clone();
    stale_hint["data"]["kakehashi"]["inner"] = old_envelope["inner"].clone();
    let stale_data = stale_hint["data"].clone();
    let stale = client.send_request("inlayHint/resolve", stale_hint);
    assert!(stale.get("error").is_none(), "{stale}");
    assert!(stale["result"].get("tooltip").is_none());
    assert_eq!(stale["result"]["data"], stale_data);

    shutdown_client(&mut client);
}

#[test]
fn e2e_virtual_inlay_hint_from_same_key_replacement_stays_unresolved() {
    assert_virtual_inlay_hint_replacement_fails_soft(false);
}

#[test]
fn e2e_virtual_inlay_hint_from_different_key_replacement_stays_unresolved() {
    assert_virtual_inlay_hint_replacement_fails_soft(true);
}

fn assert_host_inlay_hint_replacement_fails_soft(change_pool_key: bool) {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", true, None);
    let uri = "file:///test_host_inlay_hint_replacement.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "local x = 1\n"
        }}),
    );
    let old_hint = inlay_hints_with_retry(&mut client, uri, 0, 1).remove(0);

    let mut server = json!({
        "cmd": [mock_formatter_bin(), "inlay-hint-resolve-replacement"],
        "languages": ["lua"]
    });
    if change_pool_key {
        server["preferSharedInstance"] = json!(true);
    }
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": {
            "languageServers": { "mock-inlay-hint": server },
            "languages": { "lua": { "bridge": { "_self": { "enabled": true } } } }
        }}),
    );
    let replacement_hint = inlay_hints_with_retry(&mut client, uri, 0, 1).remove(0);
    let old_envelope = &old_hint["data"]["kakehashi"];
    let replacement_envelope = &replacement_hint["data"]["kakehashi"];
    if change_pool_key {
        assert_ne!(
            old_envelope["connection_key"],
            replacement_envelope["connection_key"]
        );
        assert_eq!(
            old_envelope["connection_generation"],
            replacement_envelope["connection_generation"]
        );
    } else {
        assert_eq!(
            old_envelope["connection_key"],
            replacement_envelope["connection_key"]
        );
        assert_ne!(
            old_envelope["connection_generation"],
            replacement_envelope["connection_generation"]
        );
    }
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
fn e2e_host_inlay_hint_from_same_key_replacement_stays_unresolved() {
    assert_host_inlay_hint_replacement_fails_soft(false);
}

#[test]
fn e2e_host_inlay_hint_from_different_key_replacement_stays_unresolved() {
    assert_host_inlay_hint_replacement_fails_soft(true);
}

#[test]
fn e2e_inlay_hint_without_resolver_keeps_ordinary_data_bare_on_both_layers() {
    for host in [false, true] {
        let (mut client, _config_dir) =
            init_mock_inlay_hint_client("inlay-hint-no-resolve", "lua", host, None);
        let uri = if host {
            "file:///test_bare_host_inlay_hint.lua"
        } else {
            "file:///test_bare_virtual_inlay_hint.md"
        };
        let (language_id, text, start, end) = if host {
            ("lua", "local x = 1\n", 0, 1)
        } else {
            ("markdown", "```lua\nlocal x = 1\n```\n", 1, 3)
        };
        client.send_notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": language_id, "version": 1, "text": text
            }}),
        );
        let hint = inlay_hints_with_retry(&mut client, uri, start, end).remove(0);
        assert_eq!(hint["data"]["mock"], "hint-1");
        assert!(hint["data"].get("kakehashi").is_none());
        shutdown_client(&mut client);
    }
}

#[test]
fn e2e_inlay_hint_reserved_data_collision_round_trips_on_both_layers() {
    for host in [false, true] {
        let (mut client, _config_dir) =
            init_mock_inlay_hint_client("inlay-hint-no-resolve-reserved-data", "lua", host, None);
        let uri = if host {
            "file:///test_collision_host_inlay_hint.lua"
        } else {
            "file:///test_collision_virtual_inlay_hint.md"
        };
        let (language_id, text, start, end) = if host {
            ("lua", "local x = 1\n", 0, 1)
        } else {
            ("markdown", "```lua\nlocal x = 1\n```\n", 1, 3)
        };
        client.send_notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": language_id, "version": 1, "text": text
            }}),
        );
        let hint = inlay_hints_with_retry(&mut client, uri, start, end).remove(0);
        assert_eq!(
            hint["data"]["kakehashi"]["inner"],
            json!({ "kakehashi": { "origin": "downstream" } })
        );
        let response = client.send_request("inlayHint/resolve", hint.clone());
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"], hint);
        shutdown_client(&mut client);
    }
}

#[test]
fn e2e_inlay_hint_from_closed_incarnation_stays_unresolved_on_both_layers() {
    for host in [false, true] {
        let (mut client, _config_dir) =
            init_mock_inlay_hint_client("inlay-hint-resolve", "lua", host, None);
        let uri = if host {
            "file:///test_reopen_host_inlay_hint.lua"
        } else {
            "file:///test_reopen_virtual_inlay_hint.md"
        };
        let (language_id, text, start, end) = if host {
            ("lua", "local x = 1\n", 0, 1)
        } else {
            ("markdown", "```lua\nlocal x = 1\n```\n", 1, 3)
        };
        let open = |client: &mut LspClient| {
            client.send_notification(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": language_id, "version": 1, "text": text
                }}),
            );
        };
        open(&mut client);
        let old_hint = inlay_hints_with_retry(&mut client, uri, start, end).remove(0);
        client.send_notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
        open(&mut client);
        let current_hint = inlay_hints_with_retry(&mut client, uri, start, end).remove(0);
        let old_envelope = &old_hint["data"]["kakehashi"];
        let current_envelope = &current_hint["data"]["kakehashi"];
        assert_eq!(
            old_envelope["connection_key"],
            current_envelope["connection_key"]
        );
        assert_eq!(
            old_envelope["connection_generation"],
            current_envelope["connection_generation"]
        );
        assert_ne!(old_envelope["incarnation"], current_envelope["incarnation"]);

        let response = client.send_request("inlayHint/resolve", old_hint.clone());
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"], old_hint);
        shutdown_client(&mut client);
    }
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

    // Diverge the editor-side and downstream id counters first: a bare hint
    // passes through without reaching any server, so from here on the id the
    // mock records can only be the translated downstream one.
    let passthrough = client.send_request(
        "inlayHint/resolve",
        json!({ "position": { "line": 0, "character": 0 }, "label": "bare" }),
    );
    assert!(passthrough.get("error").is_none(), "{passthrough}");

    let request_id = client.send_request_async("inlayHint/resolve", hint);
    wait_for_resolve_started(&mut client);
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
    assert_ne!(
        request_event["id"],
        json!(request_id),
        "the downstream must see its own request id, not the editor's"
    );

    shutdown_client(&mut client);
}

/// Editors ask for the visible or the whole document's range, which starts
/// in host text before any injection: every region the range intersects is
/// bridged, and the hints are merged in document order. A range that stops
/// before a later region does not reach it.
#[test]
fn e2e_inlay_hint_range_starting_before_regions_covers_every_intersected_region() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_whole_document.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "# Test\n\n```lua\nlocal x = 1\n```\n\ntext\n\n```lua\nlocal y = 2\n```\n"
        }}),
    );

    // Warm both regions with in-region requests, so the assertions below see
    // live virtual documents and never accept a partial merge on a retry.
    inlay_hints_with_retry(&mut client, uri, 3, 4);
    inlay_hints_with_retry(&mut client, uri, 9, 10);

    let whole = inlay_hints_once(&mut client, uri, 0, 11);
    let positions: Vec<&Value> = whole.iter().map(|hint| &hint["position"]).collect();
    assert_eq!(
        positions,
        [
            &json!({ "line": 3, "character": 1 }),
            &json!({ "line": 9, "character": 1 })
        ],
        "{whole:?}"
    );
    assert_ne!(
        whole[0]["data"]["kakehashi"]["region_id"], whole[1]["data"]["kakehashi"]["region_id"],
        "each hint is enveloped for the region that produced it"
    );

    // Each region is asked for the range clipped to its content, which is
    // one line in both fences: virtual (0,0)-(1,0).
    let whole_line = json!({
        "start": { "line": 0, "character": 0 },
        "end": { "line": 1, "character": 0 }
    });
    for hint in &whole {
        assert_eq!(
            hint["data"]["kakehashi"]["inner"]["receivedRange"], whole_line,
            "{hint}"
        );
    }

    let first_only = inlay_hints_once(&mut client, uri, 0, 6);
    let positions: Vec<&Value> = first_only.iter().map(|hint| &hint["position"]).collect();
    assert_eq!(positions, [&json!({ "line": 3, "character": 1 })]);

    // A range from inside the first fence to inside the second is clipped at
    // both ends: the first region keeps its start, the second its end.
    let response = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 4 },
                "end": { "line": 9, "character": 5 }
            }
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let received: Vec<&Value> = response["result"]
        .as_array()
        .expect("hints for both regions")
        .iter()
        .map(|hint| &hint["data"]["kakehashi"]["inner"]["receivedRange"])
        .collect();
    assert_eq!(
        received,
        [
            &json!({
                "start": { "line": 0, "character": 4 },
                "end": { "line": 1, "character": 0 }
            }),
            &json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 5 }
            })
        ]
    );

    // The resolve of the later region's hint routes back to that region.
    let response = client.send_request("inlayHint/resolve", whole[1].clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["tooltip"], "mock resolved:hint-1");
    assert_eq!(response["result"]["position"], whole[1]["position"]);

    shutdown_client(&mut client);
}

/// The editor's work-done progress covers the whole request: every region
/// routes its downstream's progress into the one lifecycle on the editor's
/// token, so the progress ends only once every region settles rather than
/// when the first one does.
#[test]
fn e2e_inlay_hint_work_done_progress_spans_every_region() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_progress.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "# Test\n\n```lua\nlocal x = 1\n```\n\ntext\n\n```lua\nlocal y = 2\n```\n"
        }}),
    );
    inlay_hints_with_retry(&mut client, uri, 3, 4);
    inlay_hints_with_retry(&mut client, uri, 9, 10);

    let response = client.send_request(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 11, "character": 0 }
            },
            "workDoneToken": "editor-inlay-progress"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let hints = response["result"]
        .as_array()
        .expect("hints for both regions");
    assert_eq!(hints.len(), 2, "{response}");
    for hint in hints {
        let token = &hint["data"]["kakehashi"]["inner"]["receivedWorkDoneToken"];
        assert!(
            token.is_string() || token.is_number(),
            "every region's downstream must get a bridge progress token: {hint}"
        );
        assert_ne!(
            token,
            &json!("editor-inlay-progress"),
            "the downstream gets a bridge token, not the editor's"
        );
    }

    shutdown_client(&mut client);
}

const PROGRESS_TOKEN: &str = "editor-inlay-progress";
const HELD_REGION_TEXT: &str =
    "# Test\n\n```lua\nlocal x = 1\n```\n\ntext\n\n```lua\nlocal slow = 2\n```\n";

/// Open the two-fence document whose second fence the `inlay-hint-progress`
/// mock holds, warm both regions, and send a whole-document request carrying
/// the editor's work-done token. Returns the request id once the fast
/// region's Begin and Report reached the editor and a quiet window showed no
/// End and no answer while the held region is still in flight.
fn start_held_progress_request(client: &mut LspClient, uri: &str) -> (i64, Vec<String>) {
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": HELD_REGION_TEXT
        }}),
    );
    inlay_hints_with_retry(client, uri, 3, 4);
    inlay_hints_with_retry(client, uri, 9, 10);

    let id = client.send_request_async(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 11, "character": 0 }
            },
            "workDoneToken": PROGRESS_TOKEN
        }),
    );
    let mut kinds = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !kinds.iter().any(|kind| kind == "report") {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let message = client
            .receive_any_message(remaining)
            .expect("the fast region's progress must reach the editor");
        assert_ne!(
            message["id"],
            json!(id),
            "answered while a region is held: {message}"
        );
        record_progress(&message, &mut kinds);
    }
    // The fast region has reported; its answer follows on the same pipe. A
    // lifecycle tied to that region would end now, while the other is held.
    let quiet_until = std::time::Instant::now() + std::time::Duration::from_millis(700);
    loop {
        let remaining = quiet_until.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Some(message) = client.receive_any_message(remaining) else {
            break;
        };
        assert_ne!(
            message["id"],
            json!(id),
            "answered while a region is held: {message}"
        );
        record_progress(&message, &mut kinds);
    }
    assert_eq!(
        kinds,
        ["begin", "report"],
        "progress must stay open while a region is still in flight"
    );
    (id, kinds)
}

fn record_progress(message: &Value, kinds: &mut Vec<String>) {
    if message["method"] == "$/progress" && message["params"]["token"] == PROGRESS_TOKEN {
        kinds.push(
            message["params"]["value"]["kind"]
                .as_str()
                .expect("progress kind")
                .to_string(),
        );
    }
}

/// Collect messages until `id` and every id in `also` are answered and the
/// editor's progress ended, returning `id`'s answer. The other answers are
/// awaited here too, as they can interleave with it in either order.
fn finish_held_progress_request(
    client: &mut LspClient,
    id: i64,
    also: &[i64],
    kinds: &mut Vec<String>,
) -> Value {
    let mut response = None;
    let mut pending: Vec<i64> = also.to_vec();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while response.is_none() || !pending.is_empty() || !kinds.iter().any(|kind| kind == "end") {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let message = client.receive_any_message(remaining).unwrap_or_else(|| {
            panic!("no answer or no End: {kinds:?} {response:?} pending {pending:?}")
        });
        if message.get("method").is_none()
            && let Some(answered) = message["id"].as_i64()
        {
            if answered == id {
                response = Some(message);
            } else {
                pending.retain(|other| *other != answered);
            }
        } else {
            record_progress(&message, kinds);
        }
    }
    response.expect("answered")
}

/// The editor's work-done progress is one lifecycle for the whole request:
/// the region that reports first owns it, and the bridge's terminal End
/// comes only after the last region settles, not when the first one does.
#[test]
fn e2e_inlay_hint_work_done_progress_ends_after_the_last_region() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-progress", "lua", false, None);
    let uri = "file:///test_inlay_hint_progress_held.md";
    let (id, mut kinds) = start_held_progress_request(&mut client, uri);

    // Any later inlay hint request releases the held region's answer.
    let release = client.send_request_async(
        "textDocument/inlayHint",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 9, "character": 0 },
                "end": { "line": 10, "character": 0 }
            }
        }),
    );
    let response = finish_held_progress_request(&mut client, id, &[release], &mut kinds);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"].as_array().map(Vec::len),
        Some(2),
        "{response}"
    );
    assert_eq!(kinds, ["begin", "report", "end"]);

    shutdown_client(&mut client);
}

/// A cancel that lands while a region is held still ends the editor's
/// progress, once, with the cancelled answer.
#[test]
fn e2e_inlay_hint_work_done_progress_ends_on_cancel() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-progress", "lua", false, None);
    let uri = "file:///test_inlay_hint_progress_cancel.md";
    let (id, mut kinds) = start_held_progress_request(&mut client, uri);

    client.send_notification("$/cancelRequest", json!({ "id": id }));
    let response = finish_held_progress_request(&mut client, id, &[], &mut kinds);
    assert_eq!(response["error"]["code"], -32800, "{response}");
    assert_eq!(kinds, ["begin", "report", "end"]);

    shutdown_client(&mut client);
}

/// A range wholly inside the stripped `> ` prefix of a blockquoted fence
/// line covers no embedded text, so no region is asked (as for a range that
/// starts there on a single-region request). A range from the prefix into
/// the line's text is asked from where the text starts.
#[test]
fn e2e_inlay_hint_range_inside_a_stripped_prefix_asks_no_region() {
    let (mut client, _config_dir) =
        init_mock_inlay_hint_client("inlay-hint-resolve", "lua", false, None);
    let uri = "file:///test_inlay_hint_blockquote_prefix.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "> ```lua\n> local x = 1\n> local y = 2\n> ```\n"
        }}),
    );
    inlay_hints_with_retry(&mut client, uri, 1, 2);

    let request = |client: &mut LspClient, start: u64, end: u64| {
        client.send_request(
            "textDocument/inlayHint",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 2, "character": start },
                    "end": { "line": 2, "character": end }
                }
            }),
        )
    };
    let prefix_only = request(&mut client, 0, 1);
    assert!(prefix_only.get("error").is_none(), "{prefix_only}");
    assert!(prefix_only["result"].is_null(), "{prefix_only}");
    let caret_in_prefix = request(&mut client, 1, 1);
    assert!(caret_in_prefix["result"].is_null(), "{caret_in_prefix}");

    let into_text = request(&mut client, 1, 5);
    let hints = into_text["result"]
        .as_array()
        .expect("hints for the line's text");
    assert_eq!(
        hints[0]["data"]["kakehashi"]["inner"]["receivedRange"],
        json!({
            "start": { "line": 1, "character": 0 },
            "end": { "line": 1, "character": 3 }
        })
    );

    shutdown_client(&mut client);
}
