//! End-to-end coverage for staged type-hierarchy routing.

use std::time::Duration;

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client(host_layer: bool) -> (LspClient, tempfile::TempDir) {
    init_client_with_mode(host_layer, "type-hierarchy-prepare", None)
}

fn init_client_with_mode(
    host_layer: bool,
    mode: &str,
    wire_log: Option<&std::path::Path>,
) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("type_hierarchy.toml");
    std::fs::write(&config_path, "").expect("write config");
    let builder = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"));
    let builder = match wire_log {
        Some(path) if path.is_dir() => builder.env("MOCK_LSP_CANCEL_DIR", path.to_string_lossy()),
        Some(path) => builder.env("MOCK_LSP_WIRE_LOG", path.to_string_lossy()),
        None => builder,
    };
    let mut client = builder.build();
    let mut initialization_options = json!({
        "languageServers": {
            "mock-type-hierarchy": {
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
    assert!(initialized.get("error").is_none(), "{initialized}");
    assert!(
        initialized["result"]["capabilities"]
            .get("typeHierarchyProvider")
            .is_none(),
        "prepare+supertypes stack must not advertise until subtypes lands"
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

#[test]
fn prepare_type_hierarchy_skips_a_server_without_the_capability() {
    let wire_dir = tempfile::TempDir::new().unwrap();
    let wire_log = wire_dir.path().join("wire.log");
    let (mut client, _config_dir) =
        init_client_with_mode(false, "type-hierarchy-unsupported", Some(&wire_log));
    let uri = "file:///test_type_hierarchy_unsupported.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "```lua\nMockChild\n```\n"
        }}),
    );
    for _ in 0..100 {
        if std::fs::read_to_string(&wire_log)
            .unwrap_or_default()
            .contains("initialize")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let response = client.send_request(
        "textDocument/prepareTypeHierarchy",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 1 }
        }),
    );
    assert_eq!(response["result"], Value::Null);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !std::fs::read_to_string(&wire_log)
            .unwrap_or_default()
            .contains("textDocument/prepareTypeHierarchy"),
        "unsupported downstream server must not receive prepareTypeHierarchy"
    );
    shutdown(&mut client);
}

fn prepare(client: &mut LspClient, uri: &str, line: u64, character: u64) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/prepareTypeHierarchy",
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
    panic!("timed out waiting for type hierarchy items");
}

fn supertypes(client: &mut LspClient, item: Value) -> Vec<Value> {
    let response = client.send_request("typeHierarchy/supertypes", json!({ "item": item }));
    assert!(response.get("error").is_none(), "{response}");
    response["result"]
        .as_array()
        .cloned()
        .expect("supertype array")
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
fn supertypes_restore_the_virtual_item_and_re_envelope_results() {
    let (mut client, _config_dir) = init_client(false);
    let uri = "file:///test_type_hierarchy_supertypes.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "> ```lua\n> MockChild\n> ```\n"
        }}),
    );
    let item = prepare(&mut client, uri, 1, 3).remove(0);

    let parent = supertypes(&mut client, item).remove(0);

    assert_eq!(parent["name"], "MockParent");
    assert_eq!(parent["uri"], uri);
    assert_eq!(
        parent["range"]["start"],
        json!({ "line": 1, "character": 2 })
    );
    assert_eq!(parent["tags"], json!([1]));
    assert_eq!(
        parent["data"]["kakehashi"]["inner"],
        json!({ "mock": "parent-item" })
    );
    let grandparent = supertypes(&mut client, parent).remove(0);
    assert_eq!(grandparent["name"], "MockGrandparent");
    assert_eq!(grandparent["uri"], uri);
    assert_eq!(
        grandparent["data"]["kakehashi"]["inner"],
        json!({ "mock": "grandparent-item" })
    );
    shutdown(&mut client);
}

