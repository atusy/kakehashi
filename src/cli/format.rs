//! `kakehashi format <paths...>` — format files through the same
//! injection-region bridge pipeline the LSP server uses.
//!
//! The command runs the LSP server in-process (no JSON-RPC framing): it
//! builds the [`tower_lsp_server::LspService`] the same way `run_lsp_server`
//! does, then drives `initialize` → `didOpen` → `textDocument/formatting` →
//! `didClose` by calling the handler implementations directly. This reuses
//! config loading, language detection, injection resolution, and the
//! downstream language-server pool verbatim, so CLI formatting can never
//! drift from editor formatting.
//!
//! File selection semantics:
//! - Directories are walked recursively, **respecting `.gitignore`** (also
//!   outside git repositories) and skipping hidden files.
//! - Explicitly listed files are always formatted, even when gitignored —
//!   naming a path is a stronger signal than a `.gitignore` entry.
//! - `--excludes` patterns (gitignore syntax, relative to the current
//!   directory) filter *everything*, including explicitly listed paths.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tower_lsp_server::LspService;

use crate::lsp::Kakehashi;

/// Options for the `format` subcommand, mirroring its CLI flags.
pub struct FormatOptions {
    /// Files or directories to format. With `--stdin-filename`, must be
    /// empty or exactly `["-"]`.
    pub paths: Vec<PathBuf>,
    /// Dry-run: report files that would change, write nothing, exit 1 if
    /// any file would change.
    pub check: bool,
    /// Read content from stdin, treat it as this file path (for language
    /// detection and config resolution), and print the result to stdout.
    pub stdin_filename: Option<PathBuf>,
    /// Gitignore-style exclusion patterns, relative to the current directory.
    pub excludes: Vec<String>,
    /// Write changes, but exit 1 if any file was changed.
    pub fail_on_change: bool,
    /// `FormattingOptions.tabSize` sent to downstream servers. LSP makes the
    /// field mandatory; whether to honor it is each server's decision (most
    /// read their own config instead).
    pub tab_size: u32,
    /// `FormattingOptions.insertSpaces` sent to downstream servers; a hint,
    /// like `tab_size`.
    pub insert_spaces: bool,
}

impl FormatOptions {
    /// The LSP `FormattingOptions` every formatting request carries. In LSP
    /// mode the editor fills this from its buffer settings; in CLI mode the
    /// flags (or their defaults) stand in for them.
    fn formatting_options(&self) -> tower_lsp_server::ls_types::FormattingOptions {
        tower_lsp_server::ls_types::FormattingOptions {
            tab_size: self.tab_size,
            insert_spaces: self.insert_spaces,
            ..Default::default()
        }
    }
}

/// Exit status of the `format` run, kept as plain `u8` so the binary can map
/// it onto `std::process::ExitCode` without this module depending on it.
pub const EXIT_OK: u8 = 0;
/// At least one file changed (with `--fail-on-change`) or would change
/// (with `--check`).
pub const EXIT_CHANGED: u8 = 1;
/// Usage or I/O error.
pub const EXIT_ERROR: u8 = 2;

/// Per-server bound for waiting on cold downstream language servers. Spawning
/// and the LSP initialize handshake usually complete well under a second;
/// the generous bound covers slow first launches (e.g. an interpreter-based
/// server warming caches) without hanging an unconfigured run forever.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Entry point for `kakehashi format`. Returns the process exit code.
pub fn run(options: FormatOptions) -> u8 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {e}");
            return EXIT_ERROR;
        }
    };
    runtime.block_on(run_async(options))
}

async fn run_async(options: FormatOptions) -> u8 {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("error: cannot determine current directory: {e}");
            return EXIT_ERROR;
        }
    };

    // Same construction as LSP server mode, but the loopback client socket is
    // pumped by a stub instead of an editor, and handlers are called directly.
    let (service, socket) = LspService::new(Kakehashi::new);
    spawn_client_pump(socket);
    let server = service.inner();
    server.cli_initialize(&cwd).await;

    let code = if options.stdin_filename.is_some() {
        run_stdin(server, &cwd, &options).await
    } else {
        run_paths(server, &cwd, &options).await
    };

    // Graceful downstream shutdown even on the error paths above this point
    // is unnecessary — servers only spawn once formatting starts.
    server.cli_shutdown().await;
    code
}

/// Drain server→client traffic so `Client` calls never block: notifications
/// (logMessage etc.) are dropped — the CLI reports its own progress — and the
/// rare server→client *request* (e.g. workDoneProgress/create) is answered
/// with `null` so the awaiting handler proceeds.
fn spawn_client_pump(socket: tower_lsp_server::ClientSocket) {
    use futures::{SinkExt, StreamExt};
    use tower_lsp_server::jsonrpc::Response;

    let (mut requests, mut responses) = socket.split();
    tokio::spawn(async move {
        while let Some(request) = requests.next().await {
            let (_method, id, _params) = request.into_parts();
            if let Some(id) = id {
                let _ = responses
                    .send(Response::from_parts(id, Ok(serde_json::Value::Null)))
                    .await;
            }
        }
    });
}

