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

use super::injections::{DiscoveredBridgeRegion, DiscoveredInjections, SnapshotLayerTree};
use crate::language::injection::ResolvedInjection;

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
    /// Owned injection discovery derived from THIS snapshot's tree by the
    /// parse pass (ADR §3, don't-discover-twice): readers rebuild injection
    /// contexts from it instead of re-running the injection query per request.
    /// `None` when discovery didn't run or didn't qualify (tree-less snapshot,
    /// below the region gate, combined groups, or a not-yet-loaded injected
    /// parser tainted it) — readers then discover inline, byte-identically to
    /// the pre-lever path. Immutability of the snapshot is the tree-identity
    /// binding: text, tree, and regions are one value, so the regions can
    /// never be consumed against a different tree.
    pub(crate) injection_regions: Option<Arc<DiscoveredInjections>>,
    /// The bridge downstream's region list, derived by the same populate pass
    /// (`None` when populate didn't run for this snapshot or no bridge server
    /// was configured — the downstream then resolves inline; `Some(empty)`
    /// means genuinely no regions). Stamped with the settings generation the
    /// discovery ran under, like `resolved_regions`: a reload can change the
    /// injection query without publishing a new snapshot, and the consumer
    /// must fall back inline rather than open virtual documents for regions
    /// the new query would not discover.
    pub(crate) bridge_regions: Option<(u64, Arc<Vec<DiscoveredBridgeRegion>>)>,
    /// Fully resolved injection regions (`InjectionResolver::resolve_all`'s
    /// shape) from the same populate pass, for the whole-document readers —
    /// pull/push diagnostics, documentSymbol/Color, formatting's virt layer —
    /// which previously each re-ran the injection query per request. Same
    /// `None`/`Some(empty)` semantics as `bridge_regions`.
    /// Stamped with the settings generation the populate pass ran under: a
    /// reload (which can change injection resolution) bumps the generation
    /// WITHOUT publishing a new snapshot, so consumers gate on the stamp and
    /// fall back to inline resolution on mismatch (same pattern as
    /// `DiscoveredInjections.generation` and `layer_trees`).
    pub(crate) resolved_regions: Option<(u64, Arc<Vec<ResolvedInjection>>)>,
    /// Lazily-built, per-snapshot injection layer trees (document-order DFS,
    /// depth ≥ 1) for the captures/node layer walk: the FIRST walking request
    /// on this snapshot builds them (on the compute pool — the same cost the
    /// inline walk paid), and every subsequent walk on the same snapshot —
    /// captures full + delta both walk per keystroke, plus range and node
    /// lookups — reuses the parsed trees instead of re-running the injection
    /// query and re-parsing every region. Deliberately lazy rather than
    /// populate-built: non-captures users never pay, and the parse loop's
    /// publish latency is untouched.
    ///
    /// The `u64` is the settings generation the trees were built under: an
    /// injected-grammar auto-install bumps the generation WITHOUT publishing
    /// a new snapshot, and trees built pre-install would keep serving an
    /// empty embedded layer for the rest of this snapshot's life. A walker
    /// seeing a generation mismatch bypasses the cell and walks fresh (the
    /// pre-cache per-request cost) until the next snapshot rebuilds it.
    pub(crate) layer_trees: std::sync::OnceLock<(u64, Arc<Vec<SnapshotLayerTree>>)>,
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
    /// 2. no snapshot yet (bootstrap), **or** strictly newer `parsed_version`
    ///    (an equal-version double-publish must not swap the `Tree` under an
    ///    already-issued `result_id`) that does not **tree-downgrade**
    ///    (`Some → None`: no parse pass legitimately publishes tree-less over
    ///    a tree at a newer version — the reparse give-up paths are
    ///    bootstrap-gated, so such a publish can only be a give-up racing a
    ///    slower successful parse, and admitting it would strip serve-stale
    ///    readers of a usable tree until the next edit), **or** an
    ///    equal-version **tree upgrade** (`None → Some`): a give-up publish
    ///    (tree-less, releases parked first-parse waiters) must not block
    ///    the real parse of the same version that a later successful install
    ///    produces. Same version means same input text, so the upgrade only
    ///    adds information; the equal-version tree *swap* stays rejected.
    pub(crate) fn admits(&self, snapshot: &ParseSnapshot) -> bool {
        // The sentinel is reserved: no snapshot legitimately carries it (the
        // store's counter never draws it), so a closed slot admits nothing —
        // checked explicitly rather than relying on the counter's guarantee.
        snapshot.incarnation != CLOSED_INCARNATION
            && snapshot.incarnation == self.current_incarnation
            && self.snapshot.as_ref().is_none_or(|current| {
                let tree_downgrade = current.tree.is_some() && snapshot.tree.is_none();
                let tree_upgrade = current.tree.is_none() && snapshot.tree.is_some();
                (snapshot.parsed_version > current.parsed_version && !tree_downgrade)
                    || (snapshot.parsed_version == current.parsed_version && tree_upgrade)
            })
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
            injection_regions: None,
            bridge_regions: None,
            resolved_regions: None,
            layer_trees: std::sync::OnceLock::new(),
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

    fn snap_with_tree(incarnation: u64, parsed_version: u64) -> ParseSnapshot {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        ParseSnapshot {
            text: Arc::from("fn main() {}"),
            tree: Some(parser.parse("fn main() {}", None).unwrap()),
            language: Some("rust".to_string()),
            parsed_version,
            incarnation,
            injection_regions: None,
            bridge_regions: None,
            resolved_regions: None,
            layer_trees: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn newer_version_tree_downgrade_is_rejected() {
        let mut slot = SnapshotSlot::bootstrap(7);
        slot.snapshot = Some(Arc::new(snap_with_tree(7, 3)));
        assert!(
            !slot.admits(&snap(7, 4)),
            "a newer tree-less publish must not strip a usable tree (only a \
             give-up racing a slower parse can produce it)"
        );
        assert!(
            slot.admits(&snap_with_tree(7, 4)),
            "a newer tree-ful publish advances normally"
        );
    }

    #[test]
    fn equal_version_tree_upgrade_is_admitted_but_swap_and_downgrade_are_not() {
        let mut slot = SnapshotSlot::bootstrap(7);
        // A give-up publish (tree-less) landed first.
        slot.snapshot = Some(Arc::new(snap(7, 3)));
        assert!(
            slot.admits(&snap_with_tree(7, 3)),
            "a successful parse of the same version must upgrade a tree-less give-up"
        );
        assert!(
            !slot.admits(&snap_with_tree(6, 3)),
            "the incarnation clause is never bypassed by the upgrade"
        );

        // With a tree in place, the equal version is frozen again.
        slot.snapshot = Some(Arc::new(snap_with_tree(7, 3)));
        assert!(
            !slot.admits(&snap_with_tree(7, 3)),
            "an equal-version tree swap must stay rejected"
        );
        assert!(
            !slot.admits(&snap(7, 3)),
            "an equal-version tree downgrade must stay rejected"
        );
    }
}
