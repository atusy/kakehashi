use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::language::coordinator::WorkerGrammarDescriptor;
use crate::lsp::text_sync::SequentialByteEdit;
use crate::tree_worker::{
    ApplyDocumentEditsAndDerive, ByteEdit, Client, CloseDocument, DeriveDocumentSnapshot,
    RequestContext, Response, SyncDocument, named_node_count,
};

const SHADOW_ENV: &str = "KAKEHASHI_TREE_WORKER_SHADOW";
const SHADOW_THREADS_ENV: &str = "KAKEHASHI_TREE_WORKER_THREADS";
const SHADOW_QUEUE_CAPACITY: usize = 256;
const SHADOW_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_WORKER_GENERATION: AtomicU64 = AtomicU64::new(1);

enum ShadowCommand {
    Sync(SyncDocument),
    Apply {
        request: ApplyDocumentEditsAndDerive,
        fallback: SyncDocument,
    },
    Close(CloseDocument),
    Shutdown(mpsc::SyncSender<()>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReplicaIdentity {
    incarnation: u64,
    language: String,
    grammar_symbol: String,
    parser_path: PathBuf,
    configuration_generation: u64,
}

impl ReplicaIdentity {
    fn from_sync(request: &SyncDocument) -> Self {
        Self {
            incarnation: request.context.incarnation,
            language: request.language.clone(),
            grammar_symbol: request.grammar_symbol.clone(),
            parser_path: request.parser_path.clone(),
            configuration_generation: request.context.configuration_generation,
        }
    }
}

pub(super) struct TreeWorkerShadow {
    sender: Option<mpsc::SyncSender<ShadowCommand>>,
    accepting: Arc<AtomicBool>,
    disabled: Arc<AtomicBool>,
    next_request_id: AtomicU64,
    worker_generation: u64,
    comparisons: Arc<ComparisonStore>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TreeSummary {
    language: String,
    configuration_generation: u64,
    root_kind: String,
    root_start_byte: usize,
    root_end_byte: usize,
    has_error: bool,
    named_node_count: usize,
}

#[derive(Default)]
struct PendingComparison {
    incarnation: u64,
    content_version: u64,
    configuration_generation: u64,
    authoritative: Option<TreeSummary>,
    shadow: Option<TreeSummary>,
}

#[derive(Default)]
struct ComparisonSlot {
    open_incarnation: u64,
    pending: Option<PendingComparison>,
}

#[derive(Default)]
struct ComparisonStore {
    slots: dashmap::DashMap<String, ComparisonSlot>,
    matched: AtomicU64,
    mismatched: AtomicU64,
    superseded: AtomicU64,
}

impl TreeWorkerShadow {
    pub(super) fn from_environment() -> Self {
        let disabled = Arc::new(AtomicBool::new(false));
        let accepting = Arc::new(AtomicBool::new(false));
        let comparisons = Arc::new(ComparisonStore::default());
        let worker_generation = NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed);
        if !shadow_enabled() {
            return Self {
                sender: None,
                accepting,
                disabled,
                next_request_id: AtomicU64::new(1),
                worker_generation,
                comparisons,
            };
        }
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => {
                log::error!(target: "kakehashi::tree_worker_shadow", "cannot resolve worker executable: {error}");
                disabled.store(true, Ordering::Release);
                return Self {
                    sender: None,
                    accepting,
                    disabled,
                    next_request_id: AtomicU64::new(1),
                    worker_generation,
                    comparisons,
                };
            }
        };
        let compute_threads = configured_compute_threads();
        let (sender, receiver) = mpsc::sync_channel(SHADOW_QUEUE_CAPACITY);
        let actor_disabled = Arc::clone(&disabled);
        let actor_comparisons = Arc::clone(&comparisons);
        let spawn = std::thread::Builder::new()
            .name("kakehashi-tree-worker-shadow".into())
            .spawn(move || {
                run_actor(
                    receiver,
                    executable,
                    compute_threads,
                    worker_generation,
                    actor_disabled,
                    actor_comparisons,
                );
            });
        if let Err(error) = spawn {
            disabled.store(true, Ordering::Release);
            log::error!(
                target: "kakehashi::tree_worker_shadow",
                "disabled shadow worker because actor thread could not spawn: {error}"
            );
            return Self {
                sender: None,
                accepting,
                disabled,
                next_request_id: AtomicU64::new(1),
                worker_generation,
                comparisons,
            };
        }
        accepting.store(true, Ordering::Release);
        log::info!(
            target: "kakehashi::tree_worker_shadow",
            "enabled one shadow worker with {compute_threads} compute threads"
        );
        Self {
            sender: Some(sender),
            accepting,
            disabled,
            next_request_id: AtomicU64::new(1),
            worker_generation,
            comparisons,
        }
    }

    pub(super) fn mirror_full(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        language: String,
        grammar: WorkerGrammarDescriptor,
        text: String,
    ) {
        let request = self.sync_request(uri, incarnation, content_version, language, grammar, text);
        self.comparisons.open(uri.as_str(), incarnation);
        self.submit(ShadowCommand::Sync(request));
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.sender.is_some()
            && self.accepting.load(Ordering::Acquire)
            && !self.disabled.load(Ordering::Acquire)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn mirror_change(
        &self,
        uri: &url::Url,
        incarnation: u64,
        base_version: u64,
        content_version: u64,
        language: String,
        grammar: WorkerGrammarDescriptor,
        text: String,
        edits: Option<Vec<SequentialByteEdit>>,
    ) {
        let fallback =
            self.sync_request(uri, incarnation, content_version, language, grammar, text);
        let Some(edits) = edits else {
            self.submit(ShadowCommand::Sync(fallback));
            return;
        };
        let request = ApplyDocumentEditsAndDerive {
            context: fallback.context.clone(),
            base_version,
            edits: edits
                .into_iter()
                .map(|edit| ByteEdit {
                    start_byte: edit.start_byte,
                    old_end_byte: edit.old_end_byte,
                    new_text: edit.new_text,
                })
                .collect(),
        };
        self.submit(ShadowCommand::Apply { request, fallback });
    }

    pub(super) fn mirror_close(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) {
        self.comparisons.mark_closed(uri.as_str(), incarnation);
        self.submit(ShadowCommand::Close(CloseDocument {
            context: self.context(uri, incarnation, content_version, configuration_generation),
        }));
    }

    pub(super) async fn shutdown(&self) {
        let accepting = Arc::clone(&self.accepting);
        let sender = self.sender.clone();
        let comparisons = Arc::clone(&self.comparisons);
        if let Err(error) = tokio::task::spawn_blocking(move || {
            shutdown_with_timeout(
                &accepting,
                sender.as_ref(),
                &comparisons,
                SHADOW_SHUTDOWN_TIMEOUT,
            );
        })
        .await
        {
            log_incomplete_shutdown(&format!("shutdown task failed: {error}"));
        }
    }

    pub(super) fn record_authoritative_summary(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        summary: TreeSummary,
    ) {
        if !self.is_enabled() {
            return;
        }
        self.comparisons.record(
            uri.as_str(),
            incarnation,
            content_version,
            ComparisonSide::Authoritative(summary),
        );
    }

    fn sync_request(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        language: String,
        grammar: WorkerGrammarDescriptor,
        text: String,
    ) -> SyncDocument {
        SyncDocument {
            context: self.context(
                uri,
                incarnation,
                content_version,
                grammar.configuration_generation,
            ),
            language,
            grammar_symbol: grammar.grammar_symbol,
            parser_path: grammar.parser_path,
            text,
        }
    }

    fn context(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> RequestContext {
        RequestContext {
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            worker_generation: self.worker_generation,
            uri: uri.as_str().to_string(),
            incarnation,
            content_version,
            configuration_generation,
        }
    }

    fn submit(&self, command: ShadowCommand) {
        if !self.accepting.load(Ordering::Acquire) || self.disabled.load(Ordering::Acquire) {
            return;
        }
        let Some(sender) = &self.sender else {
            return;
        };
        if let Err(error) = sender.try_send(command) {
            self.disabled.store(true, Ordering::Release);
            log::error!(
                target: "kakehashi::tree_worker_shadow",
                "disabled shadow worker after bounded queue failure: {error}"
            );
        }
    }

    #[cfg(test)]
    fn shutdown_with_timeout(&self, timeout: Duration) {
        shutdown_with_timeout(
            &self.accepting,
            self.sender.as_ref(),
            &self.comparisons,
            timeout,
        );
    }
}

fn shutdown_with_timeout(
    accepting: &AtomicBool,
    sender: Option<&mpsc::SyncSender<ShadowCommand>>,
    comparisons: &ComparisonStore,
    timeout: Duration,
) {
    if !accepting.swap(false, Ordering::AcqRel) {
        return;
    }
    let deadline = Instant::now() + timeout;
    let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
    let Some(sender) = sender else {
        return;
    };
    let mut command = ShadowCommand::Shutdown(ack_sender);
    loop {
        match sender.try_send(command) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Full(returned)) if Instant::now() < deadline => {
                command = returned;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => {
                log_incomplete_shutdown(&format!("could not enqueue drain request: {error}"));
                return;
            }
        }
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if ack_receiver.recv_timeout(remaining).is_err() {
        log_incomplete_shutdown("timed out waiting for actor drain");
        return;
    }
    log::info!(
        target: "kakehashi::tree_worker_shadow_metrics",
        "shadow comparisons matched={} mismatched={} superseded={} pending={}",
        comparisons.matched.load(Ordering::Relaxed),
        comparisons.mismatched.load(Ordering::Relaxed),
        comparisons.superseded.load(Ordering::Relaxed),
        comparisons.pending_count(),
    );
}

fn log_incomplete_shutdown(reason: &str) {
    log::error!(
        target: "kakehashi::tree_worker_shadow_metrics",
        "shadow validation incomplete: {reason}"
    );
}

fn shadow_enabled() -> bool {
    std::env::var(SHADOW_ENV)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE"))
}

fn configured_compute_threads() -> usize {
    std::env::var(SHADOW_THREADS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|threads| threads.get().min(8))
                .unwrap_or(1)
        })
}

fn run_actor(
    receiver: mpsc::Receiver<ShadowCommand>,
    executable: PathBuf,
    compute_threads: usize,
    worker_generation: u64,
    disabled: Arc<AtomicBool>,
    comparisons: Arc<ComparisonStore>,
) {
    let client = match Client::spawn(&executable, compute_threads, worker_generation) {
        Ok(client) => client,
        Err(error) => {
            disabled.store(true, Ordering::Release);
            log::error!(target: "kakehashi::tree_worker_shadow", "worker spawn failed: {error}");
            return;
        }
    };
    let mut replicas = HashMap::<String, ReplicaIdentity>::new();
    let mut shutdown_ack = None;
    while let Ok(command) = receiver.recv() {
        if let ShadowCommand::Shutdown(ack) = command {
            shutdown_ack = Some(ack);
            break;
        }
        if disabled.load(Ordering::Acquire) {
            break;
        }
        let started = Instant::now();
        let (response, synced_identity) = match command {
            ShadowCommand::Sync(request) => {
                let identity = ReplicaIdentity::from_sync(&request);
                comparisons.open(&request.context.uri, request.context.incarnation);
                (sync_and_derive(&client, request), Some(identity))
            }
            ShadowCommand::Apply { request, fallback } => {
                let uri = fallback.context.uri.clone();
                let identity = ReplicaIdentity::from_sync(&fallback);
                if replica_requires_sync(&replicas, &uri, &identity) {
                    (sync_and_derive(&client, fallback), Some(identity))
                } else {
                    match client.apply_document_edits_and_derive(request) {
                        Ok(Response::Snapshot(snapshot)) => {
                            (Ok(Response::Snapshot(snapshot)), None)
                        }
                        Ok(Response::WorkerRestartRequired(required)) => {
                            (Ok(Response::WorkerRestartRequired(required)), None)
                        }
                        Ok(_) => (sync_and_derive(&client, fallback), Some(identity)),
                        Err(error) => (Err(error), None),
                    }
                }
            }
            ShadowCommand::Close(request) => {
                replicas.remove(&request.context.uri);
                comparisons.mark_closed(&request.context.uri, request.context.incarnation);
                (client.close_document(request), None)
            }
            ShadowCommand::Shutdown(_) => unreachable!(),
        };
        match response {
            Ok(Response::Snapshot(snapshot)) => {
                if let Some(identity) = synced_identity {
                    replicas.insert(snapshot.context.uri.clone(), identity);
                }
                log::debug!(
                    target: "kakehashi::tree_worker_shadow_metrics",
                    "uri={} version={} parent_us={} queue_us={} compute_us={}",
                    snapshot.context.uri,
                    snapshot.context.content_version,
                    started.elapsed().as_micros(),
                    snapshot.queue_wait_ns / 1_000,
                    snapshot.compute_ns / 1_000,
                );
                comparisons.record(
                    &snapshot.context.uri,
                    snapshot.context.incarnation,
                    snapshot.context.content_version,
                    ComparisonSide::Shadow(TreeSummary::from_snapshot(&snapshot)),
                );
            }
            Ok(Response::WorkerRestartRequired(required)) => {
                disabled.store(true, Ordering::Release);
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "disabled shadow worker pending restart: {}",
                    required.reason
                );
                break;
            }
            Ok(Response::Error(error)) => log::warn!(
                target: "kakehashi::tree_worker_shadow",
                "shadow request rejected: {}",
                error.message
            ),
            Ok(_) => {}
            Err(error) => {
                disabled.store(true, Ordering::Release);
                log::error!(target: "kakehashi::tree_worker_shadow", "worker transport failed: {error}");
                break;
            }
        }
    }
    if let Err(error) = client.shutdown() {
        log::warn!(target: "kakehashi::tree_worker_shadow", "worker shutdown failed: {error}");
    }
    if let Some(ack) = shutdown_ack {
        let _ = ack.send(());
    }
}

