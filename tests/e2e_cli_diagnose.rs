//! E2E tests for the `kakehashi diagnose` CLI subcommand.
//!
//! Spawns the real `kakehashi` binary with a workspace-local `kakehashi.toml`
//! that bridges lua injections to the `mock-lsp-formatter` test binary in
//! `diagnostics` mode (answers `textDocument/diagnostic` with one severity-2
//! warning whose message echoes the virtual URI). Asserts on the `default` and
//! `jsonl` output formats, the `--threshold` exit-code gating, the
//! stdout/stderr split, stdin mode, and the broken-server error path.

#![cfg(feature = "e2e")]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

/// Shared persistent data dir with markdown/lua parsers preinstalled (same one
/// the rest of the e2e suite uses).
fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir = kakehashi::install::test_support::test_data_dir_path();
        std::fs::create_dir_all(&dir).expect("create shared test data dir");
        kakehashi::install::test_support::ensure_test_languages_installed(&dir)
            .expect("install test parsers into the shared data dir");
        dir
    });
    let _ = std::fs::remove_file(dir.join("parsing_in_progress"));
    let _ = std::fs::remove_file(dir.join("failed_parsers"));
    dir.as_path()
}

/// Workspace config bridging lua injections to the mock diagnostics server.
fn config_toml() -> String {
    format!(
        r#"autoInstall = false

[languageServers.mock-diag]
cmd = ['{}', 'diagnostics']
languages = ["lua"]
"#,
        env!("CARGO_BIN_EXE_mock-lsp-formatter")
    )
}

/// A markdown host with a lua injection region. The lua line `local x = 1` is
/// host line index 3 (0-based), so the mock's virtual-(0,0) diagnostic
/// transforms to 1-based host position 4:1.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
/// Markdown with no injected code, so no downstream server runs.
const PLAIN_MARKDOWN: &str = "# Title\n\nJust prose, no code.\n";

fn workspace_with(config: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create workspace tempdir");
    std::fs::write(dir.path().join("kakehashi.toml"), config).expect("write config");
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write workspace file");
    }
    dir
}

