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
    /// The edits since the published tree, each tagged with the content
    /// version it produced — the inputs of the **off-ingress** incremental
    /// parse's seed (per-document-parse-scheduler), which
    /// [`incremental_seed`](Self::incremental_seed) derives by replaying
    /// them onto a clone of the published tree. Entries the published tree
    /// already consumed (tagged at or below its `parsed_version`) are pruned
    /// on the next edit; a burst that outgrows [`MAX_SEED_EDITS`] gives up on
    /// seeding (a full parse is cheaper than an unbounded log).
    seed_edits: Vec<(u64, InputEdit)>,
    /// The content version below which no published tree may seed a parse.
    /// A full-text sync (`apply_edit` with no `InputEdit`s), a grammar
    /// reload, and an edit log that outgrows [`MAX_SEED_EDITS`] set it to the
    /// version they produce: seeding an unedited tree against wholly-replaced
    /// text violates tree-sitter's incremental contract and corrupted
    /// external scanners (#348), and the edits that follow cannot be replayed
    /// onto a tree from before the cut either.
    seed_floor: u64,
    /// The document's **open incarnation** — a process-wide-unique number drawn
    /// from [`DocumentStore`](crate::document::store::DocumentStore)'s monotonic
    /// counter at every construction (so a `didClose` + reopen of the same URI
    /// yields a fresh value). Stored *on the document* so `install_parse` or a
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
    /// Cancellation scope for work derived from the current input version.
    /// Every input mutation cancels this token before installing a fresh one,
    /// so queued or running parse/derived work can stop once its
    /// `(incarnation, content_version)` is obsolete.
    version_cancel: crate::cancel::CancelToken,
}

/// The most edits `seed_edits` keeps before the document gives up on seeding
/// the next parse incrementally.
const MAX_SEED_EDITS: usize = 1024;

/// A published tree plus the edits it has not seen, in order — the
/// incremental parse seed before its replay (see
/// [`Document::incremental_seed`]).
pub(crate) struct IncrementalSeed {
    tree: Tree,
    edits: Vec<InputEdit>,
}

impl IncrementalSeed {
    /// Apply the edits to the tree, in order, yielding the seed tree-sitter's
    /// incremental contract requires: the old tree with exactly the edits
    /// between its text and the current text applied (#348). Editing the
    /// clone leaves the published snapshot's own tree untouched (tree-sitter
    /// copies the edited path on write).
    pub(crate) fn replay(self) -> Tree {
        let Self { mut tree, edits } = self;
        for edit in &edits {
            tree.edit(edit);
        }
        tree
    }
}

impl Document {
    /// Create a new document with just text
    pub(crate) fn new(text: String, incarnation: u64) -> Self {
        Self {
            text: Arc::from(text),
            language_id: None,
            seed_edits: Vec::new(),
            seed_floor: 0,
            incarnation,
            content_version: 0,
            snapshot_tx: watch::Sender::new(SnapshotSlot::bootstrap(incarnation)),
            version_cancel: crate::cancel::CancelToken::default(),
        }
    }

    /// Create with language but no tree yet (for early document registration)
    pub(crate) fn with_language(text: String, language_id: String, incarnation: u64) -> Self {
        Self {
            text: Arc::from(text),
            language_id: Some(language_id),
            seed_edits: Vec::new(),
            seed_floor: 0,
            incarnation,
            content_version: 0,
            snapshot_tx: watch::Sender::new(SnapshotSlot::bootstrap(incarnation)),
            version_cancel: crate::cancel::CancelToken::default(),
        }
    }