fn replica_requires_sync(
    replicas: &HashMap<String, ReplicaIdentity>,
    uri: &str,
    desired: &ReplicaIdentity,
) -> bool {
    replicas.get(uri) != Some(desired)
}

enum ComparisonSide {
    Authoritative(TreeSummary),
    Shadow(TreeSummary),
}

impl ComparisonStore {
    fn record(&self, uri: &str, incarnation: u64, content_version: u64, side: ComparisonSide) {
        let incoming_generation = match &side {
            ComparisonSide::Authoritative(summary) | ComparisonSide::Shadow(summary) => {
                summary.configuration_generation
            }
        };
        let incoming = (incarnation, content_version, incoming_generation);
        let Some(mut slot) = self.slots.get_mut(uri) else {
            self.superseded.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if slot.open_incarnation != incarnation {
            self.superseded.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let pending = slot.pending.get_or_insert_with(|| PendingComparison {
            incarnation,
            content_version,
            configuration_generation: incoming_generation,
            ..Default::default()
        });
        let current = (
            pending.incarnation,
            pending.content_version,
            pending.configuration_generation,
        );
        if incoming < current {
            self.superseded.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if incoming > current {
            if pending.authoritative.is_some() || pending.shadow.is_some() {
                self.superseded.fetch_add(1, Ordering::Relaxed);
            }
            *pending = PendingComparison {
                incarnation,
                content_version,
                configuration_generation: incoming_generation,
                ..Default::default()
            };
        }
        match side {
            ComparisonSide::Authoritative(summary) => pending.authoritative = Some(summary),
            ComparisonSide::Shadow(summary) => pending.shadow = Some(summary),
        }
        if pending.authoritative.is_none() || pending.shadow.is_none() {
            return;
        }
        let completed = slot
            .pending
            .take()
            .expect("completed comparison remains in its URI slot");
        let authoritative = completed
            .authoritative
            .expect("completed comparison has an authoritative summary");
        let shadow = completed
            .shadow
            .expect("completed comparison has a shadow summary");
        drop(slot);
        if authoritative == shadow {
            self.matched.fetch_add(1, Ordering::Relaxed);
        } else {
            self.mismatched.fetch_add(1, Ordering::Relaxed);
            log::error!(
                target: "kakehashi::tree_worker_shadow",
                "tree mismatch uri={uri} incarnation={incarnation} version={content_version} authoritative={authoritative:?} shadow={shadow:?}"
            );
        }
    }

    fn mark_closed(&self, uri: &str, incarnation: u64) {
        self.slots.remove_if(uri, |_, slot| {
            if slot.open_incarnation > incarnation {
                return false;
            }
            if slot.pending.is_some() {
                self.superseded.fetch_add(1, Ordering::Relaxed);
            }
            true
        });
    }

    fn open(&self, uri: &str, incarnation: u64) {
        let mut slot = self.slots.entry(uri.to_string()).or_default();
        if slot.open_incarnation < incarnation {
            if slot.pending.is_some() {
                self.superseded.fetch_add(1, Ordering::Relaxed);
            }
            *slot = ComparisonSlot {
                open_incarnation: incarnation,
                pending: None,
            };
        }
    }

    fn pending_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.pending.is_some())
            .count()
    }
}

impl TreeSummary {
    pub(super) fn from_tree(
        language: &str,
        configuration_generation: u64,
        tree: &tree_sitter::Tree,
    ) -> Self {
        let root = tree.root_node();
        Self {
            language: language.to_string(),
            configuration_generation,
            root_kind: root.kind().to_string(),
            root_start_byte: root.start_byte(),
            root_end_byte: root.end_byte(),
            has_error: root.has_error(),
            named_node_count: named_node_count(root),
        }
    }

