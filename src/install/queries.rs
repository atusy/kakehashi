//! Query file downloading from nvim-treesitter repository.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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

/// Stage the queries for `language` and its `; inherits:` chain from
/// `base_url`, leaving publication to the caller.
///
/// Used by the language installer, which stages both halves of a language
/// before publishing either.
pub(crate) fn stage_queries_with_dependencies_from(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<StagedQueryInstall, QueryInstallError> {
    stage_queries_with_dependencies(
        base_url,
        language,
        data_dir,
        force,
        QueryHttpPolicy::HttpsOnly,
    )
}

/// Like [`stage_queries_with_dependencies_from`] but disables the HTTPS-only
/// policy for tests that serve fixture query files over local plain HTTP.
#[cfg(test)]
pub(crate) fn stage_queries_with_dependencies_from_allowing_http_for_tests(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
) -> Result<StagedQueryInstall, QueryInstallError> {
    stage_queries_with_dependencies(
        base_url,
        language,
        data_dir,
        force,
        QueryHttpPolicy::AllowHttpForTests,
    )
}

/// Like [`install_queries_with_dependencies`] but downloading from `base_url`.
///
/// Production installs stage and publish in separate steps (see
/// [`stage_queries_with_dependencies_from`]); this one-shot form is what the
/// tests that only exercise the queries half use.
#[cfg(test)]
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
    let staged = stage_queries_with_dependencies(base_url, language, data_dir, force, http_policy)?;
    match staged.publish()?.commit() {
        CommittedQueryInstall::Installed(result) => Ok(result),
        // The `install_queries_*` entry points predate staging and report an
        // untouched language as `AlreadyExists`; their callers read it as a
        // successful no-op.
        CommittedQueryInstall::AlreadyInstalled(path) => {
            Err(QueryInstallError::AlreadyExists(path))
        }
    }
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

/// One language's query files downloaded into a staging directory, waiting to
/// be renamed into `queries/<language>`.
struct StagedQueryDir {
    language: String,
    queries_dir: PathBuf,
    /// The `force` this language was staged with — the install's flag for the
    /// requested language, and always `false` for an inherited parent, which is
    /// only ever fetched when it is missing.
    force: bool,
    tmp: TempQueryDirGuard,
}

/// Everything one install needs to publish: the requested language plus every
/// language it reaches through `; inherits:`.
///
/// Staging the whole dependency chain before publishing any of it is what makes
/// an install all-or-nothing — a parent that fails to download aborts the
/// install before anything is published, instead of leaving a language whose
/// inherited queries are missing.
pub(crate) struct StagedQueryInstall {
    language: String,
    /// Where the requested language's queries live once published.
    install_path: PathBuf,
    files_downloaded: Vec<String>,
    /// True when the requested language was already installed and only its
    /// missing parents (if any) were staged.
    requested_already_complete: bool,
    entries: Vec<StagedQueryDir>,
}

/// A query directory that has been renamed into place, with the directory it
/// displaced kept aside until the install as a whole is committed.
struct PublishedQueryDir {
    language: String,
    queries_dir: PathBuf,
    backup: Option<PathBuf>,
    /// Whether this is the language the install was asked for, as opposed to
    /// one it pulled in through `; inherits:`. Only the requested language is
    /// un-published on rollback (see [`PublishedQueryInstall::rollback`]).
    requested: bool,
}

/// The outcome of publishing a [`StagedQueryInstall`], still undoable.
pub(crate) struct PublishedQueryInstall {
    language: String,
    install_path: PathBuf,
    files_downloaded: Vec<String>,
    requested_already_complete: bool,
    published: Vec<PublishedQueryDir>,
}

impl StagedQueryInstall {
    /// Rename every staged directory into place, keeping the displaced
    /// directories so the whole install can still be undone.
    pub(crate) fn publish(self) -> Result<PublishedQueryInstall, QueryInstallError> {
        let Self {
            language: requested_language,
            install_path,
            files_downloaded,
            requested_already_complete,
            entries,
        } = self;
        let mut publish = PublishedQueryInstall {
            install_path,
            files_downloaded,
            requested_already_complete,
            language: requested_language.clone(),
            published: Vec::new(),
        };
        for entry in entries {
            let requested = entry.language == requested_language;
            // Each entry publishes with the `force` it was staged with, so an
            // inherited parent never overwrites a copy that appeared while this
            // install was busy compiling the parser.
            match publish_query_dir(
                &entry.tmp.path,
                &entry.queries_dir,
                &entry.language,
                entry.force,
            ) {
                Ok(PublishQueryDirOutcome::Published { backup }) => {
                    publish.published.push(PublishedQueryDir {
                        language: entry.language,
                        queries_dir: entry.queries_dir,
                        backup,
                        requested,
                    });
                }
                // A concurrent installer completed this language while we were
                // downloading. Its files are as good as ours, so keep them.
                Ok(PublishQueryDirOutcome::AlreadyComplete) => {
                    if requested {
                        publish.requested_already_complete = true;
                    }
                }
                Ok(PublishQueryDirOutcome::Uninstalled) => {
                    publish.rollback();
                    return Err(QueryInstallError::IoError(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        format!(
                            "Query install for {} was superseded by uninstall",
                            entry.language
                        ),
                    )));
                }
                Err(e) => {
                    publish.rollback();
                    return Err(e);
                }
            }
        }
        Ok(publish)
    }
}

