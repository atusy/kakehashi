//! Query file downloading from nvim-treesitter repository.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use fs4::fs_std::FileExt;

use super::http::agent_with_timeout;
#[cfg(test)]
use super::http::agent_with_timeout_allowing_http;

/// Base URL for nvim-treesitter query files on GitHub (main branch).
/// Note: In the main branch, queries are under runtime/queries instead of queries.
pub(crate) const NVIM_TREESITTER_QUERIES_URL: &str =
    "https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/main/runtime/queries";

/// Query file types to download.
const QUERY_FILES: &[&str] = &["highlights.scm", "injections.scm"];

/// HTTP timeout for query file downloads; keeps installs bounded when a
/// response stalls (query files are small text files, so 60s is generous).
const QUERY_HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const QUERY_INSTALL_COMPLETE_MARKER: &str = ".kakehashi-install-complete";
const QUERY_BACKUP_OWNERSHIP_MARKER: &str = ".kakehashi-backup";
const QUERY_UNINSTALL_TOMBSTONE_SUFFIX: &str = ".uninstalled";

static QUERY_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Error types for query installation.
#[derive(Debug)]
pub enum QueryInstallError {
    /// The language is not supported (queries don't exist in nvim-treesitter).
    LanguageNotSupported(String),
    /// The language name is not a valid path/URL segment (see
    /// [`is_safe_language_name`]) — invalid input, not a missing upstream.
    InvalidLanguageName(String),
    /// HTTP request failed.
    HttpError(String),
    /// HTTP response returned a structured non-success status code.
    HttpStatus { code: u16, url: String },
    /// Plain HTTP was rejected by the production HTTPS-only policy.
    HttpsOnly { url: String },
    /// File system operation failed.
    IoError(std::io::Error),
    /// Queries already exist and --force not specified.
    AlreadyExists(PathBuf),
}

