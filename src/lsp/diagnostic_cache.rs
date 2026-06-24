//! Per-host diagnostic cache — the single source of truth for proactive
//! `textDocument/publishDiagnostics` (push-propagation-diagnostic-forwarding).
//!
//! A client replaces *all* diagnostics for a URI on each publish, and every
//! injection region of a host maps to the same host URI, so there can be exactly
//! **one** proactive publisher per host. Every proactive feed writes slots here
//! and one publisher merges every slot and emits the cumulative result — that is
//! what keeps sibling regions intact against the clobber.
//!
//! The cache is **nested** `host_uri → source → server` (not a flat tuple key) so
//! the targeted lifecycle evictions are O(1): a whole host on `didClose`
//! (`evict_host`) and a single source on an edit (`evict_source`). Per-connection
//! crash eviction (`evict_connection`) is the exception — it scans every slot to
//! find the dead connection's, which is fine on that rare path.
//!
//! ## Staging
//! Three source kinds are populated:
//! - [`DiagnosticSource::PullLayer`] — the host-event pull's already
//!   cross-layer-combined result, in host coordinates, as one blob.
//! - [`DiagnosticSource::Region`] — a downstream push for an injection region, in
//!   virtual coordinates, transformed to host coordinates at publish time.
//! - [`DiagnosticSource::Host`] — a downstream `_self` host-layer push for the
//!   real host document, in host coordinates (passes through unchanged).
//!
//! **Known interim overlap (deferred capability gate).** The pull feed fans out
//! to *every* configured server (region and host) with no capability
//! classification, so a server that both answers `textDocument/diagnostic`
//! (landing in `PullLayer`) *and* spontaneously pushes `publishDiagnostics`
//! (landing in a `Region` or `Host` slot) is counted twice. In practice the two
//! channels are disjoint — a pull-capable server should not also push (LSP) — so
//! this only *duplicates*, never *hides*, and only for a misbehaving server. The
//! deferred `pullFallback` / capability classification removes the overlap by
//! giving each server one native source.
//!
//! Region-invalidation eviction is implemented (`evict_source`, wired into the
//! edit path that orphans a region — #424) and crash/server eviction too
//! (`evict_connection`, wired into the reader-exit path — #469; slots are tagged
//! with the producing connection's id so a restart's slots survive). Still
//! deferred: per-source strategy fan-in (`preferred` sticky / `concatenated`
//! visible-walk; cross-source order is HashMap-nondeterministic until then), the
//! `content_epoch` version gate, and host-layer eager-open (diagnostics on open
//! before the first request).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::Diagnostic;
use url::Url;

use crate::error::LockResultExt;
use crate::lsp::bridge::{
    ProgressConnectionId, RegionOffset, VirtualDocumentUri, translate_virtual_range_to_host,
};

/// Which contributor a slot belongs to under a host document.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum DiagnosticSource {
    /// A downstream **push** for an injection region, identified by its stable
    /// region id (lazy-node-identity-tracking ULID). Held in **virtual**
    /// coordinates and transformed to host coordinates at publish time against
    /// the region's *current* offset (lazy re-anchor).
    Region(String),
    /// A downstream **push** from a `_self` host-layer server for the real host
    /// document (host-document-bridge). Held in **host** coordinates (the host
    /// path applies no translation), so it passes through the merge unchanged.
    /// Keyed per server, so several host servers on one host document coexist.
    Host,
    /// The host-event pull's cross-layer-combined result, in host coordinates.
    /// A single blob (one synthetic server slot) for now — a follow-up replaces
    /// it with per-`(region, server)` slots gated by `pullFallback`.
    PullLayer,
}

/// The synthetic server key under which the pull-layer blob is stored. Real
/// downstream servers can never collide (config names are validated and this
/// contains brackets).
pub(crate) const PULL_LAYER_SERVER: &str = "<pull-layer>";

/// One server's latest diagnostics for a `(host, source)`.
///
/// A repeat publish from the same server replaces this slot wholesale (the
/// within-server "latest wins" rule). An empty `diagnostics` is a kept-but-empty
/// slot: it contributes nothing to the merge but still exists, so a server going
/// from errors to clean clears only its own contribution.
// No `Default`: a `SlotEntry` is only ever built by `record` with an explicit
// `connection_id` tag, and a defaulted (untagged) slot would silently break crash
// eviction (#469), so the derive is deliberately omitted.
#[derive(Debug, Clone)]
pub(crate) struct SlotEntry {
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// The downstream connection that produced this slot, or `None` for the
    /// synthetic pull-layer blob (not tied to one connection's lifetime). A server
    /// restart mints a *new* connection id, so a later push from the restart
    /// replaces this slot and re-tags it — letting crash eviction
    /// ([`DiagnosticAggregator::evict_connection`]) drop only the dead connection's
    /// slots, never a live restart's (#469).
    pub(crate) connection_id: Option<ProgressConnectionId>,
}

/// `server name → slot`. Several servers can attach to one source.
pub(crate) type ServerSlots = HashMap<String, SlotEntry>;
/// `source → server slots` under one host.
pub(crate) type SourceSlots = HashMap<DiagnosticSource, ServerSlots>;

