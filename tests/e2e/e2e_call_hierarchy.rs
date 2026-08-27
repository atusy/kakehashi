//! End-to-end coverage for staged call-hierarchy routing.

use std::time::Duration;

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client(host_layer: bool) -> (LspClient, tempfile::TempDir) {
    init_client_with_mode(host_layer, "call-hierarchy-prepare", None)
}

fn init_client_with_mode(
    host_layer: bool,
    mode: &str,
    event_dir: Option<&std::path::Path>,
) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("call_hierarchy.toml");
    std::fs::write(&config_path, "").expect("write config");
    let builder = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"));
    let builder = match event_dir {
        Some(dir) => builder.env("MOCK_LSP_CANCEL_DIR", dir.to_string_lossy()),
        None => builder,
    };
    let mut client = builder.build();
    let mut initialization_options = json!({
        "languageServers": {
            "mock-call-hierarchy": {
                "cmd": [mock_formatter_bin(), mode],
                "languages": ["lua"]
            }
        }
    });
    if host_layer {
        initialization_options["languages"] = json!({
            "lua": { "bridge": { "_self": { "enabled": true } } }
        });
    }
    let initialized = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": initialization_options
        }),
    );
    assert!(
        initialized["result"]["capabilities"]["callHierarchyProvider"].is_null(),
        "capability stays hidden until incomingCalls and outgoingCalls are implemented"
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

fn prepare_with_retry(client: &mut LspClient, uri: &str, line: u64, character: u64) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        if let Some(items) = response["result"].as_array()
            && !items.is_empty()
        {
            return items.clone();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for call hierarchy items");
}

fn incoming_calls(client: &mut LspClient, item: Value) -> Vec<Value> {
    let response = client.send_request("callHierarchy/incomingCalls", json!({ "item": item }));
    assert!(response.get("error").is_none(), "{response}");
    response["result"]
        .as_array()
        .cloned()
        .expect("incoming call array")
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

fn wait_for_log_message(client: &mut LspClient, needle: &str) -> Option<Value> {
    for _ in 0..20 {
        let message = client.wait_for_notification("window/logMessage", Duration::from_secs(1));
        if message.as_ref().is_some_and(|params| {
            params["message"]
                .as_str()
                .is_some_and(|message| message.contains(needle))
        }) {
            return message;
        }
    }
    None
}

#[test]
fn prepare_call_hierarchy_translates_virtual_items_and_envelopes_origin() {
    let (mut client, _config_dir) = init_client(false);
    let uri = "file:///test_call_hierarchy.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "> ```lua\n> call()\n> ```\n"
        }}),
    );

    let items = prepare_with_retry(&mut client, uri, 1, 3);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["uri"], uri);
    assert_eq!(item["detail"], "0:1");
    assert_eq!(item["range"]["start"], json!({ "line": 1, "character": 2 }));
    assert_eq!(
        item["selectionRange"]["end"],
        json!({ "line": 1, "character": 6 })
    );
    assert_eq!(item["data"]["kakehashi"]["origin"], "mock-call-hierarchy");
    assert_eq!(
        item["data"]["kakehashi"]["inner"],
        json!({ "mock": "call-item" })
    );
    assert_eq!(item["data"]["kakehashi"]["host_layer"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn prepare_call_hierarchy_envelopes_host_items_without_translation() {
    let (mut client, _config_dir) = init_client(true);
    let uri = "file:///test_call_hierarchy.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "call()\n"
        }}),
    );

    let items = prepare_with_retry(&mut client, uri, 0, 1);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["uri"], uri);
    assert_eq!(item["range"]["start"], json!({ "line": 0, "character": 0 }));
    assert_eq!(item["data"]["kakehashi"]["host_layer"], true);
    assert_eq!(
        item["data"]["kakehashi"]["inner"],
        json!({ "mock": "call-item" })
    );
    shutdown(&mut client);
}

