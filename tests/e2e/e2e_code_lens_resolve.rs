//! E2E tests for bridged `codeLens/resolve` (#355 Phase 1), using the
//! `mock-lsp-formatter` test binary in `code-lens` mode (one unresolved lens,
//! resolve materializes a command echoing the lens data).
//!
//! Proves end-to-end:
//! - Unresolved lenses are forwarded (not dropped) with host-translated
//!   ranges, and `resolveProvider` is advertised.
//! - `codeLens/resolve` routes to the origin server, round-trips the
//!   downstream's own `data`, and returns the command with the range still in
//!   host coordinates.
//! - After an edit that shifts the region, resolve fails soft: the lens comes
//!   back unresolved instead of being translated with a stale offset.

use std::time::Duration;

use crate::helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

fn init_client() -> (LspClient, tempfile::TempDir) {
    let bin = mock_formatter_bin();
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("code_lens_resolve.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock-codelens": { "cmd": [bin, "code-lens"], "languages": ["lua"] }
            }}
        }),
    );
    assert_eq!(
        init_response["result"]["capabilities"]["codeLensProvider"]["resolveProvider"],
        json!(true),
        "codeLens resolveProvider must be advertised (#355)"
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

fn init_host_client_with_mode(mode: &str) -> (LspClient, tempfile::TempDir) {
    init_host_client_with_mode_and_cancel_dir(mode, None)
}

fn init_host_client_with_mode_and_cancel_dir(
    mode: &str,
    cancel_dir: Option<&std::path::Path>,
) -> (LspClient, tempfile::TempDir) {
    let bin = mock_formatter_bin();
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("host_code_lens_resolve.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

    let builder = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"));
    let builder = match cancel_dir {
        Some(cancel_dir) => builder.env("MOCK_LSP_CANCEL_DIR", cancel_dir.to_string_lossy()),
        None => builder,
    };
    let mut client = builder.build();

    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-codelens": {
                        "cmd": [bin, mode],
                        "languages": ["markdown"]
                    }
                },
                "languages": {
                    "markdown": {
                        "bridge": { "_self": { "enabled": true } }
                    }
                }
            }
        }),
    );
    assert_eq!(
        init_response["result"]["capabilities"]["codeLensProvider"]["resolveProvider"],
        json!(true),
        "codeLens resolveProvider must cover host-layer lenses"
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

fn init_host_client() -> (LspClient, tempfile::TempDir) {
    init_host_client_with_mode("code-lens")
}

/// Markdown host: the lua fence content sits on host line 3.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_code_lens_resolve.md";

fn open_markdown(client: &mut LspClient) {
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
    std::thread::sleep(Duration::from_millis(1000));
}

/// Issue `textDocument/codeLens`, retrying while the result is null (cold
/// downstream still handshaking).
fn code_lens_with_retry(client: &mut LspClient) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/codeLens",
            json!({ "textDocument": { "uri": MARKDOWN_URI } }),
        );
        assert!(
            response.get("error").is_none(),
            "unexpected codeLens error: {:?}",
            response.get("error")
        );
        let result = &response["result"];
        if result.is_null() {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let lenses = result
            .as_array()
            .cloned()
            .expect("non-null codeLens result must be an array");
        if lenses.is_empty() {
            // Region may not be resolved yet; keep retrying.
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        return lenses;
    }
    panic!("timed out waiting for a non-empty codeLens result");
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_code_lens_resolve_round_trips_to_origin_server() {
    let (mut client, _config_dir) = init_client();
    open_markdown(&mut client);

    // The mock's unresolved lens (virtual line 0) must arrive host-translated
    // (fence content is host line 3), command-less, with a routing envelope.
    let lenses = code_lens_with_retry(&mut client);
    assert_eq!(lenses.len(), 1);
    let lens = &lenses[0];
    assert_eq!(lens["range"]["start"]["line"], 3);
    assert!(
        lens.get("command").is_none() || lens["command"].is_null(),
        "lens must arrive unresolved; got: {lens:?}"
    );
    assert_eq!(
        lens["data"]["kakehashi"]["origin"], "mock-codelens",
        "lens must carry the routing envelope"
    );

    // Resolve: routed to the origin server, command materialized from the
    // downstream's own data (proving inner round-trip), range still host.
    let response = client.send_request("codeLens/resolve", lens.clone());
    assert!(response.get("error").is_none());
    let resolved = &response["result"];
    assert_eq!(
        resolved["command"]["title"], "mock resolved:lens-1",
        "resolve must reach the origin server with the downstream's original data; got: {resolved:?}"
    );
    assert_eq!(
        resolved["range"]["start"]["line"], 3,
        "resolved range must stay in host coordinates"
    );

    // Staleness: insert a line above the fence (shifts the region down one
    // line) and resolve the OLD lens again — it must fail soft (come back
    // unresolved) instead of translating with the stale offset. No sleep:
    // the ingress gate orders codeLens/resolve (a reader, keyed by the
    // envelope's host_uri) behind the wire-preceding didChange, so the
    // freshness check deterministically sees the post-edit tracker state.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MARKDOWN_URI, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "text": "extra line\n"
            }]
        }),
    );

    let response = client.send_request("codeLens/resolve", lens.clone());
    assert!(response.get("error").is_none());
    let stale = &response["result"];
    assert!(
        stale.get("command").is_none() || stale["command"].is_null(),
        "resolve against a shifted region must fail soft (stay unresolved); got: {stale:?}"
    );
    // The routing envelope must survive the fail-soft path intact — this
    // distinguishes the fail-soft return from any other command-less shape.
    assert_eq!(
        stale["data"]["kakehashi"]["origin"], "mock-codelens",
        "fail-soft must return the lens with its routing envelope intact; got: {stale:?}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_code_lens_resolve_round_trips_verbatim() {
    let (mut client, _config_dir) = init_host_client();
    open_markdown(&mut client);

    let lenses = code_lens_with_retry(&mut client);
    assert_eq!(lenses.len(), 1);
    let lens = &lenses[0];
    assert_eq!(lens["range"]["start"]["line"], 0);
    assert_eq!(
        lens["data"]["kakehashi"]["origin"], "mock-codelens",
        "a resolvable host lens must carry its origin server: {lens}"
    );
    assert_eq!(
        lens["data"]["kakehashi"]["host_layer"], true,
        "the resolve path must distinguish verbatim host coordinates: {lens}"
    );

    let response = client.send_request("codeLens/resolve", lens.clone());
    assert!(
        response.get("error").is_none(),
        "unexpected host codeLens/resolve error: {:?}",
        response.get("error")
    );
    let resolved = &response["result"];
    assert_eq!(
        resolved["command"]["title"], "mock resolved:lens-1",
        "resolve must reach the host origin with its original data: {resolved}"
    );
    assert_eq!(
        resolved["range"], lens["range"],
        "the complete host range must pass through without virtual translation"
    );
    assert_eq!(resolved["data"]["kakehashi"]["host_layer"], true);

    shutdown(&mut client);
}

