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

use crate::cli::files::{collect_files, read_regular_file_to_string};
use crate::cli::terminal::{escape_terminal_controls, escape_terminal_controls_keeping_newlines};
use crate::lsp::Kakehashi;

/// Write one diagnostic line without turning a closed stderr pipe into panic
/// exit 101. Format mode deliberately ignores SIGPIPE, so a consumer that
/// stops reading (`kakehashi format … 2>&1 | head`) surfaces as a write error
/// rather than a signal, and `std::eprintln!` panics on one.
///
/// The write result is dropped unconditionally, including for failures that
/// are not `BrokenPipe`: unlike the stdout path — which distinguishes them and
/// returns [`EXIT_ERROR`] — there is nowhere left to report a failure to
/// report. `--check` sends its report here, so a stderr that fails for another
/// reason (a full disk under `2>report.txt`) yields the exit code without the
/// report.
fn write_line_lossy(mut writer: impl std::io::Write, args: std::fmt::Arguments<'_>) {
    let _ = writeln!(writer, "{args}");
}

/// `eprintln!` that tolerates a closed stderr pipe — see [`write_line_lossy`].
///
/// Deliberately *not* named `eprintln`: shadowing the std macro would silently
/// change every call site in the module, and any future one written above the
/// definition would quietly get the panicking version back. Mirrors
/// `diagnose::elnln!`.
macro_rules! elnln {
    ($($arg:tt)*) => {
        write_line_lossy(std::io::stderr(), format_args!($($arg)*))
    };
}

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
/// Usage error, I/O error, an unloadable `--config-file`, or downstream
/// formatter failure (a configured server failed to start, errored on the
/// request, timed out, or returned a protocol-invalid response).
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
            elnln!("error: failed to start async runtime: {e}");
            return EXIT_ERROR;
        }
    };
    runtime.block_on(run_async(options))
}

