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
//! **One native source per server (#425).** A server that both answers
//! `textDocument/diagnostic` (landing in `PullLayer`) *and* spontaneously pushes
//! `publishDiagnostics` (landing in a `Region` or `Host` slot) would be counted
//! twice. The proactive
//! [`DiagnosticPublisher`](crate::lsp::lsp_impl::coordinator::DiagnosticPublisher)'s
//! `filter_pull_driven_push_slots` drops a
//! **pull-driven** server's push slots from the publish whenever a `PullLayer`
//! blob is present (classification is live via
//! [`LanguageServerPool::pull_driven_servers`](crate::lsp::bridge::LanguageServerPool)),
//! so the pull contribution wins and the push is kept only as the proactive
//! source for genuinely **push-driven** servers. The slot stays cached either
//! way (so a pull-driven server's spontaneous push still closes #380 when
//! `pullFallback` is off and no `PullLayer` exists).
//!
//! Region-invalidation eviction is implemented (`evict_source`, wired into the
//! edit path that orphans a region — #424) and crash/server eviction too
//! (`evict_connection`, wired into the reader-exit path — #469; slots are tagged
//! with the producing connection's id so a restart's slots survive). Still
//! deferred: the `preferred` sticky-election strategy fan-in (the `concatenated`
//! strategy — keep every server, in a deterministic position order — ships in #423;
//! `preferred` needs a per-source version baseline #422 left unbuilt) and host-layer
//! eager-open (diagnostics on open before the first request). The `content_epoch`
//! version gate was evaluated and rejected (it converts a self-healing stale-overwrite
//! into a reopen-resurrection hide); the stale-overwrite is left self-healing.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
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
/// Strategy: the `concatenated` strategy (keep every server's diagnostics) in a
/// deterministic position order (see the sort below, #423). The `preferred`
/// sticky-election strategy is a follow-up — it needs a per-source version baseline
/// (`Veff`), which #422 left unbuilt.
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
    // Deterministic published order (#423): the cache walks `HashMap`-keyed sources
    // and servers, so without this the array order varies between republishes for a
    // multi-source/multi-server host. This is the `concatenated` strategy (every
    // server's diagnostics are kept); the `preferred` election is deferred (it needs a
    // per-source version baseline, which #422 left unbuilt).
    sort_diagnostics(&mut merged);
    merged
}

/// Order diagnostics deterministically: by host position (top-to-bottom —
/// "order cross-region merge by region start position"), then by the cheap
/// distinguishing fields (message, source, severity).
///
/// The final tiebreak is each diagnostic's full serialized form — a **total**
/// order over every field (code, tags, data, relatedInformation, …), so two
/// genuinely distinct diagnostics at the same span can never fall back to
/// input (e.g. `HashMap`-walk or fan-in completion) order. It is evaluated
/// **lazily** (`then_with`), only when every cheap field above already ties,
/// so the serialization is off the hot path. Fully-identical diagnostics
/// serialize equally and keep their (immaterial) relative order.
///
/// Shared by the proactive publish merge ([`merge_cached_diagnostics`], #423)
/// and the client-pull response (`diagnostic_impl`): the pull's `resultId` is
/// a content hash of the serialized items, so the same logical set must
/// serialize identically across pulls regardless of fan-in completion order.
///
/// The tiebreak's `unwrap_or_default` cannot fire in practice: `ls_types`
/// values serialize infallibly (string-keyed maps only, no non-UTF-8, no
/// custom `Serialize`), and the idiom predates this sort's extraction. Were
/// serialization ever to fail, the cheap keys above still order by
/// position/message/source/severity — only fully-tied distinct diagnostics
/// could then keep input order, and the `resultId`'s length suffix plus the
/// per-item hash of everything that DID serialize bound the unchanged-report
/// consequence to the same accepted 2^-64 collision class documented at
/// `diagnostic_result_id`.
pub(crate) fn sort_diagnostics(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        let key = |d: &Diagnostic| {
            (
                d.range.start.line,
                d.range.start.character,
                d.range.end.line,
                d.range.end.character,
            )
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| a.message.cmp(&b.message))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.severity.cmp(&b.severity))
            .then_with(|| {
                serde_json::to_string(a)
                    .unwrap_or_default()
                    .cmp(&serde_json::to_string(b).unwrap_or_default())
            })
    });
}

/// Whether `slots` holds any **non-empty** `Region` push slot — the shared
/// predicate behind both [`DiagnosticAggregator::has_region_slots`] (the
/// reparse loop's post-parse republish gate, on the raw cache) and
/// `republish`'s `needs_geometry` (on the filtered publish snapshot). One
/// implementation on purpose: the geometry-unknown deferral is retried only
/// while this predicate holds, so the two call sites must never diverge.
pub(crate) fn has_live_region_slots(slots: &SourceSlots) -> bool {
    slots.iter().any(|(source, servers)| {
        matches!(source, DiagnosticSource::Region(_))
            && servers.values().any(|slot| !slot.diagnostics.is_empty())
    })
}

/// Every distinct server name with a **push** slot (`Region`/`Host`, never
/// `PullLayer`) in `snapshot`. Path B's `pushFallback` fold uses this to
/// classify which cached pushers are push-driven (#425). Takes the snapshot the
/// caller already holds so the candidate set and the folded slots come from the
/// **same** read (no TOCTOU window across the classifying `await`).
pub(crate) fn push_slot_servers(snapshot: &SourceSlots) -> std::collections::HashSet<&str> {
    let mut servers = std::collections::HashSet::new();
    for (source, slots) in snapshot {
        if matches!(source, DiagnosticSource::PullLayer) {
            continue;
        }
        servers.extend(slots.keys().map(String::as_str));
    }
    servers
}

/// Cached **push** diagnostics from `snapshot`, partitioned by layer, for every
/// `(source, server)` the `include` predicate accepts — Path B's `pushFallback`
/// fold (#425). `Region` slots are transformed to host coordinates via
/// `region_offsets` (a region with no current offset is skipped, like the
/// proactive merge); `Host` slots are already host-local. The `PullLayer` blob
/// is never returned — Path B live-pulls pull-driven servers, so folding the
/// blob would double-count them.
///
/// The caller passes the snapshot (rather than re-reading the cache) so the
/// folded slots match the snapshot its candidate classification was derived
/// from. Returns `(virt_layer_items, host_layer_items)` so the caller can extend
/// each layer's live-pull result before the cross-layer combine.
pub(crate) fn cached_push_diagnostics(
    host: &Url,
    snapshot: SourceSlots,
    region_offsets: &HashMap<String, RegionOffset>,
    include: impl Fn(&DiagnosticSource, &str) -> bool,
) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    let host_str = host.as_str();
    let mut region_items = Vec::new();
    let mut host_items = Vec::new();
    for (source, servers) in snapshot {
        match &source {
            DiagnosticSource::PullLayer => {}
            DiagnosticSource::Region(region_id) => {
                let Some(offset) = region_offsets.get(region_id) else {
                    continue;
                };
                for (server, slot) in servers {
                    if !include(&source, &server) {
                        continue;
                    }
                    for mut diagnostic in slot.diagnostics {
                        transform_region_diagnostic(&mut diagnostic, offset, host_str);
                        region_items.push(diagnostic);
                    }
                }
            }
            DiagnosticSource::Host => {
                for (server, slot) in servers {
                    if !include(&source, &server) {
                        continue;
                    }
                    host_items.extend(slot.diagnostics);
                }
            }
        }
    }
    (region_items, host_items)
}

