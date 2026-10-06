//! Cache and single flight for `kakehashi/virtualDocument/prepare`.
//!
//! Every site that hands a virtual document to downstream servers — the
//! didOpen/didChange pass, request fan-out, push-diagnostic translation —
//! looks its prepared form up here by the exact virtual text and gaps it
//! holds, so a site can never pair one text with another text's map. One
//! answer per virtual document revision is shared by all of them.
//!
//! The lifecycle pass must not wait for the peer under the document's edit
//! lock, so it only looks up: on a miss it starts the request and holds the
//! document back, and the finished request asks for the host to be synced
//! again. Requests are already asynchronous and wait for the answer.
//!
//! Every attempt runs on its own task, so a cancelled request cannot strand
//! one half-done, and all waiters share it. An attempt that gets no answer
//! (the peer not starting in time, crashing, timing out) is retried with
//! backoff by syncing the host again; one that gets an unusable answer is
//! final for that revision.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use url::Url;

use super::pool::LanguageServerPool;
use super::protocol::{
    PrepareHostTextDocument, PrepareParams, PrepareTextDocument, PreparedDocument,
    VirtualDocumentUri, VirtualLayout, apply_prepare_result,
};
use crate::config::settings::BridgeServerConfig;
use crate::language::injection::VirtualGap;

/// The peer that prepares one (host, injection) pair's virtual documents.
#[derive(Debug, Clone)]
pub(crate) struct PrepareTarget {
    pub(crate) server_name: String,
    /// `None` when the name is configured but not a startable server: every
    /// prepare then fails without touching the pool, whose connection for
    /// that name (if any) belongs to a different launch config.
    pub(crate) config: Option<Arc<BridgeServerConfig>>,
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
    /// The prepared document to send.
    Ready(Arc<PreparedDocument>),
    /// The peer could not prepare this revision: send nothing.
    Failed,
    /// The answer is not in yet: send nothing now; the host is synced again
    /// when it arrives.
    Pending,
}

/// `None` is a prepare the peer answered unusably.
type Outcome = Option<Arc<PreparedDocument>>;

/// One region's prepared document: the generation the lifecycle pass works
/// on, and the one before it.
///
/// Keeping the previous generation lets a request built on slightly older
/// text find its answer without evicting the current one (which would
/// restart the lifecycle pass's attempt and hold the document longer), and
/// lets a lookup by the text a server still holds succeed while the next
/// text is being prepared.
struct Entry {
    /// The region this entry is for (the map key is their hash).
    host_uri: String,
    injection_language: String,
    region_id: String,
    /// The host language and peer the entry was prepared for, so a settings
    /// change that retargets the pair can drop it.
    host_language: String,
    server_name: String,
    /// The peer's launch config the answers came from (`None`: unstartable).
    server_config: Option<Arc<BridgeServerConfig>>,
    current: Generation,
    previous: Option<Generation>,
}

impl Entry {
    fn is(&self, host_uri: &str, injection_language: &str, region_id: &str) -> bool {
        self.host_uri == host_uri
            && self.injection_language == injection_language
            && self.region_id == region_id
    }

    fn generations(&self) -> impl Iterator<Item = &Generation> {
        std::iter::once(&self.current).chain(&self.previous)
    }
}

/// The prepared answer for one input (text, gaps, peer).
#[derive(Clone)]
struct Generation {
    /// Identity of the input (text, gaps, peer, host language).
    key: u64,
    /// Identity of the virtual text alone, for lookups that only know it.
    text_key: u64,
    /// The `textDocument.version` sent for this input.
    revision: i32,
    cell: Arc<Cell>,
}

/// The answer for one revision, and the attempts to get it.
#[derive(Default)]
struct Cell {
    /// Set once an attempt got an answer: `Some` prepared, `None` unusable.
    outcome: std::sync::OnceLock<Outcome>,
    /// An attempt is running.
    in_flight: AtomicBool,
    /// Attempts that got no answer, for the backoff.
    misses: AtomicU32,
    /// No new attempt before this, after a miss.
    retry_at: std::sync::Mutex<Option<Instant>>,
    /// Bumped after every attempt, answered or not.
    attempts: tokio::sync::watch::Sender<u32>,
}

impl Cell {
    fn lookup(&self) -> Option<PrepareLookup> {
        self.outcome.get().map(|outcome| match outcome {
            Some(prepared) => PrepareLookup::Ready(Arc::clone(prepared)),
            None => PrepareLookup::Failed,
        })
    }

    fn backing_off(&self) -> bool {
        self.retry_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|retry_at| Instant::now() < retry_at)
    }
}

/// Ends an attempt however its task ends — a panic included — so waiters
/// wake and the next lookup may start another.
struct AttemptEnd(Arc<Cell>);

impl Drop for AttemptEnd {
    fn drop(&mut self) {
        self.0.in_flight.store(false, Ordering::Release);
        self.0.attempts.send_modify(|attempts| *attempts += 1);
    }
}

/// A host to sync again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resync {
    pub(crate) host_uri: Url,
    /// A held document became ready (rather than a retry being due), so the
    /// host's diagnostics should be collected again too.
    pub(crate) ready: bool,
}

/// How a virtual document reached downstream servers, as far as the registry
/// knows from its text alone.
#[derive(Debug, Clone)]
pub(crate) enum PreparedState {
    /// Never prepared: sent as is.
    Unprepared,
    /// Sent prepared, with this map.
    Prepared(Option<Arc<super::protocol::PreparedMap>>),
    /// Prepared, but servers were sent another text (or none yet):
    /// downstream coordinates for this text are unknown.
    Unavailable,
}

type Entries = DashMap<u64, Entry>;

