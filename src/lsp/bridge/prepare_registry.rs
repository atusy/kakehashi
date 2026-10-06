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

struct Entry {
    /// The host language and peer the entry was prepared for, so a settings
    /// change that retargets the pair can drop it.
    host_language: String,
    server_name: String,
    /// Identity of the input the cell answers (text, gaps, peer).
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

/// (host URI, injection language, region id).
type EntryKey = (String, String, String);

/// How a virtual document reached downstream servers, as far as the registry
/// knows from its text alone.
#[derive(Debug, Clone)]
pub(crate) enum PreparedState {
    /// Never prepared: sent as is.
    Unprepared,
    /// Sent prepared; `None` when the answer changed nothing.
    Prepared(Option<Arc<super::protocol::PreparedMap>>),
    /// Prepared for another text, or not (successfully) yet: downstream
    /// coordinates for this text are unknown.
    Unavailable,
}

pub(crate) struct PrepareRegistry {
    entries: DashMap<EntryKey, Entry>,
    /// Whether anything was ever prepared: lets the lookups every bridged
    /// region makes per edit skip the map (and its key allocations) when no
    /// pair has a prepare peer.
    ever_used: AtomicBool,
    /// Source of `textDocument.version`: shared by all documents, so a
    /// document forgotten and prepared again (a settings change, a reopen)
    /// never repeats a version the peer saw.
    next_revision: std::sync::atomic::AtomicI32,
    resync_tx: UnboundedSender<Url>,
    resync_rx: std::sync::Mutex<Option<UnboundedReceiver<Url>>>,
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
            entries: DashMap::new(),
            ever_used: AtomicBool::new(false),
            next_revision: std::sync::atomic::AtomicI32::new(1),
            resync_tx,
            resync_rx: std::sync::Mutex::new(Some(resync_rx)),
        }
    }
}

impl PrepareRegistry {
    /// Host documents to sync again: a held-back virtual document became
    /// ready, or an attempt that got no answer finished its retry backoff.
    /// Taken once by the server loop.
    pub(crate) fn take_resync_rx(&self) -> Option<UnboundedReceiver<Url>> {
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
        let (cell, revision) = self.cell(target, input);
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
        let (cell, revision) = self.cell(target, input);
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
        let cell = Arc::clone(cell);
        let pool = Arc::clone(pool);
        let target = target.clone();
        let job = PrepareJob::from(input);
        let resync_tx = self.resync_tx.clone();
        tokio::spawn(async move {
            match run(&pool, &target, &job, revision).await {
                Ok(outcome) => {
                    let prepared = outcome.is_some();
                    let _ = cell.outcome.set(outcome);
                    if prepared {
                        // The lifecycle pass held the document meanwhile.
                        // The receiver is gone only at shutdown.
                        let _ = resync_tx.send(job.host_uri.clone());
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
                    // the next edit; sync the host once the backoff ends.
                    let host_uri = job.host_uri.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        let _ = resync_tx.send(host_uri);
                    });
                }
            }
            cell.in_flight.store(false, Ordering::Release);
            cell.attempts.send_modify(|attempts| *attempts += 1);
        });
        true
    }

    /// How the document with this exact virtual text was sent, for paths
    /// that translate downstream coordinates without the settings that
    /// select a prepare peer (resolve gates, inbound edits).
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
        let Some(entry) = self.entries.get(&(
            host_uri.to_string(),
            injection_language.to_string(),
            region_id.to_string(),
        )) else {
            return PreparedState::Unprepared;
        };
        if entry.text_key != text_key(virtual_text) {
            return PreparedState::Unavailable;
        }
        match entry.cell.outcome.get() {
            Some(Some(prepared)) => PreparedState::Prepared(prepared.map.clone()),
            Some(None) | None => PreparedState::Unavailable,
        }
    }

    /// Keep only the entries `keep(host language, injection language, peer)`
    /// accepts — after a settings change, those whose pair still names the
    /// same peer. A dropped entry reads as unprepared until prepared again.
    pub(crate) fn retain(&self, keep: impl Fn(&str, &str, &str) -> bool) {
        self.entries.retain(|(_, injection_language, _), entry| {
            keep(&entry.host_language, injection_language, &entry.server_name)
        });
    }

    /// Forget one region's document: its pair no longer has a peer, or the
    /// region itself was replaced or invalidated. `injection_language`
    /// `None` forgets the region under every language.
    pub(crate) fn forget_region(
        &self,
        host_uri: &Url,
        injection_language: Option<&str>,
        region_id: &str,
    ) {
        if !self.ever_used.load(Ordering::Acquire) {
            return;
        }
        match injection_language {
            Some(language) => {
                self.entries.remove(&(
                    host_uri.to_string(),
                    language.to_string(),
                    region_id.to_string(),
                ));
            }
            None => self.entries.retain(|(host, _, region), _| {
                host.as_str() != host_uri.as_str() || region != region_id
            }),
        }
    }

    /// Forget a closed host's documents.
    pub(crate) fn forget_host(&self, host_uri: &Url) {
        self.entries
            .retain(|(host, _, _), _| host.as_str() != host_uri.as_str());
    }

    /// The cell answering `input`, replacing a stale one (and bumping the
    /// revision) when the text, gaps or peer changed.
    fn cell(&self, target: &PrepareTarget, input: PrepareInput<'_>) -> (Arc<Cell>, i32) {
        self.ever_used.store(true, Ordering::Release);
        let key = input_key(target, input);
        let mut entry = self
            .entries
            .entry((
                input.host_uri.to_string(),
                input.injection_language.to_string(),
                input.region_id.to_string(),
            ))
            .or_insert_with(|| Entry {
                host_language: input.host_language.to_string(),
                server_name: target.server_name.clone(),
                key,
                text_key: text_key(input.virtual_text),
                revision: self.revision(),
                cell: Arc::default(),
            });
        if entry.key != key {
            entry.host_language = input.host_language.to_string();
            entry.server_name = target.server_name.clone();
            entry.key = key;
            entry.text_key = text_key(input.virtual_text);
            entry.revision = self.revision();
            entry.cell = Arc::default();
        }
        (Arc::clone(&entry.cell), entry.revision)
    }
}