    fn from_snapshot(snapshot: &crate::tree_worker::DerivedSnapshot) -> Self {
        Self {
            language: snapshot.language.clone(),
            configuration_generation: snapshot.context.configuration_generation,
            root_kind: snapshot.root_kind.clone(),
            root_start_byte: snapshot.root_start_byte,
            root_end_byte: snapshot.root_end_byte,
            has_error: snapshot.has_error,
            named_node_count: snapshot.named_node_count,
        }
    }
}

fn sync_and_derive(client: &Client, request: SyncDocument) -> std::io::Result<Response> {
    let context = request.context.clone();
    match client.sync_document(request)? {
        Response::DocumentAck(_) => {
            client.derive_document_snapshot(DeriveDocumentSnapshot { context })
        }
        response => Ok(response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow(sender: mpsc::SyncSender<ShadowCommand>) -> TreeWorkerShadow {
        TreeWorkerShadow {
            sender: Some(sender),
            accepting: Arc::new(AtomicBool::new(true)),
            disabled: Arc::new(AtomicBool::new(false)),
            next_request_id: AtomicU64::new(1),
            worker_generation: 7,
            comparisons: Arc::new(ComparisonStore::default()),
        }
    }

    fn grammar() -> WorkerGrammarDescriptor {
        WorkerGrammarDescriptor {
            parser_path: "/parser/rust.so".into(),
            grammar_symbol: "rust".into(),
            configuration_generation: 3,
        }
    }

    fn summary(root_kind: &str) -> TreeSummary {
        TreeSummary {
            language: "rust".into(),
            configuration_generation: 3,
            root_kind: root_kind.into(),
            root_start_byte: 0,
            root_end_byte: 10,
            has_error: false,
            named_node_count: 3,
        }
    }

    #[test]
    fn mirror_change_preserves_sequential_edits_and_full_sync_fallback() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let uri = url::Url::parse("file:///example.rs").unwrap();

        shadow.mirror_change(
            &uri,
            2,
            4,
            5,
            "rust".into(),
            grammar(),
            "aXXbYde".into(),
            Some(vec![
                SequentialByteEdit {
                    start_byte: 1,
                    old_end_byte: 1,
                    new_text: "XX".into(),
                },
                SequentialByteEdit {
                    start_byte: 4,
                    old_end_byte: 5,
                    new_text: "Y".into(),
                },
            ]),
        );

        let ShadowCommand::Apply { request, fallback } = receiver.recv().unwrap() else {
            panic!("incremental mirror must enqueue an apply command");
        };
        assert_eq!(request.base_version, 4);
        assert_eq!(request.context.content_version, 5);
        assert_eq!(request.edits[0].new_text, "XX");
        assert_eq!(request.edits[1].start_byte, 4);
        assert_eq!(fallback.text, "aXXbYde");
    }

    #[test]
    fn bounded_queue_failure_disables_the_shadow_session() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let (ack, _receiver) = mpsc::sync_channel(0);
        shadow.submit(ShadowCommand::Shutdown(ack));
        let (ack, _receiver) = mpsc::sync_channel(0);
        shadow.submit(ShadowCommand::Shutdown(ack));

        assert!(!shadow.is_enabled());
    }

    #[test]
    fn shutdown_disables_the_actor_even_when_its_queue_is_full() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let (ack, _receiver) = mpsc::sync_channel(0);
        shadow.submit(ShadowCommand::Shutdown(ack));

        shadow.shutdown_with_timeout(Duration::from_millis(1));

        assert!(!shadow.is_enabled());
    }

