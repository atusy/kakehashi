//! E2E tests for the downstream peer protocol (bridge-peer-protocol): a
//! downstream server discovers other running downstream servers through
//! kakehashi and proxies a request to one of them, over the real wire.
//!
//! The `mock-lsp-formatter` binary's `peer-caller` mode performs that flow
//! when its `mock.peer` command is executed and reports what it saw in the
//! command's result, so one editor-side `workspace/executeCommand` proves
//! feature detection, discovery, self-exclusion, forwarding, and the
//! bridge-level error contract end to end.

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// One lua fence so both lua servers are started by the open.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_bridge_peer.md";

/// The routed command name kakehashi decodes back to the caller's exact
/// client-fallback connection. The layout is the normative one in
/// execute-command-routing-token (tag `c`, empty root), so this test is a
/// consumer of that contract rather than of an implementation detail.
const CALLER_COMMAND: &str = "kakehashi|c|mock-caller||mock.peer";

fn init_client() -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("bridge_peer.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");
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
                    "mock-caller": {
                        "cmd": [mock_bin(), "peer-caller"],
                        "languages": ["lua"]
                    },
                    "mock-target": {
                        "cmd": [mock_bin(), "definition"],
                        "languages": ["lua"]
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

/// Execute `mock.peer` on the caller, asking it to forward `method` (with
/// `params`, or the mock's default when null) to the peer named `target`, and
/// return the caller's report.
fn peer_command(client: &mut LspClient, target: &str, method: &str, params: Value) -> Value {
    let mut arguments = vec![json!(target), json!(method)];
    if !params.is_null() {
        arguments.push(params);
    }
    let response = client.send_request(
        "workspace/executeCommand",
        json!({ "command": CALLER_COMMAND, "arguments": arguments }),
    );
    assert!(
        response.get("error").is_none(),
        "executeCommand must not fail: {:?}",
        response.get("error")
    );
    response["result"].clone()
}

/// The target becomes discoverable only once it is running; retry until the
/// caller reports it.
fn peer_command_until_discovered(
    client: &mut LspClient,
    target: &str,
    method: &str,
    params: Value,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..300 {
        let report = peer_command(client, target, method, params.clone());
        assert!(
            report.is_object(),
            "the dispatch failed soft instead of reaching the caller: {report:?}"
        );
        let discovered = report["peers"]
            .as_array()
            .is_some_and(|peers| peers.iter().any(|peer| peer["name"] == target));
        if discovered {
            return report;
        }
        last = report;
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("{target} never became discoverable; last report: {last:?}")
}

#[test]
fn downstream_server_discovers_and_proxies_to_a_running_peer() {
    let (mut client, _config_dir) = init_client();

    let report =
        peer_command_until_discovered(&mut client, "mock-target", "custom/echo", Value::Null);
    assert_eq!(
        report["bridgePeer"], true,
        "kakehashi must advertise the peer API in the downstream initialize"
    );
    let names = report["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|peer| peer["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec!["mock-target"],
        "the caller is excluded from its own discovery"
    );
    assert_eq!(
        report["forwarded"]["result"],
        json!({
            "result": {
                "method": "custom/echo",
                "hasParams": true,
                "params": { "probe": true }
            }
        }),
        "the inner method and params must reach the target unchanged and its \
         answer come back wrapped: {report:?}"
    );

    shutdown(&mut client);
}

/// The peer methods exist only on downstream connections; the editor-facing
/// service must not dispatch them (bridge-peer-protocol, per-side dispatch).
#[test]
fn peer_methods_are_not_editor_facing() {
    let (mut client, _config_dir) = init_client();

    for method in ["kakehashi/bridge/peer", "kakehashi/bridge/peer/request"] {
        let response = client.send_request(method, json!({}));
        assert_eq!(
            response["error"]["code"], -32601,
            "{method} must be MethodNotFound on the editor side: {response:?}"
        );
    }

    shutdown(&mut client);
}

/// A request whose partial results would stream to nobody is refused with
/// its own reason instead of answering with an empty final result.
#[test]
fn partial_result_tokens_are_refused_over_the_wire() {
    let (mut client, _config_dir) = init_client();

    let report = peer_command_until_discovered(
        &mut client,
        "mock-target",
        "custom/echo",
        json!({ "partialResultToken": "batch-1" }),
    );
    assert_eq!(report["forwarded"]["error"]["code"], -32803);
    assert_eq!(
        report["forwarded"]["error"]["data"]["reason"], "partialResultsUnsupported",
        "{report:?}"
    );

    shutdown(&mut client);
}

#[test]
fn lifecycle_methods_are_denied_over_the_wire() {
    let (mut client, _config_dir) = init_client();

    // Every retry asks to forward `shutdown`, but denial precedes peer
    // resolution, so no target ever receives a lifecycle request.
    let report = peer_command_until_discovered(&mut client, "mock-target", "shutdown", Value::Null);
    assert_eq!(report["forwarded"]["error"]["code"], -32803);
    assert_eq!(
        report["forwarded"]["error"]["data"]["reason"], "methodDenied",
        "a lifecycle method must be refused with the documented reason: {report:?}"
    );

    shutdown(&mut client);
}