impl PrepareRegistry {
    fn revision(&self) -> i32 {
        // Wraps after 2^31 prepares; the peer only compares a document's
        // versions over its lifetime.
        self.next_revision.fetch_add(1, Ordering::Relaxed)
    }
}

/// Backoff before retrying an attempt that got no answer: one second,
/// doubling, at most a minute.
fn retry_delay(misses: u32) -> Duration {
    Duration::from_secs(1 << misses.saturating_sub(1).min(6))
}

fn input_key(target: &PrepareTarget, input: PrepareInput<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.server_name.hash(&mut hasher);
    input.host_language.hash(&mut hasher);
    input.virtual_text.hash(&mut hasher);
    input.gaps.hash(&mut hasher);
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
    match try_run(pool, target, job, revision).await {
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
            None,
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
        let (cell, _) = registry.cell(&target, input);
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
        let (cell, _) = registry.cell(&target, input);
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
        assert_eq!(resynced, Some(host.clone()));
    }

    #[test]
    fn retries_back_off_to_a_minute() {
        assert_eq!(retry_delay(1), Duration::from_secs(1));
        assert_eq!(retry_delay(2), Duration::from_secs(2));
        assert_eq!(retry_delay(7), Duration::from_secs(64));
        assert_eq!(retry_delay(100), Duration::from_secs(64));
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
        let (first, revision) = registry.cell(&target, input("a"));
        let (same, same_revision) = registry.cell(&target, input("a"));
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(revision, same_revision);
        let (changed, changed_revision) = registry.cell(&target, input("b"));
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(changed_revision > revision);
        registry.retain(|host_language, _, server| host_language == "markdown" && server == "peer");
        assert_eq!(registry.entries.len(), 1);
        registry.retain(|_, _, server| server == "other");
        assert!(registry.entries.is_empty());
        registry.cell(&target, input("b"));
        registry.forget_region(&host, None, "01J0000000000000000000000A");
        assert!(registry.entries.is_empty());
        let (_, again) = registry.cell(&target, input("b"));
        assert!(
            again > changed_revision,
            "a forgotten document never repeats a version"
        );
        registry.forget_region(&host, None, "01J0000000000000000000000A");
        registry.cell(&target, input("b"));
        registry.forget_region(&host, Some("python"), "01J0000000000000000000000A");
        assert_eq!(registry.entries.len(), 1, "another language's region stays");
        registry.forget_host(&host);
        assert!(registry.entries.is_empty());
    }
}
