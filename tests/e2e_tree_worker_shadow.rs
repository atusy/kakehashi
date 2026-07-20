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

fn wait_for_file(path: &std::path::Path, failure: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{failure}");
}

fn open_rust_and_request_tokens(client: &mut LspClient, uri: &str, text: &str) {
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
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response.get("result").is_some(), "{response:?}");
}

#[test]
fn grammar_work_does_not_require_a_parent_round_trip_acknowledgment() {
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_ABORT_ON_HAZARD_ACK", "1")
        .build();
    initialize(&mut client);

    let started = std::time::Instant::now();
    open_rust_and_request_tokens(
        &mut client,
        "file:///committed-hazard.rs",
        "fn committed_hazard() {}\n",
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "grammar request waited for worker restart: {:?}",
        started.elapsed(),
    );

    let stderr = shutdown_and_stderr(client);
    assert!(!stderr.contains("restarted worker generation"), "{stderr}");
}

fn assert_host_tier_available(client: &mut LspClient, command: &str) {
    let response = client.send_request(
        "workspace/executeCommand",
        json!({ "command": command, "arguments": [] }),
    );
    assert!(response.get("result").is_some(), "{response:?}");
}

#[cfg(unix)]
#[test]
fn normal_shutdown_terminates_worker_descendants() {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let directory = tempfile::tempdir().unwrap();
    let pid_path = directory.path().join("worker-descendant.pid");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env(
            "KAKEHASHI_TREE_WORKER_DESCENDANT_PID_FILE",
            pid_path.to_string_lossy(),
        )
        .build();
    initialize(&mut client);
    wait_for_file(&pid_path, "worker descendant pid was not published");
    let descendant = Pid::from_raw(
        std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    );

    let _stderr = shutdown_and_stderr(client);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let descendant_survived = loop {
        match kill(descendant, None) {
            Err(Errno::ESRCH) => break false,
            Ok(()) | Err(Errno::EPERM) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(()) | Err(Errno::EPERM) => break true,
            Err(error) => panic!("unexpected descendant liveness error: {error}"),
        }
    };
    if descendant_survived {
        let _ = kill(descendant, Signal::SIGKILL);
    }
    assert!(!descendant_survived, "worker descendant survived shutdown");
}

#[cfg(unix)]
#[test]
fn parent_death_terminates_worker_with_a_hung_compute_thread() {
    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let directory = tempfile::tempdir().unwrap();
    let worker_pid_path = directory.path().join("worker.pid");
    let hang_marker = directory.path().join("hung-compute");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env(
            "KAKEHASHI_TREE_WORKER_PID_FILE",
            worker_pid_path.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_ONCE_FILE",
            hang_marker.to_string_lossy(),
        )
        .env("KAKEHASHI_TREE_WORKER_HANG_URI_SUFFIX", "/hung.rs")
        .build();
    initialize(&mut client);
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///hung.rs",
                "languageId": "rust",
                "version": 1,
                "text": "fn hangs_forever() {}\n"
            }
        }),
    );
    wait_for_file(&hang_marker, "worker compute thread did not enter the hang");
    let worker = Pid::from_raw(
        std::fs::read_to_string(&worker_pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    );

    drop(client);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let worker_survived = loop {
        match kill(worker, None) {
            Err(Errno::ESRCH) => break false,
            Ok(()) | Err(Errno::EPERM) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(()) | Err(Errno::EPERM) => break true,
            Err(error) => panic!("unexpected worker liveness error: {error}"),
        }
    };
    if worker_survived {
        let _ = kill(worker, Signal::SIGKILL);
    }
    assert!(!worker_survived, "worker survived parent death");
}

