//! Stable node identity tracking (lazy-node-identity-tracking).
//!
//! This module provides ULID-based identifiers for tree-sitter nodes — originally
//! used for injection regions, now generalized for [node-reference-protocol](../../docs/architecture-decisions/node-reference-protocol.md) —
//! that remain stable across document edits using position-based composite keys
//! with START-priority invalidation.

use dashmap::DashMap;
use log::{error, warn};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use ulid::Ulid;
use url::Url;

/// Tracks stable ULID-based identifiers for tree-sitter nodes.
///
/// Uses position-based composite keys (start_byte, end_byte, kind, layer) per lazy-node-identity-tracking
/// and applies START-priority invalidation rules to maintain stable ULIDs across edits.
/// Originally introduced for injection regions; generalized for the Node Reference
/// Protocol ([node-reference-protocol](../../docs/architecture-decisions/node-reference-protocol.md)).
///
/// Maintains a bidirectional index per URI:
/// - forward (`PositionKey -> (Ulid, incarnation)`) for assignment / dedup
/// - reverse (`Ulid -> (PositionKey, incarnation)`) for resolving a held ULID back to a node
///   range (used by `kakehashi/node/text` and future navigation methods).
///
/// Both directions are kept in sync across `get_or_create`, `adjust_for_edits`,
/// and `cleanup` so an invalidated ULID becomes indistinguishable from a never-issued
/// one (per lazy-node-identity-tracking "no tombstone" rule).
pub(crate) struct NodeTracker {
    entries: DashMap<Url, UriEntries>,
    /// Current document lifetime for URIs managed by the LSP lifecycle.
    /// `None` is a close tombstone: it prevents a direct mint that passed its
    /// last reader-side liveness check before didClose from recreating an old
    /// entry after cleanup. Markers are retained at one `(Url, Option<u64>)`
    /// per distinct URI for process lifetime; they contain no node IDs or
    /// document data. An absent key keeps standalone/test trackers usable.
    incarnations: DashMap<Url, Option<u64>>,
    /// Bumped by every [`cleanup`](Self::cleanup) (didClose). Folded into the
    /// mint latch ([`mint_epoch`](Self::mint_epoch)) so a close/reopen —
    /// which removes the per-URI index and would reset its `shift_gen` to 0
    /// (ABA) — can never make an old-lifetime latch compare equal. Global
    /// (not per-URI) so closed documents leave no retained node-id state
    /// (only the small lifecycle marker documented on `incarnations`); the
    /// cost is that a close of any document mid-pass refuses that pass's
    /// remaining mint batches — a rare event with a one-null-re-sync
    /// consequence.
    cleanup_epoch: std::sync::atomic::AtomicU64,
}

/// Per-URI bidirectional index between `PositionKey` and `Ulid`.
///
/// The two maps describe the same set of (key, ulid) pairs from different
/// directions and MUST stay in sync. All mutating methods on this struct
/// enforce that invariant; outside callers should not touch the fields
/// directly.
#[derive(Default)]
struct UriEntries {
    forward: HashMap<PositionKey, TrackedUlid>,
    reverse: HashMap<Ulid, TrackedPosition>,
    /// Highest document incarnation that has minted into this URI index.
    /// Once a reopen mints, a straggling older-lifetime reader must not evict
    /// or append entries behind it.
    latest_incarnation: u64,
    /// Count of coordinate shifts (edit applications) this index has
    /// received. Read/written under the same DashMap entry lock as the maps,
    /// so [`mint_batch_if_unshifted`](NodeTracker::mint_batch_if_unshifted)
    /// can atomically prove "no shift intervened between the pass's latch
    /// and this batch" — the correct-at-birth predicate (coordinate
    /// comparison alone cannot: with two mid-pass edits, an entry minted
    /// between them is shifted by the second yet still missed the first).
    shift_gen: u64,
    /// Collision-free discriminators for bridge injection alternatives. Kept
    /// with the URI lifecycle so document-controlled dynamic language names
    /// are reclaimed on close instead of accumulating process-wide.
    named_layers: HashMap<(usize, String), usize>,
}

#[derive(Clone, Copy)]
struct TrackedUlid {
    ulid: Ulid,
    incarnation: u64,
}

#[derive(Clone, Copy)]
struct TrackedPosition {
    key: PositionKey,
    incarnation: u64,
}

impl UriEntries {
    fn named_layer(
        &mut self,
        pattern_index: usize,
        language: &str,
        incarnation: u64,
    ) -> Option<usize> {
        if incarnation < self.latest_incarnation {
            return None;
        }
        if incarnation > self.latest_incarnation {
            self.named_layers.clear();
        }
        self.latest_incarnation = incarnation;
        let next = self.named_layers.len();
        match self
            .named_layers
            .entry((pattern_index, language.to_owned()))
        {
            Entry::Occupied(entry) => Some(*entry.get()),
            Entry::Vacant(entry) => (next < usize::MAX / 2 + 1).then(|| {
                entry.insert(next);
                next
            }),
        }
    }

    /// Get or insert a ULID for a position key, keeping both maps in sync.
    fn get_or_insert(&mut self, key: PositionKey, incarnation: u64) -> Option<Ulid> {
        if incarnation < self.latest_incarnation {
            return None;
        }
        self.latest_incarnation = incarnation;
        if let Some(existing) = self.forward.get(&key) {
            if existing.incarnation == incarnation {
                return Some(existing.ulid);
            }
            self.reverse.remove(&existing.ulid);
        }
        let ulid = Ulid::new();
        self.forward.insert(key, TrackedUlid { ulid, incarnation });
        self.reverse
            .insert(ulid, TrackedPosition { key, incarnation });
        Some(ulid)
    }

    /// Returns the number of (key, ulid) pairs currently tracked.
    fn len(&self) -> usize {
        debug_assert_eq!(
            self.forward.len(),
            self.reverse.len(),
            "NodeTracker forward/reverse indices out of sync"
        );
        self.forward.len()
    }

    /// Drain all (key, ulid) entries, clearing both maps.
    fn drain(&mut self) -> impl Iterator<Item = (PositionKey, TrackedUlid)> + '_ {
        self.reverse.clear();
        self.forward.drain()
    }

    /// Insert a (key, ulid) pair; both maps must remain in sync.
    ///
    /// Returns `Err(existing_ulid)` if a different ULID already occupies the
    /// position. Caller decides what to do with the collision (the current
    /// adjust_for_edits policy is "first wins").
    fn insert(&mut self, key: PositionKey, tracked: TrackedUlid) -> std::result::Result<(), Ulid> {
        match self.forward.entry(key) {
            Entry::Vacant(e) => {
                e.insert(tracked);
                self.reverse.insert(
                    tracked.ulid,
                    TrackedPosition {
                        key,
                        incarnation: tracked.incarnation,
                    },
                );
                Ok(())
            }
            Entry::Occupied(e) => Err(e.get().ulid),
        }
    }

    fn retain_newer_than(&mut self, closing_incarnation: u64) {
        self.forward
            .retain(|_, tracked| tracked.incarnation > closing_incarnation);
        self.reverse
            .retain(|_, tracked| tracked.incarnation > closing_incarnation);
        if self.latest_incarnation <= closing_incarnation {
            self.named_layers.clear();
        }
    }
}

/// Composite key for position-based tracking (lazy-node-identity-tracking).
///
/// `kind` was originally assumed to separate co-located nodes from different
/// injection layers ("a Markdown `fenced_code_block` vs a Python `module`
/// differ by kind"). Recursive same-language injection breaks that: a
/// ```` ```markdown ```` block injected into Markdown yields the same kind at the
/// same span in both the host and injected tree. `layer` (injection depth, `0` =
/// host) restores uniqueness so the two get distinct ULIDs — see
/// lazy-node-identity-tracking §"Node Uniqueness Key". `layer` is internal:
/// clients only ever see the opaque ULID.
#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)]
struct PositionKey {
    start_byte: usize,
    end_byte: usize,
    /// tree-sitter node kinds are interned in the grammar's static data, so
    /// every call site we accept input from (`Node::kind()`) returns a
    /// `&'static str`. Storing the static slice avoids an allocation per
    /// tracker entry without losing any genuinely owned data.
    kind: &'static str,
    /// Injection depth that minted the node (`0` = host). Preserved by position
    /// adjustment: a depth index does not move with byte positions, so it is
    /// carried through edits unchanged. It is *not* an absolute identity — an
    /// edit that restructures injection nesting (adds/removes an outer layer)
    /// shifts a node's true depth, leaving the stored `layer` stale. The held
    /// ULID then degrades to "re-acquire" (see `with_resolved_node`), rather
    /// than guaranteeing the node still lives at this depth.
    layer: usize,
}

impl PositionKey {
    /// Create a new position key from byte range, node kind, and injection layer.
    fn new(start: usize, end: usize, kind: &'static str, layer: usize) -> Self {
        Self {
            start_byte: start,
            end_byte: end,
            kind,
            layer,
        }
    }

    /// Create a position key with adjusted positions (for edit operations).
    ///
    /// `kind` and `layer` are carried over verbatim via struct-update syntax —
    /// neither shifts with byte positions (`PositionKey` is `Copy`).
    fn with_positions(self, start: usize, end: usize) -> Self {
        Self {
            start_byte: start,
            end_byte: end,
            ..self
        }
    }
}

/// Edit position information for node tracking.
///
/// Represents byte positions of a text edit. Used to decouple
/// NodeTracker from tree_sitter::InputEdit.
///
/// Fields are intentionally private - use `new()` or `From<&InputEdit>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditInfo {
    start_byte: usize,
    old_end_byte: usize,
    new_end_byte: usize,
}

impl EditInfo {
    /// Create a new EditInfo with byte positions.
    ///
    /// Debug-asserts `old_end_byte >= start_byte`: a correct LSP client never
    /// sends an inverted edit, so a violation signals a caller bug.
    pub(crate) fn new(start_byte: usize, old_end_byte: usize, new_end_byte: usize) -> Self {
        debug_assert!(
            old_end_byte >= start_byte,
            "Invalid EditInfo: old_end_byte ({}) < start_byte ({}). \
             This indicates a bug in the caller.",
            old_end_byte,
            start_byte
        );
        Self {
            start_byte,
            old_end_byte,
            new_end_byte,
        }
    }

    /// Calculate the byte delta (positive for insertion, negative for deletion).
    fn delta(&self) -> i64 {
        self.new_end_byte as i64 - self.old_end_byte as i64
    }

    /// Check if this is a zero-length (insertion-only) edit.
    ///
    /// Zero-length edits get special invalidation handling in lazy-node-identity-tracking.
    fn is_insertion_only(&self) -> bool {
        self.start_byte == self.old_end_byte
    }
}

impl From<&tree_sitter::InputEdit> for EditInfo {
    fn from(edit: &tree_sitter::InputEdit) -> Self {
        Self::new(edit.start_byte, edit.old_end_byte, edit.new_end_byte)
    }
}

/// Apply a signed delta to a byte position with overflow protection.
///
/// Computes in `i64` and saturates so large negative deltas can't underflow,
/// clamping the result to 0 rather than wrapping.
fn apply_delta(position: usize, delta: i64) -> usize {
    (position as i64).saturating_add(delta).max(0) as usize
}

/// Adjust a position key based on an edit operation (lazy-node-identity-tracking position adjustment).
///
/// Returns the adjusted `PositionKey`, or `None` if the range collapsed
/// (the node should then be invalidated).
fn adjust_position_for_edit(key: PositionKey, edit: &EditInfo, delta: i64) -> Option<PositionKey> {
    if key.start_byte >= edit.old_end_byte {
        // Node E: AFTER edit → shift both start and end
        Some(key.with_positions(
            apply_delta(key.start_byte, delta),
            apply_delta(key.end_byte, delta),
        ))
    } else if key.end_byte > edit.start_byte {
        // Node A/B: CONTAINS or OVERLAPS edit
        //
        // Two sub-cases for end position:
        // 1. End INSIDE edit range (absorbed): clamp to edit.new_end_byte
        // 2. End AFTER edit range: apply delta normally
        let new_end = if key.end_byte <= edit.old_end_byte {
            // End absorbed: clamp to where the edit ends in the new document
            // Example: Node [20, 30), Edit delete [25, 55) → Node becomes [20, 25)
            edit.new_end_byte
        } else {
            // End after edit: apply delta
            apply_delta(key.end_byte, delta)
        };

        // Guard: If range collapses (start >= end), return None to invalidate
        //
        // PRECONDITION: This branch requires node passed START-priority check,
        // meaning key.start_byte < edit.start_byte (otherwise node would be
        // invalidated before reaching here).
        //
        // Given this precondition, collapse is unreachable because:
        //   - For collapse: new_end <= key.start_byte
        //   - If end absorbed: new_end = edit.new_end_byte >= edit.start_byte > key.start_byte
        //   - If end after edit: new_end = key.end_byte + delta > key.start_byte
        //     (since key.end_byte > edit.start_byte > key.start_byte initially)
        //
        // Kept as defense-in-depth: protects against future refactoring that
        // might invalidate the precondition or introduce new branches.
        if new_end <= key.start_byte {
            None // Range collapsed to zero or negative
        } else {
            Some(key.with_positions(key.start_byte, new_end))
        }
    } else {
        // Node F: BEFORE edit → unchanged
        Some(key)
    }
}