    #[test]
    fn replica_identity_change_requires_a_full_sync() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let uri = url::Url::parse("file:///example").unwrap();
        let request =
            shadow.sync_request(&uri, 2, 4, "rust".into(), grammar(), "fn main() {}".into());
        let identity = ReplicaIdentity::from_sync(&request);
        let mut replicas = HashMap::from([(uri.as_str().to_string(), identity.clone())]);

        assert!(!replica_requires_sync(&replicas, uri.as_str(), &identity));

        let mut changed = identity.clone();
        changed.language = "bash".into();
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed));

        changed = identity.clone();
        changed.grammar_symbol = "rust_with_changes".into();
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed));

        changed = identity.clone();
        changed.configuration_generation += 1;
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed));

        replicas.remove(uri.as_str());
        assert!(replica_requires_sync(&replicas, uri.as_str(), &identity));
    }

    #[test]
    fn comparisons_match_regardless_of_arrival_order() {
        for authoritative_first in [true, false] {
            let store = ComparisonStore::default();
            store.open("file:///a.rs", 1);
            let authoritative = ComparisonSide::Authoritative(summary("source_file"));
            let shadow = ComparisonSide::Shadow(summary("source_file"));
            if authoritative_first {
                store.record("file:///a.rs", 1, 2, authoritative);
                store.record("file:///a.rs", 1, 2, shadow);
            } else {
                store.record("file:///a.rs", 1, 2, shadow);
                store.record("file:///a.rs", 1, 2, authoritative);
            }
            assert_eq!(store.matched.load(Ordering::Relaxed), 1);
            assert_eq!(store.pending_count(), 0);
        }
    }

    #[test]
    fn comparisons_keep_only_the_latest_document_version() {
        let store = ComparisonStore::default();
        store.open("file:///a.rs", 1);
        store.record(
            "file:///a.rs",
            1,
            1,
            ComparisonSide::Authoritative(summary("old")),
        );
        store.record(
            "file:///a.rs",
            1,
            2,
            ComparisonSide::Authoritative(summary("new")),
        );
        store.record("file:///a.rs", 1, 1, ComparisonSide::Shadow(summary("old")));
        store.record(
            "file:///a.rs",
            1,
            2,
            ComparisonSide::Shadow(summary("different")),
        );

        assert_eq!(store.matched.load(Ordering::Relaxed), 0);
        assert_eq!(store.mismatched.load(Ordering::Relaxed), 1);
        assert_eq!(store.superseded.load(Ordering::Relaxed), 2);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn comparisons_do_not_pair_across_configuration_generations() {
        let store = ComparisonStore::default();
        store.open("file:///a.rs", 1);
        let authoritative = summary("source_file");
        let mut stale_shadow = authoritative.clone();
        stale_shadow.configuration_generation -= 1;

        store.record(
            "file:///a.rs",
            1,
            2,
            ComparisonSide::Authoritative(authoritative.clone()),
        );
        store.record("file:///a.rs", 1, 2, ComparisonSide::Shadow(stale_shadow));
        store.record("file:///a.rs", 1, 2, ComparisonSide::Shadow(authoritative));

        assert_eq!(store.matched.load(Ordering::Relaxed), 1);
        assert_eq!(store.mismatched.load(Ordering::Relaxed), 0);
        assert_eq!(store.superseded.load(Ordering::Relaxed), 1);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn closed_incarnation_rejects_late_results_until_reopen() {
        let store = ComparisonStore::default();
        let uri = "file:///a.rs";
        store.open(uri, 1);
        store.mark_closed(uri, 1);

        store.record(uri, 1, 2, ComparisonSide::Authoritative(summary("late")));
        assert_eq!(store.pending_count(), 0);
        assert_eq!(store.superseded.load(Ordering::Relaxed), 1);

        store.open(uri, 2);
        store.record(
            uri,
            2,
            0,
            ComparisonSide::Authoritative(summary("source_file")),
        );
        store.record(uri, 2, 0, ComparisonSide::Shadow(summary("source_file")));
        assert_eq!(store.matched.load(Ordering::Relaxed), 1);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn duplicate_close_keeps_the_uri_closed() {
        let store = ComparisonStore::default();
        let uri = "file:///a.rs";
        store.open(uri, 1);
        store.mark_closed(uri, 1);
        store.mark_closed(uri, 1);

        assert!(!store.slots.contains_key(uri));
    }

    #[test]
    fn delayed_old_close_preserves_reopened_incarnation_comparison() {
        let store = ComparisonStore::default();
        let uri = "file:///a.rs";
        store.open(uri, 2);
        store.record(
            uri,
            2,
            0,
            ComparisonSide::Authoritative(summary("source_file")),
        );

        store.mark_closed(uri, 1);
        store.record(uri, 1, 9, ComparisonSide::Shadow(summary("stale")));
        store.record(uri, 2, 0, ComparisonSide::Shadow(summary("source_file")));

        assert_eq!(store.matched.load(Ordering::Relaxed), 1);
        assert_eq!(store.mismatched.load(Ordering::Relaxed), 0);
        assert_eq!(store.superseded.load(Ordering::Relaxed), 1);
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn closed_comparison_slots_are_removed() {
        let store = ComparisonStore::default();
        for index in 0..=4_096 {
            let uri = format!("file:///{index}.rs");
            store.open(&uri, 1);
            store.mark_closed(&uri, 1);
        }

        assert!(store.slots.is_empty());
    }
}