#[cfg(windows)]
fn run_windows_parent_death_case(skip_job: bool, skip_parent_monitor: bool) {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};

    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    fn open_process(pid: u32) -> OwnedHandle {
        // SAFETY: OpenProcess returns a newly owned kernel handle on success.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(
            !handle.is_null(),
            "failed to open process {pid}: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the successful OpenProcess result is owned by this test.
        unsafe { OwnedHandle::from_raw_handle(handle) }
    }

    fn wait_for_termination(handle: &OwnedHandle) -> bool {
        use std::os::windows::io::AsRawHandle as _;

        // SAFETY: the owned process handle remains valid for the duration of the wait.
        let result = unsafe { WaitForSingleObject(handle.as_raw_handle(), 2_000) };
        if result == WAIT_TIMEOUT {
            // Avoid leaking a deliberately surviving RED-test process into the runner.
            unsafe { TerminateProcess(handle.as_raw_handle(), 99) };
        }
        result == WAIT_OBJECT_0
    }

    let directory = tempfile::tempdir().unwrap();
    let worker_pid_path = directory.path().join("worker.pid");
    let descendant_pid_path = directory.path().join("worker-descendant.pid");
    let hang_marker = directory.path().join("hung-compute");
    let mut builder = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env(
            "KAKEHASHI_TREE_WORKER_PID_FILE",
            worker_pid_path.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_AFTER_START_FILE",
            hang_marker.to_string_lossy(),
        );
    if !skip_job {
        builder = builder
            .env(
                "KAKEHASHI_TREE_WORKER_DESCENDANT_PID_FILE",
                descendant_pid_path.to_string_lossy(),
            )
            .env(
                "KAKEHASHI_TREE_WORKER_DESCENDANT_EXE",
                env!("CARGO_BIN_EXE_worker-descendant"),
            );
    } else {
        builder = builder.env("KAKEHASHI_TREE_WORKER_TEST_SKIP_JOB_ASSIGNMENT", "1");
    }
    if skip_parent_monitor {
        builder = builder.env("KAKEHASHI_TREE_WORKER_TEST_SKIP_PARENT_MONITOR", "1");
    }
    let mut client = builder.build();
    initialize(&mut client);
    wait_for_file(&hang_marker, "worker compute thread did not enter the hang");
    let worker = open_process(
        std::fs::read_to_string(worker_pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    );
    let descendant = if skip_job {
        None
    } else {
        wait_for_file(
            &descendant_pid_path,
            "worker descendant pid was not published",
        );
        Some(open_process(
            std::fs::read_to_string(descendant_pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        ))
    };

    client.force_kill_and_wait();
    let worker_terminated = wait_for_termination(&worker);
    let descendant_terminated = descendant
        .as_ref()
        .is_none_or(|descendant| wait_for_termination(descendant));
    assert!(worker_terminated, "worker survived parent death");
    assert!(
        descendant_terminated,
        "worker descendant survived parent death"
    );
}

#[cfg(windows)]
#[test]
fn windows_parent_handle_terminates_hung_worker() {
    run_windows_parent_death_case(true, false);
}

#[cfg(windows)]
#[test]
fn windows_job_terminates_worker_descendants() {
    run_windows_parent_death_case(false, true);
}

#[cfg(windows)]
#[test]
fn windows_combined_supervision_terminates_hung_worker_and_descendants() {
    run_windows_parent_death_case(false, false);
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
    let Some(id) = node["id"].as_str() else {
        let stderr = shutdown_and_stderr(client);
        panic!("worker node must have an id: {node:?}\n{stderr}");
    };
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
    assert!(
        semantic["result"]["data"]
            .as_array()
            .is_some_and(|data| !data.is_empty()),
        "{semantic:?}"
    );

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
        semantic_range["result"]["data"]
            .as_array()
            .is_some_and(|data| !data.is_empty()),
        "{semantic_range:?}"
    );

    let stderr = shutdown_and_stderr(client);
    assert!(stderr.contains("Authoritative tree worker"), "{stderr}");
    assert!(
        stderr.contains("authoritative worker owns didOpen parsing"),
        "authoritative didOpen must not create a parent-owned tree: {stderr}"
    );
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
fn repeated_systemic_failures_open_the_breaker_once_and_keep_lsp_alive() {
    let directory = tempfile::tempdir().unwrap();
    let breaker_open = directory.path().join("breaker-open");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_ALWAYS_URI_SUFFIX",
            "/breaker.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_BREAKER_OPEN_FILE",
            breaker_open.to_string_lossy().into_owned(),
        )
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    let uri = "file:///breaker.rs";
    open_rust_and_request_tokens(&mut client, uri, "fn breaker() { let value = 1; }\n");
    wait_for_file(&breaker_open, "systemic failures did not open breaker");

    for version in 2..=5 {
        client.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": format!("fn breaker() {{ let value = {version}; }}\n") }]
            }),
        );
    }
    assert_host_tier_available(&mut client, "kakehashi.test.host-tier-after-breaker");

    let stderr = shutdown_and_stderr(client);
    assert_eq!(
        stderr
            .matches("disabled shadow tree tier after restart budget exhaustion")
            .count(),
        1,
        "{stderr}"
    );
    assert_eq!(stderr.matches("full resync failed").count(), 2, "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
}

