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
                    "mock-symbol": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), "workspace-symbol"],
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
    let symbol = search["result"][0].clone();
    assert_eq!(symbol["name"], "mock:needle");
    assert_eq!(symbol["location"]["uri"], "file:///workspace/main.rs");
    assert!(symbol["location"].get("range").is_none());
    assert_eq!(
        symbol["data"]["kakehashi"]["workspaceSymbol"]["origin"],
        "mock-symbol"
    );

    let resolved = client.send_request("workspaceSymbol/resolve", symbol);
    assert!(
        resolved.get("error").is_none(),
        "resolve failed: {resolved:?}"
    );
    assert_eq!(resolved["result"]["location"]["range"]["start"]["line"], 4);
    assert_eq!(
        resolved["result"]["data"]["kakehashi"]["workspaceSymbol"]["inner"]["mock"],
        "needle"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