async fn run_async(options: FormatOptions) -> u8 {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            elnln!("error: cannot determine current directory: {e}");
            return EXIT_ERROR;
        }
    };

    // Same construction as LSP server mode, but the loopback client socket is
    // pumped by a stub instead of an editor, and handlers are called directly.
    let (service, socket) = LspService::new(Kakehashi::new);
    crate::cli::spawn_client_pump(socket);
    let server = service.inner();
    if let Err(error) = server.cli_initialize(&cwd).await {
        // The message quotes config-file content (a TOML parse error echoes the
        // offending source line), so escape it before it reaches a terminal.
        // Newlines survive: unlike `diagnose`, this output is prose, and the
        // caret diagram is the useful part of a parse error.
        elnln!(
            "error: failed to initialize: {}",
            escape_terminal_controls_keeping_newlines(&error.message)
        );
        return EXIT_ERROR;
    }

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
        elnln!("error: --stdin-filename accepts no paths (optionally a single \"-\")");
        return EXIT_ERROR;
    }

    let mut text = String::new();
    use std::io::Read as _;
    if let Err(e) = std::io::stdin().lock().read_to_string(&mut text) {
        elnln!("error: failed to read stdin: {e}");
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
        elnln!("error: {}", escape_terminal_controls(&failure.to_string()));
    }
    let changed = outcome.formatted.as_deref().is_some_and(|f| f != text);

    if options.check {
        if !outcome.server_failures.is_empty() {
            return EXIT_ERROR;
        }
        if changed {
            elnln!(
                "Would reformat: {}",
                escape_terminal_controls(&name.display().to_string())
            );
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
        elnln!("error: failed to write stdout: {e}");
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
        elnln!("error: no paths given; pass files/directories or use --stdin-filename");
        return EXIT_ERROR;
    }

    let collected = match collect_files(cwd, &options.paths, &options.excludes, &|path| {
        server.cli_can_handle_path(path)
    }) {
        Ok(collected) => collected,
        Err(e) => {
            elnln!("error: {}", escape_terminal_controls(&e.to_string()));
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
        // Escaped once here rather than at each use: a path comes from a
        // directory walk, so it is untrusted text on a line-oriented stream.
        // Newlines are escaped too — unlike the initialization error, these
        // lines are a stream the user greps, one record per file.
        let relative = file.strip_prefix(cwd).unwrap_or(file).display().to_string();
        let display = escape_terminal_controls(&relative);
        let text = match read_regular_file_to_string(file) {
            Ok(text) => text,
            Err(e) => {
                elnln!("error: cannot read '{display}': {e}");
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
            elnln!(
                "error: {display}: {}",
                escape_terminal_controls(&failure.to_string())
            );
        }
        let server_failed = !outcome.server_failures.is_empty();
        if server_failed {
            server_errors += 1;
        }
        match outcome.formatted {
            Some(formatted) if formatted != text => {
                changed += 1;
                if options.check {
                    // Deliberately not mirroring `write_atomically`'s
                    // refusals (hard links) here: `--check` answers "would
                    // the content change", not "would the write succeed",
                    // and it resolves no paths at all today. A read-only
                    // target has always passed `--check` and failed the
                    // apply run the same way.
                    elnln!("Would reformat: {display}");
                } else {
                    match write_atomically(file, &formatted) {
                        Ok(()) => elnln!("Reformatted: {display}"),
                        Err(e) => {
                            elnln!("error: cannot write '{display}': {e}");
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
        elnln!(
            "{changed} file(s) would be reformatted, {unchanged} already formatted{error_suffix}"
        );
    } else {
        // Write failures stay in `changed` for exit-code purposes, but the
        // summary must not claim a file was reformatted when its write failed.
        let reformatted = changed - write_errors;
        elnln!("{reformatted} file(s) reformatted, {unchanged} unchanged{error_suffix}");
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
///
/// Not every target can be served this way: a file with more than one hard
/// link is *refused* (`InvalidInput`) rather than written, because the rename
/// moves only the named directory entry onto the new inode and every other
/// name would silently keep the old content. That is a policy refusal, not an
/// I/O failure, and it is Unix-only — see [`reject_multiple_hard_links`].
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
    // A link can be added while the replacement is prepared, so check again
    // here. This *narrows* the race, it does not close it: a link created
    // between this check and the `rename(2)` inside `persist` is still split.
    // Nothing closes it — no stable API renames conditionally on a link
    // count, `RENAME_EXCHANGE` only reshapes the split, and the one true fix
    // (truncate-and-write in place) forfeits the crash safety this function
    // exists to provide.
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

/// Refuse a target that is reachable under more than one name, because
/// [`write_atomically`]'s rename can only move one of them onto the new
/// content (#760).
///
/// A false negative is the safe direction, and there are two: mounts that do
/// not report real link counts (some FUSE backends, `cifs` without UNIX
/// extensions) and non-Unix platforms. Both degrade to the pre-#760 behavior
/// rather than to a wrong refusal.
#[cfg(unix)]
fn reject_multiple_hard_links(target: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    // `symlink_metadata`, not `metadata`: `rename(2)` never follows a final
    // symlink, so the count has to come from the same inode the rename will
    // unlink. On the pre-create call the two are identical — `target` is
    // canonical, so its last component cannot be a symlink — but the
    // pre-persist call re-reads a path that may have changed underneath, and
    // there a symlink (which can itself carry links on Linux) must be judged
    // by its own count rather than its pointee's.
    let links = std::fs::symlink_metadata(target)?.nlink();
    if links > 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "target has {links} hard links; an atomic replacement updates only this name and would leave the other names on the old content (list them with `find . -samefile <path>`)"
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_multiple_hard_links(_target: &Path) -> std::io::Result<()> {
    // Not implemented rather than impossible, and NTFS does have hard links,
    // so #760 stays live on Windows. Stable std cannot express the check —
    // `windows::fs::MetadataExt::number_of_links` is unstable behind
    // `windows_by_handle` — but `winapi-util` and `windows-sys` are both
    // already in the lockfile on Windows and expose
    // `GetFileInformationByHandle`. Tracked in #933, together with the CI
    // gap that leaves this arm uncompiled until a release build.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn diagnostic_write_tolerates_closed_stderr() {
        write_line_lossy(ClosedPipe, format_args!("initialization failed"));
    }

    /// Tolerating a closed pipe must not degrade into writing nothing at all —
    /// every stderr line in this module goes through the same helper.
    #[test]
    fn diagnostic_write_emits_the_line_on_a_healthy_writer() {
        let mut sink = Vec::new();
        write_line_lossy(&mut sink, format_args!("initialization failed"));
        assert_eq!(sink, b"initialization failed\n");
    }

    #[test]
    fn initialization_failure_text_is_escaped_but_keeps_its_caret_diagram() {
        let toml_error = "TOML parse error at line 1\n  |\n1 | bad = \u{1b}]0;x\u{7}\n  |       ^";
        let escaped = escape_terminal_controls_keeping_newlines(toml_error);
        assert!(
            !escaped.contains('\u{1b}') && !escaped.contains('\u{7}'),
            "terminal controls must not reach the terminal: {escaped:?}"
        );
        assert_eq!(
            escaped.lines().count(),
            4,
            "the caret diagram must survive escaping: {escaped:?}"
        );
    }

    /// Before #760 this write *succeeded*: the target took `"new\n"` on a
    /// fresh inode and the alias silently stayed behind on the old one. Both
    /// names, and the inode that carries them, must survive the refusal.
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

        let error = write_atomically(&target, "new\n").unwrap_err();

        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "a policy refusal, not an I/O failure: {error}"
        );
        assert!(
            error.to_string().contains("hard link"),
            "the refusal must name its cause: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "old\n",
            "the target must not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&alias).unwrap(),
            "old\n",
            "the alias must not be left on stale content"
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().ino(),
            inode,
            "the target must keep its inode, or the link is already split"
        );
        assert_eq!(
            std::fs::metadata(&alias).unwrap().ino(),
            inode,
            "both names must still resolve to the one inode"
        );
        assert_eq!(
            sorted_entry_names(dir.path()),
            ["alias.lua", "source.lua"],
            "the refusal must leave no temp file behind"
        );
    }

    /// The refusal must not cost the normal case. This is the only test of a
    /// *successful* `write_atomically` that CI compiles — `tests/` is gated
    /// behind the `e2e` feature — so without it, inverting the guard to
    /// `links >= 1` would refuse every write and still pass the gate.
    #[cfg(unix)]
    #[test]
    fn write_atomically_replaces_a_single_link_file_keeping_its_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("source.lua");
        std::fs::write(&target, "old\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o754)).unwrap();

        write_atomically(&target, "new\n").expect("a singly linked file must stay writable");

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o754,
            "the rename must carry the target's own mode over"
        );
        assert_eq!(
            sorted_entry_names(dir.path()),
            ["source.lua"],
            "the temp file must not survive the write"
        );
    }

    /// Canonicalization writes *through* a symlink instead of replacing it,
    /// and the link count that decides the refusal is therefore the resolved
    /// file's — a symlink does not count as a hard link to it.
    #[cfg(unix)]
    #[test]
    fn write_atomically_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.lua");
        let link = dir.path().join("link.lua");
        std::fs::write(&real, "old\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomically(&link, "new\n").expect("writing through a symlink must succeed");

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive the replacement"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new\n");
    }

    /// Pins the predicate both call sites rest on. The pre-persist one is
    /// only reachable by racing a concurrent `link(2)`, so this is the only
    /// coverage it can get.
    #[cfg(unix)]
    #[test]
    fn reject_multiple_hard_links_only_rejects_aliased_files() {
        let dir = tempfile::tempdir().unwrap();
        let lone = dir.path().join("lone.lua");
        let aliased = dir.path().join("aliased.lua");
        std::fs::write(&lone, "x").unwrap();
        std::fs::write(&aliased, "x").unwrap();
        std::fs::hard_link(&aliased, dir.path().join("alias.lua")).unwrap();

        reject_multiple_hard_links(&lone).expect("a single name is not an alias");
        let error = reject_multiple_hard_links(&aliased).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    fn sorted_entry_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