/// The text the lifecycle pass last sent for a region, once anything is
/// prepared. Kept apart from [`Entry`], which a settings change drops: the
/// text a server holds does not change with the settings, and the next send
/// must still be compared with it.
struct Sent {
    host_uri: String,
    injection_language: String,
    region_id: String,
    /// The answer sent, or `None` for the virtual text sent unprepared. An
    /// answer's map is the one the server's coordinates follow, which its
    /// text alone does not identify (two answers may differ only in where a
    /// gap's replacement maps back to).
    prepared: Option<Arc<PreparedDocument>>,
    /// Fingerprint of the text sent, as connections record theirs.
    fingerprint: u64,
}

impl Sent {
    fn is(&self, host_uri: &str, region_id: &str) -> bool {
        self.host_uri == host_uri && self.region_id == region_id
    }
}

pub(crate) struct PrepareRegistry {
    /// Keyed by a hash of (host URI, injection language, region id), so a
    /// lookup allocates nothing; the entry holds the identity it checks.
    entries: Arc<Entries>,
    /// Keyed by a hash of (host URI, region id): a region is sent under one
    /// language at a time, and a push names only its region.
    sent: DashMap<u64, Sent>,
    /// Whether anything was ever prepared: lets the lookups every bridged
    /// region makes per edit skip the map when no pair has a prepare peer.
    ever_used: AtomicBool,
    /// Source of `textDocument.version`: shared by all documents, so a
    /// document forgotten and prepared again (a settings change, a reopen)
    /// never repeats a version the peer saw.
    next_revision: std::sync::atomic::AtomicI32,
    resync_tx: UnboundedSender<Resync>,
    resync_rx: std::sync::Mutex<Option<UnboundedReceiver<Resync>>>,
}

impl std::fmt::Debug for PrepareRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrepareRegistry")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl Default for PrepareRegistry {
    fn default() -> Self {
        let (resync_tx, resync_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            entries: Arc::default(),
            sent: DashMap::new(),
            ever_used: AtomicBool::new(false),
            next_revision: std::sync::atomic::AtomicI32::new(1),
            resync_tx,
            resync_rx: std::sync::Mutex::new(Some(resync_rx)),
        }
    }
}

impl PrepareRegistry {
    /// Whether any document was ever prepared: until then every virtual
    /// document was sent as is.
    pub(crate) fn ever_used(&self) -> bool {
        self.ever_used.load(Ordering::Acquire)
    }

