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

use crate::cli::files::collect_files;
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
/// Usage error, I/O error, or downstream formatter failure (a configured
/// server failed to start, errored on the request, timed out, or returned a
/// protocol-invalid response).
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
    crate::cli::spawn_client_pump(socket);
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

/// Stdin mode: format stdin as if it were `--stdin-filename`, writing the
/// result (changed or not) to stdout. `--check` writes nothing and reports
/// via exit code, mirroring file mode.
async fn run_stdin(server: &Kakehashi, cwd: &Path, options: &FormatOptions) -> u8 {
    let name = options
        .stdin_filename
        .as_ref()
        .expect("run_stdin requires stdin_filename");
    // Documented contract: with --stdin-filename, paths must be empty or
    // exactly ["-"]; anything else (real paths, or repeated "-") is a usage
    // error rather than a silently tolerated variant.
    let stdin_paths_ok = options.paths.is_empty()
        || (options.paths.len() == 1 && options.paths[0].as_os_str() == "-");
    if !stdin_paths_ok {
        eprintln!("error: --stdin-filename accepts no paths (optionally a single \"-\")");
        return EXIT_ERROR;
    }

    let mut text = String::new();
    use std::io::Read as _;
    if let Err(e) = std::io::stdin().lock().read_to_string(&mut text) {
        eprintln!("error: failed to read stdin: {e}");
        return EXIT_ERROR;
    }

    let absolute = if name.is_absolute() {
        name.clone()
    } else {
        cwd.join(name)
    };
    let outcome = server
        .cli_format_text(
            &absolute,
            &text,
            options.formatting_options(),
            SERVER_READY_TIMEOUT,
        )
        .await;
    for failure in &outcome.server_failures {
        eprintln!("error: {failure}");
    }
    let changed = outcome.formatted.as_deref().is_some_and(|f| f != text);

    if options.check {
        if !outcome.server_failures.is_empty() {
            return EXIT_ERROR;
        }
        if changed {
            eprintln!("Would reformat: {}", name.display());
            return EXIT_CHANGED;
        }
        return EXIT_OK;
    }

    let output = match &outcome.formatted {
        Some(f) if changed => f.as_str(),
        _ => text.as_str(),
    };
    // SIGPIPE is ignored in format mode (the bridge needs BrokenPipe as a
    // recoverable error), so a consumer that stops reading surfaces here as
    // a write error instead of killing the process. A broken pipe is the
    // consumer's normal early exit (`kakehashi format … | head`), not ours.
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    if let Err(e) = stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        eprintln!("error: failed to write stdout: {e}");
        return EXIT_ERROR;
    }

    if !outcome.server_failures.is_empty() {
        EXIT_ERROR
    } else if changed && options.fail_on_change {
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

    let collected = match collect_files(cwd, &options.paths, &options.excludes, &|path| {
        server.cli_can_handle_path(path)
    }) {
        Ok(collected) => collected,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };
    let files = collected.files;
    let walk_errors = collected.walk_errors;

    let mut changed = 0usize;
    let mut unchanged = 0usize;
    let mut read_errors = 0usize;
    let mut write_errors = 0usize;
    let mut server_errors = 0usize;
    for file in &files {
        // Collected paths are absolute (normalize_path); report them
        // cwd-relative so the output stays readable in deep trees.
        let display = file.strip_prefix(cwd).unwrap_or(file).display();
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("error: cannot read '{display}': {e}");
                read_errors += 1;
                continue;
            }
        };
        let outcome = server
            .cli_format_text(
                file,
                &text,
                options.formatting_options(),
                SERVER_READY_TIMEOUT,
            )
            .await;
        // A configured-but-broken downstream server means this file's
        // formatting is incomplete or unverifiable — an error, not
        // "unchanged" (docs: I/O errors exit 2). Any partial output another
        // server produced is still applied below.
        for failure in &outcome.server_failures {
            eprintln!("error: {display}: {failure}");
        }
        let server_failed = !outcome.server_failures.is_empty();
        if server_failed {
            server_errors += 1;
        }
        match outcome.formatted {
            Some(formatted) if formatted != text => {
                changed += 1;
                if options.check {
                    eprintln!("Would reformat: {display}");
                } else {
                    match write_atomically(file, &formatted) {
                        Ok(()) => eprintln!("Reformatted: {display}"),
                        Err(e) => {
                            eprintln!("error: cannot write '{display}': {e}");
                            write_errors += 1;
                        }
                    }
                }
            }
            // A server-failed file is not "already formatted" — it was never
            // (fully) inspected; it is reported via server_errors instead.
            _ if server_failed => {}
            _ => unchanged += 1,
        }
    }

    let errors = walk_errors + read_errors + write_errors + server_errors;
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
        // Write failures stay in `changed` for exit-code purposes, but the
        // summary must not claim a file was reformatted when its write failed.
        let reformatted = changed - write_errors;
        eprintln!("{reformatted} file(s) reformatted, {unchanged} unchanged{error_suffix}");
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
///
/// The path is canonicalized first so a symlinked source file keeps being a
/// symlink — renaming over the link itself would silently replace it with a
/// regular file and leave the link's target stale (chezmoi/stow setups).
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let target = std::fs::canonicalize(path)?;
    reject_multiple_hard_links(&target)?;
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty());
    let mut tmp = tempfile::NamedTempFile::new_in(dir.unwrap_or(Path::new(".")))?;
    tmp.write_all(content.as_bytes())?;
    // Flush file data to disk before the rename: rename atomicity only
    // guarantees which *name* maps to which inode — without the fsync, a
    // power loss shortly after the rename could leave the new name pointing
    // at not-yet-flushed (truncated) data, defeating the crash-safety goal.
    tmp.as_file().sync_all()?;
    // The temp file is created with restrictive default permissions (0600 on
    // Unix); rename would impose those on the target, silently stripping
    // group/other bits or the executable bit. Carry the target's own mode
    // over — after writing, so a read-only target mode can't block the write.
    tmp.as_file()
        .set_permissions(std::fs::metadata(&target)?.permissions())?;
    // A link can be added while the replacement is prepared. Check again
    // immediately before persist, which would otherwise split the new alias.
    reject_multiple_hard_links(&target)?;
    tmp.persist(&target).map_err(|e| e.error)?;
    // Best-effort directory fsync: on some filesystems the rename's
    // directory-entry update is itself buffered, so without this a power
    // loss could revert the name to the old inode. The data is already
    // durable either way (sync_all above), so failure here is not an error.
    if let Some(dir) = target.parent()
        && let Ok(dir_handle) = std::fs::File::open(dir)
    {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

#[cfg(unix)]
fn reject_multiple_hard_links(target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let links = std::fs::metadata(target)?.nlink();
    if links > 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing atomic replacement of a file with {links} hard links; remove the aliases first"
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_multiple_hard_links(_target: &Path) -> std::io::Result<()> {
    // Stable std currently exposes link counts only on Unix. In particular,
    // Windows MetadataExt::number_of_links is still a nightly-only API.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_atomically_rejects_multiply_linked_target_without_changes() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("source.lua");
        let alias = dir.path().join("alias.lua");
        std::fs::write(&target, "old\n").unwrap();
        std::fs::hard_link(&target, &alias).unwrap();
        let inode = std::fs::metadata(&target).unwrap().ino();
        let entries_before = std::fs::read_dir(dir.path()).unwrap().count();

        let error = write_atomically(&target, "new\n").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("hard link"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&alias).unwrap(), "old\n");
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), inode);
        assert_eq!(std::fs::metadata(&alias).unwrap().ino(), inode);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            entries_before
        );
    }
}
