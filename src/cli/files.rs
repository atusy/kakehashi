//! Path expansion shared by the file-oriented CLI commands (`format`,
//! `diagnose`): turn the user's `paths` + `--excludes` into the concrete,
//! deduplicated list of files to process.
//!
//! File selection semantics (identical across commands):
//! - Directories are walked recursively, **respecting `.gitignore`** (also
//!   outside git repositories) and skipping hidden files.
//! - Explicitly listed files are always included, even when gitignored —
//!   naming a path is a stronger signal than a `.gitignore` entry.
//! - `--excludes` patterns (gitignore syntax, relative to the current
//!   directory) filter every path *with a cwd-relative form*, including
//!   explicitly listed paths. A path outside the current directory has no
//!   cwd-relative form, so no pattern can match it (see
//!   `is_excluded` / the `excludes_do_not_apply_to_paths_outside_the_base`
//!   test).

use std::path::{Path, PathBuf};

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

/// Expand `paths` into the list of files to process.
///
/// - An explicit file is included unconditionally (bypassing `.gitignore`
///   and the `is_supported` filter) unless an `--excludes` pattern matches
///   it.
/// - A directory is walked respecting `.gitignore` (even outside a git
///   repository), hidden-file filtering, and `--excludes`; only files for
///   which `is_supported` returns true (language detectable from the path)
///   are kept.
/// - A path that does not exist is an error.
///
/// The result is sorted and deduplicated for deterministic processing order.
pub(crate) fn collect_files(
    base: &Path,
    paths: &[PathBuf],
    excludes: &[String],
    is_supported: &dyn Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let exclude_matcher = build_exclude_matcher(base, excludes)
        .map_err(|e| format!("invalid --excludes pattern: {e}"))?;

    let mut files = Vec::new();
    for path in paths {
        // Normalize before stat: a relative path must resolve against
        // `base`, not against whatever the process cwd happens to be.
        let path = normalize_path(base, path);
        let metadata = std::fs::metadata(&path)
            .map_err(|e| format!("cannot access '{}': {e}", path.display()))?;
        if metadata.is_dir() {
            if is_excluded(&exclude_matcher, base, &path, true) {
                continue;
            }
            walk_directory(&path, &exclude_matcher, is_supported, &mut files);
        } else {
            if is_excluded(&exclude_matcher, base, &path, false) {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Absolutize `path` against `base` and clean `.`/`..` components, so the
/// same file always collects to one canonical form — without this, passing
/// `doc.md` and `/abs/to/doc.md` (or `sub/../doc.md`) together would defeat
/// `dedup()` and process the file twice.
fn normalize_path(base: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    path_clean::clean(absolute)
}

/// Whether `path` or any of its ancestor directories *within `base`* matches
/// an `--excludes` pattern.
///
/// A directory-only pattern (`vendor/`) never matches a file path directly —
/// during a walk it works by pruning the directory, but an explicitly listed
/// file (`kakehashi format vendor/dep.md`) skips the walk, so its ancestors
/// must be tested as directories too or the documented "excludes filter
/// everything" contract breaks for exactly that pattern shape.
///
/// The path is made relative to `base` first: patterns are defined relative
/// to the current directory, so ancestors *above* it must not participate —
/// otherwise running from a directory whose own name matches a pattern
/// (e.g. `--excludes vendor/` inside a checkout living under `…/vendor/…`)
/// would exclude every file.
fn is_excluded(
    matcher: &ignore::overrides::Override,
    base: &Path,
    path: &Path,
    is_dir: bool,
) -> bool {
    // Patterns are defined relative to `base` (the current directory); a
    // path outside it has no base-relative form, so no pattern can
    // meaningfully match it — matching the absolute path instead would let
    // directories above `base` (and unrelated absolute components) trigger
    // patterns.
    let Ok(relative) = path.strip_prefix(base) else {
        return false;
    };
    if matcher.matched(relative, is_dir).is_ignore() {
        return true;
    }
    relative
        .ancestors()
        .skip(1)
        .take_while(|a| !a.as_os_str().is_empty())
        .any(|a| matcher.matched(a, true).is_ignore())
}

/// Walk `dir` respecting `.gitignore` and `--excludes`, appending every
/// supported file to `out`. Unreadable entries are warned about and skipped
/// rather than failing the whole run.
fn walk_directory(
    dir: &Path,
    exclude_matcher: &ignore::overrides::Override,
    is_supported: &dyn Fn(&Path) -> bool,
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
                if entry.file_type().is_some_and(|t| t.is_file()) && is_supported(entry.path()) {
                    out.push(entry.path().to_path_buf());
                }
            }
            Err(e) => {
                // Discard the write result rather than `eprintln!`: `diagnose`
                // and `format` ignore SIGPIPE, so writing this warning to a
                // closed stderr (`… 2>&1 | head`) would panic (exit 101).
                // There is nowhere to report a failed warning, so drop it.
                use std::io::Write as _;
                let _ = writeln!(std::io::stderr(), "warning: skipping unreadable entry: {e}");
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
    fn excludes_directory_pattern_filters_explicit_file_inside() {
        // "vendor/" is a directory-only pattern: a walk prunes the directory,
        // but an explicit file skips the walk, so its ancestors must match.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("vendor/dep.md"), "x");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().join("vendor/dep.md")],
            &["vendor/".to_string()],
            &markdown_only,
        )
        .unwrap();

        assert!(files.is_empty());
    }

    #[test]
    fn excludes_do_not_apply_to_paths_outside_the_base() {
        // A pattern is cwd-relative; an explicit file outside the cwd has no
        // cwd-relative form, so patterns (here one matching a component of
        // its absolute path) must not exclude it.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("workspace");
        std::fs::create_dir_all(&base).unwrap();
        write(&tmp.path().join("outside/doc.md"), "x");

        let files = collect_files(
            &base,
            &[tmp.path().join("outside/doc.md")],
            &["outside/".to_string()],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("outside/doc.md")]);
    }

    #[test]
    fn excludes_do_not_match_ancestors_above_the_base() {
        // Patterns are relative to the current directory: a checkout living
        // under a directory whose name matches a pattern (here the base is
        // itself named "vendor") must not have everything excluded.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("vendor");
        write(&base.join("kept.md"), "x");

        let files = collect_files(
            &base,
            &[base.join("kept.md")],
            &["vendor/".to_string()],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![base.join("kept.md")]);
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
    fn directory_walk_keeps_only_supported_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("doc.md"), "x");
        write(&tmp.path().join("notes.txt"), "x");

        let files =
            collect_files(tmp.path(), &[tmp.path().to_path_buf()], &[], &markdown_only).unwrap();

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[test]
    fn explicit_file_bypasses_supported_filter() {
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
    fn duplicates_under_different_spellings_are_deduplicated() {
        // The same file via a clean path and a `sub/..`-detour must collapse
        // to one entry, or it would be processed twice.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("doc.md"), "x");
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();

        let files = collect_files(
            tmp.path(),
            &[
                tmp.path().join("doc.md"),
                tmp.path().join("sub").join("..").join("doc.md"),
            ],
            &[],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[test]
    fn explicitly_named_hidden_directory_is_walked() {
        // The walker's hidden-file filter applies to entries, not to the
        // walk root: explicitly naming a hidden directory must process its
        // (non-hidden) contents.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".hidden/doc.md"), "x");

        let files = collect_files(
            tmp.path(),
            &[tmp.path().join(".hidden")],
            &[],
            &markdown_only,
        )
        .unwrap();

        assert_eq!(files, vec![tmp.path().join(".hidden/doc.md")]);
    }

    #[test]
    fn gitignored_directory_contents_are_processed_when_directory_is_explicit() {
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
