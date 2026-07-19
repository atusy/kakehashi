#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::json;

fn initialize(client: &mut LspClient) {
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
}

fn shutdown_and_stderr(mut client: LspClient) -> String {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
    let status = client
        .wait_for_exit(Duration::from_secs(5))
        .expect("server must exit after shutdown");
    assert!(status.success());
    client.drain_stderr()
}

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

#[test]
fn systemic_worker_restart_full_resyncs_the_open_document() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("restart-once");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_ONCE_FILE",
            marker.to_string_lossy().into_owned(),
        )
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    let uri = "file:///tree-worker-restart.rs";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn restarted() { let value = 1; }\n"
            }
        }),
    );
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response.get("result").is_some());
    std::thread::sleep(Duration::from_secs(1));

    let stderr = shutdown_and_stderr(client);
    assert!(marker.exists(), "failure injection did not run: {stderr}");
    assert!(stderr.contains("requested restart"), "{stderr}");
    assert!(
        stderr.contains("full-resynced 1 open documents"),
        "{stderr}"
    );
    assert!(stderr.contains("shadow comparisons matched=1"), "{stderr}");
    assert!(stderr.contains("pending=0"), "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    eprintln!(
        "systemic recovery measurement: {}",
        stderr
            .lines()
            .find(|line| line.contains("restarted worker generation"))
            .expect("restart measurement log must be present")
    );
}

#[test]
fn systemic_worker_restart_measures_many_document_full_resync() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("restart-many-once");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_ONCE_FILE",
            marker.to_string_lossy().into_owned(),
        )
        .env("KAKEHASHI_TREE_WORKER_RESTART_URI_SUFFIX", "/trigger.rs")
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);

    let mut expected_bytes = 0;
    for index in 0..16 {
        let uri = format!("file:///resync-{index}.rs");
        let text = format!("fn document_{index}() {{}}\n").repeat(800);
        expected_bytes += text.len();
        client.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": text
                }
            }),
        );
    }
    std::thread::sleep(Duration::from_secs(1));
    let trigger_uri = "file:///trigger.rs";
    let trigger_text = "fn trigger_many_document_resync() {}\n";
    expected_bytes += trigger_text.len();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": trigger_uri,
                "languageId": "rust",
                "version": 1,
                "text": trigger_text
            }
        }),
    );
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": trigger_uri } }),
    );
    assert!(response.get("result").is_some());
    std::thread::sleep(Duration::from_secs(2));

    let stderr = shutdown_and_stderr(client);
    let recovery = stderr
        .lines()
        .find(|line| line.contains("restarted worker generation"))
        .unwrap_or_else(|| panic!("restart measurement log must be present: {stderr}"));
    assert!(
        recovery.contains("full-resynced 17 open documents"),
        "{stderr}"
    );
    assert!(
        recovery.contains(&format!("bytes={expected_bytes}")),
        "{stderr}"
    );
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    eprintln!("many-document recovery measurement: {recovery}");
}

#[test]
fn rejected_document_does_not_block_other_documents_during_full_resync() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("restart-after-rejection");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_ONCE_FILE",
            marker.to_string_lossy().into_owned(),
        )
        .env("KAKEHASHI_TREE_WORKER_RESTART_URI_SUFFIX", "/trigger.rs")
        .env("KAKEHASHI_TREE_WORKER_ERROR_URI_SUFFIX", "/rejected.rs")
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    for (uri, language, text) in [
        ("file:///rejected.rs", "rust", "fn rejected() {}\n"),
        ("file:///healthy.lua", "lua", "local healthy = true\n"),
    ] {
        client.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language,
                    "version": 1,
                    "text": text
                }
            }),
        );
    }
    std::thread::sleep(Duration::from_secs(1));

    let trigger_uri = "file:///trigger.rs";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": trigger_uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn trigger_recovery() {}\n"
            }
        }),
    );
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": trigger_uri } }),
    );
    assert!(response.get("result").is_some());
    std::thread::sleep(Duration::from_secs(2));

    let stderr = shutdown_and_stderr(client);
    assert!(
        stderr.contains("skipped shadow resync for file:///rejected.rs"),
        "{stderr}"
    );
    assert!(
        stderr.contains("full-resynced 2 open documents"),
        "{stderr}"
    );
    assert!(stderr.contains("shadow comparisons matched=2"), "{stderr}");
    assert!(stderr.contains("pending=0"), "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    assert!(!stderr.contains("disabled shadow tree tier"), "{stderr}");
}

#[test]
fn idle_worker_exit_is_detected_and_restarted_before_the_next_document() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("idle-crash-once");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_IDLE_CRASH_ONCE_FILE",
            marker.to_string_lossy().into_owned(),
        )
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    std::thread::sleep(Duration::from_secs(2));

    let uri = "file:///after-idle-restart.rs";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn after_idle_restart() {}\n"
            }
        }),
    );
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response.get("result").is_some());
    std::thread::sleep(Duration::from_secs(1));

    let stderr = shutdown_and_stderr(client);
    assert!(marker.exists(), "failure injection did not run: {stderr}");
    assert!(stderr.contains("exited while idle"), "{stderr}");
    assert!(
        stderr.contains("full-resynced 0 open documents"),
        "{stderr}"
    );
    assert!(stderr.contains("shadow comparisons matched=1"), "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    eprintln!(
        "idle recovery measurement: {}",
        stderr
            .lines()
            .find(|line| line.contains("restarted worker generation"))
            .expect("restart measurement log must be present")
    );
}

#[test]
fn crashed_grammar_is_quarantined_only_in_session_and_other_grammar_recovers() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("crash-once");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_CRASH_ONCE_FILE",
            marker.to_string_lossy().into_owned(),
        )
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///crashing.rs",
                "languageId": "rust",
                "version": 1,
                "text": "fn crashing() {}\n"
            }
        }),
    );
    std::thread::sleep(Duration::from_secs(1));

    let healthy_uri = "file:///healthy.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": healthy_uri,
                "languageId": "lua",
                "version": 1,
                "text": "local value = 1\n"
            }
        }),
    );
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": healthy_uri } }),
    );
    assert!(response.get("result").is_some());
    std::thread::sleep(Duration::from_secs(1));

    let stderr = shutdown_and_stderr(client);
    assert!(marker.exists(), "failure injection did not run: {stderr}");
    assert!(
        stderr.contains("quarantined grammar conservatively for this session"),
        "{stderr}"
    );
    assert!(stderr.contains("symbol=rust"), "{stderr}");
    assert!(stderr.contains("restarted worker generation"), "{stderr}");
    assert!(stderr.contains("shadow comparisons matched=1"), "{stderr}");
    assert!(stderr.contains("pending=0"), "{stderr}");
    assert!(!stderr.contains("disabled shadow tree tier"), "{stderr}");
    eprintln!(
        "crash recovery measurement: {}",
        stderr
            .lines()
            .find(|line| line.contains("restarted worker generation"))
            .expect("restart measurement log must be present")
    );
}