#[test]
fn configuration_change_allows_one_failed_half_open_probe() {
    let directory = tempfile::tempdir().unwrap();
    let breaker_open = directory.path().join("breaker-open");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_ALWAYS_URI_SUFFIX",
            "/half-open.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_BREAKER_OPEN_FILE",
            breaker_open.to_string_lossy().into_owned(),
        )
        .env("KAKEHASHI_TREE_WORKER_FAST_COOLDOWN_MS", "25")
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    let uri = "file:///half-open.rs";
    open_rust_and_request_tokens(&mut client, uri, "fn half_open() { let value = 1; }\n");
    wait_for_file(&breaker_open, "initial breaker did not open");
    std::fs::remove_file(&breaker_open).unwrap();

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "autoInstall": false } }),
    );
    wait_for_file(
        &breaker_open,
        "failed half-open probe did not reopen breaker",
    );

    for version in 2..=5 {
        client.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": format!("fn half_open() {{ let value = {version}; }}\n") }]
            }),
        );
    }
    assert_host_tier_available(&mut client, "kakehashi.test.host-tier-after-half-open");

    let stderr = shutdown_and_stderr(client);
    assert_eq!(
        stderr
            .matches("disabled shadow tree tier after restart budget exhaustion")
            .count(),
        1,
        "{stderr}"
    );
    assert_eq!(stderr.matches("full resync failed").count(), 3, "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
}

#[test]
fn successful_half_open_resync_closes_the_breaker() {
    let directory = tempfile::tempdir().unwrap();
    let failure_gate = directory.path().join("restart-while-present");
    let breaker_open = directory.path().join("breaker-open");
    let breaker_closed = directory.path().join("breaker-closed");
    std::fs::write(&failure_gate, b"fail").unwrap();
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_WHILE_FILE",
            failure_gate.to_string_lossy().into_owned(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_WHILE_URI_SUFFIX",
            "/half-open-success.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_BREAKER_OPEN_FILE",
            breaker_open.to_string_lossy().into_owned(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_BREAKER_CLOSED_FILE",
            breaker_closed.to_string_lossy().into_owned(),
        )
        .env("KAKEHASHI_TREE_WORKER_FAST_COOLDOWN_MS", "25")
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    let uri = "file:///half-open-success.rs";
    open_rust_and_request_tokens(
        &mut client,
        uri,
        "fn half_open_success() { let value = 1; }\n",
    );
    wait_for_file(&breaker_open, "initial breaker did not open");
    std::fs::remove_file(&failure_gate).unwrap();
    std::fs::remove_file(&breaker_open).unwrap();

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "autoInstall": false } }),
    );
    wait_for_file(
        &breaker_closed,
        "healthy half-open probe did not close breaker",
    );
    assert!(!breaker_open.exists(), "successful probe reopened breaker");

    let recovered = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(recovered.get("result").is_some(), "{recovered:?}");

    let stderr = shutdown_and_stderr(client);
    assert_eq!(stderr.matches("full resync failed").count(), 2, "{stderr}");
    assert!(
        stderr.contains("entered shadow worker half-open probation"),
        "{stderr}"
    );
    assert!(stderr.contains("closed shadow worker breaker"), "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
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
            "KAKEHASHI_TREE_WORKER_CRASH_AFTER_HAZARD_ARMED_FILE",
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
        stderr.contains("committed active grammar hazard(s)"),
        "{stderr}"
    );
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

#[test]
fn protocol_failure_preserves_complete_active_hazard_set() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("invalid-release-once");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_INVALID_RELEASE_AFTER_HAZARD_ARMED_FILE",
            marker.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_INVALID_RELEASE_AFTER_HAZARD_ARMED_URI_SUFFIX",
            "/protocol.rs",
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
                "uri": "file:///protocol.rs",
                "languageId": "rust",
                "version": 1,
                "text": "fn corrupt_protocol() {}\n"
            }
        }),
    );
    wait_for_file(&marker, "invalid release injection did not run");

    let healthy_uri = "file:///protocol-survivor.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": healthy_uri,
                "languageId": "lua",
                "version": 1,
                "text": "local protocol_survivor = true\n"
            }
        }),
    );
    let healthy = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": healthy_uri } }),
    );
    assert!(healthy.get("result").is_some(), "{healthy:?}");

    let stderr = shutdown_and_stderr(client);
    assert!(
        stderr.contains("invalid grammar hazard release"),
        "{stderr}"
    );
    assert!(
        stderr.contains("committed active grammar hazard(s)"),
        "{stderr}"
    );
    assert!(stderr.contains("symbol=rust"), "{stderr}");
    assert!(stderr.contains("class=Systemic"), "{stderr}");
    assert_eq!(
        stderr.matches("restarted worker generation").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("full-resynced 1 open documents"),
        "{stderr}"
    );
    assert!(!stderr.contains("disabled shadow tree tier"), "{stderr}");
}

