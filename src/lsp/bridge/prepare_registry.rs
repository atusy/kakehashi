//! Answers to `kakehashi/virtualDocument/prepare`: cache, single flight and
//! retries.
//!
//! Every path that hands a virtual document to downstream servers — the
//! didOpen/didChange pass, requests, the one-shot CLI — looks its prepared
//! form up here by the exact input it was built from (virtual text, gaps,
//! host language and the candidate servers' launch configs), so one input
//! gets one answer, shared by all of them.
//!
//! This registry only answers. What each server holds, and so which map its
//! coordinates follow, is recorded by the paths that send, with the text
//! (see the document tracker's `HeldText`).
//!
//! The lifecycle pass must not wait for the peer under the document's edit
//! lock, so it only looks up: on a miss it starts the request and holds the
//! document back, and the finished request asks for the host to be synced
//! again ([`Resync`]). Requests wait for the answer instead.
//!
//! Every attempt runs on its own task, so a cancelled request cannot strand
//! one half-done, and all waiters share it. An attempt that gets no answer
//! from the peer it chose (it crashes, times out, or cancels) is retried
//! with backoff by syncing the host again; one that gets an unusable answer
//! is final for that input, as is one that finds no candidate to ask (the
//! document is then sent as is). A candidate that cannot start is passed
//! over, as one that does not advertise the request.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use url::Url;

use super::ResolvedServerConfig;
use super::pool::LanguageServerPool;
use super::protocol::{
    PrepareHostTextDocument, PrepareParams, PrepareTextDocument, PreparedDocument,
    VirtualDocumentUri, VirtualLayout, apply_prepare_result,
};
use crate::language::injection::VirtualGap;

/// Answers kept per region besides the one the lifecycle pass holds its
/// document on: enough for an undo, or a request on text an edit or two
/// old, to find its answer without asking again.
const SLOTS_PER_REGION: usize = 3;

/// The servers that may prepare one (host, injection) pair's virtual
/// documents, in priority order: the first of them advertising the request
/// prepares each input (the `preferred` strategy).
#[derive(Debug, Clone)]
pub(crate) struct PrepareTarget {
    pub(crate) candidates: Arc<[ResolvedServerConfig]>,
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

/// What a lookup found without waiting.
#[derive(Debug, Clone)]
pub(crate) enum PrepareLookup {
    /// The document to send: prepared, or (without a map) as is, when no
    /// candidate prepares it.
    Ready(Arc<PreparedDocument>),
    /// The peer could not prepare this input: send nothing.
    Failed,
    /// No answer yet: send nothing now; the host is synced again when it
    /// arrives.
    Pending,
}

/// Why waiting gave no document to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrepareMiss {
    /// No answer is in (yet): the attempt got none, or a retry is backing
    /// off; a later one may bring it.
    Unanswered,
    /// The peer's answer was unusable, final for this input.
    Failed,
}

/// `None` is an answer the peer gave unusably.
type Outcome = Option<Arc<PreparedDocument>>;

/// A host to sync again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resync {
    pub(crate) host_uri: Url,
    /// A document held back became ready to send (rather than a retry
    /// coming due, or an answer coming back unusable), so the host's
    /// diagnostics are collected again too.
    pub(crate) ready: bool,
}

/// The answer for one input, and the attempts to get it.
#[derive(Default)]
struct Cell {
    /// Set once an attempt got an answer.
    outcome: std::sync::OnceLock<Outcome>,
    /// An attempt is running.
    in_flight: AtomicBool,
    /// Attempts that got no answer, for the backoff.
    misses: AtomicU32,
    /// No new attempt before this, after a miss.
    retry_at: std::sync::Mutex<Option<Instant>>,
    /// Bumped after every attempt, answered or not.
    attempts: tokio::sync::watch::Sender<u32>,
    /// The `textDocument.version` of each attempt, in order.
    #[cfg(test)]
    versions: std::sync::Mutex<Vec<i32>>,
}

impl Cell {
    fn lookup(&self) -> Option<PrepareLookup> {
        self.outcome.get().map(|outcome| match outcome {
            Some(prepared) => PrepareLookup::Ready(Arc::clone(prepared)),
            None => PrepareLookup::Failed,
        })
    }