#[test]
fn supertypes_preserve_host_item_coordinates() {
    let (mut client, _config_dir) = init_client(true);
    let uri = "file:///test_type_hierarchy_supertypes.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "MockChild\n"
        }}),
    );
    let item = prepare(&mut client, uri, 0, 1).remove(0);

    let parent = supertypes(&mut client, item).remove(0);

    assert_eq!(parent["uri"], uri);
    assert_eq!(
        parent["range"]["start"],
        json!({ "line": 0, "character": 0 })
    );
    assert_eq!(parent["data"]["kakehashi"]["host_layer"], true);
    shutdown(&mut client);
}

#[test]
fn supertypes_without_a_routing_envelope_return_null() {
    let (mut client, _config_dir) = init_client(false);
    let response = client.send_request(
        "typeHierarchy/supertypes",
        json!({ "item": {
            "name": "Foreign", "kind": 5, "uri": "file:///foreign.lua",
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } }
        }}),
    );
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

fn assert_supertypes_discard_response_after_document_change(host_layer: bool) {
    let (mut client, _config_dir) =
        init_client_with_mode(host_layer, "type-hierarchy-delayed-supertypes", None);
    let (uri, language_id, text, line, character) = if host_layer {
        (
            "file:///test_delayed_supertype.lua",
            "lua",
            "MockChild\n",
            0,
            1,
        )
    } else {
        (
            "file:///test_delayed_supertype.md",
            "markdown",
            "```lua\nMockChild\n```\n",
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
    let item = prepare(&mut client, uri, line, character).remove(0);

    let request_id = client.send_request_async("typeHierarchy/supertypes", json!({ "item": item }));
    assert!(
        wait_for_log_message(&mut client, "type-hierarchy-supertypes-started").is_some(),
        "downstream request must reach the sent-state barrier"
    );
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": text.replace("MockChild", "Changed") }]
        }),
    );
    let response = client.receive_response_for_id_public(request_id);
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn supertypes_discard_stale_responses_for_both_layers() {
    assert_supertypes_discard_response_after_document_change(false);
    assert_supertypes_discard_response_after_document_change(true);
}

