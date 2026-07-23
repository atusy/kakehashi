use std::sync::Arc;

use tokio::sync::watch;
use tree_sitter::{InputEdit, Tree};

use super::snapshot::{ParseSnapshot, SnapshotSlot};

/// Immutable snapshot of document state for lock-free processing
pub(crate) struct DocumentSnapshot {
    text: Arc<str>,
    tree: Tree,
    incarnation: u64,
}

impl DocumentSnapshot {
    /// Get the text content
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Cheaply clone the text as a shared `Arc<str>` (a refcount bump, no copy)
    /// — for callers that need an owned handle to the snapshot's text, e.g. the
    /// host bridge's `HostRequestContext` (#498).
    pub(crate) fn text_arc(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    /// Get the parse tree
    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
    }

    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }
}

/// Unified document structure combining text, parsing, and LSP state
pub struct Document {
    /// Stored as `Arc<str>` so cloning the text — on every `snapshot()` and on
    /// each host-bridge live read — is a refcount bump rather than a full copy
    /// (#498). The cost is one `String → Arc<str>` reallocation per construct /
    /// edit, paid back by the many cheap clones per edit.
    text: Arc<str>,
    language_id: Option<String>,
    tree: Option<Tree>,
    /// Edited-but-not-yet-reparsed seed for the **off-ingress** incremental parse
    /// (per-document-parse-scheduler).
    ///
    /// `didChange` clears the reader-visible `tree` (so a reader never sees a tree
    /// that predates the edit — the flip's reader-safety invariant) but stashes the
    /// pre-edit tree here with the edit's `InputEdit`s already applied
    /// (`tree.edit()`). The off-ingress `reparse_latest` consumes it as
    /// `parser.parse(text, Some(seed))` to parse incrementally instead of from
    /// scratch. Coalesced edits accumulate their `InputEdit`s onto this same seed,
    /// so a burst still produces one correctly-edited seed for the final reparse.
    ///
    /// **Read by `reparse_latest` only** — never by `tree()` / `snapshot()`, which
    /// must stay `None` until the reparse lands. Cleared the moment a fresh tree is
    /// attached (`set_tree`) — a present tree means the seed is consumed — and on a
    /// full-text sync (`apply_edit_and_seed` with no `InputEdit`s), where seeding an
    /// unedited tree against wholly-replaced text would violate tree-sitter's
    /// incremental contract and corrupt external scanners (#348).
    pending_seed: Option<Tree>,
    /// The document's **open incarnation** — a process-wide-unique number drawn
    /// from [`DocumentStore`](crate::document::store::DocumentStore)'s monotonic
    /// counter at every construction (so a `didClose` + reopen of the same URI
    /// yields a fresh value). Stored *on the document* so a tree-write CAS or a
    /// watermark advance can check it **atomically with the document state**
    /// under the same shard lock — closing the residual where an in-flight
    /// off-ingress parse from a prior lifetime publishes against the reopened
    /// document (`per-document-parse-scheduler`, the `(incarnation, ticket)`
    /// epoch).
    ///
    /// Edits preserve it (an edit is the same lifetime); only a fresh
    /// construction (didOpen / a reordered mutation registering an unopened URI)
    /// draws a new one. A `Document` built outside the store keeps `0`;
    /// incarnation is only meaningful relative to that store's counter, and the
    /// store assigns every document it owns a nonzero value.
    incarnation: u64,
    /// The document's monotonic **input version** (parse-snapshot ADR §1):
    /// `0` at construction (didOpen), bumped on every text mutation —
    /// incremental edit and full-text sync alike. Parse-result writes never
    /// touch it: it versions the *inputs*, so a derived `ParseSnapshot` can
    /// carry the `parsed_version` it was computed from and readers can compare
    /// staleness (`parsed_version < content_version`) without any wait.
    content_version: u64,
    /// The per-URI snapshot cell (parse-snapshot ADR §2), co-located on the
    /// document so one store lookup yields both the live inputs (incarnation,
    /// `content_version`) and the latest [`ParseSnapshot`] — a single
    /// authoritative incarnation, no cross-map TOCTOU. Seeded at bootstrap
    /// (`snapshot: None`) for this lifetime; a reopen constructs a fresh
    /// `Document` and with it a fresh cell, which is what clears the version
    /// floor for the new lifetime.
    snapshot_tx: watch::Sender<SnapshotSlot>,
    /// Store-wide settings generation floor shared with every document so a
    /// snapshot published concurrently with a reload inherits the new floor
    /// under the snapshot cell's publication lock.
    semantic_artifact_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl Document {
    /// Create a new document with just text
    pub(crate) fn new(text: String, incarnation: u64) -> Self {
        Self {
            text: Arc::from(text),
            language_id: None,
            tree: None,
            pending_seed: None,
            incarnation,
            content_version: 0,
            snapshot_tx: watch::Sender::new(SnapshotSlot::bootstrap(incarnation)),
            semantic_artifact_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Create with language but no tree yet (for early document registration)
    pub(crate) fn with_language(text: String, language_id: String, incarnation: u64) -> Self {
        Self {
            text: Arc::from(text),
            language_id: Some(language_id),
            tree: None,
            pending_seed: None,
            incarnation,
            content_version: 0,
            snapshot_tx: watch::Sender::new(SnapshotSlot::bootstrap(incarnation)),
            semantic_artifact_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Create with language and tree
    pub(crate) fn with_tree(
        text: String,
        language_id: String,
        tree: Tree,
        incarnation: u64,
    ) -> Self {
        Self {
            text: Arc::from(text),
            language_id: Some(language_id),
            tree: Some(tree),
            pending_seed: None,
            incarnation,
            content_version: 0,
            snapshot_tx: watch::Sender::new(SnapshotSlot::bootstrap(incarnation)),
            semantic_artifact_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub(crate) fn with_semantic_artifact_generation(
        mut self,
        generation: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        self.semantic_artifact_generation = generation;
        self
    }

    /// Get the text content
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Cheaply clone the text as a shared `Arc<str>` (a refcount bump, no copy).
    /// Used by the host-bridge live read so reading the document's current text
    /// under the lock no longer full-copies it (#498).
    pub(crate) fn text_arc(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    /// Get the language ID
    pub(crate) fn language_id(&self) -> Option<&str> {
        self.language_id.as_deref()
    }

    /// Get the tree
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// The document's open incarnation (see the [`incarnation`](Self::incarnation)
    /// field).
    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// The document's monotonic input version (see the
    /// [`content_version`](Self::content_version) field).
    pub(crate) fn content_version(&self) -> u64 {
        self.content_version
    }

    /// Install `snapshot` in this document's cell iff the slot admits it —
    /// the one publish primitive (parse-snapshot ADR §2), executed inside
    /// `send_if_modified` so the guard and the write are atomic under the
    /// channel's own lock. Returns whether the publish landed; a rejected
    /// publish (a racing edit's newer snapshot, a reopen, a close) must make
    /// the caller emit no downstream effects.
    pub(crate) fn publish_snapshot(&self, snapshot: Arc<ParseSnapshot>) -> bool {
        let mut installed = false;
        self.snapshot_tx.send_if_modified(|slot| {
            if slot.admits(&snapshot) {
                snapshot.advance_semantic_artifact_generation(
                    self.semantic_artifact_generation
                        .load(std::sync::atomic::Ordering::Acquire),
                );
                if let Some(previous) = slot.snapshot.as_ref() {
                    previous.cancel_semantic_artifact();
                }
                slot.snapshot = Some(Arc::clone(&snapshot));
                installed = true;
            }
            installed
        });
        installed
    }

    /// Install the terminal closed slot (see
    /// [`CLOSED_INCARNATION`](super::snapshot::CLOSED_INCARNATION)): wakes any
    /// reader parked on the first-parse `watch::changed()` and rejects every
    /// later publish, including a stale same-lifetime one that would otherwise
    /// pass the bootstrap branch. Explicit because stale parse tasks may hold
    /// `Sender` clones that keep the channel alive past this document's drop.
    pub(crate) fn publish_closed(&self) {
        self.cancel_current_semantic_artifact();
        self.snapshot_tx.send_replace(SnapshotSlot::closed());
    }

    /// Borrow the latest snapshot slot, wait-free (cheap `Arc` clones).
    pub(crate) fn latest_snapshot_slot(&self) -> SnapshotSlot {
        self.snapshot_tx.borrow().clone()
    }

    /// Subscribe for slot changes — used only by the bounded first-parse wait
    /// (and Stage 2's explicit-action wait); per-keystroke readers never wait.
    pub(crate) fn subscribe_snapshots(&self) -> watch::Receiver<SnapshotSlot> {
        self.snapshot_tx.subscribe()
    }

    /// Create an immutable snapshot of current document state
    ///
    /// Returns None if document is not fully initialized (missing tree).
    /// The snapshot clones text and tree to enable lock-free processing.
    pub(crate) fn snapshot(&self) -> Option<DocumentSnapshot> {
        Some(DocumentSnapshot {
            text: self.text.clone(),
            tree: self.tree.as_ref()?.clone(),
            incarnation: self.incarnation,
        })
    }

    /// Install a freshly parsed tree together with its text.
    ///
    /// Replaces the text, so it counts as an input mutation and bumps
    /// `content_version` (its callers pass the text the tree was parsed from,
    /// which may or may not equal the stored text; bumping unconditionally
    /// keeps the version a safe upper bound — a spurious bump only marks a
    /// snapshot stale early, never serves a wrong one).
    #[cfg(test)]
    pub(crate) fn update_tree_and_text(&mut self, new_tree: Tree, new_text: String) {
        self.cancel_current_semantic_artifact();
        self.text = Arc::from(new_text);
        self.content_version += 1;
        self.tree = Some(new_tree);
        // Any pending incremental seed is for an edit superseded by this fresh
        // tree+text; keep the invariant "visible tree present ⟹ no stale seed" so a
        // later `reparse_latest` can't seed from a tree that predates this text
        // (the #348 contract hazard).
        self.pending_seed = None;
    }

    /// Attach a parsed tree **without** touching the text.
    ///
    /// For an on-demand reader parse whose parsed text already equals the stored
    /// text: there is no content-version transition to record, so the text is
    /// neither re-cloned nor replaced.
    ///
    /// Clears `pending_seed`: a present reader-visible tree means the off-ingress
    /// reparse's seed has been consumed (the tree it would have produced is now
    /// here), so the next `didChange` seeds from this fresh tree, not a stale seed.
    pub(crate) fn set_tree(&mut self, tree: Tree) {
        self.tree = Some(tree);
        self.pending_seed = None;
    }

    pub(crate) fn invalidate_parse(&mut self) {
        self.cancel_current_semantic_artifact();
        self.tree = None;
        self.pending_seed = None;
        // Grammar/query settings are parse inputs even when text is unchanged.
        // Advancing the internal version makes every pre-reload parse result
        // stale and lets the scheduled current-generation snapshot supersede it.
        self.content_version = self.content_version.wrapping_add(1);
        let semantic_artifact = Arc::new(crate::analysis::SemanticArtifactSlot::new());
        semantic_artifact.advance_minimum_generation(
            self.semantic_artifact_generation
                .load(std::sync::atomic::Ordering::Acquire),
        );
        self.snapshot_tx.send_replace(SnapshotSlot {
            current_incarnation: self.incarnation,
            snapshot: Some(Arc::new(ParseSnapshot {
                text: Arc::clone(&self.text),
                tree: None,
                language: self.language_id.clone(),
                parsed_version: self.content_version,
                incarnation: self.incarnation,
                injection_regions: None,
                bridge_regions: None,
                resolved_regions: None,
                layer_trees: std::sync::OnceLock::new(),
                semantic_artifact,
            })),
        });
    }

    /// Store the open-time parse result — the detected `language` and the parsed
    /// `tree` (`None` for a parsed-to-nothing / no-language / crashed-parser open) —
    /// **preserving the existing text**.
    ///
    /// For the didOpen parse, whose text was already stored when the document was
    /// registered: it reparses that same text and records the result *in place*,
    /// rather than re-inserting a fresh copy of the (potentially large) text. Clears
    /// any `pending_seed` (the open path never has one — it is set only by an edit).
    pub(crate) fn set_parse_result(&mut self, language: Option<String>, tree: Option<Tree>) {
        self.language_id = language;
        self.tree = tree;
        self.pending_seed = None;
    }

    /// Apply an edit's new text and stash an **incremental parse seed** for the
    /// off-ingress reparse, clearing the reader-visible tree.
    ///
    /// The reader-visible `tree` is cleared (a reader must never see a tree that
    /// predates this edit). The pre-edit tree — or the seed already accumulated by
    /// an earlier coalesced edit — has `edits` applied via `tree.edit()` and is
    /// stashed in `pending_seed` for `reparse_latest` to parse incrementally.
    ///
    /// With **no** `edits` (a full-text sync) the seed is dropped to `None`: seeding
    /// an unedited tree against wholly-replaced text violates tree-sitter's
    /// incremental contract and corrupted external scanners in #348, so a full-text
    /// sync must parse from scratch.
    pub(crate) fn apply_edit_and_seed(&mut self, new_text: String, edits: &[InputEdit]) {
        self.cancel_current_semantic_artifact();
        // Base the seed on the reader-visible tree if present, else the seed an
        // earlier coalesced edit already accumulated (the visible tree is cleared
        // on the first edit of a burst, so subsequent edits chain onto the seed).
        let base = self.tree.take().or_else(|| self.pending_seed.take());
        self.pending_seed = match base {
            Some(mut tree) if !edits.is_empty() => {
                for edit in edits {
                    tree.edit(edit);
                }
                Some(tree)
            }
            // Full-text sync (no edits) or no base tree: parse from scratch (#348).
            _ => None,
        };
        self.text = Arc::from(new_text);
        self.content_version += 1;
    }

    /// The off-ingress incremental parse seed, if any. **Read only by
    /// `reparse_latest`** — `tree()` / `snapshot()` deliberately ignore it so a
    /// reader never observes a pre-reparse tree. See [`pending_seed`](Self::pending_seed).
    pub(crate) fn pending_seed(&self) -> Option<&Tree> {
        self.pending_seed.as_ref()
    }

    /// Update text and clear layers/state
    #[cfg(test)]
    pub(crate) fn update_text(&mut self, text: String) {
        self.cancel_current_semantic_artifact();
        self.text = Arc::from(text);
        // Note: Tree needs to be rebuilt after text change
        self.tree = None;
        self.pending_seed = None;
        self.content_version += 1;
    }

    fn cancel_current_semantic_artifact(&self) {
        if let Some(snapshot) = self.snapshot_tx.borrow().snapshot.as_ref() {
            snapshot.cancel_semantic_artifact();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_creation() {
        let doc = Document::new("hello world".to_string(), 0);
        assert_eq!(doc.text(), "hello world");
        assert_eq!(doc.text().len(), 11);
        assert!(!doc.text().is_empty());
    }

    #[test]
    fn test_document_with_layer() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();

        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        assert_eq!(doc.text(), "fn main() {}");
        assert!(doc.tree().is_some());
        assert_eq!(doc.language_id(), Some("rust"));
    }

    /// The parse-snapshot model's input-side version (§1): `0` at construction,
    /// bumped on every text mutation (incremental edit and full-text sync
    /// alike), and NOT bumped by parse-result writes — the version tracks the
    /// *inputs*, and a parse landing changes only derived state.
    #[test]
    fn content_version_tracks_text_mutations_only() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        let mut doc = Document::new("fn main() {}".to_string(), 1);
        assert_eq!(doc.content_version(), 0, "fresh document starts at 0");

        // Full-text sync bumps.
        doc.update_text("fn main() { }".to_string());
        assert_eq!(doc.content_version(), 1);

        // A parse-result write does not bump.
        let tree = parser.parse("fn main() { }", None).unwrap();
        doc.set_tree(tree.clone());
        assert_eq!(
            doc.content_version(),
            1,
            "set_tree is not an input mutation"
        );
        doc.set_parse_result(Some("rust".to_string()), Some(tree));
        assert_eq!(
            doc.content_version(),
            1,
            "set_parse_result is not an input mutation"
        );

        // An incremental edit bumps.
        doc.apply_edit_and_seed("fn main() {  }".to_string(), &[]);
        assert_eq!(doc.content_version(), 2);
    }

    #[test]
    fn test_update_text() {
        let mut doc = Document::new("initial".to_string(), 0);
        doc.update_text("updated".to_string());
        assert_eq!(doc.text(), "updated");
        assert!(doc.tree().is_none());
    }

    /// A non-empty edit stashes an incremental parse seed and clears the
    /// reader-visible tree (readers must not see a pre-reparse tree).
    #[test]
    fn apply_edit_and_seed_stashes_seed_and_clears_tree() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        // Insert a space at byte 11 (before the closing brace): "fn main() { }".
        let edit = InputEdit {
            start_byte: 11,
            old_end_byte: 11,
            new_end_byte: 12,
            start_position: tree_sitter::Point::new(0, 11),
            old_end_position: tree_sitter::Point::new(0, 11),
            new_end_position: tree_sitter::Point::new(0, 12),
        };
        doc.apply_edit_and_seed("fn main() { }".to_string(), &[edit]);

        assert!(doc.tree().is_none(), "reader-visible tree must be cleared");
        assert!(
            doc.pending_seed().is_some(),
            "incremental seed must be stashed"
        );
        assert_eq!(doc.text(), "fn main() { }");
    }

    /// A full-text sync (no `InputEdit`s) must drop the seed: seeding an unedited
    /// tree against wholly-replaced text is the tree-sitter contract violation that
    /// caused the #348 heap corruption.
    #[test]
    fn apply_edit_and_seed_drops_seed_on_full_text_sync() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        // Full-text sync carries no InputEdits.
        doc.apply_edit_and_seed("totally different content".to_string(), &[]);

        assert!(doc.tree().is_none());
        assert!(
            doc.pending_seed().is_none(),
            "full-text sync must not leave a stale seed (#348)"
        );
    }

    /// Coalesced edits accumulate onto the same seed: after a first edit clears the
    /// visible tree, a second edit chains its `InputEdit` onto the stashed seed.
    #[test]
    fn apply_edit_and_seed_coalesces_across_edits() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        let edit1 = InputEdit {
            start_byte: 11,
            old_end_byte: 11,
            new_end_byte: 12,
            start_position: tree_sitter::Point::new(0, 11),
            old_end_position: tree_sitter::Point::new(0, 11),
            new_end_position: tree_sitter::Point::new(0, 12),
        };
        doc.apply_edit_and_seed("fn main() { }".to_string(), &[edit1]);
        assert!(doc.tree().is_none());

        // Second edit lands while the visible tree is still cleared: it must chain
        // onto the accumulated seed, not silently drop incrementality.
        let edit2 = InputEdit {
            start_byte: 12,
            old_end_byte: 12,
            new_end_byte: 13,
            start_position: tree_sitter::Point::new(0, 12),
            old_end_position: tree_sitter::Point::new(0, 12),
            new_end_position: tree_sitter::Point::new(0, 13),
        };
        doc.apply_edit_and_seed("fn main() {  }".to_string(), &[edit2]);

        assert!(doc.tree().is_none());
        assert!(
            doc.pending_seed().is_some(),
            "coalesced edit must keep the accumulated seed"
        );
        assert_eq!(doc.text(), "fn main() {  }");
    }

    /// Attaching a fresh tree consumes (clears) the pending seed.
    #[test]
    fn set_tree_clears_pending_seed() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        let edit = InputEdit {
            start_byte: 11,
            old_end_byte: 11,
            new_end_byte: 12,
            start_position: tree_sitter::Point::new(0, 11),
            old_end_position: tree_sitter::Point::new(0, 11),
            new_end_position: tree_sitter::Point::new(0, 12),
        };
        doc.apply_edit_and_seed("fn main() { }".to_string(), &[edit]);
        assert!(doc.pending_seed().is_some());

        let reparsed = parser.parse("fn main() { }", None).unwrap();
        doc.set_tree(reparsed);

        assert!(doc.tree().is_some());
        assert!(
            doc.pending_seed().is_none(),
            "attaching a fresh tree must consume the seed"
        );
    }

    /// Every method that installs a fresh visible tree must clear `pending_seed`,
    /// upholding "visible tree present ⟹ no stale seed" — else a later
    /// `reparse_latest` could seed from a tree predating the new text (#348).
    #[test]
    fn fresh_tree_updates_clear_pending_seed() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let edit = InputEdit {
            start_byte: 11,
            old_end_byte: 11,
            new_end_byte: 12,
            start_position: tree_sitter::Point::new(0, 11),
            old_end_position: tree_sitter::Point::new(0, 11),
            new_end_position: tree_sitter::Point::new(0, 12),
        };

        // update_tree_and_text clears the seed.
        let tree = parser.parse("fn main() {}", None).unwrap();
        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);
        doc.apply_edit_and_seed("fn main() { }".to_string(), &[edit]);
        assert!(doc.pending_seed().is_some());
        let t2 = parser.parse("fn main() { }", None).unwrap();
        doc.update_tree_and_text(t2, "fn main() { }".to_string());
        assert!(
            doc.pending_seed().is_none(),
            "update_tree_and_text must clear the pending seed"
        );
    }

    #[test]
    fn test_document_snapshot() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();

        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        // Snapshot should succeed for fully initialized document
        let snapshot = doc.snapshot();
        assert!(snapshot.is_some());

        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.text(), "fn main() {}");
        assert_eq!(snapshot.tree().root_node().kind(), "source_file");
    }

