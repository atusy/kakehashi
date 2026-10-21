//! Answers to `kakehashi/virtualDocument/prepare`: cache, single flight and
//! retries.
//!
//! Every path that hands a virtual document to downstream servers — the
//! didOpen/didChange pass, requests — looks its prepared form up here by the
//! exact input it was built from (virtual text, gaps, host language, the
//! candidate servers and the prepare epoch), so one input gets one answer,
//! shared by all of them.
//!
//! An answer stands only under the epoch it was asked in (see
//! [`LanguageServerPool::prepare_epoch`]): whatever can change which candidate answers, or how —
//! an advertisement, a start failure cleared, settings — moves the epoch, and
//! every lookup after that is a new input. No answer is forgotten to get
//! there; outdated answers age out of their region's slots, and an attempt
//! still running across the move is no longer admitted to ask its peer.
//!
//! This registry only answers. What each server holds, and so which map its
//! coordinates follow, is recorded by the paths that send, with the text
//! (see the document tracker's `HeldText`).
//!
//! The lifecycle pass must not wait for the peer under the document's edit
//! lock, so it only looks up: on a miss it starts the attempt and holds the
//! document back, and the attempt's end asks for the host to be synced again
//! ([`Resync`]). A save, under the same lock at the saved version, holds its
//! documents the same way. Requests that can wait for the answer do; those
//! that cannot (a diagnostic snapshot, a pull folding pushes), and the
//! re-open sweep and recovery probes, look up without waiting, never moving
//! what the pass holds the document on.
//!
//! Every attempt runs on its own task, so a cancelled request cannot strand
//! one half-done, and all waiters share it. An attempt that gets no answer
//! from the peer it chose (it crashes, times out, or cancels) is retried
//! with backoff; one that gets an unusable answer is final for that input,
//! as is one that finds no candidate to ask (the document is then sent as
//! is). A candidate that cannot start is passed over, as one that does not
//! advertise the request.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use url::Url;

use super::ResolvedServerConfig;
use super::pool::LanguageServerPool;
use super::protocol::{
    PrepareHostTextDocument, PrepareParams, PrepareTextDocument, PreparedDocument,
    VirtualDocumentUri, VirtualLayout, apply_prepare_result_at,
};
use crate::language::injection::VirtualGap;
use crate::lsp::bridge::pool::RegionPrepare;
use crate::text::terminal::escape_terminal_controls;

/// How many closed hosts are remembered as closed at most, the oldest close
/// going first. No time bound: a request resolved before a close may look
/// region after region up, each after a peer's start and answer.
const CLOSED_HOSTS_KEPT: usize = 512;

/// Answers kept per region besides the one the lifecycle pass holds its
/// document on: enough for an undo, or a request on text an edit or two
/// old, to find its answer without asking again.
const SLOTS_PER_REGION: usize = 3;

/// The servers that may prepare one (host, injection) pair's virtual
/// documents, in priority order — the first of them advertising the request
/// prepares each input (the `preferred` strategy) — and the prepare epoch
/// they were resolved under: a target from before the epoch moved (settings
/// changed since, say) asks nothing.
#[derive(Debug, Clone)]
pub(crate) struct PrepareTarget {
    pub(crate) candidates: Arc<[ResolvedServerConfig]>,
    pub(crate) epoch: u64,
}

/// One virtual document to prepare.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PrepareInput<'a> {
    pub(crate) host_uri: &'a Url,
    pub(crate) host_language: &'a str,
    pub(crate) injection_language: &'a str,
    pub(crate) region_id: &'a str,
    pub(crate) virtual_text: &'a str,
    pub(crate) gaps: &'a [VirtualGap],
}

/// The answer for one input.
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    /// No candidate prepares it: sent as is.
    AsIs,
    /// The document to send.
    Prepared(Arc<PreparedDocument>),
    /// The peer's answer was unusable: sent not at all.
    Unusable,
}

/// What a lookup found without waiting.
#[derive(Debug, Clone)]
pub(crate) enum PrepareLookup {
    Answered(Answer),
    /// No answer yet: send nothing now; the host is synced again when it
    /// arrives.
    Pending,
}

/// Why waiting gave no document to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrepareMiss {
    /// No answer is in (yet): the attempt got none, a retry is backing off,
    /// or the epoch moved; a later one may bring it.
    Unanswered,
    /// The peer's answer was unusable, final for this input.
    Failed,
}

/// A host to sync again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resync {
    pub(crate) host_uri: Url,
    pub(crate) cause: ResyncCause,
}

/// Why a host is synced again, which decides whether its diagnostics are
/// collected again too. Several of a host's resyncs merge into one (see
/// [`ResyncCause::merge`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResyncCause {
    /// Only synced: a retry came due, a pass is repeated, or the epoch moved
    /// before anything was looked up.
    Retry,
    /// A document held back became ready to send: the host's diagnostics
    /// are collected again too.
    Ready {
        /// See [`ResyncCause::edited`].
        edited: bool,
    },
    /// The host's diagnostics are collected again too, dropping those
    /// placed from a document's earlier text: one held back got an unusable
    /// answer, so it is no longer bridged, or the epoch moved.
    Recollect {
        /// See [`ResyncCause::edited`].
        edited: bool,
    },
}

impl ResyncCause {
    /// Whether a document held back became ready to send.
    pub(crate) fn ready(self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// Whether the host's diagnostics are collected again.
    pub(crate) fn collects(self) -> bool {
        !matches!(self, Self::Retry)
    }

    /// Whether the answer, ready or unusable, follows one for its
    /// document's earlier text under the same epoch: an edit's, whose
    /// diagnostics the edit's debounced collection gathers, rather than a
    /// collection of its own at once. Never for a retry, which collects
    /// nothing.
    pub(crate) fn edited(self) -> bool {
        match self {
            Self::Retry => false,
            Self::Ready { edited } | Self::Recollect { edited } => edited,
        }
    }

    /// One cause for two of a host's resyncs: ready when either is,
    /// collecting when either does, and edited only when every collecting
    /// one is (any other collects at once).
    pub(crate) fn merge(self, other: Self) -> Self {
        let edited = match (self.collects(), other.collects()) {
            (true, true) => self.edited() && other.edited(),
            (true, false) => self.edited(),
            (false, _) => other.edited(),
        };
        if self.ready() || other.ready() {
            Self::Ready { edited }
        } else if self.collects() || other.collects() {
            Self::Recollect { edited }
        } else {
            Self::Retry
        }
    }
}

/// The answer for one input under one epoch, and the attempts to get it.
struct Cell {
    /// The prepare epoch the input was looked up under: its answer stands
    /// while that is the current one.
    epoch: u64,
    /// Where the input was first looked up in the presentation order (see
    /// `protocol::next_presentation`): an answer to it is as new as its
    /// virtual text, not as the moment it came.
    taken: u64,
    /// The input apart from its epoch and candidates (see [`input_identity`]).
    input: u64,
    /// Set once an attempt got an answer.
    outcome: std::sync::OnceLock<Answer>,
    /// An attempt is running.
    in_flight: AtomicBool,
    /// Attempts that got no answer, for the backoff.
    misses: AtomicU32,
    /// No new attempt before this, after a miss.
    retry_at: std::sync::Mutex<Option<Instant>>,
    /// Bumped after every attempt, answered or not.
    attempts: tokio::sync::watch::Sender<u32>,
    /// The lifecycle pass moved its document onto this input from another
    /// under the same epoch, answered or still pending: the text was edited.
    after_edit: AtomicBool,
    /// The `textDocument.version` of each attempt, in order.
    #[cfg(test)]
    versions: std::sync::Mutex<Vec<i32>>,
}

impl Cell {
    fn new(epoch: u64, input: u64) -> Self {
        Self {
            epoch,
            taken: super::protocol::next_presentation(),
            input,
            outcome: std::sync::OnceLock::new(),
            in_flight: AtomicBool::new(false),
            misses: AtomicU32::new(0),
            retry_at: std::sync::Mutex::new(None),
            attempts: tokio::sync::watch::channel(0).0,
            after_edit: AtomicBool::new(false),
            #[cfg(test)]
            versions: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The answer, while it stands at `epoch` — the one check every reader
    /// goes through. `Err` when the epoch moved since the input was looked
    /// up: no answer to it describes what the candidates answer now.
    fn standing(&self, epoch: u64) -> Result<Option<&Answer>, ()> {
        if self.epoch != epoch {
            return Err(());
        }
        Ok(self.outcome.get())
    }

    fn settled(&self, epoch: u64) -> Result<Answer, PrepareMiss> {
        match self.standing(epoch) {
            Ok(Some(Answer::Unusable)) => Err(PrepareMiss::Failed),
            Ok(Some(answer)) => Ok(answer.clone()),
            Ok(None) | Err(()) => Err(PrepareMiss::Unanswered),
        }
    }

    fn backing_off(&self) -> bool {
        self.retry_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|retry_at| Instant::now() < retry_at)
    }
}

/// Ends an attempt however its task ends — a panic included — so waiters
/// wake and the next lookup may start another. Every release of a claim goes
/// through it, a claim that ran no attempt included: a waiter that saw the
/// claim waits for this bump.
struct AttemptEnd(Arc<Cell>);

impl Drop for AttemptEnd {
    fn drop(&mut self) {
        self.0.in_flight.store(false, Ordering::Release);
        self.0.attempts.send_modify(|attempts| *attempts += 1);
    }
}

/// One input's cell.
#[derive(Clone)]
struct Slot {
    key: u64,
    cell: Arc<Cell>,
}

/// One region's answers.
struct Region {
    /// The region this is for (the map key is their hash).
    host_uri: String,
    injection_language: String,
    region_id: String,
    /// Most recently used first.
    slots: Vec<Slot>,
    /// The input the lifecycle pass holds the document on: only its answer
    /// re-syncs the host, whoever started the attempt.
    held: Option<u64>,
    /// The host incarnation under which the lifecycle pass withheld the
    /// region for a prepare peer known to prepare it, until text is
    /// recorded for it again (see [`PrepareFailures::withhold`]): no request
    /// opens it with text as is meanwhile. Apart from `held`, which outlives
    /// the answer and is set for any lookup: it is kept past a forget of the
    /// region's answers (only a host's close takes it), and read only under
    /// the incarnation it was set in.
    withheld: Option<u64>,
}

impl Region {
    fn is(&self, host_uri: &str, injection_language: &str, region_id: &str) -> bool {
        self.host_uri == host_uri
            && self.injection_language == injection_language
            && self.region_id == region_id
    }

    fn holds(&self, key: u64) -> bool {
        self.held == Some(key)
    }

    fn has(&self, cell: &Arc<Cell>) -> bool {
        self.slots.iter().any(|slot| Arc::ptr_eq(&slot.cell, cell))
    }