/// Largest set for which the serialization-free O(n²) match is used; above this, a
/// noisy server's payload would make the quadratic compare a republish bottleneck,
/// so we switch to the O(n) serialized-count path.
const MULTISET_QUADRATIC_CAP: usize = 64;

/// Whether two diagnostic slices are equal as **multisets** (same elements with the
/// same multiplicity, order-independent) — used by no-op-publish suppression to
/// ignore the merge's `HashMap` ordering (#422).
///
/// For the common small set (`<= MULTISET_QUADRATIC_CAP`) an O(n²) greedy match: for
/// each `a` element, consume the first unused equal `b` element — no serialization
/// (just a small `bool` match-mask). For a large set (a noisy server) that would be a
/// republish bottleneck, so fall back to an O(n) multiset compare keyed by each
/// diagnostic's serialized form (a total, order-independent key). Neither path
/// collapses distinct sets.
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
    //
    // A serialization failure (effectively impossible for `Diagnostic`) must not
    // collapse distinct diagnostics onto an empty key — that could read two different
    // sets as equal and wrongly suppress a needed publish. So on any `Err`, bail out
    // reporting "not equal" (the caller then publishes — safe, never hides).
    let mut counts: HashMap<String, isize> = HashMap::new();
    for d in a {
        let Ok(key) = serde_json::to_string(d) else {
            return false;
        };
        *counts.entry(key).or_default() += 1;
    }
    for d in b {
        let Ok(key) = serde_json::to_string(d) else {
            return false;
        };
        *counts.entry(key).or_default() -= 1;
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
    /// The last **merged-and-recorded** diagnostic set per host, so a republish
    /// producing an identical set is suppressed — a redundant re-emission is
    /// needless flicker/noise (#422) — and the change signal driving the
    /// refresh nudging stays exact. The wire send can lag this record (the
    /// quiet window withholds it, tracked by [`WireGate::dirty`]) or be
    /// skipped entirely (the publish seal: a pull-first client receives the
    /// set via re-pull instead). Updated under the host's republish lock
    /// (same-host republishes are serialized), and forgotten on `didClose`
    /// ([`Self::forget_published`]).
    last_published: Mutex<HashMap<Url, Vec<Diagnostic>>>,
    /// Single-flight guard for the **workspace-wide** `workspace/diagnostic/refresh`
    /// nudge (#497). `workspace/diagnostic/refresh` is param-less and workspace-wide,
    /// so concurrent refreshes are redundant; without this, a burst of set-changing
    /// pushes spawns one detached refresh request *each*, every one a tower-lsp
    /// pending-request entry until the editor acks. This collapses a burst: at most
    /// one refresh is in flight (awaiting the editor's ack); further requests during
    /// that window set `pending`, which fires exactly one more refresh on
    /// completion. Drives [`Self::try_begin_refresh`]/[`Self::finish_refresh`].
    refresh_flight: Mutex<RefreshFlight>,
    /// Per-host coverage versions for the refresh **coverage gate** (#497, commit 2).
    /// `current` bumps on every set-changing republish (the editor's pulled view is
    /// now stale); a pull captures the epoch plus `current` version through
    /// [`Self::coverage_stamp`], and `served` records that version. A
    /// gated refresh fires only when some host has `current > served` ("dirty") — so
    /// a change the editor already re-pulled (its own `didChange` pull advances
    /// `served`) sends no redundant nudge. Drives [`Self::bump_current`],
    /// [`Self::mark_served`], [`Self::is_dirty`]; forgotten on `didClose`.
    coverage: Mutex<HashMap<Url, HostCoverage>>,
    /// Mints a distinct lifetime for each newly-created coverage entry. A pull
    /// captures this epoch and the current version in [`Self::coverage_stamp`];
    /// after `didClose` removes the entry, its completion must not advance a
    /// later reopened lifetime's `served` value.
    next_coverage_epoch: AtomicU64,
    /// Hosts whose LAST pull was answered degraded — the bounded parse wait
    /// lapsed while the aggregator held live region pushes, so the response
    /// was missing the region fold and deliberately did not `mark_served`.
    /// The reparse loop's post-parse backstop consumes an entry
    /// ([`Self::take_degraded_pull`]) to request the recovery
    /// `workspace/diagnostic/refresh` for exactly the hosts that owe one —
    /// keying the recovery on this instead of the workspace-wide coverage
    /// dirtiness keeps unrelated stale hosts from turning every edit's parse
    /// pass into a refresh trigger. A later non-degraded pull clears the debt
    /// (it marks served); `didClose` forgets it.
    degraded_pulls: Mutex<HashSet<Url>>,
    /// Per-host coalescing state for the editor-facing `publishDiagnostics`
    /// wire sends (the quiet window): see [`WireGate`] and
    /// [`Self::wire_gate_admit`]. Mutated under the host's republish lock
    /// (same-host republishes are serialized) with two off-lock exceptions:
    /// the trailing task's [`Self::wire_gate_take_pending`] (flips only
    /// `pending`) and `clear_host`'s [`Self::forget_wire_gate`] (idempotent
    /// removal; a republish for a closed host never touches the gate, so the
    /// off-lock forget cannot race state back in). The inner mutex is held
    /// briefly, never across an await. Forgotten on `didClose` and under the
    /// publish seal.
    wire_gate: Mutex<HashMap<Url, WireGate>>,
    /// Always-on counters for the diagnostic path (#533): push-origin republishes
    /// in, `workspace/diagnostic/refresh` requested vs actually sent (the gap is
    /// what the #497 single-flight + coverage gate saves), and pulls answered with
    /// coarse latency. Read on shutdown / in tests to quantify refresh amplification
    /// on a real session before/after a change. Relaxed atomics — free on the hot
    /// path, and the counts need no cross-counter ordering.
    metrics: DiagnosticMetrics,
}

/// Workspace-wide single-flight state for `workspace/diagnostic/refresh` (#497).
#[derive(Default)]
struct RefreshFlight {
    /// A refresh request has been sent and its ack is not yet received.
    in_flight: bool,
    /// A refresh was requested while one was in flight; bounds the trailing refresh
    /// to one per window-with-activity (so a never-pulling editor can't spin).
    pending: bool,
    /// At least one request during this window was **forced** (a downstream-forwarded
    /// refresh, #521), so the trailing must fire regardless of the coverage gate —
    /// there is no version representing what the downstream asked to refresh.
    pending_forced: bool,
}

