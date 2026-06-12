//! E2E tests for the `kakehashi format` CLI subcommand.
//!
//! Spawns the real `kakehashi` binary with a workspace-local `kakehashi.toml`
//! that bridges lua injections to the `mock-lsp-formatter` test binary
//! (`upper` mode: uppercases the region), then asserts on exit codes and
//! on-disk effects for the plain, `--check`, `--fail-on-change`, stdin, and
//! gitignore-walk behaviors.

#![cfg(feature = "e2e")]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

/// Shared persistent data dir with markdown/lua parsers preinstalled (same
/// one the LSP e2e suite uses; see `tests/helpers/lsp_client.rs`).
fn data_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir = kakehashi::install::test_support::test_data_dir_path();
        let _ = std::fs::create_dir_all(&dir);
        let _ = kakehashi::install::test_support::ensure_test_languages_installed(&dir);
        dir
    });
    // Clear crash-recovery state before every spawn so an earlier test's
    // mid-parse shutdown can't poison this one.
    let _ = std::fs::remove_file(dir.join("parsing_in_progress"));
    let _ = std::fs::remove_file(dir.join("failed_parsers"));
    dir.as_path()
}

/// Workspace config bridging lua injections to the uppercasing mock server.
fn config_toml() -> String {
    format!(
        r#"autoInstall = false

[languageServers.mock-upper]
cmd = ["{}", "upper"]
languages = ["lua"]
"#,
        env!("CARGO_BIN_EXE_mock-lsp-formatter")
    )
}

const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";

/// Create a workspace tempdir holding `kakehashi.toml` plus `files`.
fn workspace_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create workspace tempdir");
    std::fs::write(dir.path().join("kakehashi.toml"), config_toml()).expect("write config");
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write workspace file");
    }
    dir
}

/// Run `kakehashi format <args>` with the workspace as cwd. The workspace
/// `kakehashi.toml` is passed via `--config-file` so the developer's own
/// `~/.config/kakehashi/kakehashi.toml` can never leak into the test (it
/// would e.g. add host-layer servers like marksman to the formatting race).
fn run_format(workspace: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .arg("format")
        .arg("--config-file")
        .arg("kakehashi.toml")
        .args(args)
        .current_dir(workspace)
        .env("KAKEHASHI_DATA_DIR", data_dir())
        .env("RUST_LOG", "kakehashi=debug")
        .output()
        .expect("spawn kakehashi format")
}

fn read(workspace: &Path, name: &str) -> String {
    std::fs::read_to_string(workspace.join(name)).expect("read workspace file")
}

