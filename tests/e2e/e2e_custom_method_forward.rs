//! E2E tests for custom-method-host-forwarding: a method kakehashi does not
//! implement is forwarded to the host document's servers when — and only
//! when — `bridge._self.aggregation` names it explicitly. Requests get the
//! first non-empty downstream result; notifications fan out to every
//! selected server. The `custom-echo` mock advertises no capability for the
//! methods, so a successful echo also proves the forward is capability-blind.

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const MARKDOWN: &str = "# Title\n\nprose\n";
const MARKDOWN_URI: &str = "file:///test_custom_forward.md";

const FORWARDING_CONFIG: &str = r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."custom/ping"]
priorities = ["mock-host"]

[languages.markdown.bridge._self.aggregation."custom/merged"]
priorities = ["mock-host"]
strategy = "concatenated"

# Inherited by every method entry above that sets no strategy of its own.
# The typed host methods ignore an inherited `concatenated`; so must the
# forward, or this one line would break every forwarded method.
[languages.markdown.bridge._self.aggregation._]
strategy = "concatenated"
"#;

fn init_client(config_toml: &str) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("custom_forward.toml");
    std::fs::write(&config_path, config_toml).expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    let _init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host": {
                        "cmd": [mock_bin(), "custom-echo"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MARKDOWN_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MARKDOWN
            }
        }),
    );
    (client, config_dir)
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// LSP `RequestFailed`: well-formed request, refused for a server-side reason.
const REQUEST_FAILED: i64 = -32803;

fn error_code(response: &Value) -> Option<i64> {
    response.pointer("/error/code").and_then(Value::as_i64)
}

/// Send `custom/echo` until the host server answers. Like every host
/// request, the forward fails fast while the downstream is still
/// initializing (an empty result; "the next request gets it"), and didOpen
/// is what spawned it, so the first probes after open may come back `null`.
fn echo_until_answered(client: &mut LspClient, params: Value) -> Value {
    for _ in 0..300 {
        let response = client.send_request("custom/echo", params.clone());
        assert!(
            response.get("error").is_none(),
            "forwarded request must not error; got {response}"
        );
        if !response["result"].is_null() {
            return response["result"].clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("host server never answered custom/echo");
}

#[test]
fn e2e_custom_request_is_forwarded_verbatim_to_the_host_server() {
    let (mut client, _config_dir) = init_client(FORWARDING_CONFIG);

    let params = json!({
        "textDocument": { "uri": MARKDOWN_URI },
        "position": { "line": 2, "character": 1 },
        "extra": { "nested": [1, 2, 3] }
    });
    let result = echo_until_answered(&mut client, params.clone());
    assert_eq!(result["method"], "custom/echo");
    assert_eq!(
        result["params"], params,
        "params must reach the server verbatim (real URI, no translation)"
    );
    assert_eq!(
        result["opened"], true,
        "the host document must be synced to the server before the request"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_custom_notification_is_forwarded_to_the_host_server() {
    let (mut client, _config_dir) = init_client(FORWARDING_CONFIG);

    let ping = json!({ "textDocument": { "uri": MARKDOWN_URI }, "n": 7 });
    client.send_notification("custom/ping", ping.clone());

    // The notification and the probe request are independent handler tasks,
    // so the probe may overtake the notification; poll until it shows up.
    let mut recorded = Value::Null;
    for _ in 0..300 {
        let result = echo_until_answered(
            &mut client,
            json!({ "textDocument": { "uri": MARKDOWN_URI } }),
        );
        recorded = result["notifications"].clone();
        if recorded.as_array().is_some_and(|n| !n.is_empty()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        recorded,
        json!([{ "method": "custom/ping", "params": ping }]),
        "the notification must reach the host server exactly once, verbatim"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_unlisted_custom_request_keeps_method_not_found() {
    let (mut client, _config_dir) = init_client(FORWARDING_CONFIG);

    let response = client.send_request(
        "custom/unlisted",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert_eq!(
        error_code(&response),
        Some(METHOD_NOT_FOUND),
        "a method without a literal aggregation entry is not forwarded; got {response}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_custom_request_without_host_opt_in_keeps_method_not_found() {
    // The aggregation entry exists but `_self.enabled` does not: the host
    // layer is off, so nothing is forwarded and the router's answer stands.
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge._self.aggregation."custom/echo"]
priorities = ["mock-host"]
"#,
    );

    let response = client.send_request(
        "custom/echo",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert_eq!(
        error_code(&response),
        Some(METHOD_NOT_FOUND),
        "got {response}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_custom_request_without_text_document_is_invalid_params() {
    let (mut client, _config_dir) = init_client(FORWARDING_CONFIG);

    let response = client.send_request("custom/echo", json!({ "no": "document" }));
    assert_eq!(
        error_code(&response),
        Some(INVALID_PARAMS),
        "a forwardable method needs textDocument.uri to pick a host; got {response}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_concatenated_strategy_on_a_forwarded_request_is_request_failed() {
    let (mut client, _config_dir) = init_client(FORWARDING_CONFIG);

    let response = client.send_request(
        "custom/merged",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    assert_eq!(
        error_code(&response),
        Some(REQUEST_FAILED),
        "only `preferred` can combine results of unknown shape; got {response}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_built_in_method_is_not_shadowed_by_forwarding() {
    // `textDocument/hover` has a handler; the router answers it (empty here,
    // since the mock advertises no hoverProvider) and the forward never runs.
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."textDocument/hover"]
priorities = ["mock-host"]
"#,
    );

    let response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": MARKDOWN_URI },
            "position": { "line": 0, "character": 0 }
        }),
    );
    assert!(response.get("error").is_none(), "got {response}");
    assert_eq!(response["result"], Value::Null);

    shutdown(&mut client);
}