    fn bare(host_uri: &str, injection_language: &str, region_id: &str) -> Self {
        Self {
            host_uri: host_uri.to_string(),
            injection_language: injection_language.to_string(),
            region_id: region_id.to_string(),
            slots: Vec::new(),
            held: None,
            withheld: None,
        }
    }

    /// Whether nothing is kept for it: no answer, nothing held or withheld.
    fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.held.is_none() && self.withheld.is_none()
    }

    /// Forget its answers and what the lifecycle pass holds it on, keeping
    /// only its withheld mark; `true` when nothing is left to keep.
    fn forget_answers(&mut self) -> bool {
        self.slots.clear();
        self.held = None;
        self.withheld.is_none()
    }

    fn held_cell(&self) -> Option<&Arc<Cell>> {
        let held = self.held?;
        self.slots
            .iter()
            .find(|slot| slot.key == held)
            .map(|slot| &slot.cell)
    }
}

type Regions = DashMap<u64, Region>;

/// Who looks a document up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// The lifecycle pass, which holds the document until the answer
    /// re-syncs it.
    LifecyclePass,
    /// Anything else — a request (which may wait for the answer itself), a
    /// re-open sweep, a recovery probe: it never moves what the lifecycle
    /// pass holds the document on.
    Request,
}

pub(crate) struct PrepareRegistry {
    /// Keyed by a hash of (host URI, injection language, region id), so a
    /// lookup allocates nothing; the entry holds the identity it checks.
    regions: Arc<Regions>,
    /// What revokes the maps it gave out (see [`Revocations`]).
    revocations: Arc<Revocations>,
    /// Some region was ever looked up: until then every query answers "no
    /// region" without touching the map.
    used: AtomicBool,
    /// Moved whenever what the lifecycle pass holds a document on, or the
    /// answer it holds it on, may have changed: keys
    /// [`Self::any_region_prepared`]'s cached scan.
    held_changes: Arc<AtomicU64>,
    /// The last scan of [`Self::any_region_prepared`]: `(epoch, held
    /// changes, result)`.
    prepared_scan: std::sync::Mutex<Option<(u64, u64, bool)>>,
    /// Hosts closed, until reopened (see [`ClosedHosts`]). Shared with the
    /// attempts.
    closed_hosts: Arc<ClosedHosts>,
    /// How many answers re-synced each host, since it was last forgotten:
    /// what tells a pull that skipped a host's documents whether any of
    /// their answers landed while it ran. Shared with the attempts.
    host_answers: Arc<DashMap<String, u64>>,
    /// Source of `textDocument.version`: shared by all documents, so a
    /// document forgotten and prepared again (a reopen) never repeats a
    /// version the peer saw.
    next_revision: Arc<AtomicI32>,
    resync_tx: UnboundedSender<Resync>,
    resync_rx: std::sync::Mutex<Option<UnboundedReceiver<Resync>>>,
    /// Run once a lookup found its host open, before its region goes in
    /// (see `slot`): where a close can land.
    #[cfg(test)]
    slot_checked: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl std::fmt::Debug for PrepareRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrepareRegistry")
            .field("regions", &self.regions.len())
            .finish_non_exhaustive()
    }
}

impl Default for PrepareRegistry {
    fn default() -> Self {
        let (resync_tx, resync_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            regions: Arc::default(),
            revocations: Arc::default(),
            used: AtomicBool::new(false),
            held_changes: Arc::default(),
            prepared_scan: std::sync::Mutex::new(None),
            closed_hosts: Arc::default(),
            host_answers: Arc::default(),
            next_revision: Arc::new(AtomicI32::new(1)),
            resync_tx,
            resync_rx: std::sync::Mutex::new(Some(resync_rx)),
            #[cfg(test)]
            slot_checked: std::sync::Mutex::new(None),
        }
    }
}

/// Hosts closed, until reopened: a request still running past the close
/// looks nothing up for them, and an attempt for one asks no peer. Each entry
/// goes on a reopen, or once [`CLOSED_HOSTS_KEPT`] later closes were
/// recorded (keyed by the order of closes).
///
/// A set of hosts closed now, not a compare of host incarnations. A request
/// resolved before a close and a reopen looks up under the reopen as any
/// does. And a host counts as closed from the registry's own forget on, not
/// from the moment the pool drops its documents. An incarnation compare
/// would refuse the first and move the second, and some lookups (a pull
/// owed a refresh) carry no incarnation to compare.
#[derive(Default)]
struct ClosedHosts {
    hosts: DashMap<String, u64>,
    /// Source of `hosts`' order.
    closes: AtomicU64,
}

impl ClosedHosts {
    fn contains(&self, host_uri: &str) -> bool {
        self.hosts.contains_key(host_uri)
    }

    fn close(&self, host_uri: &str) {
        let order = self.closes.fetch_add(1, Ordering::AcqRel);
        self.hosts.insert(host_uri.to_string(), order);
        if self.hosts.len() > CLOSED_HOSTS_KEPT
            && let Some(oldest) = self
                .hosts
                .iter()
                .min_by_key(|entry| *entry.value())
                .map(|entry| entry.key().clone())
        {
            self.hosts.remove(&oldest);
        }
    }

    fn reopen(&self, host_uri: &str) {
        self.hosts.remove(host_uri);
    }
}

/// The [`Revocation`](super::protocol::Revocation) of each input of each
/// region a live map was answered for: held weakly, so a record lives
/// exactly as long as a map that a failure of its input could revoke.
#[derive(Default)]
struct Revocations {
    records: DashMap<(u64, u64), std::sync::Weak<super::protocol::Revocation>>,
    /// Records seen live at the last sweep of dead ones: the next sweeps
    /// once the table has doubled since.
    swept: AtomicU64,
}

impl Revocations {
    /// The record answers for `input` of `region` share, created if no
    /// live map holds one.
    fn record(&self, region: u64, input: u64) -> Arc<super::protocol::Revocation> {
        let record = {
            let mut entry = self.records.entry((region, input)).or_default();
            match entry.upgrade() {
                Some(record) => record,
                None => {
                    let record = Arc::default();
                    *entry = Arc::downgrade(&record);
                    record
                }
            }
        };
        let len = self.records.len() as u64;
        if len > 2 * self.swept.load(Ordering::Relaxed).max(16) {
            self.records.retain(|_, record| record.strong_count() > 0);
            self.swept
                .store(self.records.len() as u64, Ordering::Relaxed);
        }
        record
    }

    /// Record a failure of `input` of `region` taken at `taken`: every live
    /// map answered for it before then is revoked.
    fn revoke(&self, region: u64, input: u64, taken: u64) {
        if let Some(record) = self
            .records
            .get(&(region, input))
            .and_then(|record| record.upgrade())
        {
            record.revoke(taken);
        }
    }
}

/// The failures the lifecycle pass holds regions on, and the regions it
/// withholds for their prepare peers, read from the registry's own table:
/// what a request or a lazy open asks before it reads coordinates through a
/// region, or opens it with text as is, at the moment an answer is recorded,
/// with no copy to wait for.
#[derive(Clone, Default)]
pub(crate) struct PrepareFailures {
    regions: Arc<Regions>,
    /// The registry's, marked by the test helpers as the registry marks it.
    #[cfg(test)]
    revocations: Arc<Revocations>,
}

/// What the registry's table says of a text as is for one region, read
/// under one guard (see [`PrepareFailures::admission`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RegionStanding {
    /// The failure the lifecycle pass holds the region on stands at the
    /// epoch asked about, so the region is not bridged.
    pub(crate) failed: bool,
    /// The host incarnation the region is withheld under, if any (see
    /// `Region::withheld`).
    pub(crate) withheld: Option<u64>,
}

impl PrepareFailures {
    /// What a text as is meets for `region_id` (under `injection_language`)
    /// of `host_uri` at `epoch` (see [`RegionStanding`]). (Whether a
    /// prepared text is refused is the map's own to say: see
    /// `PreparedMap::revoked`.)
    pub(crate) fn admission(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        epoch: u64,
    ) -> RegionStanding {
        let host_uri = host_uri.as_str();
        self.regions
            .get(&region_hash(host_uri, injection_language, region_id))
            .filter(|region| region.is(host_uri, injection_language, region_id))
            .map(|region| RegionStanding {
                failed: region.held_cell().is_some_and(|cell| {
                    matches!(cell.outcome.get(), Some(Answer::Unusable)) && cell.epoch == epoch
                }),
                withheld: region.withheld,
            })
            .unwrap_or_default()
    }

    /// Withhold `region_id` (under `injection_language`) of `host_uri` for
    /// its prepare peer under the host's `incarnation` (see
    /// `Region::withheld`), keeping whatever else its region holds.
    pub(crate) fn withhold(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        incarnation: u64,
    ) {
        let host_uri = host_uri.as_str();
        let mut region = self
            .regions
            .entry(region_hash(host_uri, injection_language, region_id))
            .or_insert_with(|| Region::bare(host_uri, injection_language, region_id));
        if !region.is(host_uri, injection_language, region_id) {
            // A hash collision with another region: start over, its answers
            // gone with it (as `slot`'s reset takes a withheld mark).
            *region = Region::bare(host_uri, injection_language, region_id);
        }
        region.withheld = Some(incarnation);
    }

    /// Text was recorded for `region_id` (under `injection_language`) of
    /// `host_uri` under the host's `incarnation`: it is withheld no more.
    pub(crate) fn release(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        incarnation: u64,
    ) {
        let host_uri = host_uri.as_str();
        let hash = region_hash(host_uri, injection_language, region_id);
        // Read first, under the shard's read lock: every recorded text
        // comes here, and most regions are withheld by nothing.
        if !self
            .regions
            .get(&hash)
            .is_some_and(|region| region.withheld == Some(incarnation))
        {
            return;
        }
        self.regions.remove_if_mut(&hash, |_, region| {
            if region.is(host_uri, injection_language, region_id)
                && region.withheld == Some(incarnation)
            {
                region.withheld = None;
            }
            region.is_empty()
        });
    }

    /// An answer for `input`, taken at `taken` in the presentation order, as
    /// the registry gives it out (its input named, sharing its input's
    /// record).
    #[cfg(test)]
    pub(crate) fn answered_for_test(
        &self,
        input: PrepareInput<'_>,
        taken: u64,
    ) -> super::protocol::PreparedDocument {
        let mut prepared = super::protocol::apply_prepare_result_at(
            input.virtual_text,
            &super::protocol::VirtualLayout::single(input.virtual_text),
            None,
            taken,
        )
        .expect("a null answer");
        let map = Arc::get_mut(&mut prepared.map).expect("fresh");
        map.set_input(input_identity(input));
        self.share_for_test(
            input.host_uri,
            input.injection_language,
            input.region_id,
            map,
        );
        prepared
    }

