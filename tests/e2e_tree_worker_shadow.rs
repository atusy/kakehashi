#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::json;

#[test]
fn shadow_worker_matches_authoritative_incremental_lifecycle() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": {
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": ["keyword", "variable", "number"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                }
            },
            "initializationOptions": {
                "searchPaths": ["${KAKEHASHI_DATA_DIR}"]
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///tree-worker-shadow.rs";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn main() { let value = 100000; }\n"
            }
        }),
    );
    std::thread::sleep(Duration::from_millis(500));

    for version in 2..=21 {
        client.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{
                    "range": {
                        "start": { "line": 0, "character": 24 },
                        "end": { "line": 0, "character": 30 }
                    },
                    "rangeLength": 6,
                    "text": format!("{version:06}")
                }]
            }),
        );
        let response = client.send_request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri } }),
        );
        assert!(response.get("result").is_some());
    }
    std::thread::sleep(Duration::from_millis(500));
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    std::thread::sleep(Duration::from_millis(100));

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
    let status = client
        .wait_for_exit(Duration::from_secs(5))
        .expect("server must exit after shutdown");
    assert!(status.success());
    let stderr = client.drain_stderr();
    assert!(
        stderr.contains("shadow comparisons matched="),
        "missing shadow summary: {stderr}"
    );
    let summary = stderr
        .lines()
        .find(|line| line.contains("shadow comparisons matched="))
        .expect("shadow summary was checked above");
    let metric = |name: &str| {
        summary
            .split_whitespace()
            .find_map(|field| field.strip_prefix(&format!("{name}=")))
            .and_then(|value| value.trim_end_matches(',').parse::<u64>().ok())
            .unwrap_or_else(|| panic!("missing {name} in shadow summary: {summary}"))
    };
    assert!(metric("matched") > 0, "{summary}");
    assert_eq!(metric("mismatched"), 0, "{summary}");
    assert_eq!(metric("pending"), 0, "{summary}");
    assert!(!stderr.contains("shadow validation incomplete"), "{stderr}");
    assert!(!stderr.contains("tree mismatch"), "{stderr}");
    assert!(!stderr.contains("worker transport failed"), "{stderr}");
}
