//! Parser compilation and installation.
//!
//! This module handles fetching parser source (via HTTP archive or git clone),
//! compiling them with tree-sitter-loader, and installing the resulting shared library.

use std::fs;
use std::io::{Read, Write};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use flate2::read::GzDecoder;
use fs4::fs_std::FileExt;
use tar::Archive;
use tree_sitter_loader::Loader;

use super::http::agent_with_timeout;
use super::metadata::{FetchOptions, MetadataError, fetch_parser_metadata};

/// HTTP timeout for archive downloads.
const ARCHIVE_HTTP_TIMEOUT: Duration = Duration::from_secs(120);
const PARSER_OPERATION_MARKER_SUFFIX: &str = ".operation";
const PARSER_BACKUP_MARKER_CONTENT: &[u8] = b"kakehashi parser backup v1\n";

#[cfg(any(windows, test))]
trait ParserFileOps {
    fn rename(&mut self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn remove_file(&mut self, path: &Path) -> std::io::Result<()>;
    fn create_backup_marker(&mut self, backup: &Path) -> std::io::Result<()>;
    fn remove_backup_marker(&mut self, backup: &Path) -> std::io::Result<()>;
}

#[cfg(any(windows, test))]
fn remove_published_backup_marker(
    ops: &mut impl ParserFileOps,
    backup_file: &Path,
) -> std::io::Result<()> {
    ops.remove_backup_marker(backup_file).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "published parser but failed to remove backup marker '{}': {error}",
                parser_backup_ownership_sidecar(backup_file).display()
            ),
        )
    })
}

#[cfg(any(windows, test))]
fn publish_parser_transactionally(
    ops: &mut impl ParserFileOps,
    tmp_file: &Path,
    parser_file: &Path,
    backup_file: &Path,
) -> std::io::Result<()> {
    ops.create_backup_marker(backup_file)?;
    let had_old_parser = match ops.rename(parser_file, backup_file) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ops.remove_backup_marker(backup_file)?;
            false
        }
        Err(error) => {
            if let Err(marker_error) = ops.remove_backup_marker(backup_file) {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "failed to claim parser backup: {error}; failed to remove new ownership marker '{}': {marker_error}",
                        parser_backup_ownership_sidecar(backup_file).display()
                    ),
                ));
            }
            return Err(error);
        }
    };

    match ops.rename(tmp_file, parser_file) {
        Ok(()) => {
            if had_old_parser {
                match ops.remove_file(backup_file) {
                    Ok(()) => remove_published_backup_marker(ops, backup_file)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        remove_published_backup_marker(ops, backup_file)?;
                    }
                    Err(error) => {
                        return Err(std::io::Error::new(
                            error.kind(),
                            format!(
                                "published parser but failed to remove backup '{}': {error}",
                                backup_file.display()
                            ),
                        ));
                    }
                }
            }
            Ok(())
        }
        Err(publish_error) => {
            if !had_old_parser {
                return Err(publish_error);
            }
            match ops.rename(backup_file, parser_file) {
                Ok(()) => {
                    if let Err(marker_error) = ops.remove_backup_marker(backup_file) {
                        return Err(std::io::Error::new(
                            publish_error.kind(),
                            format!(
                                "failed to publish parser: {publish_error}; restored parser but failed to remove backup marker '{}': {marker_error}",
                                parser_backup_ownership_sidecar(backup_file).display()
                            ),
                        ));
                    }
                    Err(publish_error)
                }
                Err(rollback_error) => Err(std::io::Error::new(
                    publish_error.kind(),
                    format!(
                        "failed to publish parser: {publish_error}; failed to restore backup '{}': {rollback_error}",
                        backup_file.display()
                    ),
                )),
            }
        }
    }
}

#[cfg(windows)]
struct StdParserFileOps;

#[cfg(windows)]
impl ParserFileOps for StdParserFileOps {
    fn rename(&mut self, from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&mut self, path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }

    fn create_backup_marker(&mut self, backup: &Path) -> std::io::Result<()> {
        match fs::symlink_metadata(backup) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("parser backup '{}' already exists", backup.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let marker_path = parser_backup_ownership_sidecar(backup);
        let marker_tmp = marker_path.with_extension(format!("owner.{}.tmp", ulid::Ulid::new()));
        let mut marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_tmp)?;
        if let Err(error) = marker.write_all(PARSER_BACKUP_MARKER_CONTENT) {
            drop(marker);
            let _ = fs::remove_file(marker_tmp);
            return Err(error);
        }
        if let Err(error) = marker.sync_all() {
            drop(marker);
            let _ = fs::remove_file(marker_tmp);
            return Err(error);
        }
        drop(marker);
        if let Err(error) = fs::rename(&marker_tmp, &marker_path) {
            let _ = fs::remove_file(marker_tmp);
            return Err(error);
        }
        Ok(())
    }

    fn remove_backup_marker(&mut self, backup: &Path) -> std::io::Result<()> {
        match fs::remove_file(parser_backup_ownership_sidecar(backup)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn generated_parser_backup_matches_language(name: &str, language: &str) -> bool {
    parser_backup_language(name).is_some_and(|backup_language| backup_language == language)
}

fn parser_backup_language(name: &str) -> Option<&str> {
    let stem = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".backup"))?;
    let stem = stem.strip_suffix(&format!(".{}", std::env::consts::DLL_EXTENSION))?;
    let (prefix, counter) = stem.rsplit_once('.')?;
    let (backup_language, pid) = prefix.rsplit_once('.')?;
    if super::queries::is_safe_language_name(backup_language)
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && !counter.is_empty()
        && counter.bytes().all(|byte| byte.is_ascii_digit())
    {
        Some(backup_language)
    } else {
        None
    }
}

fn parser_backup_ownership_sidecar(backup: &Path) -> PathBuf {
    backup.with_file_name(format!(
        "{}.owner",
        backup
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(".parser.backup")
    ))
}

fn remove_parser_backup_marker(backup: &Path) -> std::io::Result<()> {
    match fs::remove_file(parser_backup_ownership_sidecar(backup)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn parser_backup_marker_is_owned(marker: &Path) -> std::io::Result<bool> {
    let metadata = match fs::symlink_metadata(marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Ok(false);
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself so a swap after `symlink_metadata`
        // cannot redirect ownership validation outside the parser directory.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match options.open(marker) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.len() != PARSER_BACKUP_MARKER_CONTENT.len() as u64
    {
        return Ok(false);
    }
    let mut content = Vec::with_capacity(PARSER_BACKUP_MARKER_CONTENT.len() + 1);
    file.take((PARSER_BACKUP_MARKER_CONTENT.len() + 1) as u64)
        .read_to_end(&mut content)?;
    Ok(content == PARSER_BACKUP_MARKER_CONTENT)
}

fn parser_backup_files(parser_dir: &Path, language: &str) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(parser_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !entry.file_type()?.is_file()
            || !generated_parser_backup_matches_language(name, language)
        {
            continue;
        }
        if parser_backup_marker_is_owned(&parser_backup_ownership_sidecar(&entry.path()))? {
            backups.push(entry.path());
        }
    }
    Ok(backups)
}

/// Return languages with kakehashi-owned transactional backups.
pub fn owned_parser_backup_languages(parser_dir: &Path) -> std::io::Result<Vec<String>> {
    let entries = match fs::read_dir(parser_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut languages = std::collections::BTreeSet::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(language) = name.to_str().and_then(parser_backup_language) else {
            continue;
        };
        let marker = parser_backup_ownership_sidecar(&entry.path());
        if parser_backup_marker_is_owned(&marker)? {
            languages.insert(language.to_owned());
        }
    }
    Ok(languages.into_iter().collect())
}

fn cleanup_orphan_parser_backup_markers(parser_dir: &Path, language: &str) -> std::io::Result<()> {
    let entries = match fs::read_dir(parser_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let marker = entry.path();
        let Some(marker_name) = marker.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let (backup_name, staged) = if let Some(backup_name) = marker_name.strip_suffix(".owner") {
            (backup_name, false)
        } else if let Some((backup_name, nonce)) = marker_name
            .strip_suffix(".tmp")
            .and_then(|name| name.rsplit_once(".owner."))
            && ulid::Ulid::from_string(nonce).is_ok()
        {
            (backup_name, true)
        } else {
            continue;
        };
        if !generated_parser_backup_matches_language(backup_name, language) {
            continue;
        }
        let backup = parser_dir.join(backup_name);
        if parser_backup_marker_is_owned(&marker)? {
            if staged {
                match fs::remove_file(marker) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            } else if !backup.try_exists()? {
                remove_parser_backup_marker(&backup)?;
            }
        }
    }
    Ok(())
}

fn remove_owned_parser_backup(backup: &Path) -> std::io::Result<()> {
    match fs::remove_file(backup) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    remove_parser_backup_marker(backup)
}

#[cfg(any(windows, test))]
fn recover_parser_backups(
    parser_dir: &Path,
    parser_file: &Path,
    language: &str,
) -> std::io::Result<bool> {
    cleanup_orphan_parser_backup_markers(parser_dir, language)?;
    let backups = parser_backup_files(parser_dir, language)?;
    if backups.is_empty() {
        return Ok(false);
    }
    if parser_file.try_exists()? {
        if !fs::metadata(parser_file)?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("parser path '{}' is not a file", parser_file.display()),
            ));
        }
        for backup in backups {
            remove_owned_parser_backup(&backup)?;
        }
        return Ok(false);
    }
    let mut timed_backups = backups
        .into_iter()
        .map(|path| {
            let modified = fs::metadata(&path)?.modified()?;
            Ok((modified, path))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    timed_backups.sort_by_key(|(modified, _)| *modified);
    let (_, restore) = timed_backups.pop().expect("non-empty parser backup list");
    fs::rename(&restore, parser_file)?;
    remove_parser_backup_marker(&restore)?;
    for (_, obsolete) in timed_backups {
        remove_owned_parser_backup(&obsolete)?;
    }
    Ok(true)
}

/// Remove parser artifacts without operation coordination for focused unit tests.
#[cfg(test)]
fn remove_parser_install_and_backups(parser_dir: &Path, language: &str) -> std::io::Result<bool> {
    if !super::queries::is_safe_language_name(language) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe parser language name",
        ));
    }
    if !parser_dir.try_exists()? {
        return Ok(false);
    }
    cleanup_orphan_parser_backup_markers(parser_dir, language)?;
    let parser_file = parser_dir.join(format!("{}.{}", language, std::env::consts::DLL_EXTENSION));
    // Remove recovery copies first. If uninstall is interrupted, leaving the
    // canonical parser is an incomplete uninstall that can be retried; deleting
    // canonical first could let the next install resurrect an old backup.
    let mut removed = false;
    for backup in parser_backup_files(parser_dir, language)? {
        remove_owned_parser_backup(&backup)?;
        removed = true;
    }
    match fs::remove_file(parser_file) {
        Ok(()) => removed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(removed)
}

