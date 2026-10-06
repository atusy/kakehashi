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
//! again. Requests are already asynchronous and simply wait for the answer.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
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
    pub(crate) config: Arc<BridgeServerConfig>,
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

/// `None` is a failed prepare.
type Outcome = Option<Arc<PreparedDocument>>;

struct Entry {
    /// Identity of the input the cell answers (text, gaps, peer).
    key: u64,
    /// Identity of the virtual text alone, for lookups that only know it.
    text_key: u64,
    /// The revision sent as `textDocument.version`.
    revision: i32,
    outcome: Arc<OnceCell<Outcome>>,
    /// Whether a lookup already started the request for this revision.
    started: Arc<AtomicBool>,
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
            resync_tx,
            resync_rx: std::sync::Mutex::new(Some(resync_rx)),
        }
    }
}

impl PrepareRegistry {
    /// Host documents whose held-back virtual documents became ready (or
    /// failed) and must be synced again. Taken once by the server loop.
    pub(crate) fn take_resync_rx(&self) -> Option<UnboundedReceiver<Url>> {
        self.resync_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Look up without waiting; on a miss, start the request in the
    /// background and report [`PrepareLookup::Pending`].
    pub(crate) fn lookup_or_start(
        &self,
        pool: &Arc<LanguageServerPool>,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        let (outcome, started, revision) = self.cell(target, input);
        if let Some(outcome) = outcome.get() {
            return match outcome {
                Some(prepared) => PrepareLookup::Ready(Arc::clone(prepared)),
                None => PrepareLookup::Failed,
            };
        }
        if !started.swap(true, Ordering::AcqRel) {
            let pool = Arc::clone(pool);
            let target = target.clone();
            let job = PrepareJob::from(input);
            let resync_tx = self.resync_tx.clone();
            tokio::spawn(async move {
                outcome
                    .get_or_init(|| run(&pool, &target, &job, revision))
                    .await;
                // The receiver is gone only at shutdown.
                let _ = resync_tx.send(job.host_uri);
            });
        }
        PrepareLookup::Pending
    }

    /// Wait for the prepared document, sharing a request already in flight.
    /// `None` when the peer could not prepare it.
    pub(crate) async fn prepare(
        &self,
        pool: &LanguageServerPool,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> Outcome {
        let (outcome, started, revision) = self.cell(target, input);
        started.store(true, Ordering::Release);
        let job = PrepareJob::from(input);
        outcome
            .get_or_init(|| run(pool, target, &job, revision))
            .await
            .clone()
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
        match entry.outcome.get() {
            Some(Some(prepared)) => PreparedState::Prepared(prepared.map.clone()),
            Some(None) | None => PreparedState::Unavailable,
        }
    }

    /// Forget a closed host's documents.
    pub(crate) fn forget_host(&self, host_uri: &Url) {
        self.entries
            .retain(|(host, _, _), _| host.as_str() != host_uri.as_str());
    }

    /// The cell answering `input`, replacing a stale one (and bumping the
    /// revision) when the text, gaps or peer changed.
    fn cell(
        &self,
        target: &PrepareTarget,
        input: PrepareInput<'_>,
    ) -> (Arc<OnceCell<Outcome>>, Arc<AtomicBool>, i32) {
        let key = input_key(target, input);
        let mut entry = self
            .entries
            .entry((
                input.host_uri.to_string(),
                input.injection_language.to_string(),
                input.region_id.to_string(),
            ))
            .or_insert_with(|| Entry {
                key,
                text_key: text_key(input.virtual_text),
                revision: 1,
                outcome: Arc::new(OnceCell::new()),
                started: Arc::new(AtomicBool::new(false)),
            });
        if entry.key != key {
            entry.key = key;
            entry.text_key = text_key(input.virtual_text);
            entry.revision = entry.revision.saturating_add(1);
            entry.outcome = Arc::new(OnceCell::new());
            entry.started = Arc::new(AtomicBool::new(false));
        }
        (
            Arc::clone(&entry.outcome),
            Arc::clone(&entry.started),
            entry.revision,
        )
    }
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

/// Ask the peer and apply its answer. Every failure is logged and yields
/// `None`: the document is then not sent, never sent unprepared.
async fn run(
    pool: &LanguageServerPool,
    target: &PrepareTarget,
    job: &PrepareJob,
    revision: i32,
) -> Outcome {
    match try_run(pool, target, job, revision).await {
        Ok(prepared) => Some(Arc::new(prepared)),
        Err(error) => {
            log::warn!(
                target: "kakehashi::bridge::prepare",
                "Not sending the {} virtual document of {}: {} could not prepare it: {}",
                job.injection_language,
                job.host_uri,
                target.server_name,
                error
            );
            None
        }
    }
}

async fn try_run(
    pool: &LanguageServerPool,
    target: &PrepareTarget,
    job: &PrepareJob,
    revision: i32,
) -> std::io::Result<PreparedDocument> {
    let handle = pool
        .get_or_create_connection_wait_ready_admitted(
            &target.server_name,
            &target.config,
            Some(&job.host_uri),
            Duration::from_secs(super::INIT_TIMEOUT_SECS),
            None,
            None,
        )
        .await?;
    let host_uri = crate::lsp::lsp_impl::url_to_uri(&job.host_uri)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
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
    let result = handle.request_virtual_document_prepare(&params).await?;
    apply_prepare_result(&job.virtual_text, &layout, result)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
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

    #[test]
    fn a_changed_input_gets_a_new_revision_and_cell() {
        let registry = PrepareRegistry::default();
        let target = PrepareTarget {
            server_name: "peer".to_string(),
            config: Arc::new(BridgeServerConfig::default()),
        };
        let host = Url::parse("file:///host.md").unwrap();
        let input = |text| PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: "01J0000000000000000000000A",
            virtual_text: text,
            gaps: &[],
        };
        let (first, _, revision) = registry.cell(&target, input("a"));
        let (same, _, same_revision) = registry.cell(&target, input("a"));
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(revision, same_revision);
        let (changed, _, changed_revision) = registry.cell(&target, input("b"));
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(changed_revision, revision + 1);
        registry.forget_host(&host);
        assert!(registry.entries.is_empty());
    }
}