/// Per-host coalescing state for the editor-facing `publishDiagnostics` wire
/// sends. A changed merge inside the quiet window after the last send is
/// **withheld** from the wire (`dirty`) and a single trailing republish is
/// scheduled (`pending`); the trailing run re-merges the *latest* cache, so
/// every state change inside the window collapses into one send. An isolated
/// change (window already elapsed, or first publish) passes through
/// immediately — the gate adds no latency outside bursts.
struct WireGate {
    /// When the last `publishDiagnostics` was actually **committed** (written
    /// to the wire and stamped by [`DiagnosticAggregator::wire_gate_commit_send`]).
    /// `None` means the entry was minted by an admit whose send has not
    /// committed (yet, or ever — an aborted send): such a host's next publish
    /// passes immediately, and the `dirty` debt below records that the
    /// last-recorded set may never have reached the wire.
    last_sent_at: Option<tokio::time::Instant>,
    /// A trailing republish task is scheduled; bounds the tasks to one per
    /// host per window.
    pending: bool,
    /// The recorded last-published set may not have reached the editor's
    /// push namespace: either a changed merge was withheld by the quiet
    /// window, or a send was admitted but its commit never happened (aborted
    /// at the send await). A later republish must send even if its own merge
    /// compares unchanged. Set on every admit, cleared only by the post-send
    /// commit.
    dirty: bool,
}

/// The wire-gate decision for one republish attempt: send now, or withhold
/// until the quiet window elapses (scheduling the trailing republish iff no
/// task is already parked).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WireAdmit {
    /// Window clear: write to the wire now.
    SendNow,
    /// Inside the quiet window: withhold. `schedule_trailing` is `true` for
    /// at most one caller at a time — the first defer while no trailing task
    /// is parked (it must spawn the trailing republish after `remaining`);
    /// attempts while one is parked get `false`.
    Defer {
        schedule_trailing: bool,
        remaining: std::time::Duration,
    },
}

/// Per-host coverage versions for the refresh gate (#497, commit 2).
#[derive(Clone, Copy)]
struct HostCoverage {
    epoch: u64,
    /// Bumped on each set-changing republish for this host.
    current: u64,
    /// The `current` value a pull was last answered against (a lower bound — read
    /// before the pull's fold, so never ahead of what the editor actually received).
    served: u64,
}

/// The diagnostic coverage observed when a pull begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiagnosticCoverageStamp {
    epoch: u64,
    version: u64,
}

/// Always-on diagnostic-path counters (#533). The four counts trace the refresh
/// amplification chain — push-origin republishes in → refreshes requested vs sent →
/// pulls answered — so one [`Self::snapshot`] reveals where volume is created or
/// saved. Plain relaxed `AtomicU64`s: incrementing on the hot path is negligible and
/// the counters carry no cross-counter invariant.
#[derive(Default)]
struct DiagnosticMetrics {
    /// Push/eviction-origin republishes that changed the editor-visible set (the
    /// ingress that can drive a refresh). Counted in [`DiagnosticAggregator::bump_current`].
    push_republishes: AtomicU64,
    /// `workspace/diagnostic/refresh` asks that passed the client capability gate
    /// (entry to `request_pull_diagnostic_refresh`), *before* the single-flight /
    /// coverage gate decides whether to actually send.
    refreshes_requested: AtomicU64,
    /// `workspace/diagnostic/refresh` requests actually written to the wire (post
    /// single-flight + coverage gate, including trailing fires). `requested - sent`
    /// is what the #497 gate saves.
    refreshes_sent: AtomicU64,
    /// `textDocument/diagnostic` pulls answered (every return of the LSP handler).
    pulls_answered: AtomicU64,
    /// Total wall time spent in the pull handler, microseconds.
    /// `pull_micros_total / pulls_answered` = mean pull latency.
    pull_micros_total: AtomicU64,
}

/// A point-in-time copy of [`DiagnosticMetrics`] for logging and assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DiagnosticMetricsSnapshot {
    pub(crate) push_republishes: u64,
    pub(crate) refreshes_requested: u64,
    pub(crate) refreshes_sent: u64,
    pub(crate) pulls_answered: u64,
    pub(crate) pull_micros_total: u64,
}

impl DiagnosticMetrics {
    fn record_push_republish(&self) {
        self.push_republishes.fetch_add(1, Ordering::Relaxed);
    }

    fn record_refresh_requested(&self) {
        self.refreshes_requested.fetch_add(1, Ordering::Relaxed);
    }

