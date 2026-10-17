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
//! [`PrepareEpoch`]): whatever can change which candidate answers, or how —
//! an advertisement, a start failure cleared, settings — moves the epoch, and
//! every lookup after that is a new input. Nothing is forgotten or revoked to
//! get there; outdated answers age out of their region's slots.
//!
//! This registry only answers. What each server holds, and so which map its
//! coordinates follow, is recorded by the paths that send, with the text
//! (see the document tracker's `HeldText`).
//!
//! The lifecycle pass must not wait for the peer under the document's edit
//! lock, so it only looks up: on a miss it starts the attempt and holds the
//! document back, and the attempt's end asks for the host to be synced again
//! ([`Resync`]). Requests wait for the answer instead.
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
    VirtualDocumentUri, VirtualLayout, apply_prepare_result_at,
};
use crate::language::injection::VirtualGap;
use crate::lsp::bridge::pool::RegionPrepare;
use crate::text::terminal::escape_terminal_controls;

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
#[allow(dead_code, reason = "requests wait for their answers through it, next")]
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
    /// A document held back became ready to send (rather than a retry
    /// coming due, or an answer coming back unusable), so the host's
    /// diagnostics are collected again too.
    pub(crate) ready: bool,
    /// The host's diagnostics are collected again too, dropping those
    /// placed from a document's earlier text: one held back got an unusable
    /// answer, so it is no longer bridged.
    pub(crate) recollect: bool,
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
    /// The `textDocument.version` of each attempt, in order.
    #[cfg(test)]
    versions: std::sync::Mutex<Vec<i32>>,
}