/// Run `kakehashi diagnose <args>` with the workspace as cwd. The workspace
/// `kakehashi.toml` is passed via `--config-file` so the developer's own
/// config can never leak host-layer servers into the run.
fn run_diagnose(workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .arg("diagnose")
        .arg("--config-file")
        .arg("kakehashi.toml")
        .args(args)
        .current_dir(workspace)
        .env("KAKEHASHI_DATA_DIR", data_dir())
        .env("RUST_LOG", "kakehashi=debug")
        .output()
        .expect("spawn kakehashi diagnose")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn e2e_diagnose_default_format_reports_transformed_position() {
    let ws = workspace_with(&config_toml(), &[("doc.md", MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md"]);
    // Warning < default threshold "error", so a clean exit despite a finding.
    assert_eq!(
        output.status.code(),
        Some(0),
        "a warning does not meet the default error threshold; stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    // Default format: file:line:col: severity: message, at the host-transformed
    // 1-based position.
    assert!(
        stdout.starts_with("doc.md:4:1: warning: "),
        "default line should carry the position and severity; got: {stdout:?}"
    );
    assert!(
        stdout.contains("mock-diagnostic:"),
        "the downstream diagnostic message should be present; got: {stdout:?}"
    );
}

#[test]
fn e2e_diagnose_threshold_warning_exits_one() {
    let ws = workspace_with(&config_toml(), &[("doc.md", MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md", "--threshold", "warning"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a warning meets the warning threshold and must exit 1; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_threshold_none_always_exits_zero() {
    let ws = workspace_with(&config_toml(), &[("doc.md", MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md", "--threshold", "none"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--threshold none must never exit 1 even with diagnostics; stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("mock-diagnostic:"),
        "the diagnostic is still printed under --threshold none"
    );
}

#[test]
fn e2e_diagnose_explicit_default_output_format() {
    // `--output-format default` is accepted and renders the same severity-bearing
    // line as the implicit default.
    let ws = workspace_with(&config_toml(), &[("doc.md", MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md", "--output-format", "default"]);
    assert!(
        output.status.success(),
        "default-format run should succeed; stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.starts_with("doc.md:4:1: warning: "),
        "default line should carry the severity word; got: {stdout:?}"
    );
}

#[test]
fn e2e_diagnose_jsonl_format_is_structured() {
    let ws = workspace_with(&config_toml(), &[("doc.md", MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md", "--output-format", "jsonl"]);
    assert!(
        output.status.success(),
        "jsonl run should succeed; stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    let line = stdout.lines().next().expect("at least one jsonl line");
    let value: serde_json::Value = serde_json::from_str(line).expect("each line is valid JSON");
    assert_eq!(value["file"], "doc.md");
    assert_eq!(value["line"], 4);
    assert_eq!(value["column"], 1);
    assert_eq!(value["severity"], "warning");
    assert!(
        value["message"]
            .as_str()
            .is_some_and(|m| m.contains("mock-diagnostic:")),
        "message should be the downstream diagnostic; got: {value}"
    );
}

#[test]
fn e2e_diagnose_diagnostics_on_stdout_summary_on_stderr() {
    let ws = workspace_with(&config_toml(), &[("doc.md", MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md"]);
    assert!(
        output.status.success(),
        "run should succeed; stderr: {}",
        stderr_of(&output)
    );
    // stdout is the data channel: only diagnostics, no summary.
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("mock-diagnostic:"),
        "diagnostics go to stdout; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("diagnostic in"),
        "the summary must not pollute stdout; got: {stdout:?}"
    );
    // The one-line summary always goes to stderr (no --quiet switch needed).
    assert!(
        stderr_of(&output).contains("1 diagnostic in 1 file"),
        "the summary goes to stderr; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_clean_file_exits_zero_with_no_output() {
    // No injected code -> no downstream server runs -> no diagnostics.
    let ws = workspace_with(&config_toml(), &[("doc.md", PLAIN_MARKDOWN)]);

    let output = run_diagnose(ws.path(), &["doc.md"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a clean file exits 0; stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).is_empty(),
        "a clean file prints no diagnostics; got: {:?}",
        stdout_of(&output)
    );
}

#[test]
fn e2e_diagnose_stdin_mode_prints_diagnostics() {
    let ws = workspace_with(&config_toml(), &[]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "diagnose",
            "--config-file",
            "kakehashi.toml",
            "--stdin-filename",
            "doc.md",
            "--threshold",
            "warning",
        ])
        .current_dir(ws.path())
        .env("KAKEHASHI_DATA_DIR", data_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kakehashi diagnose (stdin mode)");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(MARKDOWN.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for kakehashi");

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdin warning meets the warning threshold; stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.starts_with("doc.md:4:1:"),
        "stdin diagnostics carry the stdin filename and transformed position; got: {stdout:?}"
    );
}

#[test]
fn e2e_diagnose_broken_downstream_server_exits_two() {
    // A configured-but-unstartable server must surface as exit 2, not a silent
    // "0 diagnostics" success.
    let ws = workspace_with(
        r#"autoInstall = false

[languageServers.broken]
cmd = ["/nonexistent/kakehashi-test-diagnoser"]
languages = ["lua"]
"#,
        &[("doc.md", MARKDOWN)],
    );

    let output = run_diagnose(ws.path(), &["doc.md"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a broken configured server must exit 2; stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("failed to start"),
        "stderr should name the broken server; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_broken_server_still_exits_two_under_threshold_none() {
    // Operational errors are independent of --threshold: a tool that could not
    // even start its server must not look "clean" to CI under `none`.
    let ws = workspace_with(
        r#"autoInstall = false

[languageServers.broken]
cmd = ["/nonexistent/kakehashi-test-diagnoser"]
languages = ["lua"]
"#,
        &[("doc.md", MARKDOWN)],
    );

    let output = run_diagnose(ws.path(), &["doc.md", "--threshold", "none"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--threshold none does not suppress operational errors; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_request_time_server_failure_exits_two() {
    // The server handshakes fine (advertises diagnosticProvider) but errors on
    // the diagnostic request — distinct from never starting. The fan-in
    // collapses the failure into an empty report, so without the request-error
    // sink this would look clean; it must exit 2.
    let ws = workspace_with(
        &format!(
            r#"autoInstall = false

[languageServers.mock-diag-fail]
cmd = ['{}', 'diagnostics-fail']
languages = ["lua"]
"#,
            env!("CARGO_BIN_EXE_mock-lsp-formatter")
        ),
        &[("doc.md", MARKDOWN)],
    );

    let output = run_diagnose(ws.path(), &["doc.md"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a request-time downstream failure must exit 2; stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stderr_of(&output).contains("operation(s) failed"),
        "stderr should report the failed request; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_request_time_failure_exits_two_even_under_threshold_none() {
    // Request-time failure is operational, so --threshold none must not hide it.
    let ws = workspace_with(
        &format!(
            r#"autoInstall = false

[languageServers.mock-diag-fail]
cmd = ['{}', 'diagnostics-fail']
languages = ["lua"]
"#,
            env!("CARGO_BIN_EXE_mock-lsp-formatter")
        ),
        &[("doc.md", MARKDOWN)],
    );

    let output = run_diagnose(ws.path(), &["doc.md", "--threshold", "none"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--threshold none does not suppress a request-time failure; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_host_layer_request_failure_exits_two() {
    // Same request-time failure but through the HOST layer
    // (bridge._self.enabled): the host fan-in must also count the error so the
    // CLI exits 2 rather than silently losing the host layer.
    let ws = workspace_with(
        &format!(
            r#"autoInstall = false

[languages.markdown.bridge._self]
enabled = true

[languageServers.mock-host-fail]
cmd = ['{}', 'diagnostics-fail']
languages = ["markdown"]
"#,
            env!("CARGO_BIN_EXE_mock-lsp-formatter")
        ),
        &[("doc.md", MARKDOWN)],
    );

    let output = run_diagnose(ws.path(), &["doc.md"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a host-layer request failure must exit 2; stderr: {}",
        stderr_of(&output)
    );
}

#[test]
fn e2e_diagnose_directory_walk_respects_gitignore_but_explicit_path_wins() {
    let ws = workspace_with(
        &config_toml(),
        &[
            ("kept.md", MARKDOWN),
            ("ignored.md", MARKDOWN),
            (".gitignore", "ignored.md\n"),
        ],
    );

    let output = run_diagnose(ws.path(), &[".", "--threshold", "warning"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "directory walk finds the warning; stderr: {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("kept.md:4:1:"),
        "non-ignored file is diagnosed; got: {stdout:?}"
    );
    assert!(
        !stdout.contains("ignored.md:"),
        "gitignored file is skipped by the walk; got: {stdout:?}"
    );

    // Explicitly naming the gitignored file diagnoses it anyway — naming a path
    // is a stronger signal than a .gitignore entry.
    let output = run_diagnose(ws.path(), &["ignored.md", "--threshold", "warning"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "explicit gitignored path is diagnosed; stderr: {}",
        stderr_of(&output)
    );
    assert!(
        stdout_of(&output).contains("ignored.md:4:1:"),
        "explicitly named path wins over gitignore; got: {}",
        stdout_of(&output)
    );
}
