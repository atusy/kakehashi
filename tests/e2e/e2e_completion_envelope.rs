//! The completion routing envelope is minted only for an origin that
//! advertises `completionItem/resolve` (reserved-key collision aside); a
//! non-resolving origin's items reach the client bare, on the virt layer as
//! on the host layer (`e2e_host_bridge` covers the host side).

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lua_bridge::shutdown_client;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_completion_envelope.md";

fn init_virtual_completion_client(mode: &str) -> (LspClient, tempfile::TempDir, Value) {
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("completion_envelope.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"))
        .build();

    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-completion": {
                        "cmd": [mock_bin(), mode],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    assert_eq!(
        init["result"]["capabilities"]["completionProvider"]["resolveProvider"],
        json!(true),
        "kakehashi advertises resolveProvider whatever the downstream does"
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

    let item = (0..300)
        .find_map(|_| {
            let resp = client.send_request(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": MARKDOWN_URI },
                    "position": { "line": 3, "character": 11 }
                }),
            );
            assert!(
                resp.get("error").is_none(),
                "unexpected completion error: {:?}",
                resp.get("error")
            );
            let first = resp["result"]["items"]
                .as_array()
                .and_then(|items| items.first().cloned());
            if first.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            first
        })
        .expect("virt completion returns an item");

    (client, config_dir, item)
}

#[test]
fn e2e_virtual_completion_from_non_resolving_server_keeps_its_data() {
    let (mut client, _config_dir, item) = init_virtual_completion_client("completion-no-resolve");

    assert_eq!(item["label"], "./test");
    assert!(
        item["data"].get("kakehashi").is_none(),
        "a producer that cannot resolve must keep its own payload: {item}"
    );
    assert!(
        item["data"]["mockPath"]
            .as_str()
            .is_some_and(|uri| uri.contains("kakehashi-virtual")),
        "the payload is the downstream's own, minted for the virtual document: {item}"
    );

    let response = client.send_request("completionItem/resolve", item.clone());
    assert!(
        response.get("error").is_none(),
        "unexpected resolve error: {:?}",
        response.get("error")
    );
    assert_eq!(
        response["result"], item,
        "a bare item has no origin to route to and comes back unchanged"
    );

    shutdown_client(&mut client);
}

#[test]
fn e2e_virtual_completion_from_resolving_server_is_enveloped() {
    let (mut client, _config_dir, item) = init_virtual_completion_client("completion-resolve");

    assert_eq!(item["data"]["kakehashi"]["origin"], "mock-completion");
    assert!(
        item["data"]["kakehashi"]["inner"]["mockPath"].is_string(),
        "the downstream payload is nested as inner: {item}"
    );

    shutdown_client(&mut client);
}

/// A completion list is designed to outlive edits: the editor filters it
/// locally while the user keeps typing and resolves an item on accept, which
/// itself edits. An edit inside the fence — same shape, or typing that grows
/// the region — must therefore NOT refuse the resolve: the region is
/// the same region, and the downstream computes the lazy fields against its
/// own copy of the text, which the bridge keeps in step. (Inlay hints and
/// lazy code actions, which the editor re-requests on every edit, are
/// refused instead.)
#[test]
fn e2e_virtual_completion_resolve_survives_an_edit_inside_the_region() {
    let (mut client, _config_dir, item) =
        init_virtual_completion_client("completion-resolve-plain");
    assert_eq!(item["data"]["kakehashi"]["origin"], "mock-completion");

    // Positive control: before any edit the item resolves.
    let response = client.send_request("completionItem/resolve", item.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert!(
        response["result"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("mock-resolved:")),
        "the control resolve must reach the downstream: {response}"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MARKDOWN_URI, "version": 2 },
            "contentChanges": [{ "text": "# Test\n\n```lua\nlocal y = 1\n```\n" }]
        }),
    );
    let response = client.send_request("completionItem/resolve", item.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert!(
        response["result"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("mock-resolved:")),
        "a resolve after an edit that kept the region must still reach the downstream: {response}"
    );

    // Typing that grows the fence moves the region's end; the region is
    // still the same one at the same offset.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MARKDOWN_URI, "version": 3 },
            "contentChanges": [{ "text": "# Test\n\n```lua\nlocal y = 12\nlocal z = 3\n```\n" }]
        }),
    );
    let response = client.send_request("completionItem/resolve", item.clone());
    assert!(response.get("error").is_none(), "{response}");
    assert!(
        response["result"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("mock-resolved:")),
        "a resolve after typing that grew the region must still reach the downstream: {response}"
    );

    shutdown_client(&mut client);
}

/// A close and reopen can complete while the downstream is still answering a
/// resolve; the reply then belongs to the closed document and must not be
/// surfaced into the reopened one.
#[test]
fn e2e_virtual_completion_resolve_reply_after_a_reopen_is_discarded() {
    let (mut client, _config_dir, item) =
        init_virtual_completion_client("completion-resolve-reopen-delayed");

    let request_id = client.send_request_async("completionItem/resolve", item.clone());
    assert!(
        client.wait_for_log_message(
            "completion-resolve-started",
            std::time::Duration::from_secs(10)
        ),
        "resolve must reach the downstream before the reopen"
    );
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": MARKDOWN_URI } }),
    );
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": MARKDOWN_URI,
            "languageId": "markdown",
            "version": 1,
            "text": MARKDOWN
        }}),
    );
    // The mock answers the parked resolve when the reopened document asks for
    // completions again; this request is only that trigger.
    let _trigger = client.send_request_async(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": MARKDOWN_URI },
            "position": { "line": 3, "character": 11 }
        }),
    );
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response["result"], item,
        "a reply that lands after a reopen must leave the item unresolved: {response}"
    );

    shutdown_client(&mut client);
}
