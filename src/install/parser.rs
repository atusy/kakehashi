//! Parser compilation and installation.
//!
//! This module handles fetching parser source (via HTTP archive or git clone),
//! compiling them with tree-sitter-loader, and installing the resulting shared library.

use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use flate2::read::GzDecoder;
#[cfg(unix)]
use fs4::fs_std::FileExt;
use tar::Archive;
use tree_sitter_loader::Loader;

use super::http::agent_with_timeout;
use super::metadata::{FetchOptions, MetadataError, fetch_parser_metadata};

/// HTTP timeout for archive downloads.
const ARCHIVE_HTTP_TIMEOUT: Duration = Duration::from_secs(120);
static PARSER_TMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
    #[cfg(windows)]
    let _staging_guard = open_windows_staging_guard(output_path)?;
    let loader = Loader::with_parser_lib_path(parent_dir.to_path_buf());
    loader
        .compile_parser_at_path(grammar_path, output_path.to_path_buf(), &[])
        .map_err(|e| ParserInstallError::CompileError(e.to_string()))
}

#[cfg(windows)]
fn open_windows_staging_guard(path: &Path) -> Result<fs::File, ParserInstallError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    fs::OpenOptions::new()
        .read(true)
        .write(true)
        // Excluding FILE_SHARE_DELETE makes rename/delete fail while the
        // re-exec child or its in-process compile is still using this path.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .map_err(ParserInstallError::IoError)
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
fn compile_parser(
    grammar_path: &Path,
    output_path: &Path,
    staging_lock: Option<&fs::File>,
) -> Result<(), ParserInstallError> {
    let mut cmd = Command::new(self_exe_for_reexec()?);
    cmd.arg("__compile-parser")
        .arg(grammar_path)
        .arg(output_path)
        // Null stdin/stdout: when the parent is the LSP server, stdout is the
        // JSON-RPC channel and no child output may reach it. stderr stays
        // inherited so `cc` diagnostics land in the logs.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    if let Some(staging_lock) = staging_lock {
        inherit_staging_lock(&mut cmd, staging_lock);
    }
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

