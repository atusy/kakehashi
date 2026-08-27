//! End-to-end coverage for staged `textDocument/prepareCallHierarchy` routing.

use std::time::Duration;

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client(host_layer: bool) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("call_hierarchy.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    let mut initialization_options = json!({
        "languageServers": {
            "mock-call-hierarchy": {
                "cmd": [mock_formatter_bin(), "call-hierarchy-prepare"],
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

fn prepare_with_retry(client: &mut LspClient, uri: &str, line: u64) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": 1 }
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

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
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
            "text": "# Test\n\n```lua\ncall()\n```\n"
        }}),
    );

    let items = prepare_with_retry(&mut client, uri, 3);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["uri"], uri);
    assert_eq!(item["range"]["start"], json!({ "line": 3, "character": 0 }));
    assert_eq!(
        item["selectionRange"]["end"],
        json!({ "line": 3, "character": 4 })
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

    let items = prepare_with_retry(&mut client, uri, 0);
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