impl NodeTracker {
    /// Create a new empty tracker.
    pub(crate) fn new() -> Self {
        Self {
            entries: DashMap::new(),
            incarnations: DashMap::new(),
            cleanup_epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Register the current open lifetime before any request can mint for it.
    pub(crate) fn open_incarnation(&self, uri: &Url, incarnation: u64) {
        self.incarnations.insert(uri.clone(), Some(incarnation));
    }

    fn admits_incarnation(&self, uri: &Url, incarnation: u64) -> bool {
        self.incarnations
            .get(uri)
            .is_none_or(|current| *current == Some(incarnation))
    }

    /// Allocate a collision-free, URI-lifecycle-owned layer discriminator for
    /// an injection query pattern and its (possibly dynamic) language.
    pub(crate) fn named_layer_for_incarnation(
        &self,
        uri: &Url,
        pattern_index: usize,
        language: &str,
        incarnation: u64,
    ) -> Option<usize> {
        if !self.admits_incarnation(uri, incarnation) {
            return None;
        }
        if let Some(mut entry) = self.entries.get_mut(uri) {
            if !self.admits_incarnation(uri, incarnation) {
                return None;
            }
            return entry.named_layer(pattern_index, language, incarnation);
        }
        let mut entry = self.entries.entry(uri.clone()).or_default();
        if !self.admits_incarnation(uri, incarnation) {
            drop(entry);
            self.remove_pristine_entry(uri);
            return None;
        }
        entry.named_layer(pattern_index, language, incarnation)
    }

    /// Reclaim an entry this call materialized only to lose the lifecycle
    /// admission race. A concurrent reopen may already have edited or minted;
    /// retain either signal (`shift_gen > 0` or non-empty ids).
    fn remove_pristine_entry(&self, uri: &Url) {
        self.entries.remove_if(uri, |_, entry| {
            entry.shift_gen == 0 && entry.len() == 0 && entry.named_layers.is_empty()
        });
    }

    /// Get or create a stable ULID for a **host-layer** tree-sitter node.
    ///
    /// Convenience for the common case (host tree, region content nodes): it
    /// delegates to [`get_or_create_in_layer`](Self::get_or_create_in_layer)
    /// with `layer = 0`. Injection-aware call sites that mint nodes from a
    /// deeper layer MUST use `get_or_create_in_layer` so host and injected
    /// nodes sharing `(start, end, kind)` receive distinct ULIDs.
    #[cfg(test)]
    pub(crate) fn get_or_create(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
        kind: &'static str,
    ) -> Ulid {
        self.get_or_create_for_incarnation(uri, start, end, kind, 0)
            .expect("unmanaged test incarnation is admitted")
    }

    pub(crate) fn get_or_create_for_incarnation(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
        kind: &'static str,
        incarnation: u64,
    ) -> Option<Ulid> {
        self.get_or_create_in_layer_for_incarnation(uri, start, end, kind, 0, incarnation)
    }

    /// Get or create a stable ULID for a tree-sitter node at injection `layer`.
    ///
    /// Uses lazy-node-identity-tracking composite key `(start_byte, end_byte,
    /// kind, layer)`: the same key returns the same ULID within one document
    /// incarnation, while a reopen remints it. Different nodes — including a host vs injected node that share an
    /// identical span and kind (`layer` differs) — receive distinct ULIDs.
    ///
    /// Updates both the forward and reverse index so the returned ULID can be
    /// resolved back to its position via [`lookup_node`](Self::lookup_node).
    #[cfg(test)]
    pub(crate) fn get_or_create_in_layer(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
        kind: &'static str,
        layer: usize,
    ) -> Ulid {
        self.get_or_create_in_layer_for_incarnation(uri, start, end, kind, layer, 0)
            .expect("unmanaged test incarnation is admitted")
    }

    pub(crate) fn get_or_create_in_layer_for_incarnation(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
        kind: &'static str,
        layer: usize,
        incarnation: u64,
    ) -> Option<Ulid> {
        let key = PositionKey::new(start, end, kind, layer);

        // Reject a known-closed/wrong lifetime before materializing an empty
        // tracker entry. Re-check under the entry lock below: didClose can
        // change lifecycle admission between this fast rejection and locking.
        if !self.admits_incarnation(uri, incarnation) {
            return None;
        }

        // `get_mut` first: the hot path (index already exists) avoids the
        // `Url` clone `entry()` needs for its owned key. (Explicit two-step
        // pattern to avoid DashMap lifetime ambiguity.)
        if let Some(mut entry) = self.entries.get_mut(uri) {
            if !self.admits_incarnation(uri, incarnation) {
                drop(entry);
                self.remove_pristine_entry(uri);
                return None;
            }
            return entry.get_or_insert(key, incarnation);
        }
        let mut entry = self.entries.entry(uri.clone()).or_default();
        if !self.admits_incarnation(uri, incarnation) {
            drop(entry);
            self.remove_pristine_entry(uri);
            return None;
        }
        entry.get_or_insert(key, incarnation)
    }

    /// The count of coordinate shifts (edit applications) `uri`'s index has
    /// received — see [`mint_batch_if_unshifted`](Self::mint_batch_if_unshifted)
    /// for the atomicity contract. `0` for an index no edit has touched yet
    /// (edits bump the generation even for a URI with no entries, so a latch
    /// taken before the first mint still detects an intervening shift).
    pub(crate) fn shift_generation(&self, uri: &Url) -> u64 {
        self.entries.get(uri).map(|e| e.shift_gen).unwrap_or(0)
    }

    /// Resolve a ULID back to its tracked `(start_byte, end_byte, kind)` triple.
    ///
    /// Drops the layer discriminator — callers that need it (the navigation
    /// handlers, to re-mint in the same layer) use
    /// [`lookup_node`](Self::lookup_node) instead.
    ///
    /// Returns `None` if the ULID was never issued for this URI, if it was
    /// invalidated by an edit (START fell inside the edit range per lazy-node-identity-tracking),
    /// or if the URI has been closed. These three cases are deliberately
    /// indistinguishable — see node-reference-protocol "Invalidate vs Not-Found".
    pub(crate) fn lookup_position(
        &self,
        uri: &Url,
        ulid: &Ulid,
    ) -> Option<(usize, usize, &'static str, u64)> {
        let (start, end, kind, _layer, incarnation) = self.lookup_node(uri, ulid)?;
        Some((start, end, kind, incarnation))
    }

    /// Resolve a ULID back to its tracked `(start_byte, end_byte, kind, layer)`.
    ///
    /// Like [`lookup_position`](Self::lookup_position) but also returns the
    /// injection `layer` that minted the node, so navigation handlers can
    /// resolve it in the correct language tree and re-mint parent/children in
    /// the same layer.
    pub(crate) fn lookup_node(
        &self,
        uri: &Url,
        ulid: &Ulid,
    ) -> Option<(usize, usize, &'static str, usize, u64)> {
        let entries = self.entries.get(uri)?;
        let tracked = entries.reverse.get(ulid)?;
        let key = tracked.key;
        Some((
            key.start_byte,
            key.end_byte,
            key.kind,
            key.layer,
            tracked.incarnation,
        ))
    }

    /// Get the ULID for a position in a layer if it exists, without creating
    /// it — the read-only half of `get_or_create_in_layer`.
    ///
    /// Serve-stale captures use it so a trailing snapshot can reuse the ids
    /// of positions the intervening edits did not shift (keeping the delta
    /// lineage minimal) without ever writing its stale coordinates into this
    /// live-coordinate index.
    #[cfg(test)]
    pub(crate) fn lookup_in_layer(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
        kind: &'static str,
        layer: usize,
    ) -> Option<Ulid> {
        let key = PositionKey::new(start, end, kind, layer);
        self.entries
            .get(uri)?
            .forward
            .get(&key)
            .map(|entry| entry.ulid)
    }

    /// Incarnation-filtered read-only lookup for stale-serving paths. A pass
    /// from an older lifetime must never reuse a reopened lifetime's ID even
    /// when its position key is byte-identical.
    pub(crate) fn lookup_in_layer_for_incarnation(
        &self,
        uri: &Url,
        start: usize,
        end: usize,
        kind: &'static str,
        layer: usize,
        incarnation: u64,
    ) -> Option<Ulid> {
        let key = PositionKey::new(start, end, kind, layer);
        let entries = self.entries.get(uri)?;
        let tracked = entries.forward.get(&key)?;
        (tracked.incarnation == incarnation).then_some(tracked.ulid)
    }

    /// Get the ULID for a host-layer position if it exists, without creating it.
    ///
    /// Returns None if no entry exists for this position.
    /// Used in tests to verify position adjustment without side effects.
    #[cfg(test)]
    fn get(&self, uri: &Url, start: usize, end: usize, kind: &'static str) -> Option<Ulid> {
        self.lookup_in_layer(uri, start, end, kind, 0)
    }

    /// Reconstruct character-level edits between `old_text` and `new_text` and
    /// apply them right-to-left, returning every invalidated ULID so callers
    /// can send `didClose` for orphaned virtual documents. Identical texts
    /// fast-path to an empty result.
    ///
    /// Reverse order is used here (unlike `apply_input_edits`) because the
    /// diff-reconstructed edits all reference *original* coordinates; applying
    /// from highest position first keeps each subsequent edit's coordinates valid.
    pub(crate) fn apply_text_diff(&self, uri: &Url, old_text: &str, new_text: &str) -> Vec<Ulid> {
        // Fast path: identical texts need no processing
        if old_text == new_text {
            return Vec::new();
        }

        let edits = Self::reconstruct_individual_edits(old_text, new_text);

        if edits.is_empty() {
            // Defensive: similar crate should always produce edits for different texts.
            // Empty edits with different texts indicates a library bug, not recoverable.
            // Returning empty Vec is safe: worst case is nodes aren't invalidated.
            // Next full parse will correct positions anyway.
            debug_assert!(false, "No edits from diff despite old_text != new_text");
            warn!(
                target: "kakehashi::node_tracker",
                "No edits from diff despite old_text != new_text (uri={}). \
                 This may indicate a similar crate bug.",
                uri
            );
            return Vec::new();
        }

        // Runtime validation for non-overlapping invariant
        let has_overlap = edits
            .windows(2)
            .any(|w| w[0].old_end_byte > w[1].start_byte);
        if has_overlap {
            // Use error! level - this path should be unreachable if similar crate behaves correctly
            error!(
                target: "kakehashi::node_tracker",
                "Overlapping edits from diff (uri={}, edit_count={}). \
                 Falling back to whole-document invalidation. \
                 This may indicate a similar crate bug or version incompatibility.",
                uri,
                edits.len()
            );
            // Conservative fallback: treat entire document as single edit
            let fallback = EditInfo::new(0, old_text.len(), new_text.len());
            return self.apply_single_edit(uri, &fallback);
        }

        // IMPORTANT: Process in REVERSE order because diff edits use ORIGINAL coordinates.
        // This differs from apply_input_edits() which uses FORWARD order because
        // LSP incremental edits use RUNNING coordinates (each edit's position is
        // relative to the document state AFTER all previous edits).
        // See lazy-node-identity-tracking for invalidation rules and docs/architecture-decisions/lazy-node-identity-tracking.md.
        //
        // KNOWN RACE CONDITION (documented, accepted):
        // A concurrent get_or_create() between edit applications may create nodes
        // in intermediate coordinate space. These nodes may be incorrectly processed
        // by subsequent edits (which use original coordinates).
        //
        // Why this is acceptable:
        // 1. Consequence is benign: misplaced nodes are at stale positions anyway
        // 2. Next get_or_create() will recreate at correct position
        // 3. Atomic locking adds complexity without proportional benefit
        // 4. If bugs emerge, refactor to atomic locking (hold lock for entire loop)
        let mut all_invalidated = Vec::with_capacity(edits.len() * 2); // Heuristic: ~2 invalidations per edit
        for edit in edits.iter().rev() {
            all_invalidated.extend(self.apply_single_edit(uri, edit));
        }
        all_invalidated
    }

    /// Reconstruct individual edits from character-level diff.
    ///
    /// Returns edits in ascending position order. Caller MUST process in reverse.
    ///
    /// `TextDiff::from_chars()` yields one `Change` per code point, and `.len()`
    /// on the resulting single-char `&str` is its UTF-8 byte length — so byte
    /// offsets stay correct for multi-byte characters (emoji, CJK) without extra
    /// conversion.
    #[must_use]
    fn reconstruct_individual_edits(old_text: &str, new_text: &str) -> Vec<EditInfo> {
        use similar::{ChangeTag, TextDiff};

        let diff = TextDiff::from_chars(old_text, new_text);
        let mut edits = Vec::with_capacity(4); // Heuristic: typical edit count
        let mut current_edit: Option<(usize, usize, usize)> = None; // (start, old_end, inserted_len)
        let mut old_byte = 0;

        for change in diff.iter_all_changes() {
            match change.tag() {
                ChangeTag::Equal => {
                    if let Some((start, old_end, inserted_len)) = current_edit.take() {
                        edits.push(EditInfo::new(start, old_end, start + inserted_len));
                    }
                    old_byte += change.value().len();
                }
                ChangeTag::Delete | ChangeTag::Insert => {
                    let edit = current_edit.get_or_insert((old_byte, old_byte, 0));
                    if change.tag() == ChangeTag::Delete {
                        old_byte += change.value().len();
                        edit.1 = old_byte;
                    } else {
                        edit.2 += change.value().len();
                    }
                }
            }
        }

        if let Some((start, old_end, inserted_len)) = current_edit {
            edits.push(EditInfo::new(start, old_end, start + inserted_len));
        }

        // Sort for guaranteed invariant (similar crate empirically sorted, but undocumented)
        edits.sort_unstable_by_key(|e| e.start_byte);

        // NOTE: Non-overlapping invariant is validated at runtime in apply_text_diff()
        // with proper fallback handling. No duplicate check needed here.

        edits
    }

    /// Apply edits in array order, returning every invalidated ULID.
    ///
    /// No coordinate conversion is needed: LSP edits are in *running* coordinates
    /// (each relative to the doc state after previous edits) and `apply_single_edit`
    /// updates node positions after each call, so they stay aligned.
    ///
    /// Array order is mandatory — LSP does not guarantee ascending positions
    /// (VSCode sends multi-cursor edits bottom-to-top), and the spec requires
    /// applying events in receive order.
    pub(crate) fn apply_input_edits(&self, uri: &Url, edits: &[EditInfo]) -> Vec<Ulid> {
        let mut all_invalidated = Vec::new();

        for edit in edits {
            // Debug-only assertion: catch invalid edits early in development.
            // LSP implementations should never send old_end < start, so this
            // helps identify bugs in upstream code or test fixtures.
            debug_assert!(
                edit.old_end_byte >= edit.start_byte,
                "Invalid edit: old_end_byte ({}) < start_byte ({}). \
                 This indicates a bug in the LSP client or test setup.",
                edit.old_end_byte,
                edit.start_byte
            );

            // Defensive: skip invalid edits in production (graceful degradation)
            if edit.old_end_byte < edit.start_byte {
                warn!(
                    target: "kakehashi::node_tracker",
                    "Skipping invalid edit: old_end_byte ({}) < start_byte ({})",
                    edit.old_end_byte, edit.start_byte
                );
                continue;
            }

            // apply_single_edit updates tracker's internal positions
            // So next edit's running coords will match
            all_invalidated.extend(self.apply_single_edit(uri, edit));
        }

        all_invalidated
    }

    /// START-priority invalidation (lazy-node-identity-tracking): invalidate when the node's START
    /// lies in the half-open `[edit.start, edit.old_end)`.
    ///
    /// For zero-length inserts the ADR compares old- vs new-tree START, but we
    /// have no tree access here, so we conservatively invalidate iff the insert
    /// lands exactly at the node's START. This may over-invalidate when the
    /// start happens to remain stable, but never preserves stale identity.
    fn should_invalidate_node(key: &PositionKey, edit: &EditInfo) -> bool {
        if edit.is_insertion_only() {
            // Zero-length insert: invalidate if insert is AT node's START
            key.start_byte == edit.start_byte
        } else {
            // Normal edit: invalidate if START is inside [edit.start, edit.old_end)
            key.start_byte >= edit.start_byte && key.start_byte < edit.old_end_byte
        }
    }

    /// Apply a single edit operation with START-priority invalidation (lazy-node-identity-tracking).
    ///
    /// Returns ULIDs that were invalidated by this edit (for Phase 3 cleanup).
    /// Both the forward and reverse indices for the URI are rebuilt in place,
    /// so a ULID returned in the invalidated list will no longer resolve via
    /// [`lookup_position`](Self::lookup_position).
    fn apply_single_edit(&self, uri: &Url, edit: &EditInfo) -> Vec<Ulid> {
        let delta = edit.delta();
        let mut invalidated = Vec::new();

        // The shift generation must advance even for a URI with no entries
        // yet, so a serve-stale walk's latch taken before its first mint
        // still detects this edit (the mint would land in this edit's
        // superseded coordinate space). `get_mut` first so the hot path
        // (index exists) skips the `Url` clone `entry()` needs.
        let entries_hit = self.entries.get_mut(uri);
        let mut entries = match entries_hit {
            Some(entries) => entries,
            None => self.entries.entry(uri.clone()).or_default(),
        };

        // Pre-size new index: most edits don't invalidate, so the current count
        // is a good estimate of the surviving entries.
        let mut new_entries = UriEntries {
            forward: HashMap::with_capacity(entries.len()),
            reverse: HashMap::with_capacity(entries.len()),
            latest_incarnation: entries.latest_incarnation,
            shift_gen: entries.shift_gen + 1,
            named_layers: std::mem::take(&mut entries.named_layers),
        };

        for (key, tracked) in entries.drain() {
            if Self::should_invalidate_node(&key, edit) {
                invalidated.push(tracked.ulid);
                continue; // INVALIDATE
            }

            // Position adjustment (returns None if range collapsed)
            let Some(new_key) = adjust_position_for_edit(key, edit, delta) else {
                invalidated.push(tracked.ulid);
                continue; // INVALIDATE: range collapsed
            };

            // Handle potential position collision after adjustment
            // Two nodes may collapse to same (start, end, kind) after large edits
            //
            // Policy: "first wins" - whichever ULID is encountered first during
            // HashMap iteration is kept. Since HashMap order is non-deterministic,
            // the surviving ULID is arbitrary in collision cases.
            //
            // This is acceptable because:
            // 1. Collisions are rare (require extreme edits like massive deletions)
            // 2. Either ULID is equally valid for the resulting position
            // 3. Collisions may indicate a bug in invalidation logic anyway
            //
            // Log at warn level for observability and debugging.
            if let Err(_existing) = new_entries.insert(new_key, tracked) {
                warn!(
                    target: "kakehashi::node_tracker",
                    "Position collision after edit - ULID mapping dropped: ulid={}, start={}, end={}, kind={}, layer={}",
                    tracked.ulid, new_key.start_byte, new_key.end_byte, new_key.kind, new_key.layer
                );
                // Note: Collided ULID is also invalidated (both nodes can't coexist)
                invalidated.push(tracked.ulid);
            }
        }

        *entries = new_entries;
        invalidated
    }

    /// Remove tracked nodes minted by the closing document lifetime.
    ///
    /// Each bidirectional entry carries the document incarnation that minted
    /// it. A raced reopen may have already minted newer entries before this
    /// didClose reaches cleanup, so cleanup retains only entries whose
    /// incarnation is newer than `closing_incarnation`. This simultaneously
    /// invalidates every pre-close id and preserves every published reopened id.
    /// When no newer entries remain, the whole URI index is removed.
    /// The lifecycle map keeps a lightweight close tombstone so a direct mint
    /// that passed its reader-side check before didClose cannot recreate the
    /// just-removed lifetime afterward; didOpen replaces it with the new
    /// incarnation before that lifetime starts minting.
    ///
    /// The global epoch bump runs BEFORE retention, and the order is
    /// load-bearing: a
    /// latch check that passes reads the pre-bump epoch, so it strictly
    /// precedes this close in epoch order, and whatever it minted is wiped
    /// by the retention that follows. Retaining FIRST would open a gap where a pre-close latch
    /// `(0, E)` re-materializes the just-removed entry (`or_default`,
    /// `shift_gen` back at 0) and passes against the not-yet-bumped epoch,
    /// leaving stale-lifetime coordinates alive after cleanup.
    ///
    /// Memory-ordering note (why `Release` on the bump suffices, no
    /// `SeqCst` needed): the only way another thread can OBSERVE the
    /// retention is through the entry's shard lock (`get_mut` /
    /// `entry().or_default()`), and that lock's release/acquire pair makes
    /// every write sequenced before retention — including this bump —
    /// visible to the observer, so a check that finds the entry gone always
    /// reads the bumped epoch and refuses. A check that instead wins the
    /// shard lock BEFORE retention may still read the pre-bump epoch and
    /// pass, but it then minted into the still-present entry, which the
    /// retention (queued behind that same lock) removes it.
    pub(crate) fn cleanup(&self, uri: &Url, closing_incarnation: u64) {
        self.cleanup_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.incarnations
            .entry(uri.clone())
            .and_modify(|current| {
                if *current == Some(closing_incarnation) {
                    *current = None;
                }
            })
            .or_insert(None);
        self.entries.remove_if_mut(uri, |_, entries| {
            entries.retain_newer_than(closing_incarnation);
            entries.len() == 0 && entries.named_layers.is_empty()
        });
    }

    /// Run `commit` only if `uri`'s (shift generation, cleanup epoch) still
    /// equals `expected` — checked while HOLDING the tracker entry's
    /// EXCLUSIVE lock, which coordinate shifts (`apply_*_edits`) and
    /// `cleanup` also acquire: a commit that passes the check therefore
    /// serializes strictly before any concurrent shift, closing the
    /// check-to-commit window a bare re-read would leave. The entry is
    /// materialized (`or_default`) for a URI that never minted, so
    /// absent-entry commits serialize with a concurrent FIRST edit too; an
    /// entry materialized only to fail the check stays behind as an empty
    /// index (bounded: it takes a stale pass racing a close to produce one,
    /// and any later mint reuses it). Returns `None` without running
    /// `commit` on a mismatch.
    ///
    /// `commit` MUST NOT touch this tracker: it runs while this method holds
    /// the entry's exclusive lock, so any re-entrant access to the same URI
    /// (or shard) would self-deadlock.
    pub(crate) fn commit_if_unshifted<R>(
        &self,
        uri: &Url,
        expected: (u64, u64),
        incarnation: u64,
        commit: impl FnOnce() -> R,
    ) -> Option<R> {
        if !self.admits_incarnation(uri, incarnation) {
            return None;
        }
        let entry = self.entries.entry(uri.clone()).or_default();
        if !self.admits_incarnation(uri, incarnation) {
            drop(entry);
            self.remove_pristine_entry(uri);
            return None;
        }
        let epoch = self
            .cleanup_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if (entry.shift_gen, epoch) != expected {
            return None;
        }
        Some(commit())
    }

    /// Mint every `(start, end, kind, layer)` key — reusing an existing id
    /// by position, drawing a fresh ULID for a genuinely new key — under ONE
    /// acquisition of the URI's entry lock, but only if the (shift
    /// generation, cleanup epoch) latch still equals `expected`.
    ///
    /// This is the mint half of the parse-snapshot ADR §3 **reconciliation
    /// step**: the latch is checked while HOLDING the entry's exclusive lock
    /// — the same lock coordinate shifts (`apply_*_edits`) and `cleanup`
    /// acquire — so a batch that passes serializes strictly before any
    /// concurrent shift. Every id it mints is therefore *correct-at-birth*
    /// (its coordinates match the live text at mint time; later edits shift
    /// it like any live entry), which is why callers need no post-pass purge
    /// set: a refused batch (`None`) minted **nothing** — the stale pass
    /// simply defers identity to the next current pass. Correct-at-birth is
    /// **relative to the latch**: the batch only proves no shift happened
    /// since `expected` was read, so callers must take it (via
    /// [`mint_epoch`](Self::mint_epoch)) at or before validating that the
    /// coordinates they are about to mint belong to the live text.
    ///
    /// `keys` is consumed while this method holds the entry's exclusive
    /// lock — like `commit_if_unshifted`'s closure, it MUST NOT touch this
    /// tracker (a re-entrant access to the same URI or shard would
    /// self-deadlock). An entry materialized only to fail the check stays
    /// behind as an empty index — the same bounded residue
    /// [`commit_if_unshifted`](Self::commit_if_unshifted) documents.
    ///
    /// Returns the ids aligned with `keys`' iteration order.
    #[cfg(test)]
    pub(crate) fn mint_batch_if_unshifted(
        &self,
        uri: &Url,
        expected: (u64, u64),
        keys: impl IntoIterator<Item = (usize, usize, &'static str, usize)>,
    ) -> Option<Vec<Ulid>> {
        self.mint_batch_if_unshifted_for_incarnation(uri, expected, 0, keys)
    }

    pub(crate) fn mint_batch_if_unshifted_for_incarnation(
        &self,
        uri: &Url,
        expected: (u64, u64),
        incarnation: u64,
        keys: impl IntoIterator<Item = (usize, usize, &'static str, usize)>,
    ) -> Option<Vec<Ulid>> {
        // Avoid leaving an empty `entries` slot for a known-stale batch. The
        // under-lock checks below remain load-bearing against a raced close.
        if !self.admits_incarnation(uri, incarnation) {
            return None;
        }
        // `get_mut` first: the hot path (index already exists — every batch
        // after a document's first) avoids the `Url` clone `entry()` needs
        // for its owned key. The absent-entry `entry().or_default()`
        // fallback stays load-bearing: it serializes the latch check with a
        // concurrent FIRST edit on a never-minted URI.
        if let Some(mut entry) = self.entries.get_mut(uri) {
            if !self.admits_incarnation(uri, incarnation) {
                drop(entry);
                self.remove_pristine_entry(uri);
                return None;
            }
            return self.mint_batch_in_entry(&mut entry, expected, incarnation, keys);
        }
        let mut entry = self.entries.entry(uri.clone()).or_default();
        if !self.admits_incarnation(uri, incarnation) {
            drop(entry);
            self.remove_pristine_entry(uri);
            return None;
        }
        self.mint_batch_in_entry(&mut entry, expected, incarnation, keys)
    }

    /// The latch check + mint body of
    /// [`mint_batch_if_unshifted`](Self::mint_batch_if_unshifted), run while
    /// the caller holds `entry`'s exclusive lock.
    fn mint_batch_in_entry(
        &self,
        entry: &mut UriEntries,
        expected: (u64, u64),
        incarnation: u64,
        keys: impl IntoIterator<Item = (usize, usize, &'static str, usize)>,
    ) -> Option<Vec<Ulid>> {
        let epoch = self
            .cleanup_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if (entry.shift_gen, epoch) != expected {
            return None;
        }
        keys.into_iter()
            .map(|(start, end, kind, layer)| {
                entry.get_or_insert(PositionKey::new(start, end, kind, layer), incarnation)
            })
            .collect()
    }

    /// The (per-URI shift generation, global cleanup epoch) pair a pass
    /// latches at entry and hands to
    /// [`mint_batch_if_unshifted`](Self::mint_batch_if_unshifted): a
    /// mismatch in either half means the pass's coordinates were superseded
    /// (an edit shifted the index, or a close/reopen replaced it) and the
    /// batch refuses to mint.
    ///
    /// The two halves are read WITHOUT mutual synchronization, so a
    /// close+reopen completing between them yields a mixed
    /// `(old_gen, new_epoch)` latch that can match the reopened index.
    /// Callers therefore must latch FIRST and validate the pass's
    /// liveness/currency (incarnation, snapshot version) AFTER — only a
    /// post-latch check is guaranteed to observe the lifetime change that
    /// produced the mixed latch and bail before minting under it.
    pub(crate) fn mint_epoch(&self, uri: &Url) -> (u64, u64) {
        (
            self.shift_generation(uri),
            self.cleanup_epoch
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }
}

impl Default for NodeTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///test/{}.md", name)).unwrap()
    }

    #[test]
    fn stale_cleanup_preserves_newer_named_layers() {
        let tracker = NodeTracker::new();
        let uri = test_uri("named_layer_cleanup");
        tracker.open_incarnation(&uri, 2);
        let javascript = tracker
            .named_layer_for_incarnation(&uri, 0, "javascript", 2)
            .unwrap();

        tracker.cleanup(&uri, 1);

        assert_eq!(
            tracker.named_layer_for_incarnation(&uri, 0, "javascript", 2),
            Some(javascript)
        );
        assert_ne!(
            tracker.named_layer_for_incarnation(&uri, 0, "typescript", 2),
            Some(javascript)
        );
    }

    /// The batch reconciliation mint reuses an existing id by position and
    /// mints fresh ids for genuinely new keys — all under one entry lock,
    /// gated on the latch (parse-snapshot ADR §3 reconciliation).
    #[test]
    fn batch_mint_reuses_by_position_and_mints_new_under_one_latch() {
        let tracker = NodeTracker::new();
        let uri = test_uri("batch_mint");
        let existing = tracker.get_or_create_in_layer(&uri, 0, 4, "word", 0);
        let latch = tracker.mint_epoch(&uri);
        let ulids = tracker
            .mint_batch_if_unshifted(&uri, latch, [(0, 4, "word", 0), (10, 14, "word", 1)])
            .expect("an unshifted latch mints");
        assert_eq!(ulids[0], existing, "existing position reuses its id");
        assert_eq!(
            tracker.lookup_in_layer(&uri, 10, 14, "word", 1),
            Some(ulids[1]),
            "new position minted live"
        );
    }

    /// A latch an edit has shifted refuses the whole batch — nothing is
    /// minted (the stale pass defers to the next current pass; there is no
    /// purge because there is nothing to purge).
    #[test]
    fn batch_mint_refuses_shifted_latch_and_mints_nothing() {
        let tracker = NodeTracker::new();
        let uri = test_uri("batch_mint_shifted");
        let latch = tracker.mint_epoch(&uri);
        let _ = tracker.apply_input_edits(&uri, &[EditInfo::new(0, 0, 3)]);
        assert_eq!(
            tracker.mint_batch_if_unshifted(&uri, latch, [(0, 4, "word", 0)]),
            None,
            "a shifted latch refuses"
        );
        assert_eq!(
            tracker.lookup_in_layer(&uri, 0, 4, "word", 0),
            None,
            "the refused batch minted nothing"
        );
    }

    /// A close raced by a completed reopen (the probe sees the document
    /// open again) keeps the reopened lifetime's index — its freshly minted
    /// ids must survive — while the epoch bump still refuses any
    /// old-lifetime latch.
    #[test]
    fn cleanup_invalidates_closed_incarnation_but_keeps_reopened_entries() {
        let tracker = NodeTracker::new();
        let uri = test_uri("cleanup_reopen");
        let old_id = tracker
            .get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 1)
            .unwrap();
        let pre_close_latch = tracker.mint_epoch(&uri);
        let reopened_id = tracker
            .get_or_create_in_layer_for_incarnation(&uri, 10, 14, "word", 0, 2)
            .unwrap();

        tracker.cleanup(&uri, 1);

        assert_eq!(
            tracker.lookup_node(&uri, &old_id),
            None,
            "the closed lifetime's id is invalidated"
        );
        assert_eq!(
            tracker.lookup_node(&uri, &reopened_id),
            Some((10, 14, "word", 0, 2)),
            "the reopened lifetime's id survives the raced close"
        );
        assert_eq!(
            tracker.mint_batch_if_unshifted(&uri, pre_close_latch, [(10, 14, "word", 0)]),
            None,
            "the epoch bump still refuses latches taken before the close"
        );
    }

    #[test]
    fn reopened_incarnation_remints_same_position_with_a_fresh_id() {
        let tracker = NodeTracker::new();
        let uri = test_uri("same_position_reopen");
        let old_id = tracker
            .get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 1)
            .unwrap();
        let reopened_id = tracker
            .get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 2)
            .unwrap();

        assert_ne!(old_id, reopened_id);
        assert_eq!(tracker.lookup_node(&uri, &old_id), None);
        assert_eq!(
            tracker.lookup_node(&uri, &reopened_id),
            Some((0, 4, "word", 0, 2))
        );
    }

    #[test]
    fn stale_incarnation_cannot_evict_reopened_entry() {
        let tracker = NodeTracker::new();
        let uri = test_uri("stale_after_reopen");
        tracker.open_incarnation(&uri, 2);
        let reopened_id = tracker
            .get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 2)
            .unwrap();

        let stale_id = tracker.get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 1);