/// Error types for parser installation.
#[derive(Debug)]
pub enum ParserInstallError {
    /// Metadata fetch failed.
    MetadataError(MetadataError),
    /// Git operation failed.
    GitError(String),
    /// Archive download or extraction failed.
    ArchiveError(String),
    /// Compilation failed.
    CompileError(String),
    /// File system operation failed.
    IoError(std::io::Error),
    /// Parser already exists.
    AlreadyExists(PathBuf),
}

impl std::fmt::Display for ParserInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MetadataError(e) => write!(f, "{}", e),
            Self::GitError(msg) => write!(f, "Git error: {}", msg),
            Self::ArchiveError(msg) => write!(f, "Archive error: {}", msg),
            Self::CompileError(msg) => write!(f, "Compilation error: {}", msg),
            Self::IoError(e) => write!(f, "IO error: {}", e),
            Self::AlreadyExists(path) => {
                write!(
                    f,
                    "Parser already exists at {}. Use --force to overwrite.",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ParserInstallError {}

impl From<std::io::Error> for ParserInstallError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError(e)
    }
}

impl From<MetadataError> for ParserInstallError {
    fn from(e: MetadataError) -> Self {
        Self::MetadataError(e)
    }
}

/// Result of installing a parser.
#[derive(Debug)]
pub struct ParserInstallResult {
    /// The language that was installed.
    pub language: String,
    /// Path where parser was installed.
    pub install_path: PathBuf,
    /// Git revision that was used.
    pub revision: String,
}

/// Internal outcome for callers that coordinate parser and query operations.
#[doc(hidden)]
pub enum ParserInstallOutcome {
    Installed(ParserInstallResult),
    Recovered(PathBuf),
}

/// How the parser source is compiled.
///
/// The killable path re-execs **this** binary's `__compile-parser` subcommand, so
/// it is only valid when the running executable is the `kakehashi` binary (the LSP
/// server and the `language install` CLI). A caller whose `current_exe()` is
/// something else — a test harness binary, an external embedder — must compile
/// in-process, since that other binary has no `__compile-parser` subcommand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParserCompile {
    /// Compile inside a killable subprocess (re-exec `__compile-parser`) so a hung
    /// `cc` can be deadline-killed. The production default.
    #[default]
    KillableSubprocess,
    /// Compile in-process — no killable deadline. For callers whose `current_exe()`
    /// is not the `kakehashi` binary (e.g. test setup).
    InProcess,
}

/// Options for parser installation.
pub struct InstallOptions {
    /// Base data directory.
    pub data_dir: PathBuf,
    /// Whether to overwrite existing parser.
    pub force: bool,
    /// Whether to print verbose output.
    pub verbose: bool,
    /// Whether to bypass the metadata cache.
    pub no_cache: bool,
    /// How to compile the parser source (see [`ParserCompile`]).
    pub compile: ParserCompile,
}

/// Check if a parser file exists for the given language.
///
/// Returns the path to the parser file if it exists, None otherwise.
pub fn parser_file_exists(language: &str, data_dir: &Path) -> Option<PathBuf> {
    let parser_file =
        data_dir
            .join("parser")
            .join(format!("{}.{}", language, std::env::consts::DLL_EXTENSION));
    if parser_file.exists() {
        Some(parser_file)
    } else {
        None
    }
}

/// Upper bound for a single parser compile.
///
/// The loader shells out to `cc` with no timeout of its own; a pathological
/// toolchain (a hung or wedged compiler) would otherwise block the install — and,
/// in the LSP server, the in-progress install marker and process exit — forever.
/// Generous enough not to false-kill a legitimately slow grammar (e.g. a cold
/// TypeScript build) while still bounding a true hang.
const PARSER_COMPILE_TIMEOUT: Duration = Duration::from_secs(300);

/// Compile a Tree-sitter parser from source using tree-sitter-loader, **in
/// process**.
///
/// This is the raw loader call: it shells out to `cc` and, on a pathological
/// toolchain, can hang with no surfaced child to kill. Production install does
/// **not** call this directly — it goes through [`compile_parser`], which runs it
/// inside a killable subprocess. This is `pub` only as the entry point for that
/// subprocess: the hidden `__compile-parser` subcommand (`src/bin/main.rs`) calls
/// here.
pub fn compile_parser_inprocess(
    grammar_path: &Path,
    output_path: &Path,
) -> Result<(), ParserInstallError> {
    let parent_dir = output_path.parent().ok_or_else(|| {
        ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Could not determine parent directory for output path '{}'",
                output_path.display()
            ),
        ))
    })?;
    let loader = Loader::with_parser_lib_path(parent_dir.to_path_buf());
    loader
        .compile_parser_at_path(grammar_path, output_path.to_path_buf(), &[])
        .map_err(|e| ParserInstallError::CompileError(e.to_string()))
}

/// Arm a self-deadline inside the `__compile-parser` subprocess.
///
/// The parent's [`run_killable_subprocess`] normally enforces the deadline and
/// kills the compile's process group. But if the *parent* (`kakehashi`) dies
/// mid-compile — editor crash, `SIGTERM`, cancellation — this subprocess is
/// orphaned and the loader's synchronous `cc` would keep running forever. This
/// backstop closes that: become our own process-group leader (so the group kill
/// can only reach us and our `cc`, never a shell that launched us directly), then
/// spawn a watchdog that group-kills us shortly after [`PARSER_COMPILE_TIMEOUT`].
///
/// On a normal/quick compile the process exits first and the watchdog thread is
/// torn down with it; the grace margin keeps the parent's deadline the usual
/// trigger. Unix-only (process groups); a no-op elsewhere. The subcommand entry
/// (`src/bin/main.rs`) calls this before compiling.
pub fn arm_compile_watchdog() {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::{Pid, getpgrp, getpid, setpgid};
        // Become our own group leader. When spawned by run_killable_subprocess the
        // parent already did this via process_group(0) (a harmless repeat); when run
        // directly it isolates us from the launching shell's group.
        let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
        // CRITICAL: only arm the group-kill if we are *confirmed* our own group
        // leader (pgid == pid). If setpgid failed (sandbox, already a session
        // leader), we're still in the parent's/shell's group, and killpg(0) would
        // SIGKILL *that* group — terminating the parent (or an interactive shell)
        // along with us. In that case skip the watchdog: the parent-side deadline
        // still bounds the compile; we only forgo the orphan backstop.
        if getpgrp() != getpid() {
            return;
        }
        std::thread::spawn(|| {
            std::thread::sleep(PARSER_COMPILE_TIMEOUT + Duration::from_secs(30));
            // pgid 0 = our own process group (us + the cc we shelled out).
            let _ = killpg(Pid::from_raw(0), Signal::SIGKILL);
        });
    }
}

/// Resolve the path to re-exec for the `__compile-parser` subprocess.
///
/// On Linux, prefer `/proc/self/exe`: if the running `kakehashi` binary is
/// replaced (a package upgrade while the LSP server runs), `current_exe()`
/// resolves to a stale `".../kakehashi (deleted)"` path that no longer spawns,
/// whereas `execve("/proc/self/exe")` still runs the original image. Elsewhere
/// `current_exe()` is the portable answer.
fn self_exe_for_reexec() -> Result<PathBuf, ParserInstallError> {
    #[cfg(target_os = "linux")]
    {
        // Check the symlink with `symlink_metadata` (lstat), NOT `exists()`:
        // `exists()` follows the link to the target and returns false exactly when
        // the binary was deleted/replaced — the case this branch exists to handle.
        // `symlink_metadata` confirms the `/proc/self/exe` link itself is present
        // (so we fall back to `current_exe()` when `/proc` isn't mounted) without
        // requiring the target to still exist.
        let proc_self = Path::new("/proc/self/exe");
        if std::fs::symlink_metadata(proc_self).is_ok() {
            return Ok(proc_self.to_path_buf());
        }
    }
    std::env::current_exe().map_err(|e| {
        ParserInstallError::CompileError(format!("locating the kakehashi binary: {e}"))
    })
}

/// Compile a Tree-sitter parser under a hard, killable deadline.
///
/// The loader's `compile_parser_at_path` shells out to `cc` without surfacing the
/// child, so a `tokio::time::timeout` cannot kill a hung compiler (the blocking
/// thread and its `cc` would keep running). Instead we **re-exec** this binary's
/// hidden `__compile-parser` subcommand inside a killable process group (see the
/// per-document-parse-scheduler ADR, "a real install deadline via a killable
/// subprocess") and bound it with [`PARSER_COMPILE_TIMEOUT`]. Re-exec rather than
/// `fork`: forking a multithreaded process is unsafe past the child's first
/// non-async-signal-safe call.
fn compile_parser(grammar_path: &Path, output_path: &Path) -> Result<(), ParserInstallError> {
    let mut cmd = Command::new(self_exe_for_reexec()?);
    cmd.arg("__compile-parser")
        .arg(grammar_path)
        .arg(output_path)
        // Null stdin/stdout: when the parent is the LSP server, stdout is the
        // JSON-RPC channel and no child output may reach it. stderr stays
        // inherited so `cc` diagnostics land in the logs.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    let status = run_killable_subprocess(cmd, PARSER_COMPILE_TIMEOUT, "parser compile")?;
    if !status.success() {
        return Err(ParserInstallError::CompileError(format!(
            "parser compile subprocess for '{}' exited with {}",
            grammar_path.display(),
            status
        )));
    }
    Ok(())
}

struct ParserReplaceLockGuard {
    _file: fs::File,
}

