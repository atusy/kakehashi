//! The upstream client's workspace, as the pool forwards it to future
//! client-fallback handshakes.
//!
//! The folder list and the root URI derived from it used to live in two
//! independent cells — a [`WorkspaceFolderSet`](super::WorkspaceFolderSet)
//! and an `ArcSwap` root — written one after the other by
//! `workspace/didChangeWorkspaceFolders`. Two concurrent changes could then
//! interleave their read-then-store so the root named a folder from the
//! older change while the set held the newer one (#955), and a spawn reading
//! the two cells separately could pair a root and a folder list that never
//! coexisted. Here the root is not stored beside the folders; it is derived
//! from them on read, under the same lock, so the two cannot diverge.
//!
//! The one piece of root state that is not a function of the folders is
//! `initialize`'s: a client may send `rootUri` with an empty (or absent)
//! folder list, and that root must still reach downstream servers. It is kept
//! as `seed_root`, consulted only while the folder list is empty, and dropped
//! by the first applied change — after a change the root is the first folder
//! or nothing, never a root the client named before it changed its folders.

use std::sync::Mutex;

use tower_lsp_server::ls_types::WorkspaceFolder;

use super::folder_set::WorkspaceFolderSet;
use crate::error::LockResultExt;

/// A consistent view of the client workspace: `root` is derived from the very
/// `folders` it is returned with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientWorkspaceSnapshot {
    pub(crate) root: Option<String>,
    pub(crate) folders: Option<Vec<WorkspaceFolder>>,
}

#[derive(Default)]
struct State {
    folders: Option<Vec<WorkspaceFolder>>,
    /// `initialize`'s root, used only while `folders` is empty and cleared by
    /// the first applied change.
    seed_root: Option<String>,
}

impl State {
    fn root(&self) -> Option<String> {
        match self.folders.as_deref() {
            Some([first, ..]) => Some(first.uri.to_string()),
            _ => self.seed_root.clone(),
        }
    }
}

/// The upstream client's workspace folders and the root derived from them.
#[derive(Default)]
pub(crate) struct ClientWorkspace {
    inner: Mutex<State>,
}

impl ClientWorkspace {
    /// Seed from `initialize`: `root` is the client's root ladder result and
    /// `folders` the forwarded folder list. While `folders` is non-empty the
    /// root follows its first entry; `root` itself matters only when the
    /// client sent `rootUri` with no folders.
    pub(crate) fn seed(&self, root: Option<String>, folders: Option<Vec<WorkspaceFolder>>) {
        *self.inner.lock().recover_poison("ClientWorkspace::seed") = State {
            folders: WorkspaceFolderSet::deduplicate(folders),
            seed_root: root,
        };
    }

    /// Apply an upstream `workspace/didChangeWorkspaceFolders` event with
    /// [`WorkspaceFolderSet::apply_to`]'s semantics. The root follows the
    /// changed list from here on, so the `initialize` seed is dropped in the
    /// same critical section.
    pub(crate) fn apply_change(&self, added: Vec<WorkspaceFolder>, removed: &[WorkspaceFolder]) {
        let mut state = self
            .inner
            .lock()
            .recover_poison("ClientWorkspace::apply_change");
        WorkspaceFolderSet::apply_to(&mut state.folders, added, removed);
        state.seed_root = None;
    }

    /// The folders and the root derived from them, read under one lock.
    pub(crate) fn snapshot(&self) -> ClientWorkspaceSnapshot {
        let state = self
            .inner
            .lock()
            .recover_poison("ClientWorkspace::snapshot");
        ClientWorkspaceSnapshot {
            root: state.root(),
            folders: state.folders.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::Uri;

    fn folder(uri: &str) -> WorkspaceFolder {
        WorkspaceFolder {
            uri: Uri::from_str(uri).unwrap(),
            name: uri.rsplit('/').next().unwrap_or(uri).to_string(),
        }
    }

    fn seeded(root: Option<&str>, folders: Option<Vec<WorkspaceFolder>>) -> ClientWorkspace {
        let workspace = ClientWorkspace::default();
        workspace.seed(root.map(String::from), folders);
        workspace
    }

    #[test]
    fn root_follows_the_first_seeded_folder() {
        let workspace = seeded(
            Some("file:///a"),
            Some(vec![folder("file:///a"), folder("file:///b")]),
        );

        assert_eq!(
            workspace.snapshot(),
            ClientWorkspaceSnapshot {
                root: Some("file:///a".to_string()),
                folders: Some(vec![folder("file:///a"), folder("file:///b")]),
            }
        );
    }

    /// `rootUri` with `workspaceFolders: []` is a legal initialize shape: the
    /// client named a root but no folder, and downstream servers still get
    /// that root.
    #[test]
    fn seeded_root_stands_while_the_folder_list_is_empty() {
        let workspace = seeded(Some("file:///root"), Some(Vec::new()));

        assert_eq!(
            workspace.snapshot(),
            ClientWorkspaceSnapshot {
                root: Some("file:///root".to_string()),
                folders: Some(Vec::new()),
            }
        );
    }

    #[test]
    fn a_change_moves_the_root_to_the_new_first_folder() {
        let workspace = seeded(Some("file:///a"), Some(vec![folder("file:///a")]));

        workspace.apply_change(vec![folder("file:///b")], &[folder("file:///a")]);

        assert_eq!(
            workspace.snapshot(),
            ClientWorkspaceSnapshot {
                root: Some("file:///b".to_string()),
                folders: Some(vec![folder("file:///b")]),
            }
        );
    }

    /// Emptying the folder list leaves no root: the initialize root names a
    /// workspace the client has since changed, typically the very folder it
    /// just removed.
    #[test]
    fn emptying_the_folders_drops_the_root_rather_than_reviving_the_seed() {
        let workspace = seeded(Some("file:///a"), Some(vec![folder("file:///a")]));

        workspace.apply_change(Vec::new(), &[folder("file:///a")]);

        assert_eq!(
            workspace.snapshot(),
            ClientWorkspaceSnapshot {
                root: None,
                folders: Some(Vec::new()),
            }
        );
    }

    /// Any applied change retires the seed, even one that leaves an absent
    /// folder list absent (a removal-only event on a `null` set).
    #[test]
    fn a_change_retires_the_seed_even_when_the_list_stays_absent() {
        let workspace = seeded(Some("file:///root"), None);

        workspace.apply_change(Vec::new(), &[folder("file:///elsewhere")]);

        assert_eq!(
            workspace.snapshot(),
            ClientWorkspaceSnapshot {
                root: None,
                folders: None,
            }
        );
    }

    /// `null` and `[]` are distinct answers to a `workspace/workspaceFolders`
    /// pull; seeding must not collapse an explicitly empty list.
    #[test]
    fn seeding_preserves_an_explicit_empty_folder_list() {
        assert_eq!(
            seeded(None, Some(Vec::new())).snapshot().folders,
            Some(Vec::new())
        );
        assert_eq!(seeded(None, None).snapshot().folders, None);
    }

    #[test]
    fn seeding_deduplicates_folders_by_uri() {
        let renamed_a = WorkspaceFolder {
            uri: folder("file:///a").uri,
            name: "renamed".to_string(),
        };
        let workspace = seeded(
            Some("file:///a"),
            Some(vec![folder("file:///a"), renamed_a]),
        );

        assert_eq!(
            workspace.snapshot().folders,
            Some(vec![folder("file:///a")])
        );
    }
}