/// Merge a host's cached slots into the publishable diagnostic set, in host
/// coordinates.
///
/// - [`DiagnosticSource::Region`] slots hold virtual coordinates and are
///   transformed via `region_offsets[region_id]` (lazy re-anchor against the
///   region's *current* offset). A region with no current offset (it no longer
///   resolves, e.g. it was edited away) is skipped here; the edit that orphaned it
///   also evicts its now-stale slot (`evict_source`, #424), so it no longer lingers
///   until the host's `didClose`. (A crashed connection's slots are likewise
///   dropped on reader exit — `evict_connection`, #469.)
/// - [`DiagnosticSource::Host`] and [`DiagnosticSource::PullLayer`] slots are
///   already host-local and pass through unchanged.
///
/// Staged: results are concatenated (the default `textDocument/publishDiagnostics`
/// strategy). Per-source strategy fan-in (`preferred` sticky / `concatenated`
/// visible-walk) is a follow-up.
pub(crate) fn merge_cached_diagnostics(
    host: &Url,
    snapshot: SourceSlots,
    region_offsets: &HashMap<String, RegionOffset>,
) -> Vec<Diagnostic> {
    let host_str = host.as_str();
    let mut merged = Vec::new();
    // Consume the (already-cloned-from-cache) snapshot so diagnostics move into
    // the result instead of being cloned again.
    for (source, servers) in snapshot {
        match source {
            DiagnosticSource::Region(region_id) => {
                let Some(offset) = region_offsets.get(&region_id) else {
                    // Stale region: no current offset to anchor against.
                    continue;
                };
                for slot in servers.into_values() {
                    for mut diagnostic in slot.diagnostics {
                        transform_region_diagnostic(&mut diagnostic, offset, host_str);
                        merged.push(diagnostic);
                    }
                }
            }
            DiagnosticSource::Host | DiagnosticSource::PullLayer => {
                // Already host-local: pass through unchanged.
                for slot in servers.into_values() {
                    merged.extend(slot.diagnostics);
                }
            }
        }
    }
    merged
}

/// Largest set for which the allocation-free O(n²) match is used; above this, a
/// noisy server's payload would make the quadratic compare a republish bottleneck,
/// so we switch to the O(n) serialized-count path.
const MULTISET_QUADRATIC_CAP: usize = 64;

/// Whether two diagnostic slices are equal as **multisets** (same elements with the
/// same multiplicity, order-independent) — used by no-op-publish suppression to
/// ignore the merge's `HashMap` ordering (#422).
///
/// For the common small set (`<= MULTISET_QUADRATIC_CAP`) an O(n²) greedy match: for
/// each `a` element, consume the first unused equal `b` element — no allocation or
/// serialization. For a large set (a noisy server) that would be a republish
/// bottleneck, so fall back to an O(n) multiset compare keyed by each diagnostic's
/// serialized form (a total, order-independent key). Neither path collapses distinct
/// sets.
fn same_diagnostic_multiset(a: &[Diagnostic], b: &[Diagnostic]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if a.len() <= MULTISET_QUADRATIC_CAP {
        let mut matched = vec![false; b.len()];
        for da in a {
            let found = b
                .iter()
                .enumerate()
                .find(|(i, db)| !matched[*i] && *db == da);
            match found {
                Some((i, _)) => matched[i] = true,
                None => return false,
            }
        }
        return true;
    }
    // Large set: O(n) signed-count multiset compare. `+1` for each `a`, `-1` for each
    // `b`; equal multisets net to all-zero (the length check above means a non-empty
    // residual implies a real difference, not just a count imbalance).
    let mut counts: HashMap<String, isize> = HashMap::new();
    for d in a {
        *counts
            .entry(serde_json::to_string(d).unwrap_or_default())
            .or_default() += 1;
    }
    for d in b {
        *counts
            .entry(serde_json::to_string(d).unwrap_or_default())
            .or_default() -= 1;
    }
    counts.values().all(|&c| c == 0)
}

/// Transform a pushed region diagnostic from virtual to host coordinates.
///
/// Mirrors the pull path's `transform_diagnostic`
/// (`bridge::text_document::diagnostic`): the main range is shifted by the
/// region offset, and `relatedInformation` entries referencing **virtual** URIs
/// are dropped (clients cannot resolve them — without this they would leak to the
/// editor); entries on the same host document are translated, others kept as-is.
fn transform_region_diagnostic(diag: &mut Diagnostic, offset: &RegionOffset, host_uri: &str) {
    translate_virtual_range_to_host(&mut diag.range, offset);
    if let Some(related) = &mut diag.related_information {
        related.retain_mut(|info| {
            let uri_str = info.location.uri.as_str();
            if VirtualDocumentUri::is_virtual_uri(uri_str) {
                return false;
            }
            if uri_str == host_uri {
                translate_virtual_range_to_host(&mut info.location.range, offset);
            }
            true
        });
    }
}

