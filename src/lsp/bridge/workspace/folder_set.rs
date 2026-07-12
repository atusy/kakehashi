//! Per-connection, mutable workspace-folder set.
//!
//! This is the *data type* backing the `workspace/workspaceFolders` pull; the
//! pull *handler* that answers a downstream query from it lives in
//! [`workspace_folders`](super::workspace_folders).
//!
//! Historically the folders a downstream connection serves were frozen at spawn
//! in an immutable `Arc<Option<Vec<WorkspaceFolder>>>`: set once from the
//! resolved root (or the upstream fallback) and only ever read, to answer
//! downstream `workspace/workspaceFolders` pulls. The shared-instance opt-in
//! (#391) needs that set to *grow* as new marker roots join a single
//! connection, so this type wraps it in a shared mutex that both the reader
//! (pull answers) and the pool (announcing new roots) hold a clone of.
//!
//! The lock is only ever held for synchronous work (clone, scan, push) — never
//! across an `.await` — so it is a plain `std::sync::Mutex`, with poison
//! recovery per the project lock convention.
//!
//! `None` is preserved as a distinct "no folders / answer `null`" state, not
//! collapsed into an empty list, matching the pre-#391 behavior.

use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::WorkspaceFolder;

use crate::error::LockResultExt;

/// The workspace folders one downstream connection currently serves, shared
/// (cheaply cloneable) between the reader task and the pool. Marker ownership
/// is tracked separately so an upstream removal cannot remove a folder that
/// the downstream connection still serves for marker-based routing.
#[derive(Clone)]
pub(crate) struct WorkspaceFolderSet {
    inner: Arc<Mutex<Option<Vec<WorkspaceFolder>>>>,
    marker_owned: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl WorkspaceFolderSet {
    /// Seed the set with the connection's initialize-time folders (`None` when
    /// the connection has no folders to advertise).
    pub(crate) fn new(initial: Option<Vec<WorkspaceFolder>>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Self::deduplicate(initial))),
            marker_owned: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Seed a connection whose initialize-time folders came from marker
    /// resolution rather than the upstream client's workspace list.
    pub(crate) fn new_marker_owned(initial: Option<Vec<WorkspaceFolder>>) -> Self {
        let initial = Self::deduplicate(initial);
        let marker_owned = initial
            .iter()
            .flatten()
            .map(|folder| folder.uri.as_str().to_string())
            .collect();
        Self {
            inner: Arc::new(Mutex::new(initial)),
            marker_owned: Arc::new(Mutex::new(marker_owned)),
        }
    }

    fn deduplicate(mut folders: Option<Vec<WorkspaceFolder>>) -> Option<Vec<WorkspaceFolder>> {
        if let Some(folders) = folders.as_mut() {
            let mut unique = Vec::<WorkspaceFolder>::with_capacity(folders.len());
            for folder in folders.drain(..) {
                if !unique.iter().any(|existing| existing.uri == folder.uri) {
                    unique.push(folder);
                }
            }
            *folders = unique;
        }
        folders
    }

    /// Snapshot the current folders for answering a `workspace/workspaceFolders`
    /// pull (the LSP `WorkspaceFolder[] | null` response).
    pub(crate) fn snapshot(&self) -> Option<Vec<WorkspaceFolder>> {
        self.inner
            .lock()
            .recover_poison("WorkspaceFolderSet::snapshot")
            .clone()
    }

    /// Replace the complete set, preserving the protocol distinction between
    /// `None` (`null`) and `Some(vec![])` (an explicitly empty folder list).
    pub(crate) fn replace(&self, folders: Option<Vec<WorkspaceFolder>>) {
        let mut inner = self
            .inner
            .lock()
            .recover_poison("WorkspaceFolderSet::replace");
        let mut marker_owned = self
            .marker_owned
            .lock()
            .recover_poison("WorkspaceFolderSet::marker_owned");
        *inner = Self::deduplicate(folders);
        marker_owned.clear();
    }

    /// Whether a folder with `folder`'s URI is already in the set.
    pub(crate) fn contains(&self, folder: &WorkspaceFolder) -> bool {
        self.inner
            .lock()
            .recover_poison("WorkspaceFolderSet::contains")
            .as_ref()
            .is_some_and(|folders| folders.iter().any(|existing| existing.uri == folder.uri))
    }

    /// Apply an upstream workspace-folder change atomically. Removals match by
    /// URI (folder names are display metadata), then additions append in event
    /// order while preserving URI uniqueness.
    pub(crate) fn apply_change(
        &self,
        added: Vec<WorkspaceFolder>,
        removed: &[WorkspaceFolder],
    ) -> bool {
        let mut guard = self
            .inner
            .lock()
            .recover_poison("WorkspaceFolderSet::apply_change");
        if guard.is_none() && added.is_empty() {
            return false;
        }
        let folders = guard.get_or_insert_with(Vec::new);
        let before_len = folders.len();
        folders.retain(|existing| !removed.iter().any(|removed| removed.uri == existing.uri));
        let mut changed = folders.len() != before_len;
        for folder in added {
            if !folders.iter().any(|existing| existing.uri == folder.uri) {
                folders.push(folder);
                changed = true;
            }
        }
        changed
    }