#[test]
fn windows_and_unix_crash_quarantine_every_unique_committed_grammar_hazard() {
    let directory = tempfile::tempdir().unwrap();
    let trigger = directory.path().join("trigger");
    let rust_committed = directory.path().join("rust-committed");
    let lua_committed = directory.path().join("lua-committed");
    let recovery_ready = directory.path().join("recovery-ready");
    let post_recovery_hazards = directory.path().join("post-recovery-hazards");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_MULTI_HAZARD_TRIGGER_FILE",
            trigger.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HOLD_COMMITTED_HAZARD_URI_SUFFIX",
            "/active-rust.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HOLD_COMMITTED_HAZARD_MARKER",
            rust_committed.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_CRASH_COMMITTED_HAZARD_URI_SUFFIX",
            "/active-lua.lua",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_CRASH_COMMITTED_HAZARD_MARKER",
            lua_committed.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RECOVERY_READY_FILE",
            recovery_ready.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_POST_RECOVERY_HAZARD_LOG",
            post_recovery_hazards.to_string_lossy(),
        )
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);

    let documents = [
        ("file:///active-rust.rs", "rust", "fn active_rust() {}\n"),
        ("file:///active-lua.lua", "lua", "local active_lua = true\n"),
        ("file:///healthy.yaml", "yaml", "healthy: true\n"),
    ];
    for (uri, language_id, text) in documents {
        client.send_notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        );
        let response = client.send_request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri } }),
        );
        assert!(response.get("result").is_some(), "{response:?}");
    }

    std::fs::write(&trigger, b"trigger").unwrap();
    let rust_request = client.send_request_async(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": "file:///active-rust.rs" },
            "position": { "line": 0, "character": 4 }
        }),
    );
    wait_for_file(
        &rust_committed,
        "Rust hazard was not committed before the second request",
    );
    let lua_request = client.send_request_async(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": "file:///active-lua.lua" },
            "position": { "line": 0, "character": 6 }
        }),
    );
    wait_for_file(&lua_committed, "Lua hazard did not commit before abort");

    let mut completed = std::collections::HashSet::new();
    for _ in 0..2 {
        let response = client.receive_next_response_public();
        completed.insert(response.get("id").and_then(|id| id.as_i64()).unwrap());
    }
    assert_eq!(
        completed,
        std::collections::HashSet::from([rust_request, lua_request])
    );
    wait_for_file(
        &recovery_ready,
        "replacement worker did not complete quarantine-aware resync",
    );

    for (uri, text) in [
        ("file:///active-rust.rs", "fn active_rust_v2() {}\n"),
        ("file:///active-lua.lua", "local active_lua_v2 = true\n"),
        ("file:///healthy.yaml", "healthy: still\n"),
    ] {
        client.send_notification(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{ "text": text }]
            }),
        );
    }
    wait_for_file(
        &post_recovery_hazards,
        "healthy YAML did not cross the post-recovery actor barrier",
    );
    let post_recovery_hazards = std::fs::read_to_string(&post_recovery_hazards).unwrap();
    assert!(
        post_recovery_hazards
            .lines()
            .all(|grammar| grammar == "yaml"),
        "quarantined grammar was rearmed in the same session: {post_recovery_hazards}"
    );

    let healthy = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": "file:///healthy.yaml" },
            "position": { "line": 0, "character": 1 }
        }),
    );
    assert!(healthy.get("result").is_some(), "{healthy:?}");

    let stderr = shutdown_and_stderr(client);
    let quarantine = stderr
        .lines()
        .filter(|line| line.contains("quarantined grammar conservatively for this session"))
        .collect::<Vec<_>>();
    assert_eq!(quarantine.len(), 2, "{stderr}");
    assert!(
        quarantine.iter().any(|line| line.contains("symbol=rust")),
        "{stderr}"
    );
    assert!(
        quarantine.iter().any(|line| line.contains("symbol=lua")),
        "{stderr}"
    );
    assert!(
        !quarantine.iter().any(|line| line.contains("symbol=yaml")),
        "{stderr}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("restarted worker generation"))
            .count(),
        1,
        "{stderr}",
    );
    assert!(
        stderr.contains("full-resynced 1 open documents"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "worker loss quarantine summary active_leases=2 unique_grammars=2 newly_quarantined=2 already_quarantined=0 class=NativeEvidenced"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("disabled shadow tree tier"), "{stderr}");
    eprintln!(
        "multi-hazard recovery measurement: {}",
        stderr
            .lines()
            .find(|line| line.contains("restarted worker generation"))
            .expect("restart measurement log must be present")
    );

    let mut next_session = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .build();
    initialize(&mut next_session);
    open_rust_and_request_tokens(
        &mut next_session,
        "file:///next-session.rs",
        "fn next_session() {}\n",
    );
    let rust = next_session.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": "file:///next-session.rs" },
            "position": { "line": 0, "character": 4 }
        }),
    );
    assert!(
        rust.get("result").is_some_and(|result| !result.is_null()),
        "next session did not parse Rust: {rust:?}"
    );
    next_session.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///next-session.lua",
                "languageId": "lua",
                "version": 1,
                "text": "local next_session = true\n"
            }
        }),
    );
    let lua = next_session.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": "file:///next-session.lua" },
            "position": { "line": 0, "character": 6 }
        }),
    );
    assert!(
        lua.get("result").is_some_and(|result| !result.is_null()),
        "next session did not parse Lua: {lua:?}"
    );
    let _stderr = shutdown_and_stderr(next_session);
}