/// The per-host diagnostic slot cache (see module docs).
#[derive(Default)]
pub(crate) struct DiagnosticAggregator {
    cache: Mutex<HashMap<Url, SourceSlots>>,
    /// Per-host republish locks. The publisher holds a host's lock across its
    /// snapshot→merge→publish so two concurrent republishes for the **same** host
    /// (a region push vs a host-event pull, on different tasks) cannot interleave
    /// and emit out of order — which would let a stale snapshot publish *after* a
    /// fresh one and permanently hide diagnostics on a quiescent file. Keying the
    /// lock by host means a slow editor publish for one host no longer stalls
    /// *every* host's republish, only that host's (#426). The outer `Mutex` is held
    /// only briefly to fetch/insert the per-host lock; the per-host
    /// `tokio::sync::Mutex` is the one held across the publish await.
    ///
    /// The map holds the `Arc` itself, so a host's lock is reused across its
    /// republishes (a quiet file's sequential republishes do not reallocate it
    /// between reclamation sweeps). It is reclaimed off the hot path by
    /// [`Self::reclaim_republish_locks`] (#466), which removes only entries the map
    /// *solely* owns (`Arc::strong_count == 1`): a live holder's `OwnedMutexGuard`
    /// or a queued waiter's pending `lock_owned()` future each keeps an extra
    /// strong ref, so an entry with any in-flight or queued republish is never
    /// removed — reclamation cannot race a republish into minting a second lock for
    /// the same host.
    republish_locks: Mutex<HashMap<Url, Arc<tokio::sync::Mutex<()>>>>,
    /// The last diagnostic set published to the editor per host, so a republish
    /// that would re-send an identical set is suppressed — the editor already has
    /// it, and a redundant `publishDiagnostics` is a needless flicker/noise source
    /// (#422). Updated under the host's republish lock (same-host republishes are
    /// serialized), and forgotten on `didClose` ([`Self::forget_published`]).
    last_published: Mutex<HashMap<Url, Vec<Diagnostic>>>,
}