#[test]
fn incoming_calls_restore_virtual_item_and_translate_caller_to_host() {
    let (mut client, _config_dir) = init_client(false);
    let uri = "file:///test_incoming_calls.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "> ```lua\n> call()\n> ```\n"
        }}),
    );

    let item = prepare_with_retry(&mut client, uri, 1, 3).remove(0);
    let calls = incoming_calls(&mut client, item);
    assert_eq!(calls.len(), 1);
    let caller = &calls[0]["from"];
    assert_eq!(caller["uri"], uri);
    assert_eq!(
        caller["range"]["start"],
        json!({ "line": 1, "character": 2 })
    );
    assert_eq!(
        calls[0]["fromRanges"][0],
        json!({
            "start": { "line": 1, "character": 3 },
            "end": { "line": 1, "character": 4 }
        })
    );
    assert_eq!(
        caller["data"]["kakehashi"]["inner"],
        json!({ "mock": "incoming-caller" })
    );
    let observation: Value =
        serde_json::from_str(caller["detail"].as_str().expect("mock observation")).unwrap();
    assert!(
        observation["receivedUri"]
            .as_str()
            .is_some_and(|uri| uri.contains("kakehashi-virtual-uri-"))
    );
    assert_eq!(
        observation["receivedRange"]["start"],
        json!({ "line": 0, "character": 0 })
    );
    assert_eq!(
        observation["receivedSelectionRange"]["end"],
        json!({ "line": 0, "character": 4 })
    );
    assert_eq!(observation["receivedData"], json!({ "mock": "call-item" }));
    shutdown(&mut client);
}

#[test]
fn incoming_calls_preserve_host_item_coordinates() {
    let (mut client, _config_dir) = init_client(true);
    let uri = "file:///test_incoming_calls.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "call()\n"
        }}),
    );

    let item = prepare_with_retry(&mut client, uri, 0, 1).remove(0);
    let calls = incoming_calls(&mut client, item);
    let caller = &calls[0]["from"];
    assert_eq!(caller["uri"], uri);
    assert_eq!(
        caller["range"]["start"],
        json!({ "line": 0, "character": 0 })
    );
    assert_eq!(
        calls[0]["fromRanges"][0]["start"],
        json!({ "line": 0, "character": 1 })
    );
    let observation: Value =
        serde_json::from_str(caller["detail"].as_str().expect("mock observation")).unwrap();
    assert_eq!(observation["receivedUri"], uri);
    assert_eq!(observation["receivedData"], json!({ "mock": "call-item" }));
    shutdown(&mut client);
}

#[test]
fn incoming_calls_reject_items_from_stale_document_content() {
    let (mut client, _config_dir) = init_client(false);
    let uri = "file:///test_stale_incoming_calls.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\ncall()\n```\n"
        }}),
    );

    let stale_item = prepare_with_retry(&mut client, uri, 1, 1).remove(0);
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "```lua\nchanged()\n```\n" }]
        }),
    );
    let response =
        client.send_request("callHierarchy/incomingCalls", json!({ "item": stale_item }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn incoming_calls_reject_stale_virtual_geometry() {
    let (mut client, _config_dir) = init_client(false);
    let uri = "file:///test_stale_incoming_geometry.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\ncall()\n```\n"
        }}),
    );

    let mut item = prepare_with_retry(&mut client, uri, 1, 1).remove(0);
    item["data"]["kakehashi"]["offset"]["line"] = json!(99);
    let response = client.send_request("callHierarchy/incomingCalls", json!({ "item": item }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn incoming_calls_discard_response_after_document_change() {
    let (mut client, _config_dir) =
        init_client_with_mode(false, "call-hierarchy-delayed-incoming", None);
    let uri = "file:///test_delayed_incoming.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\ncall()\n```\n"
        }}),
    );
    let item = prepare_with_retry(&mut client, uri, 1, 1).remove(0);

    let request_id =
        client.send_request_async("callHierarchy/incomingCalls", json!({ "item": item }));
    let started = wait_for_log_message(&mut client, "call-hierarchy-incoming-started");
    assert!(
        started.as_ref().is_some_and(|params| params["message"]
            .as_str()
            .is_some_and(|message| message.contains("call-hierarchy-incoming-started"))),
        "incoming request must reach the downstream sent-state barrier: {started:?}"
    );
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "```lua\nchanged()\n```\n" }]
        }),
    );
    let response = client.receive_response_for_id_public(request_id);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn incoming_calls_cancel_targets_exact_downstream_request() {
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let request_file = event_dir
        .path()
        .join("call-hierarchy-slow-incoming.request.json");
    let cancel_file = event_dir
        .path()
        .join("call-hierarchy-slow-incoming.cancel.json");
    let (mut client, _config_dir) =
        init_client_with_mode(true, "call-hierarchy-slow-incoming", Some(event_dir.path()));
    let uri = "file:///test_cancel_incoming.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri, "languageId": "lua", "version": 1, "text": "call()\n"
        }}),
    );
    let item = prepare_with_retry(&mut client, uri, 0, 1).remove(0);

    let request_id =
        client.send_request_async("callHierarchy/incomingCalls", json!({ "item": item }));
    let started = wait_for_log_message(&mut client, "call-hierarchy-incoming-started");
    assert!(started.is_some(), "downstream request must start");
    assert!(request_file.exists(), "downstream request must be recorded");
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["error"]["code"], -32800, "{response}");
    let forwarded = (0..200).any(|_| {
        if cancel_file.exists() {
            true
        } else {
            std::thread::sleep(Duration::from_millis(50));
            false
        }
    });
    assert!(forwarded, "cancel must reach downstream");
    let request: Value = serde_json::from_slice(&std::fs::read(request_file).unwrap()).unwrap();
    let cancel: Value = serde_json::from_slice(&std::fs::read(cancel_file).unwrap()).unwrap();
    assert_eq!(cancel["params"]["id"], request["id"]);
    shutdown(&mut client);
}

