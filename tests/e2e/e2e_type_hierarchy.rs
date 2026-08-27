//! End-to-end coverage for type-hierarchy preparation.

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
        "prepare-only stack must not advertise the incomplete hierarchy surface"
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