impl DiagnosticAggregator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Acquire the republish lock for `host`; held by the publisher across
    /// snapshot→merge→publish so emissions for that host stay ordered (see field
    /// docs). Different hosts hold different locks and so never block each other.
    pub(crate) async fn lock_republish(&self, host: &Url) -> tokio::sync::OwnedMutexGuard<()> {
        // Brief outer lock to fetch-or-create this host's lock, then await it.
        // Clone the `Url` key only when inserting a new entry — the steady state
        // (lock already present) avoids the allocation on this per-republish path.
        let lock = {
            let mut locks = self
                .republish_locks
                .lock()
                .recover_poison("DiagnosticAggregator::republish_locks");
            match locks.get(host) {
                Some(lock) => Arc::clone(lock),
                None => Arc::clone(locks.entry(host.clone()).or_default()),
            }
        };
        lock.lock_owned().await
    }

    /// Drop republish-lock entries the map *solely* owns, bounding the
    /// `republish_locks` map so it does not retain one entry per distinct host
    /// ever republished (#466). Only entries with `Arc::strong_count == 1` are
    /// removed: a live holder's `OwnedMutexGuard` or a queued waiter's pending
    /// `lock_owned()` future each holds an extra strong ref, so an entry with any
    /// in-flight or queued republish is kept. This is race-free because the `Arc`
    /// is only ever cloned inside `lock_republish` under this same outer mutex, so
    /// while the sweep holds it no count can rise; `strong_count == 1` therefore
    /// means no holder/waiter exists to strand, and removing it merely lets the
    /// next republish mint a fresh lock (nothing to serialize against meanwhile).
    /// Called off the hot path (host `didClose`); a momentarily-idle but still-open
    /// host is collected too and cheaply re-creates its lock on its next republish.
    ///
    /// A push already past its open-document / region-resolve guard when the host
    /// closed can resume and re-create one entry after this sweep (the narrow
    /// resurrection window the deferred lifecycle/`content_epoch` gate closes
    /// generally — #422); it is bounded by distinct host URIs and self-heals, since
    /// every later `didClose` sweeps the whole map and collects it once idle.
    pub(crate) fn reclaim_republish_locks(&self) {
        let mut locks = self
            .republish_locks
            .lock()
            .recover_poison("DiagnosticAggregator::republish_locks");
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    }

    /// Record (replacing) one server's diagnostics for a `(host, source)`.
    ///
    /// An empty `diagnostics` keeps an empty slot — the merge skips it, so the
    /// server's prior diagnostics are cleared without dropping the slot key.
    ///
    /// `connection_id` tags the slot with the downstream connection that produced
    /// it (`None` for the synthetic pull-layer), so a later crash can evict only
    /// that connection's slots (#469). A restart re-pushes with a new id, replacing
    /// and re-tagging the slot.
    pub(crate) fn record(
        &self,
        host: &Url,
        source: DiagnosticSource,
        server: String,
        connection_id: Option<ProgressConnectionId>,
        diagnostics: Vec<Diagnostic>,
    ) {
        let mut cache = self.lock();
        // Look up by `&Url` first and clone the host key only when inserting a new
        // host entry, rather than `entry(host.clone())` cloning on every call.
        let source_slots = if let Some(source_slots) = cache.get_mut(host) {
            source_slots
        } else {
            cache.entry(host.clone()).or_default()
        };
        source_slots.entry(source).or_default().insert(
            server,
            SlotEntry {
                diagnostics,
                connection_id,
            },
        );
    }

    /// Replace the cached host-event pull blob for a host
    /// ([`DiagnosticSource::PullLayer`]). Equivalent to a single-server `record`.
    ///
    /// The pull-layer is a cross-connection aggregate, not a single connection's
    /// push, so its slot is tagged `None` and is never touched by crash eviction.
    pub(crate) fn set_pull_layer(&self, host: &Url, diagnostics: Vec<Diagnostic>) {
        self.record(
            host,
            DiagnosticSource::PullLayer,
            PULL_LAYER_SERVER.to_string(),
            None,
            diagnostics,
        );
    }

    /// Snapshot every source/server slot for a host, cloned for merging off-lock.
    /// Empty when the host has no slots.
    pub(crate) fn snapshot(&self, host: &Url) -> SourceSlots {
        let cache = self.lock();
        cache.get(host).cloned().unwrap_or_default()
    }

    /// Whether `host` has a cached `Region` push slot with **non-empty** diagnostics
    /// — i.e. a downstream diagnostic held in *virtual* coordinates that re-anchors
    /// against the region's current offset at publish time. Used to decide whether a
    /// host edit that moved regions needs a geometry re-merge (#422); `Host`/`PullLayer`
    /// slots are already host-local and don't move with a region edit. A *kept-but-empty*
    /// Region slot (a server cleared its diagnostics) has nothing to re-anchor, so it is
    /// ignored — otherwise a quiet/diagnostic-free file would keep paying the offset
    /// recompute on every edit.
    pub(crate) fn has_region_slots(&self, host: &Url) -> bool {
        let cache = self.lock();
        cache.get(host).is_some_and(|sources| {
            sources.iter().any(|(source, servers)| {
                matches!(source, DiagnosticSource::Region(_))
                    && servers.values().any(|slot| !slot.diagnostics.is_empty())
            })
        })
    }

    /// Drop everything for a host (host `didClose`). Returns whether it existed.
    pub(crate) fn evict_host(&self, host: &Url) -> bool {
        let mut cache = self.lock();
        cache.remove(host).is_some()
    }

    /// Whether `diagnostics` differs from the set last published for `host`,
    /// recording it as the new last when it does. Returns `false` when identical,
    /// so the caller skips a redundant `publishDiagnostics` the editor already has
    /// (#422). Called under the host's republish lock, so same-host calls serialize.
    ///
    /// The comparison is **order-independent**: `merge_cached_diagnostics` walks
    /// `HashMap`-keyed sources/servers, so the same logical set can serialize in a
    /// different order between republishes. The two sets are compared as **multisets**
    /// of full `Diagnostic` values ([`same_diagnostic_multiset`]), so a
    /// multi-source/multi-server host is not wrongly seen as "changed" merely because
    /// the merge order shuffled — while a genuine change in any field is still
    /// detected. The per-host diagnostic count is small, so the O(n²) match is cheap
    /// and avoids serializing every diagnostic.
    pub(crate) fn published_set_changed(&self, host: &Url, diagnostics: &[Diagnostic]) -> bool {
        let mut last = self
            .last_published
            .lock()
            .recover_poison("DiagnosticAggregator::last_published");
        // Single lookup via `get_mut`: update in place when the host is already
        // present (the common path), cloning the `host` key only on first insert.
        if let Some(prev) = last.get_mut(host) {
            if same_diagnostic_multiset(prev, diagnostics) {
                return false;
            }
            *prev = diagnostics.to_vec();
            return true;
        }
        last.insert(host.clone(), diagnostics.to_vec());
        true
    }

    /// Forget the last-published set for `host` (host `didClose`), so its entry
    /// does not linger and a later re-open publishes afresh.
    pub(crate) fn forget_published(&self, host: &Url) {
        self.last_published
            .lock()
            .recover_poison("DiagnosticAggregator::last_published")
            .remove(host);
    }

    /// Drop one `source`'s slots (every server) under `host` — e.g. a region
    /// invalidated by an edit, whose stale `Region` slots would otherwise linger
    /// until the whole host is closed (#424). Returns whether the source existed.
    /// The host entry is removed if it becomes empty.
    pub(crate) fn evict_source(&self, host: &Url, source: &DiagnosticSource) -> bool {
        let mut cache = self.lock();
        // Borrow by `&Url` (no key clone) — the common case (host present, other
        // sources remain) is a single lookup. The host entry is removed only when
        // this was its last source, which costs a second lookup but is rare.
        let Some(slots) = cache.get_mut(host) else {
            return false;
        };
        let removed = slots.remove(source).is_some();
        if slots.is_empty() {
            cache.remove(host);
        }
        removed
    }

    /// Number of entries currently held in the republish-lock map (test-only).
    #[cfg(test)]
    fn republish_lock_count(&self) -> usize {
        self.republish_locks
            .lock()
            .recover_poison("DiagnosticAggregator::republish_locks")
            .len()
    }

    /// Strong-ref count of a host's republish lock, or 0 if absent (test-only).
    /// Includes the map's own ref, so an idle entry reads 1; each holder
    /// (`OwnedMutexGuard`) or queued waiter's pending `lock_owned()` future adds 1.
    /// Lets a test observe that a queued waiter holds its own `Arc` to the lock,
    /// independent of the active holder's guard.
    #[cfg(test)]
    fn republish_lock_strong_count(&self, host: &Url) -> usize {
        self.republish_locks
            .lock()
            .recover_poison("DiagnosticAggregator::republish_locks")
            .get(host)
            .map_or(0, Arc::strong_count)
    }

    /// Drop every push slot produced by `connection_id` — a downstream connection
    /// whose reader exited (crash/respawn, #469) — returning the host URIs that
    /// lost at least one slot, so the caller can re-merge and republish them. The
    /// synthetic pull-layer (tagged `None`) is never touched, and a restart's
    /// re-push lands under a *new* connection id, so its slots survive this sweep.
    /// O(total slots); called only on the rare connection-exit path.
    ///
    /// This evicts only **pushed** slots. A pull-driven server that dies leaves its
    /// contribution in the cross-connection `PullLayer` blob until the next
    /// host-event pull recomputes it — an intentional asymmetry (#469 targets the
    /// push path; the pull layer self-refreshes on the next pull).
    pub(crate) fn evict_connection(&self, connection_id: ProgressConnectionId) -> Vec<Url> {
        let mut cache = self.lock();
        let mut affected = Vec::new();
        cache.retain(|host, sources| {
            let mut host_changed = false;
            sources.retain(|_source, servers| {
                let before = servers.len();
                servers.retain(|_server, slot| slot.connection_id != Some(connection_id));
                host_changed |= servers.len() != before;
                !servers.is_empty()
            });
            if host_changed {
                affected.push(host.clone());
            }
            !sources.is_empty()
        });
        affected
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Url, SourceSlots>> {
        // Recover from a poisoned lock rather than propagating a panic: a
        // diagnostic cache is best-effort state, never a correctness invariant
        // worth crashing the server over.
        self.cache
            .lock()
            .recover_poison("DiagnosticAggregator::cache")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn diag(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn host() -> Url {
        Url::parse("file:///doc.md").unwrap()
    }

    #[tokio::test]
    async fn per_host_republish_locks_are_independent_but_serialize_same_host() {
        use std::time::Duration;
        let agg = DiagnosticAggregator::new();
        let a = Url::parse("file:///a.md").unwrap();
        let b = Url::parse("file:///b.md").unwrap();

        // Hold host A's republish lock.
        let _guard_a = agg.lock_republish(&a).await;

        // A different host's lock must acquire without blocking on A's (#426) — if
        // it serialized globally, this would hang and the timeout would fire.
        let _guard_b = tokio::time::timeout(Duration::from_secs(1), agg.lock_republish(&b))
            .await
            .expect("a different host's republish lock must not block on host A's");

        // The SAME host's lock must serialize: a second acquire blocks while A's
        // guard is held.
        let same = tokio::time::timeout(Duration::from_millis(100), agg.lock_republish(&a)).await;
        assert!(
            same.is_err(),
            "the same host's republish lock must serialize (preserving per-host order)"
        );
    }

    #[tokio::test]
    async fn reclaim_removes_lock_entries_with_no_live_holder() {
        let agg = DiagnosticAggregator::new();
        let a = Url::parse("file:///a.md").unwrap();
        let b = Url::parse("file:///b.md").unwrap();

        // Two hosts republished, then both guards dropped — the map is now the sole
        // owner of each lock.
        drop(agg.lock_republish(&a).await);
        drop(agg.lock_republish(&b).await);
        assert_eq!(
            agg.republish_lock_count(),
            2,
            "an entry is recorded per host republished"
        );

        agg.reclaim_republish_locks();
        assert_eq!(
            agg.republish_lock_count(),
            0,
            "entries the map solely owns (no holder/waiter) are reclaimed"
        );
    }

    #[tokio::test]
    async fn reclaim_keeps_locks_with_a_live_holder() {
        let agg = DiagnosticAggregator::new();
        let a = Url::parse("file:///a.md").unwrap();

        // Hold host A's lock across the sweep: the holder adds a strong ref beyond
        // the map's, so removing it could let a concurrent acquire mint a second
        // lock — reclaim must keep it.
        let guard = agg.lock_republish(&a).await;
        assert_eq!(agg.republish_lock_strong_count(&a), 2, "map + holder");
        agg.reclaim_republish_locks();
        assert_eq!(
            agg.republish_lock_count(),
            1,
            "a lock with a live holder is retained"
        );
        // The same host re-acquires the *same* lock while the entry survives, so
        // serialization is preserved (a second acquire would block).
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                agg.lock_republish(&a)
            )
            .await
            .is_err(),
            "the retained lock still serializes the same host"
        );

        drop(guard);
        agg.reclaim_republish_locks();
        assert_eq!(
            agg.republish_lock_count(),
            0,
            "once the holder drops, the map-only entry is reclaimed"
        );
    }

    #[tokio::test]
    async fn reclaim_during_contention_preserves_the_waiter_handoff() {
        // The end-to-end shape reclaim must never break: a republish is queued
        // behind an in-flight one for the same host while a sweep runs. A queued
        // waiter's pending lock_owned() future owns an `Arc` to the lock, so the
        // entry has an owner beyond the map and survives the sweep — and the waiter
        // then acquires the *same* lock, preserving per-host ordering.
        let agg = Arc::new(DiagnosticAggregator::new());
        let a = Url::parse("file:///a.md").unwrap();

        // In-flight republish holds the lock: map (1) + holder (1) = 2 strong refs.
        let held = agg.lock_republish(&a).await;
        assert_eq!(agg.republish_lock_strong_count(&a), 2, "map + holder");

        // A second republish for the same host parks as a queued waiter.
        let agg2 = Arc::clone(&agg);
        let a2 = a.clone();
        let waiter = tokio::spawn(async move {
            let _g = agg2.lock_republish(&a2).await;
        });

        // Deterministically wait until the waiter has cloned its own `Arc` — that
        // happens synchronously (under the outer mutex) before it parks on
        // lock_owned(), so strong_count reaching 3 (map + holder + waiter) proves the
        // parked waiter is a distinct owner. No sleep: yield to let the spawned task run.
        let mut spins = 0;
        while agg.republish_lock_strong_count(&a) < 3 {
            assert!(spins < 10_000, "waiter never cloned its Arc");
            spins += 1;
            tokio::task::yield_now().await;
        }

        // The waiter holds a strong ref beyond the map's, so a sweep at this instant
        // must not remove the entry (doing so would strand the waiter on a stale lock).
        agg.reclaim_republish_locks();
        assert_eq!(
            agg.republish_lock_count(),
            1,
            "a lock with a queued waiter must survive reclamation"
        );

        // Releasing the in-flight guard lets the queued waiter acquire and finish —
        // proving reclaim did not strand it nor split it onto a second lock.
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("the queued waiter must acquire once the holder releases")
            .expect("the waiter task must not panic");
    }

    fn messages(slots: &ServerSlots) -> Vec<String> {
        let mut out: Vec<String> = slots
            .values()
            .flat_map(|s| s.diagnostics.iter().map(|d| d.message.clone()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn set_pull_layer_then_snapshot_returns_blob() {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host(), vec![diag("a"), diag("b")]);
        let snap = agg.snapshot(&host());
        let pull = &snap[&DiagnosticSource::PullLayer];
        assert_eq!(messages(pull), vec!["a", "b"]);
    }

    #[test]
    fn set_pull_layer_replaces_blob() {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host(), vec![diag("old")]);
        agg.set_pull_layer(&host(), vec![diag("new")]);
        let snap = agg.snapshot(&host());
        assert_eq!(
            messages(&snap[&DiagnosticSource::PullLayer]),
            vec!["new"],
            "latest replaces, not appends"
        );
    }

    #[test]
    fn empty_pull_layer_keeps_an_empty_slot() {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host(), vec![diag("x")]);
        agg.set_pull_layer(&host(), vec![]);
        let snap = agg.snapshot(&host());
        assert!(
            snap[&DiagnosticSource::PullLayer][PULL_LAYER_SERVER]
                .diagnostics
                .is_empty()
        );
    }

    fn diag_at(message: &str, line: u32, col: u32) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn merge_concatenates_pull_layer() {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host(), vec![diag("a"), diag("b")]);
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        let mut msgs: Vec<&str> = merged.iter().map(|d| d.message.as_str()).collect();
        msgs.sort();
        assert_eq!(msgs, vec!["a", "b"]);
    }

    #[test]
    fn merge_of_empty_snapshot_is_empty() {
        assert!(merge_cached_diagnostics(&host(), SourceSlots::new(), &HashMap::new()).is_empty());
    }

    #[test]
    fn merge_transforms_region_slots_to_host_coords() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "luals".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("err", 0, 2)],
        );
        let mut offsets = HashMap::new();
        // Region r1 sits at host line 5, column offset 4 on its first line.
        offsets.insert("r1".to_string(), RegionOffset::new(5, 4));
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &offsets);
        assert_eq!(merged.len(), 1);
        // line 0 -> 0+5, character 2 -> 2+4
        assert_eq!(merged[0].range.start, Position::new(5, 6));
    }

    #[test]
    fn merge_drops_virtual_related_information_and_translates_host() {
        use std::str::FromStr;
        use tower_lsp_server::ls_types::{DiagnosticRelatedInformation, Location, Uri as LsUri};

        let host_uri = "file:///doc.md";
        let related = |uri: &str, line: u32| DiagnosticRelatedInformation {
            location: Location {
                uri: LsUri::from_str(uri).unwrap(),
                range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            },
            message: uri.to_string(),
        };
        let mut d = diag_at("err", 0, 0);
        d.related_information = Some(vec![
            related("file:///doc.md/kakehashi-virtual-uri-R.lua", 0), // virtual -> dropped
            related(host_uri, 0),                                     // host -> translated
            related("file:///other.lua", 3),                          // other -> kept as-is
        ]);

        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "luals".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![d],
        );
        let mut offsets = HashMap::new();
        offsets.insert("r1".to_string(), RegionOffset::new(5, 0));
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &offsets);

        let related = merged[0].related_information.as_ref().unwrap();
        assert_eq!(related.len(), 2, "virtual-URI related info is dropped");
        let host_entry = related
            .iter()
            .find(|r| r.location.uri.as_str() == host_uri)
            .unwrap();
        assert_eq!(
            host_entry.location.range.start,
            Position::new(5, 0),
            "host-document related info is translated by the offset"
        );
        let other = related
            .iter()
            .find(|r| r.location.uri.as_str() == "file:///other.lua")
            .unwrap();
        assert_eq!(
            other.location.range.start,
            Position::new(3, 0),
            "other-document related info keeps its coordinates"
        );
    }

    #[test]
    fn merge_skips_region_without_current_offset() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("gone".into()),
            "luals".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("stale")],
        );
        // No offset for "gone" -> region is stale -> skipped.
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_combines_region_push_and_pull_layer() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "luals".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("push", 0, 0)],
        );
        agg.set_pull_layer(&host(), vec![diag_at("pull", 9, 0)]);
        let mut offsets = HashMap::new();
        offsets.insert("r1".to_string(), RegionOffset::new(2, 0));
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &offsets);
        let mut msgs: Vec<&str> = merged.iter().map(|d| d.message.as_str()).collect();
        msgs.sort();
        assert_eq!(msgs, vec!["pull", "push"]);
    }

    #[test]
    fn merge_passes_host_push_slots_through_unchanged() {
        let agg = DiagnosticAggregator::new();
        // Two host servers push for the same host doc; both pass through in host
        // coords (no transform), keyed per server.
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "lua_ls".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("hostA", 7, 3)],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "selene".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("hostB", 8, 0)],
        );
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        let mut by_msg: Vec<(&str, Position)> = merged
            .iter()
            .map(|d| (d.message.as_str(), d.range.start))
            .collect();
        by_msg.sort();
        assert_eq!(
            by_msg,
            vec![
                ("hostA", Position::new(7, 3)),
                ("hostB", Position::new(8, 0))
            ],
            "host push slots pass through in host coordinates, per server"
        );
    }

    #[test]
    fn evict_host_drops_all() {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host(), vec![diag("h")]);
        assert!(agg.evict_host(&host()));
        assert!(agg.snapshot(&host()).is_empty());
        assert!(!agg.evict_host(&host()), "second evict is a no-op");
    }

    #[test]
    fn evict_source_drops_only_that_source() {
        let agg = DiagnosticAggregator::new();
        let region = DiagnosticSource::Region("R1".to_string());
        agg.record(
            &host(),
            region.clone(),
            "srv".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("r")],
        );
        agg.set_pull_layer(&host(), vec![diag("p")]);

        assert!(agg.evict_source(&host(), &region));
        let snap = agg.snapshot(&host());
        assert!(!snap.contains_key(&region), "the region source is evicted");
        assert!(
            snap.contains_key(&DiagnosticSource::PullLayer),
            "other sources for the host are untouched"
        );
        assert!(
            !agg.evict_source(&host(), &region),
            "a second evict of the same source is a no-op"
        );
    }

    #[test]
    fn evict_source_removes_the_host_when_its_last_source_goes() {
        let agg = DiagnosticAggregator::new();
        let region = DiagnosticSource::Region("R1".to_string());
        agg.record(
            &host(),
            region.clone(),
            "srv".to_string(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("r")],
        );
        assert!(agg.evict_source(&host(), &region));
        assert!(
            !agg.lock().contains_key(&host()),
            "the host entry itself is dropped (not left present-but-empty) once its last source is evicted"
        );
        assert!(
            !agg.evict_source(&host(), &region),
            "evicting a source from an absent host is a no-op"
        );
    }

    #[test]
    fn evict_connection_drops_only_that_connections_slots() {
        let agg = DiagnosticAggregator::new();
        let conn_a = ProgressConnectionId::for_test(1);
        let conn_b = ProgressConnectionId::for_test(2);
        // conn_a pushed a region slot; conn_b a host slot; plus a pull-layer (None).
        agg.record(
            &host(),
            DiagnosticSource::Region("r".into()),
            "luals".into(),
            Some(conn_a),
            vec![diag("a")],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "selene".into(),
            Some(conn_b),
            vec![diag("b")],
        );
        agg.set_pull_layer(&host(), vec![diag("pull")]);

        let affected = agg.evict_connection(conn_a);
        assert_eq!(
            affected,
            vec![host()],
            "the host that lost a slot is reported"
        );
        let snap = agg.snapshot(&host());
        assert!(
            !snap.contains_key(&DiagnosticSource::Region("r".into())),
            "conn_a's region slot is evicted"
        );
        assert!(
            snap.contains_key(&DiagnosticSource::Host),
            "conn_b's host slot survives"
        );
        assert!(
            snap.contains_key(&DiagnosticSource::PullLayer),
            "the pull-layer (tagged None) is never touched by connection eviction"
        );
    }

    #[test]
    fn evict_connection_spares_a_restarts_replaced_slot() {
        let agg = DiagnosticAggregator::new();
        let dead = ProgressConnectionId::for_test(1);
        let restart = ProgressConnectionId::for_test(2);
        // The dead connection pushed, then a restart re-pushed for the *same*
        // (host, source, server) — `record` replaced and re-tagged the slot.
        agg.record(
            &host(),
            DiagnosticSource::Region("r".into()),
            "luals".into(),
            Some(dead),
            vec![diag("old")],
        );
        agg.record(
            &host(),
            DiagnosticSource::Region("r".into()),
            "luals".into(),
            Some(restart),
            vec![diag("new")],
        );

        let affected = agg.evict_connection(dead);
        assert!(
            affected.is_empty(),
            "nothing tagged with the dead id remains, so no host is republished"
        );
        let snap = agg.snapshot(&host());
        let slot = &snap[&DiagnosticSource::Region("r".into())]["luals"];
        assert_eq!(slot.connection_id, Some(restart));
        assert_eq!(
            slot.diagnostics[0].message, "new",
            "the restart's slot is intact"
        );
    }

    #[test]
    fn evict_connection_then_restart_repush_repopulates() {
        // The reverse ordering: the dead connection is evicted *before* the restart
        // re-pushes (evict-before-repush). The slot is cleared, then the restart's
        // push under a new id re-adds it — no slot is ever wrongly retained or lost.
        let agg = DiagnosticAggregator::new();
        let dead = ProgressConnectionId::for_test(1);
        let restart = ProgressConnectionId::for_test(2);
        let region = DiagnosticSource::Region("r".into());
        agg.record(
            &host(),
            region.clone(),
            "luals".into(),
            Some(dead),
            vec![diag("old")],
        );

        assert_eq!(agg.evict_connection(dead), vec![host()]);
        assert!(agg.snapshot(&host()).is_empty(), "the dead slot is cleared");

        agg.record(
            &host(),
            region.clone(),
            "luals".into(),
            Some(restart),
            vec![diag("new")],
        );
        let snap = agg.snapshot(&host());
        let slot = &snap[&region]["luals"];
        assert_eq!(slot.connection_id, Some(restart));
        assert_eq!(slot.diagnostics[0].message, "new");
        // A late eviction for the already-dead id now finds nothing.
        assert!(agg.evict_connection(dead).is_empty());
    }

    #[test]
    fn evict_connection_removes_emptied_host_and_ignores_unknown() {
        let agg = DiagnosticAggregator::new();
        let conn = ProgressConnectionId::for_test(1);
        agg.record(
            &host(),
            DiagnosticSource::Region("r".into()),
            "luals".into(),
            Some(conn),
            vec![diag("x")],
        );
        // Unknown connection: no slot matches, so no host is affected.
        assert!(
            agg.evict_connection(ProgressConnectionId::for_test(99))
                .is_empty(),
            "evicting an unknown connection is a no-op"
        );
        // The real connection: its only slot goes, emptying and dropping the host.
        assert_eq!(agg.evict_connection(conn), vec![host()]);
        assert!(
            !agg.lock().contains_key(&host()),
            "the now-empty host entry is removed, not left present-but-empty"
        );
    }

    #[test]
    fn published_set_changed_suppresses_an_identical_republish() {
        let agg = DiagnosticAggregator::new();
        // First publish always counts as changed (nothing published before).
        assert!(agg.published_set_changed(&host(), &[diag("a")]));
        // An identical set is suppressed.
        assert!(!agg.published_set_changed(&host(), &[diag("a")]));
        // A different set publishes again.
        assert!(agg.published_set_changed(&host(), &[diag("a"), diag("b")]));
        assert!(!agg.published_set_changed(&host(), &[diag("a"), diag("b")]));
        // Going back to empty is a change (clears the editor).
        assert!(agg.published_set_changed(&host(), &[]));
        assert!(!agg.published_set_changed(&host(), &[]));
    }

    #[test]
    fn forget_published_lets_the_next_identical_set_publish_again() {
        let agg = DiagnosticAggregator::new();
        assert!(agg.published_set_changed(&host(), &[diag("a")]));
        assert!(!agg.published_set_changed(&host(), &[diag("a")]));
        // After didClose forgets the host, a re-open's identical first set publishes.
        agg.forget_published(&host());
        assert!(
            agg.published_set_changed(&host(), &[diag("a")]),
            "a re-opened host must publish its first set even if it matches the pre-close one"
        );
    }

    #[test]
    fn published_set_changed_ignores_merge_order() {
        let agg = DiagnosticAggregator::new();
        let a = diag_at("a", 0, 0);
        let b = diag_at("b", 1, 0);
        assert!(agg.published_set_changed(&host(), &[a.clone(), b.clone()]));
        // The same logical set in a different (HashMap-shuffled) order is NOT a change.
        assert!(
            !agg.published_set_changed(&host(), &[b, a]),
            "suppression must be order-independent for multi-source/server hosts"
        );
    }

    #[test]
    fn published_set_changed_large_set_is_order_independent_and_exact() {
        // > MULTISET_QUADRATIC_CAP diagnostics → exercises the O(n) serialized-count
        // path. It must stay order-independent and still detect a real change.
        let agg = DiagnosticAggregator::new();
        let big: Vec<Diagnostic> = (0..100u32).map(|i| diag_at("m", i, 0)).collect();
        assert!(agg.published_set_changed(&host(), &big));
        let mut shuffled = big.clone();
        shuffled.reverse();
        assert!(
            !agg.published_set_changed(&host(), &shuffled),
            "large set: a mere reorder is not a change"
        );
        let mut changed = big.clone();
        changed[50] = diag_at("DIFFERENT", 50, 0);
        assert!(
            agg.published_set_changed(&host(), &changed),
            "large set: a single differing diagnostic is a change"
        );
    }

    #[test]
    fn has_region_slots_only_true_for_region_sources() {
        let agg = DiagnosticAggregator::new();
        assert!(!agg.has_region_slots(&host()), "empty host has none");

        // A Host push slot is host-local — it does not count as a region slot.
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "selene".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("h")],
        );
        assert!(
            !agg.has_region_slots(&host()),
            "a Host slot is host-local and does not need geometry re-anchoring"
        );

        // A kept-but-EMPTY Region slot (a server cleared its diagnostics) has nothing
        // to re-anchor, so it must NOT trigger the geometry re-merge.
        agg.record(
            &host(),
            DiagnosticSource::Region("r".into()),
            "luals".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![],
        );
        assert!(
            !agg.has_region_slots(&host()),
            "an empty Region slot has no diagnostics to re-anchor"
        );

        // A NON-EMPTY Region push slot does.
        agg.record(
            &host(),
            DiagnosticSource::Region("r".into()),
            "luals".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("r")],
        );
        assert!(agg.has_region_slots(&host()));
    }
}
