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
//! default `workspaceMarkers = [".git"]` resolves each host document to a distinct
//! root.

use crate::helpers::lsp_client::LspClient;
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
    init_client_with_folders(prefer_shared, mock_mode, Value::Null, None, None)
}

/// [`init_client_mode`] with the client's `workspaceFolders` at `initialize`,
/// optionally the mock's cross-process wire log (`MOCK_LSP_WIRE_LOG`), and
/// optionally the server's `workspaceMarkers` (default: the `.git` default).
fn init_client_with_folders(
    prefer_shared: bool,
    mock_mode: &str,
    workspace_folders: Value,
    wire_log: Option<&std::path::Path>,
    workspace_markers: Option<Value>,
) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config tempdir");
    let config_path = config_dir.path().join("shared.toml");
    // Host-bridge the markdown document itself onto the downstream server.
    std::fs::write(
        &config_path,
        "[languages.markdown.bridge._self]\nenabled = true\n",
    )
    .expect("write config");

    let mut builder = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"));
    if let Some(path) = wire_log {
        builder = builder.env("MOCK_LSP_WIRE_LOG", path.to_string_lossy());
    }
    let mut client = builder.build();

    let mut server = json!({
        "cmd": [mock_bin(), mock_mode],
        "languages": ["markdown"],
        "preferSharedInstance": prefer_shared
    });
    if let Some(markers) = workspace_markers {
        server["workspaceMarkers"] = markers;
    }
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": workspace_folders,
            "initializationOptions": {
                "languageServers": { "mock-ws": server }
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

/// The process id a `workspace-folders-dynamic` hover reports.
fn hover_pid(folders: &str) -> &str {
    folders
        .split_once(";pid:")
        .map(|(_, pid)| pid)
        .unwrap_or_default()
}

/// A server that declares folder-change support only through a dynamic
/// `client/registerCapability` (Pyright-style) is as capable as one that
/// declares it statically (#968): opting in keeps ONE process that learns root
/// B through `didChangeWorkspaceFolders`, rather than diverting B to a per-root
/// process that never hears of A.
#[test]
fn e2e_opt_in_shares_one_process_with_a_dynamically_registering_server() {
    let roots = two_roots();
    let (mut client, _cfg) = init_client_mode(true, "workspace-folders-dynamic");

    open(&mut client, &roots.doc_a, "# A\n");
    // The mock registers on `initialized`, before it can answer this hover, and
    // the bridge records a registration before routing any later response — so
    // once A answers, the shared connection is known capable.
    let root_a = roots.root_a.clone();
    let folders_a = poll_hover(&mut client, &roots.doc_a, |f| f.contains(&root_a));
    assert!(
        folders_a.contains(&roots.root_a),
        "first root must be known to the shared process; got {folders_a:?}"
    );

    open(&mut client, &roots.doc_b, "# B\n");
    let root_b = roots.root_b.clone();
    let folders_b = poll_hover(&mut client, &roots.doc_b, |f| f.contains(&root_b));
    assert!(
        folders_b.contains(&roots.root_a) && folders_b.contains(&roots.root_b),
        "a dynamically registered server must serve both roots; got {folders_b:?}"
    );
    assert_eq!(
        hover_pid(&folders_b),
        hover_pid(&folders_a),
        "root B must join the shared process, not a diverted one"
    );
}

/// A client-root fallback whose server registered folder-change support
/// dynamically takes an upstream `workspace/didChangeWorkspaceFolders` as a
/// notification (#968). Recycling it instead would ALSO leave the new folder
/// known — the replacement initializes with the current snapshot — so only the
/// unchanged process id tells forwarding apart from a restart.
#[test]
fn e2e_client_folder_change_is_forwarded_to_a_dynamically_registering_server() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    // Marker search is switched off below, so the document resolves to the
    // client-root fallback even when the temp dir sits inside a checkout.
    let dir_a = tmp.path().join("a");
    let dir_b = tmp.path().join("b");
    std::fs::create_dir_all(&dir_a).expect("mkdir a");
    std::fs::create_dir_all(&dir_b).expect("mkdir b");
    let doc_path = dir_a.join("doc.md");
    std::fs::write(&doc_path, "# A\n").expect("write doc");
    let to_uri = |p: &std::path::Path| url::Url::from_file_path(p).unwrap().to_string();
    let (root_a, root_b, doc) = (to_uri(&dir_a), to_uri(&dir_b), to_uri(&doc_path));

    let (mut client, _cfg) = init_client_with_folders(
        false,
        "workspace-folders-dynamic",
        json!([{ "uri": root_a, "name": "a" }]),
        None,
        Some(json!([])),
    );
    open(&mut client, &doc, "# A\n");
    let before = poll_hover(&mut client, &doc, |f| f.contains(&root_a));
    assert!(
        before.contains(&root_a),
        "the fallback must start with the client folder; got {before:?}"
    );

    client.send_notification(
        "workspace/didChangeWorkspaceFolders",
        json!({ "event": { "added": [{ "uri": root_b, "name": "b" }], "removed": [] } }),
    );
    let after = poll_hover(&mut client, &doc, |f| f.contains(&root_b));
    assert!(
        after.contains(&root_b),
        "the added folder must reach the server; got {after:?}"
    );
    assert_eq!(
        hover_pid(&after),
        hover_pid(&before),
        "the folder change must be forwarded, not answered with a restart"
    );
}