    fn settled(&self) -> Result<Arc<PreparedDocument>, PrepareMiss> {
        match self.outcome.get() {
            Some(Some(prepared)) => Ok(Arc::clone(prepared)),
            Some(None) => Err(PrepareMiss::Failed),
            None => Err(PrepareMiss::Unanswered),
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
    /// The host language and candidates the lifecycle pass last looked the
    /// region up under. Answers under other ones are not kept: the same
    /// servers launched differently answer differently.
    host_language: String,
    candidates: Arc<[ResolvedServerConfig]>,
    /// Most recently used first.
    slots: Vec<Slot>,
    /// The input the lifecycle pass holds the document on: only its answer
    /// re-syncs the host, whoever started the attempt.
    held: Option<u64>,
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

    fn held_slot(&self) -> Option<&Slot> {
        let held = self.held?;
        self.slots.iter().find(|slot| slot.key == held)
    }
}

type Regions = DashMap<u64, Region>;

/// Who looks a document up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// The lifecycle pass, which holds the document until the answer
    /// re-syncs it.
    LifecyclePass,
    /// A request (or snapshot), which waits for the answer itself, or does
    /// without: it never displaces what the lifecycle pass holds on.
    Request,
}

pub(crate) struct PrepareRegistry {
    /// Keyed by a hash of (host URI, injection language, region id), so a
    /// lookup allocates nothing; the entry holds the identity it checks.
    regions: Arc<Regions>,
    /// Some region was ever looked up: until then every query answers "no
    /// region" without touching the map, which the paths every bridged
    /// region takes per edit rely on when nothing is prepared.
    used: AtomicBool,
    /// Source of `textDocument.version`: shared by all documents, so a
    /// document forgotten and prepared again (a settings change, a reopen)
    /// never repeats a version the peer saw.
    next_revision: AtomicI32,
    resync_tx: UnboundedSender<Resync>,
    resync_rx: std::sync::Mutex<Option<UnboundedReceiver<Resync>>>,
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
            used: AtomicBool::new(false),
            next_revision: AtomicI32::new(1),
            resync_tx,
            resync_rx: std::sync::Mutex::new(Some(resync_rx)),
        }
    }
}

impl PrepareRegistry {
    /// Whether no region was ever looked up (see `used`).
    pub(crate) fn unused(&self) -> bool {
        !self.used.load(Ordering::Acquire)
    }