impl std::fmt::Display for QueryInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LanguageNotSupported(lang) => {
                write!(
                    f,
                    "Language '{}' is not supported or queries not found in nvim-treesitter",
                    lang
                )
            }
            Self::InvalidLanguageName(lang) => {
                write!(
                    f,
                    "Invalid language name '{}' (allowed: lowercase ASCII letters, digits, underscore)",
                    lang
                )
            }
            Self::HttpError(msg) => write!(f, "HTTP error: {}", msg),
            Self::HttpStatus { code, url } => write!(f, "HTTP {} for {}", code, url),
            Self::HttpsOnly { url } => write!(f, "HTTPS-only policy rejected {}", url),
            Self::IoError(e) => write!(f, "IO error: {}", e),
            Self::AlreadyExists(path) => {
                write!(
                    f,
                    "Queries already exist at {}. Use --force to overwrite.",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for QueryInstallError {}

impl From<std::io::Error> for QueryInstallError {
    fn from(e: std::io::Error) -> Self {
        Self::IoError(e)
    }
}

/// Result of installing queries for a language.
pub struct QueryInstallResult {
    /// The language that was installed.
    pub language: String,
    /// Path where queries were installed.
    pub install_path: PathBuf,
    /// List of files that were downloaded.
    pub files_downloaded: Vec<String>,
}

/// Whether a language name is safe to use as a path and URL segment.
///
/// Language names are used as path segments (`queries/<name>/`) and URL
/// segments, so anything outside nvim-treesitter's `[a-z0-9_]+` naming is
/// rejected: a name like `../../x` (from a caller or a `; inherits:` line in
/// a compromised or custom query source) must not escape the data dir.
pub fn is_safe_language_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn validate_safe_language_name(language: &str) -> Result<(), QueryInstallError> {
    if is_safe_language_name(language) {
        Ok(())
    } else {
        Err(QueryInstallError::InvalidLanguageName(
            language.escape_default().to_string(),
        ))
    }
}

/// Parse the `; inherits: lang1,lang2` directive from query content.
/// Returns the list of parent languages, dropping unsafe names
/// (see [`is_safe_language_name`]).
fn parse_inherits_directive(content: &str) -> Vec<String> {
    let first_line = content.lines().next().unwrap_or("");
    if let Some(rest) = first_line.strip_prefix("; inherits:") {
        rest.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|s| {
                let safe = is_safe_language_name(s);
                if !safe {
                    // Debug-format: the name is untrusted input and could
                    // smuggle ANSI escapes into the terminal if printed raw.
                    eprintln!("Warning: ignoring unsafe inherited language name {:?}", s);
                }
                safe
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Download and install query files for a language, including inherited dependencies.
///
/// This recursively downloads parent queries (e.g., ecma, jsx for TypeScript).
pub fn install_queries_with_dependencies(
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    clear_uninstall_tombstone_for_install(data_dir, language)?;
    install_queries_with_dependencies_from_with_http_policy(
        NVIM_TREESITTER_QUERIES_URL,
        language,
        data_dir,
        force,
        QueryHttpPolicy::HttpsOnly,
    )
}

pub fn install_queries_with_dependencies_after_install_started(
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    install_queries_with_dependencies_from_with_http_policy(
        NVIM_TREESITTER_QUERIES_URL,
        language,
        data_dir,
        force,
        QueryHttpPolicy::HttpsOnly,
    )
}

/// Like [`install_queries_with_dependencies`] but downloading from `base_url`.
pub(crate) fn install_queries_with_dependencies_from(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    install_queries_with_dependencies_from_with_http_policy(
        base_url,
        language,
        data_dir,
        force,
        QueryHttpPolicy::HttpsOnly,
    )
}

/// Like [`install_queries_with_dependencies_from`] but disables the HTTPS-only
/// policy for tests that serve fixture query files over local plain HTTP.
#[cfg(test)]
pub(crate) fn install_queries_with_dependencies_from_allowing_http_for_tests(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<QueryInstallResult, QueryInstallError> {
    install_queries_with_dependencies_from_with_http_policy(
        base_url,
        language,
        data_dir,
        force,
        QueryHttpPolicy::AllowHttpForTests,
    )
}

#[derive(Clone, Copy)]
enum QueryHttpPolicy {
    HttpsOnly,
    #[cfg(test)]
    AllowHttpForTests,
}

fn install_queries_with_dependencies_from_with_http_policy(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    http_policy: QueryHttpPolicy,
) -> Result<QueryInstallResult, QueryInstallError> {
    let mut installed = std::collections::HashSet::new();
    install_queries_recursive(
        base_url,
        language,
        data_dir,
        force,
        &mut installed,
        http_policy,
    )
}

fn validate_url_http_policy(
    url: &str,
    http_policy: QueryHttpPolicy,
) -> Result<(), QueryInstallError> {
    match http_policy {
        QueryHttpPolicy::HttpsOnly if url.starts_with("http://") => {
            Err(QueryInstallError::HttpsOnly {
                url: url.to_string(),
            })
        }
        _ => Ok(()),
    }
}

/// Internal recursive helper for installing queries with dependencies.
fn install_queries_recursive(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    installed: &mut std::collections::HashSet<String>,
    http_policy: QueryHttpPolicy,
) -> Result<QueryInstallResult, QueryInstallError> {
    // The name becomes a path and URL segment below; reject anything that
    // could escape the data dir (e.g. a caller-provided `../../x`).
    // Escape the untrusted name: the error's Display is printed raw by
    // the CLI, so control characters must not reach the terminal.
    validate_safe_language_name(language)?;

    // Skip if already installed in this session
    if installed.contains(language) {
        return Ok(QueryInstallResult {
            language: language.to_string(),
            install_path: data_dir.join("queries").join(language),
            files_downloaded: vec![],
        });
    }

    let queries_dir = data_dir.join("queries").join(language);
    let queries_parent = data_dir.join("queries");
    fs::create_dir_all(&queries_parent)?;
    recover_interrupted_query_install(&queries_parent, language)?;

    // Check if queries already exist. A previous interrupted install may
    // leave a directory without the required highlights.scm; that is treated
    // as incomplete so a later install can repair it without --force.
    if query_install_is_complete(&queries_dir) && !force {
        // Mark as installed BEFORE recursing into parents: an inheritance
        // cycle among on-disk query files (self-inherit typo, A↔B) would
        // otherwise recurse forever and overflow the stack. The download
        // branch below already inserts before its parent loop.
        installed.insert(language.to_string());

        // Even if skipping, we need to check for inherited dependencies
        let highlights_path = queries_dir.join("highlights.scm");
        if highlights_path.exists()
            && let Ok(content) = std::fs::read_to_string(&highlights_path)
        {
            let parents = parse_inherits_directive(&content);
            for parent in parents {
                // Install parent dependencies (don't force, just ensure they exist)
                clear_uninstall_tombstone(&queries_parent, &parent)?;
                match install_queries_recursive(
                    base_url,
                    &parent,
                    data_dir,
                    false,
                    installed,
                    http_policy,
                ) {
                    Ok(_) | Err(QueryInstallError::AlreadyExists(_)) => {}
                    Err(e) => {
                        eprintln!(
                            "Warning: Failed to install inherited queries '{}': {}",
                            parent, e
                        );
                    }
                }
            }
        }
        return Err(QueryInstallError::AlreadyExists(queries_dir));
    }

    let tmp_queries_dir = create_unique_temp_query_dir(&queries_parent, language)?;
    let _tmp_guard = TempQueryDirGuard {
        path: tmp_queries_dir.clone(),
    };

    let mut files_downloaded = Vec::new();
    let mut any_success = false;
    let mut parents_to_install = Vec::new();

    // Download each query file
    for query_file in QUERY_FILES {
        let url = format!("{}/{}/{}", base_url, language, query_file);

        match download_file(&url, http_policy) {
            Ok(content) => {
                // Check for inherits directive in highlights.scm
                if *query_file == "highlights.scm" {
                    parents_to_install = parse_inherits_directive(&content);
                }

                let file_path = tmp_queries_dir.join(query_file);
                write_query_file(&file_path, &content)?;
                files_downloaded.push(query_file.to_string());
                any_success = true;
            }
            Err(e) => {
                // highlights.scm is required, others are optional
                if *query_file == "highlights.scm" {
                    return match e {
                        QueryInstallError::HttpStatus { code: 404, .. } => Err(
                            QueryInstallError::LanguageNotSupported(language.to_string()),
                        ),
                        other => Err(other),
                    };
                }
                // Log but continue for optional files
                eprintln!(
                    "Note: {} not available for {} ({})",
                    query_file, language, e
                );
            }
        }
    }

    if !any_success {
        return Err(QueryInstallError::LanguageNotSupported(
            language.to_string(),
        ));
    }

    write_install_marker(&tmp_queries_dir)?;

    match replace_query_dir(&tmp_queries_dir, &queries_dir, language, force) {
        Ok(ReplaceQueryDirResult::Replaced) => {}
        Ok(ReplaceQueryDirResult::AlreadyComplete) => {
            return Err(QueryInstallError::AlreadyExists(queries_dir));
        }
        Ok(ReplaceQueryDirResult::Uninstalled) => {
            return Err(QueryInstallError::IoError(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("Query install for {language} was superseded by uninstall"),
            )));
        }
        Err(e) => {
            return Err(e);
        }
    }

    installed.insert(language.to_string());

    // Install parent dependencies
    for parent in parents_to_install {
        eprintln!("Installing inherited queries: {}", parent);
        // Don't fail if parent already exists
        clear_uninstall_tombstone(&queries_parent, &parent)?;
        match install_queries_recursive(base_url, &parent, data_dir, false, installed, http_policy)
        {
            Ok(_) | Err(QueryInstallError::AlreadyExists(_)) => {}
            Err(e) => {
                eprintln!(
                    "Warning: Failed to install inherited queries '{}': {}",
                    parent, e
                );
            }
        }
    }

    Ok(QueryInstallResult {
        language: language.to_string(),
        install_path: queries_dir,
        files_downloaded,
    })
}

pub fn query_install_is_complete(queries_dir: &Path) -> bool {
    let highlights_path = queries_dir.join("highlights.scm");
    let Ok(metadata) = fs::metadata(&highlights_path) else {
        return false;
    };
    // The marker is written only after a staged install has written all
    // required files. Legacy direct-write directories did not have it, so a
    // non-empty highlights.scm still counts as installed to avoid clobbering
    // valid user-managed or pre-marker query directories.
    metadata.is_file()
        && (queries_dir.join(QUERY_INSTALL_COMPLETE_MARKER).is_file() || metadata.len() > 0)
}

/// RAII cleanup for a staging directory: removes it on drop so every error
/// path (including `?` propagation added later) leaves nothing stranded. On
/// the success path `replace_query_dir` renames the directory away, making
/// the drop-time removal a harmless no-op.
struct TempQueryDirGuard {
    path: PathBuf,
}

impl Drop for TempQueryDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn create_unique_temp_query_dir(
    queries_parent: &Path,
    language: &str,
) -> Result<PathBuf, QueryInstallError> {
    loop {
        let candidate = queries_parent.join(format!(
            ".{}.{}.{}.tmp",
            language,
            std::process::id(),
            QUERY_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(QueryInstallError::IoError(e)),
        }
    }
}

fn unique_backup_query_dir(queries_dir: &Path, language: &str) -> PathBuf {
    loop {
        let candidate = queries_dir.with_file_name(format!(
            ".{}.{}.{}.backup",
            language,
            std::process::id(),
            QUERY_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
}

/// Recover query directories stranded by a process exit during replacement.
pub fn recover_interrupted_query_installs(queries_parent: &Path) -> Result<(), QueryInstallError> {
    let entries = match fs::read_dir(queries_parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(QueryInstallError::IoError(e)),
    };

    // Recover at most once per language: a single recovery pass already
    // considers every backup for that language (newest_complete_backup_dir
    // rescans the parent), so running it per backup directory would redo the
    // same scan and lock acquisition for each stranded backup.
    let mut recovered_languages = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(language) = backup_language_name(&path) {
            if recovered_languages.insert(language.clone()) {
                recover_interrupted_query_install(queries_parent, &language)?;
            }
        } else if let Some((language, _)) = temp_language_name_and_pid(&path) {
            remove_interrupted_temp_query_install(queries_parent, &language, &path)?;
        }
    }

    Ok(())
}

pub struct QueryRemoval {
    pub removed_queries: bool,
    pub removed_backups: bool,
}

impl QueryRemoval {
    pub fn removed_anything(&self) -> bool {
        self.removed_queries || self.removed_backups
    }
}

pub fn remove_query_install_and_backups(
    queries_parent: &Path,
    language: &str,
) -> Result<QueryRemoval, QueryInstallError> {
    remove_query_install_and_backups_with_tombstone_writer(
        queries_parent,
        language,
        write_uninstall_tombstone_at,
    )
}

fn remove_query_install_and_backups_with_tombstone_writer(
    queries_parent: &Path,
    language: &str,
    write_tombstone: impl FnOnce(&cap_std::fs::Dir, &Path, &str) -> Result<(), QueryInstallError>,
) -> Result<QueryRemoval, QueryInstallError> {
    remove_query_install_and_backups_with_hooks(queries_parent, language, write_tombstone, |_| {})
}

fn remove_query_install_and_backups_with_hooks(
    queries_parent: &Path,
    language: &str,
    write_tombstone: impl FnOnce(&cap_std::fs::Dir, &Path, &str) -> Result<(), QueryInstallError>,
    before_remove: impl FnOnce(&Path),
) -> Result<QueryRemoval, QueryInstallError> {
    validate_safe_language_name(language)?;
    fs::create_dir_all(queries_parent)?;
    let pinned_parent =
        cap_std::fs::Dir::open_ambient_dir(queries_parent, cap_std::ambient_authority())?;
    let _replace_lock = QueryReplaceLockGuard::acquire_at(&pinned_parent, language)?;
    write_tombstone(&pinned_parent, queries_parent, language)?;
    before_remove(queries_parent);
    let mut removal = QueryRemoval {
        removed_queries: false,
        removed_backups: false,
    };

    // No exists() pre-check: Path::exists() reads false on metadata errors
    // (e.g. PermissionDenied), which would skip removal and report "not
    // installed" over a still-present unreadable dir. The tolerant removal
    // reports whether anything was actually removed.
    removal.removed_queries = remove_dir_all_at_tolerating_vanished(&pinned_parent, language)?;

    // Propagate per-entry read_dir errors: uninstall must not report success
    // while backups it could not even enumerate stay behind.
    for entry in pinned_parent.entries()? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // file_type() over path.is_dir(): is_dir() swallows metadata errors
        // as "not a directory", which could leave an unreadable backup behind
        // while uninstall reports success.
        if entry.file_type()?.is_dir()
            && generated_backup_matches_language(name, language)
            && pinned_parent
                .metadata(backup_ownership_sidecar_name(name))
                .is_ok_and(|metadata| metadata.is_file())
        {
            let ownership = backup_ownership_sidecar_name(name);
            // Same NotFound tolerance as the canonical dir above: a backup
            // deleted externally after enumeration is already the end state.
            let removed_dir = remove_dir_all_at_tolerating_vanished(&pinned_parent, &file_name)?;
            // The sidecar is a kakehashi-owned artifact too: deleting it
            // counts as removal even when the dir itself vanished first —
            // and, like every other I/O in this loop, only NotFound is
            // tolerated (an unremovable marker must fail the uninstall, not
            // linger behind a success report).
            let removed_sidecar = match pinned_parent.remove_file(ownership) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(QueryInstallError::IoError(e)),
            };
            if removed_dir || removed_sidecar {
                removal.removed_backups = true;
            }
        }
    }
    Ok(removal)
}

fn remove_dir_all_at_tolerating_vanished(
    parent: &cap_std::fs::Dir,
    name: impl AsRef<Path>,
) -> Result<bool, QueryInstallError> {
    let name = name.as_ref();
    match parent.remove_dir_all(name) {
        Ok(()) => Ok(true),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && matches!(parent.try_exists(name), Ok(false)) =>
        {
            Ok(false)
        }
        Err(e) => Err(QueryInstallError::IoError(e)),
    }
}

/// `fs::remove_dir_all` that treats a CONFIRMED-vanished directory as the
/// desired end state. Returns whether this call actually removed anything
/// (`false` = the dir was already gone).
///
/// NotFound = the dir disappeared between the caller's observation and the
/// removal (external cleanup — the replace lock only serializes kakehashi's
/// own installers): already gone is the desired end state. Confirmed via
/// `try_exists` because (a) remove_dir_all can also surface NotFound for a
/// child that vanished mid-recursion while the dir survives partially
/// deleted, and (b) `Path::exists()` returns false on ANY metadata error
/// (e.g. PermissionDenied), which must propagate the original error instead
/// of being mistaken for absence.
#[cfg(test)]
fn remove_dir_all_tolerating_vanished(dir: &Path) -> Result<bool, QueryInstallError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(true),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && matches!(dir.try_exists(), Ok(false)) =>
        {
            Ok(false)
        }
        Err(e) => Err(QueryInstallError::IoError(e)),
    }
}

fn backup_language_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let (language, _, _) = generated_backup_parts(name)?;
    if is_safe_language_name(language) {
        Some(language.to_string())
    } else {
        None
    }
}

fn temp_language_name_and_pid(path: &Path) -> Option<(String, u32)> {
    let name = path.file_name()?.to_str()?;
    let (language, pid, _) = generated_temp_parts(name)?;
    if is_safe_language_name(language) {
        Some((language.to_string(), pid.parse().ok()?))
    } else {
        None
    }
}

fn generated_backup_matches_language(name: &str, language: &str) -> bool {
    matches!(
        generated_backup_parts(name),
        Some((backup_language, _, _)) if backup_language == language
    )
}

fn generated_backup_parts(name: &str) -> Option<(&str, &str, &str)> {
    let rest = name.strip_prefix('.')?.strip_suffix(".backup")?;
    let mut parts = rest.split('.');
    let language = parts.next()?;
    let pid = parts.next()?;
    let counter = parts.next()?;
    if parts.next().is_none()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && counter.bytes().all(|b| b.is_ascii_digit())
    {
        Some((language, pid, counter))
    } else {
        None
    }
}

fn generated_temp_parts(name: &str) -> Option<(&str, &str, &str)> {
    let rest = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let mut parts = rest.split('.');
    let language = parts.next()?;
    let pid = parts.next()?;
    let counter = parts.next()?;
    if parts.next().is_none()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && counter.bytes().all(|b| b.is_ascii_digit())
    {
        Some((language, pid, counter))
    } else {
        None
    }
}

fn remove_interrupted_temp_query_install(
    queries_parent: &Path,
    language: &str,
    tmp_dir: &Path,
) -> Result<(), QueryInstallError> {
    validate_safe_language_name(language)?;
    let Some(name) = tmp_dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let Some((tmp_language, pid, _)) = generated_temp_parts(name) else {
        return Ok(());
    };
    if tmp_language != language || process_is_running(pid) {
        return Ok(());
    }

    let _replace_lock = QueryReplaceLockGuard::acquire(queries_parent, language)?;
    match fs::remove_dir_all(tmp_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(QueryInstallError::IoError(e)),
    }
    Ok(())
}

#[cfg(unix)]
fn process_is_running(pid: &str) -> bool {
    let Ok(pid) = pid.parse::<i32>() else {
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

#[cfg(not(unix))]
fn process_is_running(pid: &str) -> bool {
    // No portable std API can test another process's liveness. Be
    // conservative: generated temp names contain numeric PIDs, so treat them
    // as possibly live and leave cleanup to a future platform-specific pass.
    pid.parse::<u32>().is_ok()
}

fn recover_interrupted_query_install(
    queries_parent: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    validate_safe_language_name(language)?;
    if uninstall_tombstone_path(queries_parent, language).is_file() {
        return Ok(());
    }
    let queries_dir = queries_parent.join(language);
    if queries_dir.exists() {
        return Ok(());
    }

    let _replace_lock = QueryReplaceLockGuard::acquire(queries_parent, language)?;
    if uninstall_tombstone_path(queries_parent, language).is_file() {
        return Ok(());
    }
    if queries_dir.exists() {
        return Ok(());
    }

    // Select the backup UNDER the lock: chosen before it, a concurrent
    // uninstall/cleanup could delete the directory between selection and the
    // rename, turning a clean "nothing to restore" into a NotFound error.
    let Some(backup_dir) = newest_complete_backup_dir(queries_parent, language)? else {
        return Ok(());
    };

    let ownership = backup_ownership_sidecar(&backup_dir);
    match fs::rename(&backup_dir, queries_dir) {
        Ok(()) => {}
        // The backup vanished after selection (external cleanup — the lock
        // only serializes kakehashi's own installers): nothing to restore,
        // but drop the now-orphaned ownership sidecar so markers don't
        // accumulate under queries/ (idempotent if it is already gone).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = fs::remove_file(ownership);
            return Ok(());
        }
        Err(e) => return Err(QueryInstallError::IoError(e)),
    }
    let _ = fs::remove_file(ownership);
    Ok(())
}

fn uninstall_tombstone_path(queries_parent: &Path, language: &str) -> PathBuf {
    queries_parent.join(format!(".{language}{QUERY_UNINSTALL_TOMBSTONE_SUFFIX}"))
}

#[cfg(test)]
fn write_uninstall_tombstone(
    queries_parent: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    let pinned = cap_std::fs::Dir::open_ambient_dir(queries_parent, cap_std::ambient_authority())?;
    write_uninstall_tombstone_at(&pinned, queries_parent, language)
}

#[cfg(unix)]
fn write_uninstall_tombstone_at(
    queries_parent: &cap_std::fs::Dir,
    ambient_path: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    write_uninstall_tombstone_at_with_hooks(
        queries_parent,
        ambient_path,
        language,
        |_, _, _| {},
        |parent| parent.try_clone()?.into_std_file().sync_all(),
    )
}

#[cfg(unix)]
fn write_uninstall_tombstone_at_with_hooks(
    queries_parent: &cap_std::fs::Dir,
    _ambient_path: &Path,
    language: &str,
    before_publish: impl FnOnce(&cap_std::fs::Dir, &str, &str),
    sync_parent: impl FnOnce(&cap_std::fs::Dir) -> std::io::Result<()>,
) -> Result<(), QueryInstallError> {
    use cap_fs_ext::{
        FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt, OpenOptionsSyncExt,
    };

    validate_safe_language_name(language)?;
    let tombstone_name = format!(".{language}{QUERY_UNINSTALL_TOMBSTONE_SUFFIX}");
    let mut existing_options = cap_std::fs::OpenOptions::new();
    existing_options
        .write(true)
        .follow(FollowSymlinks::No)
        .nonblock(true);
    let existing_permissions = match queries_parent.open_with(&tombstone_name, &existing_options) {
        Ok(file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(QueryInstallError::IoError(std::io::Error::other(
                    "query uninstall tombstone is not a regular file",
                )));
            }
            require_single_link(CapMetadataExt::nlink(&metadata))?;
            Some(file.into_std().metadata()?.permissions())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(QueryInstallError::IoError(error)),
    };

    let stage_name = loop {
        let counter = QUERY_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!(
            ".{language}.uninstalled.{}.{}.tmp",
            std::process::id(),
            counter
        );
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match queries_parent.open_with(&candidate, &options) {
            Ok(file) => break (candidate, file.into_std()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(QueryInstallError::IoError(error)),
        }
    };
    let (stage_name, mut staged) = stage_name;
    let result = (|| -> Result<(), QueryInstallError> {
        if let Some(permissions) = existing_permissions {
            staged.set_permissions(permissions)?;
        }
        staged.write_all(b"ok\n")?;
        staged.sync_all()?;
        let staged_metadata = staged.metadata()?;
        before_publish(queries_parent, &tombstone_name, &stage_name);
        queries_parent.rename(&stage_name, queries_parent, &tombstone_name)?;

        let mut published_options = cap_std::fs::OpenOptions::new();
        published_options
            .read(true)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        let published = queries_parent
            .open_with(&tombstone_name, &published_options)?
            .into_std();
        let published_metadata = published.metadata()?;
        if !published_metadata.is_file()
            || std::os::unix::fs::MetadataExt::dev(&published_metadata)
                != std::os::unix::fs::MetadataExt::dev(&staged_metadata)
            || std::os::unix::fs::MetadataExt::ino(&published_metadata)
                != std::os::unix::fs::MetadataExt::ino(&staged_metadata)
        {
            return Err(QueryInstallError::IoError(std::io::Error::other(
                "published query uninstall tombstone changed identity",
            )));
        }
        require_single_link(std::os::unix::fs::MetadataExt::nlink(&published_metadata))?;
        sync_parent(queries_parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = queries_parent.remove_file(&stage_name);
    }
    result
}

#[cfg(windows)]
fn write_uninstall_tombstone_at(
    queries_parent: &cap_std::fs::Dir,
    ambient_path: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    write_uninstall_tombstone_at_with_hooks(
        queries_parent,
        ambient_path,
        language,
        |_, _, _| {},
        |_| Ok(()),
    )
}

#[cfg(windows)]
fn write_uninstall_tombstone_at_with_hooks(
    queries_parent: &cap_std::fs::Dir,
    _ambient_path: &Path,
    language: &str,
    before_publish: impl FnOnce(&cap_std::fs::Dir, &str, &str),
    sync_parent: impl FnOnce(&cap_std::fs::Dir) -> std::io::Result<()>,
) -> Result<(), QueryInstallError> {
    write_uninstall_tombstone_at_with_security_copy(
        queries_parent,
        _ambient_path,
        language,
        before_publish,
        sync_parent,
        copy_windows_tombstone_security,
    )
}

#[cfg(windows)]
fn write_uninstall_tombstone_at_with_security_copy(
    queries_parent: &cap_std::fs::Dir,
    _ambient_path: &Path,
    language: &str,
    before_publish: impl FnOnce(&cap_std::fs::Dir, &str, &str),
    sync_parent: impl FnOnce(&cap_std::fs::Dir) -> std::io::Result<()>,
    copy_security: impl FnOnce(&fs::File, &fs::File) -> std::io::Result<()>,
) -> Result<(), QueryInstallError> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_WRITE_THROUGH, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileRenameInfo, ReOpenFile, SetFileInformationByHandle,
    };

    validate_safe_language_name(language)?;
    let tombstone_name = format!(".{language}{QUERY_UNINSTALL_TOMBSTONE_SUFFIX}");
    let mut existing_options = cap_std::fs::OpenOptions::new();
    existing_options.write(true).follow(FollowSymlinks::No);
    let (existing_permissions, existing_file) =
        match queries_parent.open_with(&tombstone_name, &existing_options) {
            Ok(file) => {
                let file = file.into_std();
                let metadata = file.metadata()?;
                if !metadata.is_file() {
                    return Err(QueryInstallError::IoError(std::io::Error::other(
                        "query uninstall tombstone is not a regular file",
                    )));
                }
                require_single_link(u64::from(windows_file_information(&file)?.nNumberOfLinks))?;
                (Some(metadata.permissions()), Some(file))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
            Err(error) => return Err(QueryInstallError::IoError(error)),
        };

    let (stage_name, mut staged) = loop {
        let counter = QUERY_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!(
            ".{language}.uninstalled.{}.{}.tmp",
            std::process::id(),
            counter
        );
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match queries_parent.open_with(&candidate, &options) {
            Ok(file) => break (candidate, file.into_std()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(QueryInstallError::IoError(error)),
        }
    };
    let result = (|| -> Result<(), QueryInstallError> {
        if let Some(permissions) = existing_permissions {
            staged.set_permissions(permissions)?;
        }
        staged.write_all(b"ok\n")?;
        staged.sync_all()?;
        let directory = queries_parent.try_clone()?.into_std_file();
        // SAFETY: `staged` owns a valid handle; success returns a new owned handle.
        let handle = unsafe {
            ReOpenFile(
                staged.as_raw_handle(),
                GENERIC_READ | GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_WRITE_THROUGH,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(QueryInstallError::IoError(std::io::Error::last_os_error()));
        }
        // SAFETY: successful ReOpenFile returned a newly owned handle.
        let rename_handle = unsafe { fs::File::from_raw_handle(handle) };
        normalize_windows_stage_attributes(&rename_handle)?;
        if let Some(existing_file) = &existing_file {
            copy_security(existing_file, &rename_handle)?;
        }
        before_publish(queries_parent, &tombstone_name, &stage_name);
        let filename: Vec<u16> = std::ffi::OsStr::new(&tombstone_name)
            .encode_wide()
            .collect();
        let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let size = header + filename.len() * std::mem::size_of::<u16>();
        let units = size.div_ceil(std::mem::size_of::<FILE_RENAME_INFO>());
        let mut buffer = vec![FILE_RENAME_INFO::default(); units];
        let information = buffer.as_mut_ptr();
        // SAFETY: the buffer covers the header/name and both handles stay valid.
        let succeeded = unsafe {
            (*information).Anonymous.ReplaceIfExists = true;
            (*information).RootDirectory = directory.as_raw_handle();
            (*information).FileNameLength = u32::try_from(filename.len() * 2).unwrap();
            std::ptr::copy_nonoverlapping(
                filename.as_ptr(),
                (*information).FileName.as_mut_ptr(),
                filename.len(),
            );
            SetFileInformationByHandle(
                rename_handle.as_raw_handle(),
                FileRenameInfo,
                information.cast(),
                u32::try_from(size).unwrap(),
            )
        };
        if succeeded == 0 {
            return Err(QueryInstallError::IoError(std::io::Error::last_os_error()));
        }
        let mut published_options = cap_std::fs::OpenOptions::new();
        published_options.read(true).follow(FollowSymlinks::No);
        let published = queries_parent
            .open_with(&tombstone_name, &published_options)?
            .into_std();
        let expected = windows_file_information(&rename_handle)?;
        let actual = windows_file_information(&published)?;
        if expected.dwVolumeSerialNumber != actual.dwVolumeSerialNumber
            || expected.nFileIndexHigh != actual.nFileIndexHigh
            || expected.nFileIndexLow != actual.nFileIndexLow
        {
            return Err(QueryInstallError::IoError(std::io::Error::other(
                "published query uninstall tombstone changed identity",
            )));
        }
        require_single_link(u64::from(actual.nNumberOfLinks))?;
        sync_parent(queries_parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = queries_parent.remove_file(&stage_name);
    }
    result
}

#[cfg(test)]
fn write_uninstall_tombstone_with_before_publish(
    queries_parent: &Path,
    language: &str,
    before_publish: impl FnOnce(&Path, &Path),
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), QueryInstallError> {
    let pinned = cap_std::fs::Dir::open_ambient_dir(queries_parent, cap_std::ambient_authority())?;
    write_uninstall_tombstone_at_with_hooks(
        &pinned,
        queries_parent,
        language,
        |_, tombstone_name, stage_name| {
            before_publish(
                &queries_parent.join(tombstone_name),
                &queries_parent.join(stage_name),
            );
        },
        |_| sync_parent(queries_parent),
    )
}

#[cfg(windows)]
fn normalize_windows_stage_attributes(file: &fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TEMPORARY, FILE_BASIC_INFO, FileBasicInfo,
        GetFileInformationByHandleEx, SetFileInformationByHandle,
    };

    let mut information = FILE_BASIC_INFO::default();
    // SAFETY: `information` is the exact buffer for FileBasicInfo and `file`
    // keeps the handle valid throughout both calls.
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut information as *mut FILE_BASIC_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_BASIC_INFO>()).unwrap(),
        )
    };
    if read == 0 {
        return Err(std::io::Error::last_os_error());
    }
    information.FileAttributes &= !FILE_ATTRIBUTE_TEMPORARY;
    if information.FileAttributes == 0 {
        information.FileAttributes = FILE_ATTRIBUTE_NORMAL;
    }
    // SAFETY: same valid handle and correctly sized initialized structure.
    let written = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileBasicInfo,
            (&information as *const FILE_BASIC_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_BASIC_INFO>()).unwrap(),
        )
    };
    if written == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn copy_windows_tombstone_security(source: &fs::File, target: &fs::File) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, EqualSid, GROUP_SECURITY_INFORMATION,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PRESENT, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, ReOpenFile, WRITE_DAC,
        WRITE_OWNER,
    };

    struct SecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);
    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocates this descriptor with LocalAlloc.
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    unsafe fn read_security(
        file: &fs::File,
    ) -> std::io::Result<(
        SecurityDescriptor,
        windows_sys::Win32::Security::PSID,
        windows_sys::Win32::Security::PSID,
        *mut windows_sys::Win32::Security::ACL,
    )> {
        use std::os::windows::io::AsRawHandle as _;

        let mut owner = std::ptr::null_mut();
        let mut group = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: output pointers remain valid until the returned descriptor is dropped.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                &mut group,
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::other(format!(
                "GetSecurityInfo failed: {}",
                std::io::Error::from_raw_os_error(status as i32)
            )));
        }
        Ok((SecurityDescriptor(descriptor), owner, group, dacl))
    }

    // SAFETY: both handles remain open and descriptor-owned pointers stay valid.
    let (source_descriptor, source_owner, source_group, source_dacl) =
        unsafe { read_security(source)? };
    // SAFETY: same contract for the already-opened private stage.
    let (_target_descriptor, target_owner, target_group, _) = unsafe { read_security(target)? };
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is valid and outputs are initialized by the API.
    if unsafe { GetSecurityDescriptorControl(source_descriptor.0, &mut control, &mut revision) }
        == 0
    {
        return Err(std::io::Error::other(format!(
            "GetSecurityDescriptorControl failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    if control & SE_DACL_PRESENT == 0 {
        return Err(std::io::Error::other(
            "validated tombstone security descriptor has no DACL presence bit",
        ));
    }
    let dacl_mode = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    // New stages normally inherit the same owner/group. Only request the
    // privileged WRITE_OWNER path when either SID actually differs.
    let sids_differ = |source: windows_sys::Win32::Security::PSID,
                       target: windows_sys::Win32::Security::PSID| {
        if source.is_null() || target.is_null() {
            source != target
        } else {
            // SAFETY: both non-null pointers refer to descriptor-owned SIDs.
            (unsafe { EqualSid(source, target) }) == 0
        }
    };
    let owner_differs = sids_differ(source_owner, target_owner);
    let group_differs = sids_differ(source_group, target_group);
    let mut owner_information = 0;
    if owner_differs {
        owner_information |= OWNER_SECURITY_INFORMATION;
    }
    if group_differs {
        owner_information |= GROUP_SECURITY_INFORMATION;
    }
    let mut security_access = READ_CONTROL | WRITE_DAC;
    if owner_information != 0 {
        security_access |= WRITE_OWNER;
    }
    // Keep security mutation on a distinct handle so the proven
    // DELETE-capable/write-through rename handle retains its original access
    // contract. Success returns a new owned handle for the same stage inode.
    let security_handle = unsafe {
        ReOpenFile(
            target.as_raw_handle(),
            security_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            0,
        )
    };
    if security_handle.is_null()
        || security_handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
    {
        return Err(std::io::Error::other(format!(
            "ReOpenFile security access failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: successful ReOpenFile returned a newly owned handle.
    let security_file = unsafe { fs::File::from_raw_handle(security_handle) };
    apply_windows_security_parts(
        owner_information,
        || {
            // SAFETY: selected SID pointers remain owned by source_descriptor.
            let status = unsafe {
                SetSecurityInfo(
                    security_file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    owner_information,
                    source_owner,
                    source_group,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "SetSecurityInfo owner/group failed: {}",
                    std::io::Error::from_raw_os_error(status as i32)
                )));
            }
            Ok(())
        },
        || {
            // SAFETY: target has WRITE_DAC and the source DACL remains descriptor-owned.
            let status = unsafe {
                SetSecurityInfo(
                    security_file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | dacl_mode,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    source_dacl,
                    std::ptr::null_mut(),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "SetSecurityInfo DACL failed: {}",
                    std::io::Error::from_raw_os_error(status as i32)
                )));
            }
            Ok(())
        },
    )
}

#[cfg(windows)]
fn apply_windows_security_parts(
    owner_information: u32,
    apply_owner_group: impl FnOnce() -> std::io::Result<()>,
    apply_dacl: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    // A restrictive source DACL may deny reopening with WRITE_OWNER, so owner
    // and group must be complete before the DACL is installed.
    if owner_information != 0 {
        apply_owner_group()?;
    }
    apply_dacl()
}

#[cfg(any(unix, windows))]
fn require_single_link(links: u64) -> std::io::Result<()> {
    if links != 1 {
        Err(std::io::Error::other(
            "query uninstall tombstone must have exactly one hard link",
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_file_information(
    file: &fs::File,
) -> std::io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` keeps this handle valid for the call, and Windows
    // initializes the complete output structure whenever it reports success.
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: GetFileInformationByHandle succeeded above.
    Ok(unsafe { information.assume_init() })
}

fn clear_uninstall_tombstone(
    queries_parent: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    validate_safe_language_name(language)?;
    let _replace_lock = if queries_parent.exists() {
        Some(QueryReplaceLockGuard::acquire(queries_parent, language)?)
    } else {
        None
    };
    match fs::remove_file(uninstall_tombstone_path(queries_parent, language)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(QueryInstallError::IoError(e)),
    }
}

pub fn clear_uninstall_tombstone_for_install(
    data_dir: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    clear_uninstall_tombstone(&data_dir.join("queries"), language)
}

fn backup_is_owned(path: &Path) -> bool {
    backup_ownership_sidecar(path).is_file()
}

fn newest_complete_backup_dir(
    queries_parent: &Path,
    language: &str,
) -> Result<Option<PathBuf>, QueryInstallError> {
    let entries = match fs::read_dir(queries_parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(QueryInstallError::IoError(e)),
    };
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !generated_backup_matches_language(name, language) {
            continue;
        }
        if !backup_is_owned(&path) || !query_install_is_complete(&path) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .is_none_or(|(current, _)| modified > *current)
        {
            newest = Some((modified, path));
        }
    }

    Ok(newest.map(|(_, path)| path))
}

fn write_query_file(file_path: &Path, content: &str) -> Result<(), QueryInstallError> {
    let mut file = fs::File::create(file_path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

fn write_install_marker(queries_dir: &Path) -> Result<(), QueryInstallError> {
    let mut file = fs::File::create(queries_dir.join(QUERY_INSTALL_COMPLETE_MARKER))?;
    file.write_all(b"ok\n")?;
    Ok(())
}

fn backup_ownership_sidecar(backup_dir: &Path) -> PathBuf {
    backup_dir.with_file_name(format!(
        "{}{}",
        backup_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(".query.backup"),
        QUERY_BACKUP_OWNERSHIP_MARKER
    ))
}

fn backup_ownership_sidecar_name(backup_name: &str) -> String {
    format!("{backup_name}{QUERY_BACKUP_OWNERSHIP_MARKER}")
}

fn write_backup_ownership_marker(backup_dir: &Path) -> Result<(), QueryInstallError> {
    let mut file = fs::File::create(backup_ownership_sidecar(backup_dir))?;
    file.write_all(b"ok\n")?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_install_marker_for_tests(queries_dir: &Path) -> Result<(), QueryInstallError> {
    write_install_marker(queries_dir)
}

enum ReplaceQueryDirResult {
    Replaced,
    AlreadyComplete,
    Uninstalled,
}

fn replace_query_dir(
    tmp_queries_dir: &Path,
    queries_dir: &Path,
    language: &str,
    force: bool,
) -> Result<ReplaceQueryDirResult, QueryInstallError> {
    let _replace_lock = QueryReplaceLockGuard::acquire(
        queries_dir
            .parent()
            .ok_or_else(|| QueryInstallError::IoError(std::io::Error::other("missing parent")))?,
        language,
    )?;

    if !force && query_install_is_complete(queries_dir) {
        return Ok(ReplaceQueryDirResult::AlreadyComplete);
    }
    if uninstall_tombstone_path(
        queries_dir
            .parent()
            .ok_or_else(|| QueryInstallError::IoError(std::io::Error::other("missing parent")))?,
        language,
    )
    .is_file()
    {
        return Ok(ReplaceQueryDirResult::Uninstalled);
    }

    if !queries_dir.exists() {
        fs::rename(tmp_queries_dir, queries_dir)?;
        return Ok(ReplaceQueryDirResult::Replaced);
    }

    let backup_dir = unique_backup_query_dir(queries_dir, language);
    write_backup_ownership_marker(&backup_dir)?;
    if let Err(e) = fs::rename(queries_dir, &backup_dir) {
        let _ = fs::remove_file(backup_ownership_sidecar(&backup_dir));
        // The target vanished between the exists() check above and this
        // rename (external cleanup — the lock only serializes kakehashi's own
        // installers): nothing to back up, so publish the staged dir instead
        // of aborting the install.
        if e.kind() == std::io::ErrorKind::NotFound {
            fs::rename(tmp_queries_dir, queries_dir)?;
            return Ok(ReplaceQueryDirResult::Replaced);
        }
        return Err(QueryInstallError::IoError(e));
    }

    if let Err(e) = fs::rename(tmp_queries_dir, queries_dir) {
        match fs::rename(&backup_dir, queries_dir) {
            Ok(()) => {
                let _ = fs::remove_file(backup_ownership_sidecar(&backup_dir));
            }
            Err(rollback_error) => {
                return Err(QueryInstallError::IoError(std::io::Error::other(format!(
                    "failed to publish staged queries: {e}; failed to restore backup: {rollback_error}"
                ))));
            }
        }
        return Err(QueryInstallError::IoError(e));
    }

    if fs::remove_dir_all(&backup_dir).is_ok() {
        let _ = fs::remove_file(backup_ownership_sidecar(&backup_dir));
    }
    Ok(ReplaceQueryDirResult::Replaced)
}

struct QueryReplaceLockGuard {
    _file: fs::File,
}

impl QueryReplaceLockGuard {
    fn acquire(queries_parent: &Path, language: &str) -> Result<Self, QueryInstallError> {
        validate_safe_language_name(language)?;
        let path = queries_parent.join(format!(".{}.replace.lock", language));
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }

    fn acquire_at(
        queries_parent: &cap_std::fs::Dir,
        language: &str,
    ) -> Result<Self, QueryInstallError> {
        validate_safe_language_name(language)?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(false);
        let file = queries_parent
            .open_with(format!(".{language}.replace.lock"), &options)?
            .into_std();
        file.lock_exclusive()?;
        Ok(Self { _file: file })
    }
}

/// Download a file from a URL.
fn download_file(url: &str, http_policy: QueryHttpPolicy) -> Result<String, QueryInstallError> {
    validate_url_http_policy(url, http_policy)?;
    let agent = match http_policy {
        QueryHttpPolicy::HttpsOnly => agent_with_timeout(QUERY_HTTP_TIMEOUT),
        #[cfg(test)]
        QueryHttpPolicy::AllowHttpForTests => agent_with_timeout_allowing_http(QUERY_HTTP_TIMEOUT),
    };

    let mut response = agent.get(url).call().map_err(|e| match e {
        ureq::Error::StatusCode(code) => QueryInstallError::HttpStatus {
            code,
            url: url.to_string(),
        },
        ureq::Error::RequireHttpsOnly(_) => QueryInstallError::HttpsOnly {
            url: url.to_string(),
        },
        e => QueryInstallError::HttpError(e.to_string()),
    })?;

    response
        .body_mut()
        .read_to_string()
        .map_err(|e| QueryInstallError::HttpError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn remove_dir_all_tolerates_a_confirmed_vanished_dir() {
        // The dir disappearing between the caller's observation and the
        // removal (external cleanup) must read as already-removed, not fail
        // the uninstall.
        let temp = TempDir::new().unwrap();
        let gone = temp.path().join("never-created");

        assert!(
            matches!(remove_dir_all_tolerating_vanished(&gone), Ok(false)),
            "a confirmed-absent dir is the desired end state (nothing removed)"
        );
    }

    #[test]
    fn remove_dir_all_removes_a_dir_with_contents() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("queries-lang");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("highlights.scm"), "(x) @y").unwrap();

        assert!(
            remove_dir_all_tolerating_vanished(&dir).expect("normal removal succeeds"),
            "an actual removal reports true"
        );
        assert!(!dir.exists(), "the dir and its contents are removed");
    }

    /// Serve canned query files over HTTP from an OS-assigned local port.
    fn spawn_query_file_server(routes: Vec<(&str, &str)>) -> String {
        use std::io::{BufRead, BufReader, Write};

        let routes: Vec<(String, String)> = routes
            .into_iter()
            .map(|(p, b)| (p.to_string(), b.to_string()))
            .collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            // Bounded so the thread (and its socket) terminates instead of
            // living until process exit: no test downloads anywhere near this
            // many files (2 query files per language, short inherits chains).
            for stream in listener.incoming().take(64) {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(&mut stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut header = String::new();
                loop {
                    header.clear();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" || header == "\n" => break,
                        Ok(_) => {}
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("");
                let response = match routes.iter().find(|(p, _)| p == path) {
                    Some((_, body)) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        base_url
    }

    #[cfg(unix)]
    fn dead_test_pid() -> u32 {
        let mut pid = std::process::id().saturating_add(100_000);
        while process_is_running(&pid.to_string()) {
            pid = pid.saturating_add(1);
        }
        pid
    }

    /// The rejected name flows into the error's `Display` (printed raw by
    /// the CLI), so control characters in an untrusted name must be escaped
    /// before they reach terminal output.
    #[test]
    fn unsafe_language_name_error_escapes_control_characters() {
        let temp = TempDir::new().unwrap();
        let result = install_queries_with_dependencies_from(
            "http://127.0.0.1:1",
            "evil\u{1b}[31m",
            temp.path(),
            false,
        );
        match result {
            Err(QueryInstallError::InvalidLanguageName(name)) => {
                assert!(
                    !name.contains('\u{1b}'),
                    "stored name must not carry raw escape bytes: {:?}",
                    name
                );
            }
            other => panic!("expected InvalidLanguageName, got {:?}", other.err()),
        }
    }

    #[test]
    fn production_query_install_rejects_plain_http_base_url() {
        let temp = TempDir::new().unwrap();
        let result =
            install_queries_with_dependencies_from("http://127.0.0.1:1", "lua", temp.path(), false);

        assert!(
            matches!(result, Err(QueryInstallError::HttpsOnly { url }) if url == "http://127.0.0.1:1/lua/highlights.scm"),
            "plain HTTP downloads should fail before being reported as missing queries"
        );
    }

    #[test]
    fn clear_uninstall_tombstone_rejects_unsafe_language_before_path_use() {
        let temp = TempDir::new().unwrap();
        let data_dir = temp.path();
        fs::create_dir_all(data_dir.join("queries/.a")).unwrap();
        fs::write(data_dir.join("victim.uninstalled"), "keep").unwrap();

        let result = clear_uninstall_tombstone_for_install(data_dir, "a/../../victim");

        assert!(
            matches!(result, Err(QueryInstallError::InvalidLanguageName(_))),
            "unsafe language must be rejected before tombstone path construction"
        );
        assert_eq!(
            fs::read_to_string(data_dir.join("victim.uninstalled")).unwrap(),
            "keep",
            "unsafe tombstone cleanup must not escape queries/"
        );
    }

    #[test]
    fn installed_queries_skip_plain_http_sentinel_base_url() {
        let temp = TempDir::new().unwrap();
        let queries_dir = temp.path().join("queries").join("lua");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "(comment) @comment\n").unwrap();

        let result =
            install_queries_with_dependencies_from("http://127.0.0.1:1", "lua", temp.path(), false);

        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(path)) if path == queries_dir),
            "already-installed queries must not validate an unused HTTP sentinel URL"
        );
    }

    #[test]
    fn missing_required_highlights_remains_language_not_supported() {
        let temp = TempDir::new().unwrap();
        let base_url = spawn_query_file_server(vec![]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "missing_lang",
            temp.path(),
            false,
        );

        assert!(
            matches!(result, Err(QueryInstallError::LanguageNotSupported(lang)) if lang == "missing_lang"),
            "404 for required highlights.scm still means the language has no query support"
        );
    }

    #[test]
    fn download_file_preserves_http_status_code() {
        let base_url = spawn_query_file_server(vec![]);
        let result = download_file(
            &format!("{base_url}/missing_lang/highlights.scm"),
            QueryHttpPolicy::AllowHttpForTests,
        );

        assert!(
            matches!(result, Err(QueryInstallError::HttpStatus { code: 404, .. })),
            "download errors should preserve structured status codes"
        );
    }

    #[test]
    fn remove_query_install_rejects_unsafe_language_before_creating_queries_parent() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");

        let result = remove_query_install_and_backups(&queries_parent, "a/../../victim");

        assert!(
            matches!(result, Err(QueryInstallError::InvalidLanguageName(_))),
            "unsafe language must be rejected before cleanup paths are derived"
        );
        assert!(
            !queries_parent.exists(),
            "unsafe cleanup must not even create the queries directory"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_query_install_rejects_symlinked_uninstall_tombstone() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep me\n").unwrap();
        symlink(&victim, uninstall_tombstone_path(&queries_parent, "rust")).unwrap();

        let result = remove_query_install_and_backups(&queries_parent, "rust");

        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "keep me\n",
            "opening the tombstone must not follow and overwrite its target"
        );
        assert!(
            matches!(result, Err(QueryInstallError::IoError(_))),
            "a managed tombstone symlink must fail the uninstall"
        );
    }

    #[test]
    fn remove_query_install_reuses_regular_uninstall_tombstone() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "stale tombstone contents\n").unwrap();

        remove_query_install_and_backups(&queries_parent, "rust").unwrap();

        assert_eq!(fs::read_to_string(tombstone).unwrap(), "ok\n");
    }

    #[test]
    #[cfg(unix)]
    fn remove_query_install_supports_symlinked_queries_parent() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let real_queries = temp.path().join("real-queries");
        fs::create_dir_all(&real_queries).unwrap();
        let linked_queries = temp.path().join("queries");
        symlink(&real_queries, &linked_queries).unwrap();

        remove_query_install_and_backups(&linked_queries, "rust").unwrap();

        assert_eq!(
            fs::read_to_string(uninstall_tombstone_path(&real_queries, "rust")).unwrap(),
            "ok\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_query_install_stays_bound_when_symlinked_parent_is_retargeted() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let original = temp.path().join("original-queries");
        let replacement = temp.path().join("replacement-queries");
        fs::create_dir_all(original.join("rust")).unwrap();
        fs::create_dir_all(replacement.join("rust")).unwrap();
        fs::write(original.join("rust/highlights.scm"), "original\n").unwrap();
        fs::write(replacement.join("rust/highlights.scm"), "outside\n").unwrap();
        let linked = temp.path().join("queries");
        symlink(&original, &linked).unwrap();

        remove_query_install_and_backups_with_hooks(
            &linked,
            "rust",
            |pinned, ambient_parent, language| {
                fs::remove_file(ambient_parent).unwrap();
                symlink(&replacement, ambient_parent).unwrap();
                write_uninstall_tombstone_at(pinned, ambient_parent, language)
            },
            |_| {},
        )
        .unwrap();

        assert!(
            !original.join("rust").exists(),
            "cleanup must stay in the directory that received durable intent"
        );
        assert_eq!(
            fs::read_to_string(replacement.join("rust/highlights.scm")).unwrap(),
            "outside\n",
            "retargeting must not redirect destructive cleanup"
        );
        assert!(
            uninstall_tombstone_path(&original, "rust").exists(),
            "the pinned directory retains its uninstall intent"
        );
        assert!(
            !uninstall_tombstone_path(&replacement, "rust").exists(),
            "the replacement directory must remain untouched"
        );
    }

    #[test]
    fn remove_query_install_rejects_hard_linked_uninstall_tombstone() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep me\n").unwrap();
        fs::hard_link(&victim, uninstall_tombstone_path(&queries_parent, "rust")).unwrap();

        let result = remove_query_install_and_backups(&queries_parent, "rust");

        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "keep me\n",
            "opening the tombstone must not overwrite another hard-link name"
        );
        assert!(
            matches!(result, Err(QueryInstallError::IoError(_))),
            "an already multiply linked managed tombstone must fail the uninstall"
        );
    }

    #[test]
    fn uninstall_sync_failure_preserves_queries_and_backups() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let queries_dir = queries_parent.join("rust");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "old queries\n").unwrap();
        let backup = queries_parent.join(".rust.123.0.backup");
        fs::create_dir(&backup).unwrap();
        fs::write(backup.join("highlights.scm"), "backup\n").unwrap();
        write_backup_ownership_marker(&backup).unwrap();

        let result = remove_query_install_and_backups_with_tombstone_writer(
            &queries_parent,
            "rust",
            |_, _, _| {
                Err(QueryInstallError::IoError(std::io::Error::other(
                    "injected durability failure",
                )))
            },
        );

        assert!(matches!(result, Err(QueryInstallError::IoError(_))));
        assert!(queries_dir.exists(), "canonical queries are not removed");
        assert!(backup.exists(), "owned backup is not removed");
        assert!(
            backup_ownership_sidecar(&backup).exists(),
            "backup ownership evidence is not removed"
        );
    }

    #[test]
    fn tombstone_publish_does_not_truncate_raced_hard_link_alias() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "old tombstone\n").unwrap();
        let outside_alias = temp.path().join("outside-alias");

        write_uninstall_tombstone_with_before_publish(
            &queries_parent,
            "rust",
            |path, _| {
                fs::hard_link(path, &outside_alias).unwrap();
            },
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(&outside_alias).unwrap(),
            "old tombstone\n",
            "a link raced after validation must retain the old inode contents"
        );
        assert_eq!(fs::read_to_string(tombstone).unwrap(), "ok\n");
    }

    #[test]
    #[cfg(unix)]
    fn tombstone_publish_rejects_substituted_private_stage() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep me\n").unwrap();

        let result = write_uninstall_tombstone_with_before_publish(
            &queries_parent,
            "rust",
            |_, staged_path| {
                fs::remove_file(staged_path).unwrap();
                symlink(&victim, staged_path).unwrap();
            },
            |_| Ok(()),
        );

        assert!(
            result.is_err(),
            "publication must reject a stage path detached from its opened handle"
        );
        assert_eq!(fs::read_to_string(victim).unwrap(), "keep me\n");
    }

    #[test]
    fn failed_tombstone_publish_cleans_private_stage() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "old tombstone\n").unwrap();

        let result = write_uninstall_tombstone_with_before_publish(
            &queries_parent,
            "rust",
            |path, _| {
                fs::remove_file(path).unwrap();
                fs::create_dir(path).unwrap();
            },
            |_| Ok(()),
        );

        assert!(result.is_err(), "a directory cannot be replaced by a file");
        let names: Vec<_> = fs::read_dir(&queries_parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![tombstone.file_name().unwrap().to_os_string()],
            "the private stage must be removed on every persist failure"
        );
    }

    #[test]
    fn tombstone_publish_propagates_parent_sync_failure() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let sync_called = std::cell::Cell::new(false);

        let result = write_uninstall_tombstone_with_before_publish(
            &queries_parent,
            "rust",
            |_, _| {},
            |_| {
                sync_called.set(true);
                Err(std::io::Error::other("injected parent sync failure"))
            },
        );

        assert!(sync_called.get(), "publication must cross the sync gate");
        assert!(
            matches!(result, Err(QueryInstallError::IoError(_))),
            "sync failure must prevent uninstall from reaching removal"
        );
    }

    #[test]
    #[cfg(unix)]
    fn tombstone_atomic_publish_preserves_file_create_umask_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let control = queries_parent.join("control");
        fs::File::create(&control).unwrap();

        write_uninstall_tombstone(&queries_parent, "rust").unwrap();

        let expected = fs::metadata(control).unwrap().permissions().mode() & 0o777;
        let actual = fs::metadata(uninstall_tombstone_path(&queries_parent, "rust"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(actual, expected);
    }

    #[test]
    #[cfg(unix)]
    fn tombstone_atomic_publish_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "old\n").unwrap();
        fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();

        write_uninstall_tombstone(&queries_parent, "rust").unwrap();

        assert_eq!(
            fs::metadata(tombstone).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[cfg(windows)]
    fn remove_query_install_supports_windows_symlinked_queries_parent() {
        use std::os::windows::fs::symlink_dir;

        let temp = TempDir::new().unwrap();
        let real_queries = temp.path().join("real-queries");
        fs::create_dir_all(&real_queries).unwrap();
        let linked_queries = temp.path().join("queries");
        symlink_dir(&real_queries, &linked_queries).unwrap();

        remove_query_install_and_backups(&linked_queries, "rust").unwrap();

        assert_eq!(
            fs::read_to_string(uninstall_tombstone_path(&real_queries, "rust")).unwrap(),
            "ok\n"
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_tombstone_publish_replaces_raced_symlink_entry() {
        use std::os::windows::fs::MetadataExt as _;
        use std::os::windows::fs::symlink_file;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;

        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "old\n").unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "keep me\n").unwrap();

        write_uninstall_tombstone_with_before_publish(
            &queries_parent,
            "rust",
            |path, _| {
                fs::remove_file(path).unwrap();
                symlink_file(&victim, path).unwrap();
            },
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(fs::read_to_string(victim).unwrap(), "keep me\n");
        assert!(
            fs::symlink_metadata(&tombstone)
                .unwrap()
                .file_type()
                .is_file(),
            "handle-relative replace must replace the symlink entry itself"
        );
        assert_eq!(fs::read_to_string(tombstone).unwrap(), "ok\n");
        assert_eq!(
            fs::metadata(uninstall_tombstone_path(&queries_parent, "rust"))
                .unwrap()
                .file_attributes()
                & FILE_ATTRIBUTE_TEMPORARY,
            0,
            "published tombstone must not retain tempfile cache semantics"
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_security_copy_failure_aborts_before_tombstone_publication() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(queries_parent.join("rust")).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "old\n").unwrap();
        let pinned =
            cap_std::fs::Dir::open_ambient_dir(&queries_parent, cap_std::ambient_authority())
                .unwrap();

        let result = write_uninstall_tombstone_at_with_security_copy(
            &pinned,
            &queries_parent,
            "rust",
            |_, _, _| {},
            |_| Ok(()),
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
            },
        );

        assert!(matches!(result, Err(QueryInstallError::IoError(_))));
        assert_eq!(fs::read_to_string(&tombstone).unwrap(), "old\n");
        assert!(queries_parent.join("rust").exists());
        assert_eq!(
            fs::read_dir(&queries_parent)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0,
            "a denied security copy must clean the private stage"
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_differing_owner_is_applied_before_restrictive_dacl() {
        use std::cell::RefCell;
        use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;

        let operations = RefCell::new(Vec::new());
        apply_windows_security_parts(
            OWNER_SECURITY_INFORMATION,
            || {
                if operations.borrow().contains(&"dacl") {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "restrictive DACL already installed",
                    ));
                }
                operations.borrow_mut().push("owner");
                Ok(())
            },
            || {
                operations.borrow_mut().push("dacl");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(operations.into_inner(), ["owner", "dacl"]);
    }

    #[test]
    #[cfg(windows)]
    fn windows_tombstone_preserves_null_protected_dacl() {
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, INVALID_HANDLE_VALUE, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl,
            PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, ReOpenFile,
            WRITE_DAC,
        };

        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tombstone = uninstall_tombstone_path(&queries_parent, "rust");
        fs::write(&tombstone, "old\n").unwrap();
        let existing = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tombstone)
            .unwrap();
        // SAFETY: success returns a distinct owned handle with WRITE_DAC.
        let security_handle = unsafe {
            ReOpenFile(
                existing.as_raw_handle(),
                READ_CONTROL | WRITE_DAC,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                0,
            )
        };
        assert_ne!(security_handle, INVALID_HANDLE_VALUE);
        // SAFETY: successful ReOpenFile returned a newly owned handle.
        let security_file = unsafe { fs::File::from_raw_handle(security_handle) };
        // SAFETY: the handle is valid; a null DACL is intentional and grants
        // full access while remaining observably distinct from an inherited ACL.
        let status = unsafe {
            SetSecurityInfo(
                security_file.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        drop(existing);

        write_uninstall_tombstone(&queries_parent, "rust").unwrap();

        let published = fs::File::open(&tombstone).unwrap();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: outputs live until descriptor is released below.
        let status = unsafe {
            GetSecurityInfo(
                published.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor was returned successfully by GetSecurityInfo.
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        assert!(dacl.is_null(), "the explicit null DACL must be preserved");
        assert_ne!(control & SE_DACL_PROTECTED, 0);
        // SAFETY: GetSecurityInfo allocates the descriptor with LocalAlloc.
        unsafe { LocalFree(descriptor.cast()) };
    }

    #[test]
    #[cfg(unix)]
    fn recover_interrupted_query_installs_removes_stranded_tmp_dirs() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tmp = queries_parent.join(format!(".lua.{}.0.tmp", dead_test_pid()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("highlights.scm"), "(comment) @comment\n").unwrap();

        recover_interrupted_query_installs(&queries_parent).unwrap();

        assert!(
            !tmp.exists(),
            "generated staging dirs from crashed installs should be collected"
        );
    }

    #[test]
    fn recover_interrupted_query_installs_preserves_live_tmp_dirs() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tmp = queries_parent.join(format!(".lua.{}.0.tmp", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("highlights.scm"), "(comment) @comment\n").unwrap();

        recover_interrupted_query_installs(&queries_parent).unwrap();

        assert!(
            tmp.exists(),
            "generated staging dirs from live installers must not be collected"
        );
    }

    #[test]
    #[cfg(unix)]
    fn remove_interrupted_temp_query_install_treats_missing_tmp_as_clean() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tmp = queries_parent.join(format!(".lua.{}.0.tmp", dead_test_pid()));

        remove_interrupted_temp_query_install(&queries_parent, "lua", &tmp).unwrap();
    }

    #[test]
    fn recover_interrupted_query_installs_ignores_unsafe_tmp_language_names() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tmp = queries_parent.join(".bad-name.123.0.tmp");
        fs::create_dir_all(&tmp).unwrap();

        recover_interrupted_query_installs(&queries_parent).unwrap();

        assert!(
            tmp.exists(),
            "tmp cleanup must only derive paths from safe generated language names"
        );
    }

    /// Inherited language names become path segments (`queries/<name>/`) and
    /// URL segments, so anything outside nvim-treesitter's `[a-z0-9_]+`
    /// naming must be dropped — `; inherits: ../../x` from a compromised or
    /// custom query source must not escape the data dir.
    #[test]
    fn parse_inherits_directive_drops_unsafe_language_names() {
        let parents = parse_inherits_directive(
            "; inherits: ../../evil, html_tags, UPPER, with-dash, c3\n(comment) @comment\n",
        );
        assert_eq!(
            parents,
            vec!["html_tags".to_string(), "c3".to_string()],
            "only lowercase/digit/underscore names may survive"
        );
    }

    #[test]
    fn test_install_queries_creates_directory_structure() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        // This test requires network access - skip in CI if needed
        let result = install_queries_with_dependencies("lua", &data_dir, false);

        // The test may fail due to network issues, but structure should be correct
        if let Ok(result) = result {
            assert_eq!(result.language, "lua");
            assert!(result.install_path.exists());
            assert!(
                result
                    .files_downloaded
                    .contains(&"highlights.scm".to_string())
            );
        }
    }

    #[test]
    fn install_with_dependencies_survives_inheritance_cycles_on_disk() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        // Self-cycle: a query file inheriting its own language (a one-word
        // typo in a real highlights.scm). No network: both branches hit the
        // already-exists path.
        let a_dir = data_dir.join("queries").join("cyclic_a");
        fs::create_dir_all(&a_dir).unwrap();
        std::fs::write(a_dir.join("highlights.scm"), "; inherits: cyclic_a\n").unwrap();
        write_install_marker_for_tests(&a_dir).unwrap();

        let result = install_queries_with_dependencies("cyclic_a", &data_dir, false);
        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(_))),
            "self-inheriting installed queries must terminate with AlreadyExists"
        );

        // Mutual cycle between two installed languages.
        let b_dir = data_dir.join("queries").join("cyclic_b");
        let c_dir = data_dir.join("queries").join("cyclic_c");
        fs::create_dir_all(&b_dir).unwrap();
        fs::create_dir_all(&c_dir).unwrap();
        std::fs::write(b_dir.join("highlights.scm"), "; inherits: cyclic_c\n").unwrap();
        std::fs::write(c_dir.join("highlights.scm"), "; inherits: cyclic_b\n").unwrap();
        write_install_marker_for_tests(&b_dir).unwrap();
        write_install_marker_for_tests(&c_dir).unwrap();

        let result = install_queries_with_dependencies("cyclic_b", &data_dir, false);
        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(_))),
            "mutually-inheriting installed queries must terminate with AlreadyExists"
        );
    }

    #[test]
    fn test_install_queries_returns_error_for_nonexistent_language() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();

        let result =
            install_queries_with_dependencies("nonexistent_language_xyz_123", &data_dir, false);

        assert!(result.is_err());
        if let Err(QueryInstallError::LanguageNotSupported(lang)) = result {
            assert_eq!(lang, "nonexistent_language_xyz_123");
        }
    }

    #[test]
    fn test_install_queries_respects_force_flag() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let queries_dir = data_dir.join("queries").join("lua");

        // Create existing directory
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "existing content").unwrap();
        write_install_marker_for_tests(&queries_dir).unwrap();

        // Without force, should error
        let result = install_queries_with_dependencies("lua", &data_dir, false);
        assert!(matches!(result, Err(QueryInstallError::AlreadyExists(_))));

        // With force, should succeed (requires network)
        // Skip actual download test to avoid flaky CI
    }

    #[test]
    fn install_repairs_partial_query_dir_without_force() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let queries_dir = data_dir.join("queries").join("partial_lang");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "").unwrap();
        fs::write(queries_dir.join("injections.scm"), "stale optional query").unwrap();

        let base_url = spawn_query_file_server(vec![(
            "/partial_lang/highlights.scm",
            "(identifier) @variable\n",
        )]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "partial_lang",
            &data_dir,
            false,
        )
        .expect("partial install should be repaired");

        assert_eq!(result.install_path, queries_dir);
        assert_eq!(result.files_downloaded, vec!["highlights.scm"]);
        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "(identifier) @variable\n"
        );
        assert!(
            !queries_dir.join("injections.scm").exists(),
            "repair should replace stale partial contents with the successful download"
        );
    }

    #[test]
    fn install_preserves_legacy_non_marker_query_dir_without_force() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let queries_dir = data_dir.join("queries").join("legacy_lang");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "legacy highlights").unwrap();
        fs::write(queries_dir.join("bindings.scm"), "user managed query").unwrap();

        let base_url = spawn_query_file_server(vec![(
            "/legacy_lang/highlights.scm",
            "replacement highlights\n",
        )]);

        let result =
            install_queries_with_dependencies_from(&base_url, "legacy_lang", &data_dir, false);

        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(path)) if path == queries_dir),
            "legacy query dir should be treated as already installed"
        );
        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "legacy highlights",
            "non-force install must not overwrite legacy highlights"
        );
        assert_eq!(
            fs::read_to_string(queries_dir.join("bindings.scm")).unwrap(),
            "user managed query",
            "non-force install must preserve user-managed query files"
        );
    }

    #[test]
    fn install_treats_marker_with_empty_highlights_as_complete() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let base_url = spawn_query_file_server(vec![("/empty_lang/highlights.scm", "")]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "empty_lang",
            &data_dir,
            false,
        )
        .expect("empty staged highlights should install");
        assert_eq!(result.files_downloaded, vec!["highlights.scm"]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "empty_lang",
            &data_dir,
            false,
        );
        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(_))),
            "marker should make an empty staged highlights.scm count as complete"
        );
    }

    #[test]
    fn replace_query_dir_aborts_when_uninstall_tombstone_exists() {
        let temp_dir = TempDir::new().unwrap();
        let queries_parent = temp_dir.path().join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        let tmp_queries_dir = create_unique_temp_query_dir(&queries_parent, "raced_lang").unwrap();
        fs::write(
            tmp_queries_dir.join("highlights.scm"),
            "(comment) @comment\n",
        )
        .unwrap();
        write_install_marker_for_tests(&tmp_queries_dir).unwrap();
        write_uninstall_tombstone(&queries_parent, "raced_lang").unwrap();

        let result = replace_query_dir(
            &tmp_queries_dir,
            &queries_parent.join("raced_lang"),
            "raced_lang",
            false,
        );

        assert!(
            matches!(result, Ok(ReplaceQueryDirResult::Uninstalled)),
            "replacement should observe uninstall tombstone under the lock"
        );
        assert!(
            !queries_parent.join("raced_lang").exists(),
            "uninstall tombstone must prevent restoring canonical queries"
        );
    }

    #[test]
    fn force_reinstall_preserves_existing_queries_on_required_download_failure() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let queries_dir = data_dir.join("queries").join("stable_lang");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "working highlights").unwrap();
        fs::write(queries_dir.join("injections.scm"), "working injections").unwrap();
        write_install_marker_for_tests(&queries_dir).unwrap();

        let base_url = spawn_query_file_server(vec![]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "stable_lang",
            &data_dir,
            true,
        );

        assert!(
            matches!(result, Err(QueryInstallError::LanguageNotSupported(lang)) if lang == "stable_lang"),
            "required highlights download failure should be reported"
        );
        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "working highlights",
            "force reinstall must not destroy previously working highlights"
        );
        assert_eq!(
            fs::read_to_string(queries_dir.join("injections.scm")).unwrap(),
            "working injections",
            "force reinstall must not destroy previously working optional queries"
        );
    }
}