/// What committing a publish left in place.
pub(crate) enum CommittedQueryInstall {
    /// This install published the requested language's queries.
    Installed(QueryInstallResult),
    /// The requested language's queries were already there, so only the
    /// parents it was missing (if any) were published.
    AlreadyInstalled(PathBuf),
}

impl PublishedQueryInstall {
    /// Make the publish final by discarding the displaced directories.
    ///
    /// Infallible on purpose. It runs after the parser has been published, so
    /// there is no half-installed state left to report — and a fallible commit
    /// would give callers an error arm whose only honest handling is to undo
    /// work that already succeeded.
    pub(crate) fn commit(mut self) -> CommittedQueryInstall {
        for published in std::mem::take(&mut self.published) {
            discard_backup_locked(&published);
        }
        if self.requested_already_complete {
            return CommittedQueryInstall::AlreadyInstalled(std::mem::take(&mut self.install_path));
        }
        CommittedQueryInstall::Installed(QueryInstallResult {
            language: std::mem::take(&mut self.language),
            install_path: std::mem::take(&mut self.install_path),
            files_downloaded: std::mem::take(&mut self.files_downloaded),
        })
    }

    /// Un-publish the requested language, restoring the directory it displaced.
    ///
    /// Only the requested language is un-published. Query files for the base
    /// languages it inherits are shared: another install running concurrently
    /// may already have seen ours and skipped staging its own, so removing them
    /// would break *its* language rather than undo ours. A base language whose
    /// queries are present but unused is inert — no parser is registered for it
    /// — so keeping them is the safe direction, and their backups are simply
    /// discarded.
    ///
    /// Best-effort: every step is the inverse of a rename that already
    /// succeeded, and a failure here is reported rather than retried, because
    /// the alternative — leaving a half-restored directory — is worse than
    /// telling the user what state they are in.
    ///
    /// Known gap: the lock keeps this out of another process's critical
    /// section, but nothing records *whose* publish the live directory is. A
    /// second process that installed the same language and committed while this
    /// one was compiling would have its work undone here. Closing that needs
    /// provenance in the install marker; same-language cross-process installs
    /// are rare enough that the marker format has not been changed for it.
    pub(crate) fn rollback(mut self) {
        for published in std::mem::take(&mut self.published) {
            if !published.requested {
                discard_backup_locked(&published);
                continue;
            }
            // Take the same lock the publish held, so the un-publish cannot
            // land inside another installer's or the uninstaller's critical
            // section.
            let Some(queries_parent) = published.queries_dir.parent() else {
                continue;
            };
            let _replace_lock =
                match QueryReplaceLockGuard::acquire(queries_parent, &published.language) {
                    Ok(guard) => guard,
                    Err(e) => {
                        eprintln!(
                            "Warning: could not lock '{}' to undo its query install: {}",
                            published.language, e
                        );
                        continue;
                    }
                };
            if let Err(e) = fs::remove_dir_all(&published.queries_dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!(
                    "Warning: failed to remove the queries published for '{}': {}. The data \
                     directory still holds them; run `kakehashi language uninstall {}` before \
                     retrying.",
                    published.language, e, published.language
                );
                continue;
            }
            let Some(backup) = &published.backup else {
                continue;
            };
            // An uninstall that landed while this install ran wants the
            // language gone; restoring the backup would resurrect it.
            if uninstall_tombstone_path(queries_parent, &published.language).is_file() {
                discard_backup(&published);
                continue;
            }
            match fs::rename(backup, &published.queries_dir) {
                Ok(()) => {
                    let _ = fs::remove_file(backup_ownership_sidecar(backup));
                }
                Err(e) => eprintln!(
                    "Warning: failed to restore the previous queries for '{}': {}. They are kept \
                     at {}.",
                    published.language,
                    e,
                    backup.display()
                ),
            }
        }
    }
}

impl Drop for PublishedQueryInstall {
    /// Backstop for an install abandoned without committing or rolling back —
    /// today only a panic between the two.
    ///
    /// Keep the publish and drop the backups: a stranded backup is only
    /// reachable by a later `language status`/`uninstall` sweep, so dropping it
    /// here is what keeps the common case clean.
    ///
    /// Unlocked, unlike `commit`: this can run while unwinding, and blocking a
    /// panic on another process's `flock` is worse than the race the lock
    /// avoids (a concurrent uninstall reporting a failure for work that did
    /// happen).
    fn drop(&mut self) {
        for published in std::mem::take(&mut self.published) {
            discard_backup(&published);
        }
    }
}

