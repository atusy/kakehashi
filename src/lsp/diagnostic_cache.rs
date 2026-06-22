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
//! the lifecycle's evictions are O(1): a whole host on `didClose` today, a source
//! / server later.
//!
//! ## Staging
//! Two source kinds are populated:
//! - [`DiagnosticSource::PullLayer`] — the host-event pull's already
//!   cross-layer-combined result, in host coordinates, as one blob.
//! - [`DiagnosticSource::Region`] — a downstream push for an injection region, in
//!   virtual coordinates, transformed to host coordinates at publish time.
//!
//! **Known interim overlap (deferred capability gate).** The pull feed fans out
//! to *every* configured region server with no capability classification, so a
//! server that both answers `textDocument/diagnostic` (landing in `PullLayer`)
//! *and* spontaneously pushes `publishDiagnostics` (landing in a `Region` slot) is
//! counted twice. In practice the two channels are disjoint — a pull-capable
//! server should not also push (LSP) — so this only *duplicates*, never *hides*,
//! and only for a misbehaving server. The deferred `pullFallback` / capability
//! classification removes the overlap by giving each server one native source.
//!
//! Also deferred: per-source strategy fan-in (`preferred` sticky / `concatenated`
//! visible-walk; cross-source order is HashMap-nondeterministic until then), the
//! `_self` host-layer push source, the `content_epoch` version gate, and
//! region-invalidation/crash eviction.

use std::collections::HashMap;
use std::sync::Mutex;

use tower_lsp_server::ls_types::Diagnostic;
use url::Url;

use crate::error::LockResultExt;
use crate::lsp::bridge::{RegionOffset, VirtualDocumentUri, translate_virtual_range_to_host};

/// Which contributor a slot belongs to under a host document.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum DiagnosticSource {
    /// A downstream **push** for an injection region, identified by its stable
    /// region id (lazy-node-identity-tracking ULID). Held in **virtual**
    /// coordinates and transformed to host coordinates at publish time against
    /// the region's *current* offset (lazy re-anchor).
    Region(String),
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
#[derive(Debug, Clone, Default)]
pub(crate) struct SlotEntry {
    pub(crate) diagnostics: Vec<Diagnostic>,
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
///   resolves, e.g. it was edited away) is skipped — its slot is stale and gets
///   evicted by the lifecycle.
/// - [`DiagnosticSource::PullLayer`] slots are already host-local and pass through.
///
/// Staged: results are concatenated (the default `textDocument/publishDiagnostics`
/// strategy). Per-source strategy fan-in (`preferred` sticky / `concatenated`
/// visible-walk) is a follow-up.
pub(crate) fn merge_cached_diagnostics(
    host: &Url,
    snapshot: &SourceSlots,
    region_offsets: &HashMap<String, RegionOffset>,
) -> Vec<Diagnostic> {
    let host_str = host.as_str();
    let mut merged = Vec::new();
    for (source, servers) in snapshot {
        match source {
            DiagnosticSource::Region(region_id) => {
                let Some(offset) = region_offsets.get(region_id) else {
                    // Stale region: no current offset to anchor against.
                    continue;
                };
                for slot in servers.values() {
                    for diagnostic in &slot.diagnostics {
                        let mut diagnostic = diagnostic.clone();
                        transform_region_diagnostic(&mut diagnostic, offset, host_str);
                        merged.push(diagnostic);
                    }
                }
            }
            DiagnosticSource::PullLayer => {
                for slot in servers.values() {
                    merged.extend(slot.diagnostics.iter().cloned());
                }
            }
        }
    }
    merged
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
    /// Serializes the publisher's snapshot→merge→publish so two concurrent
    /// republishes (a region push vs a host-event pull, on different tasks)
    /// cannot interleave and emit out of order — which would let a stale snapshot
    /// publish *after* a fresh one and permanently hide diagnostics on a quiescent
    /// file. Global (not per-host) for simplicity; republishes are per diagnostic
    /// event, not hot. A per-host lock is a possible follow-up.
    republish_lock: tokio::sync::Mutex<()>,
}

impl DiagnosticAggregator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Acquire the global republish lock; held by the publisher across
    /// snapshot→merge→publish so emissions stay ordered (see field docs).
    pub(crate) async fn lock_republish(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.republish_lock.lock().await
    }

