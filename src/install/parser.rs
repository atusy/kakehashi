//! Parser compilation and installation.
//!
//! This module handles fetching parser source (via HTTP archive or git clone),
//! compiling them with tree-sitter-loader, and installing the resulting shared library.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use flate2::read::GzDecoder;
use tar::Archive;
use tree_sitter_loader::Loader;

use super::metadata::{FetchOptions, MetadataError, fetch_parser_metadata};

/// HTTP timeout for archive downloads.
const ARCHIVE_HTTP_TIMEOUT: Duration = Duration::from_secs(120);

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
pub struct ParserInstallResult {
    /// The language that was installed.
    pub language: String,
    /// Path where parser was installed.
    pub install_path: PathBuf,
    /// Git revision that was used.
    pub revision: String,
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

/// Compile a Tree-sitter parser from source using tree-sitter-loader.
fn compile_parser(grammar_path: &Path, output_path: &Path) -> Result<(), ParserInstallError> {
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

/// Install a Tree-sitter parser for a language.
pub fn install_parser(
    language: &str,
    options: &InstallOptions,
) -> Result<ParserInstallResult, ParserInstallError> {
    let parser_dir = options.data_dir.join("parser");
    let parser_file = parser_dir.join(format!("{}.{}", language, std::env::consts::DLL_EXTENSION));

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
    let source_dir = if let Some(ref location) = metadata.location {
        clone_dir.join(location)
    } else {
        clone_dir.clone()
    };

    if options.verbose {
        eprintln!("Building parser in: {}", source_dir.display());
    }

    // Compile the parser directly to the install path
    fs::create_dir_all(&parser_dir)?;
    compile_parser(&source_dir, &parser_file)?;

    if options.verbose {
        eprintln!("Installed to: {}", parser_file.display());
    }

    Ok(ParserInstallResult {
        language: language.to_string(),
        install_path: parser_file,
        revision: metadata.revision,
    })
}

/// Construct a GitHub archive download URL from a repository URL and revision.
///
/// Returns `None` if the URL is not a GitHub HTTPS URL.
/// GitHub serves tarballs at `https://github.com/{owner}/{repo}/archive/{revision}.tar.gz`.
fn github_archive_url(repo_url: &str, revision: &str) -> Option<String> {
    let url = repo_url.strip_suffix(".git").unwrap_or(repo_url);
    if !url.starts_with("https://github.com/") {
        return None;
    }
    Some(format!("{}/archive/{}.tar.gz", url, revision))
}

/// Extract the repository name from a URL.
///
/// For `https://github.com/tree-sitter/tree-sitter-json.git`, returns `"tree-sitter-json"`.
fn repo_name_from_url(url: &str) -> Option<String> {
    let url = url.strip_suffix(".git").unwrap_or(url);
    url.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Fetch parser source code into a destination directory.
///
/// Strategy:
/// 1. If the URL is a GitHub HTTPS URL, try downloading the archive tarball.
/// 2. If archive download fails (or URL is not GitHub), fall back to git clone.
fn fetch_source(url: &str, revision: &str, dest: &Path) -> Result<(), ParserInstallError> {
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
    let client = reqwest::blocking::Client::builder()
        .timeout(ARCHIVE_HTTP_TIMEOUT)
        .build()
        .map_err(|e| ParserInstallError::ArchiveError(e.to_string()))?;

    let response = client
        .get(archive_url)
        .send()
        .map_err(|e| ParserInstallError::ArchiveError(format!("Download failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(ParserInstallError::ArchiveError(format!(
            "HTTP {} downloading {}",
            response.status(),
            archive_url
        )));
    }

    let decoder = GzDecoder::new(response);
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

/// Clone a git repository at a specific revision.
fn clone_repo(url: &str, revision: &str, dest: &Path) -> Result<(), ParserInstallError> {
    // First, clone with depth 1 (we'll fetch the specific revision)
    let status = Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(dest)
        .status()
        .map_err(|e| ParserInstallError::GitError(e.to_string()))?;

    if !status.success() {
        return Err(ParserInstallError::GitError(format!(
            "Failed to clone {}",
            url
        )));
    }

    // Fetch the specific revision
    let status = Command::new("git")
        .current_dir(dest)
        .args(["fetch", "--depth", "1", "origin", revision])
        .status()
        .map_err(|e| ParserInstallError::GitError(e.to_string()))?;

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
    let status = Command::new("git")
        .current_dir(dest)
        .args(["checkout", "FETCH_HEAD"])
        .status()
        .map_err(|e| ParserInstallError::GitError(e.to_string()))?;

    if !status.success() {
        return Err(ParserInstallError::GitError(format!(
            "Failed to checkout revision {} (FETCH_HEAD)",
            revision
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_dll_extension_is_valid() {
        let ext = std::env::consts::DLL_EXTENSION;
        assert!(ext == "so" || ext == "dylib" || ext == "dll");
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
        assert_eq!(
            url,
            Some(
                "https://github.com/tree-sitter/tree-sitter-json/archive/v0.24.8.tar.gz"
                    .to_string()
            )
        );
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
        assert_eq!(
            url,
            Some(
                "https://github.com/tree-sitter/tree-sitter-json/archive/v0.24.8.tar.gz"
                    .to_string()
            )
        );
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

    /// PBI-015: Test that clone_repo works with tag revisions (e.g., v0.25.0)
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

        compile_parser(&clone_dir, &output_path).expect("compile should succeed");

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
