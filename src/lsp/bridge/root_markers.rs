//! Marker-based workspace-root detection for bridge servers.
//!
//! Downstream language servers historically received the client-supplied
//! workspace root verbatim, which points at the *editor's* workspace — in a
//! monorepo that is rarely the project the document belongs to. `rootMarkers`
//! locates the root the way editors' LSP clients do: walk up from the
//! triggering document and pick the first directory containing a marker.

use std::path::{Path, PathBuf};

use url::Url;

/// Built-in default applied when a server config has no `rootMarkers`.
pub(crate) const DEFAULT_ROOT_MARKERS: &[&str] = &[".git"];

/// Find the nearest ancestor directory of `document_path` that contains any
/// of `markers` (as a file or a directory). Returns `None` when no ancestor
/// matches or `markers` is empty (the explicit `[]` kill switch).
fn find_marker_root(document_path: &Path, markers: &[String]) -> Option<PathBuf> {
    if markers.is_empty() {
        return None;
    }
    document_path
        .parent()?
        .ancestors()
        .find(|dir| markers.iter().any(|marker| dir.join(marker).exists()))
        .map(Path::to_path_buf)
}

/// Resolve the marker-derived root for a server spawn from the document that
/// triggered it. `None` (no markers configured) falls back to
/// [`DEFAULT_ROOT_MARKERS`]; callers fall back to the client-supplied root
/// when this returns `None` (non-file URI, no hint, no marker found, or
/// markers explicitly disabled with `[]`).
pub(crate) fn resolve_marker_root(
    root_markers: Option<&[String]>,
    document_uri: Option<&Url>,
) -> Option<Url> {
    let default_markers: Vec<String> = DEFAULT_ROOT_MARKERS
        .iter()
        .map(|marker| marker.to_string())
        .collect();
    let markers = root_markers.unwrap_or(default_markers.as_slice());
    let document_path = document_uri?.to_file_path().ok()?;
    let root = find_marker_root(&document_path, markers)?;
    Url::from_file_path(root).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markers(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn finds_nearest_ancestor_with_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let inner = outer.join("sub/project");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        std::fs::create_dir(outer.join(".git")).unwrap();
        std::fs::create_dir(inner.join(".git")).unwrap();
        let doc = inner.join("src/main.rs");

        let root = find_marker_root(&doc, &markers(&[".git"]));
        // canonicalize: macOS tempdirs live behind /private symlinks
        assert_eq!(
            root.map(|p| p.canonicalize().unwrap()),
            Some(inner.canonicalize().unwrap()),
            "nearest marker wins over the outer one"
        );
    }

    #[test]
    fn marker_may_be_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("Cargo.toml"), "").unwrap();
        let doc = project.join("src/main.rs");

        let root = find_marker_root(&doc, &markers(&["Cargo.toml"]));
        assert_eq!(
            root.map(|p| p.canonicalize().unwrap()),
            Some(project.canonicalize().unwrap())
        );
    }

    #[test]
    fn returns_none_without_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let doc = tmp.path().join("orphan.txt");
        // Search a marker name that cannot exist anywhere up the tree.
        let unique = format!(".kakehashi-no-such-marker-{}", std::process::id());
        assert_eq!(find_marker_root(&doc, &markers(&[unique.as_str()])), None);
    }

    #[test]
    fn empty_markers_disable_the_search() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let doc = tmp.path().join("main.rs");
        assert_eq!(find_marker_root(&doc, &[]), None);
    }

    #[test]
    fn resolve_defaults_to_git_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join("docs")).unwrap();
        std::fs::create_dir(project.join(".git")).unwrap();
        let doc_uri = Url::from_file_path(project.join("docs/readme.md")).unwrap();

        let root = resolve_marker_root(None, Some(&doc_uri)).expect("should find .git root");
        let root_path = root.to_file_path().unwrap();
        assert_eq!(
            root_path.canonicalize().unwrap(),
            project.canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_returns_none_without_document_hint() {
        assert_eq!(resolve_marker_root(None, None), None);
    }

    #[test]
    fn resolve_returns_none_for_non_file_uri() {
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        assert_eq!(resolve_marker_root(None, Some(&uri)), None);
    }

    #[test]
    fn resolve_honors_explicit_empty_markers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let doc_uri = Url::from_file_path(tmp.path().join("main.rs")).unwrap();
        assert_eq!(resolve_marker_root(Some(&[]), Some(&doc_uri)), None);
    }
}