/// Stdin mode: format stdin as if it were `--stdin-filename`, writing the
/// result (changed or not) to stdout. `--check` writes nothing and reports
/// via exit code, mirroring file mode.
async fn run_stdin(server: &Kakehashi, cwd: &Path, options: &FormatOptions) -> u8 {
    let name = options
        .stdin_filename
        .as_ref()
        .expect("run_stdin requires stdin_filename");
    if options.paths.iter().any(|p| p.as_os_str() != "-") {
        eprintln!("error: --stdin-filename cannot be combined with file paths");
        return EXIT_ERROR;
    }

    let mut text = String::new();
    if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut text) {
        eprintln!("error: failed to read stdin: {e}");
        return EXIT_ERROR;
    }

    let absolute = if name.is_absolute() {
        name.clone()
    } else {
        cwd.join(name)
    };
    let formatted = server
        .cli_format_text(
            &absolute,
            &text,
            options.formatting_options(),
            SERVER_READY_TIMEOUT,
        )
        .await;
    let changed = formatted.as_deref().is_some_and(|f| f != text);

    if options.check {
        if changed {
            eprintln!("Would reformat: {}", name.display());
            return EXIT_CHANGED;
        }
        return EXIT_OK;
    }

    let output = match &formatted {
        Some(f) if changed => f.as_str(),
        _ => text.as_str(),
    };
    print!("{output}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    if changed && options.fail_on_change {
        EXIT_CHANGED
    } else {
        EXIT_OK
    }
}

/// File mode: expand `paths`, format each file, and write/report per flags.
async fn run_paths(server: &Kakehashi, cwd: &Path, options: &FormatOptions) -> u8 {
    if options.paths.is_empty() {
        eprintln!("error: no paths given; pass files/directories or use --stdin-filename");
        return EXIT_ERROR;
    }

    let files = match collect_files(cwd, &options.paths, &options.excludes, &|path| {
        server.cli_can_format_path(path)
    }) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };

    let mut changed = 0usize;
    let mut read_errors = 0usize;
    let mut write_errors = 0usize;
    for file in &files {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("error: cannot read '{}': {e}", file.display());
                read_errors += 1;
                continue;
            }
        };
        let absolute = if file.is_absolute() {
            file.clone()
        } else {
            cwd.join(file)
        };
        let Some(formatted) = server
            .cli_format_text(
                &absolute,
                &text,
                options.formatting_options(),
                SERVER_READY_TIMEOUT,
            )
            .await
        else {
            continue;
        };
        if formatted == text {
            continue;
        }
        changed += 1;
        if options.check {
            eprintln!("Would reformat: {}", file.display());
        } else {
            match write_atomically(file, &formatted) {
                Ok(()) => eprintln!("Reformatted: {}", file.display()),
                Err(e) => {
                    eprintln!("error: cannot write '{}': {e}", file.display());
                    write_errors += 1;
                }
            }
        }
    }

    // Unreadable files were never inspected by a formatter, so they belong in
    // neither the changed nor the "already formatted" bucket. (Write-failed
    // files stay in `changed` — the formatter did produce a change for them.)
    let unchanged = files.len() - changed - read_errors;
    let errors = read_errors + write_errors;
    let error_suffix = if errors > 0 {
        format!(", {errors} error(s)")
    } else {
        String::new()
    };
    if options.check {
        eprintln!(
            "{changed} file(s) would be reformatted, {unchanged} already formatted{error_suffix}"
        );
    } else {
        eprintln!("{changed} file(s) reformatted, {unchanged} unchanged{error_suffix}");
    }

    if errors > 0 {
        EXIT_ERROR
    } else if changed > 0 && (options.check || options.fail_on_change) {
        EXIT_CHANGED
    } else {
        EXIT_OK
    }
}

/// Replace `path`'s content via write-to-temp + atomic rename, so a crash
/// mid-write (OOM kill, power loss) can never leave a truncated source file
/// behind. The temp file lives in the target's directory: `persist` renames,
/// and rename is only atomic within one filesystem.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let mut tmp = tempfile::NamedTempFile::new_in(dir.unwrap_or(Path::new(".")))?;
    tmp.write_all(content.as_bytes())?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Build the `--excludes` matcher: gitignore-style patterns rooted at `base`
/// (the current directory). [`ignore::overrides::Override`] is
/// whitelist-oriented, so each user pattern is added negated (`!pattern`) to
/// mean "exclude"; paths matching no pattern pass through.
fn build_exclude_matcher(
    base: &Path,
    excludes: &[String],
) -> Result<ignore::overrides::Override, ignore::Error> {
    let mut builder = ignore::overrides::OverrideBuilder::new(base);
    for pattern in excludes {
        builder.add(&format!("!{pattern}"))?;
    }
    builder.build()
}