/// [`discard_backup`] under the language's replace lock.
///
/// `language uninstall` enumerates and removes owned backups while holding that
/// lock; deleting one underneath it makes its own removal fail on a directory
/// that is vanishing as it reads it, and the uninstall reports a failure for
/// work that did happen.
///
/// Never call this while already holding the lock for the same language: the
/// guard opens its own file descriptor, and `flock` blocks a second one even
/// within one process.
fn discard_backup_locked(published: &PublishedQueryDir) {
    let _replace_lock = published
        .queries_dir
        .parent()
        .map(|parent| QueryReplaceLockGuard::acquire(parent, &published.language))
        .transpose()
        .inspect_err(|e| {
            // Still discard: an uncollected backup is worse than an unlocked
            // removal, and the lock only avoids making a concurrent uninstall
            // report a failure for work that did happen.
            eprintln!(
                "Warning: could not lock '{}' before dropping its query backup: {}",
                published.language, e
            )
        });
    discard_backup(published);
}

/// Drop a displaced query directory and the sidecar that marks it as ours.
///
/// The sidecar is removed even when the directory is already gone: gating it on
/// a successful removal is how orphaned `.kakehashi-backup` files accumulate
/// when a concurrent uninstall collects the directory first.
fn discard_backup(published: &PublishedQueryDir) {
    let Some(backup) = &published.backup else {
        return;
    };
    discard_backup_dir(backup);
}

/// Remove a backup directory and, once it is confirmed gone, the sidecar that
/// marks it as ours.
///
/// The sidecar outlives a removal that *failed*: every collector — uninstall,
/// the recovery sweep, `newest_complete_backup_dir` — gates on it, so dropping
/// it while the directory survives would make that directory unreachable by all
/// of them. It is dropped when the directory is confirmed absent, which is how
/// a concurrent uninstall's collection stops leaving sidecars behind.
fn discard_backup_dir(backup: &Path) {
    match remove_dir_all_tolerating_vanished(backup) {
        Ok(_) => {
            let _ = fs::remove_file(backup_ownership_sidecar(backup));
        }
        Err(e) => eprintln!(
            "Warning: failed to remove the superseded query backup at {}: {}",
            backup.display(),
            e
        ),
    }
}

/// What staging found for one language.
enum StageOutcome {
    /// Query files were downloaded and are waiting to be published.
    Staged { files_downloaded: Vec<String> },
    /// The language's queries are already installed; nothing was downloaded.
    NothingToDo,
}

/// Stage the queries for `language` and everything it inherits from.
fn stage_queries_with_dependencies(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    http_policy: QueryHttpPolicy,
) -> Result<StagedQueryInstall, QueryInstallError> {
    let mut entries = Vec::new();
    let mut staged = std::collections::HashSet::new();
    // On any error the entries collected so far are dropped here, and with them
    // every staging directory: a failed install publishes nothing.
    let outcome = stage_queries_recursive(
        base_url,
        language,
        data_dir,
        force,
        &mut staged,
        &mut entries,
        http_policy,
    )?;
    let (files_downloaded, requested_already_complete) = match outcome {
        StageOutcome::Staged { files_downloaded } => (files_downloaded, false),
        StageOutcome::NothingToDo => (Vec::new(), true),
    };
    Ok(StagedQueryInstall {
        language: language.to_string(),
        install_path: data_dir.join("queries").join(language),
        files_downloaded,
        requested_already_complete,
        entries,
    })
}