    /// Hosts to sync again: a held-back virtual document became ready, or
    /// an attempt that got no answer finished its retry backoff. Taken once
    /// by the server loop.
    pub(crate) fn take_resync_rx(&self) -> Option<UnboundedReceiver<Resync>> {
        self.resync_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Look up without waiting; on a miss, start an attempt in the
    /// background and report [`PrepareLookup::Pending`].
    pub(crate) fn lookup_or_start(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        pool.note_prepare_used();
        let (cell, revision) = self.cell(target, input, Holder::LifecyclePass);
        if let Some(found) = cell.lookup() {
            return found;
        }
        self.start(&cell, pool, target, input, revision);
        PrepareLookup::Pending
    }

    /// Wait for the prepared document, sharing an attempt already running.
    /// `None` when this revision has no prepared document (yet): the peer
    /// answered unusably, or the attempt got no answer, or the last one did
    /// not and the retry is still backing off.
    pub(crate) async fn prepare(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> Outcome {
        pool.note_prepare_used();
        let (cell, revision) = self.cell(target, input, Holder::Request);
        if let Some(outcome) = cell.outcome.get() {
            return outcome.clone();
        }
        // Subscribe before starting, so the attempt's end cannot be missed.
        let mut attempts = cell.attempts.subscribe();
        if !self.start(&cell, pool, target, input, revision)
            && !cell.in_flight.load(Ordering::Acquire)
        {
            // Backing off after a miss: do not queue another attempt per
            // request, each waiting out its own timeouts.
            return cell.outcome.get().cloned().flatten();
        }
        // The attempt ends (answered or not) with a bump.
        let _ = attempts.changed().await;
        cell.outcome.get().cloned().flatten()
    }

    /// Start an attempt unless one is running, an answer is in, or a miss is
    /// backing off. `true` when this call started it.
    fn start(
        &self,
        cell: &Arc<Cell>,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        revision: i32,
    ) -> bool {
        if cell.outcome.get().is_some() || cell.backing_off() {
            return false;
        }
        if cell.in_flight.swap(true, Ordering::AcqRel) {
            return false;
        }
        let end = AttemptEnd(Arc::clone(cell));
        let entries = Arc::clone(&self.entries);
        let region = region_hash(
            input.host_uri.as_str(),
            input.injection_language,
            input.region_id,
        );
        let pool = Arc::clone(pool);
        let target = target.clone();
        let job = PrepareJob::from(input);
        let resync_tx = self.resync_tx.clone();
        tokio::spawn(async move {
            let cell = Arc::clone(&end.0);
            // Only the generation the lifecycle pass works on holds a
            // document back; an older one's answer re-syncs nothing.
            let current = |entries: &Entries| {
                entries
                    .get(&region)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.current.cell, &cell))
            };
            // A settings change or close that dropped this cell's entry
            // revokes the attempt: it must not start (or replace) a peer
            // the settings no longer name.
            let admitted = || {
                entries.get(&region).is_some_and(|entry| {
                    entry
                        .generations()
                        .any(|generation| Arc::ptr_eq(&generation.cell, &cell))
                })
            };
            match run(&pool, &target, &job, revision, &admitted).await {
                Ok(outcome) => {
                    let prepared = outcome.is_some();
                    let _ = cell.outcome.set(outcome);
                    if prepared && current(&entries) {
                        // The receiver is gone only at shutdown.
                        let _ = resync_tx.send(Resync {
                            host_uri: job.host_uri.clone(),
                            ready: true,
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
                    // the next edit; sync the host once the backoff ends,
                    // unless newer text has taken over by then.
                    let host_uri = job.host_uri.clone();
                    let cell = Arc::clone(&cell);
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let still_current = entries
                            .get(&region)
                            .is_some_and(|entry| Arc::ptr_eq(&entry.current.cell, &cell));
                        if still_current {
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

    /// How the document with this exact virtual text was sent, for paths
    /// that translate downstream coordinates without the settings that
    /// select a prepare peer (resolve gates, inbound edits).
    ///
    /// Only the answer the lifecycle pass last sent describes what servers
    /// hold: an answer that is in but not sent yet (the next text's, or one
    /// held back) is not theirs, and neither is any answer once the stored
    /// text differs from the one it was sent for.
    pub(crate) fn state(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        virtual_text: &str,
    ) -> PreparedState {
        if !self.ever_used.load(Ordering::Acquire) {
            return PreparedState::Unprepared;
        }
        let host_uri = host_uri.as_str();
        let sent = self
            .sent
            .get(&sent_key(host_uri, region_id))
            .filter(|sent| {
                sent.is(host_uri, region_id) && sent.injection_language == injection_language
            })
            .and_then(|sent| sent.prepared.clone());
        let entry = self
            .entries
            .get(&region_hash(host_uri, injection_language, region_id))
            .filter(|entry| entry.is(host_uri, injection_language, region_id));
        let (entry, sent) = match (entry, sent) {
            // Never prepared, or sent unprepared since the pair lost its peer.
            (None, None) => return PreparedState::Unprepared,
            // Held since it was prepared: servers hold nothing of it.
            (Some(_), None) => return PreparedState::Unavailable,
            // Servers hold a prepared text whose entry a settings change
            // dropped: no map for it is left.
            (None, Some(_)) => return PreparedState::Unavailable,
            (Some(entry), Some(sent)) => (entry, sent),
        };
        let wanted = text_key(virtual_text);
        // Matched by identity: an edit inside a gap changes only the gaps,
        // which the virtual text masks, and two answers can even share a
        // prepared text while mapping it back differently.
        entry
            .generations()
            .filter(|generation| generation.text_key == wanted)
            .find_map(|generation| {
                generation
                    .cell
                    .outcome
                    .get()
                    .and_then(Option::as_ref)
                    .filter(|prepared| Arc::ptr_eq(prepared, &sent))
            })
            .map_or(PreparedState::Unavailable, |prepared| {
                PreparedState::Prepared(prepared.map.clone())
            })
    }

    /// Keep only the entries `keep(host language, injection language, peer,
    /// peer config)` accepts — after a settings change, those whose pair
    /// still names the same peer with the same launch config. With `drop_unusable`, answers cached as final go too: the
    /// change may have fixed what made them unusable (a peer's command,
    /// say), and the caller guarantees a pass that asks again — a dropped
    /// entry reads as unprepared until then, which would misread a server
    /// still holding an older prepared text. A dropped entry otherwise reads
    /// as unprepared until prepared again.
    pub(crate) fn retain(
        &self,
        keep: impl Fn(&str, &str, &str, Option<&BridgeServerConfig>) -> bool,
        drop_unusable: bool,
    ) {
        self.entries.retain(|_, entry| {
            keep(
                &entry.host_language,
                &entry.injection_language,
                &entry.server_name,
                entry.server_config.as_deref(),
            ) && !(drop_unusable && matches!(entry.current.cell.outcome.get(), Some(None)))
        });
    }

    /// Note that the lifecycle pass sends `prepared` for a region. `true`
    /// unless its text is the prepared text sent last: diagnostics a server
    /// pushed for any other text — an earlier prepared one, or the unprepared
    /// one sent while the pair had no peer — are in coordinates no current
    /// map describes.
    pub(crate) fn note_sent(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        prepared: &Arc<PreparedDocument>,
    ) -> bool {
        let fingerprint = super::pool::content_fingerprint(&prepared.text);
        self.record_sent(
            host_uri,
            injection_language,
            region_id,
            Some(prepared),
            fingerprint,
        )
    }

    /// Note that the lifecycle pass sends a region's virtual text unprepared
    /// (its pair has no peer, or no longer): whatever was prepared for it no
    /// longer describes it. `true` when a prepared text had been sent for it,
    /// whose pushed diagnostics no longer describe what is sent now. Nothing
    /// is recorded until anything is prepared.
    pub(crate) fn note_unprepared_sent(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        virtual_text: &str,
    ) -> bool {
        if !self.ever_used.load(Ordering::Acquire) {
            return false;
        }
        let host = host_uri.as_str();
        self.entries.remove_if(
            &region_hash(host, injection_language, region_id),
            |_, entry| entry.is(host, injection_language, region_id),
        );
        let fingerprint = super::pool::content_fingerprint(virtual_text);
        self.record_sent(host_uri, injection_language, region_id, None, fingerprint)
    }

    /// The fingerprint of the text last sent for a region, and the language
    /// it was sent under, once anything is prepared: a server's push for the
    /// region is in that text's coordinates only if it holds that text.
    pub(crate) fn sent_fingerprint(
        &self,
        host_uri: &Url,
        region_id: &str,
    ) -> Option<(u64, String)> {
        if !self.ever_used.load(Ordering::Acquire) {
            return None;
        }
        let host_uri = host_uri.as_str();
        self.sent
            .get(&sent_key(host_uri, region_id))
            .filter(|sent| sent.is(host_uri, region_id))
            .map(|sent| (sent.fingerprint, sent.injection_language.clone()))
    }

    /// Record what is sent for a region; `true` when it replaces a text in
    /// other coordinates: another prepared one, a prepared one replacing an
    /// unprepared one (or none), an unprepared one replacing a prepared one,
    /// or either under another language.
    fn record_sent(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        prepared: Option<&Arc<PreparedDocument>>,
        fingerprint: u64,
    ) -> bool {
        let host = host_uri.as_str();
        let key = sent_key(host, region_id);
        if let Some(mut sent) = self.sent.get_mut(&key)
            && sent.is(host, region_id)
        {
            let same_language = sent.injection_language == injection_language;
            let replaced = match (&sent.prepared, prepared) {
                (Some(previous), Some(prepared)) => {
                    !same_language
                        || !Arc::ptr_eq(previous, prepared) && sent.fingerprint != fingerprint
                }
                (None, None) => !same_language,
                _ => true,
            };
            if !same_language {
                sent.injection_language = injection_language.to_string();
            }
            sent.prepared = prepared.cloned();
            sent.fingerprint = fingerprint;
            return replaced;
        }
        self.sent.insert(
            key,
            Sent {
                host_uri: host.to_string(),
                injection_language: injection_language.to_string(),
                region_id: region_id.to_string(),
                prepared: prepared.cloned(),
                fingerprint,
            },
        );
        // Nothing recorded: the first prepared text replaces whatever was
        // sent before anything was prepared.
        prepared.is_some()
    }

    /// Forget one region's document: its pair no longer has a peer, or the
    /// region itself was replaced or invalidated. `injection_language`
    /// `None` forgets the region under every language. `true` when a
    /// prepared text had been sent for it, whose pushed diagnostics no
    /// longer describe what is sent next.
    pub(crate) fn forget_region(
        &self,
        host_uri: &Url,
        injection_language: Option<&str>,
        region_id: &str,
    ) -> bool {
        if !self.ever_used.load(Ordering::Acquire) {
            return false;
        }
        let host_uri = host_uri.as_str();
        match injection_language {
            Some(language) => {
                self.entries
                    .remove_if(&region_hash(host_uri, language, region_id), |_, entry| {
                        entry.is(host_uri, language, region_id)
                    });
                self.sent
                    .remove_if(&sent_key(host_uri, region_id), |_, sent| {
                        sent.is(host_uri, region_id) && sent.injection_language == language
                    })
                    .is_some_and(|(_, sent)| sent.prepared.is_some())
            }
            None => {
                self.entries
                    .retain(|_, entry| entry.host_uri != host_uri || entry.region_id != region_id);
                self.sent
                    .remove_if(&sent_key(host_uri, region_id), |_, sent| {
                        sent.is(host_uri, region_id)
                    })
                    .is_some_and(|(_, sent)| sent.prepared.is_some())
            }
        }
    }

    /// Forget a replaced region's documents under every language but
    /// `keep` — the language the region resolves to now, whose document may
    /// already be preparing.
    pub(crate) fn forget_region_except(&self, host_uri: &Url, region_id: &str, keep: Option<&str>) {
        if !self.ever_used.load(Ordering::Acquire) {
            return;
        }
        let host_uri = host_uri.as_str();
        self.entries.retain(|_, entry| {
            entry.host_uri != host_uri
                || entry.region_id != region_id
                || Some(entry.injection_language.as_str()) == keep
        });
        self.sent.retain(|_, sent| {
            sent.host_uri != host_uri
                || sent.region_id != region_id
                || Some(sent.injection_language.as_str()) == keep
        });
    }

    /// Forget a closed host's documents.
    pub(crate) fn forget_host(&self, host_uri: &Url) {
        self.entries
            .retain(|_, entry| entry.host_uri != host_uri.as_str());
        self.sent
            .retain(|_, sent| sent.host_uri != host_uri.as_str());
    }

    /// The cell answering `input`: the current or previous generation when
    /// either matches, else a new current generation (with a new revision)
    /// that demotes the current one.
    fn cell(
        &self,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
        holder: Holder,
    ) -> (Arc<Cell>, i32) {
        self.ever_used.store(true, Ordering::Release);
        let key = input_key(target, input);
        let host_uri = input.host_uri.as_str();
        let fresh = || Generation {
            key,
            text_key: text_key(input.virtual_text),
            revision: self.revision(),
            cell: Arc::default(),
        };
        let mut entry = self
            .entries
            .entry(region_hash(
                host_uri,
                input.injection_language,
                input.region_id,
            ))
            .or_insert_with(|| Entry {
                host_uri: host_uri.to_string(),
                injection_language: input.injection_language.to_string(),
                region_id: input.region_id.to_string(),
                host_language: input.host_language.to_string(),
                server_name: target.server_name.clone(),
                server_config: target.config.clone(),
                current: fresh(),
                previous: None,
            });
        if !entry.is(host_uri, input.injection_language, input.region_id)
            // The same peer launched differently answers differently, and
            // the config is no part of the input key: a lookup made under
            // other settings than the entry's (one still in flight across a
            // settings change, which may even have recreated the entry after
            // the prune) must not see its answers, and its own must not
            // outlive it.
            || entry.server_config.as_deref() != target.config.as_deref()
        {
            if holder == Holder::Request
                && entry.is(host_uri, input.injection_language, input.region_id)
            {
                // A request does not take the slot over: under settings
                // older than the entry's it would displace the generation
                // the lifecycle pass holds the document on, whose answer
                // alone re-syncs. Its cell belongs to no entry, so its
                // attempt is not admitted and the request answers nothing;
                // under newer settings, the lifecycle pass they bring takes
                // the slot over.
                return (Arc::default(), self.revision());
            }
            // A hash collision with another region, or another launch config:
            // take the slot over.
            *entry = Entry {
                host_uri: host_uri.to_string(),
                injection_language: input.injection_language.to_string(),
                region_id: input.region_id.to_string(),
                host_language: input.host_language.to_string(),
                server_name: target.server_name.clone(),
                server_config: target.config.clone(),
                current: fresh(),
                previous: None,
            };
        }
        if entry.current.key == key {
            return (Arc::clone(&entry.current.cell), entry.current.revision);
        }
        if let Some(previous) = entry
            .previous
            .as_ref()
            .filter(|previous| previous.key == key)
        {
            // An answered previous generation serves anyone. An unanswered
            // one serves a request (which waits on it) but not the lifecycle
            // pass, which would hold the document on it: only the current
            // generation's answer re-syncs, so a text the user returned to
            // (an undo) gets a new current generation and revision instead.
            if previous.cell.outcome.get().is_some() || holder == Holder::Request {
                return (Arc::clone(&previous.cell), previous.revision);
            }
        }
        if holder == Holder::Request {
            // A request's text that is neither generation (older than both,
            // or not yet seen by the lifecycle pass) must not take the
            // current slot: the lifecycle pass holds its document on that
            // one, and only its answer re-syncs. It takes the previous slot,
            // where a later lifecycle lookup of the same text still finds an
            // answer that has arrived.
            let generation = fresh();
            let handle = (Arc::clone(&generation.cell), generation.revision);
            entry.previous = Some(generation);
            return handle;
        }
        entry.host_language = input.host_language.to_string();
        entry.server_name = target.server_name.clone();
        entry.server_config = target.config.clone();
        let current = std::mem::replace(&mut entry.current, fresh());
        entry.previous = Some(current);
        (Arc::clone(&entry.current.cell), entry.current.revision)
    }

    fn revision(&self) -> i32 {
        // Wraps after 2^31 prepares; the peer only compares a document's
        // versions over its lifetime.
        self.next_revision.fetch_add(1, Ordering::Relaxed)
    }
}

/// Who looks a document up: what an unanswered older generation is good for
/// depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// The lifecycle pass, which holds the document until the answer
    /// re-syncs it.
    LifecyclePass,
    /// A request, which waits for the answer itself.
    Request,
}

/// Backoff before retrying an attempt that got no answer: one second,
/// doubling, at most a minute.
fn retry_delay(misses: u32) -> Duration {
    Duration::from_secs((1u64 << misses.saturating_sub(1).min(6)).min(60))
}

fn region_hash(host_uri: &str, injection_language: &str, region_id: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (host_uri, injection_language, region_id).hash(&mut hasher);
    hasher.finish()
}

fn input_key(target: &PrepareTarget, input: PrepareInput<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.server_name.hash(&mut hasher);
    input.host_language.hash(&mut hasher);
    input.virtual_text.hash(&mut hasher);
    input.gaps.hash(&mut hasher);
    hasher.finish()
}

fn sent_key(host_uri: &str, region_id: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (host_uri, region_id).hash(&mut hasher);
    hasher.finish()
}

fn text_key(virtual_text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    virtual_text.hash(&mut hasher);
    hasher.finish()
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

/// Ask the peer and apply its answer. `Ok(None)` is an answer kakehashi
/// refused (or the peer refused to give); `Err` is no answer at all. Both
/// are logged; neither lets the document through unprepared.
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
            "Not sending the {} virtual document of {}: {} could not prepare it: {}",
            job.injection_language,
            job.host_uri,
            target.server_name,
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
/// inner one is final for the revision: an answer that cannot be used (an
/// error response, a malformed or refused result) or a peer that cannot
/// answer (not startable, not advertising the request).
async fn try_run(
    pool: &LanguageServerPool,
    target: &PrepareTarget,
    job: &PrepareJob,
    revision: i32,
    admitted: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<std::io::Result<PreparedDocument>> {
    let Some(config) = target.config.as_deref() else {
        return Ok(Err(std::io::Error::other(format!(
            "{} is not a startable language server",
            target.server_name
        ))));
    };
    let handle = pool
        .get_or_create_connection_wait_ready_admitted(
            &target.server_name,
            config,
            Some(&job.host_uri),
            Duration::from_secs(super::INIT_TIMEOUT_SECS),
            Some(admitted),
            None,
        )
        .await?;
    let host_uri = match crate::lsp::lsp_impl::url_to_uri(&job.host_uri) {
        Ok(host_uri) => host_uri,
        Err(error) => return Ok(Err(std::io::Error::other(error.to_string()))),
    };
    let virtual_uri =
        VirtualDocumentUri::new(&host_uri, &job.injection_language, &job.region_id).to_uri_string();
    let layout = layout(&job.virtual_text, &job.gaps);
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
    let result = match handle.request_virtual_document_prepare(&params).await {
        Ok(result) => result,
        // Final for this revision: an unusable answer (an error response or a
        // malformed result), or a peer that does not answer this request at
        // all — its advertisement is fixed until it restarts.
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::Unsupported
            ) =>
        {
            return Ok(Err(error));
        }
        // No answer: timed out, cancelled by the peer, connection gone.
        Err(error) => return Err(error),
    };
    Ok(apply_prepare_result(&job.virtual_text, &layout, result)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)))
}

/// Present a virtual document as content/gap segments: the gaps the
/// resolver recorded, and content everywhere between them.
fn layout(virtual_text: &str, gaps: &[VirtualGap]) -> VirtualLayout {
    use super::protocol::SegmentKind;
    let mut pieces = Vec::with_capacity(gaps.len() * 2 + 1);
    let mut cursor = 0;
    for gap in gaps {
        if cursor < gap.virtual_range.start {
            pieces.push((
                SegmentKind::Content,
                cursor..gap.virtual_range.start,
                String::new(),
            ));
        }
        pieces.push((
            SegmentKind::Gap,
            gap.virtual_range.clone(),
            gap.host_text.clone(),
        ));
        cursor = cursor.max(gap.virtual_range.end);
    }
    if cursor < virtual_text.len() || pieces.is_empty() {
        pieces.push((
            SegmentKind::Content,
            cursor..virtual_text.len(),
            String::new(),
        ));
    }
    VirtualLayout::from_pieces(virtual_text, pieces)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::SegmentKind;

    #[test]
    fn layout_fills_content_between_gaps() {
        let virtual_text = "x =     \nprint(x)\n";
        let gaps = [VirtualGap {
            virtual_range: 4..8,
            host_text: "${a}".to_string(),
        }];
        let layout = layout(virtual_text, &gaps);
        let segments: Vec<_> = layout
            .segments()
            .iter()
            .map(|segment| (segment.kind, segment.text.as_str()))
            .collect();
        assert_eq!(
            segments,
            vec![
                (SegmentKind::Content, "x = "),
                (SegmentKind::Gap, "${a}"),
                (SegmentKind::Content, "\nprint(x)\n"),
            ]
        );
    }

    #[test]
    fn layout_of_an_isolated_document_is_one_content_segment() {
        let layout = layout("a\n", &[]);
        assert_eq!(layout, VirtualLayout::single("a\n"));
        // An empty document still presents one (empty) content segment.
        assert_eq!(layout_of_empty().segments().len(), 1);
    }

    fn layout_of_empty() -> VirtualLayout {
        layout("", &[])
    }

    fn unstartable_target() -> PrepareTarget {
        PrepareTarget {
            server_name: "peer".to_string(),
            config: None,
        }
    }

    #[tokio::test]
    async fn an_unusable_peer_is_final_and_asked_once() {
        let registry = PrepareRegistry::default();
        let pool = Arc::new(LanguageServerPool::new());
        let host = Url::parse("file:///host.md").unwrap();
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: "a",
            gaps: &[],
        };
        let target = unstartable_target();
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input),
            PrepareLookup::Pending
        ));
        // A request joins the attempt and sees its (final) failure.
        assert!(registry.prepare(&pool, &target, input).await.is_none());
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input),
            PrepareLookup::Failed
        ));
        let (cell, _) = registry.cell(&target, input, Holder::Request);
        assert!(!cell.in_flight.load(Ordering::Acquire));
        assert_eq!(*cell.attempts.borrow(), 1, "the failure was not retried");
    }