    /// Apply an upstream delta only after its effective wire delta is queued.
    /// Marker-owned URIs survive upstream removals; adding an already-present
    /// marker URI likewise needs no downstream notification.
    pub(crate) fn apply_upstream_change_and_announce<F>(
        &self,
        added: Vec<WorkspaceFolder>,
        removed: &[WorkspaceFolder],
        announce: F,
    ) -> bool
    where
        F: FnOnce(&[WorkspaceFolder], &[WorkspaceFolder]) -> bool,
    {
        let mut guard = self
            .inner
            .lock()
            .recover_poison("WorkspaceFolderSet::apply_upstream_change_and_announce");
        let effective_removed: Vec<_> = {
            let marker_owned = self
                .marker_owned
                .lock()
                .recover_poison("WorkspaceFolderSet::marker_owned");
            removed
                .iter()
                .filter(|folder| {
                    guard
                        .as_ref()
                        .is_some_and(|folders| folders.iter().any(|f| f.uri == folder.uri))
                        && !marker_owned.contains(folder.uri.as_str())
                })
                .cloned()
                .collect()
        };
        let mut effective_added = Vec::new();
        for folder in added {
            let already_present = guard.as_ref().is_some_and(|folders| {
                folders.iter().any(|existing| {
                    existing.uri == folder.uri
                        && !effective_removed
                            .iter()
                            .any(|removed| removed.uri == existing.uri)
                })
            });
            if !already_present
                && !effective_added
                    .iter()
                    .any(|accepted: &WorkspaceFolder| accepted.uri == folder.uri)
            {
                effective_added.push(folder);
            }
        }
        if effective_added.is_empty() && effective_removed.is_empty() {
            return true;
        }
        if !announce(&effective_added, &effective_removed) {
            return false;
        }
        let folders = guard.get_or_insert_with(Vec::new);
        folders.retain(|existing| {
            !effective_removed
                .iter()
                .any(|removed| removed.uri == existing.uri)
        });
        folders.extend(effective_added);
        true
    }