fn assert_replaced_call_hierarchy_producer_fails_soft(host_layer: bool, change_pool_key: bool) {
    let (mut client, _config_dir) = init_client(host_layer);
    let (uri, language_id, text, line, character) = if host_layer {
        ("file:///test_replace_incoming.lua", "lua", "call()\n", 0, 1)
    } else {
        (
            "file:///test_replace_incoming.md",
            "markdown",
            "```lua\ncall()\n```\n",
            1,
            1,
        )
    };
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri, "languageId": language_id, "version": 1, "text": text
        }}),
    );
    let old_item = prepare_with_retry(&mut client, uri, line, character).remove(0);

    let mut server = json!({
        "cmd": [mock_formatter_bin(), "call-hierarchy-replacement"],
        "languages": ["lua"]
    });
    if change_pool_key {
        server["preferSharedInstance"] = json!(true);
    }
    let mut settings = json!({
        "languageServers": { "mock-call-hierarchy": server }
    });
    if host_layer {
        settings["languages"] = json!({
            "lua": { "bridge": { "_self": { "enabled": true } } }
        });
    }
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": settings }),
    );
    if !host_layer {
        client.send_notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        );
        client.send_notification(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": language_id, "version": 1, "text": text
            }}),
        );
    }

    let replacement_item = prepare_with_retry(&mut client, uri, line, character).remove(0);
    let old_envelope = &old_item["data"]["kakehashi"];
    let replacement_envelope = &replacement_item["data"]["kakehashi"];
    if change_pool_key {
        assert_ne!(
            old_envelope["connection_key"],
            replacement_envelope["connection_key"]
        );
        assert_eq!(
            old_envelope["connection_generation"], replacement_envelope["connection_generation"],
            "different keys exercise the equal-generation collision"
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
    assert_eq!(
        incoming_calls(&mut client, replacement_item.clone()).len(),
        1
    );
    // Keep current content/incarnation/geometry and replace only producer
    // identity, isolating the key and generation checks.
    let mut stale_item = replacement_item;
    stale_item["data"]["kakehashi"]["connection_key"] = old_envelope["connection_key"].clone();
    stale_item["data"]["kakehashi"]["connection_generation"] =
        old_envelope["connection_generation"].clone();
    stale_item["data"]["kakehashi"]["inner"] = old_envelope["inner"].clone();
    let response =
        client.send_request("callHierarchy/incomingCalls", json!({ "item": stale_item }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn incoming_calls_reject_same_key_replacement_for_both_layers() {
    assert_replaced_call_hierarchy_producer_fails_soft(false, false);
    assert_replaced_call_hierarchy_producer_fails_soft(true, false);
}

#[test]
fn incoming_calls_reject_different_key_equal_generation_for_both_layers() {
    assert_replaced_call_hierarchy_producer_fails_soft(false, true);
    assert_replaced_call_hierarchy_producer_fails_soft(true, true);
}

fn assert_reopened_call_hierarchy_item_fails_soft(host_layer: bool) {
    let (mut client, _config_dir) = init_client(host_layer);
    let (uri, language_id, text, line, character) = if host_layer {
        ("file:///test_reopen_incoming.lua", "lua", "call()\n", 0, 1)
    } else {
        (
            "file:///test_reopen_incoming.md",
            "markdown",
            "```lua\ncall()\n```\n",
            1,
            1,
        )
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
    let old_item = prepare_with_retry(&mut client, uri, line, character).remove(0);
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    open(&mut client);
    let current_item = prepare_with_retry(&mut client, uri, line, character).remove(0);
    assert_ne!(
        old_item["data"]["kakehashi"]["incarnation"],
        current_item["data"]["kakehashi"]["incarnation"]
    );
    assert_eq!(incoming_calls(&mut client, current_item).len(), 1);
    let response = client.send_request("callHierarchy/incomingCalls", json!({ "item": old_item }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn incoming_calls_reject_items_from_reopened_documents_for_both_layers() {
    assert_reopened_call_hierarchy_item_fails_soft(false);
    assert_reopened_call_hierarchy_item_fails_soft(true);
}
