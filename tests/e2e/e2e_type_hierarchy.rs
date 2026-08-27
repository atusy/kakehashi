//! End-to-end coverage for type-hierarchy preparation.

use std::time::Duration;

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client(host_layer: bool) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("type_hierarchy.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    let mut initialization_options = json!({
        "languageServers": {
            "mock-type-hierarchy": {
                "cmd": [mock_formatter_bin(), "type-hierarchy-prepare"],
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
    client.send_notification("initialized", json!({}));
    (client, config_dir)
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