#[test]
fn request_timeout_without_native_segment_does_not_quarantine_grammar() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("hang-once");
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_SHADOW", "true")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env("KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_MS", "250")
        .env(
            "KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_URI_SUFFIX",
            "/hung.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_AFTER_HAZARD_ARMED_FILE",
            marker.to_string_lossy().into_owned(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_AFTER_HAZARD_ARMED_URI_SUFFIX",
            "/hung.rs",
        )
        .env(
            "RUST_LOG",
            "kakehashi::tree_worker_shadow=info,kakehashi::tree_worker_shadow_metrics=info",
        )
        .build();
    initialize(&mut client);
    let healthy_uri = "file:///survives-hang.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": healthy_uri,
                "languageId": "lua",
                "version": 1,
                "text": "local recovered = true\n"
            }
        }),
    );
    let initial = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": healthy_uri } }),
    );
    assert!(initial.get("result").is_some(), "{initial:?}");

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": "file:///hung.rs",
                "languageId": "rust",
                "version": 1,
                "text": "fn hangs_forever() {}\n"
            }
        }),
    );
    std::thread::sleep(Duration::from_secs(2));

    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": healthy_uri } }),
    );
    assert!(response.get("result").is_some(), "{response:?}");
    std::thread::sleep(Duration::from_secs(1));

    let stderr = shutdown_and_stderr(client);
    assert!(marker.exists(), "failure injection did not run: {stderr}");
    assert!(stderr.contains("timed out"), "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    assert!(stderr.contains("restarted worker generation"), "{stderr}");
    assert!(
        stderr.contains("full-resynced 2 open documents"),
        "{stderr}"
    );
    assert_eq!(shadow_metric(&stderr, "matched"), 2, "{stderr}");
    assert!(stderr.contains("pending=0"), "{stderr}");
    assert!(!stderr.contains("disabled shadow tree tier"), "{stderr}");
}

#[test]
fn planned_restart_retires_a_noncooperative_published_request() {
    let directory = tempfile::tempdir().unwrap();
    let hang_marker = directory.path().join("hung-request");
    let recovery_ready = directory.path().join("recovery-ready");
    std::fs::write(&hang_marker, b"suppress initial hang").unwrap();
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "2")
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_AFTER_HAZARD_ARMED_FILE",
            hang_marker.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_AFTER_HAZARD_ARMED_URI_SUFFIX",
            "/planned-restart.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RECOVERY_READY_FILE",
            recovery_ready.to_string_lossy(),
        )
        .env("RUST_LOG", "kakehashi::tree_worker_shadow=info")
        .build();
    initialize(&mut client);

    let uri = "file:///planned-restart.rs";
    open_rust_and_request_tokens(&mut client, uri, "fn planned_restart() {}\n");
    std::fs::remove_file(&hang_marker).unwrap();
    let hung_request = client.send_request_async(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 4 }
        }),
    );
    wait_for_file(
        &hang_marker,
        "published request did not enter noncooperative native work",
    );

    let started = std::time::Instant::now();
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "autoInstall": false,
                "searchPaths": ["/definitely/missing-kakehashi-parsers"]
            }
        }),
    );
    let recovery_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !recovery_ready.exists() && std::time::Instant::now() < recovery_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !recovery_ready.exists() {
        client.force_kill_and_wait();
        let stderr = client.drain_stderr();
        panic!("planned restart did not replace the noncooperative generation: {stderr}");
    }
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "request did not outlive the cooperative quiescence window"
    );
    let hung_response = client.receive_response_for_id_public(hung_request);
    assert!(hung_response.get("result").is_some(), "{hung_response:?}");

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "autoInstall": false,
                "searchPaths": ["${KAKEHASHI_DATA_DIR}"]
            }
        }),
    );

    let healthy_uri = "file:///after-planned-restart.lua";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": healthy_uri,
                "languageId": "lua",
                "version": 1,
                "text": "local after_planned_restart = true\n"
            }
        }),
    );
    let healthy = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": healthy_uri },
            "position": { "line": 0, "character": 6 }
        }),
    );
    assert!(healthy.get("result").is_some(), "{healthy:?}");

    let stderr = shutdown_and_stderr(client);
    assert_eq!(
        stderr.matches("restarted worker generation").count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    assert!(!stderr.contains("disabled tree tier"), "{stderr}");
}

