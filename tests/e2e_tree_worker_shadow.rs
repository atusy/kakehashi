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

fn log_metric(line: &str, name: &str) -> u64 {
    line.split_whitespace()
        .find_map(|field| {
            field
                .trim_start_matches('(')
                .strip_prefix(&format!("{name}="))
        })
        .and_then(|value| value.trim_end_matches([',', ')']).parse::<u64>().ok())
        .unwrap_or_else(|| panic!("missing {name} in log line: {line}"))
}

fn shadow_metric(stderr: &str, name: &str) -> u64 {
    let summary = stderr
        .lines()
        .find(|line| line.contains("shadow comparisons matched="))
        .unwrap_or_else(|| panic!("missing shadow comparison summary: {stderr}"));
    log_metric(summary, name)
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
    assert!(shadow_metric(&stderr, "matched") > 0, "{stderr}");
    assert_eq!(shadow_metric(&stderr, "mismatched"), 0, "{stderr}");
    assert_eq!(shadow_metric(&stderr, "pending"), 0, "{stderr}");
    assert!(!stderr.contains("shadow validation incomplete"), "{stderr}");
    assert!(!stderr.contains("tree mismatch"), "{stderr}");
    assert!(!stderr.contains("worker transport failed"), "{stderr}");
}

#[test]
fn shadow_worker_matches_injected_node_and_navigation() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env("RUST_LOG", "kakehashi::tree_worker_shadow=debug")
        .build();
    initialize(&mut client);
    let uri = "file:///tree-worker-injected-node.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Heading\n\n```python\ny = 1 + 2\n```\n"
            }
        }),
    );
    let node = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 4 },
            "injection": true
        }),
    );
    let id = node["result"]["id"]
        .as_str()
        .expect("injected node must have an id");
    let parent = client.send_request(
        "kakehashi/node/parent",
        json!({ "textDocument": { "uri": uri }, "id": id }),
    );
    assert!(parent["result"].is_object(), "{parent:?}");

    let stderr = shutdown_and_stderr(client);
    assert!(stderr.contains("injection node matched"), "{stderr}");
    assert!(stderr.contains("node navigation matched"), "{stderr}");
    assert!(!stderr.contains("node mismatch"), "{stderr}");
    assert!(!stderr.contains("node navigation mismatch"), "{stderr}");
}

#[test]
fn authoritative_worker_serves_injected_node_accessors() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env("RUST_LOG", "kakehashi::tree_worker_shadow=debug")
        .build();
    initialize(&mut client);
    let uri = "file:///tree-worker-authoritative-node.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Heading\n\n```python\ny = 1 + 2\n```\n"
            }
        }),
    );
    let node = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 3, "character": 0 },
            "injection": true
        }),
    );
    let node = &node["result"];
    let id = node["id"].as_str().expect("worker node must have an id");
    assert_eq!(node["kind"], "identifier");

    let text = client.send_request(
        "kakehashi/node/text",
        json!({ "textDocument": { "uri": uri }, "id": id }),
    );
    assert_eq!(text["result"]["text"], "y");
    let parent = client.send_request(
        "kakehashi/node/parent",
        json!({ "textDocument": { "uri": uri }, "id": id }),
    );
    assert!(parent["result"].is_object(), "{parent:?}");
    let kind = client.send_request(
        "kakehashi/node/kind",
        json!({ "textDocument": { "uri": uri }, "id": id }),
    );
    assert_eq!(kind["result"]["kind"], "identifier");
    let semantic = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(semantic["result"]["data"].is_array(), "{semantic:?}");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                },
                "text": "\n"
            }]
        }),
    );
    let edited_node = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 4, "character": 0 },
            "injection": true
        }),
    );
    assert_eq!(
        edited_node["result"]["kind"], "identifier",
        "{edited_node:?}"
    );
    let edited_id = edited_node["result"]["id"].as_str().unwrap();
    let edited_range = client.send_request(
        "kakehashi/node/range",
        json!({ "textDocument": { "uri": uri }, "id": edited_id }),
    );
    assert_eq!(edited_range["result"]["start"]["line"], 4);
    let selection = client.send_request(
        "textDocument/selectionRange",
        json!({
            "textDocument": { "uri": uri },
            "positions": [{ "line": 4, "character": 0 }]
        }),
    );
    assert!(selection["result"].is_array(), "{selection:?}");
    let semantic_range = client.send_request(
        "textDocument/semanticTokens/range",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 5, "character": 0 }
            }
        }),
    );
    assert!(
        semantic_range["result"]["data"].is_array(),
        "{semantic_range:?}"
    );

    let stderr = shutdown_and_stderr(client);
    assert!(stderr.contains("Authoritative tree worker"), "{stderr}");
    assert!(!stderr.contains("node mismatch"), "{stderr}");
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
    assert_eq!(shadow_metric(&stderr, "matched"), 1, "{stderr}");
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
    assert_eq!(
        log_metric(recovery, "bytes"),
        expected_bytes as u64,
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
    assert_eq!(shadow_metric(&stderr, "matched"), 2, "{stderr}");
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
    // The debug E2E binary is hashed during the replacement handshake. Leave
    // enough room for idle detection, backoff, hashing, and the zero-document
    // resync before introducing the document whose service we verify below.
    std::thread::sleep(Duration::from_secs(5));

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
    assert_eq!(shadow_metric(&stderr, "matched"), 1, "{stderr}");
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
    assert_eq!(shadow_metric(&stderr, "matched"), 1, "{stderr}");
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
