use crate::helpers::lsp_client::LspClient;
use serde_json::json;

#[test]
fn workspace_diagnostic_starts_and_aggregates_cold_producers() {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("workspace_diagnostic.toml");
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
                "textDocument": { "diagnostic": {} },
                "workspace": { "diagnostics": { "refreshSupport": true } }
            },
            "initializationOptions": {
                "languageServers": {
                    "zeta-diagnostic": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), "workspace-diagnostic-zeta"],
                        "languages": []
                    },
                    "alpha-diagnostic": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), "workspace-diagnostic-alpha"],
                        "languages": []
                    }
                }
            }
        }),
    );
    assert_eq!(
        initialize["result"]["capabilities"]["diagnosticProvider"]["workspaceDiagnostics"],
        json!(true)
    );
    client.send_notification("initialized", json!({}));

    let response = client.send_request(
        "workspace/diagnostic",
        json!({
            "identifier": "upstream-private",
            "previousResultIds": [{
                "uri": "file:///workspace/shared.rs",
                "value": "upstream-result"
            }],
            "partialResultToken": "upstream-partial",
            "workDoneToken": "upstream-work"
        }),
    );
    assert!(
        response.get("error").is_none(),
        "request failed: {response:?}"
    );
    let items = response["result"]["items"]
        .as_array()
        .expect("report items");
    assert_eq!(items.len(), 2, "internal virtual reports must be filtered");
    assert_eq!(items[0]["uri"], "file:///workspace/shared.rs");
    assert_eq!(items[0]["version"], 4);
    assert!(items[0].get("resultId").is_none());
    assert_eq!(
        items[0]["items"][0]["message"],
        "workspace-diagnostic-alpha"
    );
    assert_eq!(items[0]["items"][1]["message"], "workspace-diagnostic-zeta");
    assert_eq!(items[1]["uri"], "file:///workspace/zeta.rs");
    assert!(items[1].get("resultId").is_none());
}
