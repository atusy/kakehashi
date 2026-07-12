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

fn has_symlink_ancestor(
    path: &Path,
    base: &Path,
    cache: &mut std::collections::HashMap<PathBuf, bool>,
) -> bool {
    let is_within_base = path.starts_with(base);
    path.ancestors()
        .skip(1)
        .take_while(|ancestor| !is_within_base || ancestor.starts_with(base))
        .any(|ancestor| {
            *cache.entry(ancestor.to_path_buf()).or_insert_with(|| {
                std::fs::symlink_metadata(ancestor)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
            })
        })
}

/// Files collected from CLI paths plus any non-fatal directory walk errors
/// encountered while discovering them.
pub(crate) struct CollectedFiles {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) walk_errors: usize,
}

fn explicit_inputs_have_alias(inputs: &mut [(PathBuf, PathBuf, bool)]) -> bool {
    inputs.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    let mut ancestors: Vec<usize> = Vec::new();
    let mut previous = None;

    for index in 0..inputs.len() {
        let (lexical, resolved, is_dir) = &inputs[index];
        if let Some(previous_index) = previous {
            let previous: &(PathBuf, PathBuf, bool) = &inputs[previous_index];
            if previous.1 == *resolved && previous.0 != *lexical {
                return true;
            }
        }
        while ancestors
            .last()
            .is_some_and(|&ancestor| !resolved.starts_with(&inputs[ancestor].1))
        {
            ancestors.pop();
        }
        for &ancestor in &ancestors {
            let (ancestor_lexical, ancestor_resolved, _) = &inputs[ancestor];
            let suffix = resolved
                .strip_prefix(ancestor_resolved)
                .expect("ancestor stack only contains resolved path prefixes");
            if ancestor_lexical.join(suffix) != *lexical {
                return true;
            }
        }
        if *is_dir {
            ancestors.push(index);
        }
        previous = Some(index);
    }
    false
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
/// The files are sorted and deduplicated for deterministic processing order.
pub(crate) fn collect_files(
    base: &Path,
    paths: &[PathBuf],
    excludes: &[String],
    is_supported: &dyn Fn(&Path) -> bool,
) -> Result<CollectedFiles, String> {
    let exclude_matcher = build_exclude_matcher(base, excludes)
        .map_err(|e| format!("invalid --excludes pattern: {e}"))?;

    let mut files = Vec::new();
    let mut walk_errors = 0usize;
    let mut explicit_inputs = Vec::new();
    let mut ancestor_symlinks = std::collections::HashMap::new();
    for path in paths {
        // Normalize before stat: a relative path must resolve against
        // `base`, not against whatever the process cwd happens to be.
        let path = normalize_path(base, path);
        let link_metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("cannot access '{}': {e}", path.display()))?;
        let is_symlink = link_metadata.file_type().is_symlink();
        let metadata = if is_symlink {
            std::fs::metadata(&path)
                .map_err(|e| format!("cannot access '{}': {e}", path.display()))?
        } else {
            link_metadata
        };
        if metadata.is_dir() {
            if is_excluded(&exclude_matcher, base, &path, true) {
                continue;
            }
            if paths.len() > 1 {
                let has_alias =
                    is_symlink || has_symlink_ancestor(&path, base, &mut ancestor_symlinks);
                explicit_inputs.push((path.clone(), true, has_alias));
            }
            walk_errors += walk_directory(&path, &exclude_matcher, is_supported, &mut files);
        } else {
            if is_excluded(&exclude_matcher, base, &path, false) {
                continue;
            }
            if paths.len() > 1 {
                let has_alias =
                    is_symlink || has_symlink_ancestor(&path, base, &mut ancestor_symlinks);
                explicit_inputs.push((path.clone(), false, has_alias));
            }
            files.push(path);
        }
    }
    files.sort();
    let has_explicit_alias = if explicit_inputs.iter().any(|input| input.2) {
        let mut identities = explicit_inputs
            .into_iter()
            .map(|(path, is_dir, _)| {
                let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                (path, resolved, is_dir)
            })
            .collect::<Vec<_>>();
        explicit_inputs_have_alias(&mut identities)
    } else {
        false
    };
    if has_explicit_alias {
        let mut representatives = std::collections::HashMap::with_capacity(files.len());
        for path in files.drain(..) {
            let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let is_alias = std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
                || has_symlink_ancestor(&path, base, &mut ancestor_symlinks);
            representatives
                .entry(identity)
                .and_modify(|representative: &mut (PathBuf, bool)| {
                    if representative.1 && !is_alias {
                        *representative = (path.clone(), false);
                    }
                })
                .or_insert((path, is_alias));
        }
        files.extend(
            representatives
                .into_values()
                .map(|(representative, _)| representative),
        );
        files.sort();
    } else {
        files.dedup();
    }
    Ok(CollectedFiles { files, walk_errors })
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
/// supported file to `out`. Unreadable entries are reported and counted so
/// callers can surface an operational-error exit code after processing the
/// files that were still discoverable.
fn walk_directory(
    dir: &Path,
    exclude_matcher: &ignore::overrides::Override,
    is_supported: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) -> usize {
    let walker = ignore::WalkBuilder::new(dir)
        .overrides(exclude_matcher.clone())
        // Respect .gitignore files even outside a git repository: the
        // command's contract is "gitignore applies", not "gitignore applies
        // only when git initialized the directory".
        .require_git(false)
        .build();
    let mut errors = 0usize;
    for entry in walker {
        match entry {
            Ok(entry) => {
                if entry.file_type().is_some_and(|t| t.is_file()) && is_supported(entry.path()) {
                    out.push(entry.path().to_path_buf());
                }
            }
            Err(e) => {
                // Discard the write result rather than `eprintln!`: `diagnose`
                // and `format` ignore SIGPIPE, so writing this error to a
                // closed stderr (`… 2>&1 | head`) would panic (exit 101).
                // There is nowhere to report a failed error message, so drop it.
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "error: skipping unreadable entry (will exit 2): {e}"
                );
                errors += 1;
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_explicit_file_lists_have_no_alias_relationship() {
        let mut inputs = (0..4096)
            .map(|index| {
                let path = PathBuf::from(format!("/workspace/file-{index}.md"));
                (path.clone(), path, false)
            })
            .collect::<Vec<_>>();

        assert!(!explicit_inputs_have_alias(&mut inputs));
    }

    #[test]
    fn large_ordinary_explicit_file_list_is_collected() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = (0..256)
            .map(|index| {
                let path = tmp.path().join(format!("file-{index}.md"));
                write(&path, "x");
                path
            })
            .collect::<Vec<_>>();

        let files = collect_paths(tmp.path(), &paths, &[], &markdown_only);

        assert_eq!(files.len(), paths.len());
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn markdown_only(path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "md")
    }

    fn collect_paths(
        base: &Path,
        paths: &[PathBuf],
        excludes: &[String],
        is_supported: &dyn Fn(&Path) -> bool,
    ) -> Vec<PathBuf> {
        collect_files(base, paths, excludes, is_supported)
            .unwrap()
            .files
    }

    #[test]
    fn directory_walk_respects_gitignore_without_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("kept.md"), "x");
        write(&tmp.path().join("ignored.md"), "x");
        write(&tmp.path().join(".gitignore"), "ignored.md\n");

        let files = collect_paths(tmp.path(), &[tmp.path().to_path_buf()], &[], &markdown_only);

        assert_eq!(files, vec![tmp.path().join("kept.md")]);
    }

    #[test]
    fn explicit_file_bypasses_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("ignored.md"), "x");
        write(&tmp.path().join(".gitignore"), "ignored.md\n");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().join("ignored.md")],
            &[],
            &markdown_only,
        );

        assert_eq!(files, vec![tmp.path().join("ignored.md")]);
    }

    #[test]
    fn excludes_filter_walked_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("kept.md"), "x");
        write(&tmp.path().join("dropped.md"), "x");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().to_path_buf()],
            &["dropped.md".to_string()],
            &markdown_only,
        );

        assert_eq!(files, vec![tmp.path().join("kept.md")]);
    }

    #[test]
    fn excludes_filter_explicit_files_too() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("dropped.md"), "x");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().join("dropped.md")],
            &["dropped.md".to_string()],
            &markdown_only,
        );

        assert!(files.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn excluded_symlink_does_not_affect_collected_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let docs = tmp.path().join("docs");
        let document = docs.join("kept.md");
        let alias = tmp.path().join("alias.md");
        write(&document, "x");
        symlink(&document, &alias).unwrap();

        let files = collect_paths(
            tmp.path(),
            &[docs, alias],
            &["alias.md".to_string()],
            &markdown_only,
        );

        assert_eq!(files, vec![document]);
    }

    #[test]
    fn excludes_directory_pattern_filters_explicit_file_inside() {
        // "vendor/" is a directory-only pattern: a walk prunes the directory,
        // but an explicit file skips the walk, so its ancestors must match.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("vendor/dep.md"), "x");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().join("vendor/dep.md")],
            &["vendor/".to_string()],
            &markdown_only,
        );

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

        let files = collect_paths(
            &base,
            &[tmp.path().join("outside/doc.md")],
            &["outside/".to_string()],
            &markdown_only,
        );

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

        let files = collect_paths(
            &base,
            &[base.join("kept.md")],
            &["vendor/".to_string()],
            &markdown_only,
        );

        assert_eq!(files, vec![base.join("kept.md")]);
    }

    #[test]
    fn excludes_match_directories() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("vendor/dep.md"), "x");
        write(&tmp.path().join("kept.md"), "x");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().to_path_buf()],
            &["vendor/".to_string()],
            &markdown_only,
        );

        assert_eq!(files, vec![tmp.path().join("kept.md")]);
    }

    #[test]
    fn directory_walk_keeps_only_supported_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("doc.md"), "x");
        write(&tmp.path().join("notes.txt"), "x");

        let files = collect_paths(tmp.path(), &[tmp.path().to_path_buf()], &[], &markdown_only);

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[cfg(unix)]
    #[test]
    fn directory_walk_counts_unreadable_entries() {
        use std::os::unix::fs::PermissionsExt as _;

        struct RestorePermissions<'a> {
            path: &'a Path,
            mode: u32,
        }

        impl Drop for RestorePermissions<'_> {
            fn drop(&mut self) {
                let _ =
                    std::fs::set_permissions(self.path, std::fs::Permissions::from_mode(self.mode));
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("kept.md"), "x");
        let unreadable = tmp.path().join("unreadable");
        write(&unreadable.join("hidden.md"), "x");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = RestorePermissions {
            path: &unreadable,
            mode: 0o700,
        };

        if std::fs::read_dir(&unreadable).is_ok() {
            return;
        }

        let result =
            collect_files(tmp.path(), &[tmp.path().to_path_buf()], &[], &markdown_only).unwrap();

        assert_eq!(result.files, vec![tmp.path().join("kept.md")]);
        assert!(result.walk_errors > 0);
    }

    #[test]
    fn explicit_file_bypasses_supported_filter() {
        // Language detection for explicit files happens later from content
        // (first-line detection), so collection must not drop them by path.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("script"), "#!/usr/bin/env lua");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().join("script")],
            &[],
            &markdown_only,
        );

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

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().join("doc.md"), tmp.path().to_path_buf()],
            &[],
            &markdown_only,
        );

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[test]
    fn duplicates_under_different_spellings_are_deduplicated() {
        // The same file via a clean path and a `sub/..`-detour must collapse
        // to one entry, or it would be processed twice.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("doc.md"), "x");
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();

        let files = collect_paths(
            tmp.path(),
            &[
                tmp.path().join("doc.md"),
                tmp.path().join("sub").join("..").join("doc.md"),
            ],
            &[],
            &markdown_only,
        );

        assert_eq!(files, vec![tmp.path().join("doc.md")]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_are_deduplicated() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let document = tmp.path().join("doc.md");
        let alias = tmp.path().join("alias.md");
        write(&document, "x");
        symlink(&document, &alias).unwrap();

        let files = collect_paths(tmp.path(), &[document.clone(), alias], &[], &markdown_only);

        assert_eq!(files.len(), 1, "one filesystem file must be processed once");
        assert_eq!(
            std::fs::canonicalize(&files[0]).unwrap(),
            std::fs::canonicalize(document).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_aliases_are_deduplicated() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let alias = tmp.path().join("alias");
        let document = real.join("doc.md");
        write(&document, "x");
        symlink(&real, &alias).unwrap();

        let files = collect_paths(
            tmp.path(),
            &[document.clone(), alias.join("doc.md")],
            &[],
            &markdown_only,
        );

        assert_eq!(files.len(), 1, "one filesystem file must be processed once");
    }

    #[cfg(unix)]
    #[test]
    fn nested_symlink_directory_aliases_are_deduplicated() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let alias = tmp.path().join("alias");
        write(&real.join("doc.md"), "x");
        symlink(&real, &alias).unwrap();

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().to_path_buf(), alias],
            &[],
            &markdown_only,
        );

        assert_eq!(files.len(), 1, "one filesystem file must be processed once");
    }

    #[cfg(unix)]
    #[test]
    fn dedup_prefers_non_symlink_processing_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let document = real.join("doc.md");
        let alias = tmp.path().join("alias.txt");
        write(&document, "x");
        symlink(&document, &alias).unwrap();

        let files = collect_paths(tmp.path(), &[real, alias], &[], &markdown_only);

        assert_eq!(files, vec![document]);
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_symlink_aliases_are_deduplicated() {
        use std::os::windows::fs::symlink_file;

        let tmp = tempfile::tempdir().unwrap();
        let document = tmp.path().join("doc.md");
        let alias = tmp.path().join("alias.md");
        write(&document, "x");
        if symlink_file(&document, &alias).is_err() {
            return;
        }

        let files = collect_paths(tmp.path(), &[document, alias], &[], &markdown_only);

        assert_eq!(files.len(), 1, "one filesystem file must be processed once");
    }

    #[cfg(windows)]
    #[test]
    fn windows_directory_symlink_aliases_are_deduplicated() {
        use std::os::windows::fs::symlink_dir;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let alias = tmp.path().join("alias");
        write(&real.join("doc.md"), "x");
        if symlink_dir(&real, &alias).is_err() {
            return;
        }

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().to_path_buf(), alias],
            &[],
            &markdown_only,
        );

        assert_eq!(files.len(), 1, "one filesystem file must be processed once");
    }

    #[test]
    fn explicitly_named_hidden_directory_is_walked() {
        // The walker's hidden-file filter applies to entries, not to the
        // walk root: explicitly naming a hidden directory must process its
        // (non-hidden) contents.
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join(".hidden/doc.md"), "x");

        let files = collect_paths(
            tmp.path(),
            &[tmp.path().join(".hidden")],
            &[],
            &markdown_only,
        );

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

        let files = collect_paths(tmp.path(), &[tmp.path().join("build")], &[], &markdown_only);

        assert_eq!(files, vec![tmp.path().join("build/out.md")]);
    }
}