    /// Share the record of `map`'s input of `region_id` (under
    /// `injection_language`) of `host_uri`, as an answer the registry gave.
    #[cfg(test)]
    pub(crate) fn share_for_test(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        map: &mut super::protocol::PreparedMap,
    ) {
        let input = map.origin().input;
        map.set_revocation(self.revocations.record(
            region_hash(host_uri.as_str(), injection_language, region_id),
            input,
        ));
    }

    /// Hold `region_id` (under `injection_language`) of `host_uri` on an
    /// input looked up at `epoch`: failed, taken and identified as `failure`
    /// says, or (`None`) still waiting for its answer.
    #[cfg(test)]
    pub(crate) fn hold_for_test(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        epoch: u64,
        failure: Option<super::protocol::AnswerOrigin>,
    ) {
        let mut cell = Cell::new(epoch, failure.map_or(0, |failure| failure.input));
        if let Some(failure) = failure {
            cell.taken = failure.taken;
            self.revocations.revoke(
                region_hash(host_uri.as_str(), injection_language, region_id),
                failure.input,
                failure.taken,
            );
            let _ = cell.outcome.set(Answer::Unusable);
        }
        let hash = region_hash(host_uri.as_str(), injection_language, region_id);
        // A withheld mark stays, as through any forget of its answers.
        let withheld = self
            .regions
            .get(&hash)
            .filter(|region| region.is(host_uri.as_str(), injection_language, region_id))
            .and_then(|region| region.withheld);
        self.regions.insert(
            hash,
            Region {
                host_uri: host_uri.to_string(),
                injection_language: injection_language.to_string(),
                region_id: region_id.to_string(),
                slots: vec![Slot {
                    key: 0,
                    cell: Arc::new(cell),
                }],
                held: Some(0),
                withheld,
            },
        );
    }
}

impl PrepareRegistry {
    /// The failures the lifecycle pass holds regions on (see
    /// [`PrepareFailures`]), shared with the table they are read from.
    pub(crate) fn failures(&self) -> PrepareFailures {
        PrepareFailures {
            regions: Arc::clone(&self.regions),
            #[cfg(test)]
            revocations: Arc::clone(&self.revocations),
        }
    }

    /// Whether no region was ever looked up (see `used`).
    pub(crate) fn unused(&self) -> bool {
        !self.used.load(Ordering::Acquire)
    }