    /// Queue `resync` again after `delay` — for a pass whose sends failed,
    /// which nothing else would repeat. A shutdown drops it.
    pub(crate) fn requeue_resync(&self, resync: Resync, delay: Duration) {
        let resync_tx = self.resync_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = resync_tx.send(resync);
        });
    }

    /// A sender of hosts to sync again, for others that find a host should be
    /// (see [`Self::take_resync_rx`]).
    pub(crate) fn resync_sender(&self) -> UnboundedSender<Resync> {
        self.resync_tx.clone()
    }

    /// Hosts to sync again: a held-back virtual document became ready, its
    /// answer came back unusable, or an attempt that got no answer finished
    /// its retry backoff. Taken once by the server loop.
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
        self.lookup_now(pool, target, input, Holder::LifecyclePass)
    }

    /// [`Self::lookup_or_start`] for a request that cannot wait (a
    /// diagnostic snapshot): it never moves what the lifecycle pass holds
    /// the document on.
    pub(crate) fn lookup_or_start_for_request(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        self.lookup_now(pool, target, input, Holder::Request)
    }

    fn lookup_now(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        holder: Holder,
    ) -> PrepareLookup {
        let Some(slot) = self.slot(target, input, holder) else {
            return PrepareLookup::Pending;
        };
        if let Some(found) = slot.cell.lookup() {
            return found;
        }
        self.start(&slot, pool, target, input);
        PrepareLookup::Pending
    }

    /// Wait for the document to send, sharing an attempt already running.
    pub(crate) async fn prepare(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> Result<Arc<PreparedDocument>, PrepareMiss> {
        let Some(slot) = self.slot(target, input, Holder::Request) else {
            return Err(PrepareMiss::Unanswered);
        };
        if slot.cell.outcome.get().is_some() {
            return slot.cell.settled();
        }
        // Subscribe before starting, so the attempt's end cannot be missed.
        let mut attempts = slot.cell.attempts.subscribe();
        if !self.start(&slot, pool, target, input) && !slot.cell.in_flight.load(Ordering::Acquire) {
            // Backing off after a miss: do not queue another attempt per
            // request, each waiting out its own timeouts.
            return slot.cell.settled();
        }
        // The attempt ends (answered or not) with a bump.
        let _ = attempts.changed().await;
        slot.cell.settled()
    }

    /// Whether any of `host_uri`'s documents the lifecycle pass holds back
    /// still waits for its answer (one is running, or a retry is due).
    pub(crate) fn host_has_pending(&self, host_uri: &Url) -> bool {
        if self.unused() {
            return false;
        }
        self.regions.iter().any(|region| {
            region.host_uri == host_uri.as_str()
                && region
                    .held_slot()
                    .is_some_and(|slot| slot.cell.outcome.get().is_none())
        })
    }

    /// Whether the peer answered the input the lifecycle pass holds a
    /// region's document on unusably: that document is not bridged, so
    /// nothing a server computed for an earlier text of it is either.
    pub(crate) fn held_failed(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
    ) -> bool {
        if self.unused() {
            return false;
        }
        let host_uri = host_uri.as_str();
        self.regions
            .get(&region_hash(host_uri, injection_language, region_id))
            .filter(|region| region.is(host_uri, injection_language, region_id))
            .and_then(|region| {
                region
                    .held_slot()
                    .map(|slot| matches!(slot.cell.outcome.get(), Some(None)))
            })
            .unwrap_or(false)
    }

    /// Whether the lifecycle pass holds a region's document on a prepared
    /// answer, or on one it still waits for (or that failed): either way,
    /// not on the virtual text as is.
    pub(crate) fn held_is_prepared(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
    ) -> bool {
        if self.unused() {
            return false;
        }
        let host_uri = host_uri.as_str();
        self.regions
            .get(&region_hash(host_uri, injection_language, region_id))
            .filter(|region| region.is(host_uri, injection_language, region_id))
            .and_then(|region| {
                region.held_slot().map(|slot| {
                    !matches!(slot.cell.outcome.get(), Some(Some(prepared)) if prepared.map.is_none())
                })
            })
            .unwrap_or(false)
    }

    /// Hold a region's document on an input whose answer is not in, as the
    /// lifecycle pass does, without asking anyone.
    #[cfg(test)]
    pub(crate) fn hold_pending_for_test(&self, input: PrepareInput<'_>) {
        let target = PrepareTarget {
            candidates: Arc::new([]),
        };
        self.slot(&target, input, Holder::LifecyclePass);
    }

    /// Hold a region's document on an input the peer answered with
    /// `outcome` (`None`: unusably), as the lifecycle pass does.
    #[cfg(test)]
    pub(crate) fn hold_answered_for_test(
        &self,
        input: PrepareInput<'_>,
        outcome: Option<Arc<PreparedDocument>>,
    ) {
        let target = PrepareTarget {
            candidates: Arc::new([]),
        };
        let slot = self
            .slot(&target, input, Holder::LifecyclePass)
            .expect("the lifecycle pass always gets a slot");
        let _ = slot.cell.outcome.set(outcome);
    }

    /// Keep only the regions `keep(host language, injection language,
    /// candidates)` accepts — after a settings change, those whose pair still
    /// lists the same servers with the same launch configs. With
    /// `drop_unusable`, answers cached as final go too: the change may have
    /// fixed what made them unusable (a peer's command, say), and the caller
    /// guarantees a pass that asks again. An attempt whose region (or slot)
    /// goes is revoked: it starts no peer.
    pub(crate) fn retain(
        &self,
        keep: impl Fn(&str, &str, &[ResolvedServerConfig]) -> bool,
        drop_unusable: bool,
    ) {
        self.regions.retain(|_, region| {
            if !keep(
                &region.host_language,
                &region.injection_language,
                &region.candidates,
            ) {
                return false;
            }
            if drop_unusable {
                region
                    .slots
                    .retain(|slot| !matches!(slot.cell.outcome.get(), Some(None)));
            }
            true
        });
    }

    /// Forget one region: it was replaced or invalidated, or its pair lost
    /// its peer. `injection_language` `None` forgets it under every language.
    pub(crate) fn forget_region(
        &self,
        host_uri: &Url,
        injection_language: Option<&str>,
        region_id: &str,
    ) {
        if self.unused() {
            return;
        }
        let host_uri = host_uri.as_str();
        match injection_language {
            Some(language) => {
                self.regions
                    .remove_if(&region_hash(host_uri, language, region_id), |_, region| {
                        region.is(host_uri, language, region_id)
                    });
            }
            None => self.regions.retain(|_, region| {
                !(region.host_uri == host_uri && region.region_id == region_id)
            }),
        }
    }

    /// Forget a replaced region under every language but the one it resolves
    /// to now (`None`: every one): a region re-routed to another language
    /// keeps its id, and its document under the new language may be being
    /// prepared already.
    pub(crate) fn forget_region_except(&self, host_uri: &Url, region_id: &str, keep: Option<&str>) {
        if self.unused() {
            return;
        }
        let host_uri = host_uri.as_str();
        self.regions.retain(|_, region| {
            !(region.host_uri == host_uri
                && region.region_id == region_id
                && Some(region.injection_language.as_str()) != keep)
        });
    }

    /// Forget a closed host's documents.
    pub(crate) fn forget_host(&self, host_uri: &Url) {
        if self.unused() {
            return;
        }
        self.regions
            .retain(|_, region| region.host_uri != host_uri.as_str());
    }

    /// The slot answering `input`, created when missing. `None` for a
    /// request under other candidates than the lifecycle pass looked the
    /// region up under (settings in flight across a change): its answer
    /// would be the old launch's, and asking would start a peer the
    /// settings no longer name.
    fn slot(
        &self,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        holder: Holder,
    ) -> Option<Slot> {
        self.used.store(true, Ordering::Release);
        let key = input_key(target, input);
        let host_uri = input.host_uri.as_str();
        let hash = region_hash(host_uri, input.injection_language, input.region_id);
        let mut region = self.regions.entry(hash).or_insert_with(|| Region {
            host_uri: host_uri.to_string(),
            injection_language: input.injection_language.to_string(),
            region_id: input.region_id.to_string(),
            host_language: input.host_language.to_string(),
            candidates: Arc::clone(&target.candidates),
            slots: Vec::new(),
            held: None,
        });
        let same_region = region.is(host_uri, input.injection_language, input.region_id);
        if !same_region || !same_candidates(&region.candidates, &target.candidates) {
            if holder == Holder::Request && same_region {
                return None;
            }
            // Another launch config (the lifecycle pass brings the current
            // settings), or a hash collision with another region: start
            // over.
            *region = Region {
                host_uri: host_uri.to_string(),
                injection_language: input.injection_language.to_string(),
                region_id: input.region_id.to_string(),
                host_language: input.host_language.to_string(),
                candidates: Arc::clone(&target.candidates),
                slots: Vec::new(),
                held: None,
            };
        }
        if holder == Holder::LifecyclePass {
            region.held = Some(key);
        }
        let slot = match region.slots.iter().position(|slot| slot.key == key) {
            Some(index) => region.slots.remove(index),
            None => Slot {
                key,
                cell: Arc::default(),
            },
        };
        region.slots.insert(0, slot.clone());
        // Evict the least recently used beyond the bound, never the one the
        // lifecycle pass holds on.
        while region.slots.len() > SLOTS_PER_REGION + 1 {
            let held = region.held;
            let Some(index) = region.slots.iter().rposition(|slot| Some(slot.key) != held) else {
                break;
            };
            region.slots.remove(index);
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
        // Every request gets its own version, in the order they are sent: a
        // retry after another input was asked (an undo while the first was
        // backing off) must not go out with an older one.
        let revision = self.revision();
        #[cfg(test)]
        cell.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(revision);
        let pool = Arc::clone(pool);
        let target = target.clone();
        let job = PrepareJob::from(input);
        let resync_tx = self.resync_tx.clone();
        tokio::spawn(async move {
            let cell = Arc::clone(&end.0);
            // A settings change or close that dropped this cell revokes the
            // attempt: it must not start (or replace) a peer the settings no
            // longer name.
            let admitted = || regions.get(&region).is_some_and(|region| region.has(&cell));
            let holds = |regions: &Regions| {
                regions
                    .get(&region)
                    .is_some_and(|region| region.has(&cell) && region.holds(key))
            };
            match run(&pool, &target, &job, revision, &admitted).await {
                Ok(outcome) => {
                    let ready = outcome.is_some();
                    let _ = cell.outcome.set(outcome);
                    // The answer the lifecycle pass holds the document on
                    // re-syncs the host, whoever asked. An unusable one
                    // does too, unready: nothing more is coming for this
                    // input, which a pull owed a refresh must learn.
                    if holds(&regions) {
                        // The receiver is gone only at shutdown.
                        let _ = resync_tx.send(Resync {
                            host_uri: job.host_uri.clone(),
                            ready,
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
                    // unless newer text has taken over by then.
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
                                ready: false,
                            });
                        }
                    });
                }
            }
            drop(end);
        });
        true
    }

    fn revision(&self) -> i32 {
        // Wraps after 2^31 prepares; the peer only compares a document's
        // versions over its lifetime.
        self.next_revision.fetch_add(1, Ordering::Relaxed)
    }
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