/// Internal recursive helper for staging queries with dependencies.
///
/// Appends the requested language's staging directory to `entries` before
/// recursing, so publication order matches the old install order: a language
/// becomes visible before the parents it inherits from.
fn stage_queries_recursive(
    base_url: &str,
    language: &str,
    data_dir: &Path,
    force: bool,
    staged: &mut std::collections::HashSet<String>,
    entries: &mut Vec<StagedQueryDir>,
    http_policy: QueryHttpPolicy,
) -> Result<StageOutcome, QueryInstallError> {
    // The name becomes a path and URL segment below; reject anything that
    // could escape the data dir (e.g. a caller-provided `../../x`).
    // Escape the untrusted name: the error's Display is printed raw by
    // the CLI, so control characters must not reach the terminal.
    validate_safe_language_name(language)?;

    // Skip if already staged (or found complete) in this session
    if staged.contains(language) {
        return Ok(StageOutcome::NothingToDo);
    }

    let queries_dir = data_dir.join("queries").join(language);
    let queries_parent = data_dir.join("queries");
    fs::create_dir_all(&queries_parent)?;
    recover_interrupted_query_install(&queries_parent, language)?;

    // Check if queries already exist. A previous interrupted install may
    // leave a directory without the required highlights.scm; that is treated
    // as incomplete so a later install can repair it without --force.
    if query_install_is_complete(&queries_dir) && !force {
        // Mark as staged BEFORE recursing into parents: an inheritance
        // cycle among on-disk query files (self-inherit typo, A↔B) would
        // otherwise recurse forever and overflow the stack. The download
        // branch below already inserts before its parent loop.
        staged.insert(language.to_string());

        // Even if skipping, we need to check for inherited dependencies
        let highlights_path = queries_dir.join("highlights.scm");
        if highlights_path.exists()
            && let Ok(content) = std::fs::read_to_string(&highlights_path)
        {
            let parents = parse_inherits_directive(&content);
            for parent in parents {
                // Stage parent dependencies (don't force, just ensure they exist)
                clear_uninstall_tombstone(&queries_parent, &parent)?;
                stage_queries_recursive(
                    base_url,
                    &parent,
                    data_dir,
                    false,
                    staged,
                    entries,
                    http_policy,
                )?;
            }
        }
        return Ok(StageOutcome::NothingToDo);
    }

    let tmp_queries_dir = create_unique_temp_query_dir(&queries_parent, language)?;
    let staged_dir = StagedQueryDir {
        language: language.to_string(),
        queries_dir,
        force,
        tmp: TempQueryDirGuard {
            path: tmp_queries_dir.clone(),
        },
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

    staged.insert(language.to_string());
    entries.push(staged_dir);

    // Stage parent dependencies. A parent that cannot be downloaded fails the
    // whole install: propagating the error here drops every staging directory
    // collected so far, so a language is never published without the queries it
    // inherits.
    for parent in parents_to_install {
        eprintln!("Staging inherited queries: {}", parent);
        clear_uninstall_tombstone(&queries_parent, &parent)?;
        stage_queries_recursive(
            base_url,
            &parent,
            data_dir,
            false,
            staged,
            entries,
            http_policy,
        )?;
    }

    Ok(StageOutcome::Staged { files_downloaded })
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
/// the success path `publish_query_dir` renames the directory away, making
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
                collect_superseded_backups(queries_parent, &language)?;
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
    validate_safe_language_name(language)?;
    fs::create_dir_all(queries_parent)?;
    let _replace_lock = QueryReplaceLockGuard::acquire(queries_parent, language)?;
    write_uninstall_tombstone(queries_parent, language)?;
    let queries_dir = queries_parent.join(language);
    let mut removal = QueryRemoval {
        removed_queries: false,
        removed_backups: false,
    };

    // No exists() pre-check: Path::exists() reads false on metadata errors
    // (e.g. PermissionDenied), which would skip removal and report "not
    // installed" over a still-present unreadable dir. The tolerant removal
    // reports whether anything was actually removed.
    removal.removed_queries = remove_dir_all_tolerating_vanished(&queries_dir)?;

    // Propagate per-entry read_dir errors: uninstall must not report success
    // while backups it could not even enumerate stay behind.
    for entry in fs::read_dir(queries_parent)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // file_type() over path.is_dir(): is_dir() swallows metadata errors
        // as "not a directory", which could leave an unreadable backup behind
        // while uninstall reports success.
        if entry.file_type()?.is_dir()
            && generated_backup_matches_language(name, language)
            && backup_is_owned(&path)
        {
            let ownership = backup_ownership_sidecar(&path);
            // Same NotFound tolerance as the canonical dir above: a backup
            // deleted externally after enumeration is already the end state.
            let removed_dir = remove_dir_all_tolerating_vanished(&path)?;
            // The sidecar is a kakehashi-owned artifact too: deleting it
            // counts as removal even when the dir itself vanished first —
            // and, like every other I/O in this loop, only NotFound is
            // tolerated (an unremovable marker must fail the uninstall, not
            // linger behind a success report).
            let removed_sidecar = match fs::remove_file(ownership) {
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

/// Drop backups left behind by an install that died after publishing a query
/// directory but before committing.
///
/// [`recover_interrupted_query_install`] restores a backup only when the live
/// directory is missing. Once the publish landed, the directory it displaced is
/// superseded — and invisible to that recovery for exactly that reason — so
/// without this it would sit under `queries/` until the user uninstalled the
/// language by name. An install publishes one such backup per language in the
/// `; inherits:` chain, so the leak is per-chain, not per-install.
///
/// Only backups whose owning process is gone are collected, mirroring
/// [`remove_interrupted_temp_query_install`]; a live install's backup is still
/// its rollback target. As there, `process_is_running` is conservative off
/// unix, so nothing is collected there.
fn collect_superseded_backups(
    queries_parent: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    validate_safe_language_name(language)?;
    // "Superseded" means a *complete* directory took the backup's place. A live
    // directory that is merely present is not enough: an interrupted uninstall
    // can leave a partially emptied one, and then the backup is the only intact
    // copy — the same reason `newest_complete_backup_dir` refuses to restore an
    // incomplete backup, applied in the other direction. A publish always
    // produces a complete directory, so this still covers the case the sweep
    // exists for.
    if !query_install_is_complete(&queries_parent.join(language)) {
        return Ok(());
    }

    let _replace_lock = QueryReplaceLockGuard::acquire(queries_parent, language)?;
    // Re-check under the lock: a concurrent uninstall may have removed the
    // directory, which makes these backups restore candidates again.
    if !query_install_is_complete(&queries_parent.join(language)) {
        return Ok(());
    }

    let entries = match fs::read_dir(queries_parent) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(QueryInstallError::IoError(e)),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || !backup_is_owned(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((backup_language, pid, _)) = generated_backup_parts(name) else {
            continue;
        };
        if backup_language != language || process_is_running(pid) {
            continue;
        }
        discard_backup_dir(&path);
    }
    Ok(())
}

/// Hold the language's replace lock and report whether an uninstall claimed it.
///
/// The parser half has no lock of its own, so an install that published its
/// queries and then took minutes to compile could publish a parser for a
/// language `language uninstall` had removed in the meantime — leaving the
/// parser-only state the uninstall reported as gone. Checking the tombstone
/// under this lock, and keeping the lock until the parser rename is done,
/// serializes the two: uninstall removes the queries and writes the tombstone
/// under the same lock, and removes the parser after.
pub(crate) fn hold_against_uninstall(
    data_dir: &Path,
    language: &str,
) -> Result<UninstallClaim, QueryInstallError> {
    validate_safe_language_name(language)?;
    let queries_parent = data_dir.join("queries");
    // The lock lives beside the query directories, so the directory has to
    // exist to take it. Staging already created it; this only covers a caller
    // that skipped straight here.
    fs::create_dir_all(&queries_parent)?;
    let guard = QueryReplaceLockGuard::acquire(&queries_parent, language)?;
    if uninstall_tombstone_path(&queries_parent, language).is_file() {
        return Ok(UninstallClaim::Uninstalled);
    }
    Ok(UninstallClaim::Clear(UninstallGuard { _lock: guard }))
}

/// Whether an uninstall has claimed a language.
pub(crate) enum UninstallClaim {
    /// It has not, and will not while the guard is held.
    Clear(UninstallGuard),
    /// An uninstall removed this language; publishing anything for it now would
    /// resurrect half of what the user removed.
    Uninstalled,
}

/// Proof that no uninstall has claimed the language, held for as long as the
/// caller needs the answer to stay true.
pub(crate) struct UninstallGuard {
    _lock: QueryReplaceLockGuard,
}

fn uninstall_tombstone_path(queries_parent: &Path, language: &str) -> PathBuf {
    queries_parent.join(format!(".{language}{QUERY_UNINSTALL_TOMBSTONE_SUFFIX}"))
}

fn write_uninstall_tombstone(
    queries_parent: &Path,
    language: &str,
) -> Result<(), QueryInstallError> {
    validate_safe_language_name(language)?;
    let mut file = fs::File::create(uninstall_tombstone_path(queries_parent, language))?;
    file.write_all(b"ok\n")?;
    Ok(())
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

fn write_backup_ownership_marker(backup_dir: &Path) -> Result<(), QueryInstallError> {
    let mut file = fs::File::create(backup_ownership_sidecar(backup_dir))?;
    file.write_all(b"ok\n")?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_install_marker_for_tests(queries_dir: &Path) -> Result<(), QueryInstallError> {
    write_install_marker(queries_dir)
}

enum PublishQueryDirOutcome {
    /// The staged directory is now the live one. `backup` holds the directory
    /// it displaced, kept until the install commits so a later failure can put
    /// it back.
    Published {
        backup: Option<PathBuf>,
    },
    AlreadyComplete,
    Uninstalled,
}

fn publish_query_dir(
    tmp_queries_dir: &Path,
    queries_dir: &Path,
    language: &str,
    force: bool,
) -> Result<PublishQueryDirOutcome, QueryInstallError> {
    let _replace_lock = QueryReplaceLockGuard::acquire(
        queries_dir
            .parent()
            .ok_or_else(|| QueryInstallError::IoError(std::io::Error::other("missing parent")))?,
        language,
    )?;

    if !force && query_install_is_complete(queries_dir) {
        return Ok(PublishQueryDirOutcome::AlreadyComplete);
    }
    if uninstall_tombstone_path(
        queries_dir
            .parent()
            .ok_or_else(|| QueryInstallError::IoError(std::io::Error::other("missing parent")))?,
        language,
    )
    .is_file()
    {
        return Ok(PublishQueryDirOutcome::Uninstalled);
    }

    if !queries_dir.exists() {
        fs::rename(tmp_queries_dir, queries_dir)?;
        return Ok(PublishQueryDirOutcome::Published { backup: None });
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
            return Ok(PublishQueryDirOutcome::Published { backup: None });
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

    // The backup outlives this function: it is dropped by
    // `PublishedQueryInstall::commit` once every language in the install has
    // been published. A process killed inside that window strands it —
    // `recover_interrupted_query_install` will not restore it, `queries_dir`
    // exists again — which is what `collect_superseded_backups` sweeps up.
    Ok(PublishQueryDirOutcome::Published {
        backup: Some(backup_dir),
    })
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
        file.lock()?;
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
mod staging_tests {
    use super::*;
    use tempfile::TempDir;

    /// Entries under `queries/` that an install is expected to leave behind.
    ///
    /// The per-language replace locks are long-lived by design: they serialize
    /// concurrent installers and are reused, so they are not install residue.
    fn residue(queries_parent: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(queries_parent)
            .expect("read queries parent")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".replace.lock"))
            .collect();
        names.sort();
        names
    }

    /// Stage a query directory by hand, without touching the network.
    fn stage(queries_parent: &Path, language: &str, contents: &str, force: bool) -> StagedQueryDir {
        fs::create_dir_all(queries_parent).expect("create queries parent");
        let tmp_dir = create_unique_temp_query_dir(queries_parent, language).expect("stage dir");
        fs::write(tmp_dir.join("highlights.scm"), contents).expect("write staged highlights");
        write_install_marker(&tmp_dir).expect("write staged marker");
        StagedQueryDir {
            language: language.to_string(),
            queries_dir: queries_parent.join(language),
            force,
            tmp: TempQueryDirGuard { path: tmp_dir },
        }
    }

    /// An install abandoned before publication must leave `queries/` exactly as
    /// it was — including for the inherited parents staged alongside the
    /// requested language.
    #[test]
    fn dropping_a_staged_install_publishes_nothing() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let staged = StagedQueryInstall {
            language: "child".to_string(),
            install_path: queries_parent.join("child"),
            files_downloaded: vec!["highlights.scm".to_string()],
            requested_already_complete: false,
            entries: vec![
                stage(&queries_parent, "child", "; inherits: parent\n", false),
                stage(&queries_parent, "parent", "(comment) @comment\n", false),
            ],
        };

        drop(staged);

        let leftovers = residue(&queries_parent);
        assert!(
            leftovers.is_empty(),
            "an unpublished staged install must leave nothing behind, found {:?}",
            leftovers
        );
    }

    /// Rolling back an install that replaced existing queries must put the
    /// previous directory back and drop the ownership sidecar, so the next run
    /// sees a clean data dir rather than an orphaned backup.
    #[test]
    fn rolling_back_a_publish_restores_the_previous_queries() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let queries_dir = queries_parent.join("child");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "previous").unwrap();
        write_install_marker(&queries_dir).unwrap();
        let staged = StagedQueryInstall {
            language: "child".to_string(),
            install_path: queries_parent.join("child"),
            files_downloaded: vec!["highlights.scm".to_string()],
            requested_already_complete: false,
            entries: vec![stage(&queries_parent, "child", "replacement", true)],
        };

        let published = staged.publish().expect("publish should succeed");
        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "replacement",
            "publish must make the staged queries visible"
        );
        published.rollback();

        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "previous",
            "rollback must restore the queries that were there before"
        );
        let leftovers: Vec<_> = residue(&queries_parent)
            .into_iter()
            .filter(|name| name != "child")
            .collect();
        assert!(
            leftovers.is_empty(),
            "rollback must not strand a backup or its sidecar, found {:?}",
            leftovers
        );
    }

    /// The requested language had no queries before, so rollback must remove it
    /// again — leaving it behind is exactly the half-installed state staging
    /// exists to prevent. Its inherited parent stays: another install may
    /// already have skipped staging its own copy because ours was there.
    #[test]
    fn rolling_back_a_first_install_removes_only_the_requested_queries() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let staged = StagedQueryInstall {
            language: "child".to_string(),
            install_path: queries_parent.join("child"),
            files_downloaded: vec!["highlights.scm".to_string()],
            requested_already_complete: false,
            entries: vec![
                stage(&queries_parent, "child", "; inherits: parent\n", false),
                stage(&queries_parent, "parent", "(comment) @comment\n", false),
            ],
        };

        staged.publish().expect("publish should succeed").rollback();

        assert!(
            !queries_parent.join("child").exists(),
            "the requested language must not stay published"
        );
        assert_eq!(
            residue(&queries_parent),
            vec!["parent".to_string()],
            "an inherited parent must survive, with no backup or sidecar beside it"
        );
    }

    /// A published install that is neither committed nor rolled back — only a
    /// panic gets here — must still drop the directories it displaced, because
    /// nothing else collects them once the live directory is back in place.
    #[test]
    fn dropping_a_published_install_discards_the_backups() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let queries_dir = queries_parent.join("child");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "previous").unwrap();
        write_install_marker(&queries_dir).unwrap();
        let staged = StagedQueryInstall {
            language: "child".to_string(),
            install_path: queries_dir.clone(),
            files_downloaded: vec!["highlights.scm".to_string()],
            requested_already_complete: false,
            entries: vec![stage(&queries_parent, "child", "replacement", true)],
        };

        drop(staged.publish().expect("publish should succeed"));

        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "replacement",
            "an abandoned publish keeps the queries it made visible"
        );
        assert_eq!(
            residue(&queries_parent),
            vec!["child".to_string()],
            "the displaced directory and its sidecar must not be stranded"
        );
    }

    /// `--force` replaces the requested language, not the base languages it
    /// inherits: staging decided a parent was missing, and by publish time a
    /// concurrent install may have filled it in. Forcing over that would
    /// destroy a copy this install never intended to touch.
    #[test]
    fn forcing_the_requested_language_leaves_an_inherited_parent_alone() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let parent_dir = queries_parent.join("parent");
        let staged = StagedQueryInstall {
            language: "child".to_string(),
            install_path: queries_parent.join("child"),
            files_downloaded: vec!["highlights.scm".to_string()],
            requested_already_complete: false,
            entries: vec![
                stage(&queries_parent, "child", "; inherits: parent\n", true),
                stage(&queries_parent, "parent", "ours", false),
            ],
        };
        // The parent appears while this install is busy with the parser.
        fs::create_dir_all(&parent_dir).unwrap();
        fs::write(parent_dir.join("highlights.scm"), "theirs").unwrap();
        write_install_marker(&parent_dir).unwrap();

        staged.publish().expect("publish should succeed").commit();

        assert_eq!(
            fs::read_to_string(parent_dir.join("highlights.scm")).unwrap(),
            "theirs",
            "forcing the requested language must not overwrite an inherited parent"
        );
        assert_eq!(
            fs::read_to_string(queries_parent.join("child").join("highlights.scm")).unwrap(),
            "; inherits: parent\n",
            "the requested language is still published"
        );
        assert_eq!(
            residue(&queries_parent),
            vec!["child".to_string(), "parent".to_string()],
            "yielding to the parent must not leave a backup behind"
        );
    }

    /// Publishing stops at the first entry it cannot publish and undoes the
    /// requested language it had already made visible.
    #[test]
    fn publishing_stops_at_an_uninstalled_entry_and_restores_the_earlier_ones() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let queries_dir = queries_parent.join("child");
        fs::create_dir_all(&queries_dir).unwrap();
        fs::write(queries_dir.join("highlights.scm"), "previous").unwrap();
        write_install_marker(&queries_dir).unwrap();
        let staged = StagedQueryInstall {
            language: "child".to_string(),
            install_path: queries_dir.clone(),
            files_downloaded: vec!["highlights.scm".to_string()],
            requested_already_complete: false,
            entries: vec![
                stage(&queries_parent, "child", "replacement", true),
                stage(&queries_parent, "parent", "(comment) @comment\n", false),
            ],
        };
        write_uninstall_tombstone(&queries_parent, "parent").unwrap();

        let result = staged.publish();

        assert!(
            matches!(&result, Err(QueryInstallError::IoError(e)) if e.kind() == std::io::ErrorKind::Interrupted),
            "a tombstoned entry must abort the publish"
        );
        assert_eq!(
            fs::read_to_string(queries_dir.join("highlights.scm")).unwrap(),
            "previous",
            "the already-published requested language must be restored"
        );
        assert_eq!(
            residue(&queries_parent),
            vec![".parent.uninstalled".to_string(), "child".to_string()],
            "no backup or sidecar may be stranded"
        );
    }
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

    /// An install killed between publishing a query directory and committing
    /// leaves the directory it displaced behind, and the restore path skips it
    /// because the live directory is present again. Something has to collect it.
    #[test]
    #[cfg(unix)]
    fn recover_interrupted_query_installs_collects_superseded_backups() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let live = queries_parent.join("lua");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("highlights.scm"), "published").unwrap();
        write_install_marker(&live).unwrap();
        let backup = queries_parent.join(format!(".lua.{}.0.backup", dead_test_pid()));
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("highlights.scm"), "displaced").unwrap();
        write_install_marker(&backup).unwrap();
        write_backup_ownership_marker(&backup).unwrap();

        recover_interrupted_query_installs(&queries_parent).unwrap();

        assert!(!backup.exists(), "a superseded backup must be collected");
        assert!(
            !backup_ownership_sidecar(&backup).exists(),
            "its ownership sidecar must go with it"
        );
        assert_eq!(
            fs::read_to_string(live.join("highlights.scm")).unwrap(),
            "published",
            "collecting a backup must not disturb the live queries"
        );
    }

    /// The parser half has no lock of its own, so this is what keeps an
    /// install from publishing a parser for a language the user just removed.
    #[test]
    fn a_tombstoned_language_is_reported_as_uninstalled() {
        let temp = TempDir::new().unwrap();
        let data_dir = temp.path();
        let queries_parent = data_dir.join("queries");
        fs::create_dir_all(&queries_parent).unwrap();
        write_uninstall_tombstone(&queries_parent, "lua").unwrap();

        assert!(matches!(
            hold_against_uninstall(data_dir, "lua").unwrap(),
            UninstallClaim::Uninstalled
        ));
    }

    #[test]
    fn an_untouched_language_is_held_clear_of_uninstall() {
        let temp = TempDir::new().unwrap();

        let claim = hold_against_uninstall(temp.path(), "lua").unwrap();

        assert!(matches!(claim, UninstallClaim::Clear(_)));
        drop(claim);
        // The guard is released, so a second caller can take it in turn — a
        // guard that outlived its scope would hang here instead.
        assert!(matches!(
            hold_against_uninstall(temp.path(), "lua").unwrap(),
            UninstallClaim::Clear(_)
        ));
    }

    /// A live directory that is merely present is not proof the backup is
    /// superseded: an interrupted uninstall leaves a partially emptied one, and
    /// then the backup is the only intact copy.
    #[test]
    #[cfg(unix)]
    fn recover_interrupted_query_installs_keeps_a_backup_over_an_incomplete_dir() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let live = queries_parent.join("lua");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("bindings.scm"), "user managed query").unwrap();
        let backup = queries_parent.join(format!(".lua.{}.0.backup", dead_test_pid()));
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("highlights.scm"), "(comment) @comment\n").unwrap();
        write_install_marker(&backup).unwrap();
        write_backup_ownership_marker(&backup).unwrap();

        recover_interrupted_query_installs(&queries_parent).unwrap();

        assert!(
            backup.join("highlights.scm").exists(),
            "the only complete copy must survive an incomplete live directory"
        );
        assert!(
            backup_ownership_sidecar(&backup).is_file(),
            "and stay owned, so it is still a restore candidate"
        );
    }

    /// A backup whose install is still running is that install's rollback
    /// target, not garbage.
    #[test]
    fn recover_interrupted_query_installs_keeps_a_live_installs_backup() {
        let temp = TempDir::new().unwrap();
        let queries_parent = temp.path().join("queries");
        let live = queries_parent.join("lua");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("highlights.scm"), "published").unwrap();
        write_install_marker(&live).unwrap();
        let backup = queries_parent.join(format!(".lua.{}.0.backup", std::process::id()));
        fs::create_dir_all(&backup).unwrap();
        fs::write(backup.join("highlights.scm"), "displaced").unwrap();
        write_install_marker(&backup).unwrap();
        write_backup_ownership_marker(&backup).unwrap();

        recover_interrupted_query_installs(&queries_parent).unwrap();

        assert!(
            backup.exists(),
            "a running install's backup must not be collected"
        );
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

    /// Queries a language inherits are part of what makes it usable, so a
    /// parent that cannot be downloaded fails the whole install — and, because
    /// nothing is published until every dependency is staged, the data dir is
    /// left untouched rather than holding a language whose `; inherits:` chain
    /// is broken.
    #[test]
    fn a_failing_inherited_parent_publishes_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let base_url = spawn_query_file_server(vec![(
            "/orphan_child/highlights.scm",
            "; inherits: missing_parent\n(identifier) @variable\n",
        )]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "orphan_child",
            &data_dir,
            false,
        );

        assert!(
            matches!(result, Err(QueryInstallError::LanguageNotSupported(lang)) if lang == "missing_parent"),
            "an unavailable inherited parent must fail the install"
        );
        let leftovers: Vec<_> = fs::read_dir(data_dir.join("queries"))
            .expect("read queries parent")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            // The per-language replace locks are reused infrastructure, not
            // install residue.
            .filter(|name| !name.ends_with(".replace.lock"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed dependency must leave no queries behind, found {:?}",
            leftovers
        );
    }

    /// Re-running an install is how a user repairs a language whose inherited
    /// queries went missing (the documented fix for TypeScript/JavaScript), so
    /// an already-complete language must still pull in the parents it is
    /// missing — without `--force`, and without touching its own files.
    #[test]
    fn installing_a_complete_language_still_repairs_a_missing_parent() {
        let temp_dir = TempDir::new().unwrap();
        let data_dir = temp_dir.path().to_path_buf();
        let child_dir = data_dir.join("queries").join("complete_child");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(
            child_dir.join("highlights.scm"),
            "; inherits: absent_parent\n(identifier) @variable\n",
        )
        .unwrap();
        write_install_marker(&child_dir).unwrap();
        let base_url = spawn_query_file_server(vec![(
            "/absent_parent/highlights.scm",
            "(comment) @comment\n",
        )]);

        let result = install_queries_with_dependencies_from_allowing_http_for_tests(
            &base_url,
            "complete_child",
            &data_dir,
            false,
        );

        assert!(
            matches!(result, Err(QueryInstallError::AlreadyExists(path)) if path == child_dir),
            "the requested language was already installed"
        );
        assert!(
            data_dir
                .join("queries")
                .join("absent_parent")
                .join("highlights.scm")
                .exists(),
            "the missing parent must be installed by the re-run"
        );
        assert_eq!(
            fs::read_to_string(child_dir.join("highlights.scm")).unwrap(),
            "; inherits: absent_parent\n(identifier) @variable\n",
            "repairing a parent must not rewrite the language's own queries"
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
    fn publish_query_dir_aborts_when_uninstall_tombstone_exists() {
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

        let result = publish_query_dir(
            &tmp_queries_dir,
            &queries_parent.join("raced_lang"),
            "raced_lang",
            false,
        );

        assert!(
            matches!(result, Ok(PublishQueryDirOutcome::Uninstalled)),
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