/// Expand `paths` into the list of files to format.
///
/// - An explicit file is included unconditionally (bypassing `.gitignore`
///   and the `is_formattable` filter) unless an `--excludes` pattern
///   matches it.
/// - A directory is walked respecting `.gitignore` (even outside a git
///   repository), hidden-file filtering, and `--excludes`; only files for
///   which `is_formattable` returns true (language detectable from the
///   path) are kept.
/// - A path that does not exist is an error.
///
/// The result is sorted and deduplicated for deterministic processing order.
fn collect_files(
    base: &Path,
    paths: &[PathBuf],
    excludes: &[String],
    is_formattable: &dyn Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let exclude_matcher = build_exclude_matcher(base, excludes)
        .map_err(|e| format!("invalid --excludes pattern: {e}"))?;

    let mut files = Vec::new();
    for path in paths {
        let metadata = std::fs::metadata(path)
            .map_err(|e| format!("cannot access '{}': {e}", path.display()))?;
        if metadata.is_dir() {
            if exclude_matcher.matched(path, true).is_ignore() {
                continue;
            }
            walk_directory(path, &exclude_matcher, is_formattable, &mut files);
        } else {
            if exclude_matcher.matched(path, false).is_ignore() {
                continue;
            }
            files.push(path.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Walk `dir` respecting `.gitignore` and `--excludes`, appending every
/// formattable file to `out`. Unreadable entries are warned about and
/// skipped rather than failing the whole run.
fn walk_directory(
    dir: &Path,
    exclude_matcher: &ignore::overrides::Override,
    is_formattable: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) {
    let walker = ignore::WalkBuilder::new(dir)
        .overrides(exclude_matcher.clone())
        // Respect .gitignore files even outside a git repository: the
        // command's contract is "gitignore applies", not "gitignore applies
        // only when git initialized the directory".
        .require_git(false)
        .build();
    for entry in walker {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_some_and(|t| t.is_file()) && is_formattable(entry.path()) {
                    out.push(entry.path().to_path_buf());
                }
            }
            Err(e) => {
                eprintln!("warning: skipping unreadable entry: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn markdown_only(path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "md")
    }

    #[test]
    fn directory_walk_respects_gitignore_without_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("kept.md"), "x");
        write(&tmp.path().join("ignored.md"), "x");
        write(&tmp.path().join(".gitignore"), "ignored.md\n");

        let files =
            collect_files(tmp.path(), &[tmp.path().to_path_buf()], &[], &markdown_only).unwrap();

        assert_eq!(files, vec![tmp.path().join("kept.md")]);
    }

    #[test]
    fn explicit_file_bypasses_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("ignored.md"), "x");
        write(&tmp.path().join(".gitignore"), "ignored.md\n");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().join("ignored.md")],
            &[],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("ignored.md")]);
    }

    #[test]
    fn excludes_filter_walked_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("kept.md"), "x");
        write(&tmp.path().join("dropped.md"), "x");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().to_path_buf()],
            &["dropped.md".to_string()],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("kept.md")]);
    }

    #[test]
    fn excludes_filter_explicit_files_too() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("dropped.md"), "x");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().join("dropped.md")],
            &["dropped.md".to_string()],
            &markdown_only,
        )
        .unwrap();

        assert!(files.is_empty());
    }

    #[test]
    fn excludes_match_directories() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("vendor/dep.md"), "x");
        write(&tmp.path().join("kept.md"), "x");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().to_path_buf()],
            &["vendor/".to_string()],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("kept.md")]);
    }

    #[test]
    fn directory_walk_keeps_only_formattable_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("doc.md"), "x");
        write(&tmp.path().join("notes.txt"), "x");

        let files =
            collect_files(tmp.path(), &[tmp.path().to_path_buf()], &[], &markdown_only).unwrap();

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[test]
    fn explicit_file_bypasses_formattable_filter() {
        // Language detection for explicit files happens later from content
        // (first-line detection), so collection must not drop them by path.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("script"), "#!/usr/bin/env lua");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().join("script")],
            &[],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("script")]);
    }

    #[test]
    fn missing_path_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = collect_files(
            tmp.path(),
            &[tmp.path().join("no-such-file.md")],
            &[],
            &markdown_only,
        );
        assert!(result.is_err());
    }

    #[test]
    fn duplicates_are_deduplicated() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("doc.md"), "x");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().join("doc.md"), tmp.path().to_path_buf()],
            &[],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[test]
    fn gitignored_directory_contents_are_formatted_when_directory_is_explicit() {
        // "paths win over gitignore" extends to directories: walking an
        // explicitly named directory starts a fresh walk rooted there, so a
        // parent rule ignoring the directory itself does not empty it.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".gitignore"), "build/\n");
        write(&tmp.path().join("build/out.md"), "x");

        let files =
            collect_files(tmp.path(), &[tmp.path().join("build")], &[], &markdown_only).unwrap();

        assert_eq!(files, vec![tmp.path().join("build/out.md")]);
    }
}
