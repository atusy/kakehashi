use crate::document::Document;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::one::Ref;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use tree_sitter::{InputEdit, Tree};
use url::Url;

// The central store for all document-related information.
pub struct DocumentStore {
    documents: DashMap<Url, Document>,
    parse_states: DashMap<Url, watch::Sender<ParseState>>,
    /// Per-document serialization lock for `didChange` application.
    ///
    /// `didChange` handlers read the current text, apply incremental ranges, and
    /// only persist the result after an async reparse. Without serialization,
    /// concurrently-dispatched handlers for the same document all read the same
    /// stale base text, so a later edit's range (authored against a newer state)
    /// is applied to an older one — corrupting the text and, before clamping,
    /// panicking in `replace_range`. Holding this per-URI async lock across a
    /// document's edit critical section gives each edit mutual exclusion and lets
    /// it see the previous one's persisted result. Handlers take the lock as their
    /// first `.await`, so acquisition follows first-poll order — a practical
    /// mitigation, not a hard JSON-RPC wire-order guarantee (tracked in
    /// <https://github.com/atusy/kakehashi/issues/342>). Different documents keep
    /// their own locks and run concurrently.
    edit_locks: DashMap<Url, Arc<Mutex<()>>>,
    /// Source of the process-wide-unique **open incarnation** stamped onto every
    /// [`Document`] this store constructs (see [`Document::incarnation`] and
    /// [`next_incarnation`](Self::next_incarnation)). Starts at 1, so a document
    /// built outside the store (incarnation `0`) is always distinguishable from
    /// one this store owns. The incarnation lives *on the document* rather than
    /// in a side map, so a tree-write CAS or a watermark advance can check it
    /// atomically with the document state under the same shard lock; a consumer
    /// that captured a snapshot detects a close-then-reopen by comparing the
    /// snapshot's incarnation against the URI's current one. See
    /// `Kakehashi::store_lineage` (captures-protocol §"Delta semantics").
    open_counter: std::sync::atomic::AtomicU64,
    /// Per-document **parse watermark**: the highest ingress writer ticket whose
    /// parse has reached a terminal outcome (a tree, or parsed-to-nothing). It is
    /// monotonic per document and published by the parse path on resolution.
    ///
    /// This is deliberately a signal **distinct** from the `IngressOrderGate`
    /// completion channel. Today the parse resolves *inline*, before its writer
    /// ticket completes, so the watermark and ticket-completion move in lockstep;
    /// a reader gated behind the ticket already observes a fresh tree. The
    /// per-document parse scheduler (see `per-document-parse-scheduler` ADR) will run the
    /// parse *off* the ingress ticket, at which point ticket-completion no longer
    /// implies a fresh tree and this watermark — not the completion channel — is
    /// what tells a virt/native reader the store reflects its tail edit. Keyed on
    /// the ticket (the intra-lifetime wire order). The channel value carries the
    /// lifetime's [`incarnation`](Watermark) too, so the off-ingress advance
    /// ([`advance_watermark_for_incarnation`](Self::advance_watermark_for_incarnation))
    /// gates on it atomically with the ticket write — a prior lifetime's parse
    /// cannot advance a reopened document's re-seeded watermark.
    watermarks: DashMap<Url, watch::Sender<Watermark>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ParseState {
    generation: u64,
    in_progress: bool,
    has_tree: bool,
}

/// The value carried by a document's parse-watermark channel: the open
/// `incarnation` the channel belongs to, and the highest `ticket` whose parse
/// has resolved for that lifetime. Storing the incarnation **in the channel**
/// lets the off-ingress advance compare it against the parse's captured
/// incarnation *atomically* with the ticket write (inside `send_if_modified`,
/// under the watch's own lock), so a straggler parse from a prior lifetime can
/// never advance a reopened document's freshly re-seeded watermark — without a
/// second-map lookup or any documents↔watermarks lock ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Watermark {
    incarnation: u64,
    ticket: u64,
}

pub struct DocumentHandle<'a> {
    inner: Ref<'a, Url, Document>,
}

impl<'a> DocumentHandle<'a> {
    fn new(inner: Ref<'a, Url, Document>) -> Self {
        Self { inner }
    }
}

