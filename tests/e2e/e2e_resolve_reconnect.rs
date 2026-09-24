//! Completion and code-action resolves may use the current process for the same server.

use crate::helpers::lsp_client::LspClient;
use crate::helpers::lua_bridge::shutdown_client;
use serde_json::{Value, json};

const URI: &str = "file:///producer_resolve.md";

fn item_until(
    client: &mut LspClient,
    completion: bool,
    host: bool,
    ready: impl Fn(&Value) -> bool,
) -> Value {
    let line = if host { 0 } else { 3 };
    let (method, params) = if completion {
        (
            "textDocument/completion",
            json!({
                "textDocument": { "uri": URI },
                "position": { "line": line, "character": 1 }
            }),
        )
    } else {
        (
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": URI },
                "range": {
                    "start": { "line": line, "character": 0 },
                    "end": { "line": line, "character": 1 }
                },
                "context": { "diagnostics": [] }
            }),
        )
    };
    for _ in 0..300 {
        let response = client.send_request(method, params.clone());
        assert!(response.get("error").is_none(), "{response}");
        let items = if completion {
            &response["result"]["items"]
        } else {
            &response["result"]
        };
        if let Some(item) = items.as_array().and_then(|items| items.first())
            && ready(item)
        {
            return item.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("no item from the expected producer");
}

#[rstest::rstest]
#[case::host_completion(true, true)]
#[case::virtual_completion(true, false)]
#[case::host_action(false, true)]
#[case::virtual_action(false, false)]
fn e2e_resolve_uses_replacement_process(
    #[case] completion: bool,
    #[case] host: bool,
    #[values(false, true)] reject_old: bool,
) {
    let mode = if completion {
        "completion-resolve-plain"
    } else {
        "code-action-lazy"
    };
    let method = if completion {
        "completionItem/resolve"
    } else {
        "codeAction/resolve"
    };
    let resolved_field = if completion { "detail" } else { "edit" };
    let bin = env!("CARGO_BIN_EXE_mock-lsp-formatter");
    let config_dir = tempfile::TempDir::new().unwrap();
    let config_path = config_dir.path().join("producer.toml");
    std::fs::write(&config_path, "").unwrap();
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .build();
    let mut settings = json!({
        "languageServers": { "mock-producer": {
            "cmd": [bin, mode, "--stamp-resolve-process"], "languages": [if host { "markdown" } else { "lua" }]
        }},
        "languages": { "markdown": { "bridge": { "_self": { "enabled": host } } } }
    });
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(), "rootUri": null, "workspaceFolders": null,
            "capabilities": { "textDocument": { "codeAction": {
                "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } },
                "dataSupport": true, "resolveSupport": { "properties": ["edit"] }
            }}},
            "initializationOptions": settings
        }),
    );
    assert!(init.get("error").is_none(), "{init}");
    client.send_notification("initialized", json!({}));
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": URI, "languageId": "markdown", "version": 1,
            "text": "# Test\n\n```lua\nlocal x = 1\n```\n"
        }}),
    );
    let old = item_until(&mut client, completion, host, |_| true);
    let old_pid = old["data"]["kakehashi"]["inner"]["mockPid"]
        .as_u64()
        .unwrap();
    let initial = client.send_request(method, old.clone());
    assert!(initial.get("error").is_none(), "{initial}");
    assert!(!initial["result"][resolved_field].is_null(), "{initial}");

    // Changing the launch configuration forces a real process replacement
    // under the same pool key. The replacement may reject process-local data.
    settings["languageServers"]["mock-producer"]["cmd"] = json!([
        bin,
        mode,
        "--stamp-resolve-process",
        if reject_old {
            "--reject-old-resolve"
        } else {
            "replacement"
        }
    ]);
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": settings }),
    );
    let fresh = item_until(&mut client, completion, host, |item| {
        item["data"]["kakehashi"]["inner"]["mockPid"].as_u64() != Some(old_pid)
    });
    let new_pid = fresh["data"]["kakehashi"]["inner"]["mockPid"]
        .as_u64()
        .unwrap();
    assert_ne!(old_pid, new_pid, "a real process replacement is required");

    // Configuration reload may republish injection geometry. Refresh only the
    // document context so it cannot mask whether old opaque data reaches the
    // new process. Any producer key/generation stamps remain the original ones.
    let mut carried = old;
    for field in ["region_id", "offset", "incarnation", "content_version"] {
        if let Some(value) = fresh["data"]["kakehashi"].get(field) {
            carried["data"]["kakehashi"][field] = value.clone();
        }
    }
    let response = client.send_request(method, carried.clone());
    if reject_old {
        if completion {
            assert!(response.get("error").is_none(), "{response}");
            // Routing geometry may be normalized while the original completion
            // fields and the downstream's opaque data remain unchanged.
            assert_eq!(
                response["result"]["data"]["kakehashi"]["inner"],
                carried["data"]["kakehashi"]["inner"]
            );
            let mut actual = response["result"].clone();
            actual.as_object_mut().unwrap().remove("data");
            carried.as_object_mut().unwrap().remove("data");
            assert_eq!(actual, carried, "completion remains usable");
        } else {
            assert_eq!(response["error"]["code"], -32803, "{response}");
            assert!(response.get("result").is_none(), "{response}");
        }
    } else {
        assert_resolved(&response, completion, new_pid, old_pid);
    }
    // Requesting fresh items recovers even when the server rejects old data.
    let response = client.send_request(method, fresh);
    assert_resolved(&response, completion, new_pid, new_pid);
    shutdown_client(&mut client);
}

fn assert_resolved(response: &Value, completion: bool, new_pid: u64, data_pid: u64) {
    assert!(response.get("error").is_none(), "{response}");
    let actual = if completion {
        response["result"]["detail"].as_str().unwrap()
    } else {
        response["result"]["edit"]["changes"][URI][0]["newText"]
            .as_str()
            .unwrap()
    };
    assert_eq!(
        actual,
        format!("resolved-pid:{new_pid};data-pid:{data_pid}")
    );
}