    /// Atomically add `folder` and announce it, returning whether the set now
    /// contains it. If `folder` is already present, returns `true` without
    /// calling `announce`. Otherwise `announce` runs **while the set lock is
    /// held** (it enqueues `workspace/didChangeWorkspaceFolders`): the folder is
    /// committed only when `announce` returns `true` (the notification queued).
    ///
    /// Holding the lock across the add + announce is what makes
    /// announce-before-`didOpen` safe under concurrency: a second caller cannot
    /// observe the folder as present — and so skip its own announce — until the
    /// first caller's notification is actually on the single-writer FIFO. If the
    /// announce fails to queue, nothing is committed (and a `None` set stays
    /// `None`), so a later acquisition re-attempts it rather than opening a
    /// document for an unannounced root.
    pub(crate) fn add_and_announce<F>(&self, folder: WorkspaceFolder, announce: F) -> bool
    where
        F: FnOnce() -> bool,
    {
        let mut guard = self
            .inner
            .lock()
            .recover_poison("WorkspaceFolderSet::add_and_announce");
        if let Some(folders) = guard.as_ref()
            && folders.iter().any(|existing| existing.uri == folder.uri)
        {
            self.marker_owned
                .lock()
                .recover_poison("WorkspaceFolderSet::marker_owned")
                .insert(folder.uri.as_str().to_string());
            return true;
        }
        if announce() {
            // Materialize the `None` set into `Some` ONLY on a committed add, so
            // a failed announce leaves the "answer null" state untouched.
            self.marker_owned
                .lock()
                .recover_poison("WorkspaceFolderSet::marker_owned")
                .insert(folder.uri.as_str().to_string());
            guard.get_or_insert_with(Vec::new).push(folder);
            true
        } else {
            false
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

    #[test]
    fn snapshot_returns_seeded_folders() {
        let set = WorkspaceFolderSet::new(Some(vec![folder("file:///a")]));
        assert_eq!(set.snapshot(), Some(vec![folder("file:///a")]));

        let empty = WorkspaceFolderSet::new(None);
        assert_eq!(empty.snapshot(), None);
    }

    #[test]
    fn initial_and_replacement_snapshots_are_uri_unique() {
        let renamed_a = WorkspaceFolder {
            uri: folder("file:///a").uri,
            name: "renamed".to_string(),
        };
        let set = WorkspaceFolderSet::new(Some(vec![folder("file:///a"), renamed_a.clone()]));
        assert_eq!(set.snapshot(), Some(vec![folder("file:///a")]));

        set.replace(Some(vec![renamed_a.clone(), folder("file:///a")]));
        assert_eq!(set.snapshot(), Some(vec![renamed_a]));
    }

    #[test]
    fn add_and_announce_commits_only_on_successful_announce() {
        let set = WorkspaceFolderSet::new(Some(vec![folder("file:///a")]));

        // Already present -> returns true WITHOUT calling announce.
        assert!(set.add_and_announce(folder("file:///a"), || panic!("must not announce present")));

        // New root, announce fails -> not committed, returns false.
        assert!(!set.add_and_announce(folder("file:///b"), || false));
        assert!(!set.contains(&folder("file:///b")));

        // New root, announce succeeds -> committed once.
        assert!(set.add_and_announce(folder("file:///b"), || true));
        // Second attempt at the same committed root -> no re-announce.
        assert!(set.add_and_announce(folder("file:///b"), || panic!("must not re-announce")));

        assert_eq!(
            set.snapshot(),
            Some(vec![folder("file:///a"), folder("file:///b")])
        );
    }

    #[test]
    fn failed_announce_keeps_a_none_set_null() {
        // Regression: a failed announce on a `None` (answer-null) set must not
        // collapse it to `Some([])`, which the reader would answer as `[]`.
        let set = WorkspaceFolderSet::new(None);
        assert!(!set.add_and_announce(folder("file:///a"), || false));
        assert_eq!(set.snapshot(), None, "None must survive a failed announce");
    }

    #[test]
    fn contains_matches_by_uri() {
        let set = WorkspaceFolderSet::new(Some(vec![folder("file:///a")]));
        assert!(set.contains(&folder("file:///a")));
        assert!(!set.contains(&folder("file:///b")));
        // A None set contains nothing.
        assert!(!WorkspaceFolderSet::new(None).contains(&folder("file:///a")));
    }

    #[test]
    fn add_and_announce_seeds_a_none_set_on_success() {
        let set = WorkspaceFolderSet::new(None);
        assert!(set.add_and_announce(folder("file:///a"), || true));
        assert_eq!(set.snapshot(), Some(vec![folder("file:///a")]));
    }

    #[test]
    fn apply_change_removes_by_uri_and_adds_without_duplicates() {
        let set = WorkspaceFolderSet::new(Some(vec![folder("file:///a"), folder("file:///b")]));

        set.apply_change(
            vec![folder("file:///b"), folder("file:///c")],
            &[folder("file:///a")],
        );

        assert_eq!(
            set.snapshot(),
            Some(vec![folder("file:///b"), folder("file:///c")])
        );
    }

    #[test]
    fn removal_only_change_preserves_a_none_set() {
        let set = WorkspaceFolderSet::new(None);

        assert!(!set.apply_change(Vec::new(), &[folder("file:///absent")]));

        assert_eq!(set.snapshot(), None);
    }

    #[test]
    fn replace_preserves_an_explicit_empty_set() {
        let set = WorkspaceFolderSet::new(None);

        set.replace(Some(Vec::new()));

        assert_eq!(set.snapshot(), Some(Vec::new()));
    }

    #[test]
    fn upstream_removal_retains_a_marker_owned_folder_without_announcing() {
        let marker = folder("file:///marker");
        let set = WorkspaceFolderSet::new_marker_owned(Some(vec![marker.clone()]));

        assert!(set.apply_upstream_change_and_announce(
            Vec::new(),
            &[marker.clone()],
            |_, _| panic!("marker ownership makes the wire delta empty"),
        ));

        assert_eq!(set.snapshot(), Some(vec![marker]));
    }

    #[test]
    fn failed_upstream_announce_keeps_prior_live_state() {
        let old = folder("file:///old");
        let set = WorkspaceFolderSet::new(Some(vec![old.clone()]));

        assert!(!set.apply_upstream_change_and_announce(
            vec![folder("file:///new")],
            &[old.clone()],
            |added, removed| {
                assert_eq!(added, &[folder("file:///new")]);
                assert_eq!(removed, &[old.clone()]);
                false
            },
        ));

        assert_eq!(set.snapshot(), Some(vec![old]));
    }

    #[test]
    fn upstream_remove_and_readd_replaces_the_folder_metadata() {
        let old = WorkspaceFolder {
            uri: folder("file:///same").uri,
            name: "old name".to_string(),
        };
        let renamed = WorkspaceFolder {
            uri: old.uri.clone(),
            name: "new name".to_string(),
        };
        let set = WorkspaceFolderSet::new(Some(vec![old.clone()]));

        assert!(set.apply_upstream_change_and_announce(
            vec![renamed.clone()],
            &[old.clone()],
            |added, removed| {
                assert_eq!(added, &[renamed.clone()]);
                assert_eq!(removed, &[old.clone()]);
                true
            },
        ));

        assert_eq!(set.snapshot(), Some(vec![renamed]));
    }

    #[test]
    fn upstream_change_deduplicates_added_uris_before_announcement() {
        let added = folder("file:///new");
        let duplicate = WorkspaceFolder {
            uri: added.uri.clone(),
            name: "duplicate name".to_string(),
        };
        let set = WorkspaceFolderSet::new(Some(Vec::new()));

        assert!(set.apply_upstream_change_and_announce(
            vec![added.clone(), duplicate],
            &[],
            |effective_added, effective_removed| {
                assert_eq!(effective_added, &[added.clone()]);
                assert!(effective_removed.is_empty());
                true
            },
        ));

        assert_eq!(set.snapshot(), Some(vec![added]));
    }
}
