//! E2E tests for the per-server shared-instance opt-in (#391):
//! `preferSharedInstance = true` makes one downstream process serve every
//! marker root, announcing each new root with
//! `workspace/didChangeWorkspaceFolders` instead of spawning a process per
//! root.
//!
//! The `mock-lsp-formatter` binary's `workspace-folders` mode advertises the
//! `workspace.workspaceFolders.{supported, changeNotifications}` capability,
//! records the folders it learns (at `initialize` and via
//! `didChangeWorkspaceFolders`), and answers `hover` with that folder set — so
//! a hover on a document under the *second* root reveals whether the bridge
//! kept one process (which now knows BOTH roots) or spawned a second
//! (which knows only its own).
//!
//! Two sibling projects each carry their own `.git` marker root, so the
//! default `rootMarkers = [".git"]` resolves each host document to a distinct
//! root.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// A workspace with two sibling `.git` projects and one markdown host document
/// in each. Returns the tempdir (kept alive) and the two document URIs.
struct TwoRoots {
    _tmp: tempfile::TempDir,
    doc_a: String,
    doc_b: String,
    root_a: String,
    root_b: String,
}

fn two_roots() -> TwoRoots {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let proj_a = tmp.path().join("a");
    let proj_b = tmp.path().join("b");
    std::fs::create_dir_all(proj_a.join(".git")).expect("mkdir a/.git");
    std::fs::create_dir_all(proj_b.join(".git")).expect("mkdir b/.git");
    let doc_a_path = proj_a.join("doc.md");
    let doc_b_path = proj_b.join("doc.md");
    std::fs::write(&doc_a_path, "# A\n").expect("write doc a");
    std::fs::write(&doc_b_path, "# B\n").expect("write doc b");

    let to_uri = |p: &std::path::Path| url::Url::from_file_path(p).unwrap().to_string();
    TwoRoots {
        doc_a: to_uri(&doc_a_path),
        doc_b: to_uri(&doc_b_path),
        root_a: to_uri(&proj_a),
        root_b: to_uri(&proj_b),
        _tmp: tmp,
    }
}

/// Start a kakehashi client whose only bridge server is a `workspace-folders`
/// mock (host-bridged for markdown), with `preferSharedInstance` set to
/// `prefer_shared` and the mock running in `mock_mode` (`"workspace-folders"`
/// advertises the capability; `"workspace-folders-incapable"` does not).
fn init_client_mode(prefer_shared: bool, mock_mode: &str) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config tempdir");
    let config_path = config_dir.path().join("shared.toml");
    // Host-bridge the markdown document itself onto the downstream server.
    std::fs::write(
        &config_path,
        "[languages.markdown.bridge._self]\nenabled = true\n",
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-ws": {
                        "cmd": [mock_bin(), mock_mode],
                        "languages": ["markdown"],
                        "preferSharedInstance": prefer_shared
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// Default to the capability-advertising mock.
fn init_client(prefer_shared: bool) -> (LspClient, tempfile::TempDir) {
    init_client_mode(prefer_shared, "workspace-folders")
}

fn open(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": text
            }
        }),
    );
}

/// Hover on line 0 of `uri`; returns the mock's `folders:<a,b,...>` string
/// (empty until the host bridge produces a result).
fn hover_folders(client: &mut LspClient, uri: &str) -> String {
    let response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 0, "character": 0 },
        }),
    );
    response
        .get("result")
        .and_then(|r| r.get("contents"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Poll `hover_folders(uri)` until `done` holds (the downstream warms up
/// lazily, and a new root is announced asynchronously), returning the last
/// observed value.
fn poll_hover(client: &mut LspClient, uri: &str, done: impl Fn(&str) -> bool) -> String {
    let mut last = String::new();
    for _ in 0..300 {
        last = hover_folders(client, uri);
        if done(&last) {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    last
}

/// With the opt-in, one process serves both roots: opening a document under the
/// second root announces it via didChangeWorkspaceFolders, so a hover there
/// sees BOTH roots.
#[test]
fn e2e_shared_instance_grows_folder_set_across_roots() {
    let roots = two_roots();
    let (mut client, _cfg) = init_client(true);

    open(&mut client, &roots.doc_a, "# A\n");
    open(&mut client, &roots.doc_b, "# B\n");

    // Root A is the shared process's initialize-time folder (warms up lazily).
    let root_a = roots.root_a.clone();
    let folders_a = poll_hover(&mut client, &roots.doc_a, |f| f.contains(&root_a));
    assert!(
        folders_a.contains(&roots.root_a),
        "first root must be known to the shared process; got {folders_a:?}"
    );

    // Root B joined the SAME process via didChangeWorkspaceFolders, so a hover
    // under B eventually sees both roots.
    let root_b = roots.root_b.clone();
    let folders_b = poll_hover(&mut client, &roots.doc_b, |f| f.contains(&root_b));
    assert!(
        folders_b.contains(&roots.root_a) && folders_b.contains(&roots.root_b),
        "shared instance must serve both roots after didChangeWorkspaceFolders; got {folders_b:?}"
    );
}

/// Without the opt-in (default), each root gets its own process, so the process
/// serving root B never learns about root A — the contrast that proves the
/// opt-in changed behavior.
#[test]
fn e2e_per_root_default_isolates_folder_sets() {
    let roots = two_roots();
    let (mut client, _cfg) = init_client(false);

    open(&mut client, &roots.doc_a, "# A\n");
    open(&mut client, &roots.doc_b, "# B\n");

    // Warm up both roots' (separate) processes, then assert B's process never
    // learned about A.
    let root_a = roots.root_a.clone();
    let _ = poll_hover(&mut client, &roots.doc_a, |f| f.contains(&root_a));
    let root_b = roots.root_b.clone();
    let folders_b = poll_hover(&mut client, &roots.doc_b, |f| f.contains(&root_b));
    assert!(
        folders_b.contains(&roots.root_b) && !folders_b.contains(&roots.root_a),
        "per-root default must keep root B's process unaware of root A; got {folders_b:?}"
    );
}

/// Opting in (`preferSharedInstance = true`) against a server that does NOT
/// advertise the workspaceFolders capability must degrade to per-root
/// instances (#391): root B's process never learns about root A, exactly like
/// the default — the opt-in degrades, it does not wedge the 2nd root.
#[test]
fn e2e_opt_in_falls_back_to_per_root_when_server_incapable() {
    let roots = two_roots();
    let (mut client, _cfg) = init_client_mode(true, "workspace-folders-incapable");

    open(&mut client, &roots.doc_a, "# A\n");
    open(&mut client, &roots.doc_b, "# B\n");

    let root_a = roots.root_a.clone();
    let _ = poll_hover(&mut client, &roots.doc_a, |f| f.contains(&root_a));
    let root_b = roots.root_b.clone();
    let folders_b = poll_hover(&mut client, &roots.doc_b, |f| f.contains(&root_b));
    assert!(
        folders_b.contains(&roots.root_b) && !folders_b.contains(&roots.root_a),
        "incapable opt-in must fall back to per-root isolation; got {folders_b:?}"
    );
}