impl ParserReplaceLockGuard {
    fn acquire(parser_dir: &Path, language: &str) -> Result<Self, ParserInstallError> {
        if !super::queries::is_safe_language_name(language) {
            return Err(unsafe_language_name_error(language));
        }
        fs::create_dir_all(parser_dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(parser_dir.join(format!(".{language}.replace.lock")))?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

fn parser_operation_marker_path(parser_dir: &Path, language: &str) -> PathBuf {
    parser_dir.join(format!(".{language}{PARSER_OPERATION_MARKER_SUFFIX}"))
}

fn unsafe_language_name_error(language: &str) -> ParserInstallError {
    ParserInstallError::IoError(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("unsafe language name '{}'", language.escape_default()),
    ))
}

fn begin_parser_install(
    parser_dir: &Path,
    parser_file: &Path,
    language: &str,
    force: bool,
) -> Result<String, ParserInstallError> {
    let _replace_lock = ParserReplaceLockGuard::acquire(parser_dir, language)?;
    if parser_file.exists() && !force {
        return Err(ParserInstallError::AlreadyExists(parser_file.to_path_buf()));
    }
    let token = format!("install:{}", ulid::Ulid::new());
    let mut marker = fs::File::create(parser_operation_marker_path(parser_dir, language))?;
    marker.write_all(token.as_bytes())?;
    Ok(token)
}

fn publish_compiled_parser(
    tmp_file: &Path,
    parser_file: &Path,
    language: &str,
    install_token: &str,
) -> Result<(), ParserInstallError> {
    let parser_dir = parser_file.parent().ok_or_else(|| {
        ParserInstallError::IoError(std::io::Error::other("parser path has no parent"))
    })?;
    let _replace_lock = ParserReplaceLockGuard::acquire(parser_dir, language)?;
    let current_token = match fs::read_to_string(parser_operation_marker_path(parser_dir, language))
    {
        Ok(token) => Some(token),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    if current_token.as_deref() != Some(install_token) {
        let _ = fs::remove_file(tmp_file);
        return Err(ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            format!("Parser install for {language} was superseded by a newer parser operation"),
        )));
    }
    // Unix atomically replaces the destination. Windows needs the same-directory
    // owned backup so a failed publication can restore the working parser.
    #[cfg(not(windows))]
    let published = fs::rename(tmp_file, parser_file);
    #[cfg(windows)]
    let published = {
        let backup_file = tmp_file.with_extension("backup");
        publish_parser_transactionally(&mut StdParserFileOps, tmp_file, parser_file, &backup_file)
    };
    if let Err(e) = published {
        let _ = fs::remove_file(tmp_file);
        return Err(ParserInstallError::IoError(e));
    }
    Ok(())
}

/// Remove a parser while preventing an already-running install from publishing later.
pub fn remove_parser_install(
    parser_dir: &Path,
    language: &str,
) -> Result<bool, ParserInstallError> {
    if !super::queries::is_safe_language_name(language) {
        return Err(unsafe_language_name_error(language));
    }
    let data_dir = parser_dir.parent().ok_or_else(|| {
        ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "parser directory must have a data-directory parent",
        ))
    })?;
    let _operation_lock = super::LanguageOperationLockGuard::acquire(data_dir, language)?;
    remove_parser_install_after_operation_started(
        parser_dir,
        language,
        super::LanguageOperationPermit::Language(&_operation_lock),
    )
}

/// Remove a parser while the caller holds this language's operation lock.
#[doc(hidden)]
pub fn remove_parser_install_after_operation_started(
    parser_dir: &Path,
    language: &str,
    permit: super::LanguageOperationPermit<'_>,
) -> Result<bool, ParserInstallError> {
    let Some(data_dir) = parser_dir.parent() else {
        return Err(ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "parser directory must have a data-directory parent",
        )));
    };
    if !permit.covers(data_dir, language) {
        return Err(ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "operation permit does not cover this parser removal",
        )));
    }
    let _replace_lock = ParserReplaceLockGuard::acquire(parser_dir, language)?;
    let mut marker = fs::File::create(parser_operation_marker_path(parser_dir, language))?;
    writeln!(marker, "uninstall:{}", ulid::Ulid::new())?;
    cleanup_orphan_parser_backup_markers(parser_dir, language)?;
    // Delete recovery copies first so an interrupted uninstall cannot leave a
    // backup that a later install would restore after canonical removal.
    let mut removed = false;
    for backup in parser_backup_files(parser_dir, language)? {
        remove_owned_parser_backup(&backup)?;
        removed = true;
    }
    let parser_file = parser_dir.join(format!("{language}.{}", std::env::consts::DLL_EXTENSION));
    match fs::remove_file(parser_file) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(removed),
        Err(error) => Err(ParserInstallError::IoError(error)),
    }
}

/// Install a Tree-sitter parser for a language.
pub fn install_parser(
    language: &str,
    options: &InstallOptions,
) -> Result<ParserInstallResult, ParserInstallError> {
    if !super::queries::is_safe_language_name(language) {
        return Err(unsafe_language_name_error(language));
    }
    let _operation_lock = super::LanguageOperationLockGuard::acquire(&options.data_dir, language)?;
    install_parser_after_operation_started(
        language,
        options,
        super::LanguageOperationPermit::Language(&_operation_lock),
    )
}

/// Install a parser while the caller holds this language's operation lock.
///
/// Most callers should use [`install_parser`]. This entry point exists for a
/// combined parser-and-query operation that must retain one lock across both.
#[doc(hidden)]
pub fn install_parser_after_operation_started(
    language: &str,
    options: &InstallOptions,
    permit: super::LanguageOperationPermit<'_>,
) -> Result<ParserInstallResult, ParserInstallError> {
    match install_parser_with_outcome_after_operation_started(language, options, permit)? {
        ParserInstallOutcome::Installed(result) => Ok(result),
        ParserInstallOutcome::Recovered(path) => Err(ParserInstallError::AlreadyExists(path)),
    }
}

/// Install a parser while retaining the recovery-specific outcome.
#[doc(hidden)]
pub fn install_parser_with_outcome_after_operation_started(
    language: &str,
    options: &InstallOptions,
    permit: super::LanguageOperationPermit<'_>,
) -> Result<ParserInstallOutcome, ParserInstallError> {
    if !permit.covers(&options.data_dir, language) {
        return Err(ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "operation permit does not cover this parser install",
        )));
    }
    // `language` becomes path segments (`parser/<language>.<ext>` and the temp
    // file) and a URL/metadata key, so reject traversal-capable names before
    // touching the filesystem. Higher-level callers (auto-install) already gate
    // this, but `install_parser` is a public entry point and must be safe on its
    // own — a name like `../../evil` would otherwise write outside the data dir.
    if !super::queries::is_safe_language_name(language) {
        return Err(unsafe_language_name_error(language));
    }

    let parser_dir = options.data_dir.join("parser");
    let parser_file = parser_dir.join(format!("{}.{}", language, std::env::consts::DLL_EXTENSION));

    fs::create_dir_all(&parser_dir)?;
    #[cfg(windows)]
    let recovered = recover_parser_backups(&parser_dir, &parser_file, language)?;

    #[cfg(windows)]
    if recovered && !options.force {
        return Ok(ParserInstallOutcome::Recovered(parser_file));
    }

    let install_token = begin_parser_install(&parser_dir, &parser_file, language, options.force)?;

    // Fetch metadata (with caching support)
    if options.verbose {
        eprintln!("Fetching metadata for '{}'...", language);
    }
    let fetch_options = FetchOptions {
        data_dir: Some(&options.data_dir),
        use_cache: !options.no_cache,
    };
    let metadata = fetch_parser_metadata(language, Some(&fetch_options))?;

    if options.verbose {
        eprintln!("Repository: {}", metadata.url);
        eprintln!("Revision: {}", metadata.revision);
    }

    // Create temp directory for source acquisition
    let temp_dir = tempfile::tempdir()?;
    let clone_dir = temp_dir.path().join("parser");

    // Fetch parser source code
    if options.verbose {
        eprintln!("Fetching source...");
    }
    fetch_source(&metadata.url, &metadata.revision, &clone_dir)?;

    // Determine the source directory (handle monorepos)
    let source_dir = parser_source_dir(&clone_dir, metadata.location.as_deref())?;
    reject_parser_source_symlinks(&source_dir)?;

    if options.verbose {
        eprintln!("Building parser in: {}", source_dir.display());
    }

    // Compile to a temp path in the install dir, then atomically rename into place
    // on success. A failed or deadline-killed compile can leave a partial/corrupt
    // library behind (`parser_file_exists` only checks existence, so a leftover
    // would make later runs skip reinstall and load a broken parser); writing to a
    // temp and renaming only on success means a failure never clobbers a
    // previously-working parser (e.g. a `force` reinstall that fails) and concurrent
    // readers never observe a half-written file.
    //
    // The temp name combines the pid with a process-local counter so two installs
    // in the same process never collide on it, independent of the per-language
    // install dedup. (Windows caveat: replacing a parser that is *currently loaded*
    // by this process still fails at the rename — a loaded DLL can't be removed — a
    // pre-existing platform limitation this scheme doesn't change.)
    static TMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let tmp_file = parser_dir.join(format!(
        ".{}.{}.{}.{}.tmp",
        language,
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::env::consts::DLL_EXTENSION
    ));
    let compiled = match options.compile {
        ParserCompile::KillableSubprocess => compile_parser(&source_dir, &tmp_file),
        ParserCompile::InProcess => compile_parser_inprocess(&source_dir, &tmp_file),
    };
    match compiled {
        Ok(()) => publish_compiled_parser(&tmp_file, &parser_file, language, &install_token)?,
        Err(e) => {
            let _ = fs::remove_file(&tmp_file);
            return Err(e);
        }
    }

    if options.verbose {
        eprintln!("Installed to: {}", parser_file.display());
    }

    Ok(ParserInstallOutcome::Installed(ParserInstallResult {
        language: language.to_string(),
        install_path: parser_file,
        revision: metadata.revision,
    }))
}

/// Construct a GitHub archive download URL from a repository URL and revision.
///
/// Returns `None` if the URL is not a GitHub HTTPS URL.
/// GitHub serves tarballs at `https://github.com/{owner}/{repo}/archive/{revision}.tar.gz`.
fn github_archive_url(repo_url: &str, revision: &str) -> Option<String> {
    let url = clean_url(repo_url);
    if !url.starts_with("https://github.com/") {
        return None;
    }
    Some(format!("{}/archive/{}.tar.gz", url, revision))
}

/// Extract the repository name from a URL.
///
/// For `https://github.com/tree-sitter/tree-sitter-json.git`, returns `"tree-sitter-json"`.
fn repo_name_from_url(url: &str) -> Option<String> {
    let url = clean_url(url);
    url.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Strip `.git` suffix and trailing slashes from a URL.
fn clean_url(url: &str) -> &str {
    let url = url.strip_suffix(".git").unwrap_or(url);
    url.trim_end_matches('/')
}

/// Fetch parser source code into a destination directory.
///
/// Strategy:
/// 1. If the URL is a GitHub HTTPS URL, try downloading the archive tarball.
/// 2. If archive download fails (or URL is not GitHub), fall back to git clone.
fn fetch_source(url: &str, revision: &str, dest: &Path) -> Result<(), ParserInstallError> {
    validate_parser_source_metadata(url, revision)?;

    // Try archive download for GitHub URLs
    if let Some(archive_url) = github_archive_url(url, revision)
        && let Some(repo_name) = repo_name_from_url(url)
    {
        log::info!(
            target: "kakehashi::install",
            "Downloading archive from {}",
            archive_url
        );
        match download_and_extract_archive(&archive_url, &repo_name, revision, dest) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!(
                    target: "kakehashi::install",
                    "Archive download failed, falling back to git clone: {}",
                    e
                );
                // Clean up partial extraction before fallback
                let _ = fs::remove_dir_all(dest);
            }
        }
    }

    // Fallback: git clone
    log::info!(
        target: "kakehashi::install",
        "Cloning repository {} at {}",
        url,
        revision
    );
    clone_repo(url, revision, dest)
}