#[test]
fn deadline_grace_preserves_a_delayed_worker_restart_requirement() {
    let directory = tempfile::tempdir().unwrap();
    let restart_once = directory.path().join("restart-once");
    let recovery_ready = directory.path().join("recovery-ready");
    std::fs::write(&restart_once, b"suppress initial restart").unwrap();
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "2")
        .env("KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_MS", "500")
        .env(
            "KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_URI_SUFFIX",
            "/grace-restart.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_ONCE_FILE",
            restart_once.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RESTART_URI_SUFFIX",
            "/grace-restart.rs",
        )
        .env("KAKEHASHI_TREE_WORKER_RESTART_RESPONSE_DELAY_MS", "600")
        .env(
            "KAKEHASHI_TREE_WORKER_RECOVERY_READY_FILE",
            recovery_ready.to_string_lossy(),
        )
        .env("RUST_LOG", "kakehashi::tree_worker_shadow=info")
        .build();
    initialize(&mut client);

    let uri = "file:///grace-restart.rs";
    open_rust_and_request_tokens(&mut client, uri, "fn grace_restart() {}\n");
    std::fs::remove_file(&restart_once).unwrap();
    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({ "settings": { "autoInstall": false } }),
    );
    wait_for_file(
        &recovery_ready,
        "restart requirement returned during cancellation grace was discarded",
    );

    let healthy = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 4 }
        }),
    );
    assert!(healthy.get("result").is_some(), "{healthy:?}");

    let stderr = shutdown_and_stderr(client);
    assert!(
        stderr.contains("requested restart: injected systemic restart"),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("restarted worker generation").count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains("timed out"), "{stderr}");
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
}

#[test]
fn noncooperative_jobs_saturating_all_compute_threads_recover_as_one_generation() {
    let directory = tempfile::tempdir().unwrap();
    let trigger = directory.path().join("trigger");
    let markers = directory.path().join("hung-jobs");
    let recovery_ready = directory.path().join("recovery-ready");
    std::fs::create_dir(&markers).unwrap();
    let mut client = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env("KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_MS", "2000")
        .env(
            "KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_TRIGGER_FILE",
            trigger.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_ALL_COMMITTED_HAZARDS_URI_PREFIX",
            "file:///saturated-",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_ALL_COMMITTED_HAZARDS_TRIGGER_FILE",
            trigger.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_HANG_ALL_COMMITTED_HAZARDS_MARKER_DIR",
            markers.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RECOVERY_READY_FILE",
            recovery_ready.to_string_lossy(),
        )
        .env("RUST_LOG", "kakehashi::tree_worker_shadow=info")
        .build();
    initialize(&mut client);

    let documents = (0..4)
        .map(|index| {
            (
                format!("file:///saturated-{index}.rs"),
                format!("fn saturated_{index}() {{}}\n"),
            )
        })
        .collect::<Vec<_>>();
    for (uri, text) in &documents {
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
        let response = client.send_request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": uri } }),
        );
        assert!(response.get("result").is_some(), "{response:?}");
    }

    std::fs::write(&trigger, b"trigger").unwrap();
    let started = std::time::Instant::now();
    let request_ids = documents
        .iter()
        .map(|(uri, _)| {
            client.send_request_async(
                "kakehashi/node",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": 4 }
                }),
            )
        })
        .collect::<std::collections::HashSet<_>>();
    let marker_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::fs::read_dir(&markers).unwrap().count() < 4
        && std::time::Instant::now() < marker_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_dir(&markers).unwrap().count(),
        4,
        "all compute threads must enter noncooperative jobs"
    );

    let mut completed = std::collections::HashSet::new();
    for _ in 0..4 {
        let response = client.receive_next_response_public();
        assert!(response.get("result").is_some(), "{response:?}");
        completed.insert(response.get("id").and_then(|id| id.as_i64()).unwrap());
    }
    assert_eq!(completed, request_ids);
    wait_for_file(
        &recovery_ready,
        "saturated worker did not recover as a replacement generation",
    );
    let recovery_elapsed = started.elapsed();
    eprintln!(
        "saturated_noncooperative_recovery_ms={}",
        recovery_elapsed.as_millis()
    );

    let healthy = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": &documents[0].0 } }),
    );
    assert!(healthy.get("result").is_some(), "{healthy:?}");

    let stderr = shutdown_and_stderr(client);
    assert!(
        recovery_elapsed < Duration::from_secs(6),
        "{recovery_elapsed:?}\n{stderr}"
    );
    assert_eq!(
        stderr.matches("restarted worker generation").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("full-resynced 4 open documents"),
        "{stderr}"
    );
    assert!(!stderr.contains("quarantined grammar"), "{stderr}");
    assert!(!stderr.contains("disabled tree tier"), "{stderr}");
}