#[test]
fn e2e_format_rewrites_file_in_place_and_is_idempotent() {
    let ws = workspace_with(&[("doc.md", MARKDOWN)]);

    let output = run_format(ws.path(), &["doc.md"]);
    assert!(
        output.status.success(),
        "format should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let formatted = read(ws.path(), "doc.md");
    assert!(
        formatted.contains("LOCAL X = 1"),
        "lua region should be uppercased by the mock formatter; got: {formatted:?}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        formatted.contains("```lua"),
        "host markdown around the region must be preserved; got: {formatted:?}"
    );

    // Second run: the mock's output is a fixpoint, so nothing changes and
    // even --fail-on-change passes.
    let output = run_format(ws.path(), &["doc.md", "--fail-on-change"]);
    assert!(
        output.status.success(),
        "second run should be a no-op; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(read(ws.path(), "doc.md"), formatted);
}

#[test]
fn e2e_check_reports_without_writing() {
    let ws = workspace_with(&[("doc.md", MARKDOWN)]);

    let output = run_format(ws.path(), &["doc.md", "--check"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "--check must exit 1 for an unformatted file; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        read(ws.path(), "doc.md"),
        MARKDOWN,
        "--check must not modify the file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Would reformat"),
        "--check should name the file; stderr: {stderr}"
    );

    // Format for real, then --check passes.
    let output = run_format(ws.path(), &["doc.md"]);
    assert!(output.status.success());
    let output = run_format(ws.path(), &["doc.md", "--check"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--check must exit 0 once formatted; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn e2e_fail_on_change_writes_and_exits_nonzero() {
    let ws = workspace_with(&[("doc.md", MARKDOWN)]);

    let output = run_format(ws.path(), &["doc.md", "--fail-on-change"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "--fail-on-change must exit 1 when a file changed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        read(ws.path(), "doc.md").contains("LOCAL X = 1"),
        "--fail-on-change still writes the change"
    );
}

#[test]
fn e2e_stdin_mode_prints_formatted_content() {
    let ws = workspace_with(&[]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_kakehashi"))
        .args([
            "format",
            "--config-file",
            "kakehashi.toml",
            "--stdin-filename",
            "doc.md",
        ])
        .current_dir(ws.path())
        .env("KAKEHASHI_DATA_DIR", data_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kakehashi format (stdin mode)");
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(MARKDOWN.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for kakehashi");

    assert!(
        output.status.success(),
        "stdin mode should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("LOCAL X = 1"),
        "formatted content goes to stdout; got: {stdout:?}"
    );
}

#[test]
fn e2e_directory_walk_respects_gitignore_but_explicit_path_wins() {
    let ws = workspace_with(&[
        ("kept.md", MARKDOWN),
        ("ignored.md", MARKDOWN),
        (".gitignore", "ignored.md\n"),
    ]);

    let output = run_format(ws.path(), &["."]);
    assert!(
        output.status.success(),
        "directory format should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        read(ws.path(), "kept.md").contains("LOCAL X = 1"),
        "non-ignored file is formatted"
    );
    assert_eq!(
        read(ws.path(), "ignored.md"),
        MARKDOWN,
        "gitignored file is skipped by the walk"
    );

    // Explicitly naming the gitignored file formats it anyway.
    let output = run_format(ws.path(), &["ignored.md"]);
    assert!(
        output.status.success(),
        "explicit gitignored path should format; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        read(ws.path(), "ignored.md").contains("LOCAL X = 1"),
        "explicitly named path wins over gitignore"
    );
}

#[test]
fn e2e_tab_size_and_insert_spaces_reach_the_downstream_server() {
    // Workspace whose mock server echoes the FormattingOptions it received.
    let ws = workspace_with(&[("doc.md", MARKDOWN)]);
    std::fs::write(
        ws.path().join("kakehashi.toml"),
        format!(
            r#"autoInstall = false

[languageServers.mock-echo]
cmd = ["{}", "options-echo"]
languages = ["lua"]
"#,
            env!("CARGO_BIN_EXE_mock-lsp-formatter")
        ),
    )
    .expect("write options-echo config");

    // Defaults: tabSize=4, insertSpaces=true.
    let output = run_format(ws.path(), &["doc.md"]);
    assert!(
        output.status.success(),
        "format should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let formatted = read(ws.path(), "doc.md");
    assert!(
        formatted.contains("tabSize=4 insertSpaces=true"),
        "defaults should reach the downstream server; got: {formatted:?}; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Explicit flags override the defaults.
    std::fs::write(ws.path().join("doc.md"), MARKDOWN).expect("reset doc.md");
    let output = run_format(
        ws.path(),
        &["doc.md", "--tab-size", "2", "--insert-spaces", "false"],
    );
    assert!(
        output.status.success(),
        "format with options should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let formatted = read(ws.path(), "doc.md");
    assert!(
        formatted.contains("tabSize=2 insertSpaces=false"),
        "--tab-size/--insert-spaces should reach the downstream server; got: {formatted:?}"
    );
}

#[test]
fn e2e_excludes_filters_directory_walk() {
    let ws = workspace_with(&[("kept.md", MARKDOWN), ("vendor/dep.md", MARKDOWN)]);

    let output = run_format(ws.path(), &[".", "--excludes", "vendor/"]);
    assert!(
        output.status.success(),
        "directory format should succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        read(ws.path(), "kept.md").contains("LOCAL X = 1"),
        "non-excluded file is formatted"
    );
    assert_eq!(
        read(ws.path(), "vendor/dep.md"),
        MARKDOWN,
        "--excludes pattern skips matching paths"
    );
}