impl<'a> Deref for DocumentHandle<'a> {
    type Target = Document;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: DashMap::new(),
            parse_states: DashMap::new(),
            edit_locks: DashMap::new(),
            open_counter: std::sync::atomic::AtomicU64::new(1),
            watermarks: DashMap::new(),
        }
    }
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update tree availability without affecting parse-in-progress tracking.
    /// The `in_progress` state is owned exclusively by mark_parse_started/mark_parse_finished.
    fn update_tree_availability(&self, uri: &Url, has_tree: bool) {
        let sender = self.parse_sender(uri);
        let mut state = *sender.borrow();
        state.has_tree = has_tree;
        sender.send_replace(state);
    }

    /// Set `has_tree = true` only if a parse-state entry already exists.
    ///
    /// Unlike [`update_tree_availability`] this does **not** create the entry
    /// (`get`, not `parse_sender`'s get-or-insert). For a live document the entry
    /// always exists (created on insert), so this is equivalent; but for the
    /// non-inserting reader CAS it avoids resurrecting a parse-state for a URI that
    /// a concurrent `didClose` removed — `remove` drops `parse_states` first, so a
    /// get-or-insert here would recreate a ghost `has_tree = true` for a closed
    /// document. Holding the `Ref` serializes against that `remove`.
    fn mark_tree_available_if_tracked(&self, uri: &Url) {
        if let Some(sender) = self.parse_states.get(uri) {
            let mut state = *sender.borrow();
            state.has_tree = true;
            sender.send_replace(state);
        }
    }

    fn parse_sender(&self, uri: &Url) -> watch::Sender<ParseState> {
        match self.parse_states.entry(uri.clone()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let (sender, _receiver) = watch::channel(ParseState::default());
                entry.insert(sender.clone());
                sender
            }
        }
    }

    pub fn mark_parse_started(&self, uri: &Url) -> u64 {
        let sender = self.parse_sender(uri);
        let mut state = *sender.borrow();
        state.generation = state.generation.saturating_add(1);
        state.in_progress = true;
        state.has_tree = false;
        sender.send_replace(state);
        state.generation
    }

    pub fn mark_parse_finished(&self, uri: &Url, generation: u64, has_tree: bool) {
        // Non-inserting (`get`, not `parse_sender`'s get-or-insert), mirroring
        // [`mark_tree_available_if_tracked`]: once the open parse runs off the ingress
        // ticket (#6) a `didClose` can land between `mark_parse_started` and here, and
        // `remove` drops `parse_states` first — a get-or-insert would recreate an
        // orphan default entry for the closed URI. The generation guard already
        // prevents state corruption; this also stops the resurrection. Holding the
        // `Ref` serializes against that `remove`.
        let Some(sender) = self.parse_states.get(uri) else {
            return;
        };
        let mut state = *sender.borrow();
        if state.generation != generation {
            return;
        }
        state.in_progress = false;
        state.has_tree = has_tree;
        sender.send_replace(state);
    }

    pub async fn wait_for_parse_completion(&self, uri: &Url, timeout: std::time::Duration) {
        let mut receiver = self.parse_sender(uri).subscribe();

        let wait_future = async {
            loop {
                let state = *receiver.borrow();

                // Already have a tree - done waiting
                if state.has_tree {
                    return;
                }

                // No tree yet - wait for state change
                // (either parse starts, or parse finishes with a tree)
                if receiver.changed().await.is_err() {
                    return; // Channel closed
                }
            }
        };

        let _ = tokio::time::timeout(timeout, wait_future).await;
    }

    // Lock safety: Single insert() call - no read lock held before or during write
    pub fn insert(&self, uri: Url, text: String, language_id: Option<String>, tree: Option<Tree>) {
        let has_tree = tree.is_some();
        // didOpen registers a fresh lifetime → a fresh incarnation, so an
        // in-flight parse from a prior open of this URI can't publish against it.
        let incarnation = self.next_incarnation();
        let document = match (language_id, tree) {
            (Some(lang), Some(t)) => Document::with_tree(text, lang, t, incarnation),
            (Some(lang), None) => Document::with_language(text, lang, incarnation),
            _ => Document::new(text, incarnation),
        };

        // The parse-state and watermark maps are independent of `documents`, so seed
        // them with the borrowed `&uri` first and let `documents.insert` consume the
        // owned `uri` last — avoiding a `Url` clone on every didOpen.
        self.update_tree_availability(&uri, has_tree);
        // Seed the watermark for this lifetime's incarnation so its lifetime tracks
        // the document's; the advance paths are non-inserting and rely on this
        // entry being present. A reopen replaces any leftover prior-lifetime channel.
        self.ensure_watermark_entry(&uri, incarnation);
        self.documents.insert(uri, document);
    }

    // Lock safety: Returns DocumentHandle wrapping Ref - caller holds read lock until drop
    // Callers must not call write methods while holding the returned handle
    pub fn get(&self, uri: &Url) -> Option<DocumentHandle<'_>> {
        self.documents.get(uri).map(DocumentHandle::new)
    }

    // Lock safety: Uses entry() API for atomic check-and-update/insert operations,
    // eliminating race conditions between get_mut and insert.
    pub fn update_document(&self, uri: Url, text: String, new_tree: Option<Tree>) {
        // Use entry API for atomic operations to prevent race conditions
        // between checking if document exists and inserting/updating.
        // Hot path (the document already exists): `get_mut` borrows the key, so no
        // `Url` clone. Only a miss falls back to `entry` (owned key), which re-checks
        // for a document a concurrent insert added between the `get_mut` and here —
        // matching `apply_edit_clearing_tree`.
        let (has_tree, incarnation) = if let Some(mut doc) = self.documents.get_mut(&uri) {
            // Update in place, preserving the incarnation (an edit is the same lifetime).
            let has_tree = if let Some(tree) = new_tree {
                doc.update_tree_and_text(tree, text);
                true
            } else {
                // No new tree provided - clear existing tree and update text only.
                // This path is used when text changes without re-parsing (rare edge case).
                doc.update_text(text);
                false
            };
            (has_tree, doc.incarnation())
        } else {
            match self.documents.entry(uri.clone()) {
                Entry::Occupied(mut entry) => {
                    let doc = entry.get_mut();
                    let has_tree = if let Some(tree) = new_tree {
                        doc.update_tree_and_text(tree, text);
                        true
                    } else {
                        doc.update_text(text);
                        false
                    };
                    (has_tree, doc.incarnation())
                }
                Entry::Vacant(entry) => {
                    // Document doesn't exist - create new one (a fresh lifetime →
                    // fresh incarnation). `next_incarnation` touches only the atomic
                    // counter, not `documents`, so drawing it while holding this
                    // entry's shard write lock cannot deadlock.
                    let incarnation = self.next_incarnation();
                    let has_tree = if let Some(tree) = new_tree {
                        entry.insert(Document::with_tree(
                            text,
                            "unknown".to_string(),
                            tree,
                            incarnation,
                        ));
                        true
                    } else {
                        entry.insert(Document::new(text, incarnation));
                        false
                    };
                    (has_tree, incarnation)
                }
            }
        };
        self.update_tree_availability(&uri, has_tree);
        // Keep the "live document ⟺ watermark entry" invariant: `update_document`
        // can insert on its `Vacant` branch, and a present document with no
        // watermark entry would make `wait_for_epoch` treat it as unregistered.
        // Idempotent on the (common) update-in-place path (same incarnation).
        self.ensure_watermark_entry(&uri, incarnation);
    }

    /// Apply a `didChange`'s new text and stash an **incremental parse seed**,
    /// clearing the reader-visible tree — the per-document-parse-scheduler flip's edit
    /// path.
    ///
    /// Replaces the prior `update_document(uri, text, None)`: it still clears the
    /// reader-visible tree (so a reader never sees a tree predating this edit) and
    /// keeps the same side effects (tree-availability → false, watermark entry
    /// ensured), but additionally stashes the pre-edit tree — with `edits` applied —
    /// as the seed for the off-ingress `reparse_latest`'s incremental parse. With no
    /// `edits` (full-text sync) no seed is kept (#348). Coalesced edits accumulate
    /// onto the seed (see [`Document::apply_edit_and_seed`]).
    ///
    /// Called under the per-URI edit lock with the document known to exist; the
    /// `Vacant` branch (a reordered notification for an unopened URI) inserts a
    /// tree-less document to mirror the prior `update_document` behavior.
    pub(crate) fn apply_edit_clearing_tree(&self, uri: &Url, text: String, edits: &[InputEdit]) {
        // Hot path (a live document being edited): `get_mut` borrows the key, so no
        // `Url` clone per keystroke. Only a miss falls back to `entry` (owned key),
        // which also re-checks for a document a concurrent open inserted between the
        // `get_mut` and here — mirroring `ParseScheduler::schedule` and the atomicity
        // of the prior `update_document`. The `RefMut` / entry is dropped before the
        // parse-state and watermark updates below (separate maps), keeping the
        // `documents` shard lock held no longer than `update_document` did.
        let incarnation = if let Some(mut doc) = self.documents.get_mut(uri) {
            doc.apply_edit_and_seed(text, edits);
            doc.incarnation()
        } else {
            match self.documents.entry(uri.clone()) {
                Entry::Occupied(mut entry) => {
                    let doc = entry.get_mut();
                    doc.apply_edit_and_seed(text, edits);
                    doc.incarnation()
                }
                Entry::Vacant(entry) => {
                    // Reordered edit registering an unopened URI: a fresh
                    // lifetime → fresh incarnation (mirrors `update_document`).
                    let incarnation = self.next_incarnation();
                    entry.insert(Document::new(text, incarnation));
                    incarnation
                }
            }
        };
        self.update_tree_availability(uri, false);
        // Keep the "live document ⟺ watermark entry" invariant (see `update_document`).
        self.ensure_watermark_entry(uri, incarnation);
    }

    /// Store `new_tree` for an **existing** document, but only if its current text
    /// still equals `expected_text`. Returns `true` iff the tree was stored.
    ///
    /// This is the safe persistence path for an **on-demand reader parse** (e.g.
    /// `kakehashi/node`'s parse fallback). Unlike [`update_document`] it is
    /// **non-inserting**: a `Vacant` entry — the document was closed while the
    /// parse ran — is left untouched, so a parse completing after a `didClose`
    /// cannot **resurrect** the document. Folding the text-equality check into the
    /// same atomic `get_mut` lock also closes the check-then-write window against a
    /// concurrent `didChange`: a tree parsed from now-stale text is dropped rather
    /// than associated with the newer text. The availability update is likewise
    /// non-inserting (see `mark_tree_available_if_tracked`), so the parse-state
    /// isn't resurrected either.
    pub(crate) fn update_tree_if_text_unchanged(
        &self,
        uri: &Url,
        expected_text: &str,
        new_tree: Tree,
    ) -> bool {
        // `get_mut` (non-inserting, borrowed key) rather than `entry(uri.clone())`:
        // a missing document — a concurrent didClose removed it — yields `None` and
        // is left untouched, so the parse can't resurrect it, and we avoid cloning
        // the `Url`. The `RefMut` holds the shard write lock for the whole arm, so
        // the text check and the tree write are atomic against didChange/didClose.
        // Text already equals `expected_text`, so only the tree is attached — no
        // text re-clone.
        let stored = if let Some(mut doc) = self.documents.get_mut(uri) {
            if doc.text() == expected_text {
                doc.set_tree(new_tree);
                true
            } else {
                false
            }
        } else {
            false
        };
        if stored {
            // Non-inserting too: don't recreate a parse-state for a URI a
            // concurrent didClose may have just dropped.
            self.mark_tree_available_if_tracked(uri);
        }
        stored
    }

    /// Like [`update_tree_if_text_unchanged`], but additionally requires the
    /// document's `language_id` to still equal `expected_language_id` **and** its
    /// open incarnation to still equal `expected_incarnation`.
    ///
    /// For the off-ingress edit reparse (`reparse_latest`), whose tree was parsed
    /// from the text, grammar, and lifetime observed at read time. Three axes,
    /// checked atomically under the same `get_mut` shard lock as the tree write:
    /// - **text** rejects a within-lifetime stale parse — a `didChange` landed
    ///   while parsing, so the tree is of now-superseded text (the scheduler's
    ///   `dirty` loop reparses the newer text);
    /// - **language** rejects a close + reopen with identical text but a different
    ///   `language_id` — a tree parsed by the *old* grammar must not attach to the
    ///   relabelled document;
    /// - **incarnation** rejects a close + reopen even with identical text *and*
    ///   language — the tree belongs to the prior lifetime, and attaching it to
    ///   the reopened document (then advancing its watermark on the old ticket)
    ///   would let a new-lifetime reader observe a parse it never requested. This
    ///   is the [`(incarnation, ticket)`](Document::incarnation) epoch's
    ///   incarnation half; the text and language checks remain because they guard
    ///   the orthogonal within-lifetime races above.
    pub(crate) fn update_tree_if_text_and_language_unchanged(
        &self,
        uri: &Url,
        expected_text: &str,
        expected_language_id: Option<&str>,
        expected_incarnation: u64,
        new_tree: Tree,
    ) -> Option<u64> {
        // On success returns the `parse_epoch` this write stamped — the identity
        // the off-ingress injection-discovery write-back rechecks so it never
        // attaches discovery built on this tree onto a later one.
        let stamped = if let Some(mut doc) = self.documents.get_mut(uri) {
            if doc.incarnation() == expected_incarnation
                && doc.text() == expected_text
                && doc.language_id() == expected_language_id
            {
                doc.set_tree(new_tree);
                Some(doc.parse_epoch())
            } else {
                None
            }
        } else {
            None
        };
        if stamped.is_some() {
            self.mark_tree_available_if_tracked(uri);
        }
        stamped
    }

    /// Like [`update_tree_if_text_and_language_unchanged`], but additionally stores
    /// the tree only if the document currently has **no** tree. For the off-ingress
    /// install reparse (`reparse_installed_document`), whose "don't clobber a
    /// concurrent parse" guard is a `tree.is_some()` pre-check: a parse attaching a
    /// tree for the same text *between* that pre-check and this write would otherwise
    /// be overwritten. Folding the `tree.is_none()` check into the same `get_mut`
    /// shard lock as the text comparison makes the guard atomic.
    ///
    /// Carries the same `language` and `incarnation` axes as
    /// [`update_tree_if_text_and_language_unchanged`], for the same reason: the
    /// install reparse is off-ingress, so a `didClose` + reopen (possibly relabelling
    /// the language) can race it. Without these checks the freshly-installed grammar's
    /// tree could attach to a reopened — even relabelled — document.
    pub(crate) fn attach_tree_if_absent(
        &self,
        uri: &Url,
        expected_text: &str,
        expected_language_id: Option<&str>,
        expected_incarnation: u64,
        new_tree: Tree,
    ) -> Option<u64> {
        // On success returns the stamped `parse_epoch` (see
        // [`update_tree_if_text_and_language_unchanged`]).
        let stamped = if let Some(mut doc) = self.documents.get_mut(uri) {
            if doc.tree().is_none()
                && doc.incarnation() == expected_incarnation
                && doc.text() == expected_text
                && doc.language_id() == expected_language_id
            {
                doc.set_tree(new_tree);
                Some(doc.parse_epoch())
            } else {
                None
            }
        } else {
            None
        };
        if stamped.is_some() {
            self.mark_tree_available_if_tracked(uri);
        }
        stamped
    }

    /// Record the didOpen parse's detected `language` + optional `tree` on the
    /// **existing** document, preserving its already-stored text, but only if the
    /// text and incarnation observed when the parse read them are unchanged.
    /// Returns `true` iff it was stored.
    ///
    /// For the **off-ingress** didOpen parse ([`parse_document`]). Unlike
    /// [`update_tree_if_text_and_language_unchanged`] it **sets** the language rather
    /// than checking it: the open parse refines `language_for_path`'s initial guess
    /// with content detection (`detect_language`), so it is the writer of the
    /// authoritative language — there is no language to match against. The two axes
    /// it does check, atomically under the same `get_mut` shard lock as the write:
    /// - **text** rejects a within-lifetime stale parse — a `didChange` landed while
    ///   parsing, so this tree (and its language, possibly shebang-derived) is of
    ///   now-superseded text; the scheduler's reparse covers the newer text. The
    ///   check is `Arc::ptr_eq` on the `Arc<str>` the parse captured, **O(1)** — any
    ///   text mutation (`didChange`) or reopen swaps in a *fresh* allocation, so
    ///   pointer identity is exactly "same text this parse read" without an O(n)
    ///   byte compare under the shard write lock (this is the large-document open
    ///   path). A spurious mismatch could only drop a still-valid tree to a harmless
    ///   reparse, never attach a stale one;
    /// - **incarnation** rejects a close + reopen — the result belongs to the prior
    ///   lifetime and must neither attach its tree nor clobber the reopened
    ///   document's freshly-inserted language.
    ///
    /// **Non-inserting** (`get_mut`): a document a concurrent `didClose` removed is
    /// left gone, not resurrected — the resurrection-safety the open parse needs once
    /// it runs off the ingress ticket. Availability is marked only when a tree
    /// landed (the no-tree paths rely on the caller's `mark_parse_finished(false)`).
    pub(crate) fn set_parse_result_if_text_and_incarnation_unchanged(
        &self,
        uri: &Url,
        expected_text: &Arc<str>,
        expected_incarnation: u64,
        language: Option<&str>,
        tree: Option<Tree>,
    ) -> Option<u64> {
        let has_tree = tree.is_some();
        // `language` is borrowed: the `String` allocation is deferred to inside the
        // successful CAS branch (`language.map(String::from)`), so a CAS that rejects
        // (a `didChange`/reopen raced this off-ingress parse) allocates nothing.
        // On success returns the stamped `parse_epoch` (see
        // [`update_tree_if_text_and_language_unchanged`]).
        let stamped = if let Some(mut doc) = self.documents.get_mut(uri) {
            if doc.incarnation() == expected_incarnation
                && Arc::ptr_eq(&doc.text_arc(), expected_text)
            {
                doc.set_parse_result(language.map(String::from), tree);
                Some(doc.parse_epoch())
            } else {
                None
            }
        } else {
            None
        };
        if stamped.is_some() && has_tree {
            self.mark_tree_available_if_tracked(uri);
        }
        stamped
    }

    /// Return the per-document `didChange` serialization lock, creating it on
    /// first use. Callers hold the guard across the document's edit critical
    /// section so concurrent edits to the same document apply in order. See the
    /// `edit_locks` field docs for why this is required.
    pub(crate) fn edit_lock(&self, uri: &Url) -> Arc<Mutex<()>> {
        self.edit_locks
            .entry(uri.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Drop the per-URI edit lock entry without touching the document itself.
    ///
    /// `edit_lock` get-or-inserts, so a handler that takes the lock and then
    /// finds the document missing (a `didChange`/semantic request for a never-
    /// opened or already-closed URI, e.g. a reordered notification) would leave a
    /// lock entry behind forever. Such handlers call this on their miss path to
    /// keep the map bounded. Safe to call while holding the lock's `Arc` guard —
    /// the guard keeps the mutex alive; only the map entry is removed.
    pub(crate) fn remove_edit_lock(&self, uri: &Url) {
        self.edit_locks.remove(uri);
    }

    /// Ensure a watermark channel exists for `uri` carrying the current lifetime's
    /// `incarnation`, created at ticket 0. Called when a document is registered or
    /// edited so the watermark's lifetime tracks the document's — it exists exactly
    /// while the document is open and is dropped by [`remove`](Self::remove) on close.
    ///
    /// Idempotent **within a lifetime**: an existing channel whose incarnation
    /// already matches is left untouched, so an edit never resets the ticket. But a
    /// channel left over from a *prior* lifetime (a different incarnation) is
    /// **replaced** with a fresh one — this is what guarantees the channel's
    /// incarnation always equals the document's, so a reopen starts at ticket 0 for
    /// its own incarnation even if the prior channel somehow outlived its `remove`.
    /// Replacing drops the old sender, waking any reader still parked on the prior
    /// lifetime's channel (it falls back, exactly as on a close).
    fn ensure_watermark_entry(&self, uri: &Url, incarnation: u64) {
        // Fast path: a borrowed `get` (no `Url` clone) covers the common case — an
        // edit whose channel is already at this incarnation needs nothing. The
        // `Ref` is dropped at the end of this `if` before any write on the map.
        if self
            .watermarks
            .get(uri)
            .is_some_and(|sender| sender.borrow().incarnation == incarnation)
        {
            return;
        }
        // Slow path: take the entry (shard write lock) and re-check under it, so two
        // concurrent registrations for the same live incarnation can't both insert
        // and reset a live ticket to 0 (the probe-then-insert race). A matching
        // channel another racer just seeded is left untouched; only a missing or
        // prior-lifetime channel is (re)seeded at ticket 0.
        match self.watermarks.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                let same_incarnation = entry.get().borrow().incarnation == incarnation;
                if !same_incarnation {
                    entry.insert(
                        watch::channel(Watermark {
                            incarnation,
                            ticket: 0,
                        })
                        .0,
                    );
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(
                    watch::channel(Watermark {
                        incarnation,
                        ticket: 0,
                    })
                    .0,
                );
            }
        }
    }

    /// Publish that the parse covering ingress writer `ticket` for `uri` has
    /// reached a terminal outcome. The watermark only ever advances — a later
    /// resolution for an earlier ticket (a parse superseded then completed out of
    /// order) cannot regress it — so a reader's `>= target` wait is stable.
    ///
    /// **Non-inserting**: it advances an existing entry but never creates one. The
    /// entry is seeded by [`insert`](Self::insert) when the document is registered
    /// and dropped on close, so a straggler parse that resolves *after* a
    /// `didClose` (its epoch now stale) finds no entry and no-ops, rather than
    /// resurrecting a ghost watermark for a closed URI. Every live mutation that
    /// schedules a parse has registered the document first, so the entry is always
    /// present when a legitimate publish runs.
    pub(crate) fn advance_watermark(&self, uri: &Url, ticket: u64) {
        // Clone the sender out of the `Ref` (so the DashMap guard is dropped
        // before `send_if_modified`); a missing entry means a closed document.
        let Some(sender) = self.watermarks.get(uri).map(|sender| sender.clone()) else {
            return;
        };
        // Bumps only the ticket; the channel's incarnation is fixed for the
        // lifetime. For the on-ingress open parse, whose writer ticket gates any
        // reopen, so it always runs against its own lifetime's channel.
        sender.send_if_modified(|watermark| {
            if ticket > watermark.ticket {
                watermark.ticket = ticket;
                true
            } else {
                false
            }
        });
    }

    /// Advance the watermark to `ticket`, but only if the channel still belongs to
    /// `expected_incarnation` — the lifetime that issued the ticket.
    ///
    /// For the off-ingress parse: its ticket is the *intra-lifetime* wire order,
    /// and the watermark is removed on close and re-seeded at 0 on reopen
    /// (per-lifetime, not globally monotonic). So a parse from a prior lifetime
    /// completing after a close + reopen must **not** advance the reopened
    /// document's fresh watermark — its (smaller) ticket could exceed a
    /// new-lifetime reader's target and release that reader before the reopen's
    /// own parse has run (a stale/empty read).
    ///
    /// The incarnation lives **in the channel value**, so the compare and the
    /// ticket write happen together inside `send_if_modified`, under the watch's
    /// own lock, against one consistent value — there is no check-then-write seam
    /// for a concurrent reopen to slip through (the off-ingress advance holds no
    /// `edit_lock`, so a `didClose`+`didOpen` *does* run concurrently with it).
    /// Whichever channel this resolves against, the compare is against *that*
    /// channel's incarnation: the reopen's fresh channel (incarnation mismatch →
    /// skip) or the prior lifetime's now-detached channel (a harmless bump of a
    /// channel whose readers the close already woke). A closed (absent) document
    /// is a no-op.
    pub(crate) fn advance_watermark_for_incarnation(
        &self,
        uri: &Url,
        ticket: u64,
        expected_incarnation: u64,
    ) {
        let Some(sender) = self.watermarks.get(uri).map(|sender| sender.clone()) else {
            return;
        };
        sender.send_if_modified(|watermark| {
            if watermark.incarnation == expected_incarnation && ticket > watermark.ticket {
                watermark.ticket = ticket;
                true
            } else {
                false
            }
        });
    }

    /// Wait until the parse watermark for `uri` reaches `target` — the tail
    /// ingress ticket a virt/native reader must observe — bounded by `timeout`.
    ///
    /// Returns early (proceed into the empty/`null` reader fallback) when the
    /// watermark already covers `target`, when there is no watermark entry — the
    /// document is not registered (never opened, or a `didClose` removed it), as
    /// `insert` seeds the entry at 0 for every live document, so there is nothing
    /// to wait for — or when the entry is dropped while waiting (a concurrent
    /// `didClose`). Non-inserting on the missing-entry path so it never resurrects
    /// a watermark for a closed URI.
    pub(crate) async fn wait_for_epoch(
        &self,
        uri: &Url,
        target: u64,
        timeout: std::time::Duration,
    ) {
        // Subscribe under the shard read lock, then drop the `Ref` *before*
        // awaiting — never hold a DashMap guard across `.await`.
        let mut receiver = {
            let Some(sender) = self.watermarks.get(uri) else {
                return;
            };
            sender.subscribe()
        };

        let wait_future = async {
            // Err => the sender was dropped (entry removed after a close): every
            // parse that will ever run for this lifetime has, so proceed. A reader
            // waits on its own lifetime's channel ticket; a reopen replaces the
            // channel (dropping this sender → Err → fallback), so the target (an
            // intra-lifetime ticket) is only ever compared within one lifetime.
            let _ = receiver
                .wait_for(|watermark| watermark.ticket >= target)
                .await;
        };

        let _ = tokio::time::timeout(timeout, wait_future).await;
    }

    // Lock safety: Single remove() call - no read lock held before or during write
    pub(crate) fn remove(&self, uri: &Url) -> Option<Document> {
        self.parse_states.remove(uri);
        self.edit_locks.remove(uri);
        // Dropping the watermark sender wakes any reader still blocked on
        // `wait_for_epoch` for this document, so a reader racing the close
        // proceeds into the empty fallback instead of stalling to the timeout.
        self.watermarks.remove(uri);
        self.documents.remove(uri).map(|(_, doc)| doc)
    }

    /// Draw the next process-wide-unique **open incarnation** (see
    /// [`Document::incarnation`]). Monotonic and never reused, so two documents
    /// — including a close-then-reopen of the same URI — always get distinct
    /// values, which is what lets a consumer detect a reopen by comparing a
    /// captured incarnation against the document's current one. Starts at 1, so
    /// `0` is reserved for a document built outside any store.
    pub(crate) fn next_incarnation(&self) -> u64 {
        self.open_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn insert_stamps_a_fresh_nonzero_incarnation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///gen.rs").unwrap();
        store.insert(uri.clone(), "x".to_string(), Some("rust".to_string()), None);
        let incarnation = store.get(&uri).unwrap().incarnation();
        assert_ne!(
            incarnation, 0,
            "a store-owned document must carry a nonzero incarnation (0 is the \
             outside-the-store sentinel)"
        );
    }

    #[test]
    fn reopen_draws_a_fresh_incarnation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///gen.rs").unwrap();
        store.insert(uri.clone(), "x".to_string(), Some("rust".to_string()), None);
        let first = store.get(&uri).unwrap().incarnation();

        store.remove(&uri);
        store.insert(uri.clone(), "x".to_string(), Some("rust".to_string()), None);
        let second = store.get(&uri).unwrap().incarnation();

        assert_ne!(
            first, second,
            "a close-then-reopen must draw a fresh incarnation so a consumer can \
             detect the reopen"
        );
    }

    #[test]
    fn edit_preserves_the_incarnation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///gen.rs").unwrap();
        store.insert(uri.clone(), "x".to_string(), Some("rust".to_string()), None);
        let before = store.get(&uri).unwrap().incarnation();

        // An edit is the same lifetime — the incarnation must not move.
        store.apply_edit_clearing_tree(&uri, "xy".to_string(), &[]);
        let after = store.get(&uri).unwrap().incarnation();

        assert_eq!(
            before, after,
            "an edit stays within one lifetime and must preserve the incarnation"
        );
    }

    fn parse_rust(text: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    #[test]
    fn update_tree_if_text_unchanged_does_not_resurrect_closed_document() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///closed.rs").unwrap();
        // No document inserted: models a didClose that removed the entry while an
        // on-demand reader parse was still running.
        let stored =
            store.update_tree_if_text_unchanged(&uri, "fn main() {}", parse_rust("fn main() {}"));
        assert!(
            !stored,
            "must not store a tree for a vacant (closed) document"
        );
        assert!(
            store.get(&uri).is_none(),
            "the closed document must not be resurrected by the reader parse"
        );
    }

    #[test]
    fn update_tree_if_text_unchanged_stores_when_text_matches() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///live.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let stored =
            store.update_tree_if_text_unchanged(&uri, "fn main() {}", parse_rust("fn main() {}"));
        assert!(stored, "a live document with unchanged text gets the tree");
        assert!(
            store.get(&uri).unwrap().tree().is_some(),
            "the parsed tree must be visible after the CAS write"
        );
    }

    #[test]
    fn update_tree_if_text_unchanged_drops_stale_text_parse() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///edited.rs").unwrap();
        // The document moved on (a didChange landed) while the parse for the old
        // text was in flight.
        store.insert(
            uri.clone(),
            "fn newer() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let stored =
            store.update_tree_if_text_unchanged(&uri, "fn main() {}", parse_rust("fn main() {}"));
        assert!(!stored, "a tree parsed from now-stale text must be dropped");
        assert!(
            store.get(&uri).unwrap().tree().is_none(),
            "the stale tree must not overwrite the current text's (still-absent) tree"
        );
    }

    #[test]
    fn attach_tree_if_absent_stores_only_when_no_tree_present() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///absent.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = store.get(&uri).unwrap().incarnation();

        // No tree yet → stores.
        assert!(
            store
                .attach_tree_if_absent(
                    &uri,
                    "fn main() {}",
                    Some("rust"),
                    incarnation,
                    parse_rust("fn main() {}")
                )
                .is_some()
        );
        let first = store.get(&uri).unwrap().tree().unwrap().root_node().id();

        // A tree is now present → a second attach must NOT clobber it (the guard
        // is atomic with the write, unlike a separate tree.is_some() pre-check).
        assert!(
            store
                .attach_tree_if_absent(
                    &uri,
                    "fn main() {}",
                    Some("rust"),
                    incarnation,
                    parse_rust("fn main() {}")
                )
                .is_none()
        );
        assert_eq!(
            store.get(&uri).unwrap().tree().unwrap().root_node().id(),
            first,
            "an existing tree must be preserved, not overwritten"
        );
    }

    #[test]
    fn attach_tree_if_absent_rejects_a_reopen_with_a_fresh_incarnation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///reinstalled.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // An install reparse read this lifetime's incarnation, then the document was
        // closed and reopened (same text + language, no tree yet) while the install
        // finished.
        let stale_incarnation = store.get(&uri).unwrap().incarnation();
        store.remove(&uri);
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        assert!(
            store
                .attach_tree_if_absent(
                    &uri,
                    "fn main() {}",
                    Some("rust"),
                    stale_incarnation,
                    parse_rust("fn main() {}")
                )
                .is_none(),
            "a freshly-installed grammar's tree from the prior lifetime must not \
             attach to the reopened document"
        );
        assert!(
            store.get(&uri).unwrap().tree().is_none(),
            "the reopened document keeps no tree from the closed lifetime"
        );
    }

    #[test]
    fn update_tree_if_text_and_language_unchanged_rejects_a_changed_language() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///relabelled.rs").unwrap();
        // Document was reopened with the same text but a different language_id
        // while an old-grammar parse was in flight.
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("ruby".to_string()),
            None,
        );
        let incarnation = store.get(&uri).unwrap().incarnation();

        let stored = store.update_tree_if_text_and_language_unchanged(
            &uri,
            "fn main() {}",
            Some("rust"),
            incarnation,
            parse_rust("fn main() {}"),
        );
        assert!(
            stored.is_none(),
            "a tree parsed for a different language must be rejected"
        );
        assert!(
            store.get(&uri).unwrap().tree().is_none(),
            "the relabelled document keeps no wrong-grammar tree"
        );

        // Same language and incarnation → stores.
        assert!(
            store
                .update_tree_if_text_and_language_unchanged(
                    &uri,
                    "fn main() {}",
                    Some("ruby"),
                    incarnation,
                    parse_rust("fn main() {}"),
                )
                .is_some()
        );
        assert!(store.get(&uri).unwrap().tree().is_some());
    }

    #[test]
    fn update_tree_if_text_and_language_unchanged_rejects_a_reopen_with_a_fresh_incarnation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///reopened.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // An off-ingress parse read this lifetime's incarnation, then the document
        // was closed and reopened with identical text and language while the parse
        // was in flight.
        let stale_incarnation = store.get(&uri).unwrap().incarnation();
        store.remove(&uri);
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let stored = store.update_tree_if_text_and_language_unchanged(
            &uri,
            "fn main() {}",
            Some("rust"),
            stale_incarnation,
            parse_rust("fn main() {}"),
        );
        assert!(
            stored.is_none(),
            "a tree from the prior lifetime must not attach to the reopened \
             document even when text and language are identical"
        );
        assert!(
            store.get(&uri).unwrap().tree().is_none(),
            "the reopened document keeps no tree from the closed lifetime"
        );
    }

    #[test]
    fn attach_tree_if_absent_does_not_resurrect_closed_document() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///closed.rs").unwrap();
        // No document exists, so the CAS rejects regardless of the expected
        // language/incarnation (they are never compared on the Vacant path).
        assert!(
            store
                .attach_tree_if_absent(
                    &uri,
                    "fn main() {}",
                    Some("rust"),
                    1,
                    parse_rust("fn main() {}")
                )
                .is_none()
        );
        assert!(
            store.get(&uri).is_none(),
            "must not resurrect a closed document"
        );
    }

    #[test]
    fn set_parse_result_if_text_and_incarnation_unchanged_sets_language_and_tree() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///open_refine.rs").unwrap();
        // didOpen inserted the document with the path/id language guess ("text");
        // the open parse refines it to the content-detected "rust" and attaches a tree.
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("text".to_string()),
            None,
        );
        let (incarnation, text) = {
            let doc = store.get(&uri).unwrap();
            (doc.incarnation(), doc.text_arc())
        };

        assert!(
            store
                .set_parse_result_if_text_and_incarnation_unchanged(
                    &uri,
                    &text,
                    incarnation,
                    Some("rust"),
                    Some(parse_rust("fn main() {}")),
                )
                .is_some()
        );
        let doc = store.get(&uri).unwrap();
        assert_eq!(
            doc.language_id(),
            Some("rust"),
            "the open parse must SET (refine) the language, not just check it"
        );
        assert!(doc.tree().is_some(), "the tree must be attached");
    }

    #[test]
    fn set_parse_result_if_text_and_incarnation_unchanged_records_language_without_a_tree() {
        // The parse-failed-but-language-detected path (parse timeout / parser
        // unavailable / join error): the open parse records the detected language
        // with NO tree, so a host-bridged document keeps its language after a parse
        // failure rather than being nulled out.
        let store = DocumentStore::new();
        let uri = Url::parse("file:///open_lang_no_tree.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("text".to_string()),
            None,
        );
        let (incarnation, text) = {
            let doc = store.get(&uri).unwrap();
            (doc.incarnation(), doc.text_arc())
        };

        assert!(
            store
                .set_parse_result_if_text_and_incarnation_unchanged(
                    &uri,
                    &text,
                    incarnation,
                    Some("rust"),
                    None,
                )
                .is_some()
        );
        let doc = store.get(&uri).unwrap();
        assert_eq!(
            doc.language_id(),
            Some("rust"),
            "the detected language is recorded even though the parse produced no tree"
        );
        assert!(
            doc.tree().is_none(),
            "no tree lands on the parse-failure path"
        );
    }

    #[test]
    fn set_parse_result_if_text_and_incarnation_unchanged_rejects_stale_text() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///open_stale_text.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // Capture the text the parse "read" (its Arc identity) before the edit.
        let (incarnation, text) = {
            let doc = store.get(&uri).unwrap();
            (doc.incarnation(), doc.text_arc())
        };
        // A didChange landed mid-parse, moving the text on (same lifetime).
        store.apply_edit_clearing_tree(&uri, "fn newer() {}".to_string(), &[]);

        let stored = store.set_parse_result_if_text_and_incarnation_unchanged(
            &uri,
            &text,
            incarnation,
            Some("rust"),
            Some(parse_rust("fn main() {}")),
        );
        assert!(
            stored.is_none(),
            "a tree parsed from now-stale text must be dropped"
        );
        assert!(
            store.get(&uri).unwrap().tree().is_none(),
            "the stale tree must not overwrite the newer text's (still-absent) tree"
        );
    }

    #[test]
    fn set_parse_result_if_text_and_incarnation_unchanged_rejects_a_reopen() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///open_reopen.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // The open parse read this lifetime's incarnation + text, then a didClose +
        // reopen (identical text + language) raced it while parsing.
        let (stale_incarnation, stale_text) = {
            let doc = store.get(&uri).unwrap();
            (doc.incarnation(), doc.text_arc())
        };
        store.remove(&uri);
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let stored = store.set_parse_result_if_text_and_incarnation_unchanged(
            &uri,
            &stale_text,
            stale_incarnation,
            Some("rust"),
            Some(parse_rust("fn main() {}")),
        );
        assert!(
            stored.is_none(),
            "a tree from the prior lifetime must not attach to the reopened document"
        );
        assert!(
            store.get(&uri).unwrap().tree().is_none(),
            "the reopened document keeps no tree from the closed lifetime"
        );
    }

    #[test]
    fn set_parse_result_if_text_and_incarnation_unchanged_does_not_resurrect_closed_document() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///open_closed.rs").unwrap();
        // No document exists (a didClose removed it before the off-ingress parse ran).
        assert!(
            store
                .set_parse_result_if_text_and_incarnation_unchanged(
                    &uri,
                    &Arc::<str>::from("fn main() {}"),
                    1,
                    Some("rust"),
                    Some(parse_rust("fn main() {}")),
                )
                .is_none()
        );
        assert!(
            store.get(&uri).is_none(),
            "the off-ingress open parse must not resurrect a closed document"
        );
    }

    #[test]
    fn update_tree_if_text_unchanged_does_not_recreate_parse_state_after_close() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///race.rs").unwrap();
        store.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // `remove` (didClose) drops `parse_states` before `documents`; model the
        // window where the parse-state is already gone but our in-flight CAS still
        // sees the document.
        store.parse_states.remove(&uri);

        let stored =
            store.update_tree_if_text_unchanged(&uri, "fn main() {}", parse_rust("fn main() {}"));
        assert!(stored, "the still-present document gets the tree");
        assert!(
            store.parse_states.get(&uri).is_none(),
            "the availability update must not recreate a parse-state for a URI whose \
             parse-state a concurrent close already removed"
        );
    }

    /// Read the published watermark ticket for a URI (test-only; the field is private).
    fn watermark_of(store: &DocumentStore, uri: &Url) -> Option<u64> {
        store.watermarks.get(uri).map(|s| s.borrow().ticket)
    }

    /// Register a document so its watermark entry is seeded (mirrors a didOpen).
    fn seed_document(store: &DocumentStore, uri: &Url) {
        store.insert(uri.clone(), "x".to_string(), Some("rust".to_string()), None);
    }

    #[test]
    fn insert_seeds_a_watermark_entry_at_zero() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        assert_eq!(
            watermark_of(&store, &uri),
            None,
            "no entry before registration"
        );

        seed_document(&store, &uri);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(0),
            "registering the document seeds a watermark at 0"
        );
    }

    #[test]
    fn update_document_seeds_a_watermark_entry_when_it_inserts() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        // `update_document` (a reader on-demand fallback) inserts on its vacant
        // branch; the watermark invariant must hold for the resulting document.
        store.update_document(uri.clone(), "x".to_string(), None);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(0),
            "a document created via update_document must still get a watermark entry"
        );
    }

    #[test]
    fn advance_watermark_publishes_the_ticket() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);

        store.advance_watermark(&uri, 7);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(7),
            "publish records the ticket"
        );
    }

    #[test]
    fn advance_watermark_is_monotonic() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 5);
        // An out-of-order resolution for an earlier ticket must not regress it.
        store.advance_watermark(&uri, 3);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(5),
            "a later-but-lower publish must not regress the watermark"
        );

        store.advance_watermark(&uri, 8);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(8),
            "a higher ticket advances it"
        );
    }

    #[test]
    fn advance_watermark_does_not_resurrect_a_closed_document() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 2);

        // didClose drops the entry; a straggler parse for a now-stale ticket then
        // resolves and tries to publish. Being non-inserting, it must NOT recreate
        // a ghost watermark for the closed URI.
        store.remove(&uri);
        store.advance_watermark(&uri, 3);
        assert_eq!(
            watermark_of(&store, &uri),
            None,
            "a publish after close must not resurrect a watermark entry"
        );
    }

    #[test]
    fn advance_watermark_for_incarnation_advances_within_the_same_lifetime() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        let incarnation = store.get(&uri).unwrap().incarnation();

        store.advance_watermark_for_incarnation(&uri, 5, incarnation);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(5),
            "a parse from the current lifetime advances the watermark"
        );
    }

    #[test]
    fn advance_watermark_for_incarnation_skips_a_prior_lifetime() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        let stale_incarnation = store.get(&uri).unwrap().incarnation();

        // Close + reopen: the watermark is dropped and re-seeded at 0 for the new
        // lifetime. A straggler parse from the prior lifetime then resolves with
        // its old ticket; it must NOT advance the reopened document's watermark
        // (which would prematurely release a new-lifetime reader).
        store.remove(&uri);
        seed_document(&store, &uri);

        store.advance_watermark_for_incarnation(&uri, 9, stale_incarnation);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(0),
            "a prior lifetime's parse must not advance the reopened watermark"
        );

        // The reopen's own parse (current incarnation) does advance it.
        let fresh_incarnation = store.get(&uri).unwrap().incarnation();
        store.advance_watermark_for_incarnation(&uri, 1, fresh_incarnation);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(1),
            "the reopened lifetime's own parse advances its watermark"
        );
    }

    #[test]
    fn apply_edit_clearing_tree_preserves_the_watermark_ticket() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        let incarnation = store.get(&uri).unwrap().incarnation();
        store.advance_watermark_for_incarnation(&uri, 7, incarnation);
        assert_eq!(watermark_of(&store, &uri), Some(7));

        // An edit is the same lifetime: `ensure_watermark_entry` must hit its
        // matching-incarnation fast path and leave the live ticket untouched (a
        // reset to 0 would prematurely release readers gated behind ticket 7).
        store.apply_edit_clearing_tree(&uri, "xy".to_string(), &[]);
        assert_eq!(
            watermark_of(&store, &uri),
            Some(7),
            "an edit (same incarnation) must not reset the live watermark ticket"
        );
    }

    #[test]
    fn remove_drops_the_watermark_entry() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 4);
        assert_eq!(watermark_of(&store, &uri), Some(4));

        store.remove(&uri);
        assert_eq!(
            watermark_of(&store, &uri),
            None,
            "closing the document drops its watermark so a reopen starts fresh"
        );
    }

    #[tokio::test]
    async fn wait_for_epoch_returns_immediately_when_already_reached() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 5);

        // target below the watermark resolves without blocking.
        timeout(
            Duration::from_millis(100),
            store.wait_for_epoch(&uri, 3, Duration::from_secs(10)),
        )
        .await
        .expect("a watermark already past the target must not block");
    }

    #[tokio::test]
    async fn wait_for_epoch_returns_immediately_when_no_entry() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///absent.rs").unwrap();
        // No publish has happened: nothing is pending, so the reader proceeds.
        timeout(
            Duration::from_millis(100),
            store.wait_for_epoch(&uri, 1, Duration::from_secs(10)),
        )
        .await
        .expect("a missing watermark entry means nothing to wait for");
    }

    #[tokio::test]
    async fn wait_for_epoch_blocks_until_watermark_reaches_target() {
        let store = Arc::new(DocumentStore::new());
        let uri = Url::parse("file:///wm.rs").unwrap();
        // Entry seeded at 0, advanced to ticket 1, below the reader's target of 3.
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 1);

        let waiter = {
            let store = Arc::clone(&store);
            let uri = uri.clone();
            tokio::spawn(async move {
                store.wait_for_epoch(&uri, 3, Duration::from_secs(10)).await;
            })
        };

        // The intermediate ticket 2 must not release a reader waiting for 3.
        store.advance_watermark(&uri, 2);
        // Let the spawned waiter run to its park point (deterministic on the
        // current-thread test runtime — no wall-clock dependency).
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "reader must keep waiting until the watermark covers its target ticket"
        );

        store.advance_watermark(&uri, 3);
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("reaching the target must wake the reader")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn wait_for_epoch_proceeds_when_entry_removed_mid_wait() {
        let store = Arc::new(DocumentStore::new());
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 1);

        let waiter = {
            let store = Arc::clone(&store);
            let uri = uri.clone();
            tokio::spawn(async move {
                store.wait_for_epoch(&uri, 9, Duration::from_secs(10)).await;
            })
        };

        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "reader is waiting for an unreached target"
        );

        // A didClose drops the watermark entry: the blocked reader must proceed
        // (into the empty fallback) rather than stall to the timeout.
        store.remove(&uri);
        timeout(Duration::from_secs(1), waiter)
            .await
            .expect("dropping the watermark sender must wake the reader")
            .expect("waiter task panicked");
    }

    #[tokio::test]
    async fn wait_for_epoch_times_out_when_target_unreached() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///wm.rs").unwrap();
        seed_document(&store, &uri);
        store.advance_watermark(&uri, 1);

        // The watermark never reaches 9; the bounded wait returns after the
        // timeout rather than hanging.
        timeout(
            Duration::from_secs(5),
            store.wait_for_epoch(&uri, 9, Duration::from_millis(80)),
        )
        .await
        .expect("wait_for_epoch must return after its own timeout even if unreached");
    }

    #[test]
    fn test_concurrent_update_and_get_no_deadlock() {
        // This test verifies that concurrent update_document and get operations
        // do not cause deadlock. The test uses a timeout to detect deadlock.
        let store = Arc::new(DocumentStore::new());
        let uri = Url::parse("file:///test.rs").unwrap();

        // Insert initial document
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let initial_text = "fn main() {}".to_string();
        let tree = parser.parse(&initial_text, None).unwrap();
        store.insert(
            uri.clone(),
            initial_text,
            Some("rust".to_string()),
            Some(tree),
        );

        let num_threads = 10;
        let iterations_per_thread = 100;
        let mut handles = vec![];

        // Spawn writer threads
        for i in 0..num_threads {
            let store_clone = store.clone();
            let uri_clone = uri.clone();
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();

            let handle = thread::spawn(move || {
                for j in 0..iterations_per_thread {
                    let text =
                        format!("fn main() {{ let x = {}; }}", i * iterations_per_thread + j);
                    let tree = parser.parse(&text, None).unwrap();
                    store_clone.update_document(uri_clone.clone(), text, Some(tree));
                }
            });
            handles.push(handle);
        }

        // Spawn reader threads
        for _ in 0..num_threads {
            let store_clone = store.clone();
            let uri_clone = uri.clone();

            let handle = thread::spawn(move || {
                for _ in 0..iterations_per_thread {
                    // get() returns a Ref which holds a read lock
                    if let Some(doc) = store_clone.get(&uri_clone) {
                        // Access the document while holding the lock
                        let _ = doc.text();
                        let _ = doc.tree();
                    }
                }
            });
            handles.push(handle);
        }

        // Wait for all threads with timeout (5 seconds)
        // If deadlock occurs, this will hang and the test will fail
        let timeout = Duration::from_secs(5);
        let start = std::time::Instant::now();

        for handle in handles {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                panic!("Test timed out - possible deadlock detected");
            }

            // Use a channel to implement join with timeout
            let (tx, rx) = std::sync::mpsc::channel();
            let join_handle = thread::spawn(move || {
                let result = handle.join();
                let _ = tx.send(result);
            });

            match rx.recv_timeout(remaining) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => panic!("Thread panicked"),
                Err(_) => panic!("Test timed out - possible deadlock detected"),
            }

            // Clean up the join wrapper thread
            let _ = join_handle.join();
        }

        // If we get here, no deadlock occurred
        // Verify final state is consistent
        let doc = store.get(&uri).expect("Document should exist");
        assert!(!doc.text().is_empty());
    }

    #[test]
    fn test_add_and_get_document() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.txt").unwrap();
        let text = "hello world".to_string();

        store.insert(uri.clone(), text.clone(), None, None);
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text(), &text);
    }

    #[test]
    fn test_update_document_preserves_language() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let text1 = "fn main() {}".to_string();
        let text2 = "fn main() { println!(\"hello\"); }".to_string();

        // Create a fake tree for testing
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&text1, None).unwrap();

        store.insert(uri.clone(), text1, Some("rust".to_string()), Some(tree));

        // Update text
        store.update_document(uri.clone(), text2.clone(), None);

        // Language info should be preserved
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text(), text2);
        assert_eq!(doc.language_id(), Some("rust"));
    }

    #[test]
    fn test_edit_lock_is_stable_per_uri_and_cleared_on_remove() {
        let store = DocumentStore::new();
        let uri_a = Url::parse("file:///a.rs").unwrap();
        let uri_b = Url::parse("file:///b.rs").unwrap();

        // Same URI yields the same lock instance, so concurrent didChange
        // handlers for one document serialize on a shared mutex.
        let a1 = store.edit_lock(&uri_a);
        let a2 = store.edit_lock(&uri_a);
        assert!(Arc::ptr_eq(&a1, &a2), "same URI must share one edit lock");

        // Different URIs get distinct locks so unrelated documents stay parallel.
        let b1 = store.edit_lock(&uri_b);
        assert!(
            !Arc::ptr_eq(&a1, &b1),
            "different URIs must not share an edit lock"
        );

        // Removing the document drops its lock entry; a later edit_lock call
        // mints a fresh, distinct lock (open/close cycles don't reuse a stale
        // mutex). Compare pointer identity directly — `a1` is kept alive so the
        // check doesn't depend on incidental Arc clone counts.
        store.remove(&uri_a);
        let a3 = store.edit_lock(&uri_a);
        assert!(
            !Arc::ptr_eq(&a1, &a3),
            "lock after remove must be a fresh instance, not the cleared one"
        );

        // remove_edit_lock drops the entry on the document-missing path, so the
        // next lock for the same URI is again a fresh instance.
        store.remove_edit_lock(&uri_a);
        let a4 = store.edit_lock(&uri_a);
        assert!(
            !Arc::ptr_eq(&a3, &a4),
            "lock after remove_edit_lock must be a fresh instance"
        );
    }

    #[test]
    fn test_document_layer_preservation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let text = "let x = 1;".to_string();

        // Create document with tree
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();

        store.insert(
            uri.clone(),
            text.clone(),
            Some("rust".to_string()),
            Some(tree.clone()),
        );

        // Update with new tree
        let new_text = "let x = 2;".to_string();
        let new_tree = parser.parse(&new_text, Some(&tree)).unwrap();
        store.update_document(uri.clone(), new_text.clone(), Some(new_tree));

        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text(), new_text);
        assert!(doc.tree().is_some());
    }

    #[tokio::test]
    async fn wait_for_parse_completion_blocks_until_finished() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///parse-wait.lua").unwrap();

        let generation = store.mark_parse_started(&uri);
        let wait_future = store.wait_for_parse_completion(&uri, Duration::from_secs(1));
        let mut wait_future = Box::pin(wait_future);

        assert!(
            timeout(Duration::from_millis(10), &mut wait_future)
                .await
                .is_err(),
            "wait should block while parse is in progress"
        );

        store.mark_parse_finished(&uri, generation, true);

        assert!(
            timeout(Duration::from_millis(200), wait_future)
                .await
                .is_ok(),
            "wait should complete after parse finishes"
        );
    }

    #[tokio::test]
    async fn wait_for_parse_completion_returns_when_tree_available() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///parse-ready.rs").unwrap();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let text = "fn main() {}".to_string();
        let tree = parser.parse(&text, None).unwrap();

        store.insert(uri.clone(), text, Some("rust".to_string()), Some(tree));

        let wait_future = store.wait_for_parse_completion(&uri, Duration::from_secs(1));
        assert!(
            timeout(Duration::from_millis(10), wait_future)
                .await
                .is_ok(),
            "wait should return immediately when a tree is already available"
        );
    }
}