/// Download a GitHub archive tarball and extract it to the destination directory.
///
/// The tarball's root directory (e.g., `tree-sitter-json-0.24.8/`) is stripped,
/// so `dest` contains the repository contents directly.
fn download_and_extract_archive(
    archive_url: &str,
    repo_name: &str,
    revision: &str,
    dest: &Path,
) -> Result<(), ParserInstallError> {
    let agent = agent_with_timeout(ARCHIVE_HTTP_TIMEOUT);

    let response = agent.get(archive_url).call().map_err(|e| match e {
        ureq::Error::StatusCode(code) => {
            ParserInstallError::ArchiveError(format!("HTTP {} downloading {}", code, archive_url))
        }
        e => ParserInstallError::ArchiveError(format!("Download failed: {}", e)),
    })?;

    // into_reader() streams without a size limit; parser archives can exceed
    // ureq's 10MB read_to_* default.
    let decoder = GzDecoder::new(response.into_body().into_reader());
    let mut archive = Archive::new(decoder);

    // GitHub names the root directory `{repo}-{revision_without_v_prefix}`
    let expected_prefix = archive_root_dir_name(repo_name, revision);

    fs::create_dir_all(dest)?;

    for entry_result in archive.entries().map_err(|e| {
        ParserInstallError::ArchiveError(format!("Failed to read archive entries: {}", e))
    })? {
        let mut entry = entry_result.map_err(|e| {
            ParserInstallError::ArchiveError(format!("Failed to read entry: {}", e))
        })?;

        let path = entry.path().map_err(|e| {
            ParserInstallError::ArchiveError(format!("Invalid path in archive: {}", e))
        })?;

        // Strip the root directory prefix (e.g., "tree-sitter-json-0.24.8/")
        let relative = match path.strip_prefix(&expected_prefix) {
            Ok(p) => p.to_path_buf(),
            Err(_) => continue,
        };

        // Skip empty relative paths (the root directory itself)
        if relative.as_os_str().is_empty() {
            continue;
        }

        // Prevent path traversal attacks (zip slip)
        if relative
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ParserInstallError::ArchiveError(format!(
                "Path traversal attempt detected in archive: {}",
                relative.display()
            )));
        }

        let target = dest.join(&relative);

        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            entry.unpack(&target).map_err(|e| {
                ParserInstallError::ArchiveError(format!(
                    "Failed to extract {}: {}",
                    relative.display(),
                    e
                ))
            })?;
        }
    }

    Ok(())
}

/// Derive the expected root directory name inside a GitHub archive tarball.
///
/// GitHub names the root directory `{repo}-{revision_without_v_prefix}`.
/// For tag `v0.24.8` of repo `tree-sitter-json`, the root is `tree-sitter-json-0.24.8`.
fn archive_root_dir_name(repo_name: &str, revision: &str) -> String {
    let clean_revision = revision.strip_prefix('v').unwrap_or(revision);
    format!("{}-{}", repo_name, clean_revision)
}

/// Upper bound for each git operation in the clone fallback.
///
/// The archive-download path bounds its HTTP calls (`ARCHIVE_HTTP_TIMEOUT`);
/// git has none of its own, and the install runs inside a non-cancellable
/// `spawn_blocking` task — an unbounded hang keeps the language's in-progress
/// marker held for the whole session (every reopen skips parsing) and blocks
/// process exit while the runtime drains blocking tasks.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// Build a git command that can never prompt, so a private/renamed
/// repository fails fast instead of waiting for credentials nobody can type:
/// stdin is nulled, `GIT_TERMINAL_PROMPT=0` covers terminal prompts,
/// `GIT_ASKPASS`/`SSH_ASKPASS` are pointed at `true` on unix (immediately
/// answers with an empty string), and ssh runs in BatchMode (fails instead
/// of prompting). stdout is nulled too — when this runs inside the LSP server,
/// stdout is the JSON-RPC channel and no child chatter may reach it; git's
/// diagnostics go to stderr, which stays inherited for the logs.
fn git_command(args: &[&str], current_dir: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "http.followRedirects=false",
        "-c",
        "protocol.allow=never",
        "-c",
        "protocol.https.allow=always",
    ])
    .args(args)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .env("GIT_TERMINAL_PROMPT", "0");
    // `true` as an askpass helper answers any credential prompt with an empty
    // string immediately. POSIX guarantees the binary; Windows does not ship
    // one (git would fail trying to run it), so gate to unix — Windows relies
    // on GIT_TERMINAL_PROMPT plus run_with_timeout's deadline.
    #[cfg(unix)]
    {
        cmd.env("GIT_ASKPASS", "true").env("SSH_ASKPASS", "true");
    }
    // GIT_SSH_COMMAND: extend an OpenSSH command with BatchMode so ssh auth
    // fails fast instead of prompting; for OpenSSH the first -o value obtained
    // wins, so a user-set BatchMode is respected. Leave non-OpenSSH commands
    // (plink etc. reject -o flags) untouched, and when unset set nothing so a
    // core.sshCommand gitconfig keeps working — run_with_timeout's deadline
    // still bounds any prompt that slips through.
    if let Ok(ssh_command) = std::env::var("GIT_SSH_COMMAND")
        && !ssh_command.trim().is_empty()
    {
        // The command word may be a quoted path containing spaces (common on
        // Windows: "C:\Program Files\...\ssh.exe" -i key); take the quoted
        // token whole instead of splitting on its inner spaces.
        let trimmed = ssh_command.trim_start();
        let cmd_word = if let Some(rest) = trimmed.strip_prefix('"') {
            rest.split('"').next().unwrap_or("")
        } else if let Some(rest) = trimmed.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("")
        } else {
            trimmed.split_whitespace().next().unwrap_or("")
        };
        let is_openssh = std::path::Path::new(cmd_word)
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("ssh"));
        if is_openssh {
            cmd.env("GIT_SSH_COMMAND", format!("{ssh_command} -oBatchMode=yes"));
        }
    }
    if let Some(dir) = current_dir {
        cmd.current_dir(dir);
    }
    cmd
}

/// Run a command to completion under a hard deadline, killing it on expiry.
///
/// Poll-based (`try_wait` + sleep) because this executes on a blocking
/// thread; `context` names the operation in the error message.
fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    context: &str,
) -> Result<std::process::ExitStatus, ParserInstallError> {
    let mut child = cmd
        .spawn()
        .map_err(|e| ParserInstallError::GitError(format!("{}: {}", context, e)))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ParserInstallError::GitError(format!(
                        "{} timed out after {:?}",
                        context, timeout
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                // try_wait failing is practically unreachable, but don't
                // leak a running child on that path either.
                let _ = child.kill();
                let _ = child.wait();
                return Err(ParserInstallError::GitError(format!("{}: {}", context, e)));
            }
        }
    }
}

/// Run a subprocess under a hard deadline, killing it on expiry.
///
/// Unlike [`run_with_timeout`], this is for a child that itself spawns children
/// (the parser compile re-execs this binary, which shells out to `cc` as a
/// *grandchild*). A bare `child.kill()` reaches only the direct child and leaves
/// the `cc` grandchild running — the very hang this deadline exists to bound. So
/// the child is spawned as its own **process-group leader** and the deadline kill
/// targets the whole **group**, terminating grandchildren too (the direct child is
/// reaped here; the terminated descendants are reaped by init after reparenting).
fn run_killable_subprocess(
    mut cmd: Command,
    timeout: Duration,
    context: &str,
) -> Result<std::process::ExitStatus, ParserInstallError> {
    // Spawn the child as its own process-group leader (pgid == child pid), so a
    // deadline kill can terminate the whole group (the `cc` the compile shells out
    // to) rather than orphaning grandchildren.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| ParserInstallError::CompileError(format!("{}: {}", context, e)))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_process_group(&mut child);
                    reap_bounded(&mut child);
                    return Err(ParserInstallError::CompileError(format!(
                        "{} timed out after {:?}",
                        context, timeout
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_process_group(&mut child);
                reap_bounded(&mut child);
                return Err(ParserInstallError::CompileError(format!(
                    "{}: {}",
                    context, e
                )));
            }
        }
    }
}