    /// Record (replacing) one server's diagnostics for a `(host, source)`.
    ///
    /// An empty `diagnostics` keeps an empty slot — the merge skips it, so the
    /// server's prior diagnostics are cleared without dropping the slot key.
    pub(crate) fn record(
        &self,
        host: &Url,
        source: DiagnosticSource,
        server: String,
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
        source_slots
            .entry(source)
            .or_default()
            .insert(server, SlotEntry { diagnostics });
    }

    /// Replace the cached host-event pull blob for a host
    /// ([`DiagnosticSource::PullLayer`]). Equivalent to a single-server `record`.
    pub(crate) fn set_pull_layer(&self, host: &Url, diagnostics: Vec<Diagnostic>) {
        self.record(
            host,
            DiagnosticSource::PullLayer,
            PULL_LAYER_SERVER.to_string(),
            diagnostics,
        );
    }

    /// Snapshot every source/server slot for a host, cloned for merging off-lock.
    /// Empty when the host has no slots.
    pub(crate) fn snapshot(&self, host: &Url) -> SourceSlots {
        let cache = self.lock();
        cache.get(host).cloned().unwrap_or_default()
    }

    /// Drop everything for a host (host `didClose`). Returns whether it existed.
    pub(crate) fn evict_host(&self, host: &Url) -> bool {
        let mut cache = self.lock();
        cache.remove(host).is_some()
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
        let merged = merge_cached_diagnostics(&host(), &agg.snapshot(&host()), &HashMap::new());
        let mut msgs: Vec<&str> = merged.iter().map(|d| d.message.as_str()).collect();
        msgs.sort();
        assert_eq!(msgs, vec!["a", "b"]);
    }

    #[test]
    fn merge_of_empty_snapshot_is_empty() {
        assert!(merge_cached_diagnostics(&host(), &SourceSlots::new(), &HashMap::new()).is_empty());
    }

    #[test]
    fn merge_transforms_region_slots_to_host_coords() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "luals".into(),
            vec![diag_at("err", 0, 2)],
        );
        let mut offsets = HashMap::new();
        // Region r1 sits at host line 5, column offset 4 on its first line.
        offsets.insert("r1".to_string(), RegionOffset::new(5, 4));
        let merged = merge_cached_diagnostics(&host(), &agg.snapshot(&host()), &offsets);
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
            vec![d],
        );
        let mut offsets = HashMap::new();
        offsets.insert("r1".to_string(), RegionOffset::new(5, 0));
        let merged = merge_cached_diagnostics(&host(), &agg.snapshot(&host()), &offsets);

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
            vec![diag("stale")],
        );
        // No offset for "gone" -> region is stale -> skipped.
        let merged = merge_cached_diagnostics(&host(), &agg.snapshot(&host()), &HashMap::new());
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_combines_region_push_and_pull_layer() {
        let agg = DiagnosticAggregator::new();
        agg.record(
            &host(),
            DiagnosticSource::Region("r1".into()),
            "luals".into(),
            vec![diag_at("push", 0, 0)],
        );
        agg.set_pull_layer(&host(), vec![diag_at("pull", 9, 0)]);
        let mut offsets = HashMap::new();
        offsets.insert("r1".to_string(), RegionOffset::new(2, 0));
        let merged = merge_cached_diagnostics(&host(), &agg.snapshot(&host()), &offsets);
        let mut msgs: Vec<&str> = merged.iter().map(|d| d.message.as_str()).collect();
        msgs.sort();
        assert_eq!(msgs, vec!["pull", "push"]);
    }

    #[test]
    fn evict_host_drops_all() {
        let agg = DiagnosticAggregator::new();
        agg.set_pull_layer(&host(), vec![diag("h")]);
        assert!(agg.evict_host(&host()));
        assert!(agg.snapshot(&host()).is_empty());
        assert!(!agg.evict_host(&host()), "second evict is a no-op");
    }
}