impl Cell {
    fn new(epoch: u64) -> Self {
        Self {
            epoch,
            taken: super::protocol::next_presentation(),
            outcome: std::sync::OnceLock::new(),
            in_flight: AtomicBool::new(false),
            misses: AtomicU32::new(0),
            retry_at: std::sync::Mutex::new(None),
            attempts: tokio::sync::watch::channel(0).0,
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
    /// A request, which waits for the answer itself: it never moves what
    /// the lifecycle pass holds the document on.
    Request,
}

pub(crate) struct PrepareRegistry {
    /// Keyed by a hash of (host URI, injection language, region id), so a
    /// lookup allocates nothing; the entry holds the identity it checks.
    regions: Arc<Regions>,
    /// Some region was ever looked up: until then every query answers "no
    /// region" without touching the map.
    used: AtomicBool,
    /// Source of `textDocument.version`: shared by all documents, so a
    /// document forgotten and prepared again (a reopen) never repeats a
    /// version the peer saw.
    next_revision: Arc<AtomicI32>,
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
            next_revision: Arc::new(AtomicI32::new(1)),
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
        let epoch = pool.prepare_epoch();
        let Some(slot) = self.slot(target, input, Holder::LifecyclePass, epoch) else {
            // A pass from before the epoch moved holds the document with no
            // answer coming: sync the host again, under the current one.
            let _ = self.resync_tx.send(Resync {
                host_uri: input.host_uri.clone(),
                ready: false,
                recollect: false,
            });
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
        if self.unused() {
            return RegionPrepare::default();
        }
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

    /// Whether any of `host_uri`'s documents the lifecycle pass holds back
    /// still waits for its answer as it stands at `epoch`.
    pub(crate) fn host_has_pending(&self, host_uri: &Url, epoch: u64) -> bool {
        if self.unused() {
            return false;
        }
        self.regions.iter().any(|region| {
            region.host_uri == host_uri.as_str()
                && region
                    .held_cell()
                    .is_some_and(|cell| !matches!(cell.standing(epoch), Ok(Some(_))))
        })
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

    /// The slot answering `input` under `epoch`, created when missing.
    /// `None` for a target resolved under another epoch (settings changed
    /// since, say): its candidates may not be the ones to ask, and asking
    /// could start a peer the settings no longer name.
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
        self.used.store(true, Ordering::Release);
        let key = input_key(target, input);
        let host_uri = input.host_uri.as_str();
        let hash = region_hash(host_uri, input.injection_language, input.region_id);
        let mut region = self.regions.entry(hash).or_insert_with(|| Region {
            host_uri: host_uri.to_string(),
            injection_language: input.injection_language.to_string(),
            region_id: input.region_id.to_string(),
            slots: Vec::new(),
            held: None,
        });
        if !region.is(host_uri, input.injection_language, input.region_id) {
            // A hash collision with another region: start over.
            *region = Region {
                host_uri: host_uri.to_string(),
                injection_language: input.injection_language.to_string(),
                region_id: input.region_id.to_string(),
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
                cell: Arc::new(Cell::new(target.epoch)),
            },
        };
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
            let Some(index) = region.slots[1..].iter().rposition(|slot| {
                Some(slot.key) != held
                    && !slot.cell.in_flight.load(Ordering::Acquire)
                    && Arc::strong_count(&slot.cell) == 1
            }) else {
                break;
            };
            region.slots.remove(index + 1);
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
        let job = PrepareJob::new(input, cell.taken);
        let resync_tx = self.resync_tx.clone();
        tokio::spawn(async move {
            let cell = Arc::clone(&end.0);
            // Its cell still stands: kept by its region, under the current
            // epoch. One forgotten (its host closed, its region replaced) or
            // outdated (settings changed, say) must not start or ask a peer
            // the settings may no longer name.
            let admitted = || {
                pool.prepare_epoch() == cell.epoch
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
            match run(&pool, &candidates, &job, &next_version, &admitted).await {
                Ok(answer) => {
                    let ready = !matches!(answer, Answer::Unusable);
                    let _ = cell.outcome.set(answer);
                    // The answer the lifecycle pass holds the document on
                    // re-syncs the host, whoever asked. An unusable one
                    // does too, unready: nothing more is coming for this
                    // input, which a pull owed a refresh must learn. One
                    // outdated meanwhile re-syncs nothing: the epoch's move
                    // does.
                    if held() && pool.prepare_epoch() == cell.epoch {
                        // The receiver is gone only at shutdown.
                        let _ = resync_tx.send(Resync {
                            host_uri: job.host_uri.clone(),
                            ready,
                            recollect: !ready,
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
                    // then.
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
                                recollect: false,
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

/// The input's identity: its text and gaps, the host language and the
/// candidates by name — under the epoch the target was resolved in, which
/// covers the candidates' launch configs (settings move it).
fn input_key(target: &PrepareTarget, input: PrepareInput<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.epoch.hash(&mut hasher);
    for candidate in target.candidates.iter() {
        candidate.server_name.hash(&mut hasher);
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
    host_uri: Url,
    host_language: String,
    injection_language: String,
    region_id: String,
    virtual_text: String,
    gaps: Vec<VirtualGap>,
}

impl PrepareJob {
    fn new(input: PrepareInput<'_>, taken: u64) -> Self {
        Self {
            taken,
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
            // epoch moved) is no failure worth a warning.
            if admitted() {
                log_failure(&unanswered);
            } else {
                log::debug!(
                    target: "kakehashi::bridge::prepare",
                    "Dropped an outdated prepare attempt for {}: {}",
                    job.host_uri,
                    escape_terminal_controls(&unanswered.to_string())
                );
            }
            Err(())
        }
    }
}

/// The outer `Err` is a failure to get an answer (worth retrying); the
/// inner one is final for the input: an answer that cannot be used (an
/// error response, a malformed or refused result). `Ok(Ok(None))`: no
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
        let failures_cleared = pool.prepare_start_failures_cleared();
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
                pool.record_prepare_advertisement_from(&handle, false);
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
            // No answer: timed out, cancelled by the peer, connection gone.
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{name} did not answer: {error}"),
                ));
            }
        };
        return Ok(
            apply_prepare_result_at(&job.virtual_text, &layout, result, job.taken)
                .map(Some)
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
        let pool = LanguageServerPool::new();
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
    fn an_input_whose_attempt_is_in_flight_is_not_evicted() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::new();
        let target = unstartable_target(&pool);
        let host = Url::parse("file:///host.md").unwrap();
        let running = cell(
            &registry,
            &pool,
            &target,
            input(&host, "a"),
            Holder::Request,
        );
        running.in_flight.store(true, Ordering::Release);
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
        let pool = Arc::new(LanguageServerPool::new());
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
                ready: true,
                recollect: false
            })
        );
    }

    #[tokio::test]
    async fn the_held_input_resyncs_whoever_started_its_attempt() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::new());
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
                ready: true,
                recollect: false
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
        let pool = Arc::new(LanguageServerPool::new());
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
                ready: false,
                recollect: false
            })
        );
    }

    #[tokio::test]
    async fn an_answer_stands_only_under_the_epoch_it_was_asked_in() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
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
        let pool = Arc::new(LanguageServerPool::new());
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

    /// A candidate advertising the request that refuses it (its first
    /// request, id 2) `delay` seconds after it arrived, recording what it
    /// was sent at `path`.
    #[cfg(unix)]
    async fn refusing_candidate(
        pool: &LanguageServerPool,
        name: &str,
        path: &std::path::Path,
        delay: f64,
    ) {
        use crate::lsp::bridge::pool::ConnectionState;
        use crate::lsp::bridge::pool::test_helpers::{
            advertise_prepare_for_test, create_handle_with_command,
        };
        let refusal = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"no"}}"#;
        let script = format!(
            // An asynchronous list reads /dev/null unless given stdin.
            "exec 3<&0; (cat <&3 > \"$1\") & \
             while ! grep -q virtualDocument \"$1\" 2>/dev/null; do sleep 0.05; done; \
             sleep {delay}; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{refusal}'; wait",
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
                ready: false,
                recollect: false
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unusable_answer_resyncs_its_host_as_failed() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::new());
        let dir = tempfile::tempdir().unwrap();
        refusing_candidate(&pool, "peer", &dir.path().join("peer"), 0.0).await;
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
            Some(PrepareMiss::Failed)
        );
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
                host_uri: host,
                ready: false,
                recollect: true
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_withdrawn_advertisement_outdates_the_answer_and_passes_its_server_over() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let sent = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
        // `first` answers a while after the request arrived, having
        // withdrawn its advertisement meanwhile.
        refusing_candidate(&pool, "first", &first, 0.5).await;
        pool.record_prepare_advertisement(&ConnectionKey::for_server("first"), true);
        recording_candidate(&pool, "second", true, &second).await;
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
    fn retries_back_off_to_a_minute() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(6), Duration::from_secs(32));
        assert_eq!(retry_delay(7), Duration::from_secs(60));
        assert_eq!(retry_delay(100), Duration::from_secs(60));
    }

    #[test]
    fn regions_are_forgotten_by_language_and_host() {
        let registry = PrepareRegistry::default();
        let pool = LanguageServerPool::new();
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
}