    /// Queue `resync` again after `delay` — for a pass that did not sync,
    /// which nothing else would repeat, or for a pull owed a refresh whose
    /// documents no longer wait. A shutdown drops it.
    pub(crate) fn requeue_resync(&self, resync: Resync, delay: Duration) {
        let resync_tx = self.resync_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = resync_tx.send(resync);
        });
    }

    /// Hosts to sync again: a held-back virtual document became ready, its
    /// answer came back unusable, an attempt that got no answer finished its
    /// retry backoff, a pass that did not sync or a pull owed a refresh is
    /// synced again ([`Self::requeue_resync`]), or a lifecycle lookup found
    /// its target resolved under a moved epoch. Taken once by the server loop.
    pub(crate) fn take_resync_rx(&self) -> Option<UnboundedReceiver<Resync>> {
        self.resync_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Look up for the lifecycle pass without waiting, which then holds the
    /// document on this input; on a miss, start an attempt in the background
    /// and report [`PrepareLookup::Pending`].
    pub(crate) fn lookup_or_start(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        let epoch = pool.prepare_epoch();
        let Some(slot) = self.slot(target, input, Holder::LifecyclePass, epoch) else {
            // A pass from before the epoch moved holds the document with no
            // answer coming: sync the host again, under the current one. (A
            // closed host is synced to nothing: its document is gone.)
            let _ = self.resync_tx.send(Resync {
                host_uri: input.host_uri.clone(),
                cause: ResyncCause::Retry,
            });
            return PrepareLookup::Pending;
        };
        if let Ok(Some(answer)) = slot.cell.standing(epoch) {
            return PrepareLookup::Answered(answer.clone());
        }
        self.start(&slot, pool, target, input);
        PrepareLookup::Pending
    }

    /// Look up without waiting for anything other than the lifecycle pass (a
    /// diagnostic snapshot, an editor's pull on a push-only region, a
    /// re-open sweep or recovery probe): on a miss, start an attempt in the
    /// background. It never moves what the lifecycle pass holds the
    /// document on.
    pub(crate) fn lookup_for_request(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        let epoch = pool.prepare_epoch();
        let Some(slot) = self.slot(target, input, Holder::Request, epoch) else {
            return PrepareLookup::Pending;
        };
        if let Ok(Some(answer)) = slot.cell.standing(epoch) {
            return PrepareLookup::Answered(answer.clone());
        }
        self.start(&slot, pool, target, input);
        PrepareLookup::Pending
    }

    /// Wait for the answer, sharing an attempt already running. A request
    /// never moves what the lifecycle pass holds the document on.
    pub(crate) async fn prepare(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> Result<Answer, PrepareMiss> {
        let epoch = pool.prepare_epoch();
        let Some(slot) = self.slot(target, input, Holder::Request, epoch) else {
            return Err(PrepareMiss::Unanswered);
        };
        if slot.cell.outcome.get().is_some() {
            return slot.cell.settled(pool.prepare_epoch());
        }
        // Subscribe before starting, so the attempt's end cannot be missed.
        let mut attempts = slot.cell.attempts.subscribe();
        if !self.start(&slot, pool, target, input) && !slot.cell.in_flight.load(Ordering::Acquire) {
            // Backing off after a miss: do not queue another attempt per
            // request, each waiting out its own timeouts.
            return slot.cell.settled(pool.prepare_epoch());
        }
        // The attempt ends (answered or not) with a bump.
        let _ = attempts.changed().await;
        slot.cell.settled(pool.prepare_epoch())
    }

    /// What the lifecycle pass knows of a region's current document (see
    /// [`RegionPrepare`]), by the answer it holds the document on, as it
    /// stands at `epoch`: one from before the epoch moved is no answer, so
    /// the region reads as waiting for one until the pass looks it up again.
    pub(crate) fn region(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        epoch: u64,
    ) -> RegionPrepare {
        let host_uri = host_uri.as_str();
        self.regions
            .get(&region_hash(host_uri, injection_language, region_id))
            .filter(|region| region.is(host_uri, injection_language, region_id))
            .and_then(|region| {
                region.held_cell().map(|cell| match cell.standing(epoch) {
                    Ok(Some(Answer::AsIs)) => RegionPrepare::default(),
                    Ok(Some(Answer::Unusable)) => RegionPrepare {
                        prepared: true,
                        failed: true,
                    },
                    Ok(Some(Answer::Prepared(_)) | None) | Err(()) => RegionPrepare {
                        prepared: true,
                        failed: false,
                    },
                })
            })
            .unwrap_or_default()
    }

    /// Whether the lifecycle pass prepares any region (holds it on an answer
    /// other than as is, or one it waits for) as it stands at `epoch`: see
    /// [`Self::region`].
    /// Scanned once per epoch and change of what is held, not per call.
    pub(crate) fn any_region_prepared(&self, epoch: u64) -> bool {
        let changes = self.held_changes.load(Ordering::Acquire);
        let cached = *self
            .prepared_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((scanned_epoch, scanned_changes, prepared)) = cached
            && (scanned_epoch, scanned_changes) == (epoch, changes)
        {
            return prepared;
        }
        let prepared = self.regions.iter().any(|region| {
            region
                .held_cell()
                .is_some_and(|cell| !matches!(cell.standing(epoch), Ok(Some(Answer::AsIs))))
        });
        // Keyed by the changes read before the scan: one made during it
        // makes the next call scan again.
        *self
            .prepared_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((epoch, changes, prepared));
        prepared
    }

    /// Whether any of `host_uri`'s documents the lifecycle pass holds back
    /// still waits for its answer as it stands at `epoch`.
    pub(crate) fn host_has_pending(&self, host_uri: &Url, epoch: u64) -> bool {
        self.regions.iter().any(|region| {
            region.host_uri == host_uri.as_str()
                && region
                    .held_cell()
                    .is_some_and(|cell| !matches!(cell.standing(epoch), Ok(Some(_))))
        })
    }

    /// Forget one region's answers: it was replaced or invalidated, or its
    /// pair lost its peer. `injection_language` `None` forgets it under
    /// every language. Its withheld mark stays (see `Region::withheld`).
    pub(crate) fn forget_region(
        &self,
        host_uri: &Url,
        injection_language: Option<&str>,
        region_id: &str,
    ) {
        let host_uri = host_uri.as_str();
        match injection_language {
            Some(language) => {
                let mut forgot = false;
                self.regions.remove_if_mut(
                    &region_hash(host_uri, language, region_id),
                    |_, region| {
                        forgot = region.is(host_uri, language, region_id);
                        forgot && region.forget_answers()
                    },
                );
                if forgot {
                    self.held_changes.fetch_add(1, Ordering::AcqRel);
                }
            }
            None => self.forget_where(|region| {
                region.host_uri == host_uri && region.region_id == region_id
            }),
        }
    }

    /// Forget the answers of the regions `goes` accepts, keeping their
    /// withheld marks (see `Region::withheld`), noting a change of what is
    /// held only when one went (an unchanged registry keeps its cached scan).
    fn forget_where(&self, goes: impl Fn(&Region) -> bool) {
        let mut removed = false;
        self.regions.retain(|_, region| {
            let gone = goes(region);
            removed |= gone;
            !gone || !region.forget_answers()
        });
        if removed {
            self.held_changes.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Forget `host_uri`'s regions of `region_ids` under every language, in
    /// one sweep: they were invalidated together.
    pub(crate) fn forget_regions(&self, host_uri: &Url, region_ids: &[String]) {
        if region_ids.is_empty() || self.unused() {
            return;
        }
        let host_uri = host_uri.as_str();
        self.forget_where(|region| {
            region.host_uri == host_uri && region_ids.contains(&region.region_id)
        });
    }

    /// Forget a replaced region under every language but the one it resolves
    /// to now (`None`: every one): a region re-routed to another language
    /// keeps its id, and its document under the new language may be being
    /// prepared already.
    pub(crate) fn forget_region_except(&self, host_uri: &Url, region_id: &str, keep: Option<&str>) {
        let host_uri = host_uri.as_str();
        self.forget_where(|region| {
            region.host_uri == host_uri
                && region.region_id == region_id
                && Some(region.injection_language.as_str()) != keep
        });
    }

    /// Whether `host_uri` closed (see `closed_hosts`).
    pub(crate) fn host_closed(&self, host_uri: &Url) -> bool {
        self.closed_hosts.contains(host_uri.as_str())
    }

    /// `host_uri` opened (again): its documents are looked up as any.
    pub(crate) fn host_opened(&self, host_uri: &Url) {
        self.closed_hosts.reopen(host_uri.as_str());
    }

    /// How many answers re-synced `host_uri` (see `host_answers`).
    pub(crate) fn host_answers(&self, host_uri: &Url) -> u64 {
        self.host_answers
            .get(host_uri.as_str())
            .map_or(0, |answers| *answers)
    }

    /// Forget a closed host's documents: until it opens again, nothing is
    /// looked up for it.
    pub(crate) fn forget_host(&self, host_uri: &Url) {
        self.host_answers.remove(host_uri.as_str());
        // Every close, whether anything was looked up yet or not: a request
        // resolved before the close may still be about to look up.
        self.closed_hosts.close(host_uri.as_str());
        // Withheld marks too: they were set under the incarnation that
        // closed, which no open reads them under.
        let mut removed = false;
        self.regions.retain(|_, region| {
            let gone = region.host_uri == host_uri.as_str();
            removed |= gone;
            !gone
        });
        if removed {
            self.held_changes.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Settle `input`'s answer as an attempt would, the lifecycle pass
    /// holding its document on it when `held`.
    #[cfg(test)]
    pub(crate) fn answer_for_test(
        &self,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        answer: Answer,
        held: bool,
    ) {
        let holder = if held {
            Holder::LifecyclePass
        } else {
            Holder::Request
        };
        let slot = self
            .slot(target, input, holder, target.epoch)
            .expect("an input under the current epoch");
        if matches!(answer, Answer::Unusable) {
            self.revocations.revoke(
                region_hash(
                    input.host_uri.as_str(),
                    input.injection_language,
                    input.region_id,
                ),
                slot.cell.input,
                slot.cell.taken,
            );
        }
        assert!(slot.cell.outcome.set(answer).is_ok(), "answered already");
        self.held_changes.fetch_add(1, Ordering::AcqRel);
        if held {
            *self
                .host_answers
                .entry(input.host_uri.to_string())
                .or_default() += 1;
        }
    }

    /// The slot answering `input` under `epoch`, created when missing.
    /// `None` for a target resolved under another epoch (settings changed
    /// since, say): its candidates may not be the ones to ask, and asking
    /// could start a peer the settings no longer name. `None` too for a
    /// closed host.
    fn slot(
        &self,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        holder: Holder,
        epoch: u64,
    ) -> Option<Slot> {
        if target.epoch != epoch {
            return None;
        }
        let host_uri = input.host_uri.as_str();
        // Closed: whatever still runs past the close finds nothing, and
        // starts nothing.
        if self.closed_hosts.contains(host_uri) {
            return None;
        }
        #[cfg(test)]
        if let Some(hook) = self
            .slot_checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hook();
        }
        self.used.store(true, Ordering::Release);
        let key = input_key(target, input);
        let hash = region_hash(host_uri, input.injection_language, input.region_id);
        let mut region = self
            .regions
            .entry(hash)
            .or_insert_with(|| Region::bare(host_uri, input.injection_language, input.region_id));
        if !region.is(host_uri, input.injection_language, input.region_id) {
            // A hash collision with another region: start over, its
            // withheld mark gone with it (as `PrepareFailures::withhold`'s
            // reset takes its answers).
            *region = Region::bare(host_uri, input.injection_language, input.region_id);
        }
        // Moved off an input of the same epoch, answered or not: an edit.
        // Typing ahead of a slow peer moves off pending inputs, and those
        // answers belong to the burst the edit's debounce collects once.
        let moved = holder == Holder::LifecyclePass && region.held != Some(key);
        let edited = moved
            && region
                .held_cell()
                .is_some_and(|cell| cell.epoch == target.epoch);
        if moved {
            region.held = Some(key);
            self.held_changes.fetch_add(1, Ordering::AcqRel);
        }
        let slot = match region.slots.iter().position(|slot| slot.key == key) {
            Some(index) => region.slots.remove(index),
            None => Slot {
                key,
                cell: Arc::new(Cell::new(target.epoch, input_identity(input))),
            },
        };
        if moved {
            slot.cell.after_edit.store(edited, Ordering::Release);
        }
        region.slots.insert(0, slot.clone());
        // Evict the least recently used beyond the bound, never the one the
        // lifecycle pass holds on, nor one in use: returned to a lookup that
        // has yet to start or wait on it, or with its attempt in flight (an
        // evicted one's attempt is refused admission, and a lookup for its
        // input would start a second request for it), nor the one being
        // looked up, at the front. Only the region's own reference is unused:
        // a reference is only taken under this region's guard, so a count
        // read here can only overstate use. (The bound is exceeded only by
        // the slots in use.)
        while region.slots.len() > SLOTS_PER_REGION + 1 {
            let held = region.held;
            let Some(index) = region.slots[1..]
                .iter()
                .rposition(|slot| Some(slot.key) != held && Arc::strong_count(&slot.cell) == 1)
            else {
                break;
            };
            region.slots.remove(index + 1);
        }
        drop(region);
        // Closed meanwhile: the close may have swept the host's regions
        // before this one went in (it marks the host closed first, so a
        // sweep that missed this region is seen here). Nothing is kept for
        // a closed host.
        if self.closed_hosts.contains(host_uri) {
            let removed = self
                .regions
                .remove_if(&hash, |_, region| {
                    region.is(host_uri, input.injection_language, input.region_id)
                })
                .is_some();
            if removed {
                self.held_changes.fetch_add(1, Ordering::AcqRel);
            }
            return None;
        }
        Some(slot)
    }

    /// Start an attempt unless one is running, an answer is in, or a miss is
    /// backing off. `true` when this call started it.
    fn start(
        &self,
        slot: &Slot,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> bool {
        let cell = &slot.cell;
        if cell.outcome.get().is_some() || cell.backing_off() {
            return false;
        }
        if cell.in_flight.swap(true, Ordering::AcqRel) {
            return false;
        }
        // An attempt that ended between the checks above and the claim (it
        // sets its outcome or backoff before releasing `in_flight`) still
        // decides: no second request for an answered input, and none inside
        // the backoff that attempt just set.
        if cell.outcome.get().is_some() || cell.backing_off() {
            drop(AttemptEnd(Arc::clone(cell)));
            return false;
        }
        let end = AttemptEnd(Arc::clone(cell));
        let regions = Arc::clone(&self.regions);
        let region = region_hash(
            input.host_uri.as_str(),
            input.injection_language,
            input.region_id,
        );
        let key = slot.key;
        // Every request takes the next version as it goes out: a retry after
        // another input was asked (an undo while the first was backing off)
        // must not reuse an older one. (Attempts for different inputs run on
        // their own tasks, so two started together may still reach the peer
        // in either order.)
        let revisions = Arc::clone(&self.next_revision);
        let pool = Arc::clone(pool);
        let candidates = Arc::clone(&target.candidates);
        let job = PrepareJob::new(
            input,
            cell.taken,
            cell.input,
            self.revocations.record(region, cell.input),
        );
        let revocations = Arc::clone(&self.revocations);
        let resync_tx = self.resync_tx.clone();
        let held_changes = Arc::clone(&self.held_changes);
        let closed_hosts = Arc::clone(&self.closed_hosts);
        let host_answers = Arc::clone(&self.host_answers);
        tokio::spawn(async move {
            let cell = Arc::clone(&end.0);
            // Its cell still stands: kept by its region, under the current
            // epoch. One forgotten (its host closed, its region replaced) or
            // outdated (settings changed, say) must not start or ask a peer
            // the settings may no longer name.
            let admitted = || {
                pool.prepare_epoch() == cell.epoch
                    && !closed_hosts.contains(job.host_uri.as_str())
                    && regions.get(&region).is_some_and(|region| region.has(&cell))
            };
            let held = || {
                regions
                    .get(&region)
                    .is_some_and(|region| region.has(&cell) && region.holds(key))
            };
            let next_version = || {
                let version = next_revision(&revisions);
                #[cfg(test)]
                cell.versions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(version);
                version
            };
            let retrying = cell.misses.load(Ordering::Acquire) > 0;
            match run(&pool, &candidates, &job, &next_version, &admitted, retrying).await {
                Ok(answer) => {
                    let ready = !matches!(answer, Answer::Unusable);
                    // Revoked before the failure is published: a request
                    // reading it already finds its map revoked.
                    if !ready {
                        revocations.revoke(region, job.identity, job.taken);
                    }
                    let _ = cell.outcome.set(answer);
                    held_changes.fetch_add(1, Ordering::AcqRel);
                    // The answer the lifecycle pass holds the document on
                    // re-syncs the host, whoever asked. An unusable one
                    // does too, unready: nothing more is coming for this
                    // input, which a pull owed a refresh must learn. One
                    // outdated meanwhile re-syncs nothing: the epoch's move
                    // does.
                    if held() && pool.prepare_epoch() == cell.epoch {
                        *host_answers.entry(job.host_uri.to_string()).or_default() += 1;
                        // The receiver is gone only at shutdown.
                        let edited = cell.after_edit.load(Ordering::Acquire);
                        let _ = resync_tx.send(Resync {
                            host_uri: job.host_uri.clone(),
                            cause: if ready {
                                ResyncCause::Ready { edited }
                            } else {
                                ResyncCause::Recollect { edited }
                            },
                        });
                    }
                }
                Err(()) => {
                    let misses = cell.misses.fetch_add(1, Ordering::AcqRel) + 1;
                    let delay = retry_delay(misses);
                    *cell
                        .retry_at
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(Instant::now() + delay);
                    // Nothing else would look the document up again before
                    // the next edit: sync the host once the backoff ends,
                    // unless newer text (or a newer epoch) has taken over by
                    // then. Holding the cell until then also keeps it from
                    // eviction for its whole backoff (only a cell nothing
                    // else references is evicted), so a lookup of its input
                    // meanwhile keeps its misses rather than asking afresh.
                    let host_uri = job.host_uri.clone();
                    let cell = Arc::clone(&cell);
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let still_held = regions
                            .get(&region)
                            .is_some_and(|region| region.has(&cell) && region.holds(key));
                        if still_held {
                            let _ = resync_tx.send(Resync {
                                host_uri,
                                cause: ResyncCause::Retry,
                            });
                        }
                    });
                }
            }
            drop(end);
        });
        true
    }
}

fn next_revision(counter: &AtomicI32) -> i32 {
    // Wraps after 2^31 prepares; the peer only compares a document's
    // versions over its lifetime.
    counter.fetch_add(1, Ordering::Relaxed)
}

/// Backoff before retrying an attempt that got no answer: one second,
/// doubling, at most a minute.
pub(crate) fn retry_delay(misses: u32) -> Duration {
    Duration::from_secs((1u64 << misses.saturating_sub(1).min(6)).min(60))
}

fn region_hash(host_uri: &str, injection_language: &str, region_id: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (host_uri, injection_language, region_id).hash(&mut hasher);
    hasher.finish()
}

/// An input's identity apart from the epoch and candidates it is asked
/// under: the same text, asked again after the epoch moved, has the same
/// one (what a `protocol::Revocation` record is kept per).
fn input_identity(input: PrepareInput<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.host_language.hash(&mut hasher);
    input.virtual_text.hash(&mut hasher);
    input.gaps.hash(&mut hasher);
    hasher.finish()
}

/// The input's key among a region's answers: its text and gaps, the host
/// language and the candidates (by name and launch config), under the
/// epoch the target was resolved in.
fn input_key(target: &PrepareTarget, input: PrepareInput<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.epoch.hash(&mut hasher);
    for candidate in target.candidates.iter() {
        candidate.server_name.hash(&mut hasher);
        // The launch config by identity: one settings snapshot shares its
        // `Arc`s, so a target resolved from other settings — whatever its
        // epoch reads — never shares a cell (or its answer) with this one.
        std::ptr::hash(Arc::as_ptr(&candidate.config), &mut hasher);
    }
    input.host_language.hash(&mut hasher);
    input.virtual_text.hash(&mut hasher);
    input.gaps.hash(&mut hasher);
    hasher.finish()
}

/// An owned [`PrepareInput`], for the background request.
struct PrepareJob {
    /// See `Cell::taken`.
    taken: u64,
    /// See `Cell::input`.
    identity: u64,
    /// What revokes the map its answer yields (see [`Revocations`]).
    revocation: Arc<super::protocol::Revocation>,
    host_uri: Url,
    host_language: String,
    injection_language: String,
    region_id: String,
    virtual_text: String,
    gaps: Vec<VirtualGap>,
}

impl PrepareJob {
    fn new(
        input: PrepareInput<'_>,
        taken: u64,
        identity: u64,
        revocation: Arc<super::protocol::Revocation>,
    ) -> Self {
        Self {
            taken,
            identity,
            revocation,
            host_uri: input.host_uri.clone(),
            host_language: input.host_language.to_string(),
            injection_language: input.injection_language.to_string(),
            region_id: input.region_id.to_string(),
            virtual_text: input.virtual_text.to_string(),
            gaps: input.gaps.to_vec(),
        }
    }
}

/// Ask the first candidate advertising the request and apply its answer.
/// `Err` is no answer at all (retried); an unusable or refused answer is
/// [`Answer::Unusable`]. Both are logged; neither lets the document
/// through unprepared. An input no candidate advertises the request for is
/// sent as is.
async fn run(
    pool: &LanguageServerPool,
    candidates: &[ResolvedServerConfig],
    job: &PrepareJob,
    next_version: &(dyn Fn() -> i32 + Sync),
    admitted: &(dyn Fn() -> bool + Sync),
    retrying: bool,
) -> Result<Answer, ()> {
    // The error carries the peer's own text (its error object, the variant
    // names of a malformed answer): escaped, as is the language name.
    let log_failure = |error: &dyn std::fmt::Display| {
        log::warn!(
            target: "kakehashi::bridge::prepare",
            "Not sending the {} virtual document of {}: {}",
            escape_terminal_controls(&job.injection_language),
            job.host_uri,
            escape_terminal_controls(&error.to_string())
        );
    };
    match try_run(pool, candidates, job, next_version, admitted).await {
        Ok(Ok(Some(prepared))) => Ok(Answer::Prepared(Arc::new(prepared))),
        Ok(Ok(None)) => Ok(Answer::AsIs),
        Ok(Err(refused)) => {
            log_failure(&refused);
            Ok(Answer::Unusable)
        }
        Err(unanswered) => {
            // A revoked attempt (its host closed, its region replaced, the
            // epoch moved) is no failure worth a warning, nor is a retry's
            // miss once the first was warned about.
            if !admitted() {
                log::debug!(
                    target: "kakehashi::bridge::prepare",
                    "Dropped an outdated prepare attempt for {}: {}",
                    job.host_uri,
                    escape_terminal_controls(&unanswered.to_string())
                );
            } else if retrying {
                log::debug!(
                    target: "kakehashi::bridge::prepare",
                    "Still no answer for the {} virtual document of {}: {}",
                    escape_terminal_controls(&job.injection_language),
                    job.host_uri,
                    escape_terminal_controls(&unanswered.to_string())
                );
            } else {
                log_failure(&unanswered);
            }
            Err(())
        }
    }
}

/// The outer `Err` is a failure to get an answer (worth retrying, a
/// retryable cancellation included); the inner one is final for the input:
/// an answer that cannot be used (an error response other than a retryable
/// cancellation, a malformed or refused result). `Ok(Ok(None))`: no
/// candidate prepares it.
///
/// Candidates are asked strictly in priority order: one still starting is
/// waited for, and one that cannot start, or does not advertise the request,
/// is passed over. Only the first that advertises it is asked, and its
/// answer stands — a failure does not fall through to the next, which would
/// let a flaky peer switch which preparation servers see.
async fn try_run(
    pool: &LanguageServerPool,
    candidates: &[ResolvedServerConfig],
    job: &PrepareJob,
    next_version: &(dyn Fn() -> i32 + Sync),
    admitted: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<std::io::Result<Option<PreparedDocument>>> {
    let host_uri = match crate::lsp::lsp_impl::url_to_uri(&job.host_uri) {
        Ok(host_uri) => host_uri,
        Err(error) => return Ok(Err(std::io::Error::other(error.to_string()))),
    };
    let virtual_uri =
        VirtualDocumentUri::new(&host_uri, &job.injection_language, &job.region_id).to_uri_string();
    let layout = VirtualLayout::from_gaps(&job.virtual_text, &job.gaps);
    let params = |version| {
        PrepareParams::new(
            PrepareTextDocument {
                uri: &virtual_uri,
                language_id: &job.injection_language,
                version,
            },
            PrepareHostTextDocument {
                uri: job.host_uri.as_str(),
                language_id: &job.host_language,
            },
            &layout,
        )
    };
    for candidate in candidates {
        let name = &candidate.server_name;
        if pool.prepare_advertisement(name) == Some(false)
            || pool.prepare_start_failed(name, &job.host_uri)
        {
            continue;
        }
        let failures_cleared = pool.prepare_start_failures_cleared(name, &job.host_uri);
        let handle = match pool
            .get_or_create_connection_wait_ready_admitted(
                name,
                &candidate.config,
                Some(&job.host_uri),
                Duration::from_secs(super::INIT_TIMEOUT_SECS),
                Some(admitted),
                None,
            )
            .await
        {
            Ok(handle) => handle,
            // Revoked: no answer for this input.
            Err(error) if !admitted() => return Err(error),
            // Passed over for this host's documents, so later inputs are not
            // held back to start it again: only for this host, whose
            // workspace root may be the one it cannot start in. Recorded, it
            // is not started again per input until its handshake advertises
            // the request (its pairs go out as is unless a later candidate
            // prepares them): worth seeing at the default level.
            Err(error) => {
                log::warn!(
                    target: "kakehashi::bridge::prepare",
                    "Prepare candidate {} for {} did not start; passing it over: {}",
                    escape_terminal_controls(name),
                    job.host_uri,
                    escape_terminal_controls(&error.to_string()),
                );
                if !pool.record_prepare_start_failure(name, &job.host_uri, failures_cleared) {
                    // Cleared while it started: the failure says nothing
                    // current, and an answer passing over it would not
                    // stand. No answer; retried.
                    return Err(std::io::Error::other(
                        "a candidate's start failure was outdated while asking",
                    ));
                }
                continue;
            }
        };
        if !handle.supports_virtual_document_prepare() {
            continue;
        }
        // Revoked while the candidate started: no request with the text it
        // was for.
        if !admitted() {
            return Err(std::io::Error::other(
                "outdated before its prepare request went out",
            ));
        }
        let result = match handle
            .request_virtual_document_prepare(&params(next_version()))
            .await
        {
            Ok(result) => result,
            // It does not answer the request after all (`MethodNotFound`
            // withdraws the advertisement, which moves the epoch): this
            // attempt is outdated, and the next one passes it over.
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                // Once, as the withdrawal lands: a misconfigured peer whose
                // documents now go out as is.
                if pool.record_prepare_advertisement_from(&handle, false) {
                    log::warn!(
                        target: "kakehashi::bridge::prepare",
                        "{} advertised {} but does not implement it; no longer asking it",
                        escape_terminal_controls(name),
                        super::protocol::PREPARE_METHOD,
                    );
                }
                return Err(error);
            }
            // Final for this input: an unusable answer (an error response or
            // a malformed result).
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Ok(Err(std::io::Error::new(
                    error.kind(),
                    format!("{name} could not prepare it: {error}"),
                )));
            }
            // Cancelled by the peer with a code LSP lets a client retry.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{name} cancelled the request; asking again: {error}"),
                ));
            }
            // No answer: timed out, connection gone.
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{name} did not answer: {error}"),
                ));
            }
        };
        return Ok(
            apply_prepare_result_at(&job.virtual_text, &layout, result, job.taken)
                .map(|mut prepared| {
                    // Fresh: its only reference.
                    if let Some(map) = Arc::get_mut(&mut prepared.map) {
                        #[cfg(test)]
                        map.set_input(job.identity);
                        map.set_revocation(Arc::clone(&job.revocation));
                    }
                    Some(prepared)
                })
                .map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{name} answered unusably: {error}"),
                    )
                }),
        );
    }
    Ok(Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::BridgeServerConfig;
    use crate::lsp::bridge::pool::ConnectionKey;

    /// A host closed while one of its regions is looked up keeps nothing:
    /// the close's sweep may run before the region goes in, and the lookup
    /// then takes it back out itself.
    #[test]
    fn a_lookup_racing_its_hosts_close_leaves_no_region() {
        let registry = Arc::new(PrepareRegistry::default());
        let host_uri = Url::parse("file:///test/closing.md").unwrap();
        let target = PrepareTarget {
            candidates: Arc::from([]),
            epoch: 0,
        };
        let input = PrepareInput {
            host_uri: &host_uri,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "region",
            virtual_text: "a",
            gaps: &[],
        };
        {
            let closer = Arc::clone(&registry);
            let host_uri = host_uri.clone();
            *registry.slot_checked.lock().unwrap() =
                Some(Box::new(move || closer.forget_host(&host_uri)));
        }
        assert!(
            registry
                .slot(&target, input, Holder::LifecyclePass, 0)
                .is_none()
        );
        assert!(
            registry
                .regions
                .iter()
                .all(|region| region.host_uri != host_uri.as_str()),
            "nothing kept for the closed host"
        );
    }

    /// A failure revokes the maps answered for its input before it for as
    /// long as they live, however the registry's answers are evicted
    /// meanwhile: what revokes them is the maps' own.
    #[test]
    fn a_failure_revokes_its_inputs_maps_past_the_eviction_of_its_answer() {
        let registry = PrepareRegistry::default();
        let failures = registry.failures();
        let host_uri = Url::parse("file:///test/revoked-evicted.md").unwrap();
        let target = PrepareTarget {
            candidates: Arc::from([]),
            epoch: 0,
        };
        let input = |virtual_text| PrepareInput {
            host_uri: &host_uri,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "region",
            virtual_text,
            gaps: &[],
        };
        let answered = failures.answered_for_test(input("a"), 0);
        let other = failures.answered_for_test(input("b"), 0);
        registry.answer_for_test(&target, input("a"), Answer::Unusable, true);
        assert!(answered.map.revoked());
        assert!(!other.map.revoked(), "another input's");
        // Later inputs held one after another evict the failed answer.
        for text in ["c", "d", "e", "f", "g"] {
            registry.answer_for_test(&target, input(text), Answer::AsIs, true);
        }
        let identity = input_identity(input("a"));
        assert!(
            registry
                .regions
                .iter()
                .all(|region| region.slots.iter().all(|slot| slot.cell.input != identity)),
            "evicted"
        );
        assert!(answered.map.revoked());
        // An answer taken after the failure is not revoked by it; a later
        // failure of its input revokes it, though the first is gone.
        let later =
            failures.answered_for_test(input("a"), super::super::protocol::next_presentation());
        assert!(!later.map.revoked());
        registry.answer_for_test(&target, input("a"), Answer::Unusable, true);
        assert!(later.map.revoked());
    }

    const REGION: &str = "01J0000000000000000000000A";

    /// Candidates named and launched as given, in this order, under `pool`'s
    /// current epoch.
    fn target_of(pool: &LanguageServerPool, candidates: &[(&str, &[&str])]) -> PrepareTarget {
        PrepareTarget {
            candidates: candidates
                .iter()
                .map(|(name, command)| ResolvedServerConfig {
                    server_name: name.to_string(),
                    config: Arc::new(BridgeServerConfig {
                        cmd: Some(command.iter().map(|part| part.to_string()).collect()),
                        ..Default::default()
                    }),
                })
                .collect(),
            epoch: pool.prepare_epoch(),
        }
    }

    fn unstartable_target(pool: &LanguageServerPool) -> PrepareTarget {
        target_of(pool, &[("peer", &["/nonexistent/kakehashi-prepare-peer"])])
    }

    fn input<'a>(host: &'a Url, text: &'a str) -> PrepareInput<'a> {
        PrepareInput {
            host_uri: host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: REGION,
            virtual_text: text,
            gaps: &[],
        }
    }

    fn cell(
        registry: &PrepareRegistry,
        pool: &LanguageServerPool,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        holder: Holder,
    ) -> Arc<Cell> {
        registry
            .slot(target, input, holder, pool.prepare_epoch())
            .unwrap()
            .cell
    }

    fn held_cell(registry: &PrepareRegistry) -> Arc<Cell> {
        let region = registry.regions.iter().next().unwrap();
        Arc::clone(region.held_cell().unwrap())
    }

    #[test]
    fn a_slot_returned_to_a_lookup_is_not_evicted_before_it_starts() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::preparing();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        // Looked up, its attempt not yet started.
        let first = registry
            .slot(&target, input(&host, "a"), Holder::Request, target.epoch)
            .unwrap();
        // Lookups for newer texts push it past the bound.
        for text in ["b", "c", "d", "e", "f"] {
            registry.slot(&target, input(&host, text), Holder::Request, target.epoch);
        }
        let region = registry.regions.iter().next().unwrap();
        assert!(
            region.has(&first.cell),
            "a slot still in a lookup's hands keeps its place"
        );
        drop(region);
        drop(first);
        // Once released, it is the least recently used, and goes.
        registry.slot(&target, input(&host, "g"), Holder::Request, target.epoch);
        let region = registry.regions.iter().next().unwrap();
        assert_eq!(region.slots.len(), SLOTS_PER_REGION + 1);
    }

    #[test]
    fn an_input_still_in_use_is_not_evicted() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::preparing();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        let running = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        // `running` stands for a waiter, or a running attempt's end guard.
        // Looked up and released: idle.
        cell(
            &registry,
            &pool,
            &target,
            input(&host, "b"),
            Holder::Request,
        );
        // A burst of other inputs past the bound.
        for text in ["c", "d", "e", "f", "g"] {
            cell(
                &registry,
                &pool,
                &target,
                input(&host, text),
                Holder::Request,
            );
        }
        let again = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        assert!(Arc::ptr_eq(&running, &again), "no second attempt for it");
        let region = registry.regions.iter().next().unwrap();
        let idle = input_key(&target, input(&host, "b"));
        assert!(
            region.slots.iter().all(|slot| slot.key != idle),
            "an idle one is still evicted"
        );
    }

    #[tokio::test]
    async fn an_input_no_candidate_prepares_is_sent_as_is() {
        // A candidate that cannot start prepares nothing: with no other, the
        // document goes out unprepared, and that is final for the input.
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target(&pool);
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert!(matches!(
            registry.prepare(&pool, &target, input(&host, "a")).await,
            Ok(Answer::AsIs)
        ));
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Answered(Answer::AsIs)
        ));
        let cell = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        assert!(!cell.in_flight.load(Ordering::Acquire));
        assert_eq!(*cell.attempts.borrow(), 1, "the input was not retried");
        // Not started again for each of this host's inputs to find that out,
        // and passed over for this host only: another host's workspace root
        // may be one it starts in.
        assert!(pool.prepare_start_failed("peer", &host));
        let other = Url::parse("file:///elsewhere/host.md").unwrap();
        assert!(!pool.prepare_start_failed("peer", &other));
        assert_eq!(pool.prepare_advertisement("peer"), None);
        // The document the lifecycle pass held back became ready to send.
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host,
                cause: ResyncCause::Ready { edited: false },
            })
        );
    }

    #[tokio::test]
    async fn an_answer_for_an_edit_resyncs_as_edited() {
        // The lifecycle pass moves from one text to the edited one under the
        // same epoch: that answer's resync is an edit's, which the edit's
        // debounced collection covers. The first answer's is not.
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target(&pool);
        let ready = |edited| Resync {
            host_uri: host.clone(),
            cause: ResyncCause::Ready { edited },
        };
        registry.lookup_or_start(&pool, &target, input(&host, "a"));
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .is_ok()
        );
        assert_eq!(resync.try_recv().ok(), Some(ready(false)));
        // Each answer that re-syncs the host is counted for it.
        assert_eq!(registry.host_answers(&host), 1);
        registry.lookup_or_start(&pool, &target, input(&host, "b"));
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "b"))
                .await
                .is_ok()
        );
        assert_eq!(resync.try_recv().ok(), Some(ready(true)));
        // Typed on before an answer: moving off the still-pending input is
        // an edit too.
        registry.lookup_or_start(&pool, &target, input(&host, "x"));
        registry.lookup_or_start(&pool, &target, input(&host, "y"));
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "y"))
                .await
                .is_ok()
        );
        assert_eq!(resync.try_recv().ok(), Some(ready(true)));
        // Under a new epoch the held answer is for other settings: not an
        // edit's.
        pool.advance_prepare_epoch();
        let target = unstartable_target(&pool);
        registry.lookup_or_start(&pool, &target, input(&host, "c"));
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "c"))
                .await
                .is_ok()
        );
        assert_eq!(resync.try_recv().ok(), Some(ready(false)));
    }

    #[tokio::test]
    async fn the_held_input_resyncs_whoever_started_its_attempt() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target(&pool);
        // A request sees the edited text first and starts the attempt; the
        // lifecycle pass shares it and holds the document on it.
        let started = cell(
            &registry,
            &pool,
            &target,
            input(&host, "v2"),
            Holder::Request,
        );
        let mut attempts = started.attempts.subscribe();
        let request = registry.prepare(&pool, &target, input(&host, "v2"));
        let lifecycle = async {
            registry.lookup_or_start(&pool, &target, input(&host, "v2"));
        };
        let (answer, ()) = tokio::join!(request, lifecycle);
        assert!(matches!(answer, Ok(Answer::AsIs)));
        assert!(Arc::ptr_eq(&held_cell(&registry), &started));
        let _ = attempts.changed().await;
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host.clone(),
                cause: ResyncCause::Ready { edited: false },
            })
        );
        // An answer for text the lifecycle pass no longer holds on re-syncs
        // nothing, nor does a request move what it holds on.
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "v1"))
                .await
                .is_ok()
        );
        assert!(resync.try_recv().is_err());
        assert!(Arc::ptr_eq(&held_cell(&registry), &started));
    }

    #[tokio::test]
    async fn a_target_from_before_the_epoch_moved_asks_nothing() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target(&pool);
        // Settings changed, say: the candidates resolved before may name a
        // peer the settings no longer do.
        pool.advance_prepare_epoch();
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
        assert!(!pool.prepare_start_failed("peer", &host), "never started");
        // A lifecycle pass under it syncs its host again, under the current
        // epoch.
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host,
                cause: ResyncCause::Retry,
            })
        );
    }

    #[tokio::test]
    async fn an_answer_stands_only_under_the_epoch_it_was_asked_in() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target(&pool);
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .is_ok()
        );
        registry.lookup_or_start(&pool, &target, input(&host, "a"));
        assert_eq!(
            registry.region(&host, "lua", REGION, pool.prepare_epoch()),
            RegionPrepare::default(),
            "answered as is"
        );
        pool.advance_prepare_epoch();
        // Read under the new epoch, the held answer is no answer: the region
        // waits for one, as the lifecycle pass will ask again.
        assert_eq!(
            registry.region(&host, "lua", REGION, pool.prepare_epoch()),
            RegionPrepare {
                prepared: true,
                failed: false
            }
        );
        assert!(registry.host_has_pending(&host, pool.prepare_epoch()));
        let renewed = unstartable_target(&pool);
        assert!(matches!(
            registry.lookup_or_start(&pool, &renewed, input(&host, "a")),
            PrepareLookup::Pending
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_forgotten_attempt_does_not_start_its_peer() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        let started = dir.path().join("started");
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(
            &pool,
            &[(
                "peer",
                &["sh", "-c", &format!("touch {}; cat", started.display())],
            )],
        );
        // The host closes while the attempt is being set up.
        let slot = registry
            .slot(
                &target,
                input(&host, "a"),
                Holder::LifecyclePass,
                target.epoch,
            )
            .unwrap();
        registry.forget_host(&host);
        assert!(registry.start(&slot, &pool, &target, input(&host, "a")));
        let mut attempts = slot.cell.attempts.subscribe();
        let _ = attempts.changed().await;
        assert!(!started.exists(), "no peer started for a forgotten input");
    }

    /// A ready connection for `name` that records what it is sent and
    /// never answers, advertising the prepare request or not.
    #[cfg(unix)]
    async fn recording_candidate(
        pool: &LanguageServerPool,
        name: &str,
        advertised: bool,
        path: &std::path::Path,
    ) {
        use crate::lsp::bridge::pool::ConnectionState;
        use crate::lsp::bridge::pool::test_helpers::{
            advertise_prepare_for_test, create_handle_with_command,
        };
        let (handle, _) = create_handle_with_command(
            ConnectionState::Ready,
            ConnectionKey::for_server(name),
            vec![
                "sh".into(),
                "-c".into(),
                "cat > \"$1\"".into(),
                "record".into(),
                path.to_str().unwrap().into(),
            ],
            None,
        )
        .await;
        advertise_prepare_for_test(&handle, advertised);
        pool.insert_connection(handle).await;
    }

    /// A candidate advertising the request that answers it (its first
    /// request, id 2) with error `code` `delay` seconds after it arrived,
    /// recording what it was sent at `path`.
    #[cfg(unix)]
    async fn refusing_candidate(
        pool: &LanguageServerPool,
        name: &str,
        path: &std::path::Path,
        delay: f64,
        code: i64,
    ) {
        let refusal =
            format!(r#"{{"jsonrpc":"2.0","id":2,"error":{{"code":{code},"message":"no"}}}}"#);
        answering_candidate(pool, name, path, delay, &refusal).await;
    }

    /// A candidate advertising the request that answers it (its first
    /// request, id 2) with the raw `refusal` response `delay` seconds after
    /// it arrived, recording what it was sent at `path`.
    #[cfg(unix)]
    async fn answering_candidate(
        pool: &LanguageServerPool,
        name: &str,
        path: &std::path::Path,
        delay: f64,
        refusal: &str,
    ) {
        use crate::lsp::bridge::pool::ConnectionState;
        use crate::lsp::bridge::pool::test_helpers::{
            advertise_prepare_for_test, create_handle_with_command,
        };
        // A negative delay: answer once `<path>.release` exists.
        let pause = if delay < 0.0 {
            "while [ ! -e \"$1.release\" ]; do sleep 0.01; done".to_string()
        } else {
            format!("sleep {delay}")
        };
        let script = format!(
            // An asynchronous list reads /dev/null unless given stdin.
            "exec 3<&0; (cat <&3 > \"$1\") & \
             while ! grep -q virtualDocument \"$1\" 2>/dev/null; do sleep 0.05; done; \
             {pause}; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{refusal}'; wait",
            refusal.len()
        );
        let (handle, _) = create_handle_with_command(
            ConnectionState::Ready,
            ConnectionKey::for_server(name),
            vec![
                "sh".into(),
                "-c".into(),
                script,
                "refuse".into(),
                path.to_str().unwrap().into(),
            ],
            None,
        )
        .await;
        advertise_prepare_for_test(&handle, true);
        pool.insert_connection(handle).await;
    }

    /// A map from a peer's answer names the answer it came from: its input's
    /// stamp and identity, and shares what revokes it with every answer for
    /// that input of its region, which a failure of the input marks.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_answers_map_names_the_input_it_answers() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        answering_candidate(
            &pool,
            "peer",
            &dir.path().join("peer"),
            0.0,
            r#"{"jsonrpc":"2.0","id":2,"result":null}"#,
        )
        .await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        let Ok(Answer::Prepared(prepared)) =
            registry.prepare(&pool, &target, input(&host, "a")).await
        else {
            panic!("prepared");
        };
        let cell = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        assert_eq!(
            prepared.map.origin(),
            super::super::protocol::AnswerOrigin {
                taken: cell.taken,
                input: input_identity(input(&host, "a")),
            }
        );
        assert_ne!(
            input_identity(input(&host, "a")),
            input_identity(input(&host, "b"))
        );
        let shared = registry.failures().answered_for_test(input(&host, "a"), 0);
        assert!(!prepared.map.revoked());
        registry.revocations.revoke(
            region_hash(host.as_str(), "lua", REGION),
            input_identity(input(&host, "a")),
            u64::MAX,
        );
        assert!(prepared.map.revoked());
        assert!(shared.map.revoked());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_first_advertising_candidate_alone_is_asked_and_retried() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        let sent = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap_or_default();
        recording_candidate(&pool, "quiet", false, &dir.path().join("quiet")).await;
        recording_candidate(&pool, "first", true, &dir.path().join("first")).await;
        recording_candidate(&pool, "second", true, &dir.path().join("second")).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(
            &pool,
            &[
                ("quiet", &["quiet"]),
                ("first", &["first"]),
                ("second", &["second"]),
            ],
        );
        // The lifecycle pass holds the document on this input.
        registry.slot(
            &target,
            input(&host, "a"),
            Holder::LifecyclePass,
            target.epoch,
        );
        // `first` never answers: no answer, not cached, so the document is
        // still pending, and `second` is not asked instead.
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
        let method = crate::lsp::bridge::protocol::PREPARE_METHOD;
        assert!(!sent("quiet").contains(method));
        assert!(sent("first").contains(method));
        assert!(!sent("second").contains(method));
        // Timed out, the request is cancelled: queued as the attempt ends,
        // written by the candidate when its turn comes.
        let cancel = r#""method":"$/cancelRequest","params":{"id":2}"#;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !sent("first").contains(cancel) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sent("first").contains(cancel), "{}", sent("first"));
        let cell = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        assert!(cell.outcome.get().is_none());
        assert_eq!(cell.misses.load(Ordering::Acquire), 1);
        // …but backing off: neither a lookup nor a request starts another
        // attempt yet.
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
        assert_eq!(*cell.attempts.borrow(), 1);
        // The host is synced again once the backoff ends.
        let resynced = tokio::time::timeout(Duration::from_secs(5), resync.recv())
            .await
            .expect("a re-sync after the backoff");
        assert_eq!(
            resynced,
            Some(Resync {
                host_uri: host.clone(),
                cause: ResyncCause::Retry,
            })
        );
    }

    /// An input backing off after a miss is not evicted while it backs off,
    /// even once the lifecycle pass moved off it through a burst past the
    /// bound: its retry timer holds the cell for the whole backoff, so a
    /// lookup of it (an undo) finds its misses, not a fresh cell asking at
    /// once. Bounded so: one timer per miss, each ending with its backoff.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_backing_off_input_is_not_evicted_while_it_backs_off() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        // Cancelled with a code LSP lets a client retry: a miss at once.
        refusing_candidate(&pool, "peer", &dir.path().join("peer"), 0.0, -32800).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        registry.slot(
            &target,
            input(&host, "a"),
            Holder::LifecyclePass,
            target.epoch,
        );
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
        // Typed on: held on other inputs, past the bound.
        for text in ["b", "c", "d", "e", "f"] {
            registry.slot(
                &target,
                input(&host, text),
                Holder::LifecyclePass,
                target.epoch,
            );
        }
        let again = registry
            .slot(&target, input(&host, "a"), Holder::Request, target.epoch)
            .unwrap();
        assert!(again.cell.backing_off());
        assert_eq!(again.cell.misses.load(Ordering::Acquire), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unusable_answer_resyncs_its_host_as_failed() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        refusing_candidate(&pool, "peer", &dir.path().join("peer"), 0.0, -32603).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        registry.slot(
            &target,
            input(&host, "a"),
            Holder::LifecyclePass,
            target.epoch,
        );
        // An answer for the input taken before (under an earlier epoch, say).
        let earlier = registry.failures().answered_for_test(input(&host, "a"), 0);
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Failed)
        );
        assert!(earlier.map.revoked(), "revoked by the failure");
        assert_eq!(
            registry.region(&host, "lua", REGION, pool.prepare_epoch()),
            RegionPrepare {
                prepared: true,
                failed: true
            }
        );
        // Diagnostics placed from the region's earlier text are collected
        // away.
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host.clone(),
                cause: ResyncCause::Recollect { edited: false },
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_edits_unusable_answer_resyncs_as_edited() {
        // An unusable answer for an edited text is an edit's like any other:
        // its collection waits for the edit's debounce.
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        refusing_candidate(&pool, "peer", &dir.path().join("peer"), 0.0, -32603).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        for text in ["before the edit", "a"] {
            registry.slot(
                &target,
                input(&host, text),
                Holder::LifecyclePass,
                target.epoch,
            );
        }
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Failed)
        );
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host,
                cause: ResyncCause::Recollect { edited: true },
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_withdrawn_advertisement_outdates_the_answer_and_passes_its_server_over() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let sent = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
        // `first` answers once released, after the request arrived and its
        // advertisement was withdrawn meanwhile.
        refusing_candidate(&pool, "first", &first, -1.0, -32603).await;
        pool.record_prepare_advertisement(&ConnectionKey::for_server("first"), true);
        // A retryable cancellation: no answer, at once.
        refusing_candidate(&pool, "second", &second, 0.0, -32800).await;
        let host = Url::parse("file:///host.md").unwrap();
        let candidates: &[(&str, &[&str])] = &[("first", &["first"]), ("second", &["second"])];
        let target = target_of(&pool, candidates);
        registry.slot(
            &target,
            input(&host, "a"),
            Holder::LifecyclePass,
            target.epoch,
        );
        let method = crate::lsp::bridge::protocol::PREPARE_METHOD;
        let withdraw = async {
            while !sent(&first).contains(method) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            pool.record_prepare_advertisement(&ConnectionKey::for_server("first"), false);
            std::fs::write(first.with_extension("release"), "").unwrap();
        };
        let (miss, ()) = tokio::join!(
            registry.prepare(&pool, &target, input(&host, "a")),
            withdraw
        );
        // `first`'s refusal is no answer under the new epoch.
        assert_eq!(miss.err(), Some(PrepareMiss::Unanswered));
        // Asked again under it, `second` is asked (and, never answering,
        // leaves the input unanswered), with a version of its own.
        let renewed = target_of(&pool, candidates);
        assert_eq!(
            registry
                .prepare(&pool, &renewed, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
        assert!(sent(&second).contains(method));
        let versions = cell(
            &registry,
            &pool,
            &renewed,
            input(&host, "a"),
            Holder::Request,
        )
        .versions
        .lock()
        .unwrap()
        .clone();
        assert_eq!(versions.len(), 1, "{versions:?}");
        assert!(versions[0] > 1, "a newer version than the first request's");
    }

    #[test]
    fn another_launch_config_never_shares_an_answer() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::preparing();
        let host = Url::parse("file:///host.md").unwrap();
        let current = target_of(&pool, &[("peer", &["peer", "--new"])]);
        // The same name and epoch, resolved from other settings.
        let other = target_of(&pool, &[("peer", &["peer", "--old"])]);
        let a = cell(
            &registry,
            &pool,
            &current,
            input(&host, "a"),
            Holder::Request,
        );
        let b = cell(&registry, &pool, &other, input(&host, "a"), Holder::Request);
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_lookups_share_one_attempt() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peer");
        refusing_candidate(&pool, "peer", &path, 0.5, -32603).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        let (a, b, ()) = tokio::join!(
            registry.prepare(&pool, &target, input(&host, "a")),
            registry.prepare(&pool, &target, input(&host, "a")),
            async {
                registry.lookup_or_start(&pool, &target, input(&host, "a"));
            }
        );
        assert_eq!(a.err(), Some(PrepareMiss::Failed));
        assert_eq!(b.err(), Some(PrepareMiss::Failed));
        let sent = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            sent.matches(crate::lsp::bridge::protocol::PREPARE_METHOD)
                .count(),
            1,
            "{sent}"
        );
        let cell = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        assert_eq!(cell.versions.lock().unwrap().len(), 1);
        assert_eq!(*cell.attempts.borrow(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_method_not_found_answer_withdraws_the_advertisement() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        refusing_candidate(&pool, "peer", &dir.path().join("peer"), 0.0, -32601).await;
        pool.record_prepare_advertisement(&ConnectionKey::for_server("peer"), true);
        let epoch = pool.prepare_epoch();
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
        assert_eq!(pool.prepare_advertisement("peer"), Some(false));
        assert!(pool.prepare_epoch() > epoch);
        // Asked again under the new epoch, it is passed over: sent as is.
        let renewed = target_of(&pool, &[("peer", &["peer"])]);
        assert!(matches!(
            registry.prepare(&pool, &renewed, input(&host, "a")).await,
            Ok(Answer::AsIs)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_attempt_forgotten_while_its_candidate_starts_asks_nothing() {
        use crate::lsp::bridge::pool::ConnectionState;
        use crate::lsp::bridge::pool::test_helpers::{
            advertise_prepare_for_test, create_handle_with_command, mark_ready_for_test,
        };
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peer");
        // Starting: the attempt is admitted to acquire it, then waits for it.
        let (handle, _) = create_handle_with_command(
            ConnectionState::Initializing,
            ConnectionKey::for_server("peer"),
            vec![
                "sh".into(),
                "-c".into(),
                "cat > \"$1\"".into(),
                "record".into(),
                path.to_str().unwrap().into(),
            ],
            None,
        )
        .await;
        advertise_prepare_for_test(&handle, true);
        pool.insert_connection(Arc::clone(&handle)).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&pool, &[("peer", &["peer"])]);
        let slot = registry
            .slot(
                &target,
                input(&host, "a"),
                Holder::LifecyclePass,
                target.epoch,
            )
            .unwrap();
        let mut attempts = slot.cell.attempts.subscribe();
        let subscribers = handle.state_subscribers_for_test();
        assert!(registry.start(&slot, &pool, &target, input(&host, "a")));
        // Admitted, the attempt waits for the candidate to be ready.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while handle.state_subscribers_for_test() == subscribers {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the attempt never waited"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // The host closes while the candidate starts.
        registry.forget_host(&host);
        assert!(mark_ready_for_test(&handle));
        let _ = attempts.changed().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let sent = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !sent.contains(crate::lsp::bridge::protocol::PREPARE_METHOD),
            "{sent}"
        );
    }

    /// A closed host gets nothing looked up for it by whatever still runs
    /// past the close — no region comes back, no attempt starts — until it
    /// opens again.
    #[tokio::test]
    async fn a_closed_host_is_looked_up_again_only_once_reopened() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///closed.md").unwrap();
        registry.slot(
            &target,
            input(&host, "a"),
            Holder::LifecyclePass,
            target.epoch,
        );
        registry.forget_host(&host);
        assert!(registry.host_closed(&host));

        assert!(matches!(
            registry.lookup_for_request(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert!(
            registry.regions.is_empty(),
            "a request re-created the region"
        );

        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert!(registry.regions.is_empty(), "a pass re-created the region");

        // Reopened.
        registry.host_opened(&host);
        assert!(!registry.host_closed(&host));
        registry.lookup_or_start(&pool, &target, input(&host, "a"));
        assert_eq!(registry.regions.len(), 1);

        // A host closed before anything was looked up is remembered too: a
        // request resolved before the close may look up after it.
        let other = Url::parse("file:///untouched.md").unwrap();
        registry.forget_host(&other);
        assert!(registry.host_closed(&other));
        assert!(matches!(
            registry.lookup_for_request(&pool, &target, input(&other, "a")),
            PrepareLookup::Pending
        ));
        assert!(
            !registry
                .regions
                .iter()
                .any(|region| region.host_uri == other.as_str())
        );
    }

    /// Closed hosts are remembered up to a bound, the oldest close going
    /// first.
    #[test]
    fn the_oldest_closed_host_goes_past_the_bound() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::preparing());
        let target = unstartable_target(&pool);
        let closed = |index: usize| {
            let host = Url::parse(&format!("file:///closed-{index}.md")).unwrap();
            registry.slot(
                &target,
                input(&host, "a"),
                Holder::LifecyclePass,
                target.epoch,
            );
            registry.forget_host(&host);
            host
        };
        let hosts: Vec<Url> = (0..=CLOSED_HOSTS_KEPT).map(closed).collect();
        assert_eq!(registry.closed_hosts.hosts.len(), CLOSED_HOSTS_KEPT);
        assert!(!registry.host_closed(&hosts[0]));
        assert!(registry.host_closed(&hosts[1]));
        assert!(registry.host_closed(hosts.last().unwrap()));
    }

    #[test]
    fn retries_back_off_to_a_minute() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(6), Duration::from_secs(32));
        assert_eq!(retry_delay(7), Duration::from_secs(60));
        assert_eq!(retry_delay(100), Duration::from_secs(60));
    }

    /// A withheld mark outlives every forget of its region's answers, as
    /// the lifecycle pass's hold does until text is recorded, and reads as
    /// no answer: only a host's close, or a release, takes it.
    #[test]
    fn a_withheld_mark_outlives_forgetting_its_answers() {
        let registry = PrepareRegistry::default();
        let failures = registry.failures();
        let pool = LanguageServerPool::preparing();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        let withheld = || {
            failures
                .admission(&host, "lua", REGION, target.epoch)
                .withheld
        };
        let forgets: [&dyn Fn(); 4] = [
            &|| registry.forget_region(&host, Some("lua"), REGION),
            &|| registry.forget_region(&host, None, REGION),
            &|| registry.forget_regions(&host, &[REGION.to_string()]),
            &|| registry.forget_region_except(&host, REGION, None),
        ];
        for forget in forgets {
            registry.slot(
                &target,
                input(&host, "a"),
                Holder::LifecyclePass,
                target.epoch,
            );
            failures.withhold(&host, "lua", REGION, 7);
            forget();
            assert_eq!(withheld(), Some(7));
            assert_eq!(
                registry.region(&host, "lua", REGION, target.epoch),
                RegionPrepare::default(),
                "its answers are gone"
            );
            assert!(!registry.any_region_prepared(target.epoch));
            assert!(!registry.host_has_pending(&host, target.epoch));
        }
        // A release under another incarnation keeps it; its own takes it,
        // and the region with it.
        failures.release(&host, "lua", REGION, 6);
        assert_eq!(withheld(), Some(7));
        failures.release(&host, "lua", REGION, 7);
        assert_eq!(withheld(), None);
        assert!(registry.regions.is_empty());
        // A close takes it.
        failures.withhold(&host, "lua", REGION, 7);
        registry.forget_host(&host);
        assert_eq!(withheld(), None);
        assert!(registry.regions.is_empty());
        // Withholding looks nothing up.
        let fresh = PrepareRegistry::default();
        fresh.failures().withhold(&host, "lua", REGION, 7);
        assert!(fresh.unused());
    }

    #[test]
    fn regions_are_forgotten_by_language_and_host() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::preparing();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        let other = Url::parse("file:///other.md").unwrap();
        let mut ruby = input(&host, "a");
        ruby.injection_language = "ruby";
        for input in [input(&host, "a"), ruby, input(&other, "a")] {
            registry.slot(&target, input, Holder::LifecyclePass, target.epoch);
        }
        // Re-routed to ruby: only the lua document goes.
        registry.forget_region_except(&host, REGION, Some("ruby"));
        assert_eq!(registry.regions.len(), 2);
        registry.forget_host(&other);
        assert_eq!(registry.regions.len(), 1);
        registry.forget_region(&host, None, REGION);
        assert!(registry.regions.is_empty());
    }

    /// Forgetting what is not there changes nothing held: the cached scan
    /// of prepared regions stands (a pass with no peer forgets every region
    /// it sends as is, on every keystroke).
    #[test]
    fn forgetting_nothing_keeps_the_prepared_scan() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::preparing();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        registry.slot(
            &target,
            input(&host, "a"),
            Holder::LifecyclePass,
            target.epoch,
        );
        let changes = || registry.held_changes.load(Ordering::Acquire);
        let before = changes();
        registry.forget_region(&host, Some("lua"), "01J0000000000000000000000Z");
        registry.forget_region(&host, None, "01J0000000000000000000000Z");
        registry.forget_region_except(&host, "01J0000000000000000000000Z", None);
        registry.forget_regions(&host, &["01J0000000000000000000000Z".to_string()]);
        assert_eq!(changes(), before);
        registry.forget_region(&host, Some("lua"), REGION);
        assert!(changes() > before);
    }

    #[test]
    fn invalidated_regions_are_forgotten_together_under_every_language() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::preparing();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        let other = Url::parse("file:///other.md").unwrap();
        let region = |host, region_id, language| {
            let mut input = input(host, "a");
            input.region_id = region_id;
            input.injection_language = language;
            registry.slot(&target, input, Holder::LifecyclePass, target.epoch);
        };
        region(&host, "01J0000000000000000000000A", "lua");
        region(&host, "01J0000000000000000000000A", "ruby");
        region(&host, "01J0000000000000000000000B", "lua");
        region(&host, "01J0000000000000000000000C", "lua");
        region(&other, "01J0000000000000000000000A", "lua");
        registry.forget_regions(
            &host,
            &[
                "01J0000000000000000000000A".to_string(),
                "01J0000000000000000000000B".to_string(),
            ],
        );
        let mut left: Vec<_> = registry
            .regions
            .iter()
            .map(|region| (region.host_uri.clone(), region.region_id.clone()))
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                (
                    "file:///host.md".to_string(),
                    "01J0000000000000000000000C".to_string()
                ),
                (
                    "file:///other.md".to_string(),
                    "01J0000000000000000000000A".to_string()
                ),
            ]
        );
    }
}