    #[test]
    fn test_document_snapshot_none_when_no_tree() {
        let doc = Document::new("test".to_string(), 0);
        // No tree, so snapshot should be None
        assert!(doc.snapshot().is_none());
    }

    #[test]
    fn test_document_snapshot_clones_independently() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();

        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        // Create snapshot
        let snapshot = doc.snapshot().unwrap();

        // Snapshot content matches the document. The text now shares the
        // document's `Arc<str>` allocation (a cheap clone — see
        // `snapshot_text_shares_the_document_allocation`); it stays a valid
        // immutable snapshot because any edit installs a *new* `Arc` on the
        // document rather than mutating this one.
        assert_eq!(snapshot.text(), doc.text());
        assert_eq!(
            snapshot.tree().root_node().kind(),
            doc.tree().unwrap().root_node().kind()
        );
    }

    #[test]
    fn text_arc_is_a_cheap_shared_clone() {
        let doc = Document::new("shared text".to_string(), 0);
        let a = doc.text_arc();
        let b = doc.text_arc();
        // Both handles point to the SAME allocation — a refcount bump, not a
        // copy (#498).
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(&*a, "shared text");
    }

    #[test]
    fn snapshot_text_shares_the_document_allocation() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        let snapshot = doc.snapshot().unwrap();

        // `snapshot()` clones the text `Arc` (refcount bump) rather than copying
        // the bytes — the whole point of #498.
        assert!(Arc::ptr_eq(&doc.text_arc(), &snapshot.text_arc()));
    }

    #[test]
    fn update_text_installs_a_fresh_allocation() {
        // An edit replaces the `Arc` (so prior snapshots keep their bytes); the
        // new text is correct.
        let mut doc = Document::new("v1".to_string(), 0);
        let before = doc.text_arc();
        doc.update_text("v2".to_string());
        let after = doc.text_arc();
        assert_eq!(&*after, "v2");
        assert!(!Arc::ptr_eq(&before, &after), "edit installs a new Arc");
        assert_eq!(&*before, "v1", "the prior Arc still holds the old text");
    }
}
