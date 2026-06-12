//! Marker-based workspace-root detection for bridge servers.
//!
//! Downstream language servers historically received the client-supplied
//! workspace root verbatim, which points at the *editor's* workspace — in a
//! monorepo that is rarely the project the document belongs to. `rootMarkers`
//! locates the root the way editors' LSP clients do: walk up from the
//! triggering document and pick the first directory containing a marker.

use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::WorkspaceFolder;
use url::Url;

/// Built-in default applied when a server config has no `rootMarkers`.
/// Mirrored as a literal by the `config init` template
/// (`config::defaults::default_language_servers`), which cannot reference
/// this bridge-private module; `default_settings_documents_root_markers_default`
/// pins the template side.
const DEFAULT_ROOT_MARKERS: &[&str] = &[".git"];

/// A marker must consist solely of normal path components to join onto
/// candidate directories. Everything else defeats the ancestor walk into an
/// always-match: absolute / Windows drive-or-root prefixed markers make
/// `Path::join` *replace* the candidate, `..` matches outside it, and `""` /
/// `"."` exist for every directory.
fn is_valid_marker(marker: &str) -> bool {
    let path = Path::new(marker);
    let mut components = path.components();
    components
        .next()
        .is_some_and(|c| matches!(c, std::path::Component::Normal(_)))
        && components.all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Find the nearest ancestor directory of `document_path` that contains any
/// of `markers` (as a file or a directory). Returns `None` when no ancestor
/// matches or `markers` is empty (the explicit `[]` kill switch).
fn find_marker_root(document_path: &Path, markers: &[String]) -> Option<PathBuf> {
    let (markers, invalid): (Vec<&String>, Vec<&String>) =
        markers.iter().partition(|marker| is_valid_marker(marker));
    if !invalid.is_empty() {
        // Spawn-time only (not per request), so a plain warn does not flood.
        log::warn!(
            target: "kakehashi::bridge::init",
            "ignoring invalid rootMarkers entries (must be plain relative names): {:?}",
            invalid
        );
    }
    if markers.is_empty() {
        return None;
    }
    document_path
        .parent()?
        .ancestors()
        .find(|dir| markers.iter().any(|marker| dir.join(marker).exists()))
        .map(Path::to_path_buf)
}

/// Compute the `rootUri` + `workspaceFolders` for a downstream server's
/// `initialize` request: a marker hit near the triggering document overrides
/// the client-supplied values (and becomes the sole workspace folder);
/// anything short of a fully usable marker root — no hint, no marker, or a
/// root that does not parse as an LSP URI — falls back to the client
/// supplied pair wholesale, never half-and-half.
pub(crate) fn resolve_spawn_workspace(
    root_markers: Option<&[String]>,
    document_uri: Option<&Url>,
    fallback: impl FnOnce() -> (Option<String>, Option<Vec<WorkspaceFolder>>),
) -> (Option<String>, Option<Vec<WorkspaceFolder>>) {
    let marker_workspace = resolve_marker_root(root_markers, document_uri).and_then(|root| {
        let uri = root.as_str().parse().ok()?;
        let name = root
            .to_file_path()
            .ok()
            .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        Some((
            Some(root.to_string()),
            Some(vec![WorkspaceFolder { uri, name }]),
        ))
    });
    marker_workspace.unwrap_or_else(fallback)
}

/// Resolve the marker-derived root for a server spawn from the document that
/// triggered it. `None` (no markers configured) falls back to
/// [`DEFAULT_ROOT_MARKERS`]; callers fall back to the client-supplied root
/// when this returns `None` (non-file URI, no hint, no marker found, or
/// markers explicitly disabled with `[]`).
fn resolve_marker_root(root_markers: Option<&[String]>, document_uri: Option<&Url>) -> Option<Url> {
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

    #[test]
    fn absolute_and_parent_dir_markers_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let doc = project.join("main.rs");

        // "/" exists for every ancestor when joined absolutely; ".." matches
        // outside the candidate; "" and "." exist for every directory.
        // All must be ignored, not "always match".
        assert_eq!(find_marker_root(&doc, &markers(&["/"])), None);
        assert_eq!(find_marker_root(&doc, &markers(&["../project"])), None);
        assert_eq!(find_marker_root(&doc, &markers(&[""])), None);
        assert_eq!(find_marker_root(&doc, &markers(&["."])), None);
        // Multi-component relative markers stay valid (e.g. ".github/workflows").
        assert!(is_valid_marker("nested/marker.txt"));
    }

    #[test]
    fn spawn_workspace_uses_marker_root_for_both_halves() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir(project.join(".git")).unwrap();
        let doc_uri = Url::from_file_path(project.join("src/main.rs")).unwrap();

        let (root_uri, folders) =
            resolve_spawn_workspace(None, Some(&doc_uri), || panic!("must not fall back"));

        let root = root_uri.expect("marker root becomes rootUri");
        let canonical_root = Url::from_file_path(project.canonicalize().unwrap()).unwrap();
        assert_eq!(
            Url::parse(&root)
                .unwrap()
                .to_file_path()
                .unwrap()
                .canonicalize()
                .unwrap(),
            canonical_root.to_file_path().unwrap()
        );
        let folders = folders.expect("marker root becomes the sole workspace folder");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].uri.as_str(), root, "folder uri matches rootUri");
        assert_eq!(folders[0].name, "repo");
    }

    #[test]
    fn spawn_workspace_falls_back_wholesale_without_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let doc_uri = Url::from_file_path(tmp.path().join("orphan.md")).unwrap();
        let unique = format!(".kakehashi-no-such-marker-{}", std::process::id());
        let fallback_folder = WorkspaceFolder {
            uri: "file:///client/root".parse().unwrap(),
            name: "root".to_string(),
        };

        let (root_uri, folders) =
            resolve_spawn_workspace(Some(&markers(&[unique.as_str()])), Some(&doc_uri), || {
                (
                    Some("file:///client/root".to_string()),
                    Some(vec![fallback_folder.clone()]),
                )
            });

        assert_eq!(root_uri.as_deref(), Some("file:///client/root"));
        assert_eq!(folders, Some(vec![fallback_folder]));
    }
}
