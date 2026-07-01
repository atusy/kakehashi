//! Versioned parse snapshots — the derived half of the parse-snapshot model
//! (`docs/architecture-decisions/parse-snapshot-architecture.md` §2).
//!
//! A parse pass publishes an immutable, internally-consistent [`ParseSnapshot`]
//! into the document's per-URI `watch` cell ([`SnapshotSlot`]); readers borrow
//! the latest slot wait-free and never block on a reparse. The slot co-locates
//! the current lifetime (`current_incarnation`) with the snapshot so the
//! publish guard is a single atomic check-then-act under the channel's own
//! lock — never a cross-map TOCTOU.

use std::sync::Arc;

use tree_sitter::Tree;

/// The reserved terminal incarnation `didClose` installs in a slot
/// (`u64::MAX`). Keeping the closed slot at its old incarnation would let a
/// stale same-lifetime publish pass both the incarnation check and the
/// `snapshot = None` bootstrap branch, resurrecting the closed document for a
/// parked first-parse waiter; the sentinel makes every later publish fail the
/// incarnation clause. The store's incarnation counter must never draw this
/// value (see `DocumentStore::next_incarnation`).
pub(crate) const CLOSED_INCARNATION: u64 = u64::MAX;

/// An immutable parse result: `text` is exactly the text `tree` was parsed
/// from (the gopls immutable-snapshot property), stamped with the input
/// version it derives from and the lifetime it belongs to.
///
/// `tree: Option` makes a **resolved-but-tree-less** outcome representable —
/// a parse that completed with no usable tree (no parser installed, install
/// failed, quarantined crashed grammar), distinct from the pre-first-parse
/// `None` slot: it advances `parsed_version` and releases first-parse waiters
/// to their empty/`null`/`ContentModified` fallbacks.
pub(crate) struct ParseSnapshot {
    pub(crate) text: Arc<str>,
    pub(crate) tree: Option<Tree>,
    /// The parse's own content-detected language — may refine the input-side
    /// `language_id` guess; never written back to the input (ADR §1).
    pub(crate) language: Option<String>,
    /// The `Document::content_version` the parse consumed.
    pub(crate) parsed_version: u64,
    /// The document lifetime the parse belongs to.
    pub(crate) incarnation: u64,
}

/// The per-URI `watch` value: the current lifetime plus the latest snapshot.
///
/// `snapshot = None` means no parse for this lifetime has completed yet
/// (bootstrap) — or, with `current_incarnation == CLOSED_INCARNATION`, that
/// the document closed (terminal).
#[derive(Clone)]
pub(crate) struct SnapshotSlot {
    pub(crate) current_incarnation: u64,
    pub(crate) snapshot: Option<Arc<ParseSnapshot>>,
}

impl SnapshotSlot {
    /// Fresh slot for a new document lifetime: no snapshot yet.
    pub(crate) fn bootstrap(incarnation: u64) -> Self {
        Self {
            current_incarnation: incarnation,
            snapshot: None,
        }
    }

    /// The terminal slot `didClose` installs (see [`CLOSED_INCARNATION`]).
    pub(crate) fn closed() -> Self {
        Self {
            current_incarnation: CLOSED_INCARNATION,
            snapshot: None,
        }
    }

    /// Whether `snapshot` may be installed in this slot — the one publish
    /// guard (ADR §2). Both clauses must hold; the incarnation clause is never
    /// bypassed:
    /// 1. `snapshot.incarnation == current_incarnation`, and
    /// 2. no snapshot yet (bootstrap) **or** strictly newer `parsed_version`
    ///    (an equal-version double-publish must not swap the `Tree` under an
    ///    already-issued `result_id`).
    pub(crate) fn admits(&self, snapshot: &ParseSnapshot) -> bool {
        // The sentinel is reserved: no snapshot legitimately carries it (the
        // store's counter never draws it), so a closed slot admits nothing —
        // checked explicitly rather than relying on the counter's guarantee.
        snapshot.incarnation != CLOSED_INCARNATION
            && snapshot.incarnation == self.current_incarnation
            && self
                .snapshot
                .as_ref()
                .is_none_or(|current| snapshot.parsed_version > current.parsed_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(incarnation: u64, parsed_version: u64) -> ParseSnapshot {
        ParseSnapshot {
            text: Arc::from(""),
            tree: None,
            language: None,
            parsed_version,
            incarnation,
        }
    }

    #[test]
    fn bootstrap_admits_first_publish_of_its_lifetime_only() {
        let slot = SnapshotSlot::bootstrap(7);
        assert!(slot.admits(&snap(7, 0)), "same-lifetime bootstrap publish");
        assert!(
            !slot.admits(&snap(6, 0)),
            "a straggler from a prior lifetime must be rejected even against None"
        );
    }

    #[test]
    fn versions_are_strictly_monotonic_within_a_lifetime() {
        let mut slot = SnapshotSlot::bootstrap(7);
        slot.snapshot = Some(Arc::new(snap(7, 3)));
        assert!(
            !slot.admits(&snap(7, 3)),
            "equal version must not re-publish"
        );
        assert!(!slot.admits(&snap(7, 2)), "older version must not publish");
        assert!(slot.admits(&snap(7, 4)));
    }

    #[test]
    fn closed_slot_rejects_every_publish() {
        let slot = SnapshotSlot::closed();
        assert!(!slot.admits(&snap(7, 0)));
        assert!(!slot.admits(&snap(CLOSED_INCARNATION, 0)), "reserved value");
    }
}