#[cfg(unix)]
fn inherit_staging_lock(cmd: &mut Command, staging_lock: &fs::File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let fd = staging_lock.as_raw_fd();
    // Only the forked child clears CLOEXEC on its copy. The descriptor then
    // keeps the advisory lock alive through our re-exec and the compiler it
    // launches if the installer process exits unexpectedly.
    unsafe {
        cmd.pre_exec(move || {
            let flags = nix::libc::fcntl(fd, nix::libc::F_GETFD);
            if flags == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::fcntl(fd, nix::libc::F_SETFD, flags & !nix::libc::FD_CLOEXEC) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn inherit_staging_lock(_cmd: &mut Command, _staging_lock: &fs::File) {}

/// Install a Tree-sitter parser for a language.
pub fn install_parser(
    language: &str,
    options: &InstallOptions,
) -> Result<ParserInstallResult, ParserInstallError> {
    // `language` becomes path segments (`parser/<language>.<ext>` and the temp
    // file) and a URL/metadata key, so reject traversal-capable names before
    // touching the filesystem. Higher-level callers (auto-install) already gate
    // this, but `install_parser` is a public entry point and must be safe on its
    // own — a name like `../../evil` would otherwise write outside the data dir.
    if !super::queries::is_safe_language_name(language) {
        return Err(ParserInstallError::IoError(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe language name '{}'", language.escape_default()),
        )));
    }

    let parser_dir = options.data_dir.join("parser");
    let parser_file = parser_dir.join(format!("{}.{}", language, std::env::consts::DLL_EXTENSION));

    fs::create_dir_all(&parser_dir)?;
    cleanup_interrupted_parser_installs(&parser_dir)?;

    // Check if parser already exists
    if parser_file.exists() && !options.force {
        return Err(ParserInstallError::AlreadyExists(parser_file));
    }

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
    let (tmp_file, tmp_lock) = reserve_parser_staging_file(&parser_dir, language)?;
    let compiled = match options.compile {
        ParserCompile::KillableSubprocess => {
            compile_parser(&source_dir, &tmp_file, tmp_lock.as_ref())
        }
        ParserCompile::InProcess => compile_parser_inprocess(&source_dir, &tmp_file),
    };
    // On Windows this is the parent's no-delete-sharing staging handle. The
    // compiler has returned (and its overlapping child handle is closed), so
    // release ours before the final rename or error-path removal.
    drop(tmp_lock);
    match compiled {
        Ok(()) => {
            // On unix `rename` atomically replaces an existing parser. Windows
            // `rename` instead fails if the destination exists, which would make a
            // `force` reinstall fail after a good compile — remove the old file
            // first there (a small non-atomic window, acceptable on the
            // non-primary platform).
            #[cfg(windows)]
            let _ = fs::remove_file(&parser_file);
            if let Err(e) = fs::rename(&tmp_file, &parser_file) {
                let _ = fs::remove_file(&tmp_file);
                return Err(ParserInstallError::IoError(e));
            }
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_file);
            return Err(e);
        }
    }

    if options.verbose {
        eprintln!("Installed to: {}", parser_file.display());
    }

    Ok(ParserInstallResult {
        language: language.to_string(),
        install_path: parser_file,
        revision: metadata.revision,
    })
}

fn reserve_parser_staging_file(
    parser_dir: &Path,
    language: &str,
) -> Result<(PathBuf, Option<fs::File>), ParserInstallError> {
    loop {
        let candidate = parser_dir.join(format!(
            ".{}.{}.{}.{}.tmp",
            language,
            std::process::id(),
            PARSER_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::env::consts::DLL_EXTENSION
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
            options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
        }
        match options.open(&candidate) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    return match file.lock_exclusive() {
                        Ok(()) => Ok((candidate, Some(file))),
                        // Locking is an ownership optimization for cleanup,
                        // not a prerequisite for compilation. On filesystems
                        // without advisory locks, cleanup also declines to
                        // claim unlocked artifacts conservatively.
                        Err(_) => Ok((candidate, None)),
                    };
                }
                #[cfg(windows)]
                {
                    // The parent retains a no-delete-sharing handle until the
                    // re-exec child has opened its overlapping guard. This
                    // closes the parent-exit/child-start cleanup race.
                    return Ok((candidate, Some(file)));
                }
                #[cfg(not(any(unix, windows)))]
                {
                    drop(file);
                    return Ok((candidate, None));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ParserInstallError::IoError(error)),
        }
    }
}

fn staged_parser_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let mut parts = rest.split('.');
    let language = parts.next()?;
    let pid = parts.next()?;
    let counter = parts.next()?;
    let extension = parts.next()?;
    if parts.next().is_some()
        || !super::queries::is_safe_language_name(language)
        || !is_canonical_decimal(pid)
        || !is_canonical_decimal(counter)
        || counter.parse::<u64>().is_err()
        || extension != std::env::consts::DLL_EXTENSION
    {
        return None;
    }
    let pid = pid.parse().ok()?;
    (1..=i32::MAX as u32).contains(&pid).then_some(pid)
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn cleanup_claim_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix(".parser-cleanup.")?;
    let mut parts = rest.split('.');
    let pid = parts.next()?;
    let counter = parts.next()?;
    if parts.next().is_some()
        || !is_canonical_decimal(pid)
        || !is_canonical_decimal(counter)
        || counter.parse::<u64>().is_err()
    {
        return None;
    }
    let pid = pid.parse().ok()?;
    (1..=i32::MAX as u32).contains(&pid).then_some(pid)
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowsProcessProbe {
    Running,
    Exited,
    AccessDenied,
    UnexpectedFailure,
}

#[cfg(any(windows, test))]
fn windows_process_probe_is_running(probe: WindowsProcessProbe) -> bool {
    !matches!(probe, WindowsProcessProbe::Exited)
}

#[cfg(windows)]
fn windows_metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn process_is_running_windows(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let probe = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            match GetLastError() {
                ERROR_INVALID_PARAMETER => WindowsProcessProbe::Exited,
                ERROR_ACCESS_DENIED => WindowsProcessProbe::AccessDenied,
                _ => WindowsProcessProbe::UnexpectedFailure,
            }
        } else {
            let mut exit_code = 0;
            let queried = GetExitCodeProcess(handle, &mut exit_code);
            let _ = CloseHandle(handle);
            if queried == 0 {
                WindowsProcessProbe::UnexpectedFailure
            } else if exit_code == STILL_ACTIVE as u32 {
                WindowsProcessProbe::Running
            } else {
                WindowsProcessProbe::Exited
            }
        }
    };
    windows_process_probe_is_running(probe)
}