#[test]
fn competing_document_finishes_while_injection_fanout_is_running() {
    let force_nested = std::env::var_os("KAKEHASHI_E2E_FORCE_NESTED_PARALLELISM").is_some();
    let mut builder = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_THREADS", "4")
        .env(
            "KAKEHASHI_TREE_WORKER_ADMISSION_DELAY_URI_SUFFIX",
            "/fair-a.md",
        )
        .env("KAKEHASHI_TREE_WORKER_ADMISSION_DELAY_MS", "100")
        .env(
            "KAKEHASHI_TREE_WORKER_INJECTION_DELAY_URI_SUFFIX",
            "/fair-a.md",
        )
        .env("KAKEHASHI_TREE_WORKER_INJECTION_DELAY_MS", "20");
    if force_nested {
        builder = builder.env("KAKEHASHI_TREE_WORKER_FORCE_NESTED_PARALLELISM", "1");
    }
    let mut client = builder.build();
    initialize(&mut client);
    let heavy_uri = "file:///fair-a.md";
    let heavy = (0..60)
        .map(|index| format!("```rust\nfn injected_{index}() {{}}\n```\n"))
        .collect::<String>();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": heavy_uri,
                "languageId": "markdown",
                "version": 1,
                "text": heavy
            }
        }),
    );
    let foreground_uri = "file:///fair-b.rs";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": foreground_uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn foreground() {}\n"
            }
        }),
    );
    std::thread::sleep(Duration::from_secs(1));

    let heavy_started = std::time::Instant::now();
    let heavy_request = client.send_request_async(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": heavy_uri } }),
    );
    let started = std::time::Instant::now();
    let foreground_request = client.send_request_async(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": foreground_uri },
            "position": { "line": 0, "character": 4 }
        }),
    );
    let mut foreground_elapsed = None;
    let mut heavy_elapsed = None;
    for _ in 0..2 {
        let response = client.receive_next_response_public();
        assert!(response.get("result").is_some(), "{response:?}");
        match response.get("id").and_then(|id| id.as_i64()) {
            Some(id) if id == foreground_request => foreground_elapsed = Some(started.elapsed()),
            Some(id) if id == heavy_request => heavy_elapsed = Some(heavy_started.elapsed()),
            _ => panic!("unexpected response while measuring fairness: {response:?}"),
        }
    }
    let foreground_elapsed = foreground_elapsed.expect("foreground response must arrive");
    let heavy_elapsed = heavy_elapsed.expect("heavy response must arrive");
    if !force_nested {
        assert!(
            heavy_elapsed > foreground_elapsed,
            "heavy fanout did not yield completion order to the foreground document: heavy={heavy_elapsed:?} foreground={foreground_elapsed:?}"
        );
    }

    let _stderr = shutdown_and_stderr(client);
    eprintln!(
        "cross-document force_nested={force_nested} foreground_latency={foreground_elapsed:?} heavy_completion={heavy_elapsed:?}"
    );
}

#[test]
fn late_competing_document_reduces_fanout_at_a_chunk_boundary() {
    run_late_competitor_scenario(4);
}

#[test]
fn one_thread_worker_yields_background_fanout_to_foreground_work() {
    run_late_competitor_scenario(1);
}