        assert_eq!(stale_id, None);
        assert_eq!(
            tracker.lookup_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 1),
            None,
            "an older lifetime cannot reuse the reopened lifetime's position ID"
        );
        assert_eq!(
            tracker.lookup_node(&uri, &reopened_id),
            Some((0, 4, "word", 0, 2))
        );
    }

    #[test]
    fn direct_mint_cannot_resurrect_closed_incarnation_after_cleanup() {
        let tracker = NodeTracker::new();
        let uri = test_uri("stale_after_cleanup");
        tracker.open_incarnation(&uri, 1);
        let stale_latch = tracker.mint_epoch(&uri);
        tracker.cleanup(&uri, 1);

        assert_eq!(
            tracker.commit_if_unshifted(&uri, stale_latch, 1, || "committed"),
            None
        );
        let stale_id = tracker.get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 1);
        assert_eq!(stale_id, None);
        assert!(
            tracker.entries.get(&uri).is_none(),
            "a denied stale mint must not materialize an empty node index"
        );

        tracker.open_incarnation(&uri, 2);
        let reopened_id = tracker
            .get_or_create_in_layer_for_incarnation(&uri, 0, 4, "word", 0, 2)
            .unwrap();
        assert_eq!(
            tracker.lookup_node(&uri, &reopened_id),
            Some((0, 4, "word", 0, 2))
        );
    }

    /// A close (any document's) bumps the global cleanup epoch, so a latch
    /// taken before it refuses — the close/reopen ABA guard the per-URI
    /// generation alone cannot provide.
    #[test]
    fn batch_mint_refuses_after_cleanup_epoch_bump() {
        let tracker = NodeTracker::new();
        let uri = test_uri("batch_mint_cleanup");
        let other = test_uri("batch_mint_other");
        let latch = tracker.mint_epoch(&uri);
        tracker.cleanup(&other, 0);
        assert_eq!(
            tracker.mint_batch_if_unshifted(&uri, latch, [(0, 4, "word", 0)]),
            None
        );
    }

    /// Edit application advances the shift generation — including on a URI
    /// with no entries yet — and a latch taken AFTER the edit still mints.
    /// (That a latch taken BEFORE the edit refuses is pinned separately by
    /// `batch_mint_refuses_shifted_latch_and_mints_nothing`.)
    #[test]
    fn shift_generation_advances_on_every_edit_even_without_entries() {
        let tracker = NodeTracker::new();
        let uri = test_uri("shift_gen");
        assert_eq!(tracker.shift_generation(&uri), 0);

        // Edit on an entry-less URI still bumps the generation.
        let _ = tracker.apply_input_edits(&uri, &[EditInfo::new(0, 0, 3)]);
        assert_eq!(
            tracker.shift_generation(&uri),
            1,
            "the latch-before-first-mint case must detect this edit"
        );

        let post_edit_latch = tracker.mint_epoch(&uri);
        assert!(
            tracker
                .mint_batch_if_unshifted(&uri, post_edit_latch, [(10, 14, "word", 0)])
                .is_some(),
            "a latch taken after the edit mints"
        );

        let _ = tracker.apply_text_diff(&uri, "aaaa aaaa aaaa", "aaaa aaaa bbbb aaaa");
        assert!(
            tracker.shift_generation(&uri) > 1,
            "text-diff application advances the generation too"
        );

        // Close/reopen must not ABA the latch: cleanup removes the per-URI
        // index (its generation restarts at 0) but bumps the GLOBAL cleanup
        // epoch, so a latch taken before the close can never compare equal
        // to a post-reopen observation — including for a URI that never
        // tracked a node.
        let (_, epoch_before) = tracker.mint_epoch(&uri);
        tracker.cleanup(&uri, 0);
        let (gen_after, epoch_after) = tracker.mint_epoch(&uri);
        assert_eq!(gen_after, 0, "the fresh index restarts at generation 0");
        assert!(
            epoch_after > epoch_before,
            "cleanup bumps the global epoch so the (gen, epoch) latch cannot ABA"
        );
    }

    /// Produce a `&'static str` kind for a numeric index by intentionally
    /// leaking a short formatted string. Used in tests that need distinct
    /// dynamic kinds; tree-sitter's real grammars already provide static
    /// interned kinds, so production code never goes through this path.
    fn leak_kind(i: usize) -> &'static str {
        Box::leak(format!("{}", i).into_boxed_str())
    }

    #[test]
    fn test_new_tracker_is_empty() {
        let tracker = NodeTracker::new();
        // No direct way to check emptiness, but get_or_create should work
        let uri = test_uri("empty");
        let ulid = tracker.get_or_create(&uri, 0, 10, "code_block");
        assert!(!ulid.to_string().is_empty(), "ULID should be generated");
    }

    #[test]
    fn test_same_position_returns_same_ulid() {
        let tracker = NodeTracker::new();
        let uri = test_uri("same");

        let ulid1 = tracker.get_or_create(&uri, 0, 10, "code_block");
        let ulid2 = tracker.get_or_create(&uri, 0, 10, "code_block");

        assert_eq!(ulid1, ulid2, "Same position key should return same ULID");
    }

    #[test]
    fn test_different_start_returns_different_ulid() {
        let tracker = NodeTracker::new();
        let uri = test_uri("start");

        let ulid0 = tracker.get_or_create(&uri, 0, 10, "code_block");
        let ulid1 = tracker.get_or_create(&uri, 10, 20, "code_block");

        assert_ne!(
            ulid0, ulid1,
            "Different start positions should return different ULIDs"
        );
    }

    #[test]
    fn test_different_kind_returns_different_ulid() {
        let tracker = NodeTracker::new();
        let uri = test_uri("kind");

        let block_ulid = tracker.get_or_create(&uri, 0, 10, "code_block");
        let fence_ulid = tracker.get_or_create(&uri, 0, 10, "fenced_code_block");

        assert_ne!(
            block_ulid, fence_ulid,
            "Different kinds should return different ULIDs"
        );
    }

    #[test]
    fn test_different_layer_returns_different_ulid() {
        // Issue #313: recursive same-language injection can place a node with an
        // identical (start, end, kind) in both the host and an injected layer.
        // The `layer` discriminator must keep their ULIDs distinct so navigation
        // can resolve each in the correct language tree.
        let tracker = NodeTracker::new();
        let uri = test_uri("layer");

        let host_ulid = tracker.get_or_create_in_layer(&uri, 0, 10, "paragraph", 0);
        let injected_ulid = tracker.get_or_create_in_layer(&uri, 0, 10, "paragraph", 1);

        assert_ne!(
            host_ulid, injected_ulid,
            "Same (start, end, kind) at different layers should return different ULIDs"
        );
    }

    #[test]
    fn test_same_layer_returns_same_ulid() {
        let tracker = NodeTracker::new();
        let uri = test_uri("layer-stable");

        let a = tracker.get_or_create_in_layer(&uri, 0, 10, "paragraph", 2);
        let b = tracker.get_or_create_in_layer(&uri, 0, 10, "paragraph", 2);

        assert_eq!(a, b, "Same key including layer should return the same ULID");
    }

    #[test]
    fn test_get_or_create_defaults_to_host_layer() {
        // The plain `get_or_create` convenience must mint at layer 0, matching a
        // node explicitly created in the host layer.
        let tracker = NodeTracker::new();
        let uri = test_uri("layer-default");

        let default_ulid = tracker.get_or_create(&uri, 0, 10, "paragraph");
        let host_ulid = tracker.get_or_create_in_layer(&uri, 0, 10, "paragraph", 0);

        assert_eq!(
            default_ulid, host_ulid,
            "get_or_create should be equivalent to layer 0"
        );
    }

    #[test]
    fn test_lookup_node_returns_layer() {
        let tracker = NodeTracker::new();
        let uri = test_uri("lookup-layer");

        let ulid = tracker.get_or_create_in_layer(&uri, 5, 15, "block", 3);

        assert_eq!(
            tracker.lookup_node(&uri, &ulid),
            Some((5, 15, "block", 3, 0)),
            "lookup_node should round-trip the layer discriminator"
        );
        assert_eq!(
            tracker.lookup_position(&uri, &ulid),
            Some((5, 15, "block", 0)),
            "lookup_position should drop the layer but retain incarnation"
        );
    }

    #[test]
    fn test_different_uri_returns_different_ulid() {
        let tracker = NodeTracker::new();
        let uri1 = test_uri("doc1");
        let uri2 = test_uri("doc2");

        let ulid1 = tracker.get_or_create(&uri1, 0, 10, "code_block");
        let ulid2 = tracker.get_or_create(&uri2, 0, 10, "code_block");

        assert_ne!(ulid1, ulid2, "Different URIs should return different ULIDs");
    }

    #[test]
    fn test_cleanup_removes_document_entries() {
        let tracker = NodeTracker::new();
        let uri = test_uri("cleanup");

        // Create some entries
        let ulid_before = tracker.get_or_create(&uri, 0, 10, "code_block");

        // Cleanup
        tracker.cleanup(&uri, 0);

        // After cleanup, same key should create NEW ULID
        tracker.open_incarnation(&uri, 0);
        let ulid_after = tracker.get_or_create(&uri, 0, 10, "code_block");

        assert_ne!(
            ulid_before, ulid_after,
            "After cleanup, new ULID should be generated"
        );
    }

    #[test]
    fn test_cleanup_does_not_affect_other_documents() {
        let tracker = NodeTracker::new();
        let uri1 = test_uri("keep");
        let uri2 = test_uri("remove");

        // Create entries for both documents
        let ulid1_before = tracker.get_or_create(&uri1, 0, 10, "code_block");
        let _ulid2 = tracker.get_or_create(&uri2, 0, 10, "code_block");

        // Cleanup only uri2
        tracker.cleanup(&uri2, 0);

        // uri1 should still have its ULID
        let ulid1_after = tracker.get_or_create(&uri1, 0, 10, "code_block");
        assert_eq!(
            ulid1_before, ulid1_after,
            "Cleanup should not affect other documents"
        );
    }

    #[test]
    fn test_lookup_position_resolves_known_ulid() {
        // Reverse index: get_or_create should make the ULID resolvable.
        let tracker = NodeTracker::new();
        let uri = test_uri("reverse_known");

        let ulid = tracker.get_or_create(&uri, 30, 50, "block");

        assert_eq!(
            tracker.lookup_position(&uri, &ulid),
            Some((30, 50, "block", 0)),
            "Newly issued ULID must be resolvable via reverse index"
        );
    }

    #[test]
    fn test_lookup_position_returns_none_for_unknown_ulid() {
        let tracker = NodeTracker::new();
        let uri = test_uri("reverse_unknown");

        // No entries exist for this URI yet.
        let stray = Ulid::new();
        assert_eq!(tracker.lookup_position(&uri, &stray), None);

        // Adding an unrelated entry must not make the stray ULID resolvable.
        let _ = tracker.get_or_create(&uri, 0, 5, "block");
        assert_eq!(tracker.lookup_position(&uri, &stray), None);
    }

    #[test]
    fn test_lookup_position_returns_none_after_cleanup() {
        // didClose semantics: reverse index must be cleared together with forward index.
        let tracker = NodeTracker::new();
        let uri = test_uri("reverse_cleanup");

        let ulid = tracker.get_or_create(&uri, 10, 20, "block");
        assert!(tracker.lookup_position(&uri, &ulid).is_some());

        tracker.cleanup(&uri, 0);

        assert_eq!(
            tracker.lookup_position(&uri, &ulid),
            None,
            "cleanup() must remove reverse index entries"
        );
    }

    #[test]
    fn test_lookup_position_after_edit_reflects_adjusted_range() {
        // Reverse index must follow position adjustments performed by adjust_for_edits.
        let tracker = NodeTracker::new();
        let uri = test_uri("reverse_adjusted");

        let ulid = tracker.get_or_create(&uri, 60, 80, "block");

        // Delete [30, 35) before the node → shift by -5.
        let edits = vec![EditInfo::new(30, 35, 30)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);
        assert!(
            invalidated.is_empty(),
            "edit before node must not invalidate"
        );

        assert_eq!(
            tracker.lookup_position(&uri, &ulid),
            Some((55, 75, "block", 0)),
            "lookup_position should follow adjusted node range"
        );
    }

    #[test]
    fn test_lookup_position_returns_none_after_invalidation() {
        // After START-priority invalidation, the ULID becomes indistinguishable from
        // never-issued (lazy-node-identity-tracking no-tombstone rule, node-reference-protocol invalidate-vs-not-found).
        let tracker = NodeTracker::new();
        let uri = test_uri("reverse_invalidated");

        let ulid = tracker.get_or_create(&uri, 40, 60, "block");

        // Edit covers the node's START → invalidate.
        let edits = vec![EditInfo::new(35, 45, 35)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);
        assert!(
            invalidated.contains(&ulid),
            "edit covering START must invalidate"
        );

        assert_eq!(
            tracker.lookup_position(&uri, &ulid),
            None,
            "invalidated ULID must not be resolvable"
        );
    }

    #[test]
    fn test_ulid_format_is_valid() {
        let tracker = NodeTracker::new();
        let uri = test_uri("format");

        let ulid = tracker.get_or_create(&uri, 0, 10, "code_block");
        let ulid_str = ulid.to_string();

        // ULID is 26 characters, uppercase alphanumeric
        assert_eq!(ulid_str.len(), 26, "ULID should be 26 characters");
        assert!(
            ulid_str.chars().all(|c| c.is_ascii_alphanumeric()),
            "ULID should be alphanumeric"
        );
    }

    #[test]
    fn test_concurrent_get_or_create_returns_same_ulid() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = test_uri("concurrent");

        // Spawn 10 threads that all try to get_or_create the same key
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();
                thread::spawn(move || tracker.get_or_create(&uri, 0, 10, "code_block"))
            })
            .collect();

        // Collect all ULIDs
        let ulids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All ULIDs should be identical (thread-safe get-or-create)
        let first = &ulids[0];
        assert!(
            ulids.iter().all(|ulid| ulid == first),
            "All concurrent get_or_create calls should return the same ULID"
        );
    }

    #[test]
    fn test_concurrent_different_keys_returns_different_ulids() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = test_uri("concurrent_diff");

        // Spawn threads that get_or_create different positions
        let handles: Vec<_> = (0..5)
            .map(|offset| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();
                let start = offset * 10;
                thread::spawn(move || {
                    (
                        offset,
                        tracker.get_or_create(&uri, start, start + 10, "code_block"),
                    )
                })
            })
            .collect();

        // Collect all (offset, ULID) pairs
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // All ULIDs should be different from each other
        for i in 0..results.len() {
            for j in (i + 1)..results.len() {
                assert_ne!(
                    results[i].1, results[j].1,
                    "Different positions {} and {} should have different ULIDs",
                    results[i].0, results[j].0
                );
            }
        }
    }

    // ============================================================
    // Concurrent apply_text_diff() Tests
    // ============================================================
    // These tests verify thread-safety when multiple threads call
    // apply_text_diff() concurrently on the same URI.

    #[test]
    fn test_concurrent_apply_text_diff_same_uri_no_panic() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = test_uri("concurrent_edit");

        // Pre-populate with several nodes
        for i in 0..10 {
            let start = i * 20;
            tracker.get_or_create(&uri, start, start + 15, "block");
        }

        // Spawn multiple threads that apply different edits concurrently
        // Each thread applies a small edit at different positions
        let handles: Vec<_> = (0..5)
            .map(|thread_id| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();
                thread::spawn(move || {
                    // Each thread does multiple edit cycles
                    for cycle in 0..3 {
                        // Create text with edit at different positions per thread
                        let edit_pos = (thread_id * 30 + cycle * 5) % 150;
                        let old_text = text_with_markers(200);
                        let mut new_text = old_text.clone();

                        // Apply a small deletion
                        if edit_pos + 5 <= new_text.len() {
                            new_text.replace_range(edit_pos..edit_pos + 5, "");
                            tracker.apply_text_diff(&uri, &old_text, &new_text);
                        }
                    }
                })
            })
            .collect();

        // Wait for all threads to complete - no panics should occur
        for handle in handles {
            handle
                .join()
                .expect("Thread should not panic during concurrent edits");
        }

        // Verify tracker is still functional after concurrent edits
        // (create a new entry to ensure no corruption)
        let new_ulid = tracker.get_or_create(&uri, 1000, 1010, "test");
        assert!(
            new_ulid.to_string().len() == 26,
            "ULID should be valid after concurrent edits"
        );
    }

    #[test]
    fn test_concurrent_apply_text_diff_different_uris_independent() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());

        // Each thread works on its own URI
        let handles: Vec<_> = (0..5)
            .map(|thread_id| {
                let tracker = Arc::clone(&tracker);
                let uri = test_uri(&format!("uri_{}", thread_id));

                thread::spawn(move || {
                    // Create initial node
                    let ulid = tracker.get_or_create(&uri, 50, 100, "block");

                    // Apply an edit that shifts the node
                    let old_text = text_with_markers(200);
                    let mut new_text = old_text.clone();
                    new_text.replace_range(20..30, ""); // Delete before node, delta = -10

                    tracker.apply_text_diff(&uri, &old_text, &new_text);

                    // Verify the node was shifted correctly
                    let shifted_ulid = tracker.get(&uri, 40, 90, "block");
                    (ulid, shifted_ulid)
                })
            })
            .collect();

        // Collect results and verify each URI maintained correct state
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        for (original, shifted) in results {
            assert_eq!(
                Some(original),
                shifted,
                "Each URI should independently maintain correct position adjustment"
            );
        }
    }

    #[test]
    fn test_concurrent_get_and_apply_interleaved() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = test_uri("interleaved");

        // Pre-populate with one stable node that won't be affected by edits
        // Node at [0, 10) - edits will be at [50+)
        let stable_ulid = tracker.get_or_create(&uri, 0, 10, "stable");

        // Spawn threads that interleave get_or_create and apply_text_diff
        let handles: Vec<_> = (0..4)
            .map(|thread_id| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();

                thread::spawn(move || {
                    let mut results = Vec::new();

                    for i in 0..5 {
                        // Alternate between creating new entries and applying edits
                        if i % 2 == 0 {
                            // Create new entry at high position (won't conflict with edits)
                            let pos = 500 + thread_id * 100 + i * 10;
                            tracker.get_or_create(&uri, pos, pos + 5, "dynamic");
                        } else {
                            // Apply edit at mid-range (affects nodes at [50, 100))
                            let old_text = text_with_markers(200);
                            let mut new_text = old_text.clone();
                            let edit_start = 60 + thread_id * 5;
                            if edit_start + 5 <= new_text.len() {
                                new_text.replace_range(edit_start..edit_start + 5, "");
                                tracker.apply_text_diff(&uri, &old_text, &new_text);
                            }
                        }

                        // Always verify stable node is accessible
                        // (might be accessed during concurrent edits elsewhere)
                        if let Some(ulid) = tracker.get(&uri, 0, 10, "stable") {
                            results.push(ulid);
                        }
                    }

                    // Verify stable node's ULID was consistent across all reads
                    (stable_ulid, results)
                })
            })
            .collect();

        // Verify all threads saw consistent stable ULID
        for handle in handles {
            let (expected, observed) = handle.join().expect("Thread should not panic");
            for ulid in observed {
                assert_eq!(
                    expected, ulid,
                    "Stable node should have consistent ULID across concurrent access"
                );
            }
        }
    }

    // ============================================================
    // Phase 2 Tests: lazy-node-identity-tracking START-Priority Invalidation
    // ============================================================

    /// Helper to create test text with unique characters at each position
    /// This ensures diff algorithms can correctly identify edits
    fn text_with_markers(size: usize) -> String {
        (0..size)
            .map(|i| {
                // Cycle through printable ASCII characters (33-126)
                char::from_u32(33 + (i % 94) as u32).unwrap()
            })
            .collect()
    }

    #[test]
    fn test_node_a_start_before_edit_end_after_keeps_ulid_adjusts_end() {
        // lazy-node-identity-tracking Node A: Node START before edit, END after edit → KEEP (adjust end)
        let tracker = NodeTracker::new();
        let uri = test_uri("node_a");

        // Create node at [30, 50)
        let ulid_original = tracker.get_or_create(&uri, 30, 50, "block");

        // Edit inside the node: [35, 40) → delete 5 bytes (delta: -5)
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(35..40, ""); // Delete 5 bytes

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node should:
        // - START unchanged (30 not in [35, 40))
        // - END adjusted: 50 + (-5) = 45
        // - ULID preserved at adjusted position [30, 45)
        let ulid_after = tracker.get(&uri, 30, 45, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node A should preserve ULID at adjusted position [30, 45)"
        );

        // Verify old position no longer exists
        assert_eq!(
            tracker.get(&uri, 30, 50, "block"),
            None,
            "Old position [30, 50) should be removed"
        );
    }

    #[test]
    fn test_node_b_start_before_edit_end_absorbed_keeps_ulid() {
        // lazy-node-identity-tracking Node B: Node START before edit, END absorbed/overlaps with edit → KEEP
        //
        // Proper Node B case: Edit partially overlaps node's end region.
        // With end clamping, the end is clamped to edit.new_end_byte.
        //
        // Example: Node [20, 50), edit deletes [40, 60)
        // - START 20 is NOT in [40, 60) → KEEP
        // - END 50 is inside [40, 60) → clamp to edit.new_end_byte = 40
        // - New node: [20, 40) - valid range, ULID preserved
        let tracker = NodeTracker::new();
        let uri = test_uri("node_b");

        // Create node at [20, 50)
        let ulid_original = tracker.get_or_create(&uri, 20, 50, "block");

        // Edit overlaps end: [40, 60) → delete 20 bytes
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(40..60, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node should:
        // - START unchanged (20 not in [40, 60))
        // - END clamped to edit.new_end_byte = 40 (since 50 is inside [40, 60))
        // - Range [20, 40) is valid (40 > 20), so ULID preserved
        let ulid_after = tracker.get(&uri, 20, 40, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node B should preserve ULID at clamped position [20, 40)"
        );

        // Verify old position no longer exists
        assert_eq!(
            tracker.get(&uri, 20, 50, "block"),
            None,
            "Old position [20, 50) should be removed"
        );
    }

    #[test]
    fn test_node_b_end_exactly_at_edit_end_keeps_ulid() {
        // lazy-node-identity-tracking Node B variant: Node END exactly at edit end → KEEP
        let tracker = NodeTracker::new();
        let uri = test_uri("node_b_exact");

        // Create node at [30, 50)
        let ulid_original = tracker.get_or_create(&uri, 30, 50, "block");

        // Edit ends exactly where node ends: [40, 50) → delete 10 bytes
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(40..50, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node should:
        // - START unchanged (30 not in [40, 50))
        // - END adjusted: 50 + (-10) = 40
        // - ULID preserved at adjusted position [30, 40)
        let ulid_after = tracker.get(&uri, 30, 40, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node B should preserve ULID at adjusted position [30, 40)"
        );

        // Verify old position no longer exists
        assert_eq!(
            tracker.get(&uri, 30, 50, "block"),
            None,
            "Old position [30, 50) should be removed"
        );
    }

    #[test]
    fn test_node_c_start_inside_edit_invalidates() {
        // lazy-node-identity-tracking Node C: Node START inside edit range → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("node_c");

        // Create node at [40, 60)
        let ulid_original = tracker.get_or_create(&uri, 40, 60, "block");

        // Edit overlaps start: [35, 45) → delete 10 bytes
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(35..45, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node should be INVALIDATED (START 40 is in [35, 45))
        // Try to get with adjusted position [35, 50) - should return NEW ULID
        let ulid_after = tracker.get_or_create(&uri, 35, 50, "block");
        assert_ne!(
            ulid_original, ulid_after,
            "Node C should invalidate when START is inside edit range"
        );
    }

    #[test]
    fn test_node_d_fully_inside_edit_invalidates() {
        // lazy-node-identity-tracking Node D: Node fully inside edit → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("node_d");

        // Create node at [40, 45)
        let ulid_original = tracker.get_or_create(&uri, 40, 45, "block");

        // Edit contains entire node: [35, 50) → delete 15 bytes
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(35..50, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node should be INVALIDATED (START 40 is in [35, 50))
        let ulid_after = tracker.get_or_create(&uri, 35, 35, "block");
        assert_ne!(
            ulid_original, ulid_after,
            "Node D should invalidate when fully inside edit range"
        );
    }

    #[test]
    fn test_node_e_after_edit_keeps_ulid_shifts_position() {
        // lazy-node-identity-tracking Node E: Node after edit → KEEP (shift position)
        let tracker = NodeTracker::new();
        let uri = test_uri("node_e");

        // Create node at [60, 80)
        let ulid_original = tracker.get_or_create(&uri, 60, 80, "block");

        // Edit before node: [30, 35) → delete 5 bytes (delta: -5)
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(30..35, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node should:
        // - START shifted: 60 + (-5) = 55
        // - END shifted: 80 + (-5) = 75
        // - ULID preserved at shifted position [55, 75)
        let ulid_after = tracker.get(&uri, 55, 75, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node E should preserve ULID at shifted position [55, 75)"
        );

        // Verify old position no longer exists
        assert_eq!(
            tracker.get(&uri, 60, 80, "block"),
            None,
            "Old position [60, 80) should be removed"
        );
    }

    #[test]
    fn test_node_f_before_edit_unchanged_keeps_ulid() {
        // lazy-node-identity-tracking Node F: Node before edit, no overlap → KEEP (unchanged)
        let tracker = NodeTracker::new();
        let uri = test_uri("node_f");

        // Create node at [10, 20)
        let ulid_original = tracker.get_or_create(&uri, 10, 20, "block");

        // Edit after node: [30, 35) → delete 5 bytes
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(30..35, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // After edit, node position unchanged
        let ulid_after = tracker.get_or_create(&uri, 10, 20, "block");
        assert_eq!(
            ulid_original, ulid_after,
            "Node F should preserve ULID and position when before edit with no overlap"
        );
    }

    #[test]
    fn test_boundary_start_at_edit_start_invalidates() {
        // Boundary condition: Node START exactly at edit.start (inclusive) → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("boundary_start");

        // Create node at [35, 50)
        let ulid_original = tracker.get_or_create(&uri, 35, 50, "block");

        // Edit starts exactly at node start: [35, 40)
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(35..40, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // START 35 is in [35, 40) (inclusive start) → INVALIDATE
        let ulid_after = tracker.get_or_create(&uri, 35, 45, "block");
        assert_ne!(
            ulid_original, ulid_after,
            "Node with START at edit.start should invalidate (inclusive boundary)"
        );
    }

    #[test]
    fn test_boundary_start_at_edit_old_end_keeps_ulid() {
        // Boundary condition: Node START exactly at edit.old_end (exclusive) → KEEP
        let tracker = NodeTracker::new();
        let uri = test_uri("boundary_end");

        // Create node at [40, 60)
        let ulid_original = tracker.get_or_create(&uri, 40, 60, "block");

        // Edit ends just before node: [30, 40)
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(30..40, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // START 40 is NOT in [30, 40) (exclusive end) → KEEP and shift
        // After edit deleting [30, 40), node shifts from [40, 60) to [30, 50)
        let ulid_after = tracker.get(&uri, 30, 50, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node with START at edit.old_end should keep ULID at shifted position [30, 50)"
        );

        // Verify old position no longer exists
        assert_eq!(
            tracker.get(&uri, 40, 60, "block"),
            None,
            "Old position [40, 60) should be removed"
        );
    }

    #[test]
    fn test_zero_length_insert_at_node_start_invalidates() {
        // Zero-length insert AT node START → INVALIDATE (conservative)
        let tracker = NodeTracker::new();
        let uri = test_uri("zero_at_start");

        // Create node at [40, 60)
        let ulid_original = tracker.get_or_create(&uri, 40, 60, "block");

        // Zero-length insert at node START: insert "abc" at position 40
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.insert_str(40, "abc"); // Insert without deleting

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // Conservative: invalidate because insert is AT START
        let ulid_after = tracker.get_or_create(&uri, 40, 63, "block");
        assert_ne!(
            ulid_original, ulid_after,
            "Zero-length insert at node START should invalidate (conservative)"
        );
    }

    #[test]
    fn test_zero_length_insert_before_node_keeps_ulid() {
        // Zero-length insert BEFORE node START → KEEP (shift)
        let tracker = NodeTracker::new();
        let uri = test_uri("zero_before");

        // Create node at [40, 60)
        let ulid_original = tracker.get_or_create(&uri, 40, 60, "block");

        // Zero-length insert before node: insert "abc" at position 30
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.insert_str(30, "abc");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // Node shifts: [43, 63)
        let ulid_after = tracker.get(&uri, 43, 63, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Zero-length insert before node START should keep ULID at shifted position [43, 63)"
        );

        // Verify old position no longer exists
        assert_eq!(
            tracker.get(&uri, 40, 60, "block"),
            None,
            "Old position [40, 60) should be removed"
        );
    }

    #[test]
    fn test_end_clamping_prevents_range_collapse() {
        // End is clamped to edit.new_end_byte so large deletes cannot make
        // end <= start, keeping the range valid for Node A/B cases.
        let tracker = NodeTracker::new();
        let uri = test_uri("collapse");

        // Create node at [30, 50)
        let _ulid_original = tracker.get_or_create(&uri, 30, 50, "block");

        // Large delete that includes node START: [20, 45) → delete 25 bytes
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(20..45, "");

        tracker.apply_text_diff(&uri, &old_text, &new_text);

        // Node at [30, 50): START 30 is in [20, 45) → INVALIDATED by START-priority

        // Test end clamping: Node where START survives but END would collapse without clamping
        let ulid2 = tracker.get_or_create(&uri, 10, 40, "block2");

        // Edit [20, 60): massive delete
        // Without clamping: END 40 adjusted to 40 + (20 - 60) = 0 → collapse
        // With clamping: END 40 is inside [20, 60) → clamp to new_end = 20 → [10, 20)
        let old_text2 = text_with_markers(100);
        let mut new_text2 = old_text2.clone();
        new_text2.replace_range(20..60, "");

        tracker.apply_text_diff(&uri, &old_text2, &new_text2);

        // Node at [10, 40): START 10 < 20 (not invalidated by START rule)
        // END 40 is inside [20, 60) → clamped to 20
        // New range: [10, 20) - valid, ULID preserved

        // ULID should be preserved at clamped position [10, 20)
        let ulid2_after = tracker.get(&uri, 10, 20, "block2");
        assert_eq!(
            Some(ulid2),
            ulid2_after,
            "End clamping should prevent range collapse, preserving ULID at [10, 20)"
        );
    }

    // ============================================================
    // UTF-8 Multi-byte Tests
    // ============================================================
    // These tests verify that position calculations correctly handle
    // multi-byte UTF-8 characters (emoji, CJK, etc.)

    #[test]
    fn test_utf8_multibyte_edit_before_node_shifts_correctly() {
        // Test: Delete multi-byte characters before a node
        // Emoji 🦀 is 4 bytes, so deleting it should shift by 4, not 1
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_before");

        // Text: "abc🦀def" where 🦀 is at bytes [3, 7)
        // Node at bytes [7, 10) covering "def"
        let old_text = "abc🦀def";
        assert_eq!(old_text.len(), 10); // 3 + 4 + 3 = 10 bytes

        let ulid_original = tracker.get_or_create(&uri, 7, 10, "block");

        // Delete the emoji: "abc🦀def" → "abcdef"
        let new_text = "abcdef";
        assert_eq!(new_text.len(), 6);

        tracker.apply_text_diff(&uri, old_text, new_text);

        // Node should shift from [7, 10) to [3, 6) (delta = -4 bytes)
        let ulid_after = tracker.get(&uri, 3, 6, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node should preserve ULID and shift by 4 bytes (emoji size)"
        );

        // Old position should be gone
        assert_eq!(
            tracker.get(&uri, 7, 10, "block"),
            None,
            "Old position [7, 10) should be removed"
        );
    }

    #[test]
    fn test_utf8_multibyte_edit_inside_node_adjusts_end() {
        // Test: Delete multi-byte characters inside a node
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_inside");

        // Text: "start日本語end" where:
        // - "start" at [0, 5)
        // - "日" at [5, 8) - 3 bytes
        // - "本" at [8, 11) - 3 bytes
        // - "語" at [11, 14) - 3 bytes
        // - "end" at [14, 17)
        let old_text = "start日本語end";
        assert_eq!(old_text.len(), 17); // 5 + 9 + 3 = 17 bytes

        // Node spans entire content [0, 17)
        let ulid_original = tracker.get_or_create(&uri, 0, 17, "block");

        // Delete "本" (bytes [8, 11)): "start日本語end" → "start日語end"
        let new_text = "start日語end";
        assert_eq!(new_text.len(), 14); // 17 - 3 = 14 bytes

        tracker.apply_text_diff(&uri, old_text, new_text);

        // Node should adjust end from 17 to 14 (delta = -3)
        // START 0 is not in [8, 11), so preserved
        let ulid_after = tracker.get(&uri, 0, 14, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node should preserve ULID with adjusted end (delta = -3 for 3-byte char)"
        );
    }

    #[test]
    fn test_utf8_multibyte_node_start_inside_edit_invalidates() {
        // Test: Node START falls inside edit range containing multi-byte chars
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_start_inside");

        // Text: "前🎉後text" where:
        // - "前" at [0, 3) - 3 bytes
        // - "🎉" at [3, 7) - 4 bytes
        // - "後" at [7, 10) - 3 bytes
        // - "text" at [10, 14)
        let old_text = "前🎉後text";
        assert_eq!(old_text.len(), 14);

        // Node at [7, 14) covering "後text"
        let ulid_original = tracker.get_or_create(&uri, 7, 14, "block");

        // Delete "🎉後" (bytes [3, 10)): "前🎉後text" → "前text"
        let new_text = "前text";
        assert_eq!(new_text.len(), 7); // 3 + 4 = 7 bytes

        tracker.apply_text_diff(&uri, old_text, new_text);

        // Node START 7 is in [3, 10) → INVALIDATED
        // Try to get at adjusted position - should be NEW ULID
        let ulid_after = tracker.get_or_create(&uri, 3, 7, "block");
        assert_ne!(
            ulid_original, ulid_after,
            "Node with START inside edit should be invalidated"
        );
    }

    #[test]
    fn test_utf8_insert_multibyte_shifts_correctly() {
        // Test: Insert multi-byte characters, verify shift
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_insert");

        // Text: "abcdef"
        let old_text = "abcdef";

        // Node at [3, 6) covering "def"
        let ulid_original = tracker.get_or_create(&uri, 3, 6, "block");

        // Insert emoji at position 2: "abcdef" → "ab🚀cdef"
        let new_text = "ab🚀cdef";
        assert_eq!(new_text.len(), 10); // 6 + 4 = 10 bytes

        tracker.apply_text_diff(&uri, old_text, new_text);

        // Node should shift from [3, 6) to [7, 10) (delta = +4)
        let ulid_after = tracker.get(&uri, 7, 10, "block");
        assert_eq!(
            Some(ulid_original),
            ulid_after,
            "Node should shift by 4 bytes when 4-byte emoji inserted before it"
        );
    }

    #[test]
    fn test_utf8_mixed_ascii_and_multibyte() {
        // Test: Complex edit with mixed ASCII and multi-byte
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_mixed");

        // Text: "Hello世界World" where:
        // - "Hello" at [0, 5)
        // - "世" at [5, 8) - 3 bytes
        // - "界" at [8, 11) - 3 bytes
        // - "World" at [11, 16)
        let old_text = "Hello世界World";
        assert_eq!(old_text.len(), 16);

        // Multiple nodes
        let ulid_hello = tracker.get_or_create(&uri, 0, 5, "greeting");
        let _ulid_cjk = tracker.get_or_create(&uri, 5, 11, "cjk");
        let ulid_world = tracker.get_or_create(&uri, 11, 16, "world");

        // Replace "世界" with "🌍" (6 bytes → 4 bytes, delta = -2)
        let new_text = "Hello🌍World";
        assert_eq!(new_text.len(), 14); // 5 + 4 + 5 = 14

        tracker.apply_text_diff(&uri, old_text, new_text);

        // "Hello" [0, 5): START 0 not in [5, 11) → KEEP unchanged
        assert_eq!(
            tracker.get(&uri, 0, 5, "greeting"),
            Some(ulid_hello),
            "Node before edit should be unchanged"
        );

        // "世界" [5, 11): START 5 is in [5, 11) → INVALIDATED
        assert_eq!(
            tracker.get(&uri, 5, 11, "cjk"),
            None,
            "Node with START inside edit should be invalidated"
        );

        // "World" [11, 16): START 11 >= 11 (edit.old_end) → KEEP and shift
        // New position: [11 + (-2), 16 + (-2)] = [9, 14)
        assert_eq!(
            tracker.get(&uri, 9, 14, "world"),
            Some(ulid_world),
            "Node after edit should shift by delta"
        );
    }

    // ========================================
    // Phase 3 Tests: Invalidated ULID Return Value
    // ========================================

    #[test]
    fn test_apply_text_diff_returns_invalidated_ulids() {
        // Phase 3: Verify apply_text_diff returns the invalidated ULIDs
        let tracker = NodeTracker::new();
        let uri = test_uri("phase3_return");

        // Create node at [40, 60) - will be invalidated
        let ulid_invalidated = tracker.get_or_create(&uri, 40, 60, "block");

        // Edit overlaps start: [35, 45) → invalidates [40, 60)
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(35..45, "");

        let invalidated = tracker.apply_text_diff(&uri, &old_text, &new_text);

        assert_eq!(
            invalidated.len(),
            1,
            "Should return exactly one invalidated ULID"
        );
        assert_eq!(
            invalidated[0], ulid_invalidated,
            "Returned ULID should match the invalidated node"
        );
    }

    #[test]
    fn test_apply_text_diff_returns_multiple_invalidated_ulids() {
        // Phase 3: Multiple nodes invalidated by a single edit
        let tracker = NodeTracker::new();
        let uri = test_uri("phase3_multiple");

        // Create multiple nodes that will be invalidated by overlapping start
        let ulid_1 = tracker.get_or_create(&uri, 40, 50, "block1");
        let ulid_2 = tracker.get_or_create(&uri, 42, 55, "block2");
        let ulid_3 = tracker.get_or_create(&uri, 70, 80, "block3"); // Not invalidated

        // Edit [35, 50) invalidates nodes with START in [35, 50)
        // ulid_1 at [40, 50): START 40 in [35, 50) → invalidated
        // ulid_2 at [42, 55): START 42 in [35, 50) → invalidated
        // ulid_3 at [70, 80): START 70 not in [35, 50) → kept (shifted)
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(35..50, "xxxxx"); // Replace 15 chars with 5

        let invalidated = tracker.apply_text_diff(&uri, &old_text, &new_text);

        assert_eq!(
            invalidated.len(),
            2,
            "Should return exactly two invalidated ULIDs"
        );
        assert!(
            invalidated.contains(&ulid_1),
            "Should contain first invalidated ULID"
        );
        assert!(
            invalidated.contains(&ulid_2),
            "Should contain second invalidated ULID"
        );
        assert!(
            !invalidated.contains(&ulid_3),
            "Should NOT contain kept ULID"
        );
    }

    #[test]
    fn test_apply_text_diff_returns_empty_when_no_invalidation() {
        // Phase 3: Edit that doesn't invalidate any node
        let tracker = NodeTracker::new();
        let uri = test_uri("phase3_no_invalidation");

        // Create node at [50, 60)
        let _ulid = tracker.get_or_create(&uri, 50, 60, "block");

        // Edit at [10, 15) - before the node, doesn't overlap START
        let old_text = text_with_markers(100);
        let mut new_text = old_text.clone();
        new_text.replace_range(10..15, "xxx");

        let invalidated = tracker.apply_text_diff(&uri, &old_text, &new_text);

        assert!(
            invalidated.is_empty(),
            "Should return empty when no nodes are invalidated"
        );
    }

    #[test]
    fn test_apply_text_diff_returns_empty_for_identical_texts() {
        // Phase 3: Fast path when texts are identical
        let tracker = NodeTracker::new();
        let uri = test_uri("phase3_identical");

        let _ulid = tracker.get_or_create(&uri, 10, 20, "block");

        let text = text_with_markers(50);
        let invalidated = tracker.apply_text_diff(&uri, &text, &text);

        assert!(
            invalidated.is_empty(),
            "Should return empty for identical texts (fast path)"
        );
    }

    #[test]
    fn test_apply_text_diff_returns_empty_for_unknown_uri() {
        // Phase 3: Unknown URI returns empty (no entries to invalidate)
        let tracker = NodeTracker::new();
        let uri = test_uri("phase3_unknown");

        let old_text = text_with_markers(50);
        let mut new_text = old_text.clone();
        new_text.replace_range(10..20, "");

        let invalidated = tracker.apply_text_diff(&uri, &old_text, &new_text);

        assert!(
            invalidated.is_empty(),
            "Should return empty for URI with no tracked entries"
        );
    }

    // ============================================================
    // Phase 4 Tests: apply_input_edits for Precise LSP Edit Processing
    // ============================================================
    // These tests verify the apply_input_edits method that processes
    // LSP InputEdits directly for precise invalidation.

    #[test]
    fn test_edit_info_from_input_edit_conversion() {
        // Verify From<&tree_sitter::InputEdit> correctly extracts byte positions.
        // This is the integration point used in lsp_impl.rs for incremental sync.
        use tree_sitter::{InputEdit, Point};

        let input_edit = InputEdit {
            start_byte: 100,
            old_end_byte: 150,
            new_end_byte: 120,
            // Position fields are not used by EditInfo::from
            start_position: Point::new(5, 10),
            old_end_position: Point::new(5, 60),
            new_end_position: Point::new(5, 30),
        };

        let edit_info = EditInfo::from(&input_edit);

        // Verify all byte fields are correctly extracted
        assert_eq!(
            edit_info,
            EditInfo::new(100, 150, 120),
            "From<&InputEdit> should extract start_byte, old_end_byte, new_end_byte"
        );

        // Verify delta calculation works correctly (deletion of 30 bytes)
        assert_eq!(
            edit_info.delta(),
            -30,
            "Delta should be new_end_byte - old_end_byte = 120 - 150 = -30"
        );
    }

    #[test]
    fn test_apply_input_edits_inside_node_keeps_with_adjusted_end() {
        // lazy-node-identity-tracking Node A case: Edit INSIDE node → KEEP (adjust end)
        // Node [10, 20), Edit [15, 18) → [15, 25)
        // START 10 NOT in [15, 18) → KEEP
        let tracker = NodeTracker::new();
        let uri = test_uri("edit_inside");

        let ulid = tracker.get_or_create(&uri, 10, 20, "block");

        let edits = vec![EditInfo::new(15, 18, 25)]; // delta = +7

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // Should NOT be invalidated (Node A case)
        assert!(
            invalidated.is_empty(),
            "Edit inside node should KEEP it, not invalidate"
        );

        // Verify ULID at adjusted position [10, 27)
        assert_eq!(
            tracker.get(&uri, 10, 27, "block"),
            Some(ulid),
            "Node should be at adjusted position [10, 27)"
        );
    }

    #[test]
    fn test_apply_input_edits_at_node_start_invalidates() {
        // Edit starts at node's START → INVALIDATE
        // Node [20, 40), Edit [20, 25) → [20, 30)
        // START 20 in [20, 25) → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("edit_at_start");

        let ulid = tracker.get_or_create(&uri, 20, 40, "block");

        let edits = vec![EditInfo::new(20, 25, 30)];

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.contains(&ulid),
            "Edit at node START should invalidate"
        );
    }

    #[test]
    fn test_apply_input_edits_exact_match_invalidates() {
        // Delete exactly matching node range → INVALIDATE
        // Node [30, 50), Edit [30, 50) delete all
        // START 30 in [30, 50) → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("exact_match");

        let ulid = tracker.get_or_create(&uri, 30, 50, "block");

        let edits = vec![EditInfo::new(30, 50, 30)]; // delete 20 bytes

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.contains(&ulid),
            "Edit exactly matching node should invalidate"
        );
    }

    #[test]
    fn test_apply_input_edits_before_node_shifts() {
        // Edit BEFORE node → KEEP and shift
        // Node [50, 70), Edit [20, 30) delete 10 bytes
        // START 50 NOT in [20, 30) → KEEP, shift to [40, 60)
        let tracker = NodeTracker::new();
        let uri = test_uri("edit_before");

        let ulid = tracker.get_or_create(&uri, 50, 70, "block");

        let edits = vec![EditInfo::new(20, 30, 20)]; // delete 10 bytes, delta = -10

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Edit before node should not invalidate"
        );

        // Node should shift from [50, 70) to [40, 60)
        assert_eq!(
            tracker.get(&uri, 40, 60, "block"),
            Some(ulid),
            "Node should shift to [40, 60)"
        );
    }

    #[test]
    fn test_apply_input_edits_node_at_edit_old_end_shifts() {
        // Boundary: Node START exactly at edit.old_end → KEEP (shift)
        // Node [50, 70), Edit [30, 50) delete
        // START 50 NOT in [30, 50) because interval is [30, 50) exclusive
        let tracker = NodeTracker::new();
        let uri = test_uri("at_old_end");

        let ulid = tracker.get_or_create(&uri, 50, 70, "block");

        let edits = vec![EditInfo::new(30, 50, 30)]; // delete [30, 50), delta = -20

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Node at edit.old_end (exclusive) should not invalidate"
        );

        // Node shifts from [50, 70) to [30, 50)
        assert_eq!(
            tracker.get(&uri, 30, 50, "block"),
            Some(ulid),
            "Node should shift to [30, 50)"
        );
    }

    #[test]
    fn test_apply_input_edits_multiple_sequential() {
        // Two edits in sequence, both shifting nodes
        //
        // Initial state:
        //   Node 1 [10, 20), Node 2 [50, 60)
        //
        // Edit 1: insert 5 bytes at [5, 5)
        //   - Zero-length insert at 5
        //   - Node 1 START 10 ≠ 5 → KEEP, shift to [15, 25)
        //   - Node 2 START 50 ≠ 5 → KEEP, shift to [55, 65)
        //
        // Edit 2: delete [60, 63) in running coords (3 bytes, delta = -3)
        //   - Node 1 at [15, 25): START 15 NOT in [60, 63) → KEEP unchanged
        //   - Node 2 at [55, 65): START 55 NOT in [60, 63) → KEEP
        //     - END 65 > 60, adjust end: 65 + (-3) = 62
        //     - Final: [55, 62)
        let tracker = NodeTracker::new();
        let uri = test_uri("multi_edit");

        let ulid1 = tracker.get_or_create(&uri, 10, 20, "block1");
        let ulid2 = tracker.get_or_create(&uri, 50, 60, "block2");

        let edits = vec![
            EditInfo::new(5, 5, 10),   // insert 5 bytes at position 5
            EditInfo::new(60, 63, 60), // delete 3 bytes (running coords after first edit)
        ];

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // Neither should be invalidated (edits don't touch STARTs)
        assert!(
            !invalidated.contains(&ulid1),
            "Node 1 should not be invalidated"
        );
        assert!(
            !invalidated.contains(&ulid2),
            "Node 2 should not be invalidated"
        );

        // Verify final positions
        assert_eq!(
            tracker.get(&uri, 15, 25, "block1"),
            Some(ulid1),
            "Node 1 should be at [15, 25) after Edit 1"
        );
        assert_eq!(
            tracker.get(&uri, 55, 62, "block2"),
            Some(ulid2),
            "Node 2 should be at [55, 62) after both edits"
        );
    }

    #[test]
    fn test_apply_input_edits_zero_length_insert_at_start_invalidates() {
        // lazy-node-identity-tracking: Zero-length insert AT node START → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("zero_at_start");

        let ulid = tracker.get_or_create(&uri, 20, 40, "block");

        let edits = vec![EditInfo::new(20, 20, 25)]; // Zero-length insert 5 bytes

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.contains(&ulid),
            "Zero-length insert at node START should invalidate"
        );
    }

    #[test]
    fn test_apply_input_edits_zero_length_insert_before_node_shifts() {
        // Zero-length insert BEFORE node START → KEEP and shift
        let tracker = NodeTracker::new();
        let uri = test_uri("zero_before");

        let ulid = tracker.get_or_create(&uri, 30, 50, "block");

        let edits = vec![EditInfo::new(10, 10, 15)]; // Zero-length insert 5 bytes

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Zero-length insert before node should not invalidate"
        );

        // Verify node shifted from [30, 50) to [35, 55)
        assert_eq!(
            tracker.get(&uri, 35, 55, "block"),
            Some(ulid),
            "Node should shift to [35, 55)"
        );
    }

    #[test]
    fn test_apply_input_edits_empty_slice() {
        let tracker = NodeTracker::new();
        let uri = test_uri("empty");

        tracker.get_or_create(&uri, 10, 20, "block");

        let invalidated = tracker.apply_input_edits(&uri, &[]);

        assert!(
            invalidated.is_empty(),
            "Empty edits should return empty Vec"
        );
    }

    #[test]
    fn test_apply_input_edits_unknown_uri_returns_empty() {
        // Unknown URI should return empty Vec (no entries to invalidate)
        let tracker = NodeTracker::new();
        let uri = test_uri("unknown");

        let edits = vec![EditInfo::new(10, 20, 15)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Unknown URI should return empty Vec"
        );
    }

    #[test]
    fn test_apply_input_edits_single_edit_spans_multiple_nodes() {
        // Single edit that affects multiple nodes differently
        // Nodes: A [20, 30), B [40, 50), C [60, 70)
        // Edit: delete [35, 55) (delta = -20)
        //
        // START-priority analysis:
        //   - Node A [20, 30): END 30 < edit.start 35 → completely before edit, KEEP unchanged
        //   - Node B [40, 50): START 40 in [35, 55) → INVALIDATE
        //   - Node C [60, 70): START 60 >= old_end 55 → KEEP, shift by delta -20
        //
        // Final positions:
        //   - Node A: [20, 30) unchanged
        //   - Node B: invalidated
        //   - Node C: [60-20, 70-20) = [40, 50)
        let tracker = NodeTracker::new();
        let uri = test_uri("multi_node");

        let ulid_a = tracker.get_or_create(&uri, 20, 30, "blockA");
        let ulid_b = tracker.get_or_create(&uri, 40, 50, "blockB");
        let ulid_c = tracker.get_or_create(&uri, 60, 70, "blockC");

        let edits = vec![EditInfo::new(35, 55, 35)]; // delete [35, 55)

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // Only Node B should be invalidated (START in edit range)
        assert!(
            !invalidated.contains(&ulid_a),
            "Node A should NOT be invalidated"
        );
        assert!(
            invalidated.contains(&ulid_b),
            "Node B should be invalidated"
        );
        assert!(
            !invalidated.contains(&ulid_c),
            "Node C should NOT be invalidated"
        );

        // Verify final positions
        assert_eq!(
            tracker.get(&uri, 20, 30, "blockA"),
            Some(ulid_a),
            "Node A should remain at [20, 30) (before edit)"
        );
        assert_eq!(
            tracker.get(&uri, 40, 50, "blockC"),
            Some(ulid_c),
            "Node C should shift to [40, 50)"
        );
    }

    #[test]
    fn test_apply_input_edits_end_absorbed_keeps_node() {
        // Node's END is inside edit range → clamp end to edit.new_end_byte, KEEP node
        // This is the "end absorbed" case from lazy-node-identity-tracking
        //
        // Node [20, 30), Edit: delete [25, 55) → [25, 25)
        // - START 20 NOT in [25, 55) → KEEP (START-priority rule)
        // - END 30 is inside [25, 55) → end absorbed, clamp to edit.new_end_byte (25)
        // - Final: [20, 25)
        //
        // IMPORTANT: This requires adjust_position_for_edit to clamp end when:
        //   edit.start_byte < node.end_byte <= edit.old_end_byte
        // Instead of applying delta (which would cause range collapse).
        let tracker = NodeTracker::new();
        let uri = test_uri("end_absorbed");

        let ulid = tracker.get_or_create(&uri, 20, 30, "block");

        let edits = vec![EditInfo::new(25, 55, 25)]; // delete [25, 55)

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // Node should NOT be invalidated (START not in edit range)
        assert!(
            invalidated.is_empty(),
            "Node with absorbed end should be KEPT, not invalidated"
        );

        // End is clamped to edit.new_end_byte (25), so node becomes [20, 25)
        assert_eq!(
            tracker.get(&uri, 20, 25, "block"),
            Some(ulid),
            "Node should be at [20, 25) (end clamped to edit.new_end_byte)"
        );
    }

    #[test]
    fn test_apply_input_edits_larger_than_node_still_invalidates() {
        // Edit range larger than node → should still invalidate
        // Node [40, 50), Edit [30, 60) delete
        // START 40 in [30, 60) → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("larger_edit");

        let ulid = tracker.get_or_create(&uri, 40, 50, "block");

        let edits = vec![EditInfo::new(30, 60, 30)]; // delete larger range

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.contains(&ulid),
            "Edit larger than node should still invalidate"
        );
    }

    #[test]
    fn test_apply_input_edits_reverse_order_vscode_multicursor() {
        // VSCode sends multi-cursor edits in REVERSE order (bottom-to-top)
        // This test verifies we handle non-ascending edit order correctly
        //
        // Document: "line1\nline2\nline3\n" (bytes: 6, 12, 18)
        // User types "X" at start of line3 (byte 12) and line1 (byte 0)
        // VSCode sends: first line3 edit, then line1 edit (reverse position order)
        //
        // Nodes: A [0, 5), B [12, 17)
        //
        // Edit 1: insert at [12, 12) → [12, 13) (running coords)
        //   - Node A [0, 5): START 0 < 12, KEEP unchanged
        //   - Node B [12, 17): START 12 == 12 (zero-length at START) → INVALIDATE
        //
        // Edit 2: insert at [0, 0) → [0, 1) (running coords after Edit 1)
        //   - Node A: already invalidated? No, A wasn't invalidated
        //   - Node A [0, 5): START 0 == 0 (zero-length at START) → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("vscode_multicursor");

        let ulid_a = tracker.get_or_create(&uri, 0, 5, "line1");
        let ulid_b = tracker.get_or_create(&uri, 12, 17, "line3");

        // VSCode sends reverse order: line3 first, then line1
        let edits = vec![
            EditInfo::new(12, 12, 13), // Insert at line3 (later position first)
            EditInfo::new(0, 0, 1),    // Insert at line1 (earlier position second)
        ];

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // Both should be invalidated (zero-length insert at START)
        assert!(
            invalidated.contains(&ulid_a),
            "Node A should be invalidated (insert at START)"
        );
        assert!(
            invalidated.contains(&ulid_b),
            "Node B should be invalidated (insert at START)"
        );
    }

    #[test]
    fn test_apply_input_edits_reverse_order_preserves_positions() {
        // VSCode sends multi-cursor edits bottom-to-top in running coordinates.
        // Nodes A=[40,50) B=[80,90) C=[120,130); inserts: +5@100, +3@60, +2@20.
        // Expected after all three: A=[42,52) B=[85,95) C=[130,140) (cumulative +2/+5/+10).
        let tracker = NodeTracker::new();
        let uri = test_uri("vscode_reverse_positions");

        let ulid_a = tracker.get_or_create(&uri, 40, 50, "block");
        let ulid_b = tracker.get_or_create(&uri, 80, 90, "block");
        let ulid_c = tracker.get_or_create(&uri, 120, 130, "block");

        // VSCode reverse order: later positions first
        let edits = vec![
            EditInfo::new(100, 100, 105), // Insert 5 at 100 (between B and C)
            EditInfo::new(60, 60, 63),    // Insert 3 at 60 (between A and B)
            EditInfo::new(20, 20, 22),    // Insert 2 at 20 (before A)
        ];

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // No nodes should be invalidated (all edits before their START)
        assert!(
            invalidated.is_empty(),
            "No nodes should be invalidated: {:?}",
            invalidated
        );

        // Verify exact final positions
        assert_eq!(
            tracker.get(&uri, 42, 52, "block"),
            Some(ulid_a),
            "Node A should be at [42, 52) after +2 shift"
        );
        assert_eq!(
            tracker.get(&uri, 85, 95, "block"),
            Some(ulid_b),
            "Node B should be at [85, 95) after +5 cumulative shift"
        );
        assert_eq!(
            tracker.get(&uri, 130, 140, "block"),
            Some(ulid_c),
            "Node C should be at [130, 140) after +10 cumulative shift"
        );

        // Verify old positions are gone
        assert_eq!(
            tracker.get(&uri, 40, 50, "block"),
            None,
            "Old position A [40, 50) should not exist"
        );
        assert_eq!(
            tracker.get(&uri, 80, 90, "block"),
            None,
            "Old position B [80, 90) should not exist"
        );
        assert_eq!(
            tracker.get(&uri, 120, 130, "block"),
            None,
            "Old position C [120, 130) should not exist"
        );
    }

    /// Test that invalid edits are skipped gracefully in production.
    ///
    /// NOTE: This test only runs in release mode (`--release`) because debug
    /// builds have a `debug_assert!` that catches invalid edits early.
    /// The debug_assert helps catch bugs during development, while the
    /// runtime check provides graceful degradation in production.
    #[test]
    #[cfg(not(debug_assertions))]
    fn test_apply_input_edits_invalid_edit_skipped() {
        // Invalid edit (old_end < start) should be skipped with warning
        let tracker = NodeTracker::new();
        let uri = test_uri("invalid_edit");

        let ulid = tracker.get_or_create(&uri, 30, 50, "block");

        let edits = vec![
            EditInfo::new(50, 40, 50), // INVALID: old_end_byte 40 < start_byte 50
            EditInfo::new(20, 25, 20), // Valid edit before node (shift)
        ];

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        // Invalid edit skipped, valid edit processed
        assert!(
            invalidated.is_empty(),
            "No invalidation (invalid edit skipped, valid edit shifts)"
        );

        // Node should shift from [30, 50) to [25, 45) due to valid delete
        assert_eq!(
            tracker.get(&uri, 25, 45, "block"),
            Some(ulid),
            "Node should shift after valid edit (invalid edit skipped)"
        );
    }

    /// Test that invalid edits trigger debug_assert in debug builds.
    ///
    /// This validates fail-fast behavior: `EditInfo::new()` panics immediately
    /// when given invalid parameters (old_end_byte < start_byte), catching
    /// bugs at the source rather than during processing.
    ///
    /// This complements `test_apply_input_edits_invalid_edit_skipped` which runs
    /// only in release mode. Together they verify the two-layer defense:
    /// - Debug builds: panic early in constructor to catch bugs at source
    /// - Release builds: skip gracefully for production resilience
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "Invalid EditInfo")]
    fn test_apply_input_edits_invalid_edit_panics_in_debug() {
        // The panic happens in EditInfo::new(), not in apply_input_edits()
        // This is intentional: fail-fast at construction time
        EditInfo::new(50, 40, 50); // INVALID: old_end_byte 40 < start_byte 50
    }

    #[test]
    fn test_apply_input_edits_start_inside_edit_range_invalidates() {
        // Node whose START falls inside edit range → INVALIDATE by START-priority
        //
        // Node [10, 12), Edit: delete [5, 20) → [5, 5)
        // - START 10 IS in [5, 20) (5 <= 10 < 20) → INVALIDATE
        //
        // NOTE: This tests START-priority invalidation, not range collapse.
        // Range collapse via end clamping is theoretically unreachable because:
        //   - For collapse: edit.new_end_byte <= node.start
        //   - But if node.start < edit.start (required to pass START-priority),
        //     then edit.new_end_byte >= edit.start > node.start
        //   - Therefore collapse condition can never be satisfied
        // The range collapse check in adjust_position_for_edit is kept
        // as defense-in-depth against unexpected edge cases.
        let tracker = NodeTracker::new();
        let uri = test_uri("start_in_edit");

        let ulid = tracker.get_or_create(&uri, 10, 12, "block");

        let edits = vec![EditInfo::new(5, 20, 5)]; // delete [5, 20)

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.contains(&ulid),
            "Node with START inside edit range should be invalidated"
        );
    }

    #[test]
    fn test_apply_input_edits_zero_length_insert_after_node_keeps_unchanged() {
        // Zero-length insert AFTER node → KEEP unchanged
        let tracker = NodeTracker::new();
        let uri = test_uri("zero_after");

        let ulid = tracker.get_or_create(&uri, 20, 40, "block");

        let edits = vec![EditInfo::new(50, 50, 55)]; // Zero-length insert after node

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Zero-length insert after node should not invalidate"
        );

        // Verify node unchanged at [20, 40)
        assert_eq!(
            tracker.get(&uri, 20, 40, "block"),
            Some(ulid),
            "Node should remain at [20, 40)"
        );
    }

    // === Phase 4 Boundary Tests ===

    #[test]
    fn test_apply_input_edits_end_exactly_at_old_end_clamps() {
        // Boundary: Node END exactly equals edit.old_end_byte
        //
        // Node [20, 55), Edit: delete [25, 55) → [25, 25)
        // - START 20 NOT in [25, 55) → KEEP
        // - END 55 == old_end 55 → condition (end <= old_end) is TRUE → clamp to 25
        // - Final: [20, 25)
        let tracker = NodeTracker::new();
        let uri = test_uri("end_at_old_end");

        let ulid = tracker.get_or_create(&uri, 20, 55, "block");

        let edits = vec![EditInfo::new(25, 55, 25)]; // delete [25, 55)

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Node with end at old_end should be KEPT (clamped)"
        );

        assert_eq!(
            tracker.get(&uri, 20, 25, "block"),
            Some(ulid),
            "Node should be at [20, 25) (end clamped to new_end)"
        );
    }

    #[test]
    fn test_apply_input_edits_end_exactly_at_edit_start_unchanged() {
        // Boundary: Node END exactly equals edit.start_byte
        //
        // Node [20, 25), Edit: delete [25, 55) → [25, 25)
        // - Branch check: end > edit.start? → 25 > 25? NO
        // - Falls to else branch (Node F) → unchanged
        let tracker = NodeTracker::new();
        let uri = test_uri("end_at_start");

        let ulid = tracker.get_or_create(&uri, 20, 25, "block");

        let edits = vec![EditInfo::new(25, 55, 25)]; // delete [25, 55)

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Node ending at edit.start should be unchanged"
        );

        assert_eq!(
            tracker.get(&uri, 20, 25, "block"),
            Some(ulid),
            "Node should remain at [20, 25) (completely before edit)"
        );
    }

    #[test]
    fn test_apply_input_edits_zero_length_insert_inside_node_expands() {
        // Zero-length insert INSIDE node (not at boundaries)
        //
        // Node [20, 40), Edit: insert at [30, 30) → [30, 35) (5 bytes)
        // - START 20 != 30 → KEEP (not zero-length at START)
        // - END 40 > edit.start 30 → enters Node A/B branch
        // - END 40 <= old_end 30? NO (40 > 30) → apply delta +5
        // - Final: [20, 45)
        let tracker = NodeTracker::new();
        let uri = test_uri("insert_inside");

        let ulid = tracker.get_or_create(&uri, 20, 40, "block");

        let edits = vec![EditInfo::new(30, 30, 35)]; // insert 5 bytes at 30

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(
            invalidated.is_empty(),
            "Insert inside node should KEEP it (expand)"
        );

        assert_eq!(
            tracker.get(&uri, 20, 45, "block"),
            Some(ulid),
            "Node should expand to [20, 45) (end shifted by +5)"
        );
    }

    // ============================================================
    // Phase 4 Concurrency Tests: apply_input_edits Thread-Safety
    // ============================================================
    // These tests verify thread-safety when multiple threads call
    // apply_input_edits() concurrently, mirroring the apply_text_diff tests.

    // ============================================================
    // Phase 4 UTF-8 Multi-byte Tests: apply_input_edits with Unicode
    // ============================================================
    // These tests verify apply_input_edits handles byte positions correctly
    // for multi-byte UTF-8 characters. LSP provides byte offsets directly,
    // so we must ensure the START-priority logic works with any byte values.

    #[test]
    fn test_apply_input_edits_utf8_delete_emoji_before_node_shifts() {
        // Delete 4-byte emoji before node → shift by -4
        // Mirrors test_utf8_multibyte_edit_before_node_shifts_correctly
        let tracker = NodeTracker::new();
        let uri = test_uri("apply_input_edits_utf8_before");

        // Text: "abc🦀def" where 🦀 is at bytes [3, 7)
        // Node at bytes [7, 10) covering "def"
        let ulid = tracker.get_or_create(&uri, 7, 10, "block");

        // Edit: delete [3, 7) (the emoji), new_end = 3 (delta = -4)
        let edits = vec![EditInfo::new(3, 7, 3)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(invalidated.is_empty(), "Node after edit should be kept");

        // Node shifts from [7, 10) to [3, 6)
        assert_eq!(
            tracker.get(&uri, 3, 6, "block"),
            Some(ulid),
            "Node should shift by 4 bytes (emoji size)"
        );
    }

    #[test]
    fn test_apply_input_edits_utf8_delete_inside_node_adjusts_end() {
        // Delete 3-byte character inside node → end shrinks
        // Node [0, 17), delete bytes [8, 11) → [0, 14)
        let tracker = NodeTracker::new();
        let uri = test_uri("apply_input_edits_utf8_inside");

        // Text: "start日本語end" (17 bytes)
        // Node covers entire text [0, 17)
        let ulid = tracker.get_or_create(&uri, 0, 17, "block");

        // Edit: delete "本" at [8, 11), new_end = 8 (delta = -3)
        let edits = vec![EditInfo::new(8, 11, 8)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(invalidated.is_empty(), "Node START 0 not in [8, 11)");

        // End adjusts from 17 to 14
        assert_eq!(
            tracker.get(&uri, 0, 14, "block"),
            Some(ulid),
            "Node end should adjust by -3 (3-byte kanji)"
        );
    }

    #[test]
    fn test_apply_input_edits_utf8_start_inside_edit_invalidates() {
        // Node START inside edit range → INVALIDATE
        // Node [7, 14), edit [3, 10) → START 7 ∈ [3, 10) → INVALIDATE
        let tracker = NodeTracker::new();
        let uri = test_uri("apply_input_edits_utf8_invalidate");

        // Text: "前🎉後text" - node [7, 14) covers "後text"
        let ulid = tracker.get_or_create(&uri, 7, 14, "block");

        // Edit: delete [3, 10) (🎉後 = 4+3 = 7 bytes), new_end = 3
        let edits = vec![EditInfo::new(3, 10, 3)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert_eq!(
            invalidated,
            vec![ulid],
            "Node with START 7 inside [3, 10) should be invalidated"
        );

        // Original position gone
        assert_eq!(tracker.get(&uri, 7, 14, "block"), None);
    }

    #[test]
    fn test_apply_input_edits_utf8_insert_emoji_shifts_node() {
        // Insert 4-byte emoji before node → shift by +4
        let tracker = NodeTracker::new();
        let uri = test_uri("apply_input_edits_utf8_insert");

        // Text: "abcdef", node [3, 6) covers "def"
        let ulid = tracker.get_or_create(&uri, 3, 6, "block");

        // Edit: insert 4 bytes at position 2 (ab|🚀|cdef)
        // [2, 2) → [2, 6) means insert 4 bytes at position 2
        let edits = vec![EditInfo::new(2, 2, 6)];
        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(invalidated.is_empty(), "Node after insert should be kept");

        // Node shifts from [3, 6) to [7, 10)
        assert_eq!(
            tracker.get(&uri, 7, 10, "block"),
            Some(ulid),
            "Node should shift by +4 bytes (emoji insertion)"
        );
    }

    #[test]
    fn test_apply_input_edits_utf8_mixed_operations() {
        // Multiple UTF-8 aware edits in sequence
        // Tests running coordinate updates with multi-byte deltas
        let tracker = NodeTracker::new();
        let uri = test_uri("apply_input_edits_utf8_mixed");

        // Three nodes: [0, 5), [10, 15), [20, 25)
        let ulid1 = tracker.get_or_create(&uri, 0, 5, "block");
        let ulid2 = tracker.get_or_create(&uri, 10, 15, "block");
        let ulid3 = tracker.get_or_create(&uri, 20, 25, "block");

        // Apply two edits in sequence (LSP order: as they were applied)
        // Edit 1: Insert 3-byte char at position 7 (between nodes 1 and 2)
        //         [7, 7) → [7, 10) delta +3
        // Edit 2: Delete 4-byte char at position 18 (between nodes 2 and 3, after first edit)
        //         [18, 22) → [18, 18) delta -4
        let edits = vec![EditInfo::new(7, 7, 10), EditInfo::new(18, 22, 18)];

        let invalidated = tracker.apply_input_edits(&uri, &edits);

        assert!(invalidated.is_empty(), "No nodes should be invalidated");

        // After edit 1 (delta +3):
        //   Node 1 [0, 5) - before edit, unchanged
        //   Node 2 [10, 15) → [13, 18) - after edit
        //   Node 3 [20, 25) → [23, 28) - after edit
        // After edit 2 (delta -4), applied to post-edit-1 positions:
        //   Node 1 [0, 5) - still unchanged (before both edits)
        //   Node 2 [13, 18) - before edit 2, unchanged
        //   Node 3 [23, 28) → [19, 24) - after edit 2 (edit at 18 < 23)
        assert_eq!(
            tracker.get(&uri, 0, 5, "block"),
            Some(ulid1),
            "Node 1 unchanged"
        );
        assert_eq!(
            tracker.get(&uri, 13, 18, "block"),
            Some(ulid2),
            "Node 2 shifted by +3 from first edit"
        );
        assert_eq!(
            tracker.get(&uri, 19, 24, "block"),
            Some(ulid3),
            "Node 3 shifted by +3 then -4 = -1 net"
        );
    }

    #[test]
    fn test_concurrent_apply_input_edits_same_uri_no_panic() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = test_uri("concurrent_apply_input_edits");

        // Pre-populate with several nodes spread across the document
        for i in 0..10 {
            let start = i * 20;
            tracker.get_or_create(&uri, start, start + 15, "block");
        }

        // Spawn multiple threads that apply different edits concurrently
        let handles: Vec<_> = (0..5)
            .map(|thread_id| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();
                thread::spawn(move || {
                    // Each thread does multiple edit cycles
                    for cycle in 0..3 {
                        // Create edit at different positions per thread
                        let edit_start = (thread_id * 30 + cycle * 5) % 150;
                        let edit_end = edit_start + 5;

                        // Small deletion: [edit_start, edit_end) → [edit_start, edit_start)
                        let edits = vec![EditInfo::new(edit_start, edit_end, edit_start)];
                        tracker.apply_input_edits(&uri, &edits);
                    }
                })
            })
            .collect();

        // Wait for all threads to complete - no panics should occur
        for handle in handles {
            handle
                .join()
                .expect("Thread should not panic during concurrent apply_input_edits");
        }

        // Verify tracker is still functional after concurrent edits
        let new_ulid = tracker.get_or_create(&uri, 1000, 1010, "test");
        assert!(
            new_ulid.to_string().len() == 26,
            "ULID should be valid after concurrent apply_input_edits"
        );
    }

    #[test]
    fn test_concurrent_apply_input_edits_different_uris_independent() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());

        // Each thread works on its own URI
        let handles: Vec<_> = (0..5)
            .map(|thread_id| {
                let tracker = Arc::clone(&tracker);
                let uri = test_uri(&format!("apply_input_edits_uri_{}", thread_id));

                thread::spawn(move || {
                    // Create initial node at [50, 100)
                    let ulid = tracker.get_or_create(&uri, 50, 100, "block");

                    // Apply an edit that shifts the node: delete [20, 30) before node
                    // This is a deletion of 10 bytes before the node (delta = -10)
                    let edits = vec![EditInfo::new(20, 30, 20)];
                    tracker.apply_input_edits(&uri, &edits);

                    // After edit, node should shift from [50, 100) to [40, 90)
                    let shifted_ulid = tracker.get(&uri, 40, 90, "block");
                    (ulid, shifted_ulid)
                })
            })
            .collect();

        // Collect results and verify each URI maintained correct state
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        for (original, shifted) in results {
            assert_eq!(
                Some(original),
                shifted,
                "Each URI should independently maintain correct position adjustment with apply_input_edits"
            );
        }
    }

    #[test]
    fn test_concurrent_get_and_apply_input_edits_interleaved() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = test_uri("apply_input_edits_interleaved");

        // Pre-populate with one stable node that won't be affected by edits
        // Node at [0, 10) - edits will be at [50+)
        let stable_ulid = tracker.get_or_create(&uri, 0, 10, "stable");

        // Spawn threads that interleave get_or_create and apply_input_edits
        let handles: Vec<_> = (0..4)
            .map(|thread_id| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();

                thread::spawn(move || {
                    let mut results = Vec::new();

                    for cycle in 0..5 {
                        // Create new node at a unique position per thread and cycle
                        let base = 50 + thread_id * 100 + cycle * 20;
                        let _ = tracker.get_or_create(&uri, base, base + 10, "dynamic");

                        // Apply edit that doesn't affect stable node at [0, 10)
                        if cycle % 2 == 0 {
                            let edit_start = 50 + thread_id * 100;
                            let edits = vec![EditInfo::new(edit_start, edit_start + 5, edit_start)];
                            tracker.apply_input_edits(&uri, &edits);
                        }

                        // Always verify stable node is accessible
                        if let Some(ulid) = tracker.get(&uri, 0, 10, "stable") {
                            results.push(ulid);
                        }
                    }

                    // Verify stable node's ULID was consistent across all reads
                    (stable_ulid, results)
                })
            })
            .collect();

        // Verify all threads saw consistent stable ULID
        for handle in handles {
            let (expected, observed) = handle.join().expect("Thread should not panic");
            for ulid in observed {
                assert_eq!(
                    expected, ulid,
                    "Stable node should have consistent ULID across concurrent apply_input_edits access"
                );
            }
        }
    }

    // =========================================================================
    // Phase 5: Library Validation Tests (Phase 0)
    // These validate `similar` crate behavior, not our code. Should PASS immediately.
    // =========================================================================

    #[test]
    fn test_similar_delete_before_insert() {
        // Validates that similar crate produces Delete operations before Insert
        // for replacements. This is important for our byte tracking logic.
        use similar::{ChangeTag, TextDiff};

        let diff = TextDiff::from_chars("ABC", "XYZ");
        let changes: Vec<_> = diff.iter_all_changes().collect();

        // For replacement, expect: Delete A, Delete B, Delete C, Insert X, Insert Y, Insert Z
        // or interleaved Delete-Insert pairs. Key point: deletes come first.
        let first_delete = changes.iter().position(|c| c.tag() == ChangeTag::Delete);
        let first_insert = changes.iter().position(|c| c.tag() == ChangeTag::Insert);

        assert!(first_delete.is_some(), "Should have delete operations");
        assert!(first_insert.is_some(), "Should have insert operations");
        // Note: similar may interleave, but the contract we care about is
        // that we can track byte offsets correctly regardless of order
    }

    #[test]
    fn test_similar_produces_edits_for_different_texts() {
        // Validates that similar crate always produces at least one change
        // when old_text != new_text. This is a proof assumption.
        use similar::{ChangeTag, TextDiff};

        let test_cases = vec![
            ("A", "B"),
            ("", "X"),
            ("X", ""),
            ("ABC", "ABD"),
            ("Hello", "World"),
        ];

        for (old, new) in test_cases {
            let diff = TextDiff::from_chars(old, new);
            let has_changes = diff.iter_all_changes().any(|c| c.tag() != ChangeTag::Equal);
            assert!(
                has_changes,
                "similar should produce changes for '{}' -> '{}'",
                old, new
            );
        }
    }

    #[test]
    fn test_reconstruct_touching_edits_verify_similar_behavior() {
        // Verify that similar crate merges "AAABBB" → "XY"
        // into single edit [0,6) rather than [0,3)+[3,6)
        //
        // This is important because touching edits mean there's no preserved
        // content between them, so similar treats them as one contiguous change.
        let tracker = NodeTracker::new();
        let uri = test_uri("touching");

        // Node exactly at what would be the "boundary" if edits were separate
        let n = tracker.get_or_create(&uri, 3, 6, "B");

        let invalidated = tracker.apply_text_diff(&uri, "AAABBB", "XY");

        // Document behavior: similar crate merges touching edits,
        // so N's START (3) is inside merged edit [0,6) → INVALIDATED
        assert!(
            invalidated.contains(&n),
            "Touching edits should be merged by similar crate, invalidating middle node"
        );
    }

    #[test]
    fn test_individual_edits_keep_untouched_middle_node() {
        // Setup: "AAABBBCCC" with nodes at [0,3), [3,6), [6,9)
        // Edit:  "XBBBYY" - changes "AAA" to "X" and "CCC" to "YY"
        //
        // With merged EditInfo [0,9), BBB's START (3) would fall inside and be
        // invalidated. With individual EditInfos [0,3) and [6,9), BBB stays untouched.
        let tracker = NodeTracker::new();
        let uri = test_uri("regression");

        let n_middle = tracker.get_or_create(&uri, 3, 6, "B");
        tracker.get_or_create(&uri, 0, 3, "A");
        tracker.get_or_create(&uri, 6, 9, "C");

        let invalidated = tracker.apply_text_diff(&uri, "AAABBBCCC", "XBBBYY");

        // CRITICAL ASSERTION: This is the Phase 5 behavioral difference
        assert!(
            !invalidated.contains(&n_middle),
            "REGRESSION: Middle node should NOT be invalidated with individual edit processing. \
             If this fails, merged edit processing may have been accidentally restored."
        );
    }

    // =========================================================================
    // Phase 5: Core Reconstruction Tests (Phase C)
    // These test `reconstruct_individual_edits` via `apply_text_diff`.
    // =========================================================================

    #[test]
    fn test_reconstruct_single_replacement() {
        // "ABC" → "XYZ": single contiguous replacement
        let tracker = NodeTracker::new();
        let uri = test_uri("single_replace");

        let n = tracker.get_or_create(&uri, 0, 3, "block");
        let invalidated = tracker.apply_text_diff(&uri, "ABC", "XYZ");

        // Entire range changed → node invalidated
        assert!(invalidated.contains(&n));
    }

    #[test]
    fn test_reconstruct_two_separate_edits() {
        // "AAABBBCCC" → "XBBBYY": two separate edits with preserved middle
        let tracker = NodeTracker::new();
        let uri = test_uri("two_edits");

        let n1 = tracker.get_or_create(&uri, 0, 3, "A");
        let n2 = tracker.get_or_create(&uri, 3, 6, "B"); // Should survive
        let n3 = tracker.get_or_create(&uri, 6, 9, "C");

        let invalidated = tracker.apply_text_diff(&uri, "AAABBBCCC", "XBBBYY");

        assert_eq!(invalidated.len(), 2, "Two nodes should be invalidated");
        assert!(
            invalidated.contains(&n1),
            "First node should be invalidated"
        );
        assert!(
            invalidated.contains(&n3),
            "Third node should be invalidated"
        );
        assert!(!invalidated.contains(&n2), "Middle node should survive");
    }

    #[test]
    fn test_reconstruct_pure_insertion() {
        // "AB" → "AXB": pure insertion at position 1
        let tracker = NodeTracker::new();
        let uri = test_uri("insert");

        // Node after insert point should shift
        let n = tracker.get_or_create(&uri, 1, 2, "B");
        let invalidated = tracker.apply_text_diff(&uri, "AB", "AXB");

        // Node's START (1) is at the insert point → invalidated (conservative)
        // Per lazy-node-identity-tracking: zero-length insert at START → invalidate
        assert!(
            invalidated.contains(&n),
            "Node at insert point should be invalidated"
        );
    }

    #[test]
    fn test_reconstruct_pure_deletion() {
        // "ABC" → "AC": pure deletion of 'B'
        let tracker = NodeTracker::new();
        let uri = test_uri("delete");

        let n_before = tracker.get_or_create(&uri, 0, 1, "A");
        let n_deleted = tracker.get_or_create(&uri, 1, 2, "B");
        let n_after = tracker.get_or_create(&uri, 2, 3, "C");

        let invalidated = tracker.apply_text_diff(&uri, "ABC", "AC");

        assert!(
            !invalidated.contains(&n_before),
            "Node before deletion unchanged"
        );
        assert!(invalidated.contains(&n_deleted), "Deleted node invalidated");
        // Node after should shift to [1,2) but not be invalidated
        assert!(
            !invalidated.contains(&n_after),
            "Node after deletion shifted, not invalidated"
        );
        assert_eq!(
            tracker.get(&uri, 1, 2, "C"),
            Some(n_after),
            "Node C shifted to [1,2)"
        );
    }

    #[test]
    fn test_reconstruct_empty_to_content() {
        // "" → "ABC": insert into empty document
        let tracker = NodeTracker::new();
        let uri = test_uri("empty_to_content");

        // No nodes to track initially
        let invalidated = tracker.apply_text_diff(&uri, "", "ABC");
        assert!(
            invalidated.is_empty(),
            "No nodes to invalidate in empty document"
        );
    }

    #[test]
    fn test_reconstruct_content_to_empty() {
        // "ABC" → "": delete all content
        let tracker = NodeTracker::new();
        let uri = test_uri("content_to_empty");

        let n = tracker.get_or_create(&uri, 0, 3, "block");
        let invalidated = tracker.apply_text_diff(&uri, "ABC", "");

        assert!(
            invalidated.contains(&n),
            "Node should be invalidated when content deleted"
        );
    }

    #[test]
    fn test_reconstruct_emoji() {
        // "A😀B" → "AXB": multi-byte character handling
        let tracker = NodeTracker::new();
        let uri = test_uri("emoji");

        // 😀 is 4 bytes, so original positions are: A[0,1), 😀[1,5), B[5,6)
        let n_emoji = tracker.get_or_create(&uri, 1, 5, "emoji");
        let n_b = tracker.get_or_create(&uri, 5, 6, "B");

        let invalidated = tracker.apply_text_diff(&uri, "A😀B", "AXB");

        // Emoji node replaced → invalidated
        assert!(
            invalidated.contains(&n_emoji),
            "Emoji node should be invalidated"
        );
        // B node shifted from [5,6) to [2,3)
        assert!(
            !invalidated.contains(&n_b),
            "B node should shift, not invalidate"
        );
        assert_eq!(
            tracker.get(&uri, 2, 3, "B"),
            Some(n_b),
            "B node at new position"
        );
    }

    #[test]
    fn test_reconstruct_identical() {
        // "ABC" → "ABC": fast path, no processing
        let tracker = NodeTracker::new();
        let uri = test_uri("identical");

        let n = tracker.get_or_create(&uri, 0, 3, "block");
        let invalidated = tracker.apply_text_diff(&uri, "ABC", "ABC");

        assert!(invalidated.is_empty(), "Identical text should return empty");
        assert_eq!(tracker.get(&uri, 0, 3, "block"), Some(n), "Node unchanged");
    }

    #[test]
    fn test_reconstruct_crlf_newlines() {
        // Test CRLF byte offset handling
        let tracker = NodeTracker::new();
        let uri = test_uri("crlf");

        // "line1\r\nline2" has \r\n at bytes [5,7)
        // Node after the newlines
        let n = tracker.get_or_create(&uri, 7, 12, "line2");

        let invalidated = tracker.apply_text_diff(&uri, "line1\r\nline2", "line1\r\nmodified");

        // "line2" (5 chars) replaced with "modified" (8 chars) at position 7
        // Node at [7,12) has START=7 which is at edit start → invalidated
        assert!(
            invalidated.contains(&n),
            "Node at edit start should be invalidated"
        );
    }

    #[test]
    fn test_reconstruct_long_identical_prefix() {
        // Long unchanged prefix followed by small change
        let prefix = "A".repeat(1000);
        let old = format!("{}_X_BBB", prefix);
        let new = format!("{}_Y_BBB", prefix);

        let tracker = NodeTracker::new();
        let uri = test_uri("long_prefix");

        // Node at the changing position (after 1000 A's + underscore)
        let n = tracker.get_or_create(&uri, 1001, 1002, "X");
        let n_after = tracker.get_or_create(&uri, 1003, 1006, "BBB");

        let invalidated = tracker.apply_text_diff(&uri, &old, &new);

        assert!(
            invalidated.contains(&n),
            "Changed node should be invalidated"
        );
        assert!(
            !invalidated.contains(&n_after),
            "Node after change should be preserved"
        );
    }

    // =========================================================================
    // Phase 5: Integration Tests
    // =========================================================================

    #[test]
    fn test_apply_text_diff_preserves_unaffected_node() {
        let tracker = NodeTracker::new();
        let uri = test_uri("preserve");

        let _n1 = tracker.get_or_create(&uri, 0, 3, "A");
        let n2 = tracker.get_or_create(&uri, 3, 6, "B"); // Should survive
        let _n3 = tracker.get_or_create(&uri, 6, 9, "C");

        let invalidated = tracker.apply_text_diff(&uri, "AAABBBCCC", "XBBBYY");

        assert!(!invalidated.contains(&n2), "Middle node should survive");
    }

    #[test]
    fn test_apply_text_diff_correct_final_position() {
        let tracker = NodeTracker::new();
        let uri = test_uri("final_pos");

        let n2 = tracker.get_or_create(&uri, 3, 6, "B");

        // "AAABBBCCC" → "XBBBYY"
        // Edit1: [0,3) AAA→X (delta=-2)
        // Edit2: [6,9) CCC→YY (delta=-1)
        // After both edits: BBB at [1,4)
        tracker.apply_text_diff(&uri, "AAABBBCCC", "XBBBYY");

        assert_eq!(
            tracker.get(&uri, 1, 4, "B"),
            Some(n2),
            "BBB should be at [1,4) after edit"
        );
    }

    #[test]
    fn test_apply_text_diff_empty_to_content() {
        let tracker = NodeTracker::new();
        let uri = test_uri("empty_start");

        let invalidated = tracker.apply_text_diff(&uri, "", "New content");
        assert!(
            invalidated.is_empty(),
            "No invalidation from empty document"
        );
    }

    #[test]
    fn test_apply_text_diff_content_to_empty() {
        let tracker = NodeTracker::new();
        let uri = test_uri("to_empty");

        let n = tracker.get_or_create(&uri, 0, 10, "block");
        let invalidated = tracker.apply_text_diff(&uri, "0123456789", "");

        assert!(
            invalidated.contains(&n),
            "All nodes invalidated when clearing"
        );
    }

    #[test]
    fn test_apply_text_diff_identical_returns_empty() {
        let tracker = NodeTracker::new();
        let uri = test_uri("identical_fast");

        let n = tracker.get_or_create(&uri, 0, 5, "block");
        let invalidated = tracker.apply_text_diff(&uri, "Hello", "Hello");

        assert!(invalidated.is_empty(), "Identical text returns empty vec");
        assert_eq!(tracker.get(&uri, 0, 5, "block"), Some(n));
    }

    #[test]
    fn test_apply_text_diff_touching_edits() {
        // When edits touch [0,3)+[3,6), similar merges them
        let tracker = NodeTracker::new();
        let uri = test_uri("touching");

        let n = tracker.get_or_create(&uri, 3, 6, "middle");
        let invalidated = tracker.apply_text_diff(&uri, "AAABBB", "XY");

        // Similar merges touching edits → middle node invalidated
        assert!(invalidated.contains(&n));
    }

    #[test]
    fn test_apply_text_diff_single_byte_node() {
        let tracker = NodeTracker::new();
        let uri = test_uri("single_byte");

        // Single byte node at position 5
        let n = tracker.get_or_create(&uri, 5, 6, "X");

        // Edit before: doesn't affect node's START
        let invalidated = tracker.apply_text_diff(&uri, "01234X6789", "0123X6789");

        // Node at [5,6) - edit deletes byte 4, so node shifts to [4,5)
        assert!(
            !invalidated.contains(&n),
            "Node should shift, not invalidate"
        );
        assert_eq!(tracker.get(&uri, 4, 5, "X"), Some(n));
    }

    #[test]
    fn test_apply_text_diff_adjacent_nodes_large_delete() {
        let tracker = NodeTracker::new();
        let uri = test_uri("adjacent_delete");

        let n1 = tracker.get_or_create(&uri, 0, 10, "A");
        let n2 = tracker.get_or_create(&uri, 20, 30, "B");

        // Delete middle section [10,20)
        let invalidated = tracker.apply_text_diff(
            &uri,
            "0123456789__________0123456789",
            "01234567890123456789",
        );

        // Neither node's START is in [10,20)
        assert!(!invalidated.contains(&n1), "First node unchanged");
        assert!(
            !invalidated.contains(&n2),
            "Second node shifts but not invalidated"
        );

        // n2 should shift from [20,30) to [10,20)
        assert_eq!(tracker.get(&uri, 10, 20, "B"), Some(n2));
    }

    // =========================================================================
    // Phase 5: Boundary Condition Tests
    // =========================================================================

    #[test]
    fn test_node_end_exactly_at_edit_start() {
        // Node [0,30), Edit [30,40) → node unchanged
        let tracker = NodeTracker::new();
        let uri = test_uri("boundary_end_at_start");

        let n = tracker.get_or_create(&uri, 0, 30, "block");

        // Edit starts exactly where node ends
        let edit = EditInfo::new(30, 40, 35);
        let invalidated = tracker.apply_single_edit(&uri, &edit);

        assert!(!invalidated.contains(&n), "Node before edit unchanged");
        assert_eq!(tracker.get(&uri, 0, 30, "block"), Some(n));
    }

    #[test]
    fn test_node_end_exactly_at_edit_old_end() {
        // Node [0,40), Edit [30,40) → end adjustment edge case
        let tracker = NodeTracker::new();
        let uri = test_uri("boundary_end_at_old_end");

        let n = tracker.get_or_create(&uri, 0, 40, "block");

        // Edit: delete [30,40), insert 5 bytes → new_end=35
        let edit = EditInfo::new(30, 40, 35);
        let invalidated = tracker.apply_single_edit(&uri, &edit);

        // Node START (0) not in edit range [30,40) → not invalidated
        // Node END (40) is exactly at edit.old_end → clamped to new_end
        assert!(!invalidated.contains(&n));
        assert_eq!(
            tracker.get(&uri, 0, 35, "block"),
            Some(n),
            "Node end clamped"
        );
    }

    #[test]
    fn test_edit_start_exactly_at_node_end() {
        // Node [0,10), Edit [10,20) → node unchanged
        let tracker = NodeTracker::new();
        let uri = test_uri("edit_at_node_end");

        let n = tracker.get_or_create(&uri, 0, 10, "block");

        let edit = EditInfo::new(10, 20, 15);
        let invalidated = tracker.apply_single_edit(&uri, &edit);

        assert!(!invalidated.contains(&n), "Node before edit not affected");
        assert_eq!(tracker.get(&uri, 0, 10, "block"), Some(n));
    }

    #[test]
    fn test_edit_fully_inside_node() {
        // Node [0,30), Edit [10,20) → start unchanged, end adjusted
        let tracker = NodeTracker::new();
        let uri = test_uri("edit_inside");

        let n = tracker.get_or_create(&uri, 0, 30, "block");

        // Delete 10 bytes inside: [10,20) → [10,10) (delta=-10)
        let edit = EditInfo::new(10, 20, 10);
        let invalidated = tracker.apply_single_edit(&uri, &edit);

        // Node START (0) not in [10,20) → not invalidated
        // Node END (30) > edit.old_end (20) → shifted by delta (-10)
        assert!(!invalidated.contains(&n));
        assert_eq!(
            tracker.get(&uri, 0, 20, "block"),
            Some(n),
            "Node end adjusted by delta"
        );
    }

    // =========================================================================
    // Phase 5: Additional Integration Tests
    // =========================================================================

    #[test]
    fn test_apply_text_diff_cumulative_delta_verification() {
        // Multiple edits accumulate delta correctly in reverse order
        let tracker = NodeTracker::new();
        let uri = test_uri("cumulative");

        // "AAABBBCCCDDDEEE" with nodes at each segment
        let nodes: Vec<_> = (0..5)
            .map(|i| tracker.get_or_create(&uri, i * 3, (i + 1) * 3, leak_kind(i)))
            .collect();

        // Remove AAA (delta=-3) and CCC (delta=-3)
        // Result: "BBBDDDEEE"
        let invalidated = tracker.apply_text_diff(&uri, "AAABBBCCCDDDEEE", "BBBDDDEEE");

        // Node 0 (AAA) and Node 2 (CCC) invalidated
        assert!(invalidated.contains(&nodes[0]), "AAA invalidated");
        assert!(invalidated.contains(&nodes[2]), "CCC invalidated");

        // Node 1 (BBB) shifts from [3,6) to [0,3)
        assert!(!invalidated.contains(&nodes[1]));
        assert_eq!(tracker.get(&uri, 0, 3, "1"), Some(nodes[1]));

        // Node 3 (DDD) shifts from [9,12) to [3,6)
        assert!(!invalidated.contains(&nodes[3]));
        assert_eq!(tracker.get(&uri, 3, 6, "3"), Some(nodes[3]));

        // Node 4 (EEE) shifts from [12,15) to [6,9)
        assert!(!invalidated.contains(&nodes[4]));
        assert_eq!(tracker.get(&uri, 6, 9, "4"), Some(nodes[4]));
    }

    #[test]
    fn test_apply_text_diff_prepend_content() {
        // "BBBCCC" → "AAABBBCCC": prepend content
        let tracker = NodeTracker::new();
        let uri = test_uri("prepend");

        let n1 = tracker.get_or_create(&uri, 0, 3, "B");
        let n2 = tracker.get_or_create(&uri, 3, 6, "C");

        let invalidated = tracker.apply_text_diff(&uri, "BBBCCC", "AAABBBCCC");

        // Insert at position 0 (zero-length edit at START of n1) → n1 invalidated
        assert!(
            invalidated.contains(&n1),
            "Node at insert point invalidated"
        );

        // n2 shifts from [3,6) to [6,9)
        assert!(!invalidated.contains(&n2));
        assert_eq!(tracker.get(&uri, 6, 9, "C"), Some(n2));
    }

    #[test]
    fn test_apply_text_diff_edit_at_document_end() {
        // Append to document: existing nodes unchanged
        let tracker = NodeTracker::new();
        let uri = test_uri("append");

        let n = tracker.get_or_create(&uri, 0, 10, "existing");

        let invalidated = tracker.apply_text_diff(&uri, "0123456789", "0123456789_appended");

        // Insert at end doesn't affect existing node
        assert!(!invalidated.contains(&n));
        assert_eq!(tracker.get(&uri, 0, 10, "existing"), Some(n));
    }

    #[test]
    fn test_apply_text_diff_multiple_zero_length_inserts() {
        // Multiple insertions at different positions
        let tracker = NodeTracker::new();
        let uri = test_uri("multi_insert");

        let n1 = tracker.get_or_create(&uri, 1, 2, "B");
        let _n2 = tracker.get_or_create(&uri, 3, 4, "D");

        // "ABCD" → "AXBCYD" (insert X after A, Y after C)
        let invalidated = tracker.apply_text_diff(&uri, "ABCD", "AXBCYD");

        // B at [1,2): insert X at position 1 → B's START=1 is at insert point → invalidated
        assert!(invalidated.contains(&n1), "B at insert point invalidated");

        // D at [3,4): insert Y at position 3 (original), but after X insert it's effectively
        // at running position 4. However, similar crate may produce different edits.
        // The key is the behavior, not the exact invalidation.
    }

    // =========================================================================
    // Phase 5: Stress Tests (Phase D)
    // =========================================================================

    #[test]
    fn test_reconstruct_many_alternating() {
        // 18 chars with 9 changes: alternating pattern
        let tracker = NodeTracker::new();
        let uri = test_uri("alternating");

        // Create nodes at alternating positions
        let nodes: Vec<_> = (0..9)
            .map(|i| tracker.get_or_create(&uri, i * 2, i * 2 + 1, leak_kind(i)))
            .collect();

        // "AXBXCXDXEXFXGXHXI" → "AYBYCYDYEYFYGYHYI"
        // Change every X to Y
        let old: String = (0..9)
            .map(|i| format!("{}X", (b'A' + i as u8) as char))
            .collect();
        let new: String = (0..9)
            .map(|i| format!("{}Y", (b'A' + i as u8) as char))
            .collect();

        let invalidated = tracker.apply_text_diff(&uri, &old, &new);

        // Nodes at even positions (A, B, C, ...) should be preserved
        // Only the X's change, which don't have nodes
        for (i, node) in nodes.iter().enumerate() {
            assert!(
                !invalidated.contains(node),
                "Node {} should not be invalidated",
                i
            );
        }
    }

    #[test]
    fn test_apply_text_diff_large_document() {
        // Performance sanity check with large document
        let tracker = NodeTracker::new();
        let uri = test_uri("large");

        // Create 1000 nodes
        let nodes: Vec<_> = (0..1000)
            .map(|i| tracker.get_or_create(&uri, i * 10, i * 10 + 5, leak_kind(i)))
            .collect();

        // Large document: 10000 bytes
        let old = "A".repeat(10000);
        // Change positions 500-510 and 9500-9510
        let mut new = old.clone();
        new.replace_range(500..510, "XXXXXXXXXX");
        new.replace_range(9500..9510, "YYYYYYYYYY");

        let invalidated = tracker.apply_text_diff(&uri, &old, &new);

        // Only nodes overlapping the edited ranges should be invalidated
        // Node 50 is at [500, 505) - edit [500, 510) → invalidated
        // Node 950 is at [9500, 9505) - edit [9500, 9510) → invalidated
        assert!(
            invalidated.contains(&nodes[50]),
            "Node at edit 1 invalidated"
        );
        assert!(
            invalidated.contains(&nodes[950]),
            "Node at edit 2 invalidated"
        );

        // Node 0 and 999 should be unchanged
        assert!(!invalidated.contains(&nodes[0]), "Node 0 unchanged");
        assert!(!invalidated.contains(&nodes[999]), "Node 999 unchanged");
    }

    // =========================================================================
    // Phase 5: Concurrency Tests (Phase D)
    // =========================================================================

    #[test]
    fn test_concurrent_apply_text_diff_individual_edits_no_panic() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = Url::parse("file:///concurrent_individual.md").unwrap();

        // Create many nodes
        for i in 0..100 {
            tracker.get_or_create(&uri, i * 10, i * 10 + 5, "block");
        }

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        // Multi-edit diff scenario
                        let old = "A".repeat(500);
                        let new = "B".repeat(50) + &"X".repeat(400) + &"C".repeat(50);
                        tracker.apply_text_diff(&uri, &old, &new);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("Thread should not panic");
        }
    }

    #[test]
    fn test_concurrent_get_and_apply_text_diff_individual() {
        use std::sync::Arc;
        use std::thread;

        let tracker = Arc::new(NodeTracker::new());
        let uri = Url::parse("file:///race_individual.md").unwrap();

        // Initial nodes
        tracker.get_or_create(&uri, 0, 10, "A");
        tracker.get_or_create(&uri, 20, 30, "B");

        let handles: Vec<_> = (0..2)
            .map(|id| {
                let tracker = Arc::clone(&tracker);
                let uri = uri.clone();
                thread::spawn(move || {
                    if id == 0 {
                        // Thread 0: applies multi-edit diff
                        for _ in 0..50 {
                            tracker.apply_text_diff(
                                &uri,
                                "0123456789____0123456789",
                                "XYZ_________ABC",
                            );
                        }
                    } else {
                        // Thread 1: creates nodes during edits
                        for i in 0..50 {
                            tracker.get_or_create(&uri, 5 + i, 8 + i, "C");
                        }
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("No panic expected");
        }
        // Test passes if no panic/deadlock - exact state is non-deterministic but safe
    }

    #[test]
    fn test_fallback_edit_delta_calculation() {
        // Verify delta = new_len - old_len for fallback edit
        let edit = EditInfo::new(0, 100, 50);
        assert_eq!(edit.delta(), -50, "Fallback edit delta should be -50");

        let edit2 = EditInfo::new(0, 50, 100);
        assert_eq!(edit2.delta(), 50, "Fallback edit delta should be +50");
    }

    // =========================================================================
    // Phase 5 Review Iteration 1: CRITICAL Test Gaps
    // =========================================================================

    #[test]
    fn test_position_collision_after_edit() {
        // CRITICAL: Test that position collision is handled correctly
        // When two nodes collapse to the same position after an edit,
        // one wins (non-deterministic) and the other is invalidated.
        let tracker = NodeTracker::new();
        let uri = test_uri("collision");

        // Setup: Two nodes with same start but different end
        // Node A at [0, 5) kind "X"
        // Node B at [0, 10) kind "X"
        // After edit [5, 10) delete 5 bytes:
        // A: start=0 not in [5,10) → unchanged [0, 5)
        // B: start=0 not in [5,10), end=10 in [5,10) → clamp end to 5, so [0, 5)
        // COLLISION! Both are [0, 5) kind "X"

        let n_a = tracker.get_or_create(&uri, 0, 5, "X");
        let n_b = tracker.get_or_create(&uri, 0, 10, "X"); // Same start, different end

        // Delete [5, 10) - this causes n_b's end to clamp to 5, colliding with n_a
        let edit = EditInfo::new(5, 10, 5); // delete bytes 5-10
        let invalidated = tracker.apply_input_edits(&uri, &[edit]);

        // Exactly one of them should be invalidated due to collision
        // HashMap iteration order is non-deterministic, so we can't predict which
        let a_invalidated = invalidated.contains(&n_a);
        let b_invalidated = invalidated.contains(&n_b);

        assert!(
            a_invalidated || b_invalidated,
            "At least one node should be invalidated due to collision"
        );

        // Verify exactly one node remains at [0, 5)
        let survivor = tracker.get(&uri, 0, 5, "X");
        assert!(survivor.is_some(), "One node should survive at [0, 5)");

        // The survivor should be the one NOT in invalidated
        let expected_survivor = if a_invalidated { n_b } else { n_a };
        assert_eq!(
            survivor,
            Some(expected_survivor),
            "Survivor should be the non-invalidated node"
        );
    }

    #[test]
    fn test_apply_input_edits_empty_slice_preserves_nodes() {
        // MAJOR: Test that empty edit slice is handled gracefully
        // (complements existing test_apply_input_edits_empty_slice)
        let tracker = NodeTracker::new();
        let uri = test_uri("empty_edits_preserve");

        // Multiple nodes should all be preserved
        let n1 = tracker.get_or_create(&uri, 0, 10, "A");
        let n2 = tracker.get_or_create(&uri, 20, 30, "B");
        let n3 = tracker.get_or_create(&uri, 50, 60, "C");

        let invalidated = tracker.apply_input_edits(&uri, &[]);

        assert!(invalidated.is_empty(), "Empty edits should return empty");
        assert_eq!(tracker.get(&uri, 0, 10, "A"), Some(n1));
        assert_eq!(tracker.get(&uri, 20, 30, "B"), Some(n2));
        assert_eq!(tracker.get(&uri, 50, 60, "C"), Some(n3));
    }

    #[test]
    #[should_panic(expected = "Invalid EditInfo")]
    fn test_edit_info_new_rejects_invalid_in_debug() {
        // MAJOR: Test that EditInfo::new panics on invalid input in debug builds
        // This is the first line of defense against invalid edits
        let _invalid = EditInfo::new(20, 10, 15); // Invalid: old_end=10 < start=20
    }

    #[test]
    fn test_apply_input_edits_forward_order_with_running_coords() {
        // MAJOR: Verify apply_input_edits() uses FORWARD-order processing
        // This is DIFFERENT from apply_text_diff() which uses REVERSE-order.
        //
        // LSP incremental edits use RUNNING coordinates - each edit's position is
        // relative to document state AFTER previous edits, so they must be
        // processed sequentially in array order.
        let tracker = NodeTracker::new();
        let uri = test_uri("lsp_order");

        // Document: "AABBCCDD" (8 bytes)
        // Node at [4, 8) covering "CCDD"
        let n = tracker.get_or_create(&uri, 4, 8, "block");

        // LSP sends two edits in FORWARD order with RUNNING coordinates:
        // Edit 1: Insert "XX" at position 0 → "XXAABBCCDD"
        // Edit 2: Delete [6, 8) in the NEW document → "XXAABBDD"
        //
        // If we process in forward order (correct for LSP):
        // After edit 1: n shifts from [4,8) to [6,10) (delta=+2)
        // After edit 2: n at [6,10), edit at [6,8) → start=6 in [6,8) → INVALIDATED
        //
        // If we incorrectly process in reverse order:
        // Edit 2 first: n at [4,8), edit at [6,8) → start=4 not in [6,8) → survives, end clamps
        // Edit 1 next: n shifts → wrong result

        let edit1 = EditInfo::new(0, 0, 2); // Insert 2 bytes at 0
        let edit2 = EditInfo::new(6, 8, 6); // Delete [6,8) in running coords

        let invalidated = tracker.apply_input_edits(&uri, &[edit1, edit2]);

        // With correct forward-order processing, n should be invalidated
        // because after edit1 shifts it to [6,10), edit2 at [6,8) invalidates it
        assert!(
            invalidated.contains(&n),
            "Node should be invalidated with forward-order LSP processing"
        );
    }

    #[test]
    fn test_reconstruct_produces_correct_edit_info_values() {
        // CRITICAL: Indirectly test reconstruct_individual_edits by verifying
        // the exact EditInfo values through apply_text_diff behavior
        let tracker = NodeTracker::new();
        let uri = test_uri("reconstruct_verify");

        // Create nodes at strategic positions to verify edit boundaries
        // "AAABBBCCC" → "AXXBBBCYY"
        // Edit 1: [1,3) "AA" → "XX" (replace at position 1)
        // Edit 2: [6,9) "CCC" → "CYY" (wait, this is [6,9) delete 3, insert 3)
        // Actually: "AAABBBCCC" vs "AXXBBBCYY"
        // Diff: A=A, A→X, A→X, B=B, B=B, B=B, C=C, C→Y, C→Y
        // So: Equal(A), Delete(AA)+Insert(XX) at [1,3), Equal(BBBC), Delete(CC)+Insert(YY) at [7,9)

        // Place nodes to detect exact edit boundaries
        let n_before_edit1 = tracker.get_or_create(&uri, 0, 1, "A0"); // [0,1) before first edit
        let n_at_edit1_start = tracker.get_or_create(&uri, 1, 2, "A1"); // [1,2) at first edit start
        let n_between = tracker.get_or_create(&uri, 4, 5, "B1"); // [4,5) between edits
        let n_at_edit2_start = tracker.get_or_create(&uri, 7, 8, "C1"); // [7,8) at second edit start

        let invalidated = tracker.apply_text_diff(&uri, "AAABBBCCC", "AXXBBBCYY");

        // n_before_edit1: start=0 not in any edit → unchanged
        assert!(
            !invalidated.contains(&n_before_edit1),
            "Node before first edit should survive"
        );

        // n_at_edit1_start: start=1 in [1,3) → invalidated
        assert!(
            invalidated.contains(&n_at_edit1_start),
            "Node at first edit start should be invalidated"
        );

        // n_between: start=4, neither edit affects it
        // After edit1 [1,3)→[1,3) delta=0, n_between unchanged
        // Edit2 is at [7,9) in original coords
        assert!(
            !invalidated.contains(&n_between),
            "Node between edits should survive"
        );

        // n_at_edit2_start: start=7 in [7,9) → invalidated
        assert!(
            invalidated.contains(&n_at_edit2_start),
            "Node at second edit start should be invalidated"
        );
    }

    #[test]
    fn test_reconstruct_multibyte_utf8_boundary_handling() {
        // HIGH PRIORITY: Test byte offset tracking across multi-byte UTF-8 characters
        // Emojis are 4 bytes each in UTF-8, CJK characters are typically 3 bytes
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_multibyte");

        // "A😀B" is 6 bytes: A(1) + 😀(4) + B(1)
        // "A😀😀B" is 10 bytes: A(1) + 😀(4) + 😀(4) + B(1)
        // Insert 😀 at byte position 5 (after first 😀)
        // Edit: [5,5) → [5,9) insert 4 bytes

        // Place nodes to verify byte positions are tracked correctly
        let n_before = tracker.get_or_create(&uri, 0, 1, "A"); // [0,1) "A"
        let n_emoji1 = tracker.get_or_create(&uri, 1, 5, "emoji1"); // [1,5) "😀"
        let n_after = tracker.get_or_create(&uri, 5, 6, "B"); // [5,6) "B"

        let invalidated = tracker.apply_text_diff(&uri, "A😀B", "A😀😀B");

        // n_before: start=0 not affected by insert at 5 → survives
        assert!(
            !invalidated.contains(&n_before),
            "Node before multi-byte insert should survive"
        );

        // n_emoji1: start=1 not affected, end=5 at insert point → should survive
        assert!(
            !invalidated.contains(&n_emoji1),
            "First emoji node should survive"
        );

        // n_after: start=5 at insert point → invalidated (zero-length insert at START)
        assert!(
            invalidated.contains(&n_after),
            "Node at multi-byte insert point should be invalidated"
        );

        // Verify n_after shifted correctly: [5,6) + 4 bytes → [9,10)
        assert_eq!(
            tracker.get(&uri, 9, 10, "B"),
            None, // Should be invalidated, not shifted
            "Invalidated node should not exist at new position"
        );
    }

    #[test]
    fn test_apply_text_diff_multibyte_replacement() {
        // Additional UTF-8 test: Replacement involving multi-byte characters
        let tracker = NodeTracker::new();
        let uri = test_uri("utf8_replacement");

        // "Hello世界" is 11 bytes: Hello(5) + 世(3) + 界(3)
        // "Hello世World" is 14 bytes: Hello(5) + 世(3) + World(5) + 界(1 removed)
        // Actually: "Hello世界" → "Hi世界" (replace "Hello" with "Hi")
        // Let's use simpler: "A😀C" → "A😀😀C"

        // "日本語" (Japanese) is 9 bytes: 日(3) + 本(3) + 語(3)
        // "日X語" replaces 本 with X: [3,6) → [3,4)

        let n_first = tracker.get_or_create(&uri, 0, 3, "日"); // [0,3) first char
        let n_middle = tracker.get_or_create(&uri, 3, 6, "本"); // [3,6) second char (to be replaced)
        let n_last = tracker.get_or_create(&uri, 6, 9, "語"); // [6,9) third char

        let invalidated = tracker.apply_text_diff(&uri, "日本語", "日X語");

        // n_first: start=0 not in edit range → survives
        assert!(!invalidated.contains(&n_first), "First CJK char survives");

        // n_middle: start=3 in edit range [3,6) → invalidated
        assert!(
            invalidated.contains(&n_middle),
            "Replaced CJK char should be invalidated"
        );

        // n_last: start=6, edit [3,6)→[3,4) delta=-2, shifts to [4,7)
        assert!(!invalidated.contains(&n_last), "Last CJK char survives");
        assert_eq!(
            tracker.get(&uri, 4, 7, "語"),
            Some(n_last),
            "Last char shifted correctly by -2 bytes"
        );
    }
}