#[cfg(unix)]
fn claim_and_unlink_stale_parser_file(
    parser_dir: &Path,
    path: &Path,
) -> Result<Option<PathBuf>, ParserInstallError> {
    use std::io::Write;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    if !initial.file_type().is_file() {
        return Ok(None);
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        // The pathname can be replaced after symlink_metadata. Never block if
        // an external actor swaps in a FIFO before open, and never follow a
        // symlink swapped into the final path component.
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => return Ok(None),
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    if !file.metadata()?.file_type().is_file() {
        return Ok(None);
    }
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(_) => return Ok(None),
    }
    let opened = file.metadata()?;
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    if opened.dev() != current.dev() || opened.ino() != current.ino() {
        return Ok(None);
    }

    loop {
        let candidate = parser_dir.join(format!(
            ".parser-cleanup.{}.{}",
            std::process::id(),
            PARSER_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ParserInstallError::IoError(error)),
        }
        let marker = candidate.join("owner");
        let marker_result = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&marker)
            .and_then(|mut file| {
                file.write_all(PARSER_CLEANUP_MARKER)?;
                file.sync_all()
            });
        if let Err(error) = marker_result {
            // `write_all` or `sync_all` may fail after creating a partial
            // marker. Remove it before the now-empty claim directory.
            let _ = fs::remove_file(&marker);
            let _ = fs::remove_dir(&candidate);
            return Err(ParserInstallError::IoError(error));
        }
        let claimed = candidate.join("artifact");
        // Among cooperating installers, `path` cannot change here: its
        // create_new reservation remains present until this atomic rename, and
        // every cleaner follows this same lock/claim protocol. A same-UID actor
        // deliberately rewriting the private data directory can also delete an
        // installed parser directly and is outside this filesystem protocol's
        // trust boundary. The post-rename inode check still fail-closes without
        // deleting an unexpected inode.
        match fs::rename(path, &claimed) {
            Ok(()) => {
                let current = fs::symlink_metadata(&claimed)?;
                if opened.dev() != current.dev() || opened.ino() != current.ino() {
                    // Never restore with rename: Unix rename would overwrite a
                    // new entry created at the original pathname. Invalidate
                    // provenance and retain the unexpected inode in the claim.
                    let _ = fs::remove_file(&marker);
                    return Ok(None);
                }
                return Ok(Some(candidate));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let _ = fs::remove_file(&marker);
                let _ = fs::remove_dir(&candidate);
                return Ok(None);
            }
            Err(_) => {
                let _ = fs::remove_file(&marker);
                let _ = fs::remove_dir(&candidate);
                return Ok(None);
            }
        }
    }
}

#[cfg(unix)]
const PARSER_CLEANUP_MARKER: &[u8] = b"kakehashi parser cleanup v1\n";

#[cfg(unix)]
fn remove_stale_cleanup_claim(path: &Path) {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;
    use nix::unistd::{UnlinkatFlags, unlinkat};

    let Ok(dir_fd) = open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
        Mode::empty(),
    ) else {
        return;
    };
    let dir = fs::File::from(dir_fd);
    let Ok(opened) = dir.metadata() else {
        return;
    };

    let Ok(marker_fd) = openat(
        &dir,
        "owner",
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) else {
        return;
    };
    let file = fs::File::from(marker_fd);
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return;
    }
    let mut contents = Vec::with_capacity(PARSER_CLEANUP_MARKER.len() + 1);
    if file
        .take(PARSER_CLEANUP_MARKER.len() as u64 + 1)
        .read_to_end(&mut contents)
        .is_err()
        || contents != PARSER_CLEANUP_MARKER
    {
        return;
    }
    // Remove only the two protocol-owned entries through the opened directory
    // descriptor. A rename-and-replace of `path` cannot redirect these unlinks.
    let _ = unlinkat(&dir, "artifact", UnlinkatFlags::NoRemoveDir);
    let _ = unlinkat(&dir, "owner", UnlinkatFlags::NoRemoveDir);

    // The remaining directory is empty. Verify the pathname still names the
    // opened directory before removing it; never recurse through the pathname.
    let Ok(current) = fs::symlink_metadata(path) else {
        return;
    };
    if opened.dev() == current.dev() && opened.ino() == current.ino() {
        let _ = fs::remove_dir(path);
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(_) => true,
    }
}