fn assert_supertypes_cancel_targets_exact_downstream_request(host_layer: bool) {
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let request_file = event_dir
        .path()
        .join("type-hierarchy-slow-supertypes.request.json");
    let cancel_file = event_dir
        .path()
        .join("type-hierarchy-slow-supertypes.cancel.json");
    let (mut client, _config_dir) = init_client_with_mode(
        host_layer,
        "type-hierarchy-slow-supertypes",
        Some(event_dir.path()),
    );
    let (uri, language_id, text, line, character) = if host_layer {
        (
            "file:///test_cancel_supertype.lua",
            "lua",
            "MockChild\n",
            0,
            1,
        )
    } else {
        (
            "file:///test_cancel_supertype.md",
            "markdown",
            "```lua\nMockChild\n```\n",
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
    let item = prepare(&mut client, uri, line, character).remove(0);

    let request_id = client.send_request_async("typeHierarchy/supertypes", json!({ "item": item }));
    assert!(
        wait_for_log_message(&mut client, "type-hierarchy-supertypes-started").is_some(),
        "downstream request must start"
    );
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

#[test]
fn supertypes_cancel_exact_downstream_request_for_both_layers() {
    assert_supertypes_cancel_targets_exact_downstream_request(false);
    assert_supertypes_cancel_targets_exact_downstream_request(true);
}

fn assert_replaced_supertype_producer_fails_soft(host_layer: bool, change_pool_key: bool) {
    let (mut client, _config_dir) = init_client(host_layer);
    let (uri, language_id, text, line, character) = if host_layer {
        (
            "file:///test_replace_supertype.lua",
            "lua",
            "MockChild\n",
            0,
            1,
        )
    } else {
        (
            "file:///test_replace_supertype.md",
            "markdown",
            "```lua\nMockChild\n```\n",
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
    let old_item = prepare(&mut client, uri, line, character).remove(0);

    let mut server = json!({
        "cmd": [mock_formatter_bin(), "type-hierarchy-replacement"],
        "languages": ["lua"]
    });
    if change_pool_key {
        server["preferSharedInstance"] = json!(true);
    }
    let mut settings = json!({ "languageServers": { "mock-type-hierarchy": server } });
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
    let replacement_item = prepare(&mut client, uri, line, character).remove(0);
    let old_envelope = &old_item["data"]["kakehashi"];
    let replacement_envelope = &replacement_item["data"]["kakehashi"];
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
    assert_eq!(supertypes(&mut client, replacement_item.clone()).len(), 1);
    let mut stale_item = replacement_item;
    stale_item["data"]["kakehashi"]["connection_key"] = old_envelope["connection_key"].clone();
    stale_item["data"]["kakehashi"]["connection_generation"] =
        old_envelope["connection_generation"].clone();
    stale_item["data"]["kakehashi"]["inner"] = old_envelope["inner"].clone();
    let response = client.send_request("typeHierarchy/supertypes", json!({ "item": stale_item }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn supertypes_reject_replaced_producers_for_both_layers_and_key_shapes() {
    for host_layer in [false, true] {
        assert_replaced_supertype_producer_fails_soft(host_layer, false);
        assert_replaced_supertype_producer_fails_soft(host_layer, true);
    }
}

fn assert_reopened_supertype_item_fails_soft(host_layer: bool) {
    let (mut client, _config_dir) = init_client(host_layer);
    let (uri, language_id, text, line, character) = if host_layer {
        (
            "file:///test_reopen_supertype.lua",
            "lua",
            "MockChild\n",
            0,
            1,
        )
    } else {
        (
            "file:///test_reopen_supertype.md",
            "markdown",
            "```lua\nMockChild\n```\n",
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
    let old_item = prepare(&mut client, uri, line, character).remove(0);
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    open(&mut client);
    let current_item = prepare(&mut client, uri, line, character).remove(0);
    assert_ne!(
        old_item["data"]["kakehashi"]["incarnation"],
        current_item["data"]["kakehashi"]["incarnation"]
    );
    assert_eq!(supertypes(&mut client, current_item).len(), 1);
    let response = client.send_request("typeHierarchy/supertypes", json!({ "item": old_item }));
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"], Value::Null);
    shutdown(&mut client);
}

#[test]
fn supertypes_reject_items_from_reopened_documents_for_both_layers() {
    assert_reopened_supertype_item_fails_soft(false);
    assert_reopened_supertype_item_fails_soft(true);
}

#[test]
fn prepare_type_hierarchy_translates_virtual_items_and_envelopes_origin() {
    let (mut client, _config_dir) = init_client(false);
    let uri = "file:///test_type_hierarchy.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "markdown",
            "version": 1,
            "text": "> ```lua\n> MockChild\n> ```\n"
        }}),
    );

    let item = prepare(&mut client, uri, 1, 3).remove(0);
    assert_eq!(item["uri"], uri);
    assert_eq!(item["detail"], "0:1");
    assert_eq!(item["tags"], json!([1]));
    assert_eq!(item["range"]["start"], json!({ "line": 1, "character": 2 }));
    assert_eq!(item["data"]["kakehashi"]["origin"], "mock-type-hierarchy");
    assert_eq!(
        item["data"]["kakehashi"]["inner"],
        json!({ "mock": "type-item" })
    );
    shutdown(&mut client);
}

#[test]
fn prepare_type_hierarchy_envelopes_host_items_without_translation() {
    let (mut client, _config_dir) = init_client(true);
    let uri = "file:///test_type_hierarchy.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri,
            "languageId": "lua",
            "version": 1,
            "text": "MockChild\n"
        }}),
    );

    let item = prepare(&mut client, uri, 0, 1).remove(0);
    assert_eq!(item["uri"], uri);
    assert_eq!(item["range"]["start"], json!({ "line": 0, "character": 0 }));
    assert_eq!(item["data"]["kakehashi"]["host_layer"], true);
    shutdown(&mut client);
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
