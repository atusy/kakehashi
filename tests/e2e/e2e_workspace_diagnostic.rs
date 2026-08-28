use crate::helpers::lsp_client::LspClient;
use serde_json::json;
use std::collections::BTreeSet;
use std::time::Duration;

fn init_dynamic_workspace_diagnostic_client(
    mode: &str,
) -> (LspClient, tempfile::TempDir, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("workspace_diagnostic_dynamic.toml");
    std::fs::write(&config_path, "").expect("write config");
    let events = tempfile::TempDir::new().expect("mock event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("UTF-8 config path"))
        .env(
            "MOCK_LSP_CANCEL_DIR",
            events.path().to_string_lossy().into_owned(),
        )
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
                    "dynamic-diagnostic": {
                        "cmd": [env!("CARGO_BIN_EXE_mock-lsp-formatter"), mode],
                        "languages": [],
                        "forceStart": true
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
    client
        .wait_for_notification_where(&["window/logMessage"], Duration::from_secs(10), |params| {
            params["message"]
                .as_str()
                .is_some_and(|message| message.contains("dynamic-diagnostics-registered"))
        })
        .expect("dynamic diagnostic registrations are acknowledged");
    (client, config_dir, events)
}

fn wait_for_file(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

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
    assert_eq!(items.len(), 3);
    assert_eq!(
        items[0]["uri"], "file:///workspace/kakehashi-virtual-uri-region-0.lua",
        "an unopened real URI must not be filtered merely because its name looks virtual"
    );
    assert_eq!(items[1]["uri"], "file:///workspace/shared.rs");
    assert_eq!(items[1]["version"], serde_json::Value::Null);
    assert!(items[1].get("resultId").is_none());
    assert_eq!(
        items[1]["items"][0]["message"],
        "workspace-diagnostic-alpha"
    );
    assert_eq!(items[1]["items"][1]["message"], "workspace-diagnostic-zeta");
    assert_eq!(items[2]["uri"], "file:///workspace/zeta.rs");
    assert!(items[2].get("resultId").is_none());
}

#[test]
fn workspace_diagnostic_sends_each_dynamic_provider_its_own_wire_params() {
    let (mut client, _config_dir, events) =
        init_dynamic_workspace_diagnostic_client("workspace-diagnostic-dynamic");

    let response = client.send_request(
        "workspace/diagnostic",
        json!({
            "identifier": "upstream-private",
            "previousResultIds": [{
                "uri": "file:///workspace/dynamic.rs",
                "value": "upstream-result"
            }],
            "partialResultToken": "upstream-partial",
            "workDoneToken": "upstream-work"
        }),
    );
    let messages: Vec<_> = response["result"]["items"][0]["items"]
        .as_array()
        .expect("aggregated diagnostics")
        .iter()
        .map(|diagnostic| diagnostic["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages, ["alpha", "zeta"]);

    for identifier in ["alpha", "zeta"] {
        let event = std::fs::read(events.path().join(format!(
            "workspace-diagnostic-dynamic.workspace-diagnostic-{identifier}.json"
        )))
        .expect("provider request event");
        let event: serde_json::Value = serde_json::from_slice(&event).unwrap();
        assert_eq!(event["params"]["identifier"], identifier);
        assert_eq!(event["params"]["previousResultIds"], json!([]));
        assert!(event["params"].get("partialResultToken").is_none());
        assert!(event["params"].get("workDoneToken").is_none());
    }
}

#[test]
fn workspace_diagnostic_cancels_every_dynamic_provider_request() {
    let (mut client, _config_dir, events) =
        init_dynamic_workspace_diagnostic_client("workspace-diagnostic-dynamic-cancel");
    let request_id =
        client.send_request_async("workspace/diagnostic", json!({ "previousResultIds": [] }));
    let mut expected_cancel_ids = BTreeSet::new();
    for identifier in ["alpha", "zeta"] {
        let request_event = events.path().join(format!(
            "workspace-diagnostic-dynamic-cancel.workspace-diagnostic-{identifier}.json"
        ));
        wait_for_file(&request_event);
        let request: serde_json::Value =
            serde_json::from_slice(&std::fs::read(request_event).expect("provider request event"))
                .expect("provider request JSON");
        expected_cancel_ids.insert(request["id"].to_string());
    }

    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["error"]["code"], -32800);

    let cancel_count = events
        .path()
        .join("workspace-diagnostic-dynamic-cancel.cancel.count");
    for _ in 0..200 {
        if std::fs::read_to_string(&cancel_count).ok().as_deref() == Some("2") {
            let actual_cancel_ids = std::fs::read_dir(events.path())
                .expect("cancel event directory")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("workspace-diagnostic-dynamic-cancel.cancel-id-")
                })
                .map(|entry| {
                    let event: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(entry.path()).expect("cancel event"))
                            .expect("cancel event JSON");
                    event["params"]["id"].to_string()
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(actual_cancel_ids, expected_cancel_ids);
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("both downstream provider requests must receive cancellation");
}
