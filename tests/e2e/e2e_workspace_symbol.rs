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