/// Reap the (already-killed) child, but only for a bounded time. After `SIGKILL`
/// a child dies at once, so this returns immediately in practice. The bound
/// matters only in the pathological case where the child can't be killed
/// (uninterruptible D-state, blocked signals, sandbox limits): rather than
/// `child.wait()` blocking forever — reintroducing the very unbounded wait this
/// deadline exists to prevent — we give up after the bound and leave a zombie.
fn reap_bounded(child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Terminate the child **and** every descendant it spawned. The child was started
/// as its own process-group leader, so its pgid equals its pid and a group-wide
/// `SIGKILL` reaches the whole tree (`cc`, linkers, etc.). This only *terminates*
/// descendants; the caller reaps the direct child via `child.wait()`, and the
/// terminated descendants are reparented to init and reaped there. On non-unix this
/// falls back to a direct `child.kill()`, which does **not** reach descendants —
/// the orphaned-`cc` hazard is unmitigated there (kakehashi targets unix in
/// practice; a Job Object would be the Windows equivalent).
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;
        // If the group signal can't be delivered (sandbox/OS limits), still make a
        // best effort to kill the direct child so it isn't left running.
        if killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL).is_err() {
            let _ = child.kill();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

/// Clone a git repository at a specific revision.
fn clone_repo(url: &str, revision: &str, dest: &Path) -> Result<(), ParserInstallError> {
    validate_parser_source_metadata(url, revision)?;

    // First, clone with depth 1 (we'll fetch the specific revision)
    let mut clone = git_command(&["clone", "--depth", "1", "--", url], None);
    clone.arg(dest);
    let status = run_with_timeout(clone, GIT_COMMAND_TIMEOUT, "git clone")?;

    if !status.success() {
        return Err(ParserInstallError::GitError(format!(
            "Failed to clone {}",
            url
        )));
    }

    // Fetch the specific revision
    let status = run_with_timeout(
        git_fetch_revision_command(revision, dest),
        GIT_COMMAND_TIMEOUT,
        "git fetch",
    )?;

    if !status.success() {
        return Err(ParserInstallError::GitError(format!(
            "Failed to fetch revision {}",
            revision
        )));
    }

    // Checkout FETCH_HEAD (the fetched revision)
    // Note: We use FETCH_HEAD instead of the revision directly because:
    // - For tags: `git fetch --depth 1 origin v0.25.0` puts the tag in FETCH_HEAD
    //   but doesn't create a local tag ref, so `git checkout v0.25.0` fails
    // - For commits: FETCH_HEAD also works correctly
    // - FETCH_HEAD always contains what we just fetched
    let status = run_with_timeout(
        git_command(&["checkout", "FETCH_HEAD"], Some(dest)),
        GIT_COMMAND_TIMEOUT,
        "git checkout",
    )?;

    if !status.success() {
        return Err(ParserInstallError::GitError(format!(
            "Failed to checkout revision {} (FETCH_HEAD)",
            revision
        )));
    }

    Ok(())
}

fn git_fetch_revision_command(revision: &str, dest: &Path) -> Command {
    git_command(
        &["fetch", "--depth", "1", "origin", "--", revision],
        Some(dest),
    )
}

fn validate_parser_source_metadata(url: &str, revision: &str) -> Result<(), ParserInstallError> {
    if url.starts_with('-') {
        return Err(invalid_metadata(format!(
            "unsafe parser repository URL '{}'",
            url.escape_default()
        )));
    }
    if !url.starts_with("https://") {
        return Err(invalid_metadata(format!(
            "parser repository URL must use the https:// scheme: '{}'",
            url.escape_default()
        )));
    }
    if url.chars().any(char::is_control) || url.contains("..") {
        return Err(invalid_metadata(format!(
            "unsafe parser repository URL '{}'",
            url.escape_default()
        )));
    }
    if revision.is_empty() || revision.chars().any(char::is_whitespace) {
        return Err(invalid_metadata(format!(
            "unsafe parser revision '{}'",
            revision.escape_default()
        )));
    }
    if revision.starts_with('-') {
        return Err(invalid_metadata(format!(
            "unsafe parser revision '{}'",
            revision.escape_default()
        )));
    }
    if revision.chars().any(char::is_control) || revision.contains("..") {
        return Err(invalid_metadata(format!(
            "unsafe parser revision '{}'",
            revision.escape_default()
        )));
    }

    Ok(())
}

fn parser_source_dir(
    clone_dir: &Path,
    location: Option<&str>,
) -> Result<PathBuf, ParserInstallError> {
    let Some(location) = location else {
        return Ok(clone_dir.canonicalize()?);
    };
    if location.is_empty()
        || Path::new(location)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_metadata(format!(
            "unsafe parser location '{}'",
            location.escape_default()
        )));
    }

    let clone_dir = clone_dir.canonicalize()?;
    let source_dir = clone_dir.join(location).canonicalize().map_err(|err| {
        invalid_metadata(format!(
            "unsafe parser location '{}': {}",
            location.escape_default(),
            err
        ))
    })?;
    if !source_dir.starts_with(&clone_dir) {
        return Err(invalid_metadata(format!(
            "unsafe parser location '{}'",
            location.escape_default()
        )));
    }
    if !source_dir.is_dir() {
        return Err(invalid_metadata(format!(
            "unsafe parser location '{}' is not a directory",
            location.escape_default()
        )));
    }

    Ok(source_dir)
}

fn reject_parser_source_symlinks(path: &Path) -> Result<(), ParserInstallError> {
    let root = path.join("src");
    let metadata = match fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(ParserInstallError::IoError(err)),
    };
    if metadata.file_type().is_symlink() {
        return Err(invalid_metadata(format!(
            "unsafe symlink in parser source '{}'",
            root.display().to_string().escape_default()
        )));
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(invalid_metadata(format!(
                    "unsafe symlink in parser source '{}'",
                    entry.path().display().to_string().escape_default()
                )));
            }
            if file_type.is_dir() && entry.file_name() != ".git" {
                pending.push(entry.path());
            }
        }
    }

    Ok(())
}