    /// Create with language and an already-parsed tree: published as the
    /// lifetime's first snapshot, at version 0.
    pub(crate) fn with_tree(
        text: String,
        language_id: String,
        tree: Tree,
        incarnation: u64,
    ) -> Self {
        let doc = Self::with_language(text, language_id, incarnation);
        doc.snapshot_tx
            .send_replace(doc.slot_with(doc.bare_snapshot(Some(tree))));
        doc
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

    /// The current parse's tree: the published snapshot's, iff that snapshot
    /// is this lifetime's and parsed this content version. An edit makes the
    /// published snapshot stale and a reload replaces it with a tree-less
    /// placeholder, so this is `None` until the reparse lands — a reader
    /// never sees a tree that predates the text. (`Tree` clone is a retain
    /// plus a small allocation; a presence probe should use
    /// [`has_current_tree`](Self::has_current_tree).)
    pub(crate) fn tree(&self) -> Option<Tree> {
        self.current_snapshot()
            .and_then(|snapshot| snapshot.tree.clone())
    }

    /// Whether [`tree`](Self::tree) would be `Some`, without cloning it.
    pub(crate) fn has_current_tree(&self) -> bool {
        let slot = self.snapshot_tx.borrow();
        slot.snapshot
            .as_ref()
            .is_some_and(|snapshot| self.is_current(snapshot) && snapshot.tree.is_some())
    }

    /// The published snapshot iff it is this lifetime's and parsed the
    /// current content version (parse-snapshot ADR §2 currency).
    fn current_snapshot(&self) -> Option<Arc<ParseSnapshot>> {
        let slot = self.snapshot_tx.borrow();
        slot.snapshot
            .as_ref()
            .filter(|snapshot| self.is_current(snapshot))
            .cloned()
    }

    fn is_current(&self, snapshot: &ParseSnapshot) -> bool {
        snapshot.incarnation == self.incarnation && snapshot.parsed_version == self.content_version
    }

    /// A snapshot of the current inputs carrying `tree` and nothing derived —
    /// the reload placeholder and the test fixtures' published tree.
    fn bare_snapshot(&self, tree: Option<Tree>) -> Arc<ParseSnapshot> {
        Arc::new(ParseSnapshot {
            text: Arc::clone(&self.text),
            tree,
            language: self.language_id.clone(),
            parsed_version: self.content_version,
            incarnation: self.incarnation,
            injection_regions: None,
            bridge_regions: None,
            resolved_regions: None,
            layer_trees: std::sync::OnceLock::new(),
        })
    }

    fn slot_with(&self, snapshot: Arc<ParseSnapshot>) -> SnapshotSlot {
        SnapshotSlot {
            current_incarnation: self.incarnation,
            snapshot: Some(snapshot),
        }
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

    /// Clone the cancellation scope bound to this document input version.
    pub(crate) fn version_cancel_token(&self) -> crate::cancel::CancelToken {
        self.version_cancel.clone()
    }

    fn advance_input_version(&mut self) {
        self.version_cancel.cancel();
        self.version_cancel = crate::cancel::CancelToken::default();
        self.content_version = self.content_version.wrapping_add(1);
    }

    /// Install `snapshot` in this document's cell iff the slot admits it —
    /// the one publish primitive (parse-snapshot ADR §2), executed inside
    /// `send_if_modified` so the guard and the write are atomic under the
    /// channel's own lock. Returns whether the publish landed; a rejected
    /// publish (a racing edit's newer snapshot, a reopen, a close) must make
    /// the caller emit no downstream effects. Takes the snapshot by reference
    /// so the caller keeps ownership of a rejected one and can drop it after
    /// releasing whatever guard it holds: destroying a tree and its region
    /// vectors is not free.
    pub(crate) fn publish_snapshot(&self, snapshot: &Arc<ParseSnapshot>) -> bool {
        let mut installed = false;
        self.snapshot_tx.send_if_modified(|slot| {
            if slot.admits(snapshot) {
                slot.snapshot = Some(Arc::clone(snapshot));
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
        self.version_cancel.cancel();
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
    /// Returns None while the document has no current tree (see
    /// [`tree`](Self::tree)). Text and tree both come from the published
    /// snapshot — the text the tree was parsed from, by construction — so
    /// their consistency is structural, not an invariant of the text writers.
    pub(crate) fn snapshot(&self) -> Option<DocumentSnapshot> {
        let snapshot = self.current_snapshot()?;
        Some(DocumentSnapshot {
            tree: snapshot.tree.clone()?,
            text: Arc::clone(&snapshot.text),
            incarnation: self.incarnation,
        })
    }

    /// Install a freshly parsed tree together with its text.
    ///
    /// Replaces the text, so it counts as an input mutation and bumps
    /// `content_version`, then publishes the tree at that version (a newer
    /// tree-bearing snapshot is always admitted).
    #[cfg(test)]
    pub(crate) fn update_tree_and_text(&mut self, new_tree: Tree, new_text: String) {
        self.text = Arc::from(new_text);
        self.advance_input_version();
        // The text was replaced wholesale: no earlier tree may seed against it
        // (#348); the tree published here, at this version, may.
        self.seed_edits.clear();
        self.seed_floor = self.content_version;
        self.publish_snapshot(&self.bare_snapshot(Some(new_tree)));
    }

    /// Publish a tree-less placeholder at the bumped version on a
    /// settings/grammar reload (see `ParseSnapshot`), which is what takes the
    /// tree away from readers, and forbid seeding from any pre-reload tree.
    pub(crate) fn invalidate_parse(&mut self) {
        // Grammar/query settings are parse inputs even when text is unchanged.
        // Advancing the internal version makes every pre-reload parse result
        // stale and lets the scheduled current-generation snapshot supersede it.
        self.advance_input_version();
        self.seed_edits.clear();
        self.seed_floor = self.content_version;
        self.snapshot_tx
            .send_replace(self.slot_with(self.bare_snapshot(None)));
    }

    /// Record a parse result's `language` (`None` for a no-language parse)
    /// **preserving the existing text**; the tree itself reaches readers only
    /// through the published snapshot. Reached only through
    /// `DocumentStore::install_parse`, for every parse path (open,
    /// installed-grammar reparse, edit reparse).
    pub(crate) fn record_language(&mut self, language: Option<String>) {
        self.language_id = language;
    }

    /// Apply an edit's new text and log its `InputEdit`s for the off-ingress
    /// reparse's **incremental parse seed**.
    ///
    /// Bumping the content version makes the published snapshot stale, so a
    /// reader never sees a tree that predates this edit. The edits are logged
    /// under the new version; [`incremental_seed`](Self::incremental_seed)
    /// replays them (and those of any coalesced edit before the reparse) onto
    /// a clone of the published tree.
    ///
    /// With **no** `edits` (a full-text sync) the seed is dropped to `None`: seeding
    /// an unedited tree against wholly-replaced text violates tree-sitter's
    /// incremental contract and corrupted external scanners in #348, so a full-text
    /// sync must parse from scratch.
    pub(crate) fn apply_edit(&mut self, new_text: String, edits: &[InputEdit]) {
        self.text = Arc::from(new_text);
        self.advance_input_version();
        if edits.is_empty() {
            // Full-text sync: parse from scratch (#348), and nothing published
            // before this version may seed the edits that follow either.
            self.seed_edits.clear();
            self.seed_floor = self.content_version;
            return;
        }
        // Entries the published tree already consumed are dead weight: a
        // seed replays only edits after that tree's version.
        let consumed = self.published_parsed_version();
        self.seed_edits.retain(|(version, _)| *version > consumed);
        self.seed_edits
            .extend(edits.iter().map(|edit| (self.content_version, *edit)));
        if self.seed_edits.len() > MAX_SEED_EDITS {
            self.seed_edits.clear();
            self.seed_floor = self.content_version;
        }
    }

    /// The version the published snapshot (this lifetime's) parsed, if any.
    fn published_parsed_version(&self) -> u64 {
        let slot = self.snapshot_tx.borrow();
        slot.snapshot
            .as_ref()
            .filter(|snapshot| snapshot.incarnation == self.incarnation)
            .map_or(0, |snapshot| snapshot.parsed_version)
    }

    /// The inputs of the off-ingress incremental parse seed, if any: a clone
    /// of the published tree and the logged edits after its version, for
    /// [`IncrementalSeed::replay`] to apply via `tree.edit()` so the reparse
    /// can `parser.parse(text, Some(&seed))` instead of parsing from scratch.
    /// Only the cheap part happens here (a tree retain plus a small
    /// allocation, and a copy of the edit tail), so the caller's store guard
    /// is released before the replay's per-edit path copies run — on the
    /// compute pool, with the parse. `None` when nothing is published, the
    /// published snapshot is tree-less, or it predates a full-text sync or
    /// reload (`seed_floor`). **Read only by `reparse_latest`** — readers go
    /// through [`tree`](Self::tree), which never serves a pre-reparse tree.
    pub(crate) fn incremental_seed(&self) -> Option<IncrementalSeed> {
        let slot = self.snapshot_tx.borrow();
        let snapshot = slot
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.incarnation == self.incarnation)?;
        if snapshot.parsed_version < self.seed_floor {
            return None;
        }
        let tree = snapshot.tree.clone()?;
        let edits = self
            .seed_edits
            .iter()
            .filter(|(version, _)| *version > snapshot.parsed_version)
            .map(|(_, edit)| *edit)
            .collect();
        Some(IncrementalSeed { tree, edits })
    }

    /// Update text and clear layers/state
    #[cfg(test)]
    pub(crate) fn update_text(&mut self, text: String) {
        // A full-text sync: the version bump makes the published tree stale
        // for readers, and nothing may seed against the replaced text.
        self.apply_edit(text, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_tree(text: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    fn edit(start: usize, old_end: usize, new_end: usize) -> InputEdit {
        InputEdit {
            start_byte: start,
            old_end_byte: old_end,
            new_end_byte: new_end,
            start_position: tree_sitter::Point::new(0, start),
            old_end_position: tree_sitter::Point::new(0, old_end),
            new_end_position: tree_sitter::Point::new(0, new_end),
        }
    }

    fn seed_tree(doc: &Document) -> Option<Tree> {
        Document::incremental_seed(doc, "rust").map(IncrementalSeed::replay)
    }

    /// The seed's own geometry against `text`: tree-sitter's contract is
    /// that the seed's byte positions already match the new text, so its
    /// root spans the whole text and its closing brace sits where the text
    /// has one. This tells a missing, duplicated or reordered edit apart —
    /// the eventual incremental parse cannot, since tree-sitter recovers
    /// the right tree from a wrong seed on inputs this small.
    fn seed_matches_text(seed: &Tree, text: &str) -> bool {
        let root = seed.root_node();
        let close = root
            .named_child(0)
            .and_then(|function| function.child_by_field_name("body"))
            .and_then(|body| body.child(body.child_count().saturating_sub(1) as u32));
        root.end_byte() == text.len()
            && close.is_some_and(|close| {
                close.kind() == "}" && Some(close.start_byte()) == text.rfind('}')
            })
    }

    fn incremental_parse_matches_fresh(seed: &Tree, text: &str) -> bool {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let incremental = parser.parse(text, Some(seed)).unwrap();
        let fresh = parser.parse(text, None).unwrap();
        seed_matches_text(seed, text)
            && incremental.root_node().to_sexp() == fresh.root_node().to_sexp()
            && incremental.root_node().end_byte() == text.len()
    }

    fn snapshot_at(doc: &Document, text: &str, parsed_version: u64) -> Arc<ParseSnapshot> {
        Arc::new(ParseSnapshot {
            text: Arc::from(text),
            tree: Some(rust_tree(text)),
            language: Some("rust".to_string()),
            parsed_version,
            incarnation: doc.incarnation(),
            injection_regions: None,
            bridge_regions: None,
            resolved_regions: None,
            layer_trees: std::sync::OnceLock::new(),
        })
    }

    /// A seed is only good for the grammar that produced its tree: detection
    /// on the edited text may select another language (a changed shebang),
    /// and tree-sitter does not check that an old tree belongs to the parser
    /// it is handed, so a seed for another grammar must parse from scratch.
    #[test]
    fn a_seed_is_refused_for_a_grammar_other_than_its_trees() {
        let mut doc = Document::with_tree(
            "fn main() {}".to_string(),
            "rust".to_string(),
            rust_tree("fn main() {}"),
            3,
        );
        doc.apply_edit("fn main() { }".to_string(), &[edit(11, 11, 12)]);
        assert!(
            doc.incremental_seed("rust").is_some(),
            "the grammar that parsed the published tree seeds"
        );
        assert!(
            doc.incremental_seed("python").is_none(),
            "another grammar must not reuse the tree"
        );
    }

    /// A tree published for an older version than the log's newest entries
    /// (a stale-but-consistent publish, the edit's own reparse still to come)
    /// seeds with only the edits after its version — the filter alone, with
    /// no edit in between to prune the log for it.
    #[test]
    fn a_tree_published_behind_the_log_replays_only_the_edits_after_it() {
        let mut doc = Document::with_tree(
            "fn main() {}".to_string(),
            "rust".to_string(),
            rust_tree("fn main() {}"),
            3,
        );
        doc.apply_edit("fn main() { }".to_string(), &[edit(11, 11, 12)]);
        doc.apply_edit("fn main() { x }".to_string(), &[edit(12, 12, 14)]);
        // The reparse of version 1 lands while the document is at version 2.
        assert!(doc.publish_snapshot(&snapshot_at(&doc, "fn main() { }", 1)));
        let seed = seed_tree(&doc).expect("seeded from the version-1 tree");
        assert!(
            seed_matches_text(&seed, "fn main() { x }"),
            "the version-1 edit is consumed; only the version-2 edit is replayed"
        );
    }

    /// A burst that outgrows the edit log gives up on seeding: the log is
    /// cleared and the floor raised to the burst's version, so a tree
    /// published for an earlier version cannot seed (its edits are gone) and
    /// only a tree at or past the floor seeds again.
    #[test]
    fn an_edit_burst_beyond_the_log_cap_gives_up_seeding_until_a_fresh_tree() {
        let mut doc = Document::with_tree(
            "fn main() {}".to_string(),
            "rust".to_string(),
            rust_tree("fn main() {}"),
            3,
        );
        let text_after = |edits: usize| format!("fn main() {{{}}}", " ".repeat(edits));
        for edits in 1..=MAX_SEED_EDITS {
            doc.apply_edit(text_after(edits), &[edit(11, 11, 12)]);
        }
        let seed = seed_tree(&doc).expect("a log at the cap still seeds");
        assert!(seed_matches_text(&seed, &text_after(MAX_SEED_EDITS)));

        doc.apply_edit(text_after(MAX_SEED_EDITS + 1), &[edit(11, 11, 12)]);
        assert!(seed_tree(&doc).is_none(), "one past the cap: no seed");
        let floor = doc.content_version();

        // A tree for the version just before the burst overflowed: below the
        // floor, and the edit that would bring it up to date is gone.
        assert!(doc.publish_snapshot(&snapshot_at(&doc, &text_after(MAX_SEED_EDITS), floor - 1)));
        assert!(
            seed_tree(&doc).is_none(),
            "a tree older than the floor must not seed"
        );
        assert!(doc.publish_snapshot(&snapshot_at(&doc, &text_after(MAX_SEED_EDITS + 1), floor)));
        let seed = seed_tree(&doc).expect("a tree at the floor seeds again");
        assert!(seed_matches_text(&seed, &text_after(MAX_SEED_EDITS + 1)));
        doc.apply_edit(text_after(MAX_SEED_EDITS + 2), &[edit(11, 11, 12)]);
        let seed = seed_tree(&doc).expect("and the log fills again");
        assert!(seed_matches_text(&seed, &text_after(MAX_SEED_EDITS + 2)));
    }

    /// The off-ingress reparse's seed is derived, not stored: the published
    /// tree with every edit since its version replayed onto a clone, so a
    /// burst of coalesced edits yields one correctly edited seed and the
    /// published snapshot itself stays unedited.
    #[test]
    fn incremental_seed_is_the_published_tree_with_the_edits_since_replayed() {
        let mut doc = Document::with_tree(
            "fn main() {}".to_string(),
            "rust".to_string(),
            rust_tree("fn main() {}"),
            3,
        );
        assert!(
            seed_tree(&doc).is_some(),
            "an unedited current tree seeds as is"
        );

        doc.apply_edit("fn main() { }".to_string(), &[edit(11, 11, 12)]);
        let seed = seed_tree(&doc).expect("one edit: seeded");
        assert!(incremental_parse_matches_fresh(&seed, "fn main() { }"));
        assert!(
            doc.tree().is_none(),
            "the edit made the published tree stale"
        );

        doc.apply_edit("fn main() { x }".to_string(), &[edit(12, 12, 14)]);
        let seed = seed_tree(&doc).expect("coalesced edits: still seeded");
        assert!(
            incremental_parse_matches_fresh(&seed, "fn main() { x }"),
            "both edits replayed, in order"
        );
        assert_eq!(
            doc.latest_snapshot_slot()
                .snapshot
                .and_then(|s| s.tree.as_ref().map(|t| t.root_node().end_byte())),
            Some("fn main() {}".len()),
            "the published tree is not edited in place"
        );
    }

    /// A full-text sync (no `InputEdit`s) invalidates seeding until a fresh
    /// tree is published: seeding an unedited tree against wholly replaced
    /// text violates tree-sitter's incremental contract (#348), and the edits
    /// that follow before the reparse cannot be replayed onto it either.
    #[test]
    fn a_full_text_sync_disables_seeding_until_a_fresh_tree_is_published() {
        let mut doc = Document::with_tree(
            "fn main() {}".to_string(),
            "rust".to_string(),
            rust_tree("fn main() {}"),
            3,
        );
        doc.apply_edit("fn other() {}".to_string(), &[]);
        assert!(seed_tree(&doc).is_none(), "full sync: parse from scratch");
        doc.apply_edit("fn other() { }".to_string(), &[edit(12, 12, 13)]);
        assert!(
            seed_tree(&doc).is_none(),
            "an edit after the sync has no tree it can be replayed onto"
        );

        assert!(doc.publish_snapshot(&doc.bare_snapshot(Some(rust_tree("fn other() { }")))));
        assert!(
            seed_tree(&doc).is_some(),
            "a fresh published tree seeds again"
        );
        doc.apply_edit("fn other() { y }".to_string(), &[edit(13, 13, 15)]);
        let seed = seed_tree(&doc).expect("seeded from the fresh tree");
        assert!(incremental_parse_matches_fresh(&seed, "fn other() { y }"));
    }

    /// Edits the published tree already saw are not replayed onto it: a
    /// parse that landed at version N consumed the edits up to N, so only
    /// the edits after N are applied to its clone.
    #[test]
    fn edits_the_published_tree_already_consumed_are_not_replayed() {
        let mut doc = Document::with_tree(
            "fn main() {}".to_string(),
            "rust".to_string(),
            rust_tree("fn main() {}"),
            3,
        );
        doc.apply_edit("fn main() { }".to_string(), &[edit(11, 11, 12)]);
        doc.apply_edit("fn main() { x }".to_string(), &[edit(12, 12, 14)]);
        // The reparse of version 2 lands.
        assert!(doc.publish_snapshot(&doc.bare_snapshot(Some(rust_tree("fn main() { x }")))));
        doc.apply_edit("fn main() { xy }".to_string(), &[edit(13, 13, 14)]);
        let seed = seed_tree(&doc).expect("seeded from the version-2 tree");
        assert!(
            incremental_parse_matches_fresh(&seed, "fn main() { xy }"),
            "only the edit after version 2 is replayed"
        );
        assert_eq!(
            doc.seed_edits.iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            vec![3],
            "the edits the published tree consumed were pruned from the log"
        );
    }

    /// A tree reaches readers only through a published, current snapshot:
    /// the document has no tree of its own.
    #[test]
    fn a_published_current_snapshot_is_the_readers_tree() {
        let text = "fn main() {}";
        let doc = Document::with_language(text.to_string(), "rust".to_string(), 7);
        assert!(doc.tree().is_none(), "unparsed: no tree");
        let tree = rust_tree(text);
        assert!(doc.publish_snapshot(&Arc::new(ParseSnapshot {
            text: doc.text_arc(),
            tree: Some(tree.clone()),
            language: Some("rust".to_string()),
            parsed_version: doc.content_version(),
            incarnation: 7,
            injection_regions: None,
            bridge_regions: None,
            resolved_regions: None,
            layer_trees: std::sync::OnceLock::new(),
        })));
        // `Tree` clones share their subtrees but not the root handle, so a
        // child node's id is the identity that survives the clone.
        let first_child = |t: &Tree| t.root_node().named_child(0).map(|n| n.id());
        assert_eq!(
            doc.tree().as_ref().and_then(first_child),
            first_child(&tree),
            "the published snapshot's tree is the document's tree"
        );
        assert!(doc.snapshot().is_some());
    }

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

        // A parse-result write (language record + published tree) does not bump.
        let tree = parser.parse("fn main() { }", None).unwrap();
        doc.record_language(Some("rust".to_string()));
        assert!(doc.publish_snapshot(&doc.bare_snapshot(Some(tree))));
        assert_eq!(
            doc.content_version(),
            1,
            "a parse result is not an input mutation"
        );
        assert!(doc.tree().is_some());

        // An incremental edit bumps.
        doc.apply_edit("fn main() {  }".to_string(), &[]);
        assert_eq!(doc.content_version(), 2);
    }

    #[test]
    fn input_mutation_cancels_only_obsolete_version_work() {
        let mut doc = Document::new("fn main() {}".to_string(), 1);
        let obsolete = doc.version_cancel.clone();
        assert!(!obsolete.is_cancelled());

        doc.apply_edit("fn main() { }".to_string(), &[]);

        assert!(
            obsolete.is_cancelled(),
            "work derived from the pre-edit version must be cancelled"
        );
        assert!(
            !doc.version_cancel.is_cancelled(),
            "the new version must receive a live token"
        );
    }

    #[test]
    fn test_update_text() {
        let mut doc = Document::new("initial".to_string(), 0);
        doc.update_text("updated".to_string());
        assert_eq!(doc.text(), "updated");
        assert!(doc.tree().is_none());
    }

    /// A non-empty edit logs its `InputEdit` for the seed and stales the
    /// reader-visible tree (readers must not see a pre-reparse tree).
    #[test]
    fn apply_edit_logs_the_edit_for_the_seed_and_stales_the_tree() {
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
        doc.apply_edit("fn main() { }".to_string(), &[edit]);

        assert!(
            doc.tree().is_none(),
            "the published tree must read as stale"
        );
        assert!(
            seed_tree(&doc).is_some_and(|seed| seed_matches_text(&seed, "fn main() { }")),
            "an incremental seed with the edit replayed must be derivable"
        );
        assert_eq!(doc.text(), "fn main() { }");
    }

    /// A full-text sync (no `InputEdit`s) must drop the seed: seeding an unedited
    /// tree against wholly-replaced text is the tree-sitter contract violation that
    /// caused the #348 heap corruption.
    #[test]
    fn apply_edit_forbids_seeding_on_full_text_sync() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree, 0);

        // Full-text sync carries no InputEdits.
        doc.apply_edit("totally different content".to_string(), &[]);

        assert!(doc.tree().is_none());
        assert!(
            seed_tree(&doc).is_none(),
            "full-text sync must not leave a stale seed (#348)"
        );
    }

    /// Coalesced edits accumulate in the log: after a first edit stales the
    /// published tree, a second edit's `InputEdit` is replayed after the first.
    #[test]
    fn apply_edit_coalesces_across_edits() {
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
        doc.apply_edit("fn main() { }".to_string(), &[edit1]);
        assert!(doc.tree().is_none());

        // Second edit lands while the published tree is still stale: it must
        // join the log, not silently drop incrementality.
        let edit2 = InputEdit {
            start_byte: 12,
            old_end_byte: 12,
            new_end_byte: 13,
            start_position: tree_sitter::Point::new(0, 12),
            old_end_position: tree_sitter::Point::new(0, 12),
            new_end_position: tree_sitter::Point::new(0, 13),
        };
        doc.apply_edit("fn main() {  }".to_string(), &[edit2]);

        assert!(doc.tree().is_none());
        assert!(
            seed_tree(&doc).is_some_and(|seed| seed_matches_text(&seed, "fn main() {  }")),
            "coalesced edits must still yield a seed with both replayed"
        );
        assert_eq!(doc.text(), "fn main() {  }");
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