    #[test]
    fn an_unused_registry_reads_unprepared_without_entries() {
        let registry = PrepareRegistry::default();
        let host = Url::parse("file:///host.md").unwrap();
        assert!(matches!(
            registry.state(&host, "lua", "r", "a"),
            PreparedState::Unprepared
        ));
        registry.forget_region(&host, None, "r");
        assert!(!registry.ever_used.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn a_peer_that_cannot_start_is_retried_after_a_backoff() {
        let registry = PrepareRegistry::default();
        let mut resync = registry.take_resync_rx().unwrap();
        let pool = Arc::new(LanguageServerPool::new());
        let host = Url::parse("file:///host.md").unwrap();
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: "a",
            gaps: &[],
        };
        let target = PrepareTarget {
            server_name: "peer".to_string(),
            config: Some(Arc::new(BridgeServerConfig {
                cmd: Some(vec!["/nonexistent/kakehashi-prepare-peer".to_string()]),
                ..Default::default()
            })),
        };
        // No answer: not cached, so the document is still pending…
        assert!(registry.prepare(&pool, &target, input).await.is_none());
        let (cell, _) = registry.cell(&target, input, Holder::Request);
        assert!(cell.outcome.get().is_none());
        assert_eq!(cell.misses.load(Ordering::Acquire), 1);
        // …but backing off: neither a lookup nor a request starts another
        // attempt yet.
        assert!(matches!(
            registry.lookup_or_start(&pool, &target, input),
            PrepareLookup::Pending
        ));
        assert!(registry.prepare(&pool, &target, input).await.is_none());
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
    fn an_older_text_finds_its_generation_without_evicting_the_current() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let input = |text| PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: text,
            gaps: &[],
        };
        let (old, _) = registry.cell(&target, input("v1"), Holder::LifecyclePass);
        let (current, _) = registry.cell(&target, input("v2"), Holder::LifecyclePass);
        // A request still on v1 gets v1's cell, and v2 stays current.
        let (again, _) = registry.cell(&target, input("v1"), Holder::Request);
        assert!(Arc::ptr_eq(&old, &again));
        let entry = registry.entries.iter().next().unwrap();
        assert!(Arc::ptr_eq(&entry.current.cell, &current));
        drop(entry);
        // The lifecycle pass moving on to a third text drops the oldest.
        let (third, _) = registry.cell(&target, input("v3"), Holder::LifecyclePass);
        let (fresh, _) = registry.cell(&target, input("v1"), Holder::Request);
        assert!(!Arc::ptr_eq(&old, &fresh));
        // That stale request did not take the current slot.
        let entry = registry.entries.iter().next().unwrap();
        assert!(Arc::ptr_eq(&entry.current.cell, &third));
    }