#[test]
fn e2e_host_code_lens_without_resolve_keeps_original_data_bare() {
    let (mut client, _config_dir) = init_host_client_with_mode("code-lens-no-resolve");
    open_markdown(&mut client);

    let lenses = code_lens_with_retry(&mut client);
    assert_eq!(lenses.len(), 1);
    assert_eq!(lenses[0]["data"], json!({ "mock": "lens-1" }));

    shutdown(&mut client);
}

#[test]
fn e2e_host_code_lens_reserved_data_cannot_impersonate_bridge_envelope() {
    let (mut client, _config_dir) =
        init_host_client_with_mode("code-lens-no-resolve-reserved-data");
    open_markdown(&mut client);

    let lens = code_lens_with_retry(&mut client).remove(0);
    assert_eq!(
        lens["data"]["kakehashi"]["inner"]["kakehashi"]["origin"], "forged",
        "foreign data using the reserved key must be nested under a bridge-owned envelope"
    );
    assert_eq!(lens["data"]["kakehashi"]["origin"], "mock-codelens");

    let response = client.send_request("codeLens/resolve", lens);
    assert_eq!(response["result"].get("command"), None);
    assert_eq!(
        response["result"]["data"]["kakehashi"]["inner"]["kakehashi"]["origin"],
        "forged"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_code_lens_from_replaced_connection_stays_unresolved() {
    let bin = mock_formatter_bin();
    let (mut client, _config_dir) = init_host_client();
    open_markdown(&mut client);
    let old_lens = code_lens_with_retry(&mut client).remove(0);

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": {
            "languageServers": {
                "mock-codelens": {
                    "cmd": [bin, "code-lens-replacement"],
                    "languages": ["markdown"]
                }
            },
            "languages": {
                "markdown": { "bridge": { "_self": { "enabled": true } } }
            }
        }}),
    );
    let replacement_lens = code_lens_with_retry(&mut client).remove(0);
    let replacement = client.send_request("codeLens/resolve", replacement_lens);
    assert_eq!(
        replacement["result"]["command"]["title"], "replacement resolved:lens-1",
        "precondition: the replacement process must be active"
    );

    let response = client.send_request("codeLens/resolve", old_lens);
    assert_eq!(
        response["result"].get("command"),
        None,
        "opaque lens data must not be sent to a replacement process"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_code_lens_from_closed_incarnation_stays_unresolved() {
    let (mut client, _config_dir) = init_host_client();
    open_markdown(&mut client);
    let lens = code_lens_with_retry(&mut client).remove(0);

    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    open_markdown(&mut client);
    let replacement_lenses = code_lens_with_retry(&mut client);
    assert_eq!(replacement_lenses.len(), 1);

    let response = client.send_request("codeLens/resolve", lens);
    assert_eq!(response["result"].get("command"), None);
    assert_eq!(response["result"]["data"]["kakehashi"]["host_layer"], true);

    shutdown(&mut client);
}

#[test]
fn e2e_host_code_lens_resolve_cancel_returns_request_cancelled() {
    let cancel_dir = tempfile::TempDir::new().expect("cancel dir");
    let request_file = cancel_dir
        .path()
        .join("code-lens-slow-resolve.request.json");
    let cancel_file = cancel_dir.path().join("code-lens-slow-resolve.cancel.json");
    let (mut client, _config_dir) = init_host_client_with_mode_and_cancel_dir(
        "code-lens-slow-resolve",
        Some(cancel_dir.path()),
    );
    open_markdown(&mut client);
    let lens = code_lens_with_retry(&mut client).remove(0);

    let request_id = client.send_request_async("codeLens/resolve", lens);
    let started = (0..100).any(|_| {
        if request_file.exists() {
            true
        } else {
            std::thread::sleep(Duration::from_millis(50));
            false
        }
    });
    assert!(started, "resolve request must reach the downstream server");
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["error"]["code"], -32800, "{response}");
    let forwarded = (0..100).any(|_| {
        if cancel_file.exists() {
            true
        } else {
            std::thread::sleep(Duration::from_millis(50));
            false
        }
    });
    assert!(
        forwarded,
        "cancel must be forwarded to the downstream server"
    );

    shutdown(&mut client);
}