fn input_key(target: &PrepareTarget, input: PrepareInput<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for candidate in target.candidates.iter() {
        candidate.server_name.hash(&mut hasher);
    }
    input.host_language.hash(&mut hasher);
    input.virtual_text.hash(&mut hasher);
    input.gaps.hash(&mut hasher);
    hasher.finish()
}

/// Whether two candidate lists are the same servers with the same launch
/// configs: by identity first (a target resolved under one settings snapshot
/// shares its `Arc`s), by value across snapshots.
pub(crate) fn same_candidates(a: &[ResolvedServerConfig], b: &[ResolvedServerConfig]) -> bool {
    std::ptr::eq(a, b)
        || a.len() == b.len()
            && a.iter().zip(b).all(|(a, b)| {
                a.server_name == b.server_name
                    && (Arc::ptr_eq(&a.config, &b.config) || a.config == b.config)
            })
}

/// An owned [`PrepareInput`], for the background request.
struct PrepareJob {
    host_uri: Url,
    host_language: String,
    injection_language: String,
    region_id: String,
    virtual_text: String,
    gaps: Vec<VirtualGap>,
}

impl From<PrepareInput<'_>> for PrepareJob {
    fn from(input: PrepareInput<'_>) -> Self {
        Self {
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
/// `Ok(None)` is an answer kakehashi refused (or the peer refused to give);
/// `Err` is no answer at all. Both are logged; neither lets the document
/// through unprepared. An input no candidate advertises the request for is
/// sent as is (a document without a map).
async fn run(
    pool: &LanguageServerPool,
    target: &PrepareTarget,
    job: &PrepareJob,
    revision: i32,
    admitted: &(dyn Fn() -> bool + Sync),
) -> Result<Outcome, ()> {
    let log_failure = |error: &dyn std::fmt::Display| {
        log::warn!(
            target: "kakehashi::bridge::prepare",
            "Not sending the {} virtual document of {}: {}",
            job.injection_language,
            job.host_uri,
            error
        );
    };
    match try_run(pool, target, job, revision, admitted).await {
        Ok(Ok(prepared)) => Ok(Some(Arc::new(prepared))),
        Ok(Err(refused)) => {
            log_failure(&refused);
            Ok(None)
        }
        Err(unanswered) => {
            log_failure(&unanswered);
            Err(())
        }
    }
}

/// The outer `Err` is a failure to get an answer (worth retrying); the
/// inner one is final for the input: an answer that cannot be used (an
/// error response, a malformed or refused result).
///
/// Candidates are asked strictly in priority order: one still starting is
/// waited for, and one that cannot start, or does not advertise the request,
/// is passed over. Only the first that advertises it is asked, and its
/// answer stands — a failure does not fall through to the next, which would
/// let a flaky peer switch which preparation servers see.
async fn try_run(
    pool: &LanguageServerPool,
    target: &PrepareTarget,
    job: &PrepareJob,
    revision: i32,
    admitted: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<std::io::Result<PreparedDocument>> {
    let host_uri = match crate::lsp::lsp_impl::url_to_uri(&job.host_uri) {
        Ok(host_uri) => host_uri,
        Err(error) => return Ok(Err(std::io::Error::other(error.to_string()))),
    };
    let virtual_uri =
        VirtualDocumentUri::new(&host_uri, &job.injection_language, &job.region_id).to_uri_string();
    let layout = VirtualLayout::from_gaps(&job.virtual_text, &job.gaps);
    let params = PrepareParams::new(
        PrepareTextDocument {
            uri: &virtual_uri,
            language_id: &job.injection_language,
            version: revision,
        },
        PrepareHostTextDocument {
            uri: job.host_uri.as_str(),
            language_id: &job.host_language,
        },
        &layout,
    );
    for candidate in target.candidates.iter() {
        let name = &candidate.server_name;
        if pool.prepare_advertisement(name) == Some(false) {
            continue;
        }
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
            // Advertising nothing until a handshake says otherwise, so later
            // inputs are not held back to start it again.
            Err(error) => {
                log::debug!(
                    target: "kakehashi::bridge::prepare",
                    "Prepare candidate {name} for {} did not start: {error}",
                    job.host_uri
                );
                pool.record_prepare_advertisement(name, false);
                continue;
            }
        };
        if !handle.supports_virtual_document_prepare() {
            continue;
        }
        let result = match handle.request_virtual_document_prepare(&params).await {
            Ok(result) => result,
            // It does not answer the request after all (`MethodNotFound`
            // withdraws the advertisement): as if it had not advertised it.
            Err(_) if !handle.supports_virtual_document_prepare() => {
                pool.record_prepare_advertisement(name, false);
                continue;
            }
            // Final for this input: an unusable answer (an error response or
            // a malformed result).
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Ok(Err(std::io::Error::new(
                    error.kind(),
                    format!("{name} could not prepare it: {error}"),
                )));
            }
            // No answer: timed out, cancelled by the peer, connection gone.
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{name} did not answer: {error}"),
                ));
            }
        };
        return Ok(
            apply_prepare_result(&job.virtual_text, &layout, result).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{name} answered unusably: {error}"),
                )
            }),
        );
    }
    Ok(Ok(PreparedDocument {
        text: job.virtual_text.clone(),
        map: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::BridgeServerConfig;

    const REGION: &str = "01J0000000000000000000000A";

    /// Candidates named and launched as given, in this order.
    fn target_of(candidates: &[(&str, &[&str])]) -> PrepareTarget {
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
        }
    }

    fn unstartable_target() -> PrepareTarget {
        target_of(&[("peer", &["/nonexistent/kakehashi-prepare-peer"])])
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
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        holder: Holder,
    ) -> Arc<Cell> {
        registry.slot(target, input, holder).unwrap().cell
    }

    fn held_cell(registry: &PrepareRegistry) -> Arc<Cell> {
        let region = registry.regions.iter().next().unwrap();
        Arc::clone(&region.held_slot().unwrap().cell)
    }

    #[tokio::test]
    async fn an_input_no_candidate_prepares_is_sent_as_is() {
        // A candidate that cannot start advertises nothing: with no other,
        // the document goes out unprepared, and that is final for the input.
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::new());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target();
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        let sent = registry
            .prepare(&pool, &target, input(&host, "a"))
            .await
            .unwrap();
        assert_eq!(sent.text, "a");
        assert!(sent.map.is_none());
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Ready(sent) if sent.map.is_none()
        ));
        let cell = cell(&registry, &target, input(&host, "a"), Holder::Request);
        assert!(!cell.in_flight.load(Ordering::Acquire));
        assert_eq!(*cell.attempts.borrow(), 1, "the input was not retried");
        // Not started again for each input to find that out (its next
        // handshake, when it is started to be bridged, says otherwise).
        assert_eq!(pool.prepare_advertisement("peer"), Some(false));
        // The document the lifecycle pass held back became ready to send.
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host,
                ready: true
            })
        );
    }

    #[tokio::test]
    async fn the_held_input_resyncs_whoever_started_its_attempt() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::new());
        let host = Url::parse("file:///host.md").unwrap();
        let target = unstartable_target();
        // A request sees the edited text first and starts the attempt; the
        // lifecycle pass shares it and holds the document on it.
        let started = cell(&registry, &target, input(&host, "v2"), Holder::Request);
        let mut attempts = started.attempts.subscribe();
        registry.lookup_or_start_for_request(&pool, &target, input(&host, "v2"));
        registry.lookup_or_start(&pool, &target, input(&host, "v2"));
        assert!(Arc::ptr_eq(&held_cell(&registry), &started));
        attempts.changed().await.unwrap();
        assert_eq!(
            resync.try_recv().ok(),
            Some(Resync {
                host_uri: host.clone(),
                ready: true
            })
        );
        // An answer for text the lifecycle pass no longer holds on re-syncs
        // nothing.
        let old = cell(&registry, &target, input(&host, "v1"), Holder::Request);
        let mut attempts = old.attempts.subscribe();
        registry.lookup_or_start_for_request(&pool, &target, input(&host, "v1"));
        attempts.changed().await.unwrap();
        assert!(resync.try_recv().is_err());
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
        use crate::lsp::bridge::pool::test_helpers::{
            advertise_prepare_for_test, create_handle_with_command,
        };
        use crate::lsp::bridge::pool::{ConnectionKey, ConnectionState};
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

    #[cfg(unix)]
    #[tokio::test]
    async fn the_first_advertising_candidate_alone_is_asked_and_retried() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::new());
        let dir = tempfile::tempdir().unwrap();
        let sent = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap_or_default();
        recording_candidate(&pool, "quiet", false, &dir.path().join("quiet")).await;
        recording_candidate(&pool, "first", true, &dir.path().join("first")).await;
        recording_candidate(&pool, "second", true, &dir.path().join("second")).await;
        let host = Url::parse("file:///host.md").unwrap();
        let target = target_of(&[
            ("quiet", &["quiet"]),
            ("first", &["first"]),
            ("second", &["second"]),
        ]);
        // The lifecycle pass holds the document on this input.
        registry.slot(&target, input(&host, "a"), Holder::LifecyclePass);
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
        let cell = cell(&registry, &target, input(&host, "a"), Holder::Request);
        assert!(cell.outcome.get().is_none());
        assert_eq!(cell.misses.load(Ordering::Acquire), 1);
        // …but backing off: neither a lookup nor a request starts another
        // attempt yet.
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .is_err()
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
                ready: false
            })
        );
    }

    #[test]
    fn requests_find_older_answers_without_moving_the_held_input() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let v1 = cell(
            &registry,
            &target,
            input(&host, "v1"),
            Holder::LifecyclePass,
        );
        let v2 = cell(
            &registry,
            &target,
            input(&host, "v2"),
            Holder::LifecyclePass,
        );
        // A request still on v1 shares v1's cell; the document stays held on
        // v2.
        let again = cell(&registry, &target, input(&host, "v1"), Holder::Request);
        assert!(Arc::ptr_eq(&v1, &again));
        assert!(Arc::ptr_eq(&held_cell(&registry), &v2));
        // Undo to v1: the lifecycle pass shares its attempt and holds on it.
        let undone = cell(
            &registry,
            &target,
            input(&host, "v1"),
            Holder::LifecyclePass,
        );
        assert!(Arc::ptr_eq(&v1, &undone));
        assert!(Arc::ptr_eq(&held_cell(&registry), &v1));
        // Requests on many older texts evict the least recently used, never
        // the held one.
        for text in ["r1", "r2", "r3", "r4", "r5"] {
            cell(&registry, &target, input(&host, text), Holder::Request);
        }
        assert!(Arc::ptr_eq(&held_cell(&registry), &v1));
        let region = registry.regions.iter().next().unwrap();
        assert_eq!(region.slots.len(), SLOTS_PER_REGION + 1);
    }

    #[test]
    fn another_peer_config_never_reuses_an_answer() {
        let registry = PrepareRegistry::default();
        let target = |command: &str| target_of(&[("peer", &[command])]);
        let host = Url::parse("file:///host.md").unwrap();
        let old = cell(
            &registry,
            &target("deno"),
            input(&host, "a"),
            Holder::LifecyclePass,
        );
        let _ = old.outcome.set(None);
        let new = cell(
            &registry,
            &target("bun"),
            input(&host, "a"),
            Holder::LifecyclePass,
        );
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(
            new.outcome.get().is_none(),
            "the old launch's failure is not reused"
        );
        // A request under the old config gets no slot: it neither sees the
        // new launch's answers nor starts the old one's peer.
        assert!(
            registry
                .slot(&target("deno"), input(&host, "a"), Holder::Request)
                .is_none()
        );
        assert!(Arc::ptr_eq(&held_cell(&registry), &new));
    }

    #[tokio::test]
    async fn a_request_under_old_settings_answers_nothing() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
        let target = |command: &str| target_of(&[("peer", &[command])]);
        let host = Url::parse("file:///host.md").unwrap();
        cell(
            &registry,
            &target("bun"),
            input(&host, "a"),
            Holder::LifecyclePass,
        );
        assert!(matches!(
            registry.lookup_or_start_for_request(&pool, &target("deno"), input(&host, "a")),
            PrepareLookup::Pending
        ));
        assert_eq!(
            registry
                .prepare(&pool, &target("deno"), input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Unanswered)
        );
    }

    #[tokio::test]
    async fn a_miss_tells_a_final_failure_from_a_pending_answer() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let pending = cell(&registry, &target, input(&host, "a"), Holder::LifecyclePass);
        assert!(registry.host_has_pending(&host), "no answer yet");
        assert!(!registry.held_failed(&host, "lua", REGION));
        let _ = pending.outcome.set(None);
        assert!(!registry.host_has_pending(&host));
        assert!(registry.held_failed(&host, "lua", REGION));
        assert_eq!(
            registry
                .prepare(&pool, &target, input(&host, "a"))
                .await
                .err(),
            Some(PrepareMiss::Failed),
            "an unusable answer is final"
        );
        // An undo to an answered input reads as available again.
        let answered = cell(&registry, &target, input(&host, "b"), Holder::LifecyclePass);
        let _ = answered.outcome.set(Some(Arc::new(PreparedDocument {
            text: "b".to_string(),
            map: None,
        })));
        assert!(!registry.held_failed(&host, "lua", REGION));
    }

    #[test]
    fn retain_sees_the_peer_config_answers_came_from() {
        let registry = PrepareRegistry::default();
        let target = target_of(&[("peer", &["deno"])]);
        let host = Url::parse("file:///host.md").unwrap();
        cell(&registry, &target, input(&host, "a"), Holder::Request);
        let same = |command: &str| {
            let other = target_of(&[("peer", &[command])]);
            move |_: &str, _: &str, seen: &[ResolvedServerConfig]| {
                same_candidates(seen, &other.candidates)
            }
        };
        registry.retain(same("deno"), false);
        assert_eq!(registry.regions.len(), 1);
        registry.retain(same("bun"), false);
        assert!(
            registry.regions.is_empty(),
            "a relaunched peer prepares again"
        );
    }

    #[test]
    fn a_settings_change_drops_final_failures_only_when_asking_again() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let failed = cell(&registry, &target, input(&host, "a"), Holder::LifecyclePass);
        failed.outcome.set(None).unwrap();
        registry.retain(|_, _, _| true, false);
        assert!(
            registry.held_failed(&host, "lua", REGION),
            "nothing would ask again"
        );
        registry.retain(|_, _, _| true, true);
        let fresh = cell(&registry, &target, input(&host, "a"), Holder::LifecyclePass);
        assert!(
            fresh.outcome.get().is_none(),
            "a fixed config may now prepare it"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_forgotten_attempt_does_not_start_its_peer() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let touch = format!("touch '{}'", marker.display());
        let target = target_of(&[("peer", &["sh", "-c", &touch])]);
        let host = Url::parse("file:///host.md").unwrap();
        // The attempt is spawned but (on this single-threaded runtime) has
        // not run yet when its region is forgotten — the host closed, say.
        registry.lookup_or_start(&pool, &target, input(&host, "a"));
        let cell = held_cell(&registry);
        let mut attempts = cell.attempts.subscribe();
        registry.forget_host(&host);
        tokio::time::timeout(Duration::from_secs(10), attempts.changed())
            .await
            .expect("the attempt ends")
            .unwrap();
        assert!(!marker.exists(), "a revoked attempt started its peer");
    }

    #[test]
    fn retries_back_off_to_a_minute() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(6), Duration::from_secs(32));
        assert_eq!(retry_delay(7), Duration::from_secs(60));
        assert_eq!(retry_delay(100), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn every_request_goes_out_with_a_newer_version() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let mut asked = Vec::new();
        for text in ["a", "b", "a"] {
            let slot = registry
                .slot(&target, input(&host, text), Holder::LifecyclePass)
                .unwrap();
            let _ = registry.prepare(&pool, &target, input(&host, text)).await;
            asked.extend(slot.cell.versions.lock().unwrap().drain(..));
            // Asked again from scratch, as after a settings change.
            registry.forget_region(&host, None, REGION);
        }
        assert_eq!(asked.len(), 3);
        assert!(asked.windows(2).all(|pair| pair[0] < pair[1]), "{asked:?}");
    }

    #[test]
    fn regions_are_forgotten_by_language_and_host() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        cell(&registry, &target, input(&host, "b"), Holder::Request);
        registry.forget_region(&host, Some("python"), REGION);
        assert_eq!(registry.regions.len(), 1, "another language's region stays");
        registry.forget_region_except(&host, REGION, Some("lua"));
        assert_eq!(registry.regions.len(), 1, "the current language stays");
        registry.forget_region_except(&host, REGION, Some("ruby"));
        assert!(registry.regions.is_empty());
        cell(&registry, &target, input(&host, "b"), Holder::Request);
        registry.forget_host(&host);
        assert!(registry.regions.is_empty());
    }
}