/// A root diverted to its own process while the shared instance had not yet
/// registered folder-change support is consolidated once it does (#968): the
/// diverted process is shut down rather than left serving a root the shared
/// instance now takes. Routing alone would already send root B's NEXT request
/// to the shared instance (the capability is read live), so the discriminating
/// observation is the diverted process's `shutdown`.
#[test]
fn e2e_late_registration_consolidates_diverted_roots() {
    let roots = two_roots();
    let log_dir = tempfile::TempDir::new().expect("wire log dir");
    let wire_log = log_dir.path().join("wire.log");
    let (mut client, _cfg) = init_client_with_folders(
        true,
        "workspace-folders-dynamic-late",
        Value::Null,
        Some(&wire_log),
        None,
    );

    // Bring the shared instance up for root A WITHOUT asking it anything, so
    // it stays unregistered (the mock registers on its first hover). Wait for
    // A's didOpen before opening B: the two eager opens race, and whichever
    // lands first spawns the shared instance under its root.
    open(&mut client, &roots.doc_a, "# A\n");
    let opened_a = format!("textDocument/didOpen\t{}", roots.doc_a);
    let mut a_is_open = false;
    for _ in 0..200 {
        a_is_open = std::fs::read_to_string(&wire_log)
            .unwrap_or_default()
            .lines()
            .any(|line| line == opened_a);
        if a_is_open {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(a_is_open, "the shared instance must come up under root A");
    open(&mut client, &roots.doc_b, "# B\n");
    // Root B lands on a diverted per-root process: the shared one is Ready
    // and still incapable.
    let root_b = roots.root_b.clone();
    let diverted = poll_hover(&mut client, &roots.doc_b, |f| f.contains(&root_b));
    assert!(
        diverted.contains(&roots.root_b) && !diverted.contains(&roots.root_a),
        "before registration root B must be served by its own process; got {diverted:?}"
    );
    let shutdowns = |log: &std::path::Path| {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("shutdown\t"))
            .count()
    };
    assert_eq!(shutdowns(&wire_log), 0, "nothing has been retired yet");

    // The shared instance's first hover makes it register.
    let root_a = roots.root_a.clone();
    let shared = poll_hover(&mut client, &roots.doc_a, |f| f.contains(&root_a));
    assert!(shared.contains(&roots.root_a), "got {shared:?}");

    let mut retired = 0;
    for _ in 0..200 {
        retired = shutdowns(&wire_log);
        if retired > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        retired > 0,
        "the diverted root's process must be retired once the shared instance registers"
    );
    // Root B's host document moves to the shared instance WITHOUT anything
    // touching it: a didOpen of it logged after the retired process's
    // `shutdown` can only come from the shared one.
    let opened_b = format!("textDocument/didOpen\t{}", roots.doc_b);
    let mut reopened = false;
    for _ in 0..200 {
        let log = std::fs::read_to_string(&wire_log).unwrap_or_default();
        reopened = log
            .split_once("shutdown\t")
            .is_some_and(|(_, after)| after.lines().any(|line| line == opened_b));
        if reopened {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        reopened,
        "an untouched document of a retired root must be re-opened on the shared instance"
    );

    let consolidated = poll_hover(&mut client, &roots.doc_b, |f| f.contains(&roots.root_a));
    assert!(
        consolidated.contains(&roots.root_a) && consolidated.contains(&roots.root_b),
        "root B must now be served by the shared instance; got {consolidated:?}"
    );
    assert_eq!(hover_pid(&consolidated), hover_pid(&shared));
    assert_eq!(
        shutdowns(&wire_log),
        1,
        "only the diverted process is retired; the shared one keeps serving"
    );
}