    fn record_refresh_sent(&self) {
        self.refreshes_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn record_pull(&self, micros: u64) {
        self.pulls_answered.fetch_add(1, Ordering::Relaxed);
        self.pull_micros_total.fetch_add(micros, Ordering::Relaxed);
    }

    /// Snapshot all counters. Not atomic across counters (a concurrent update may
    /// land between reads), which is fine for monitoring — the chain ratios are
    /// still representative.
    fn snapshot(&self) -> DiagnosticMetricsSnapshot {
        DiagnosticMetricsSnapshot {
            push_republishes: self.push_republishes.load(Ordering::Relaxed),
            refreshes_requested: self.refreshes_requested.load(Ordering::Relaxed),
            refreshes_sent: self.refreshes_sent.load(Ordering::Relaxed),
            pulls_answered: self.pulls_answered.load(Ordering::Relaxed),
            pull_micros_total: self.pull_micros_total.load(Ordering::Relaxed),
        }
    }
}

impl DiagnosticMetricsSnapshot {
    /// Mean pull-handler latency in microseconds (`0` when no pulls answered).
    pub(crate) fn mean_pull_micros(&self) -> u64 {
        self.pull_micros_total
            .checked_div(self.pulls_answered)
            .unwrap_or(0)
    }
}

impl DiagnosticAggregator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record that a `workspace/diagnostic/refresh` was requested (passed the client
    /// capability gate, before the single-flight/coverage gate). See [`DiagnosticMetrics`].
    pub(crate) fn record_refresh_requested(&self) {
        self.metrics.record_refresh_requested();
    }

    /// Record that a `workspace/diagnostic/refresh` was actually written to the wire.
    pub(crate) fn record_refresh_sent(&self) {
        self.metrics.record_refresh_sent();
    }

    /// Record an answered `textDocument/diagnostic` pull and its handler latency.
    pub(crate) fn record_pull(&self, micros: u64) {
        self.metrics.record_pull(micros);
    }

    /// Snapshot the diagnostic-path counters (#533) for logging or assertions.
    pub(crate) fn metrics_snapshot(&self) -> DiagnosticMetricsSnapshot {
        self.metrics.snapshot()
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
        cache.get(host).is_some_and(has_live_region_slots)
    }

    /// Drop everything for a host (host `didClose`). Returns whether it existed.
    pub(crate) fn evict_host(&self, host: &Url) -> bool {
        let mut cache = self.lock();
        cache.remove(host).is_some()
    }

    /// Whether `diagnostics` differs from the last merged-and-recorded set for
    /// `host` (see the `last_published` field doc — the wire send can lag or be
    /// sealed), recording it as the new last when it does. Returns `false` when
    /// identical, so the caller skips a redundant re-emission (#422). Called
    /// under the host's republish lock, so same-host calls serialize.
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
            // Fast path: `merge_cached_diagnostics` output is deterministically
            // sorted (#423 / `sort_diagnostics`, a total order), so equal sets
            // arrive slice-equal — the O(n²) multiset walk below is only the
            // fallback for order variation a future unsorted caller could
            // introduce.
            if prev.as_slice() == diagnostics || same_diagnostic_multiset(prev, diagnostics) {
                return false;
            }
            *prev = diagnostics.to_vec();
            return true;
        }
        last.insert(host.clone(), diagnostics.to_vec());
        true
    }

    /// Forget the last-recorded set for `host` (host `didClose`), so its entry
    /// does not linger and a later re-open starts change detection afresh.
    pub(crate) fn forget_published(&self, host: &Url) {
        self.last_published
            .lock()
            .recover_poison("DiagnosticAggregator::last_published")
            .remove(host);
    }

    /// Begin a workspace-wide refresh under the single-flight + coverage guard
    /// (#497). `forced` is `true` for a downstream-forwarded refresh (#521), which
    /// **bypasses the coverage gate** (no version represents what the downstream
    /// asked to refresh); `false` for a push/eviction-origin refresh, which sends
    /// only when some host is dirty ([`Self::is_dirty`]).
    ///
    /// Returns `true` if the caller should send the refresh now; `false` otherwise.
    /// A `false` means either one is already in flight (recorded as `pending` so
    /// [`Self::finish_refresh`] fires one more on completion) or a gated request found
    /// nothing dirty (the editor already has the current set — no nudge needed).
    ///
    /// `is_dirty` is evaluated **while holding** the guard lock, so the "set pending"
    /// of a racing request is serialized against the "read pending + dirty" of a
    /// concurrent [`Self::finish_refresh`] — without that, a push that bumps `current`
    /// and sets `pending` between a stale dirty read and the lock could have its
    /// trailing refresh dropped. Lock order is `refresh_flight → coverage` (the
    /// coverage lock is a leaf; `bump_current`/`mark_served`/`is_dirty` never take
    /// `refresh_flight`), so no cycle.
    pub(crate) fn try_begin_refresh(&self, forced: bool) -> bool {
        let mut flight = self
            .refresh_flight
            .lock()
            .recover_poison("DiagnosticAggregator::refresh_flight");
        if flight.in_flight {
            flight.pending = true;
            flight.pending_forced |= forced;
            return false;
        }
        if !(forced || self.is_dirty()) {
            return false; // gated + clean: the editor already has the current set
        }
        flight.in_flight = true;
        true
    }

    /// Complete an in-flight refresh (its ack arrived) under the single-flight +
    /// coverage guard (#497). Returns `true` if the caller should send exactly one
    /// more — `in_flight` stays set so the next requester still coalesces — and
    /// `false` clears the guard.
    ///
    /// The trailing fires only when a request arrived during this window (`pending`,
    /// which bounds it to one per window-with-activity so a never-pulling editor
    /// can't spin) AND that work still needs a nudge — either it was `forced`, or a
    /// host is still dirty (a covering pull would have advanced `served` and cleared
    /// it). `pending`/`pending_forced` AND `is_dirty` are all read under this one
    /// lock, so a request racing completion is never lost: it sets the flags (and its
    /// bump is visible to the in-lock dirty read) before this reads them, or finds
    /// `in_flight` already cleared and sends fresh.
    pub(crate) fn finish_refresh(&self) -> bool {
        let mut flight = self
            .refresh_flight
            .lock()
            .recover_poison("DiagnosticAggregator::refresh_flight");
        let fire = flight.pending && (flight.pending_forced || self.is_dirty());
        flight.pending = false;
        flight.pending_forced = false;
        if fire {
            true
        } else {
            flight.in_flight = false;
            false
        }
    }

    /// Bump a host's `current` coverage version (#497) — a **push-origin** change the
    /// editor doesn't know about just landed, so its last-pulled view is now stale.
    /// Called only from the push/eviction paths (`publish_recorded_hosts`,
    /// `evict_connection_diagnostics`) when their `republish` reported Changed
    /// (a changed merge was recorded; the wire send may be withheld or sealed)
    /// or Deferred (the cache changed but region geometry was pending) —
    /// paired with the gated `request_pull_diagnostic_refresh`.
    ///
    /// Deliberately **not** bumped on editor-originated republishes (`publish_pull_layer`
    /// /`clear_pull_layer`/edit-remerge/`clear_host`): those are answered by the
    /// editor's own re-pull, so bumping there would strand `current > served` between
    /// the editor's pull and its next one — defeating the gate during active editing.
    ///
    /// INVARIANT: every `current` bump must be **coverable by a later pull** so
    /// `served` catches up — push/region/eviction fold into the pull
    /// (`fold_push_fallback_diagnostics`). A future bump whose change a pull would NOT
    /// reflect would strand `current > served` and turn every later push into a
    /// refresh storm — keep this property (and the push-origin-only rule) when adding
    /// bump sites.
    pub(crate) fn bump_current(&self, host: &Url) {
        self.metrics.record_push_republish();
        let mut coverage = self
            .coverage
            .lock()
            .recover_poison("DiagnosticAggregator::coverage");
        // Look up by `&Url` first, cloning the host key only on first insert (the
        // common path is a repeat push for an already-tracked host), matching
        // `record`'s convention — no `Url` clone on the hot path.
        if let Some(cov) = coverage.get_mut(host) {
            cov.current += 1;
        } else {
            coverage.insert(
                host.clone(),
                HostCoverage {
                    epoch: self.next_coverage_epoch.fetch_add(1, Ordering::Relaxed),
                    current: 1,
                    served: 0,
                },
            );
        }
    }

    /// Read a host's current coverage version (`0` if it has never changed).
    /// Test-only numeric view; production pulls use [`Self::coverage_stamp`] so
    /// close/reopen lifetimes cannot be confused (#745).
    #[cfg(test)]
    pub(crate) fn current_version(&self, host: &Url) -> u64 {
        self.coverage
            .lock()
            .recover_poison("DiagnosticAggregator::coverage")
            .get(host)
            .map_or(0, |c| c.current)
    }

    /// Capture both the current version and the coverage-entry lifetime.
    /// Returns `None` when the host has no active coverage entry, including
    /// after `didClose`; passing that through to [`Self::mark_served`] is a no-op.
    pub(crate) fn coverage_stamp(&self, host: &Url) -> Option<DiagnosticCoverageStamp> {
        self.coverage
            .lock()
            .recover_poison("DiagnosticAggregator::coverage")
            .get(host)
            .map(|coverage| DiagnosticCoverageStamp {
                epoch: coverage.epoch,
                version: coverage.current,
            })
    }

    /// Record that a pull was answered for `host` against coverage version
    /// `stamp` (#497): the editor now has the set as of its version, so it is no
    /// longer dirty up to there. Pure bookkeeping — it must **never** bump `current`
    /// or republish, so a refresh→pull→`mark_served` cannot beget another refresh
    /// (keeps #496/#499 loop-safety). Monotonic via `max`, so a slower concurrent
    /// pull can't regress `served`. A missing stamp or one from a closed document
    /// lifetime is a no-op (#745).
    pub(crate) fn mark_served(&self, host: &Url, stamp: Option<DiagnosticCoverageStamp>) {
        let Some(stamp) = stamp else {
            return;
        };
        if let Some(cov) = self
            .coverage
            .lock()
            .recover_poison("DiagnosticAggregator::coverage")
            .get_mut(host)
            && cov.epoch == stamp.epoch
        {
            cov.served = cov.served.max(stamp.version);
        }
    }

    /// Whether any open host has an uncovered set-change (`current > served`) — the
    /// coverage gate for [`Self::try_begin_refresh`]/[`Self::finish_refresh`] (#497).
    pub(crate) fn is_dirty(&self) -> bool {
        self.coverage
            .lock()
            .recover_poison("DiagnosticAggregator::coverage")
            .values()
            .any(|c| c.current > c.served)
    }

    /// Forget a host's coverage versions (#497) — `didClose`, so the doc can no
    /// longer be pulled and must not keep the workspace dirty. Paired with
    /// [`Self::forget_published`].
    pub(crate) fn forget_coverage(&self, host: &Url) {
        self.coverage
            .lock()
            .recover_poison("DiagnosticAggregator::coverage")
            .remove(host);
    }

    /// Record that a pull for `host` was answered degraded (see the
    /// `degraded_pulls` field doc). Clones the key only on first insert.
    pub(crate) fn record_degraded_pull(&self, host: &Url) {
        let mut degraded = self
            .degraded_pulls
            .lock()
            .recover_poison("DiagnosticAggregator::degraded_pulls");
        if !degraded.contains(host) {
            degraded.insert(host.clone());
        }
    }

    /// Consume `host`'s degraded-pull debt, returning whether one existed —
    /// the post-parse backstop's trigger for the recovery refresh.
    pub(crate) fn take_degraded_pull(&self, host: &Url) -> bool {
        self.degraded_pulls
            .lock()
            .recover_poison("DiagnosticAggregator::degraded_pulls")
            .remove(host)
    }

    /// Forget `host`'s degraded-pull debt without acting on it: a later
    /// non-degraded pull covered the host (it marked served), or the host
    /// closed.
    pub(crate) fn forget_degraded_pull(&self, host: &Url) {
        self.degraded_pulls
            .lock()
            .recover_poison("DiagnosticAggregator::degraded_pulls")
            .remove(host);
    }

    /// Decide whether a wire `publishDiagnostics` for `host` may be written
    /// now, given the per-host quiet `window` (see [`WireGate`]). A `SendNow`
    /// decision marks the `dirty` debt and opens no quiet window — the caller
    /// stamps the send with [`Self::wire_gate_commit_send`] only **after** it
    /// actually completed, so a republish aborted at the send await (the
    /// synthetic pull task is abortable on supersession) leaves the debt in
    /// place: the recorded-but-unsent set forces the next republish past the
    /// unchanged check and onto the wire.
    /// `Defer` marks `dirty` (a changed merge is being withheld) and hands the
    /// trailing-republish duty to at most one caller at a time (the first
    /// defer while no trailing task is parked). Called under the host's
    /// republish lock, so same-host decisions are serialized — including the
    /// decide→commit pair.
    pub(crate) fn wire_gate_admit(&self, host: &Url, window: std::time::Duration) -> WireAdmit {
        let now = tokio::time::Instant::now();
        let mut gates = self
            .wire_gate
            .lock()
            .recover_poison("DiagnosticAggregator::wire_gate");
        // A host with no entry has no prior committed send: mint the entry
        // with the debt already marked and pass. Marking `dirty` on EVERY
        // admit (not just Defer) is what makes an aborted send recoverable —
        // `published_set_changed` recorded the merge before the send, so
        // without the debt a later identical merge would skip past the
        // unchanged check while the editor never received the set.
        let Some(gate) = gates.get_mut(host) else {
            gates.insert(
                host.clone(),
                WireGate {
                    last_sent_at: None,
                    pending: false,
                    dirty: true,
                },
            );
            return WireAdmit::SendNow;
        };
        let elapsed = gate.last_sent_at.map(|at| now.duration_since(at));
        match elapsed {
            Some(elapsed) if elapsed < window => {
                gate.dirty = true;
                let schedule_trailing = !gate.pending;
                gate.pending = true;
                WireAdmit::Defer {
                    schedule_trailing,
                    remaining: window - elapsed,
                }
            }
            _ => {
                gate.dirty = true;
                WireAdmit::SendNow
            }
        }
    }

    /// Record that a wire `publishDiagnostics` for `host` was actually sent:
    /// stamp the send time (opening a fresh quiet window) and settle the
    /// `dirty` debt. Called right after the send await, under the same
    /// republish-lock hold as the [`Self::wire_gate_admit`] that admitted it.
    /// Clones the key only on first insert.
    pub(crate) fn wire_gate_commit_send(&self, host: &Url) {
        let now = tokio::time::Instant::now();
        let mut gates = self
            .wire_gate
            .lock()
            .recover_poison("DiagnosticAggregator::wire_gate");
        if let Some(gate) = gates.get_mut(host) {
            gate.last_sent_at = Some(now);
            gate.dirty = false;
        } else {
            // The admit minted an entry, but a `forget_wire_gate` (seal) can
            // race in off the happy path; re-minting settled is fine.
            gates.insert(
                host.clone(),
                WireGate {
                    last_sent_at: Some(now),
                    pending: false,
                    dirty: false,
                },
            );
        }
    }

    /// Whether a changed merge for `host` was withheld from the wire and not
    /// yet sent — the trailing republish must send even when its own merge
    /// compares unchanged against the recorded last-published set.
    pub(crate) fn wire_gate_is_dirty(&self, host: &Url) -> bool {
        self.wire_gate
            .lock()
            .recover_poison("DiagnosticAggregator::wire_gate")
            .get(host)
            .is_some_and(|gate| gate.dirty)
    }

    /// Clear the trailing-task marker for `host`, returning whether gate state
    /// still existed. Called by the trailing republish task right before it
    /// re-runs `republish`, so a defer that races the re-run schedules a fresh
    /// trailing task instead of being silently absorbed by one that already
    /// woke. `false` means the entry was forgotten while the task was parked
    /// (`didClose`, or the seal) — the withheld debt was cancelled and the
    /// task must bail rather than republish. (The bail covers only the
    /// entry-gone case: a stale task waking against a reopened incarnation's
    /// fresh entry gets `true` and clears its `pending`, which at worst
    /// schedules one extra trailing task — the defer-reschedule design, not
    /// this bail, is what keeps that safe.)
    pub(crate) fn wire_gate_take_pending(&self, host: &Url) -> bool {
        if let Some(gate) = self
            .wire_gate
            .lock()
            .recover_poison("DiagnosticAggregator::wire_gate")
            .get_mut(host)
        {
            gate.pending = false;
            true
        } else {
            false
        }
    }

    /// Forget the wire-gate state for `host` so the entry doesn't linger and a
    /// parked trailing task bails ([`Self::wire_gate_take_pending`] returns
    /// `false`). Called on `didClose` (`clear_host`, next to
    /// [`Self::forget_coverage`] — after the clearing republish, which
    /// bypasses the gate for closed hosts) and when the publish seal renders
    /// the gate state moot.
    pub(crate) fn forget_wire_gate(&self, host: &Url) {
        self.wire_gate
            .lock()
            .recover_poison("DiagnosticAggregator::wire_gate")
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

    #[tokio::test(start_paused = true)]
    async fn wire_gate_passes_leading_edge_and_defers_within_the_window() {
        let agg = DiagnosticAggregator::new();
        let window = std::time::Duration::from_secs(1);

        assert_eq!(
            agg.wire_gate_admit(&host(), window),
            WireAdmit::SendNow,
            "the first send always passes (no prior send to be quiet after)"
        );
        agg.wire_gate_commit_send(&host());

        // Within the window: withheld, and exactly the FIRST defer is handed
        // the trailing-task duty.
        let first = agg.wire_gate_admit(&host(), window);
        assert!(
            matches!(
                first,
                WireAdmit::Defer {
                    schedule_trailing: true,
                    ..
                }
            ),
            "the first in-window attempt schedules the trailing republish, got {first:?}"
        );
        assert!(
            agg.wire_gate_is_dirty(&host()),
            "a withheld change marks the wire dirty"
        );
        let second = agg.wire_gate_admit(&host(), window);
        assert!(
            matches!(
                second,
                WireAdmit::Defer {
                    schedule_trailing: false,
                    ..
                }
            ),
            "later in-window attempts must not schedule a second task, got {second:?}"
        );

        // Window elapsed: pass again; the COMMIT (post-send) clears dirty.
        tokio::time::advance(window).await;
        assert!(
            agg.wire_gate_take_pending(&host()),
            "the gate entry still exists while the host is open"
        );
        assert_eq!(
            agg.wire_gate_admit(&host(), window),
            WireAdmit::SendNow,
            "the trailing attempt after the window sends"
        );
        assert!(
            agg.wire_gate_is_dirty(&host()),
            "the admit DECISION alone must not consume the withheld-send debt \
             (an aborted send would otherwise lose it)"
        );
        agg.wire_gate_commit_send(&host());
        assert!(
            !agg.wire_gate_is_dirty(&host()),
            "the post-send commit clears the dirty marker"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wire_gate_defer_after_take_pending_reschedules() {
        // A defer that lands after the trailing task woke (take_pending) but
        // before the window reopened must schedule a FRESH trailing task —
        // otherwise its change would wait for a task that no longer exists.
        let agg = DiagnosticAggregator::new();
        let window = std::time::Duration::from_secs(1);
        assert_eq!(agg.wire_gate_admit(&host(), window), WireAdmit::SendNow);
        agg.wire_gate_commit_send(&host());
        assert!(matches!(
            agg.wire_gate_admit(&host(), window),
            WireAdmit::Defer {
                schedule_trailing: true,
                ..
            }
        ));

        assert!(agg.wire_gate_take_pending(&host()));
        assert!(matches!(
            agg.wire_gate_admit(&host(), window),
            WireAdmit::Defer {
                schedule_trailing: true,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn wire_gate_uncommitted_send_keeps_the_debt() {
        // A republish aborted at the send await (synthetic pull tasks are
        // abortable on supersession) commits nothing: the next attempt must
        // pass again (no phantom quiet window) AND the dirty debt minted at
        // admit must survive — `published_set_changed` recorded the merge
        // before the send, so without the debt a later identical merge would
        // skip the unchanged check while the editor never received the set.
        let agg = DiagnosticAggregator::new();
        let window = std::time::Duration::from_secs(1);

        assert_eq!(agg.wire_gate_admit(&host(), window), WireAdmit::SendNow);
        // (no commit: the send was aborted)
        assert!(
            agg.wire_gate_is_dirty(&host()),
            "the admitted-but-uncommitted send leaves its debt marked"
        );
        assert_eq!(
            agg.wire_gate_admit(&host(), window),
            WireAdmit::SendNow,
            "an uncommitted send must not open a quiet window"
        );
        agg.wire_gate_commit_send(&host());
        assert!(
            !agg.wire_gate_is_dirty(&host()),
            "the completed send settles the debt"
        );
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
    fn push_slot_servers_lists_push_servers_excluding_pull_layer() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "linter".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag("x")],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "hostlint".into(),
            Some(ProgressConnectionId::for_test(2)),
            vec![diag("y")],
        );
        agg.set_pull_layer(&host(), vec![diag("p")]);

        let snapshot = agg.snapshot(&host());
        let servers = push_slot_servers(&snapshot);
        assert_eq!(
            servers,
            std::collections::HashSet::from(["linter", "hostlint"]),
            "the synthetic pull-layer server is never reported as a pusher"
        );
    }

    #[test]
    fn cached_push_diagnostics_partitions_by_layer_and_transforms_regions() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "linter".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("region", 0, 0)],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "hostlint".into(),
            Some(ProgressConnectionId::for_test(2)),
            vec![diag_at("host", 8, 0)],
        );
        // The pull blob must never be folded into the client-pull response.
        agg.set_pull_layer(&host(), vec![diag_at("pull", 9, 0)]);

        let mut offsets = HashMap::new();
        offsets.insert("r1".to_string(), RegionOffset::new(5, 0));

        let (region_items, host_items) = cached_push_diagnostics(
            &host(),
            agg.snapshot(&host()),
            &offsets,
            |_source, _server| true,
        );

        assert_eq!(region_items.len(), 1);
        assert_eq!(region_items[0].message, "region");
        assert_eq!(
            region_items[0].range.start.line, 5,
            "region push is transformed to host coordinates (base line 5)"
        );
        assert_eq!(
            host_items
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>(),
            vec!["host"],
            "host push passes through; the pull blob is excluded"
        );
    }

    #[test]
    fn cached_push_diagnostics_respects_include_predicate_and_missing_offset() {
        let agg = DiagnosticAggregator::new();
        // Two region servers; only "linter" is included by the predicate.
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "linter".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("kept", 0, 0)],
        );
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "ra".into(),
            Some(ProgressConnectionId::for_test(2)),
            vec![diag_at("excluded", 0, 0)],
        );
        // A region with no current offset is skipped entirely.
        agg.record(
            &host(),
            DiagnosticSource::Region("gone".into()),
            "linter".into(),
            Some(ProgressConnectionId::for_test(3)),
            vec![diag_at("stale", 0, 0)],
        );

        let mut offsets = HashMap::new();
        offsets.insert("r1".to_string(), RegionOffset::new(0, 0));

        let (region_items, host_items) = cached_push_diagnostics(
            &host(),
            agg.snapshot(&host()),
            &offsets,
            |_source, server| server == "linter",
        );

        assert_eq!(
            region_items
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>(),
            vec!["kept"],
            "only the included server's slot in a region with a live offset is returned"
        );
        assert!(host_items.is_empty());
    }

    #[test]
    fn merge_orders_diagnostics_top_to_bottom_by_position() {
        let agg = DiagnosticAggregator::new();
        // Record three host-source servers out of position order; the merge must
        // emit them deterministically top-to-bottom regardless of insertion/HashMap
        // order (#423).
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "c_srv".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("line9", 9, 0)],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "a_srv".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("line2", 2, 0)],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "b_srv".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("line5", 5, 0)],
        );
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        let lines: Vec<u32> = merged.iter().map(|d| d.range.start.line).collect();
        assert_eq!(
            lines,
            vec![2, 5, 9],
            "diagnostics are ordered top-to-bottom by host position, deterministically"
        );
    }

    #[test]
    fn merge_breaks_ties_by_severity_when_position_and_message_match() {
        use tower_lsp_server::ls_types::DiagnosticSeverity;
        let agg = DiagnosticAggregator::new();
        // Same position+message+source, differing only in severity — must still order
        // deterministically (ERROR=1 before WARNING=2), not by HashMap iteration.
        let mut warn = diag_at("dup", 3, 0);
        warn.severity = Some(DiagnosticSeverity::WARNING);
        let mut err = diag_at("dup", 3, 0);
        err.severity = Some(DiagnosticSeverity::ERROR);
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv_w".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![warn],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv_e".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![err],
        );
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        let sevs: Vec<_> = merged.iter().map(|d| d.severity).collect();
        assert_eq!(
            sevs,
            vec![
                Some(DiagnosticSeverity::ERROR),
                Some(DiagnosticSeverity::WARNING)
            ],
            "a severity tie-break gives same-span/message diagnostics a deterministic order"
        );
    }

    #[test]
    fn merge_breaks_position_ties_by_message() {
        let agg = DiagnosticAggregator::new();
        // Two diagnostics at the SAME position but different messages → ordered by
        // message, deterministically (not by HashMap iteration).
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv1".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("zebra", 3, 0)],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv2".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![diag_at("apple", 3, 0)],
        );
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        let msgs: Vec<&str> = merged.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs, vec!["apple", "zebra"], "ties break by message");
    }

    #[test]
    fn merge_breaks_position_and_message_ties_by_source() {
        let agg = DiagnosticAggregator::new();
        // Same position AND message, differing only in `source` → ordered by source.
        let mut from_z = diag_at("dup", 3, 0);
        from_z.source = Some("z_src".into());
        let mut from_a = diag_at("dup", 3, 0);
        from_a.source = Some("a_src".into());
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv_z".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![from_z],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv_a".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![from_a],
        );
        let merged = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        let sources: Vec<_> = merged.iter().map(|d| d.source.clone()).collect();
        assert_eq!(
            sources,
            vec![Some("a_src".to_string()), Some("z_src".to_string())],
            "same position+message ties break by source"
        );
    }

    #[test]
    fn merge_breaks_ties_by_serialized_form_for_data_only_differences() {
        let agg = DiagnosticAggregator::new();
        // Identical on every cheap key (position, message, source=None, severity=None)
        // but differing only in `data` — the lazy serialized tiebreak must still give a
        // deterministic order, not fall back to HashMap iteration.
        let mut d1 = diag_at("same", 3, 0);
        d1.data = Some(serde_json::json!({"k": 1}));
        let mut d2 = diag_at("same", 3, 0);
        d2.data = Some(serde_json::json!({"k": 2}));
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv1".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![d1.clone()],
        );
        agg.record(
            &host(),
            DiagnosticSource::Host,
            "srv2".into(),
            Some(ProgressConnectionId::for_test(1)),
            vec![d2.clone()],
        );
        let first = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
        // Re-merge a few times: a HashMap-order fallback would eventually flip; the
        // serialized tiebreak must keep the order stable across merges.
        for _ in 0..5 {
            let again = merge_cached_diagnostics(&host(), agg.snapshot(&host()), &HashMap::new());
            assert_eq!(
                again.iter().map(|d| &d.data).collect::<Vec<_>>(),
                first.iter().map(|d| &d.data).collect::<Vec<_>>(),
                "data-only-differing diagnostics keep a deterministic order across merges"
            );
        }
        // And the order matches the serialized form (d1's data `{k:1}` < d2's `{k:2}`).
        assert_eq!(first[0].data, d1.data);
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

    #[test]
    fn refresh_single_flight_collapses_a_burst_into_at_most_one_trailing() {
        // #497: while a workspace refresh is in flight (awaiting the editor's ack),
        // a burst of further requests must coalesce — at most ONE trailing refresh
        // fires on completion. Uses `forced` requests so the test exercises the pure
        // single-flight machinery without the coverage gate (covered separately).
        let agg = DiagnosticAggregator::new();

        // The first request of an idle guard is sent.
        assert!(agg.try_begin_refresh(true), "first request sends");
        // Everything during the in-flight window coalesces (no new send).
        assert!(!agg.try_begin_refresh(true), "in-flight: coalesced");
        assert!(!agg.try_begin_refresh(true), "in-flight: still coalesced");

        // On completion, the recorded `pending` fires exactly one trailing refresh
        // (guard stays in-flight so a request racing it still coalesces).
        assert!(agg.finish_refresh(), "pending → one trailing refresh");
        // The trailing refresh saw no further requests → it clears the guard.
        assert!(!agg.finish_refresh(), "no further pending → guard clears");

        // Guard clear → the next request sends again (no stuck in-flight).
        assert!(
            agg.try_begin_refresh(true),
            "cleared guard → next request sends"
        );
        assert!(!agg.finish_refresh(), "no pending → clears");
    }

    #[test]
    fn refresh_single_flight_re_pends_each_window_independently() {
        // A request arriving during the *trailing* refresh's own in-flight window
        // must itself coalesce and drive one more trailing refresh — the guard
        // re-arms per window, so no change is ever stranded.
        let agg = DiagnosticAggregator::new();
        assert!(agg.try_begin_refresh(true), "first sends");
        assert!(!agg.try_begin_refresh(true), "coalesced → pending");
        assert!(
            agg.finish_refresh(),
            "pending → trailing refresh, stays in-flight"
        );
        // A new request during the trailing refresh's window coalesces again.
        assert!(
            !agg.try_begin_refresh(true),
            "coalesced during trailing window"
        );
        assert!(agg.finish_refresh(), "second pending → one more trailing");
        assert!(!agg.finish_refresh(), "now drained → clears");
    }

    #[test]
    fn coverage_gate_blocks_a_clean_request_and_passes_a_dirty_one() {
        // #497 commit 2: a non-forced (push/eviction-origin) request sends only when
        // some host is dirty (`current > served`); a `forced` one always sends.
        let agg = DiagnosticAggregator::new();
        let h = host();

        // Nothing has changed → not dirty → a gated request sends nothing…
        assert!(!agg.is_dirty(), "fresh aggregator is clean");
        assert!(!agg.try_begin_refresh(false), "gated + clean → no refresh");
        // …but a forced (downstream-forwarded) refresh bypasses the gate.
        assert!(agg.try_begin_refresh(true), "forced bypasses the gate");
        assert!(!agg.finish_refresh(), "no pending → clears");

        // A set-change makes the host dirty → the gated request now sends.
        agg.bump_current(&h);
        assert!(agg.is_dirty(), "a bumped, un-served host is dirty");
        assert!(agg.try_begin_refresh(false), "gated + dirty → sends");
        assert!(!agg.finish_refresh(), "no pending → clears");
    }

    #[test]
    fn coverage_gate_suppresses_the_trailing_when_a_pull_covered_the_change() {
        // The win: a change pushed during the in-flight window is covered by the
        // editor's own pull (which advances `served`), so the trailing is suppressed.
        let agg = DiagnosticAggregator::new();
        let h = host();

        agg.bump_current(&h); // current=1, served=0 → dirty
        assert!(agg.try_begin_refresh(false), "dirty → first refresh sends");
        // Another change lands during the in-flight window (coalesced as pending).
        agg.bump_current(&h); // current=2
        assert!(!agg.try_begin_refresh(false), "in-flight → pending");
        // The editor pulls and is answered against the latest version → not dirty.
        let stamp = agg.coverage_stamp(&h);
        agg.mark_served(&h, stamp);
        assert!(!agg.is_dirty(), "the pull covered both changes");
        // Completion: pending was set, but nothing is dirty and it wasn't forced →
        // the trailing is suppressed.
        assert!(
            !agg.finish_refresh(),
            "covered by a pull → no redundant trailing refresh"
        );
    }

    #[test]
    fn coverage_gate_fires_the_trailing_when_still_dirty() {
        // The complement: a change lands during the window and the editor has NOT
        // pulled it → still dirty → the trailing fires.
        let agg = DiagnosticAggregator::new();
        let h = host();

        agg.bump_current(&h); // dirty
        assert!(agg.try_begin_refresh(false), "dirty → sends");
        agg.bump_current(&h); // another change during the window → pending
        assert!(!agg.try_begin_refresh(false), "in-flight → pending");
        // No pull → still dirty → trailing fires once, then drains.
        assert!(
            agg.finish_refresh(),
            "pending + still dirty → trailing fires"
        );
        assert!(!agg.finish_refresh(), "no further pending → clears");
    }

    #[test]
    fn coverage_gate_does_not_spin_without_new_requests() {
        // acks-but-never-pulls bound: with a host left dirty (editor acks the refresh
        // but never pulls), `finish_refresh` must NOT keep firing — the trailing
        // requires a `pending` (a request *during* the window), so absent new
        // requests it fires zero trailing and clears. This is what prevents an
        // ack-rate refresh spin.
        let agg = DiagnosticAggregator::new();
        let h = host();
        agg.bump_current(&h); // dirty, and stays dirty (no pull ever)
        assert!(agg.try_begin_refresh(false), "dirty → first refresh sends");
        // No request arrived during the window → no pending → no trailing despite dirty.
        assert!(
            !agg.finish_refresh(),
            "dirty but no pending → no trailing (no spin)"
        );
        // And it really cleared — a subsequent completion is a no-op.
        assert!(!agg.finish_refresh(), "guard already clear");
    }

    #[test]
    fn coverage_forget_and_served_bookkeeping() {
        let agg = DiagnosticAggregator::new();
        let h = host();
        assert_eq!(agg.current_version(&h), 0, "unseen host starts at 0");
        agg.bump_current(&h);
        agg.bump_current(&h);
        assert_eq!(agg.current_version(&h), 2);
        // `served` is monotonic: a stale (lower) mark can't regress it.
        let current = agg.coverage_stamp(&h).expect("coverage exists");
        agg.mark_served(&h, Some(current));
        agg.mark_served(
            &h,
            Some(DiagnosticCoverageStamp {
                version: 1,
                ..current
            }),
        );
        assert!(
            !agg.is_dirty(),
            "served caught up and a stale mark didn't regress it"
        );
        // didClose forgets coverage entirely → a re-open starts clean.
        agg.bump_current(&h); // dirty again
        assert!(agg.is_dirty());
        agg.forget_coverage(&h);
        assert!(
            !agg.is_dirty(),
            "a closed host no longer keeps the workspace dirty"
        );
        assert_eq!(agg.current_version(&h), 0, "re-open starts fresh");
    }

    #[test]
    fn stale_pull_does_not_mark_reopened_coverage_served() {
        let agg = DiagnosticAggregator::new();
        let h = host();
        agg.bump_current(&h);
        agg.bump_current(&h);
        let stale = agg
            .coverage_stamp(&h)
            .expect("coverage exists before close");

        agg.forget_coverage(&h);
        agg.bump_current(&h);
        agg.mark_served(&h, Some(stale));

        assert!(
            agg.is_dirty(),
            "a pull from the closed lifetime must not cover the reopened host"
        );
    }

    #[test]
    fn metrics_start_at_zero() {
        let agg = DiagnosticAggregator::new();
        assert_eq!(agg.metrics_snapshot(), DiagnosticMetricsSnapshot::default());
    }

    #[test]
    fn bump_current_counts_a_push_republish() {
        let agg = DiagnosticAggregator::new();
        agg.bump_current(&host());
        agg.bump_current(&host());
        assert_eq!(agg.metrics_snapshot().push_republishes, 2);
    }

    #[test]
    fn refresh_and_pull_counters_track_each_record() {
        let agg = DiagnosticAggregator::new();
        agg.record_refresh_requested();
        agg.record_refresh_requested();
        agg.record_refresh_requested();
        agg.record_refresh_sent(); // gate let one through; two coalesced away
        agg.record_pull(100);
        agg.record_pull(300);

        let m = agg.metrics_snapshot();
        assert_eq!(m.refreshes_requested, 3);
        assert_eq!(m.refreshes_sent, 1);
        assert_eq!(
            m.refreshes_requested.saturating_sub(m.refreshes_sent),
            2,
            "requested - sent is what the gate saved"
        );
        assert_eq!(m.pulls_answered, 2);
        assert_eq!(m.pull_micros_total, 400);
        assert_eq!(m.mean_pull_micros(), 200);
    }

    #[test]
    fn mean_pull_micros_is_zero_without_pulls() {
        assert_eq!(DiagnosticMetricsSnapshot::default().mean_pull_micros(), 0);
    }
}
