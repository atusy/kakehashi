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
    assert_ne!(hint["label"][0]["command"]["command"], "mock.hint");

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
    assert_ne!(resolved["label"][0]["command"]["command"], "mock.resolved");

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

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
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

    // The real edit made the once-single combined capture non-contiguous. Set
    // only the opaque freshness stamp to the current version so this request
    // specifically exercises the live geometry/contiguity guard.
    hint["data"]["kakehashi"]["content_version"] = json!(2);
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], hint);
    assert!(!marker.exists(), "resolve must not reach the downstream");

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
    let response = client.send_request("inlayHint/resolve", hint.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["tooltip"], "mock resolved:hint-1");
    assert_eq!(response["result"]["position"], hint["position"]);
    let observed = resolve_request_observation(&response["result"]);
    assert_eq!(observed["receivedPosition"], hint["position"]);
    assert_eq!(observed["receivedTextEdit"], hint["textEdits"][0]["range"]);
    assert_eq!(observed["receivedLocation"], hint["label"][0]["location"]);
    assert_eq!(observed["receivedCommand"], "mock.hint");

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

    shutdown_client(&mut client);
}
