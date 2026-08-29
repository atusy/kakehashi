//! End-to-end coverage for workspace symbol search and lazy resolve routing.

use crate::helpers::lsp_client::LspClient;
use serde_json::json;

#[test]
fn workspace_symbol_search_starts_a_server_and_resolves_on_its_origin() {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("workspace_symbol.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();

    let initialize = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": "file:///workspace",
            "capabilities": {
                "workspace": {
                    "symbol": {
                        "resolveSupport": { "properties": ["location.range"] }
                    }
                }
            },
            "initializationOptions": {
                "languageServers": {
                    "zeta-symbol": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), "workspace-symbol-zeta"],
                        "languages": []
                    },
                    "alpha-symbol": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), "workspace-symbol-alpha"],
                        "languages": []
                    }
                }
            }
        }),
    );
    assert_eq!(
        initialize["result"]["capabilities"]["workspaceSymbolProvider"]["resolveProvider"],
        json!(true)
    );
    client.send_notification("initialized", json!({}));

    let search = client.send_request("workspace/symbol", json!({ "query": "needle" }));
    assert!(search.get("error").is_none(), "search failed: {search:?}");
    let symbols = search["result"].as_array().expect("workspace symbols");
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0]["name"], "workspace-symbol-alpha:needle");
    assert_eq!(symbols[1]["name"], "workspace-symbol-zeta:needle");
    let symbol = symbols[1].clone();
    assert_eq!(
        symbol["location"]["uri"],
        "file:///workspace/workspace-symbol-zeta.rs"
    );
    assert!(symbol["location"].get("range").is_none());
    assert_eq!(
        symbol["data"]["kakehashi"]["workspaceSymbol"]["origin"],
        "zeta-symbol"
    );

    let resolved = client.send_request("workspaceSymbol/resolve", symbol);
    assert!(
        resolved.get("error").is_none(),
        "resolve failed: {resolved:?}"
    );
    assert_eq!(resolved["result"]["location"]["range"]["start"]["line"], 8);
    assert_eq!(
        resolved["result"]["location"]["uri"],
        "file:///workspace/workspace-symbol-zeta.rs"
    );
    assert_eq!(
        resolved["result"]["data"]["kakehashi"]["workspaceSymbol"]["inner"]["mock"],
        "needle"
    );
    assert_eq!(
        resolved["result"]["data"]["kakehashi"]["workspaceSymbol"]["inner"]["producer"],
        "workspace-symbol-zeta"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn virtual_workspace_symbol_locations_round_trip_through_the_host() {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("workspace_symbol_virtual.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .build();
    let _ = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": "file:///workspace",
            "workspaceFolders": null,
            "capabilities": {
                "workspace": {
                    "symbol": {
                        "resolveSupport": { "properties": ["location.range"] }
                    }
                }
            },
            "initializationOptions": {
                "languageServers": {
                    "virtual-symbol": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), "workspace-symbol-virtual"],
                        "languages": ["lua"],
                        "preferSharedInstance": true
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let host_uri = "file:///workspace/virtual-symbol.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": host_uri,
                "languageId": "markdown",
                "version": 1,
                "text": "```lua\nlocal value = 1\n```\n"
            }
        }),
    );

    let symbol = (0..100)
        .find_map(|_| {
            let response = client.send_request("workspace/symbol", json!({ "query": "needle" }));
            response["result"]
                .as_array()
                .and_then(|symbols| symbols.first())
                .filter(|symbol| symbol["location"]["uri"] == host_uri)
                .cloned()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                })
        })
        .expect("the virtual symbol should be projected onto its host URI");
    assert!(symbol["location"].get("range").is_none());
    assert!(
        symbol["data"]["kakehashi"]["workspaceSymbol"]["projection"]["virtual_uri"]
            .as_str()
            .is_some_and(|uri| uri.contains("kakehashi-virtual-uri-"))
    );

    let resolved = client.send_request("workspaceSymbol/resolve", symbol);
    assert!(
        resolved.get("error").is_none(),
        "resolve failed: {resolved:?}"
    );
    assert_eq!(resolved["result"]["location"]["uri"], host_uri);
    assert_eq!(
        resolved["result"]["location"]["range"]["start"]["line"], 1,
        "resolved virtual range should be projected: {resolved:?}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