fn invalid_metadata(message: String) -> ParserInstallError {
    ParserInstallError::IoError(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use tempfile::tempdir;

    const TREE_SITTER_JSON_URL: &str =
        "https://github.com/tree-sitter/tree-sitter-json/archive/v0.24.8.tar.gz";

    #[derive(Default)]
    struct FakeParserFileOps {
        files: HashMap<PathBuf, &'static str>,
        failed_renames: Vec<(PathBuf, PathBuf)>,
        rollback_error_kind: Option<std::io::ErrorKind>,
        failed_removals: Vec<PathBuf>,
        vanished_removals: Vec<PathBuf>,
        backup_markers: HashSet<PathBuf>,
        fail_marker_creation: bool,
        fail_marker_write_after_create: bool,
        fail_marker_removal: bool,
        backup_claim_observed_marker: bool,
    }

    impl ParserFileOps for FakeParserFileOps {
        fn rename(&mut self, from: &Path, to: &Path) -> std::io::Result<()> {
            if to
                .extension()
                .is_some_and(|extension| extension == "backup")
            {
                self.backup_claim_observed_marker = self.backup_markers.contains(to);
            }
            if self
                .failed_renames
                .iter()
                .any(|(fail_from, fail_to)| fail_from == from && fail_to == to)
            {
                return Err(std::io::Error::new(
                    self.rollback_error_kind
                        .filter(|_| from.extension().is_some_and(|ext| ext == "backup"))
                        .unwrap_or(std::io::ErrorKind::PermissionDenied),
                    "injected publish failure",
                ));
            }
            let value = self.files.remove(from).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "missing source")
            })?;
            self.files.insert(to.to_path_buf(), value);
            Ok(())
        }

        fn remove_file(&mut self, path: &Path) -> std::io::Result<()> {
            if self
                .vanished_removals
                .iter()
                .any(|vanished| vanished == path)
            {
                self.files.remove(path);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "injected external removal",
                ));
            }
            if self.failed_removals.iter().any(|failed| failed == path) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected cleanup failure",
                ));
            }
            self.files
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "missing file"))
        }

        fn create_backup_marker(&mut self, backup: &Path) -> std::io::Result<()> {
            if self.files.contains_key(backup)
                || self.fail_marker_creation
                || !self.backup_markers.insert(backup.to_path_buf())
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "injected marker collision",
                ));
            }
            if self.fail_marker_write_after_create {
                self.backup_markers.remove(backup);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "injected marker write failure",
                ));
            }
            Ok(())
        }

        fn remove_backup_marker(&mut self, backup: &Path) -> std::io::Result<()> {
            if self.fail_marker_removal {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected marker cleanup failure",
                ));
            }
            self.backup_markers.remove(backup);
            Ok(())
        }
    }

    #[test]
    fn transactional_publish_restores_old_parser_when_publish_fails() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.failed_renames.push((tmp.clone(), parser.clone()));

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("injected publish failure must be reported");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(ops.files.get(&parser), Some(&"old"));
        assert!(!ops.files.contains_key(&backup));
    }

    #[test]
    fn transactional_publish_identifies_marker_left_after_rollback() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.failed_renames.push((tmp.clone(), parser.clone()));
        ops.fail_marker_removal = true;

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("marker cleanup failure must identify the leftover");

        assert!(
            error.to_string().contains("parser.backup.owner"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn transactional_publish_identifies_marker_left_after_success() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.fail_marker_removal = true;

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("marker cleanup failure must identify published state");

        assert!(error.to_string().contains("published parser"));
        assert!(error.to_string().contains("parser.backup.owner"));
    }

    #[test]
    fn transactional_publish_identifies_marker_left_after_failed_claim() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.failed_renames.push((parser.clone(), backup.clone()));
        ops.fail_marker_removal = true;

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("claim cleanup failure must identify marker");

        assert!(error.to_string().contains("parser.backup.owner"));
    }

    #[test]
    fn transactional_publish_removes_backup_after_success() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");

        publish_parser_transactionally(&mut ops, &tmp, &parser, &backup).expect("publish succeeds");

        assert_eq!(ops.files.get(&parser), Some(&"new"));
        assert!(!ops.files.contains_key(&tmp));
        assert!(!ops.files.contains_key(&backup));
        assert!(ops.backup_claim_observed_marker);
        assert!(!ops.backup_markers.contains(&backup));
    }

    #[test]
    fn transactional_publish_keeps_backup_when_rollback_fails() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.failed_renames.push((tmp.clone(), parser.clone()));
        ops.failed_renames.push((backup.clone(), parser.clone()));

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("publish and rollback failures must be reported");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("failed to restore backup"));
        assert_eq!(ops.files.get(&backup), Some(&"old"));
        assert_eq!(ops.files.get(&tmp), Some(&"new"));
        assert!(!ops.files.contains_key(&parser));
    }

    #[test]
    fn transactional_publish_reports_primary_error_kind_when_rollback_differs() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.failed_renames.push((tmp.clone(), parser.clone()));
        ops.failed_renames.push((backup.clone(), parser.clone()));
        ops.rollback_error_kind = Some(std::io::ErrorKind::NotFound);

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("publish and rollback failures must be reported");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn transactional_publish_preserves_both_copies_when_backup_cleanup_fails() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.failed_removals.push(backup.clone());

        let error = publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("backup cleanup failure must fail the transaction");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(ops.files.get(&parser), Some(&"new"));
        assert!(!ops.files.contains_key(&tmp));
        assert_eq!(ops.files.get(&backup), Some(&"old"));
    }

    #[test]
    fn transactional_publish_accepts_backup_that_vanished_during_cleanup() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.vanished_removals.push(backup.clone());

        publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect("a vanished backup is already cleaned up");

        assert_eq!(ops.files.get(&parser), Some(&"new"));
        assert!(!ops.files.contains_key(&backup));
    }

    #[test]
    fn transactional_publish_does_not_claim_colliding_backup() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.files.insert(backup.clone(), "user");
        ops.failed_renames.push((parser.clone(), backup.clone()));

        publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("backup collision must abort publication");

        assert_eq!(ops.files.get(&parser), Some(&"old"));
        assert_eq!(ops.files.get(&backup), Some(&"user"));
        assert!(!ops.backup_markers.contains(&backup));
    }

    #[test]
    fn transactional_publish_preserves_colliding_sidecar() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.backup_markers.insert(backup.clone());

        publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("sidecar collision must abort publication");

        assert_eq!(ops.files.get(&parser), Some(&"old"));
        assert!(!ops.files.contains_key(&backup));
        assert!(ops.backup_markers.contains(&backup));
    }

    #[test]
    fn transactional_publish_cleans_marker_after_marker_write_failure() {
        let tmp = PathBuf::from("parser.tmp");
        let parser = PathBuf::from("parser.dll");
        let backup = PathBuf::from("parser.backup");
        let mut ops = FakeParserFileOps::default();
        ops.files.insert(tmp.clone(), "new");
        ops.files.insert(parser.clone(), "old");
        ops.fail_marker_write_after_create = true;

        publish_parser_transactionally(&mut ops, &tmp, &parser, &backup)
            .expect_err("marker write failure must abort before moving parser");

        assert_eq!(ops.files.get(&parser), Some(&"old"));
        assert_eq!(ops.files.get(&tmp), Some(&"new"));
        assert!(!ops.files.contains_key(&backup));
        assert!(!ops.backup_markers.contains(&backup));
    }

    fn generated_backup_name(language: &str, pid: u32, counter: usize) -> String {
        format!(
            ".{language}.{pid}.{counter}.{}.backup",
            std::env::consts::DLL_EXTENSION
        )
    }

    #[test]
    fn parser_backup_matching_rejects_other_or_user_files() {
        let owned = generated_backup_name("lua", 123, 4);
        assert!(generated_parser_backup_matches_language(&owned, "lua"));
        assert!(!generated_parser_backup_matches_language(&owned, "rust"));
        assert!(!generated_parser_backup_matches_language(
            ".lua.manual.dylib.backup",
            "lua"
        ));
        assert!(!generated_parser_backup_matches_language(
            "lua.123.4.dylib.backup",
            "lua"
        ));
    }

    #[test]
    fn owned_parser_backup_languages_require_exact_marker_and_shape() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let owned = parser_dir.join(generated_backup_name("lua", 123, 4));
        let unowned = parser_dir.join(generated_backup_name("rust", 123, 5));
        let unsafe_name = parser_dir.join(generated_backup_name("Lua", 123, 6));
        for backup in [&owned, &unowned, &unsafe_name] {
            fs::write(backup, b"parser").expect("write backup");
        }
        fs::write(
            parser_backup_ownership_sidecar(&owned),
            PARSER_BACKUP_MARKER_CONTENT,
        )
        .expect("mark owned backup");
        fs::write(parser_backup_ownership_sidecar(&unowned), b"user marker")
            .expect("write unowned marker");

        assert_eq!(
            owned_parser_backup_languages(&parser_dir).expect("discover backups"),
            vec!["lua"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_parser_backup_languages_ignores_marker_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        let marker = parser_backup_ownership_sidecar(&backup);
        fs::write(&backup, b"parser").expect("write backup");
        symlink(&marker, &marker).expect("create marker symlink loop");

        assert!(
            owned_parser_backup_languages(&parser_dir)
                .expect("symlink marker is unowned")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_parser_backup_languages_ignores_marker_fifo() {
        use nix::sys::stat::Mode;
        use nix::unistd::mkfifo;
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        let marker = parser_backup_ownership_sidecar(&backup);
        fs::write(&backup, b"parser").expect("write backup");
        mkfifo(&marker, Mode::S_IRUSR | Mode::S_IWUSR).expect("create marker fifo");

        assert!(
            owned_parser_backup_languages(&parser_dir)
                .expect("fifo marker is unowned")
                .is_empty()
        );
    }

    #[test]
    fn parser_uninstall_removes_owned_backups_only() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let owned = parser_dir.join(generated_backup_name("lua", 123, 4));
        let unowned_exact_shape = parser_dir.join(generated_backup_name("lua", 123, 5));
        let other_language = parser_dir.join(generated_backup_name("rust", 123, 4));
        let user_file = parser_dir.join(".lua.manual.backup");
        for path in [
            &canonical,
            &owned,
            &unowned_exact_shape,
            &other_language,
            &user_file,
        ] {
            fs::write(path, b"parser").expect("write parser fixture");
        }
        fs::write(
            parser_backup_ownership_sidecar(&owned),
            PARSER_BACKUP_MARKER_CONTENT,
        )
        .expect("mark owned backup");

        assert!(
            remove_parser_install_and_backups(&parser_dir, "lua").expect("remove parser files")
        );

        assert!(!canonical.exists());
        assert!(!owned.exists());
        assert!(unowned_exact_shape.exists());
        assert!(other_language.exists());
        assert!(user_file.exists());
    }

    #[test]
    fn parser_uninstall_does_not_create_missing_parser_directory() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");

        assert!(
            !remove_parser_install_and_backups(&parser_dir, "lua")
                .expect("missing parser install is a no-op")
        );
        assert!(!parser_dir.exists());
    }

    #[test]
    fn parser_recovery_restores_backup_when_canonical_is_missing() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        fs::write(&backup, b"working parser").expect("write backup");
        fs::write(
            parser_backup_ownership_sidecar(&backup),
            PARSER_BACKUP_MARKER_CONTENT,
        )
        .expect("mark owned backup");

        assert!(
            recover_parser_backups(&parser_dir, &canonical, "lua").expect("recover parser backup")
        );

        assert_eq!(
            fs::read(&canonical).expect("read restored parser"),
            b"working parser"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn parser_recovery_removes_obsolete_backup_when_canonical_exists() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        fs::write(&canonical, b"new parser").expect("write canonical");
        fs::write(&backup, b"old parser").expect("write backup");
        fs::write(
            parser_backup_ownership_sidecar(&backup),
            PARSER_BACKUP_MARKER_CONTENT,
        )
        .expect("mark owned backup");

        assert!(
            !recover_parser_backups(&parser_dir, &canonical, "lua").expect("clean parser backup")
        );

        assert_eq!(fs::read(&canonical).expect("read canonical"), b"new parser");
        assert!(!backup.exists());
    }

    #[test]
    fn parser_recovery_ignores_exact_shape_backup_without_ownership_marker() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let unowned = parser_dir.join(generated_backup_name("lua", 123, 4));
        fs::write(&unowned, b"user file").expect("write user file");

        recover_parser_backups(&parser_dir, &canonical, "lua").expect("scan parser backups");

        assert!(!canonical.exists());
        assert_eq!(fs::read(unowned).expect("read user file"), b"user file");
    }

    #[test]
    fn parser_backup_marker_removal_accepts_concurrent_disappearance() {
        let temp = tempdir().expect("temp dir");
        let backup = temp.path().join(generated_backup_name("lua", 123, 4));

        remove_parser_backup_marker(&backup).expect("already removed marker is clean");
    }

    #[test]
    fn parser_recovery_cleans_owned_orphan_marker_only() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let owned_backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        let owned_marker = parser_backup_ownership_sidecar(&owned_backup);
        let user_backup = parser_dir.join(generated_backup_name("lua", 123, 5));
        let user_marker = parser_backup_ownership_sidecar(&user_backup);
        let staged_marker = parser_backup_ownership_sidecar(&owned_backup)
            .with_extension(format!("owner.{}.tmp", ulid::Ulid::new()));
        let malformed_staging =
            parser_backup_ownership_sidecar(&owned_backup).with_extension("owner.not-a-ulid.tmp");
        let colliding_backup = parser_dir.join(generated_backup_name("lua", 123, 6));
        let colliding_staging = parser_backup_ownership_sidecar(&colliding_backup)
            .with_extension(format!("owner.{}.tmp", ulid::Ulid::new()));
        fs::write(&owned_marker, PARSER_BACKUP_MARKER_CONTENT).expect("write owned marker");
        fs::write(&user_marker, b"user marker").expect("write user marker");
        fs::write(&staged_marker, PARSER_BACKUP_MARKER_CONTENT).expect("write staged marker");
        fs::write(&malformed_staging, PARSER_BACKUP_MARKER_CONTENT)
            .expect("write malformed staging marker");
        fs::write(&colliding_backup, b"user backup").expect("write colliding backup");
        fs::write(&colliding_staging, PARSER_BACKUP_MARKER_CONTENT)
            .expect("write colliding staging marker");

        recover_parser_backups(&parser_dir, &canonical, "lua").expect("clean orphan markers");

        assert!(!owned_marker.exists());
        assert!(!staged_marker.exists());
        assert!(!colliding_staging.exists());
        assert!(colliding_backup.exists());
        assert!(malformed_staging.exists());
        assert!(user_marker.exists());
        assert!(!canonical.exists());
    }

    #[cfg(unix)]
    #[test]
    fn parser_recovery_ignores_ownership_marker_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        let marker = parser_backup_ownership_sidecar(&backup);
        fs::write(&backup, b"working parser").expect("write backup");
        symlink(&marker, &marker).expect("create marker symlink loop");

        assert!(
            !recover_parser_backups(&parser_dir, &canonical, "lua")
                .expect("symlink marker is unowned")
        );

        assert!(backup.exists());
        assert!(!canonical.exists());
    }

    #[cfg(unix)]
    #[test]
    fn parser_recovery_propagates_canonical_metadata_error() {
        use std::os::unix::fs::symlink;
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let canonical = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let backup = parser_dir.join(generated_backup_name("lua", 123, 4));
        fs::write(&backup, b"working parser").expect("write backup");
        fs::write(
            parser_backup_ownership_sidecar(&backup),
            PARSER_BACKUP_MARKER_CONTENT,
        )
        .expect("mark owned backup");
        symlink(&canonical, &canonical).expect("create canonical symlink loop");

        recover_parser_backups(&parser_dir, &canonical, "lua")
            .expect_err("canonical metadata error must abort recovery");

        assert!(backup.exists());
    }

    #[test]
    fn install_parser_rejects_unsafe_language_name() {
        let temp = tempdir().expect("temp dir");
        let options = InstallOptions {
            data_dir: temp.path().to_path_buf(),
            force: false,
            verbose: false,
            no_cache: false,
            compile: ParserCompile::InProcess,
        };
        // A traversal-capable name must be rejected before any filesystem or
        // network work — `install_parser` is a public entry point.
        match install_parser("../../evil", &options) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[test]
    fn git_command_pins_https_transport() {
        let cmd = git_command(&["clone", "https://example.invalid/parser"], None);
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            &args[..7],
            [
                "-c",
                "http.followRedirects=false",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.https.allow=always",
                "clone",
            ]
        );
    }

    #[test]
    fn git_fetch_revision_command_uses_revision_as_refspec() {
        let temp = tempdir().expect("temp dir");
        let cmd = git_fetch_revision_command("v0.24.8", temp.path());
        let args = cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "-c",
                "http.followRedirects=false",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.https.allow=always",
                "fetch",
                "--depth",
                "1",
                "origin",
                "--",
                "v0.24.8",
            ]
        );
    }

    #[test]
    fn clone_repo_rejects_option_like_url() {
        let temp = tempdir().expect("temp dir");
        let dest = temp.path().join("parser");

        match clone_repo("--upload-pack=sh", "main", &dest) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[test]
    fn clone_repo_rejects_non_https_url() {
        let temp = tempdir().expect("temp dir");
        let dest = temp.path().join("parser");

        match clone_repo("file:///tmp/parser", "main", &dest) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[test]
    fn clone_repo_rejects_option_like_revision() {
        let temp = tempdir().expect("temp dir");
        let dest = temp.path().join("parser");

        match clone_repo("https://example.invalid/parser", "--upload-pack=sh", &dest) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[test]
    fn clone_repo_rejects_empty_or_whitespace_revision() {
        let temp = tempdir().expect("temp dir");
        let dest = temp.path().join("parser");

        for revision in ["", " ", "main branch"] {
            match clone_repo("https://example.invalid/parser", revision, &dest) {
                Err(ParserInstallError::IoError(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                }
                other => panic!(
                    "expected an InvalidInput IoError for revision {revision:?}, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn clone_repo_rejects_control_characters() {
        let temp = tempdir().expect("temp dir");
        let dest = temp.path().join("parser");

        for (url, revision) in [
            ("https://example.invalid/parser\nnext", "main"),
            ("https://example.invalid/parser", "main\u{1b}[31m"),
        ] {
            match clone_repo(url, revision, &dest) {
                Err(ParserInstallError::IoError(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                }
                other => panic!(
                    "expected an InvalidInput IoError for {url:?} {revision:?}, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn fetch_source_rejects_unsafe_archive_metadata() {
        let temp = tempdir().expect("temp dir");
        for (url, revision) in [
            (
                "https://github.com/tree-sitter/tree-sitter-json",
                "main\nnext",
            ),
            (
                "https://github.com/tree-sitter/tree-sitter-json",
                "../../other/archive/main",
            ),
            ("https://github.com/tree-sitter/../other", "main"),
        ] {
            match fetch_source(url, revision, &temp.path().join("parser")) {
                Err(ParserInstallError::IoError(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                }
                other => panic!(
                    "expected an InvalidInput IoError for {url:?} {revision:?}, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn parser_source_dir_rejects_unsafe_location() {
        let temp = tempdir().expect("temp dir");
        for location in ["", "/tmp/parser", "../parser", "parser/../other"] {
            match parser_source_dir(temp.path(), Some(location)) {
                Err(ParserInstallError::IoError(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                }
                other => {
                    panic!("expected InvalidInput for location {location:?}, got {other:?}")
                }
            }
        }
    }

    #[test]
    fn parser_source_dir_accepts_safe_relative_location() {
        let temp = tempdir().expect("temp dir");
        fs::create_dir_all(temp.path().join("typescript/common")).expect("create location");
        assert_eq!(
            parser_source_dir(temp.path(), Some("typescript/common")).expect("safe location"),
            temp.path()
                .join("typescript/common")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            parser_source_dir(temp.path(), None).expect("missing location uses clone dir"),
            temp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn parser_source_dir_rejects_missing_or_file_location() {
        let temp = tempdir().expect("temp dir");
        fs::write(temp.path().join("parser.c"), "void parser(void) {}").expect("write file");

        for location in ["missing", "parser.c"] {
            match parser_source_dir(temp.path(), Some(location)) {
                Err(ParserInstallError::IoError(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                }
                other => {
                    panic!("expected InvalidInput for location {location:?}, got {other:?}")
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn parser_source_dir_rejects_symlink_location_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let clone_dir = temp.path().join("clone");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&clone_dir).expect("create clone dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        symlink(&outside, clone_dir.join("grammar")).expect("create symlink");

        match parser_source_dir(&clone_dir, Some("grammar")) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn reject_parser_source_symlinks_rejects_nested_source_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let source_dir = temp.path().join("grammar");
        let outside = temp.path().join("outside.c");
        fs::create_dir_all(source_dir.join("src")).expect("create source src dir");
        fs::write(&outside, "void tree_sitter_evil(void) {}").expect("write outside file");
        symlink(&outside, source_dir.join("src/parser\n.c")).expect("create nested symlink");

        match reject_parser_source_symlinks(&source_dir) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
                assert!(
                    e.to_string().contains("\\n"),
                    "symlink path should escape control characters: {e}"
                );
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn reject_parser_source_symlinks_allows_symlinks_outside_compile_source() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let source_dir = temp.path().join("grammar");
        let outside = temp.path().join("outside");
        fs::create_dir_all(source_dir.join("bindings/go")).expect("create binding dir");
        fs::create_dir_all(source_dir.join("scripts/dummy_plugin/queries"))
            .expect("create script dir");
        fs::create_dir_all(source_dir.join("src")).expect("create parser src dir");
        fs::write(&outside, "outside").expect("write outside file");
        symlink(&outside, source_dir.join("bindings/go/scanner.c"))
            .expect("create binding symlink");
        symlink(&outside, source_dir.join("scripts/dummy_plugin/queries/bp"))
            .expect("create script symlink");

        reject_parser_source_symlinks(&source_dir)
            .expect("non-compiled support files are not parser source");
    }

    #[cfg(unix)]
    #[test]
    fn reject_parser_source_symlinks_rejects_nested_dot_git_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let source_dir = temp.path().join("grammar");
        let outside = temp.path().join("outside");
        fs::create_dir_all(source_dir.join("src")).expect("create source src dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        symlink(&outside, source_dir.join("src/.git")).expect("create nested .git symlink");

        match reject_parser_source_symlinks(&source_dir) {
            Err(ParserInstallError::IoError(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
            }
            other => panic!("expected an InvalidInput IoError, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn reject_parser_source_symlinks_skips_real_dot_git_directories() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let source_dir = temp.path().join("grammar");
        let outside = temp.path().join("outside");
        fs::create_dir_all(source_dir.join("src/.git/objects")).expect("create nested .git dir");
        fs::create_dir_all(&outside).expect("create outside dir");
        symlink(&outside, source_dir.join("src/.git/objects/link"))
            .expect("create ignored git metadata symlink");

        reject_parser_source_symlinks(&source_dir)
            .expect("real .git directories are ignored after their own type check");
    }

    #[test]
    fn test_dll_extension_is_valid() {
        let ext = std::env::consts::DLL_EXTENSION;
        assert!(ext == "so" || ext == "dylib" || ext == "dll");
    }

    // Unix-only: relies on the `sleep` binary.
    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_stuck_command() {
        // Spawn `sleep` directly: with `sh -c` the kill would reach the
        // shell, and reaching `sleep` would depend on the shell exec'ing
        // single commands.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");

        let started = std::time::Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(200), "test sleep");

        match result {
            Err(ParserInstallError::GitError(message)) => {
                assert!(
                    message.contains("timed out"),
                    "expected timeout error, got: {message}"
                );
            }
            other => panic!("expected GitError timeout, got {other:?}"),
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stuck child must be killed promptly, not waited out"
        );
    }

    /// A deadline kill must terminate the whole process **group**, not just the
    /// direct child. The parser compiler shells out to `cc` as a *grandchild*; a
    /// bare `child.kill()` would leave it running — the hang this deadline exists
    /// to bound. We model it with `sh` (the group leader) backgrounding a
    /// grandchild that, *if it survives*, creates a marker file 2s out. The
    /// deadline fires at 700ms, so a group kill must stop the grandchild before it
    /// can touch the marker. Observing a side effect only a survivor produces is
    /// robust against orphan-reaping timing and PID reuse (a bare child.kill()
    /// would orphan the grandchild, which then creates the marker → RED).
    #[cfg(unix)]
    #[test]
    fn run_killable_subprocess_terminates_descendants_on_deadline() {
        let tmp = tempdir().expect("temp dir");
        let survived = tmp.path().join("grandchild-survived");
        let spawned = tmp.path().join("grandchild-spawned");
        let mut cmd = Command::new("sh");
        // Background a grandchild that touches `survived` 2s out; `spawned` is
        // touched right after launching it (proving the descendant was actually
        // started, so an absent `survived` can't be a vacuous pass from sh dying
        // before it forked the child). Paths are passed as positional params
        // ($1/$2), not interpolated, so a temp dir containing a quote can't break
        // the script.
        cmd.arg("-c")
            .arg(r#"( sleep 2; touch "$1" ) & touch "$2"; wait"#)
            .arg("sh")
            .arg(&survived)
            .arg(&spawned);

        let started = std::time::Instant::now();
        let result = run_killable_subprocess(cmd, Duration::from_millis(700), "test compile");
        assert!(
            result.is_err(),
            "a subprocess that outlives the deadline must error out"
        );
        assert!(
            spawned.exists(),
            "the grandchild must have been spawned (else the test would pass vacuously)"
        );

        // Wait past the grandchild's would-be touch time (2s), then assert it
        // never ran — its group was killed at 700ms.
        while started.elapsed() < Duration::from_millis(2500) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !survived.exists(),
            "the backgrounded grandchild created its marker, so it survived the \
             deadline — it must be terminated with the process group, not orphaned"
        );
    }

    // Unix-only: relies on `sh`.
    #[cfg(unix)]
    #[test]
    fn run_with_timeout_returns_status_of_finished_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 0"]);

        let status = run_with_timeout(cmd, Duration::from_secs(10), "test exit")
            .expect("fast command should complete within the deadline");
        assert!(status.success());
    }

    #[test]
    fn test_parser_file_exists_returns_none_when_missing() {
        let temp = tempdir().expect("Failed to create temp dir");
        let result = parser_file_exists("nonexistent", temp.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_parser_file_exists_returns_some_when_present() {
        let temp = tempdir().expect("Failed to create temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");

        // Create a fake parser file
        let ext = std::env::consts::DLL_EXTENSION;
        let parser_file = parser_dir.join(format!("lua.{}", ext));
        fs::write(&parser_file, b"fake parser").expect("write fake parser");

        let result = parser_file_exists("lua", temp.path());
        assert!(result.is_some());
        assert_eq!(result.unwrap(), parser_file);
    }

    #[test]
    fn an_already_installed_parser_does_not_supersede_an_inflight_force_install() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let parser_file = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        fs::write(&parser_file, b"installed parser").expect("write installed parser");
        let first_token = begin_parser_install(&parser_dir, &parser_file, "lua", true)
            .expect("start force install");
        let options = InstallOptions {
            data_dir: temp.path().to_path_buf(),
            force: false,
            verbose: false,
            no_cache: false,
            compile: ParserCompile::InProcess,
        };

        let result = install_parser("lua", &options);

        assert!(
            matches!(result, Err(ParserInstallError::AlreadyExists(path)) if path == parser_file)
        );
        assert_eq!(
            fs::read_to_string(parser_operation_marker_path(&parser_dir, "lua")).unwrap(),
            first_token,
            "a no-op install must not cancel the in-flight force install"
        );
    }

    #[test]
    fn uninstall_marker_prevents_staged_parser_publication() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let parser_file = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let tmp_file = parser_dir.join(".lua.staged.tmp");
        fs::write(&tmp_file, b"compiled parser").expect("write staged parser");
        fs::write(parser_operation_marker_path(&parser_dir, "lua"), b"ok\n")
            .expect("record later uninstall");

        let result = publish_compiled_parser(&tmp_file, &parser_file, "lua", "install:first");

        assert!(
            matches!(
                result,
                Err(ParserInstallError::IoError(ref error))
                    if error.kind() == std::io::ErrorKind::Interrupted
            ),
            "a later uninstall must supersede staged publication: {result:?}"
        );
        assert!(!parser_file.exists(), "the parser must remain uninstalled");
        assert!(!tmp_file.exists(), "the superseded staging file is cleaned");
    }

    #[test]
    fn removing_a_missing_parser_still_supersedes_an_inflight_install() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        let parser_file = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let tmp_file = parser_dir.join(".lua.staged.tmp");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        fs::write(&tmp_file, b"compiled parser").expect("write staged parser");
        let install_token =
            begin_parser_install(&parser_dir, &parser_file, "lua", true).expect("start install");

        assert!(!remove_parser_install(&parser_dir, "lua").expect("uninstall succeeds"));
        let publish = publish_compiled_parser(&tmp_file, &parser_file, "lua", &install_token);

        assert!(
            matches!(
                publish,
                Err(ParserInstallError::IoError(ref error))
                    if error.kind() == std::io::ErrorKind::Interrupted
            ),
            "uninstall must win even when no old parser existed: {publish:?}"
        );
        assert!(!parser_file.exists(), "the parser must remain absent");
    }

    #[test]
    fn a_later_install_replaces_the_uninstall_marker_with_its_token() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        let parser_file = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let tmp_file = parser_dir.join(".lua.staged.tmp");
        remove_parser_install(&parser_dir, "lua").expect("record uninstall");
        let install_token = begin_parser_install(&parser_dir, &parser_file, "lua", true)
            .expect("start later install");
        fs::write(&tmp_file, b"compiled parser").expect("write staged parser");

        publish_compiled_parser(&tmp_file, &parser_file, "lua", &install_token)
            .expect("later install may publish");

        assert_eq!(fs::read(parser_file).unwrap(), b"compiled parser");
        assert_eq!(
            fs::read_to_string(parser_operation_marker_path(&parser_dir, "lua")).unwrap(),
            install_token
        );
    }

    #[test]
    fn a_later_install_does_not_authorize_an_older_staged_parser() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        let parser_file = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        let first_token = begin_parser_install(&parser_dir, &parser_file, "lua", true)
            .expect("start first install");
        remove_parser_install(&parser_dir, "lua").expect("superseding uninstall");
        begin_parser_install(&parser_dir, &parser_file, "lua", true).expect("start second install");
        let first_tmp_file = parser_dir.join(".lua.first.staged.tmp");
        fs::write(&first_tmp_file, b"first parser").expect("stage first parser");

        let first_publish =
            publish_compiled_parser(&first_tmp_file, &parser_file, "lua", &first_token);

        assert!(
            matches!(
                first_publish,
                Err(ParserInstallError::IoError(ref error))
                    if error.kind() == std::io::ErrorKind::Interrupted
            ),
            "the second install must not clear the first install's supersession: {first_publish:?}"
        );
        assert!(!parser_file.exists(), "the stale parser must not publish");
    }

    #[test]
    fn test_github_archive_url_from_standard_repo() {
        let url = github_archive_url("https://github.com/tree-sitter/tree-sitter-json", "v0.24.8");
        assert_eq!(url, Some(TREE_SITTER_JSON_URL.to_string()));
    }

    #[test]
    fn test_github_archive_url_from_commit_hash() {
        let url = github_archive_url(
            "https://github.com/tree-sitter/tree-sitter-json",
            "abc123def456",
        );
        assert_eq!(
            url,
            Some(
                "https://github.com/tree-sitter/tree-sitter-json/archive/abc123def456.tar.gz"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_github_archive_url_strips_dot_git_suffix() {
        let url = github_archive_url(
            "https://github.com/tree-sitter/tree-sitter-json.git",
            "v0.24.8",
        );
        assert_eq!(url, Some(TREE_SITTER_JSON_URL.to_string()));
    }

    #[test]
    fn test_github_archive_url_returns_none_for_non_github() {
        let url = github_archive_url("https://gitlab.com/foo/bar", "v1.0.0");
        assert!(url.is_none());
    }

    #[test]
    fn test_github_archive_url_returns_none_for_ssh() {
        let url = github_archive_url("git@github.com:tree-sitter/tree-sitter-json.git", "v1.0.0");
        assert!(url.is_none());
    }

    #[test]
    fn test_github_archive_url_strips_trailing_slash() {
        let url = github_archive_url(
            "https://github.com/tree-sitter/tree-sitter-json/",
            "v0.24.8",
        );
        assert_eq!(url, Some(TREE_SITTER_JSON_URL.to_string()));
    }

    #[test]
    fn test_repo_name_from_url() {
        assert_eq!(
            repo_name_from_url("https://github.com/tree-sitter/tree-sitter-json"),
            Some("tree-sitter-json".to_string())
        );
        assert_eq!(
            repo_name_from_url("https://github.com/tree-sitter/tree-sitter-json.git"),
            Some("tree-sitter-json".to_string())
        );
        assert_eq!(
            repo_name_from_url("https://github.com/tree-sitter-grammars/tree-sitter-markdown"),
            Some("tree-sitter-markdown".to_string())
        );
        assert_eq!(
            repo_name_from_url("https://github.com/tree-sitter/tree-sitter-json/"),
            Some("tree-sitter-json".to_string())
        );
    }

    #[test]
    fn test_archive_root_dir_strips_v_prefix_from_tag() {
        assert_eq!(
            archive_root_dir_name("tree-sitter-json", "v0.24.8"),
            "tree-sitter-json-0.24.8"
        );
    }

    #[test]
    fn test_archive_root_dir_keeps_commit_hash_as_is() {
        assert_eq!(
            archive_root_dir_name("tree-sitter-json", "abc123def456"),
            "tree-sitter-json-abc123def456"
        );
    }

    #[test]
    fn test_archive_root_dir_with_tag_without_v_prefix() {
        assert_eq!(
            archive_root_dir_name("tree-sitter-json", "0.24.8"),
            "tree-sitter-json-0.24.8"
        );
    }

    /// Test that download_and_extract_archive downloads and extracts a GitHub archive.
    #[test]
    fn test_download_and_extract_archive_for_json_parser() {
        let temp = tempdir().expect("Failed to create temp dir");
        let dest = temp.path().join("source");

        let result = download_and_extract_archive(
            "https://github.com/tree-sitter/tree-sitter-json/archive/v0.24.8.tar.gz",
            "tree-sitter-json",
            "v0.24.8",
            &dest,
        );

        assert!(
            result.is_ok(),
            "download_and_extract_archive should succeed: {:?}",
            result.err()
        );

        // The source directory should contain parser source files
        assert!(
            dest.join("src").join("parser.c").exists(),
            "Extracted archive should contain src/parser.c"
        );
    }

    /// Test that fetch_source succeeds for a GitHub URL (uses archive download).
    #[test]
    fn test_fetch_source_downloads_source_with_parser_c() {
        let temp = tempdir().expect("Failed to create temp dir");
        let dest = temp.path().join("source");

        let result = fetch_source(
            "https://github.com/tree-sitter/tree-sitter-json",
            "v0.24.8",
            &dest,
        );

        assert!(
            result.is_ok(),
            "fetch_source should succeed: {:?}",
            result.err()
        );
        assert!(
            dest.join("src").join("parser.c").exists(),
            "Source should contain src/parser.c"
        );
    }

    /// clone_repo works with tag revisions (e.g., v0.25.0).
    #[test]
    fn test_clone_repo_with_tag_revision() {
        let temp = tempdir().expect("Failed to create temp dir");
        let dest = temp.path().join("tree-sitter-python");

        let result = clone_repo(
            "https://github.com/tree-sitter/tree-sitter-python",
            "v0.23.5",
            &dest,
        );

        assert!(
            result.is_ok(),
            "clone_repo should succeed with tag revision: {:?}",
            result.err()
        );

        assert!(dest.exists(), "Clone directory should exist");
        assert!(dest.join(".git").exists(), "Should be a git repository");
    }

    /// Test that compile_parser compiles a parser from source to a shared library.
    #[test]
    fn test_compile_parser_with_loader() {
        let temp = tempdir().expect("Failed to create temp dir");
        let clone_dir = temp.path().join("tree-sitter-json");

        // Use fetch_source (which uses archive download) instead of clone_repo
        fetch_source(
            "https://github.com/tree-sitter/tree-sitter-json",
            "v0.24.8",
            &clone_dir,
        )
        .expect("fetch should succeed");

        let output_path = temp
            .path()
            .join(format!("json.{}", std::env::consts::DLL_EXTENSION));

        // Exercise the in-process loader call directly: the killable subprocess
        // path (`compile_parser`) re-execs the kakehashi binary's
        // `__compile-parser` subcommand, which is not present in the unit-test
        // harness binary. End-to-end subprocess wiring is covered by
        // `tests/test_compile_parser_subprocess.rs`.
        compile_parser_inprocess(&clone_dir, &output_path).expect("compile should succeed");

        assert!(
            output_path.exists(),
            "Compiled parser should exist at {}",
            output_path.display()
        );
    }

    /// Test that clone_repo works with commit hash revisions
    #[test]
    fn test_clone_repo_with_commit_hash() {
        let temp = tempdir().expect("Failed to create temp dir");
        let dest = temp.path().join("tree-sitter-json");

        let result = clone_repo(
            "https://github.com/tree-sitter/tree-sitter-json",
            "v0.24.8",
            &dest,
        );

        assert!(
            result.is_ok(),
            "clone_repo should succeed with revision: {:?}",
            result.err()
        );

        assert!(dest.exists(), "Clone directory should exist");
    }
}