    #[test]
    fn the_lifecycle_pass_never_waits_on_an_unanswered_older_generation() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let input = |text| PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: text,
            gaps: &[],
        };
        let (v1, v1_revision) = registry.cell(&target, input("v1"), Holder::LifecyclePass);
        registry.cell(&target, input("v2"), Holder::LifecyclePass);
        // Undo to v1 before v1 was answered: a fresh, current generation.
        let (undone, undone_revision) = registry.cell(&target, input("v1"), Holder::LifecyclePass);
        assert!(!Arc::ptr_eq(&v1, &undone));
        assert!(undone_revision > v1_revision);
        let entry = registry.entries.iter().next().unwrap();
        assert!(Arc::ptr_eq(&entry.current.cell, &undone));
    }

    #[test]
    fn retain_sees_the_peer_config_answers_came_from() {
        let registry = PrepareRegistry::default();
        let config = |cmd: &str| BridgeServerConfig {
            cmd: Some(vec![cmd.to_string()]),
            ..Default::default()
        };
        let target = PrepareTarget {
            server_name: "peer".to_string(),
            config: Some(Arc::new(config("deno"))),
        };
        let host = Url::parse("file:///host.md").unwrap();
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: "a",
            gaps: &[],
        };
        registry.cell(&target, input, Holder::Request);
        registry.retain(|_, _, _, seen| seen == Some(&config("deno")), false);
        assert_eq!(registry.entries.len(), 1);
        registry.retain(|_, _, _, seen| seen == Some(&config("bun")), false);
        assert!(
            registry.entries.is_empty(),
            "a relaunched peer prepares again"
        );
    }

    fn sent_text(text: &str) -> Arc<PreparedDocument> {
        Arc::new(PreparedDocument {
            text: text.to_string(),
            map: None,
        })
    }

    #[test]
    fn a_changed_sent_text_is_noticed() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let region = "01J0000000000000000000000A";
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: region,
            virtual_text: "  a",
            gaps: &[],
        };
        registry.cell(&target, input, Holder::LifecyclePass);
        assert!(
            registry.note_sent(&host, "lua", region, &sent_text("a")),
            "the first prepared text replaces whatever was sent unprepared"
        );
        assert!(
            !registry.note_sent(&host, "lua", region, &sent_text("a")),
            "the same text"
        );
        assert!(
            registry.note_sent(&host, "lua", region, &sent_text("  a")),
            "a different text"
        );
    }

    #[test]
    fn the_sent_text_outlives_a_settings_change() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let region = "01J0000000000000000000000A";
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: region,
            virtual_text: "  a",
            gaps: &[],
        };
        registry.cell(&target, input, Holder::LifecyclePass);
        registry.note_sent(&host, "lua", region, &sent_text("a"));
        // A retargeted peer: the entry goes, the server still holds "a".
        registry.retain(|_, _, _, _| false, false);
        assert!(
            registry.note_sent(&host, "lua", region, &sent_text("  a")),
            "the new peer's text differs from the one the server holds"
        );
        // The peer removed: the region is sent unprepared next.
        assert!(
            registry.forget_region(&host, Some("lua"), region),
            "a prepared text had been sent"
        );
        assert!(
            !registry.forget_region(&host, Some("lua"), region),
            "nothing prepared is left to replace"
        );
    }

    #[test]
    fn a_gap_only_undo_reads_the_map_that_was_sent() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let region = "01J0000000000000000000000A";
        let gap = |host_text: &str| VirtualGap {
            virtual_range: 1..2,
            host_text: host_text.to_string(),
        };
        let (gaps_a, gaps_b) = ([gap("a")], [gap("b")]);
        let input = |gaps| PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: region,
            virtual_text: "x x",
            gaps,
        };
        let answer = |text: &str| {
            let map = apply_prepare_result("x x", &layout("x x", &gaps_a), None)
                .unwrap()
                .map;
            Arc::new(PreparedDocument {
                text: text.to_string(),
                map,
            })
        };
        let (a, _) = registry.cell(&target, input(&gaps_a), Holder::LifecyclePass);
        // Both answers have one text; only their maps tell them apart.
        let prepared_a = answer("x0x");
        let _ = a.outcome.set(Some(Arc::clone(&prepared_a)));
        registry.note_sent(&host, "lua", region, &prepared_a);
        let (b, _) = registry.cell(&target, input(&gaps_b), Holder::LifecyclePass);
        let _ = b.outcome.set(Some(answer("x0x")));
        // Undo the gap edit before b was sent: a's answer is sent again.
        let (undone, _) = registry.cell(&target, input(&gaps_a), Holder::LifecyclePass);
        assert!(Arc::ptr_eq(&undone, &a));
        registry.note_sent(&host, "lua", region, &prepared_a);
        let PreparedState::Prepared(Some(map)) = registry.state(&host, "lua", region, "x x") else {
            panic!("prepared");
        };
        assert!(
            Arc::ptr_eq(&map, prepared_a.map.as_ref().unwrap()),
            "the map of the text the server holds, not of the current generation"
        );
    }

    #[test]
    fn only_the_sent_answer_translates() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let region = "01J0000000000000000000000A";
        let input = |text| PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: region,
            virtual_text: text,
            gaps: &[],
        };
        let state = |text| registry.state(&host, "lua", region, text);
        let (old, _) = registry.cell(&target, input("  a"), Holder::LifecyclePass);
        let _ = old.outcome.set(Some(sent_text("a")));
        assert!(
            matches!(state("  a"), PreparedState::Unavailable),
            "answered but held: servers hold nothing of it"
        );
        let sent = Arc::clone(old.outcome.get().unwrap().as_ref().unwrap());
        registry.note_sent(&host, "lua", region, &sent);
        assert!(matches!(state("  a"), PreparedState::Prepared(_)));
        // The next text is answered before the lifecycle pass sends it.
        let (new, _) = registry.cell(&target, input("b"), Holder::LifecyclePass);
        let _ = new.outcome.set(Some(sent_text("b")));
        assert!(
            matches!(state("b"), PreparedState::Unavailable),
            "servers still hold the previous answer"
        );
        assert!(matches!(state("  a"), PreparedState::Prepared(_)));
        // A settings change drops the entry; servers still hold "a".
        registry.retain(|_, _, _, _| false, false);
        assert!(
            matches!(state("  a"), PreparedState::Unavailable),
            "no map is left for the text servers hold"
        );
    }

    #[test]
    fn another_peer_config_never_reuses_an_answer() {
        let registry = PrepareRegistry::default();
        let config = |command: &str| BridgeServerConfig {
            cmd: Some(vec![command.to_string()]),
            ..Default::default()
        };
        let target = |command: &str| PrepareTarget {
            server_name: "peer".to_string(),
            config: Some(Arc::new(config(command))),
        };
        let host = Url::parse("file:///host.md").unwrap();
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: "a",
            gaps: &[],
        };
        let (old, _) = registry.cell(&target("deno"), input, Holder::LifecyclePass);
        let _ = old.outcome.set(None);
        let (new, _) = registry.cell(&target("bun"), input, Holder::LifecyclePass);
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(
            new.outcome.get().is_none(),
            "the old launch's failure is not reused"
        );
        let (again, _) = registry.cell(&target("bun"), input, Holder::LifecyclePass);
        assert!(Arc::ptr_eq(&new, &again));
        // A request under the old config leaves the lifecycle pass's
        // generation in place.
        let (request, _) = registry.cell(&target("deno"), input, Holder::Request);
        assert!(!Arc::ptr_eq(&request, &new));
        let (current, _) = registry.cell(&target("bun"), input, Holder::LifecyclePass);
        assert!(Arc::ptr_eq(&current, &new));
    }

    #[test]
    fn unprepared_sends_are_recorded_once_anything_is_prepared() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let region = "01J0000000000000000000000A";
        assert!(!registry.note_unprepared_sent(&host, "lua", region, "  a"));
        assert!(
            registry.sent_fingerprint(&host, region).is_none(),
            "nothing is recorded before anything is prepared"
        );
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: region,
            virtual_text: "  a",
            gaps: &[],
        };
        registry.cell(&target, input, Holder::LifecyclePass);
        registry.note_sent(&host, "lua", region, &sent_text("a"));
        assert_eq!(
            registry.sent_fingerprint(&host, region),
            Some((
                crate::lsp::bridge::pool::content_fingerprint("a"),
                "lua".to_string()
            ))
        );
        // The pair loses its peer: the virtual text goes out unprepared.
        assert!(
            registry.note_unprepared_sent(&host, "lua", region, "  a"),
            "the prepared text it replaces had pushes of its own"
        );
        assert!(matches!(
            registry.state(&host, "lua", region, "  a"),
            PreparedState::Unprepared
        ));
        assert_eq!(
            registry.sent_fingerprint(&host, region),
            Some((
                crate::lsp::bridge::pool::content_fingerprint("  a"),
                "lua".to_string()
            ))
        );
        assert!(
            !registry.note_unprepared_sent(&host, "lua", region, "  b"),
            "an unprepared edit keeps the coordinates pushes are in"
        );
    }

    #[test]
    fn a_settings_change_drops_final_failures() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: "a",
            gaps: &[],
        };
        let (cell, _) = registry.cell(&target, input, Holder::Request);
        cell.outcome.set(None).unwrap();
        registry.retain(|_, _, _, _| true, false);
        assert_eq!(registry.entries.len(), 1, "nothing would ask again");
        registry.retain(|_, _, _, _| true, true);
        assert!(
            registry.entries.is_empty(),
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
        let target = PrepareTarget {
            server_name: "peer".to_string(),
            config: Some(Arc::new(BridgeServerConfig {
                cmd: Some(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("touch '{}'", marker.display()),
                ]),
                ..Default::default()
            })),
        };
        let host = Url::parse("file:///host.md").unwrap();
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: "a",
            gaps: &[],
        };
        // The attempt is spawned but (on this single-threaded runtime) has
        // not run yet when its entry is forgotten — the host closed, say.
        registry.lookup_or_start(&pool, &target, input);
        let (cell, _) = registry.cell(&target, input, Holder::Request);
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

    #[test]
    fn a_changed_input_gets_a_new_revision_and_cell() {
        let registry = PrepareRegistry::default();
        let target = unstartable_target();
        let host = Url::parse("file:///host.md").unwrap();
        let input = |text| PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: text,
            gaps: &[],
        };
        let (first, revision) = registry.cell(&target, input("a"), Holder::Request);
        let (same, same_revision) = registry.cell(&target, input("a"), Holder::Request);
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(revision, same_revision);
        let (changed, changed_revision) = registry.cell(&target, input("b"), Holder::Request);
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(changed_revision > revision);
        registry.retain(
            |host_language, _, server, _| host_language == "markdown" && server == "peer",
            true,
        );
        assert_eq!(registry.entries.len(), 1);
        registry.retain(|_, _, server, _| server == "other", true);
        assert!(registry.entries.is_empty());
        registry.cell(&target, input("b"), Holder::Request);
        registry.forget_region(&host, None, "01J0000000000000000000000A");
        assert!(registry.entries.is_empty());
        let (_, again) = registry.cell(&target, input("b"), Holder::Request);
        assert!(
            again > changed_revision,
            "a forgotten document never repeats a version"
        );
        registry.forget_region(&host, None, "01J0000000000000000000000A");
        registry.cell(&target, input("b"), Holder::Request);
        registry.forget_region(&host, Some("python"), "01J0000000000000000000000A");
        assert_eq!(registry.entries.len(), 1, "another language's region stays");
        registry.forget_region_except(&host, "01J0000000000000000000000A", Some("lua"));
        assert_eq!(registry.entries.len(), 1, "the current language stays");
        registry.forget_region_except(&host, "01J0000000000000000000000A", Some("ruby"));
        assert!(registry.entries.is_empty());
        registry.cell(&target, input("b"), Holder::Request);
        registry.forget_host(&host);
        assert!(registry.entries.is_empty());
    }
}