fn run_late_competitor_scenario(worker_threads: usize) {
    let force_nested = std::env::var_os("KAKEHASHI_E2E_FORCE_NESTED_PARALLELISM").is_some();
    let marker_dir = tempfile::tempdir().expect("create fanout marker directory");
    let fanout_started = marker_dir.path().join("started");
    let fanout_release = marker_dir.path().join("release");
    let fanout_allowed = marker_dir.path().join("allowed");
    let fanout_yielded = marker_dir.path().join("yielded");
    let competitor_runnable = marker_dir.path().join("competitor-runnable");
    let competitor_scheduled = marker_dir.path().join("competitor-scheduled");
    let mut builder = LspClient::builder()
        .env("KAKEHASHI_TREE_WORKER_MODE", "authoritative")
        .env("KAKEHASHI_TREE_WORKER_THREADS", worker_threads.to_string())
        .env(
            "KAKEHASHI_TREE_WORKER_INJECTION_DELAY_URI_SUFFIX",
            "/late-fair-a.md",
        )
        .env("KAKEHASHI_TREE_WORKER_INJECTION_DELAY_MS", "500")
        .env(
            "KAKEHASHI_TREE_WORKER_ADMISSION_DELAY_URI_SUFFIX",
            "/late-fair-b.rs",
        )
        .env("KAKEHASHI_TREE_WORKER_ADMISSION_DELAY_MS", "200")
        .env(
            "KAKEHASHI_TREE_WORKER_RUNNABLE_ENTERED_URI_SUFFIX",
            "/late-fair-b.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_RUNNABLE_ENTERED_FILE",
            competitor_runnable.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_SCHEDULED_URI_SUFFIX",
            "/late-fair-b.rs",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_SCHEDULED_FILE",
            competitor_scheduled.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_FAIRNESS_TRACE_URI_SUFFIX",
            "/late-fair-a.md",
        )
        .env(
            "KAKEHASHI_TREE_WORKER_FANOUT_STARTED_FILE",
            fanout_started.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_FANOUT_RELEASE_FILE",
            fanout_release.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_FAIRNESS_YIELDED_FILE",
            fanout_yielded.to_string_lossy(),
        )
        .env(
            "KAKEHASHI_TREE_WORKER_FAIRNESS_ALLOWED_FILE",
            fanout_allowed.to_string_lossy(),
        );
    if force_nested {
        builder = builder.env("KAKEHASHI_TREE_WORKER_FORCE_NESTED_PARALLELISM", "1");
    } else {
        builder = builder
            .env(
                "KAKEHASHI_TREE_WORKER_SCHEDULE_RELEASE_URI_SUFFIX",
                "/late-fair-b.rs",
            )
            .env(
                "KAKEHASHI_TREE_WORKER_SCHEDULE_RELEASE_FILE",
                fanout_yielded.to_string_lossy(),
            );
    }
    let mut client = builder.build();
    initialize(&mut client);
    let heavy_uri = "file:///late-fair-a.md";
    let heavy = (0..8)
        .map(|index| format!("```rust\nfn late_injected_{index}() {{}}\n```\n"))
        .collect::<String>();
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": heavy_uri,
                "languageId": "markdown",
                "version": 1,
                "text": heavy
            }
        }),
    );
    let foreground_uri = "file:///late-fair-b.rs";
    let heavy_started = std::time::Instant::now();
    let heavy_request = client.send_request_async(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": heavy_uri } }),
    );
    let marker_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while (!fanout_started.exists() || !fanout_allowed.exists())
        && std::time::Instant::now() < marker_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(fanout_started.exists(), "heavy fanout did not start");
    assert!(
        fanout_allowed.exists(),
        "heavy fanout never observed idle capacity"
    );
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": foreground_uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn late_foreground() {}\n"
            }
        }),
    );
    let foreground_started = std::time::Instant::now();
    let foreground_request = client.send_request_async(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": foreground_uri },
            "position": { "line": 0, "character": 4 }
        }),
    );
    let competitor_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !competitor_runnable.exists() && std::time::Instant::now() < competitor_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        competitor_runnable.exists(),
        "competing worker job was not registered as runnable"
    );
    if force_nested {
        while !competitor_scheduled.exists() && std::time::Instant::now() < competitor_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            competitor_scheduled.exists(),
            "competing worker job was not scheduled"
        );
    }
    std::fs::write(&fanout_release, b"release").expect("release the first fanout chunk");
    if !force_nested {
        while !competitor_scheduled.exists() && std::time::Instant::now() < competitor_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            competitor_scheduled.exists(),
            "competing worker job was not scheduled after the worker observed its priority"
        );
    }

    let mut foreground_elapsed = None;
    let mut heavy_elapsed = None;
    for _ in 0..2 {
        let response = client.receive_next_response_public();
        assert!(response.get("result").is_some(), "{response:?}");
        match response.get("id").and_then(|id| id.as_i64()) {
            Some(id) if id == foreground_request => {
                foreground_elapsed = Some(foreground_started.elapsed())
            }
            Some(id) if id == heavy_request => heavy_elapsed = Some(heavy_started.elapsed()),
            _ => panic!("unexpected response while measuring late fairness: {response:?}"),
        }
    }
    let foreground_elapsed = foreground_elapsed.expect("foreground response must arrive");
    let heavy_elapsed = heavy_elapsed.expect("heavy response must arrive");
    let _stderr = shutdown_and_stderr(client);
    eprintln!(
        "late cross-document force_nested={force_nested} foreground_latency={foreground_elapsed:?} heavy_completion={heavy_elapsed:?} yielded={}",
        fanout_yielded.exists(),
    );
    if !force_nested {
        assert!(
            fanout_yielded.exists(),
            "late competitor did not change a later chunk's admission"
        );
        assert!(
            foreground_elapsed < heavy_elapsed,
            "foreground work did not complete before background fanout: foreground={foreground_elapsed:?} heavy={heavy_elapsed:?}"
        );
    }
}
