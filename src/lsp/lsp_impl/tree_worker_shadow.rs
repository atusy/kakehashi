use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use crate::language::coordinator::WorkerGrammarDescriptor;
use crate::lsp::text_sync::SequentialByteEdit;
use crate::tree_worker::{
    ApplyDocumentEditsAndDerive, ByteEdit, Client, CloseDocument, DeriveDocumentSnapshot,
    RequestContext, Response, SyncDocument,
};

const SHADOW_ENV: &str = "KAKEHASHI_TREE_WORKER_SHADOW";
const SHADOW_THREADS_ENV: &str = "KAKEHASHI_TREE_WORKER_THREADS";
const SHADOW_QUEUE_CAPACITY: usize = 256;
const CLOSED_COMPARISON_TOMBSTONES: usize = 4_096;
static NEXT_WORKER_GENERATION: AtomicU64 = AtomicU64::new(1);

enum ShadowCommand {
    Sync(SyncDocument),
    Apply {
        request: ApplyDocumentEditsAndDerive,
        fallback: SyncDocument,
    },
    Close(CloseDocument),
    Shutdown,
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
    disabled: Arc<AtomicBool>,
    next_request_id: AtomicU64,
    worker_generation: u64,
    comparisons: Arc<ComparisonStore>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeSummary {
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
    closed_incarnation: Option<u64>,
    pending: Option<PendingComparison>,
}

#[derive(Default)]
struct ComparisonStore {
    slots: dashmap::DashMap<String, ComparisonSlot>,
    closed_order: Mutex<VecDeque<(String, u64)>>,
    matched: AtomicU64,
    mismatched: AtomicU64,
    superseded: AtomicU64,
}

impl TreeWorkerShadow {
    pub(super) fn from_environment() -> Self {
        let disabled = Arc::new(AtomicBool::new(false));
        let comparisons = Arc::new(ComparisonStore::default());
        let worker_generation = NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed);
        if !shadow_enabled() {
            return Self {
                sender: None,
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
        std::thread::Builder::new()
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
            })
            .expect("tree worker shadow actor thread must spawn");
        log::info!(
            target: "kakehashi::tree_worker_shadow",
            "enabled one shadow worker with {compute_threads} compute threads"
        );
        Self {
            sender: Some(sender),
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
        self.comparisons.reopen(uri.as_str(), incarnation);
        self.submit(ShadowCommand::Sync(request));
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.sender.is_some() && !self.disabled.load(Ordering::Acquire)
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

    pub(super) fn shutdown(&self) {
        log::info!(
            target: "kakehashi::tree_worker_shadow_metrics",
            "shadow comparisons matched={} mismatched={} superseded={} pending={}",
            self.comparisons.matched.load(Ordering::Relaxed),
            self.comparisons.mismatched.load(Ordering::Relaxed),
            self.comparisons.superseded.load(Ordering::Relaxed),
            self.comparisons.pending_count(),
        );
        self.disabled.store(true, Ordering::Release);
        if let Some(sender) = &self.sender {
            let _ = sender.try_send(ShadowCommand::Shutdown);
        }
    }

    pub(super) fn record_authoritative(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        language: &str,
        configuration_generation: u64,
        tree: &tree_sitter::Tree,
    ) {
        if !self.is_enabled() {
            return;
        }
        self.comparisons.record(
            uri.as_str(),
            incarnation,
            content_version,
            ComparisonSide::Authoritative(TreeSummary::from_tree(
                language,
                configuration_generation,
                tree,
            )),
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
        if self.disabled.load(Ordering::Acquire) {
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
    while let Ok(command) = receiver.recv() {
        if disabled.load(Ordering::Acquire) || matches!(command, ShadowCommand::Shutdown) {
            break;
        }
        let started = Instant::now();
        let (response, synced_identity) = match command {
            ShadowCommand::Sync(request) => {
                let identity = ReplicaIdentity::from_sync(&request);
                comparisons.reopen(&request.context.uri, request.context.incarnation);
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
            ShadowCommand::Shutdown => unreachable!(),
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
        let mut slot = if let Some(slot) = self.slots.get_mut(uri) {
            slot
        } else {
            self.slots.entry(uri.to_string()).or_default()
        };
        if slot
            .closed_incarnation
            .is_some_and(|closed| closed >= incarnation)
        {
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
        let mut slot = if let Some(slot) = self.slots.get_mut(uri) {
            slot
        } else {
            self.slots.entry(uri.to_string()).or_default()
        };
        if slot
            .closed_incarnation
            .is_some_and(|closed| closed >= incarnation)
        {
            return;
        }
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| pending.incarnation <= incarnation)
        {
            self.superseded.fetch_add(1, Ordering::Relaxed);
            slot.pending = None;
        }
        slot.closed_incarnation = Some(incarnation);
        drop(slot);

        use crate::error::LockResultExt;
        let mut order = self
            .closed_order
            .lock()
            .recover_poison("TreeWorkerShadow::mark_closed");
        order.push_back((uri.to_string(), incarnation));
        while order.len() > CLOSED_COMPARISON_TOMBSTONES {
            let Some((expired_uri, expired_incarnation)) = order.pop_front() else {
                break;
            };
            self.slots.remove_if(&expired_uri, |_, slot| {
                slot.closed_incarnation == Some(expired_incarnation) && slot.pending.is_none()
            });
        }
    }

    fn reopen(&self, uri: &str, incarnation: u64) {
        let Some(mut slot) = self.slots.get_mut(uri) else {
            return;
        };
        if slot
            .pending
            .as_ref()
            .is_some_and(|pending| pending.incarnation < incarnation)
        {
            slot.pending = None;
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
    fn from_tree(language: &str, configuration_generation: u64, tree: &tree_sitter::Tree) -> Self {
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

fn named_node_count(root: tree_sitter::Node<'_>) -> usize {
    let mut count = 0;
    let mut cursor = root.walk();
    let mut ascending = false;
    loop {
        if !ascending {
            count += usize::from(cursor.node().is_named());
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            ascending = false;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        ascending = true;
    }
    count
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
        shadow.submit(ShadowCommand::Shutdown);
        shadow.submit(ShadowCommand::Shutdown);

        assert!(!shadow.is_enabled());
    }

    #[test]
    fn shutdown_disables_the_actor_even_when_its_queue_is_full() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        shadow.submit(ShadowCommand::Shutdown);

        shadow.shutdown();

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
        store.mark_closed(uri, 1);

        store.record(uri, 1, 2, ComparisonSide::Authoritative(summary("late")));
        assert_eq!(store.pending_count(), 0);
        assert_eq!(store.superseded.load(Ordering::Relaxed), 1);

        store.reopen(uri, 2);
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
    fn duplicate_close_keeps_one_live_tombstone() {
        use crate::error::LockResultExt;

        let store = ComparisonStore::default();
        let uri = "file:///a.rs";
        store.mark_closed(uri, 1);
        store.mark_closed(uri, 1);

        assert_eq!(
            store
                .closed_order
                .lock()
                .recover_poison("duplicate close test")
                .len(),
            1
        );
        let slot = store.slots.get(uri).unwrap();
        assert_eq!(slot.closed_incarnation, Some(1));
    }

    #[test]
    fn delayed_old_close_preserves_reopened_incarnation_comparison() {
        let store = ComparisonStore::default();
        let uri = "file:///a.rs";
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
    fn closed_comparison_tombstones_stay_bounded() {
        let store = ComparisonStore::default();
        for index in 0..=CLOSED_COMPARISON_TOMBSTONES {
            store.mark_closed(&format!("file:///{index}.rs"), 1);
        }

        let retained = store
            .slots
            .iter()
            .filter(|slot| slot.closed_incarnation.is_some())
            .count();
        assert!(retained <= CLOSED_COMPARISON_TOMBSTONES);
    }
}
