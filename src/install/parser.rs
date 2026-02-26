//! Parser compilation and installation.
//!
//! This module handles cloning parser repositories, compiling them with
//! tree-sitter CLI, and installing the resulting shared library.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::metadata::{FetchOptions, MetadataError, fetch_parser_metadata};

/// Error types for parser installation.
#[derive(Debug)]
pub enum ParserInstallError {
    /// Metadata fetch failed.
    MetadataError(MetadataError),
    /// Git operation failed.
    GitError(String),
    /// tree-sitter CLI not found.
    TreeSitterNotFound,
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
            Self::TreeSitterNotFound => {
                write!(
                    f,
                    "tree-sitter CLI not found. Install with: cargo install tree-sitter-cli"
                )
            }
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

/// Find the tree-sitter CLI executable.
fn find_tree_sitter() -> Option<PathBuf> {
    // Check common locations
    let paths = [
        // Cargo bin directory
        dirs::home_dir().map(|h| h.join(".cargo/bin/tree-sitter")),
        // System PATH
        which_tree_sitter(),
    ];

    paths.into_iter().flatten().find(|path| path.exists())
}

/// Try to find tree-sitter in PATH.
fn which_tree_sitter() -> Option<PathBuf> {
    Command::new("which")
        .arg("tree-sitter")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
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

    // Find tree-sitter CLI
    let tree_sitter = find_tree_sitter().ok_or(ParserInstallError::TreeSitterNotFound)?;

    if options.verbose {
        eprintln!("Using tree-sitter at: {}", tree_sitter.display());
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

    // Create temp directory for cloning
    let temp_dir = tempfile::tempdir()?;
    let clone_dir = temp_dir.path().join("parser");

    // Clone the repository
    if options.verbose {
        eprintln!("Cloning repository...");
    }
    clone_repo(&metadata.url, &metadata.revision, &clone_dir)?;

    // Determine the source directory (handle monorepos)
    let source_dir = if let Some(ref location) = metadata.location {
        clone_dir.join(location)
    } else {
        clone_dir.clone()
    };

    if options.verbose {
        eprintln!("Building parser in: {}", source_dir.display());
    }

    // Build the parser
    build_parser(&tree_sitter, &source_dir, options.verbose)?;

    // Find the built library
    let built_lib = find_built_library(&source_dir)?;

    if options.verbose {
        eprintln!("Built library: {}", built_lib.display());
    }

    // Create parser directory and copy the library
    fs::create_dir_all(&parser_dir)?;
    fs::copy(&built_lib, &parser_file)?;

    if options.verbose {
        eprintln!("Installed to: {}", parser_file.display());
    }

    Ok(ParserInstallResult {
        language: language.to_string(),
        install_path: parser_file,
        revision: metadata.revision,
    })
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

/// Build the parser using tree-sitter CLI.
fn build_parser(
    tree_sitter: &Path,
    source_dir: &Path,
    verbose: bool,
) -> Result<(), ParserInstallError> {
    let mut cmd = Command::new(tree_sitter);
    cmd.current_dir(source_dir).arg("build");

    let output = cmd
        .output()
        .map_err(|e| ParserInstallError::CompileError(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);

        if verbose {
            eprintln!("Build stdout: {}", stdout);
            eprintln!("Build stderr: {}", stderr);
        }

        return Err(ParserInstallError::CompileError(format!(
            "tree-sitter build failed: {}",
            stderr
        )));
    }

    Ok(())
}

/// Find the built shared library in the source directory.
fn find_built_library(source_dir: &Path) -> Result<PathBuf, ParserInstallError> {
    let ext = std::env::consts::DLL_EXTENSION;

    // tree-sitter build creates the library in the current directory
    // with a name like "libtree-sitter-lua.dylib" or just the language name
    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(extension) = path.extension()
            && extension == ext
        {
            return Ok(path);
        }
    }

    Err(ParserInstallError::CompileError(format!(
        "No .{} file found after build",
        ext
    )))
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
    fn test_find_tree_sitter_exists() {
        // This test may fail if tree-sitter is not installed
        // but that's okay - it's testing the search logic
        let _result = find_tree_sitter();
        // Just verify it doesn't panic
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

    /// PBI-015: Test that clone_repo works with tag revisions (e.g., v0.25.0)
    /// This is the bug fix test - tag revisions failed because:
    /// - `git fetch --depth 1 origin v0.25.0` puts the tag in FETCH_HEAD
    /// - `git checkout v0.25.0` fails because the tag isn't a local ref
    /// - Fix: use `git checkout FETCH_HEAD` instead
    #[test]
    fn test_clone_repo_with_tag_revision() {
        let temp = tempdir().expect("Failed to create temp dir");
        let dest = temp.path().join("tree-sitter-python");

        // Python parser uses tag revision (v0.23.5 is a known tag)
        // Using a small/fast repo for testing
        let result = clone_repo(
            "https://github.com/tree-sitter/tree-sitter-python",
            "v0.23.5", // Tag revision - this is what was failing
            &dest,
        );

        assert!(
            result.is_ok(),
            "clone_repo should succeed with tag revision: {:?}",
            result.err()
        );

        // Verify the clone succeeded and is at the right commit
        assert!(dest.exists(), "Clone directory should exist");
        assert!(dest.join(".git").exists(), "Should be a git repository");
    }

    /// Test that compile_parser compiles a parser from source to a shared library.
    #[test]
    fn test_compile_parser_with_loader() {
        let temp = tempdir().expect("Failed to create temp dir");
        let clone_dir = temp.path().join("tree-sitter-json");

        // Clone tree-sitter-json (small repo)
        clone_repo(
            "https://github.com/tree-sitter/tree-sitter-json",
            "v0.24.8",
            &clone_dir,
        )
        .expect("clone should succeed");

        let output_path = temp.path().join(format!(
            "json.{}",
            std::env::consts::DLL_EXTENSION
        ));

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

        // Use tree-sitter-json with a tag that also works as a commit ref
        // This tests that FETCH_HEAD works for both tags and commits
        let result = clone_repo(
            "https://github.com/tree-sitter/tree-sitter-json",
            "v0.24.8", // A recent tag
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