#[cfg(any(windows, test))]
fn claim_stale_parser_file_windows(
    parser_dir: &Path,
    path: &Path,
) -> Result<Option<PathBuf>, ParserInstallError> {
    use std::io::Write;

    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    if !initial.file_type().is_file() {
        return Ok(None);
    }
    #[cfg(windows)]
    if windows_metadata_is_reparse_point(&initial) {
        return Ok(None);
    }

    loop {
        let candidate = parser_dir.join(format!(
            ".parser-cleanup.{}.{}",
            std::process::id(),
            PARSER_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(ParserInstallError::IoError(error)),
        }
        let marker = candidate.join("owner");
        let marker_result = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .and_then(|mut file| {
                file.write_all(PARSER_CLEANUP_MARKER)?;
                file.sync_all()
            });
        if let Err(error) = marker_result {
            let _ = fs::remove_file(&marker);
            let _ = fs::remove_dir(&candidate);
            return Err(ParserInstallError::IoError(error));
        }

        let claimed = candidate.join("artifact");
        match fs::rename(path, &claimed) {
            Ok(()) => {
                let _current = fs::symlink_metadata(&claimed)?;
                #[cfg(windows)]
                if !_current.file_type().is_file() || windows_metadata_is_reparse_point(&_current) {
                    // The source changed between validation and rename. Remove
                    // provenance so a later cleanup cannot delete it.
                    let _ = fs::remove_file(&marker);
                    return Ok(None);
                }
                return Ok(Some(candidate));
            }
            Err(_) => {
                // A live Windows compiler holds a handle that denies delete
                // sharing, so rename fails here. Unknown failures are equally
                // conservative: leave the source and discard our empty claim.
                let _ = fs::remove_file(&marker);
                let _ = fs::remove_dir(&candidate);
                return Ok(None);
            }
        }
    }
}

#[cfg(any(windows, test))]
fn remove_stale_cleanup_claim_windows(path: &Path) {
    use std::io::Read;

    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.file_type().is_dir() {
        return;
    }
    #[cfg(windows)]
    {
        // `is_dir` can be true for junctions and other directory reparse
        // points. Never resolve `owner` or `artifact` through one.
        if windows_metadata_is_reparse_point(&metadata) {
            return;
        }
    }
    let marker = path.join("owner");
    let Ok(marker_metadata) = fs::symlink_metadata(&marker) else {
        return;
    };
    if !marker_metadata.file_type().is_file() {
        return;
    }
    #[cfg(windows)]
    if windows_metadata_is_reparse_point(&marker_metadata) {
        return;
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let Ok(file) = options.open(&marker) else {
        return;
    };
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
    {
        return;
    }
    let mut contents = Vec::with_capacity(PARSER_CLEANUP_MARKER.len() + 1);
    if file
        .take(PARSER_CLEANUP_MARKER.len() as u64 + 1)
        .read_to_end(&mut contents)
        .is_err()
        || contents != PARSER_CLEANUP_MARKER
    {
        return;
    }
    let _ = fs::remove_file(path.join("artifact"));
    let _ = fs::remove_file(marker);
    let _ = fs::remove_dir(path);
}

#[cfg(any(windows, test))]
fn cleanup_interrupted_parser_installs_windows_with(
    parser_dir: &Path,
    process_is_running: impl Fn(u32) -> bool,
) -> Result<(), ParserInstallError> {
    let entries = match fs::read_dir(parser_dir) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(pid) = cleanup_claim_pid(&path) {
            if !process_is_running(pid) {
                remove_stale_cleanup_claim_windows(&path);
            }
            continue;
        }
        let Some(pid) = staged_parser_pid(&path) else {
            continue;
        };
        if !process_is_running(pid) {
            let Ok(Some(claimed)) = claim_stale_parser_file_windows(parser_dir, &path) else {
                continue;
            };
            remove_stale_cleanup_claim_windows(&claimed);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn cleanup_interrupted_parser_installs(parser_dir: &Path) -> Result<(), ParserInstallError> {
    let entries = match fs::read_dir(parser_dir) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(ParserInstallError::IoError(error)),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(pid) = cleanup_claim_pid(&path) {
            if !process_is_running(pid) {
                remove_stale_cleanup_claim(&path);
            }
            continue;
        }
        let Some(pid) = staged_parser_pid(&path) else {
            continue;
        };
        if !process_is_running(pid) {
            // Claim the stale pathname atomically before deleting it. A new
            // installer reserves staging paths with create_new, so after this
            // claim it may safely reuse the old PID/counter name without our
            // cleanup deleting its newly-created output.
            // Recovery is best-effort. An unreadable or otherwise unclaimable
            // lookalike must not prevent installation from proceeding.
            let Ok(Some(claimed)) = claim_and_unlink_stale_parser_file(parser_dir, &path) else {
                continue;
            };
            remove_stale_cleanup_claim(&claimed);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn cleanup_interrupted_parser_installs(parser_dir: &Path) -> Result<(), ParserInstallError> {
    cleanup_interrupted_parser_installs_windows_with(parser_dir, process_is_running_windows)
}

#[cfg(not(any(unix, windows)))]
fn cleanup_interrupted_parser_installs(_parser_dir: &Path) -> Result<(), ParserInstallError> {
    Ok(())
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
    use tempfile::tempdir;

    #[test]
    fn windows_process_probe_only_recovers_confirmed_exits() {
        assert!(windows_process_probe_is_running(
            WindowsProcessProbe::Running
        ));
        assert!(!windows_process_probe_is_running(
            WindowsProcessProbe::Exited
        ));
        assert!(windows_process_probe_is_running(
            WindowsProcessProbe::AccessDenied
        ));
        assert!(windows_process_probe_is_running(
            WindowsProcessProbe::UnexpectedFailure
        ));
    }

    #[test]
    fn windows_cleanup_removes_confirmed_dead_staging_artifact() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let staged = parser_dir.join(format!(
            ".lua.123.0.{}.tmp",
            std::env::consts::DLL_EXTENSION
        ));
        fs::write(&staged, b"stale parser").expect("write staging file");

        cleanup_interrupted_parser_installs_windows_with(&parser_dir, |_| false)
            .expect("cleanup succeeds");

        assert!(!staged.exists(), "confirmed-dead staging file is removed");
    }

    #[cfg(unix)]
    #[test]
    fn windows_claim_recovery_rejects_symlink_marker() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let claim = parser_dir.join(".parser-cleanup.123.0");
        fs::create_dir(&claim).expect("create claim dir");
        fs::write(claim.join("artifact"), b"user data").expect("write artifact");
        let target = temp.path().join("marker-target");
        fs::write(&target, PARSER_CLEANUP_MARKER).expect("write marker target");
        symlink(&target, claim.join("owner")).expect("create marker symlink");

        cleanup_interrupted_parser_installs_windows_with(&parser_dir, |_| false)
            .expect("cleanup succeeds");

        assert!(claim.exists(), "unproven claim is preserved");
        assert_eq!(
            fs::read(claim.join("artifact")).expect("read artifact"),
            b"user data"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_staging_guards_overlap_without_delete_window() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let (staged, parent_guard) =
            reserve_parser_staging_file(&parser_dir, "lua").expect("reserve staging file");
        let child_guard = open_windows_staging_guard(&staged).expect("open child guard");
        let renamed = parser_dir.join("renamed");

        assert!(
            fs::rename(&staged, &renamed).is_err(),
            "parent and child guards deny cleanup rename"
        );
        drop(parent_guard);
        assert!(
            fs::rename(&staged, &renamed).is_err(),
            "child guard remains after parent exit"
        );
        drop(child_guard);
        fs::rename(&staged, &renamed).expect("rename succeeds after compiler exit");
    }

    #[cfg(unix)]
    fn terminated_child_pid() -> u32 {
        let mut child = std::process::Command::new(
            std::env::current_exe().expect("resolve current test executable"),
        )
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn short-lived child");
        let pid = child.id();
        child.wait().expect("wait for short-lived child");
        assert!(!process_is_running(pid), "reaped child PID is not running");
        pid
    }

    #[cfg(unix)]
    #[test]
    fn staging_lock_is_inherited_through_exec() {
        let temp = tempdir().expect("temp dir");
        let path = temp.path().join("staging");
        let lock = fs::File::create(&path).expect("create staging file");
        lock.lock_exclusive().expect("lock staging file");
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        inherit_staging_lock(&mut cmd, &lock);
        let mut child = cmd.spawn().expect("spawn lock inheritor");
        drop(lock);

        let contender = fs::File::open(&path).expect("open staging file");
        assert!(
            contender.try_lock_exclusive().is_err(),
            "exec child retains the staging lock after the parent drops it"
        );

        child.kill().expect("kill lock inheritor");
        child.wait().expect("reap lock inheritor");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_removes_staged_parser_from_dead_process() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let pid = terminated_child_pid();
        let staged = parser_dir.join(format!(
            ".lua.{pid}.0.{}.tmp",
            std::env::consts::DLL_EXTENSION
        ));
        fs::write(&staged, b"partial parser").expect("write staged parser");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(!staged.exists(), "dead process staging file is removed");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_staged_parser_from_live_process() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let staged = parser_dir.join(format!(
            ".lua.{}.0.{}.tmp",
            std::process::id(),
            std::env::consts::DLL_EXTENSION
        ));
        fs::write(&staged, b"active parser").expect("write staged parser");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(staged.exists(), "live process staging file is preserved");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_noncanonical_staging_name() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let empty_counter =
            parser_dir.join(format!(".lua.123..{}.tmp", std::env::consts::DLL_EXTENSION));
        let leading_zero = parser_dir.join(format!(
            ".lua.123.01.{}.tmp",
            std::env::consts::DLL_EXTENSION
        ));
        let leading_zero_pid = parser_dir.join(format!(
            ".lua.000123.0.{}.tmp",
            std::env::consts::DLL_EXTENSION
        ));
        let zero_pid = parser_dir.join(format!(".lua.0.0.{}.tmp", std::env::consts::DLL_EXTENSION));
        let oversized_pid = parser_dir.join(format!(
            ".lua.{}.0.{}.tmp",
            i32::MAX as u32 + 1,
            std::env::consts::DLL_EXTENSION
        ));
        let noncanonical_claim = parser_dir.join(".parser-cleanup.000123.0");
        let zero_claim = parser_dir.join(".parser-cleanup.0.0");
        let oversized_claim = parser_dir.join(format!(".parser-cleanup.{}.0", i32::MAX as u32 + 1));
        fs::write(&empty_counter, b"user file").expect("write user file");
        fs::write(&leading_zero, b"user file").expect("write user file");
        fs::write(&leading_zero_pid, b"user file").expect("write user file");
        fs::write(&zero_pid, b"user file").expect("write user file");
        fs::write(&oversized_pid, b"user file").expect("write user file");
        fs::write(&noncanonical_claim, b"user file").expect("write user file");
        fs::create_dir(&zero_claim).expect("create zero-PID claim lookalike");
        fs::write(zero_claim.join("owner"), PARSER_CLEANUP_MARKER).expect("write marker");
        fs::create_dir(&oversized_claim).expect("create oversized-PID claim lookalike");
        fs::write(oversized_claim.join("owner"), PARSER_CLEANUP_MARKER).expect("write marker");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(empty_counter.exists(), "empty counter name is preserved");
        assert!(leading_zero.exists(), "leading-zero name is preserved");
        assert!(leading_zero_pid.exists(), "leading-zero PID is preserved");
        assert!(zero_pid.exists(), "zero PID is preserved");
        assert!(oversized_pid.exists(), "oversized PID is preserved");
        assert!(
            noncanonical_claim.exists(),
            "leading-zero cleanup claim PID is preserved"
        );
        assert!(zero_claim.exists(), "zero-PID cleanup claim is preserved");
        assert!(
            oversized_claim.exists(),
            "oversized-PID cleanup claim is preserved"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_staging_file_claimed_by_another_cleaner() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let pid = terminated_child_pid();
        let staged = parser_dir.join(format!(
            ".lua.{pid}.0.{}.tmp",
            std::env::consts::DLL_EXTENSION
        ));
        fs::write(&staged, b"partial parser").expect("write staged parser");
        let claimed_elsewhere = fs::File::open(&staged).expect("open staged parser");
        claimed_elsewhere
            .lock_exclusive()
            .expect("simulate another cleaner's claim");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(staged.exists(), "another cleaner owns the staging file");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_only_removes_proven_cleanup_claims() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let pid = terminated_child_pid();
        let owned = parser_dir.join(format!(".parser-cleanup.{pid}.0"));
        let unowned = parser_dir.join(format!(".parser-cleanup.{pid}.1"));
        fs::create_dir(&owned).expect("create owned cleanup claim");
        fs::write(owned.join("owner"), PARSER_CLEANUP_MARKER).expect("write ownership marker");
        fs::write(owned.join("artifact"), b"stale parser").expect("write stale artifact");
        fs::create_dir(&unowned).expect("create unowned lookalike");
        nix::unistd::mkfifo(
            &unowned.join("owner"),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("create marker-shaped FIFO");
        fs::write(unowned.join("artifact"), b"user data").expect("write user data");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(!owned.exists(), "proven abandoned claim is removed");
        assert!(unowned.exists(), "unmarked lookalike is preserved");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_skips_non_file_staging_path() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let pid = terminated_child_pid();
        let staged = parser_dir.join(format!(
            ".lua.{pid}.0.{}.tmp",
            std::env::consts::DLL_EXTENSION
        ));
        let claim = parser_dir.join(format!(".parser-cleanup.{pid}.0"));
        fs::create_dir(&staged).expect("create impostor directory");
        fs::create_dir(&claim).expect("create impostor claim directory");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(staged.is_dir(), "non-file staging path is preserved");
        assert!(claim.is_dir(), "non-file cleanup claim is preserved");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_skips_unreadable_staging_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let staged = parser_dir.join(format!(
            ".lua.{}.0.{}.tmp",
            terminated_child_pid(),
            std::env::consts::DLL_EXTENSION
        ));
        fs::write(&staged, b"unreadable lookalike").expect("write staging file");
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o000))
            .expect("remove read permission");

        if fs::File::open(&staged).is_ok() {
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))
                .expect("restore staging permissions");
            return;
        }

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup remains best-effort");

        assert!(staged.exists(), "unreadable staging file is preserved");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_staging_symlink_and_target() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let target = temp.path().join("user-file");
        fs::write(&target, b"user data").expect("write target");
        let staged = parser_dir.join(format!(
            ".lua.{}.0.{}.tmp",
            terminated_child_pid(),
            std::env::consts::DLL_EXTENSION
        ));
        symlink(&target, &staged).expect("create staging-shaped symlink");

        cleanup_interrupted_parser_installs(&parser_dir).expect("cleanup succeeds");

        assert!(
            fs::symlink_metadata(&staged)
                .expect("symlink remains")
                .file_type()
                .is_symlink(),
            "staging-shaped symlink is preserved"
        );
        assert_eq!(fs::read(&target).expect("read target"), b"user data");
    }

    #[cfg(unix)]
    #[test]
    fn existing_parser_still_cleans_stale_artifacts() {
        let temp = tempdir().expect("temp dir");
        let parser_dir = temp.path().join("parser");
        fs::create_dir_all(&parser_dir).expect("create parser dir");
        let installed = parser_dir.join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        fs::write(&installed, b"installed parser").expect("write installed parser");
        let stale = parser_dir.join(format!(
            ".json.{}.0.{}.tmp",
            terminated_child_pid(),
            std::env::consts::DLL_EXTENSION
        ));
        fs::write(&stale, b"stale parser").expect("write stale parser");
        let options = InstallOptions {
            data_dir: temp.path().to_path_buf(),
            force: false,
            verbose: false,
            no_cache: false,
            compile: ParserCompile::InProcess,
        };

        let result = install_parser("lua", &options);

        assert!(
            matches!(result, Err(ParserInstallError::AlreadyExists(path)) if path == installed)
        );
        assert!(
            !stale.exists(),
            "recovery runs even when the install is rejected"
        );
    }

    const TREE_SITTER_JSON_URL: &str =
        "https://github.com/tree-sitter/tree-sitter-json/archive/v0.24.8.tar.gz";

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
