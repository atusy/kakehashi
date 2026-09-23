//! Opaque completion and action data belongs to the process that minted it.

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
fn e2e_resolve_rejects_replaced_producer(#[case] completion: bool, #[case] host: bool) {
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
            "cmd": [bin, mode], "languages": [if host { "markdown" } else { "lua" }]
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
    let old_envelope = &old["data"]["kakehashi"];
    assert!(old_envelope["connection_key"].is_object(), "{old}");
    assert!(old_envelope["connection_generation"].is_u64(), "{old}");
    let initial = client.send_request(method, old.clone());
    assert!(initial.get("error").is_none(), "{initial}");
    assert!(!initial["result"][resolved_field].is_null(), "{initial}");

    // Legacy partial stamps must fail soft even while the original producer lives.
    for field in ["connection_key", "connection_generation"] {
        let mut legacy = old.clone();
        legacy["data"]["kakehashi"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        let response = client.send_request(method, legacy.clone());
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"], legacy);
    }

    // The mock ignores trailing arguments, but the changed launch configuration
    // forces a real process replacement under the same pool key.
    settings["languageServers"]["mock-producer"]["cmd"] = json!([bin, mode, "replacement"]);
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": settings }),
    );
    let fresh = item_until(&mut client, completion, host, |item| {
        item["data"]["kakehashi"]["connection_generation"] != old_envelope["connection_generation"]
    });
    assert_eq!(
        fresh["data"]["kakehashi"]["connection_key"],
        old_envelope["connection_key"]
    );
    let current = client.send_request(method, fresh.clone());
    assert!(current.get("error").is_none(), "{current}");
    assert!(!current["result"][resolved_field].is_null(), "{current}");

    // Use current geometry/incarnation so document freshness cannot mask an
    // incorrect dispatch of process-local data to the replacement.
    let mut stale = fresh;
    for field in ["connection_key", "connection_generation", "inner"] {
        stale["data"]["kakehashi"][field] = old_envelope[field].clone();
    }
    let rejected = client.send_request(method, stale.clone());
    assert!(rejected.get("error").is_none(), "{rejected}");
    assert_eq!(rejected["result"], stale);
    shutdown_client(&mut client);
}
