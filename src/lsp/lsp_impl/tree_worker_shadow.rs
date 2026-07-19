use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::error::LockResultExt;
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
const MAX_SYSTEMIC_RESTARTS: u8 = 3;
const MAX_NATIVE_RESTARTS: u8 = 8;
const MAX_SESSION_RESTARTS: u8 = 16;
const INITIAL_RESTART_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(300);
const WORKER_LIVENESS_POLL: Duration = Duration::from_millis(250);
const FAST_HEALTHY_INTERVAL: Duration = Duration::from_secs(60);
const LONG_HEALTHY_INTERVAL: Duration = Duration::from_secs(10 * 60);
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
    content_version: u64,
    language: String,
    grammar_symbol: String,
    source_path: PathBuf,
    parser_path: PathBuf,
    artifact_digest: String,
    configuration_generation: u64,
}

impl ReplicaIdentity {
    fn from_sync(request: &SyncDocument) -> Self {
        Self {
            incarnation: request.context.incarnation,
            content_version: request.context.content_version,
            language: request.language.clone(),
            grammar_symbol: request.grammar_symbol.clone(),
            source_path: request.source_path.clone(),
            parser_path: request.parser_path.clone(),
            artifact_digest: request.artifact_digest.clone(),
            configuration_generation: request.context.configuration_generation,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct GrammarIdentity {
    grammar_symbol: String,
    artifact_digest: String,
}

impl GrammarIdentity {
    fn from_sync(request: &SyncDocument) -> Self {
        Self {
            grammar_symbol: request.grammar_symbol.clone(),
            artifact_digest: request.artifact_digest.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureClass {
    Systemic,
    NativeEvidenced,
}

struct RestartBudget {
    systemic: u8,
    native: u8,
    tokens: u8,
    backoff_count: u8,
}

impl Default for RestartBudget {
    fn default() -> Self {
        Self {
            systemic: 0,
            native: 0,
            tokens: MAX_SESSION_RESTARTS,
            backoff_count: 0,
        }
    }
}

impl RestartBudget {
    fn consume(&mut self, class: FailureClass) -> bool {
        let class_available = match class {
            FailureClass::Systemic => self.systemic < MAX_SYSTEMIC_RESTARTS,
            FailureClass::NativeEvidenced => self.native < MAX_NATIVE_RESTARTS,
        };
        if self.tokens == 0 || !class_available {
            return false;
        }
        match class {
            FailureClass::Systemic => self.systemic = self.systemic.saturating_add(1),
            FailureClass::NativeEvidenced => self.native = self.native.saturating_add(1),
        }
        self.tokens -= 1;
        self.backoff_count = self.backoff_count.saturating_add(1);
        true
    }

    fn consume_half_open(&mut self, class: FailureClass) {
        if self.tokens == 0 {
            self.tokens = 1;
        }
        self.tokens -= 1;
        self.backoff_count = self.backoff_count.saturating_add(1);
        match class {
            FailureClass::Systemic => self.systemic = self.systemic.saturating_add(1),
            FailureClass::NativeEvidenced => self.native = self.native.saturating_add(1),
        }
    }

    fn reset_fast(&mut self) {
        self.systemic = 0;
        self.native = 0;
    }

    fn restore_long(&mut self, intervals: u64) {
        let restored = intervals.min(u64::from(MAX_SESSION_RESTARTS - self.tokens)) as u8;
        self.tokens = self.tokens.saturating_add(restored);
        self.backoff_count = 0;
    }

    fn tokens_remaining(&self) -> u8 {
        self.tokens
    }

    fn backoff(&self) -> Duration {
        let exponent = self.backoff_count.saturating_sub(1).min(10) as u32;
        INITIAL_RESTART_BACKOFF
            .saturating_mul(1_u32 << exponent)
            .min(MAX_RESTART_BACKOFF)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BreakerState {
    Closed,
    Open {
        opened_at: Instant,
        baseline_generation: u64,
        cooldown: Duration,
    },
    HalfOpen {
        probe_generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryMode {
    Normal,
    Planned,
    HalfOpen,
}

#[derive(Clone, Default)]
struct OpenDocuments {
    current: HashMap<String, SyncDocument>,
}

struct SupervisorState {
    client: Option<Client>,
    worker_generation: u64,
    replicas: HashMap<String, ReplicaIdentity>,
    open_documents: Arc<Mutex<OpenDocuments>>,
    quarantined: HashSet<GrammarIdentity>,
    restart_budget: RestartBudget,
    breaker: BreakerState,
    healthy_since: Option<Instant>,
    long_healthy_since: Option<Instant>,
}

struct ActorShared {
    disabled: Arc<AtomicBool>,
    comparisons: Arc<ComparisonStore>,
    pending_configuration_generation: Arc<AtomicU64>,
    open_documents: Arc<Mutex<OpenDocuments>>,
    registry_epoch: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ResyncStats {
    documents: usize,
    bytes: usize,
}

struct ResyncFailure {
    error: std::io::Error,
    class: FailureClass,
    implicated_grammar: Option<GrammarIdentity>,
}

impl OpenDocuments {
    fn supersedes(&self, command: &ShadowCommand) -> bool {
        match command {
            ShadowCommand::Sync(incoming)
            | ShadowCommand::Apply {
                fallback: incoming, ..
            } => self
                .current
                .get(&incoming.context.uri)
                .is_some_and(|current| {
                    current.context.incarnation > incoming.context.incarnation
                        || (current.context.incarnation == incoming.context.incarnation
                            && (current.context.configuration_generation
                                > incoming.context.configuration_generation
                                || (current.context.configuration_generation
                                    == incoming.context.configuration_generation
                                    && current.context.content_version
                                        > incoming.context.content_version)))
                }),
            ShadowCommand::Close(request) => self
                .current
                .get(&request.context.uri)
                .is_some_and(|current| current.context.incarnation > request.context.incarnation),
            ShadowCommand::Shutdown(_) => false,
        }
    }

    fn observe(&mut self, command: &ShadowCommand) -> Observation {
        if let ShadowCommand::Close(request) = command
            && self
                .current
                .get(&request.context.uri)
                .is_some_and(|current| current.context.incarnation > request.context.incarnation)
        {
            return Observation::Stale;
        }
        let incoming = match command {
            ShadowCommand::Sync(request) => Some(request),
            ShadowCommand::Apply { fallback, .. } => Some(fallback),
            ShadowCommand::Close(_) | ShadowCommand::Shutdown(_) => None,
        };
        if incoming.is_some_and(|incoming| {
            self.current.values().any(|current| {
                (current.source_path == incoming.source_path
                    && current.context.configuration_generation
                        > incoming.context.configuration_generation)
                    || (current.context.uri == incoming.context.uri
                        && (current.context.incarnation > incoming.context.incarnation
                            || (current.context.incarnation == incoming.context.incarnation
                                && (current.context.configuration_generation
                                    > incoming.context.configuration_generation
                                    || (current.context.configuration_generation
                                        == incoming.context.configuration_generation
                                        && current.context.content_version
                                            > incoming.context.content_version)))))
            })
        }) {
            return Observation::Stale;
        }
        let artifact_replaced = incoming.is_some_and(|incoming| {
            self.current.values().any(|current| {
                current.source_path == incoming.source_path
                    && current.artifact_digest != incoming.artifact_digest
            })
        });
        if artifact_replaced {
            let incoming = incoming.expect("replacement requires an incoming document");
            for current in self.current.values_mut() {
                if current.source_path == incoming.source_path {
                    current.parser_path.clone_from(&incoming.parser_path);
                    current
                        .artifact_digest
                        .clone_from(&incoming.artifact_digest);
                    current.context.configuration_generation =
                        incoming.context.configuration_generation;
                }
            }
        }
        match command {
            ShadowCommand::Sync(request) => {
                self.current
                    .insert(request.context.uri.clone(), request.clone());
            }
            ShadowCommand::Apply { fallback, .. } => {
                self.current
                    .insert(fallback.context.uri.clone(), fallback.clone());
            }
            ShadowCommand::Close(request) => {
                self.current.remove(&request.context.uri);
            }
            ShadowCommand::Shutdown(_) => {}
        }
        Observation::Accepted { artifact_replaced }
    }

    fn grammar_for(&self, uri: &str) -> Option<GrammarIdentity> {
        self.current.get(uri).map(GrammarIdentity::from_sync)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Observation {
    Accepted { artifact_replaced: bool },
    Stale,
}

pub(super) struct TreeWorkerShadow {
    sender: Option<mpsc::SyncSender<ShadowCommand>>,
    accepting: Arc<AtomicBool>,
    disabled: Arc<AtomicBool>,
    next_request_id: AtomicU64,
    worker_generation: u64,
    comparisons: Arc<ComparisonStore>,
    pending_configuration_generation: Arc<AtomicU64>,
    open_documents: Arc<Mutex<OpenDocuments>>,
    registry_epoch: Arc<AtomicU64>,
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

pub(super) struct MirrorDocument {
    pub(super) uri: url::Url,
    pub(super) incarnation: u64,
    pub(super) content_version: u64,
    pub(super) language: String,
    pub(super) grammar: Option<WorkerGrammarDescriptor>,
    pub(super) text: String,
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
    completed: Option<(u64, u64, u64)>,
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
        let pending_configuration_generation = Arc::new(AtomicU64::new(0));
        let open_documents = Arc::new(Mutex::new(OpenDocuments::default()));
        let registry_epoch = Arc::new(AtomicU64::new(0));
        let worker_generation = NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed);
        if !shadow_enabled() {
            return Self {
                sender: None,
                accepting,
                disabled,
                next_request_id: AtomicU64::new(1),
                worker_generation,
                comparisons,
                pending_configuration_generation,
                open_documents,
                registry_epoch,
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
                    pending_configuration_generation,
                    open_documents,
                    registry_epoch,
                };
            }
        };
        let compute_threads = configured_compute_threads();
        let (sender, receiver) = mpsc::sync_channel(SHADOW_QUEUE_CAPACITY);
        let actor_shared = ActorShared {
            disabled: Arc::clone(&disabled),
            comparisons: Arc::clone(&comparisons),
            pending_configuration_generation: Arc::clone(&pending_configuration_generation),
            open_documents: Arc::clone(&open_documents),
            registry_epoch: Arc::clone(&registry_epoch),
        };
        let spawn = std::thread::Builder::new()
            .name("kakehashi-tree-worker-shadow".into())
            .spawn(move || {
                run_actor(
                    receiver,
                    executable,
                    compute_threads,
                    worker_generation,
                    actor_shared,
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
                pending_configuration_generation,
                open_documents,
                registry_epoch,
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
            pending_configuration_generation,
            open_documents,
            registry_epoch,
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
        self.observe_and_submit(ShadowCommand::Sync(request));
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.is_configured() && !self.disabled.load(Ordering::Acquire)
    }

    pub(super) fn is_configured(&self) -> bool {
        self.sender.is_some() && self.accepting.load(Ordering::Acquire)
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
            self.observe_and_submit(ShadowCommand::Sync(fallback));
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
        self.observe_and_submit(ShadowCommand::Apply { request, fallback });
    }

    pub(super) fn mirror_close(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) {
        self.comparisons.mark_closed(uri.as_str(), incarnation);
        self.observe_and_submit(ShadowCommand::Close(CloseDocument {
            context: self.context(uri, incarnation, content_version, configuration_generation),
        }));
    }

    pub(super) fn configuration_changed(&self, generation: u64) {
        self.pending_configuration_generation
            .fetch_max(generation, Ordering::AcqRel);
    }

    pub(super) fn mirror_configuration(&self, generation: u64, documents: Vec<MirrorDocument>) {
        let mut commands = Vec::with_capacity(documents.len());
        for document in documents {
            let command = if let Some(grammar) = document.grammar {
                ShadowCommand::Sync(self.sync_request(
                    &document.uri,
                    document.incarnation,
                    document.content_version,
                    document.language,
                    grammar,
                    document.text,
                ))
            } else {
                ShadowCommand::Close(CloseDocument {
                    context: self.context(
                        &document.uri,
                        document.incarnation,
                        document.content_version,
                        generation,
                    ),
                })
            };
            commands.push(command);
        }
        {
            let mut registry = self
                .open_documents
                .lock()
                .recover_poison("TreeWorkerShadow::mirror_configuration");
            commands.retain(|command| registry.observe(command) != Observation::Stale);
        }
        if !commands.is_empty() {
            self.registry_epoch.fetch_add(1, Ordering::AcqRel);
        }
        for command in &commands {
            if let Some((uri, incarnation)) = command_document(command) {
                if matches!(command, ShadowCommand::Close(_)) {
                    self.comparisons.mark_closed(uri, incarnation);
                } else {
                    self.comparisons.open(uri, incarnation);
                }
            }
        }
        self.configuration_changed(generation);
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        for command in commands {
            self.submit(command);
        }
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
            source_path: grammar.source_path,
            parser_path: grammar.parser_path,
            artifact_digest: grammar.artifact_digest,
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
        if !self.accepting.load(Ordering::Acquire) {
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

    fn observe_and_submit(&self, command: ShadowCommand) {
        let observation = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::observe_and_submit")
            .observe(&command);
        if observation == Observation::Stale {
            return;
        }
        self.registry_epoch.fetch_add(1, Ordering::AcqRel);
        if self.disabled.load(Ordering::Acquire) {
            return;
        }
        self.submit(command);
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
    let pending_uris = comparisons.pending_uris();
    if !pending_uris.is_empty() {
        log::warn!(
            target: "kakehashi::tree_worker_shadow_metrics",
            "shadow comparisons still pending for URIs: {pending_uris:?}",
        );
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
    shared: ActorShared,
) {
    let ActorShared {
        disabled,
        comparisons,
        pending_configuration_generation,
        open_documents,
        registry_epoch,
    } = shared;
    let initial_client = match Client::spawn(&executable, compute_threads, worker_generation) {
        Ok(client) => Some(client),
        Err(error) => {
            log::error!(target: "kakehashi::tree_worker_shadow", "worker spawn failed: {error}");
            None
        }
    };
    let initial_healthy_since = initial_client.as_ref().map(|_| Instant::now());
    let mut state = SupervisorState {
        client: initial_client,
        worker_generation,
        replicas: HashMap::new(),
        open_documents,
        quarantined: HashSet::new(),
        restart_budget: RestartBudget::default(),
        breaker: BreakerState::Closed,
        healthy_since: initial_healthy_since,
        long_healthy_since: initial_healthy_since,
    };
    if state.client.is_none()
        && !recover_worker(
            &mut state,
            &executable,
            compute_threads,
            FailureClass::Systemic,
            None,
            &comparisons,
            RecoveryMode::Normal,
        )
    {
        mark_tree_tier_unavailable(&mut state, &disabled, &pending_configuration_generation);
    }
    let mut shutdown_ack = None;
    loop {
        if disabled.load(Ordering::Acquire) && !matches!(state.breaker, BreakerState::Open { .. }) {
            mark_tree_tier_unavailable(&mut state, &disabled, &pending_configuration_generation);
        }
        if disabled.load(Ordering::Acquire) {
            let generation = pending_configuration_generation.load(Ordering::Acquire);
            let probe_allowed = breaker_probe_allowed(state.breaker, generation, Instant::now());
            if probe_allowed {
                let recovery_epoch = registry_epoch.load(Ordering::Acquire);
                state.breaker = BreakerState::HalfOpen {
                    probe_generation: generation,
                };
                if recover_worker(
                    &mut state,
                    &executable,
                    compute_threads,
                    FailureClass::Systemic,
                    None,
                    &comparisons,
                    RecoveryMode::HalfOpen,
                ) && registry_epoch.load(Ordering::Acquire) == recovery_epoch
                {
                    disabled.store(false, Ordering::Release);
                    log::info!(
                        target: "kakehashi::tree_worker_shadow",
                        "entered shadow worker half-open probation for configuration generation {generation}",
                    );
                } else {
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                }
            }
        }
        let mut command = match receiver.recv_timeout(WORKER_LIVENESS_POLL) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(client) = state.client.as_ref() else {
                    continue;
                };
                let status = client.try_wait();
                match status {
                    Ok(None) => {
                        observe_healthy_service(&mut state, Instant::now());
                        continue;
                    }
                    Ok(Some(status)) => log::error!(
                        target: "kakehashi::tree_worker_shadow",
                        "worker generation {} exited while idle: {status}",
                        state.worker_generation,
                    ),
                    Err(error) => log::error!(
                        target: "kakehashi::tree_worker_shadow",
                        "worker generation {} liveness check failed: {error}",
                        state.worker_generation,
                    ),
                }
                if matches!(state.breaker, BreakerState::HalfOpen { .. })
                    || !recover_worker(
                        &mut state,
                        &executable,
                        compute_threads,
                        FailureClass::Systemic,
                        None,
                        &comparisons,
                        RecoveryMode::Normal,
                    )
                {
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let ShadowCommand::Shutdown(ack) = command {
            shutdown_ack = Some(ack);
            break;
        }
        if disabled.load(Ordering::Acquire) {
            continue;
        }
        if state
            .open_documents
            .lock()
            .recover_poison("run_actor(stale command)")
            .supersedes(&command)
        {
            continue;
        }
        observe_healthy_service(&mut state, Instant::now());
        let implicated_grammar = command_grammar(&command).or_else(|| {
            command_uri(&command).and_then(|uri| {
                state
                    .open_documents
                    .lock()
                    .recover_poison("run_actor(command grammar)")
                    .grammar_for(uri)
            })
        });
        let command_document =
            command_document(&command).map(|(uri, incarnation)| (uri.to_string(), incarnation));
        let artifact_replaced = command_uri(&command).is_some_and(|uri| {
            command_grammar(&command).is_some_and(|incoming| {
                state.replicas.get(uri).is_some_and(|current| {
                    current.grammar_symbol == incoming.grammar_symbol
                        && current.artifact_digest != incoming.artifact_digest
                })
            })
        });
        if artifact_replaced {
            log::info!(
                target: "kakehashi::tree_worker_shadow",
                "performing planned worker restart for parser artifact replacement",
            );
            let recovery_mode = if matches!(state.breaker, BreakerState::HalfOpen { .. }) {
                RecoveryMode::HalfOpen
            } else {
                RecoveryMode::Planned
            };
            if !recover_worker(
                &mut state,
                &executable,
                compute_threads,
                FailureClass::Systemic,
                None,
                &comparisons,
                recovery_mode,
            ) {
                mark_tree_tier_unavailable(
                    &mut state,
                    &disabled,
                    &pending_configuration_generation,
                );
                continue;
            }
            continue;
        }
        if replica_satisfies_command(&state.replicas, &command) {
            continue;
        }
        retag_command(&mut command, state.worker_generation);
        if implicated_grammar
            .as_ref()
            .is_some_and(|grammar| state.quarantined.contains(grammar))
        {
            if let Some((uri, incarnation)) = command_document.as_ref() {
                comparisons.mark_closed(uri, *incarnation);
            }
            continue;
        }
        let started = Instant::now();
        let (response, synced_identity) = execute_command(
            state
                .client
                .as_ref()
                .expect("an enabled shadow actor has a worker client"),
            command,
            &mut state.replicas,
            &comparisons,
        );
        match response {
            Ok(Response::Snapshot(snapshot)) => {
                if let Some(identity) = synced_identity {
                    state
                        .replicas
                        .insert(snapshot.context.uri.clone(), identity);
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
                log::warn!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {} requested restart: {}",
                    state.worker_generation,
                    required.reason
                );
                if matches!(state.breaker, BreakerState::HalfOpen { .. })
                    || !recover_worker(
                        &mut state,
                        &executable,
                        compute_threads,
                        FailureClass::Systemic,
                        None,
                        &comparisons,
                        RecoveryMode::Normal,
                    )
                {
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
            }
            Ok(Response::Error(error)) => {
                if let Some((uri, incarnation)) = command_document {
                    comparisons.mark_closed(&uri, incarnation);
                }
                log::warn!(
                    target: "kakehashi::tree_worker_shadow",
                    "shadow request rejected: {}",
                    error.message
                );
            }
            Ok(_) => {}
            Err(error) => {
                let class = classify_transport_failure(
                    state
                        .client
                        .as_ref()
                        .expect("failed request retains its worker client"),
                    &error,
                    implicated_grammar.as_ref(),
                );
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {} transport failed: {error}",
                    state.worker_generation,
                );
                if matches!(state.breaker, BreakerState::HalfOpen { .. })
                    || !recover_worker(
                        &mut state,
                        &executable,
                        compute_threads,
                        class,
                        implicated_grammar,
                        &comparisons,
                        RecoveryMode::Normal,
                    )
                {
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
            }
        }
    }
    if let Some(client) = state.client
        && let Err(error) = client.shutdown()
    {
        log::warn!(target: "kakehashi::tree_worker_shadow", "worker shutdown failed: {error}");
    }
    if let Some(ack) = shutdown_ack {
        let _ = ack.send(());
    }
}

fn breaker_probe_allowed(state: BreakerState, generation: u64, now: Instant) -> bool {
    matches!(
        state,
        BreakerState::Open {
            opened_at,
            baseline_generation,
            cooldown,
        } if generation > baseline_generation
            && now.saturating_duration_since(opened_at) >= cooldown
    )
}

fn mark_tree_tier_unavailable(
    state: &mut SupervisorState,
    disabled: &AtomicBool,
    pending_configuration_generation: &AtomicU64,
) {
    if let Some(client) = state.client.take()
        && let Err(error) = client.shutdown()
    {
        log::warn!(target: "kakehashi::tree_worker_shadow", "failed worker cleanup while opening breaker: {error}");
    }
    state.healthy_since = None;
    state.long_healthy_since = None;
    state.breaker = BreakerState::Open {
        opened_at: Instant::now(),
        baseline_generation: pending_configuration_generation.load(Ordering::Acquire),
        cooldown: if state.restart_budget.tokens_remaining() == 0 {
            LONG_HEALTHY_INTERVAL
        } else {
            FAST_HEALTHY_INTERVAL
        },
    };
    disabled.store(true, Ordering::Release);
}

fn observe_healthy_service(state: &mut SupervisorState, now: Instant) {
    let Some(healthy_since) = state.healthy_since else {
        return;
    };
    if now.saturating_duration_since(healthy_since) >= FAST_HEALTHY_INTERVAL {
        state.restart_budget.reset_fast();
        if let BreakerState::HalfOpen { probe_generation } = state.breaker {
            state.breaker = BreakerState::Closed;
            log::info!(
                target: "kakehashi::tree_worker_shadow",
                "closed shadow worker breaker after configuration generation {probe_generation} completed 60 seconds of healthy service",
            );
        }
    }
    let Some(last_restore) = state.long_healthy_since else {
        return;
    };
    let elapsed = now.saturating_duration_since(last_restore);
    let intervals = elapsed.as_secs() / LONG_HEALTHY_INTERVAL.as_secs();
    if intervals == 0 {
        return;
    }
    state.restart_budget.restore_long(intervals);
    state.long_healthy_since =
        Some(last_restore + LONG_HEALTHY_INTERVAL.saturating_mul(intervals as u32));
}

fn execute_command(
    client: &Client,
    command: ShadowCommand,
    replicas: &mut HashMap<String, ReplicaIdentity>,
    comparisons: &ComparisonStore,
) -> (std::io::Result<Response>, Option<ReplicaIdentity>) {
    match command {
        ShadowCommand::Sync(request) => {
            let identity = ReplicaIdentity::from_sync(&request);
            comparisons.open(&request.context.uri, request.context.incarnation);
            (sync_and_derive(client, request), Some(identity))
        }
        ShadowCommand::Apply { request, fallback } => {
            let uri = fallback.context.uri.clone();
            let identity = ReplicaIdentity::from_sync(&fallback);
            if replica_requires_sync(replicas, &uri, &identity) {
                (sync_and_derive(client, fallback), Some(identity))
            } else {
                match client.apply_document_edits_and_derive(request) {
                    Ok(Response::Snapshot(snapshot)) => (Ok(Response::Snapshot(snapshot)), None),
                    Ok(Response::WorkerRestartRequired(required)) => {
                        (Ok(Response::WorkerRestartRequired(required)), None)
                    }
                    Ok(_) => (sync_and_derive(client, fallback), Some(identity)),
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
    }
}

fn recover_worker(
    state: &mut SupervisorState,
    executable: &std::path::Path,
    compute_threads: usize,
    mut class: FailureClass,
    implicated_grammar: Option<GrammarIdentity>,
    comparisons: &ComparisonStore,
    mode: RecoveryMode,
) -> bool {
    let recovery_started = Instant::now();
    if let Some(grammar) = implicated_grammar {
        quarantine_grammar(state, grammar, class, comparisons);
    }

    let mut first_attempt = true;
    loop {
        let charged = mode != RecoveryMode::Planned || !first_attempt;
        if mode == RecoveryMode::HalfOpen {
            state.restart_budget.consume_half_open(class);
        } else if charged && !state.restart_budget.consume(class) {
            log::error!(
                target: "kakehashi::tree_worker_shadow",
                "disabled shadow tree tier after restart budget exhaustion: systemic={} native={} tokens_remaining={}",
                state.restart_budget.systemic,
                state.restart_budget.native,
                state.restart_budget.tokens_remaining(),
            );
            return false;
        }
        if let Some(old_client) = state.client.take()
            && let Err(error) = old_client.shutdown()
        {
            log::warn!(target: "kakehashi::tree_worker_shadow", "failed worker cleanup: {error}");
        }
        if charged {
            std::thread::sleep(state.restart_budget.backoff());
        }
        let generation = NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed);
        let replacement = match Client::spawn(executable, compute_threads, generation) {
            Ok(client) => client,
            Err(error) => {
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {generation} restart failed: {error}"
                );
                class = FailureClass::Systemic;
                if mode == RecoveryMode::HalfOpen {
                    return false;
                }
                first_attempt = false;
                continue;
            }
        };
        state.replicas.clear();
        match resync_open_documents(
            &replacement,
            generation,
            &state.open_documents,
            &state.quarantined,
            &mut state.replicas,
            comparisons,
        ) {
            Ok(resynced) => {
                state.worker_generation = generation;
                state.client = Some(replacement);
                let healthy_since = Instant::now();
                state.healthy_since = Some(healthy_since);
                state.long_healthy_since = Some(healthy_since);
                log::info!(
                    target: "kakehashi::tree_worker_shadow",
                    "restarted worker generation {generation} and full-resynced {} open documents (bytes={} recovery_ms={})",
                    resynced.documents,
                    resynced.bytes,
                    recovery_started.elapsed().as_millis(),
                );
                return true;
            }
            Err(failure) => {
                state.client = Some(replacement);
                if let Some(grammar) = failure.implicated_grammar {
                    quarantine_grammar(state, grammar, failure.class, comparisons);
                }
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {generation} full resync failed: {}",
                    failure.error,
                );
                class = failure.class;
                if mode == RecoveryMode::HalfOpen {
                    return false;
                }
                first_attempt = false;
            }
        }
    }
}

fn quarantine_grammar(
    state: &mut SupervisorState,
    grammar: GrammarIdentity,
    class: FailureClass,
    comparisons: &ComparisonStore,
) {
    if !state.quarantined.insert(grammar.clone()) {
        return;
    }
    let open_documents = state
        .open_documents
        .lock()
        .recover_poison("quarantine_grammar(open documents)");
    for request in open_documents.current.values() {
        if GrammarIdentity::from_sync(request) == grammar {
            comparisons.mark_closed(&request.context.uri, request.context.incarnation);
        }
    }
    log::error!(
        target: "kakehashi::tree_worker_shadow",
        "quarantined grammar conservatively for this session after {:?} worker loss: symbol={} artifact_digest={}",
        class,
        grammar.grammar_symbol,
        grammar.artifact_digest,
    );
}

fn resync_open_documents(
    client: &Client,
    generation: u64,
    open_documents: &Mutex<OpenDocuments>,
    quarantined: &HashSet<GrammarIdentity>,
    replicas: &mut HashMap<String, ReplicaIdentity>,
    comparisons: &ComparisonStore,
) -> Result<ResyncStats, ResyncFailure> {
    let mut uris = open_documents
        .lock()
        .recover_poison("resync_open_documents(uris)")
        .current
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    uris.sort_unstable();
    let mut resynced = ResyncStats::default();
    for uri in uris {
        let Some(mut request) = open_documents
            .lock()
            .recover_poison("resync_open_documents(request)")
            .current
            .get(&uri)
            .cloned()
        else {
            continue;
        };
        let grammar = GrammarIdentity::from_sync(&request);
        if quarantined.contains(&grammar) {
            continue;
        }
        let request_bytes = request.text.len();
        let request_uri = request.context.uri.clone();
        request.context.worker_generation = generation;
        let identity = ReplicaIdentity::from_sync(&request);
        comparisons.open(&request.context.uri, request.context.incarnation);
        match sync_and_derive(client, request) {
            Ok(Response::Snapshot(snapshot)) => {
                replicas.insert(snapshot.context.uri.clone(), identity);
                comparisons.record(
                    &snapshot.context.uri,
                    snapshot.context.incarnation,
                    snapshot.context.content_version,
                    ComparisonSide::Shadow(TreeSummary::from_snapshot(&snapshot)),
                );
                resynced.documents += 1;
                resynced.bytes = resynced.bytes.saturating_add(request_bytes);
            }
            Ok(Response::WorkerRestartRequired(required)) => {
                return Err(ResyncFailure {
                    error: std::io::Error::other(required.reason),
                    class: FailureClass::Systemic,
                    implicated_grammar: None,
                });
            }
            Ok(Response::Error(error)) => {
                comparisons.mark_closed(&request_uri, identity.incarnation);
                log::warn!(
                    target: "kakehashi::tree_worker_shadow",
                    "skipped shadow resync for {request_uri}: {}",
                    error.message,
                );
            }
            Ok(response) => {
                return Err(ResyncFailure {
                    error: std::io::Error::other(format!(
                        "unexpected resync response: {response:?}"
                    )),
                    class: FailureClass::Systemic,
                    implicated_grammar: None,
                });
            }
            Err(error) => {
                let class = classify_transport_failure(client, &error, Some(&grammar));
                return Err(ResyncFailure {
                    error,
                    class,
                    implicated_grammar: Some(grammar),
                });
            }
        }
    }
    Ok(resynced)
}

fn retag_command(command: &mut ShadowCommand, generation: u64) {
    match command {
        ShadowCommand::Sync(request) => request.context.worker_generation = generation,
        ShadowCommand::Apply { request, fallback } => {
            request.context.worker_generation = generation;
            fallback.context.worker_generation = generation;
        }
        ShadowCommand::Close(request) => request.context.worker_generation = generation,
        ShadowCommand::Shutdown(_) => {}
    }
}

fn classify_transport_failure(
    client: &Client,
    error: &std::io::Error,
    implicated_grammar: Option<&GrammarIdentity>,
) -> FailureClass {
    if implicated_grammar.is_none() {
        return FailureClass::Systemic;
    }
    if error.kind() == std::io::ErrorKind::TimedOut {
        return FailureClass::NativeEvidenced;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if !client.was_terminated_by_transport()
            && client
                .try_wait()
                .ok()
                .flatten()
                .is_some_and(|status| status.signal().is_some())
        {
            return FailureClass::NativeEvidenced;
        }
    }
    FailureClass::Systemic
}

fn command_uri(command: &ShadowCommand) -> Option<&str> {
    match command {
        ShadowCommand::Sync(request) => Some(&request.context.uri),
        ShadowCommand::Apply { fallback, .. } => Some(&fallback.context.uri),
        ShadowCommand::Close(request) => Some(&request.context.uri),
        ShadowCommand::Shutdown(_) => None,
    }
}

fn command_document(command: &ShadowCommand) -> Option<(&str, u64)> {
    match command {
        ShadowCommand::Sync(request) => Some((&request.context.uri, request.context.incarnation)),
        ShadowCommand::Apply { fallback, .. } => {
            Some((&fallback.context.uri, fallback.context.incarnation))
        }
        ShadowCommand::Close(request) => Some((&request.context.uri, request.context.incarnation)),
        ShadowCommand::Shutdown(_) => None,
    }
}

fn command_grammar(command: &ShadowCommand) -> Option<GrammarIdentity> {
    match command {
        ShadowCommand::Sync(request) => Some(GrammarIdentity::from_sync(request)),
        ShadowCommand::Apply { fallback, .. } => Some(GrammarIdentity::from_sync(fallback)),
        ShadowCommand::Close(_) | ShadowCommand::Shutdown(_) => None,
    }
}

fn command_sync(command: &ShadowCommand) -> Option<&SyncDocument> {
    match command {
        ShadowCommand::Sync(request) => Some(request),
        ShadowCommand::Apply { fallback, .. } => Some(fallback),
        ShadowCommand::Close(_) | ShadowCommand::Shutdown(_) => None,
    }
}

fn replica_satisfies_command(
    replicas: &HashMap<String, ReplicaIdentity>,
    command: &ShadowCommand,
) -> bool {
    command_uri(command).is_some_and(|uri| {
        command_sync(command)
            .is_some_and(|request| replicas.get(uri) == Some(&ReplicaIdentity::from_sync(request)))
    })
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
        if let Some(completed) = slot.completed
            && incoming <= completed
        {
            if incoming < completed {
                self.superseded.fetch_add(1, Ordering::Relaxed);
            }
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
        slot.completed = Some(incoming);
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
                completed: None,
            };
        }
    }

    fn pending_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.pending.is_some())
            .count()
    }

    fn pending_uris(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter(|slot| slot.pending.is_some())
            .map(|slot| slot.key().clone())
            .collect()
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
            pending_configuration_generation: Arc::new(AtomicU64::new(0)),
            open_documents: Arc::new(Mutex::new(OpenDocuments::default())),
            registry_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    fn grammar() -> WorkerGrammarDescriptor {
        WorkerGrammarDescriptor {
            source_path: "/parser/rust.so".into(),
            parser_path: "/parser/rust.so".into(),
            grammar_symbol: "rust".into(),
            artifact_digest: "sha256:rust-v1".into(),
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

    fn sync(uri: &str, incarnation: u64, version: u64, generation: u64) -> SyncDocument {
        SyncDocument {
            context: RequestContext {
                request_id: version,
                worker_generation: generation,
                uri: uri.into(),
                incarnation,
                content_version: version,
                configuration_generation: 3,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            source_path: "/parser/rust.so".into(),
            parser_path: "/parser/rust.so".into(),
            artifact_digest: "sha256:rust-v1".into(),
            text: format!("version {version}"),
        }
    }

    #[test]
    fn open_document_registry_keeps_only_latest_authoritative_text() {
        let mut documents = OpenDocuments::default();
        documents.observe(&ShadowCommand::Sync(sync("file:///a.rs", 1, 1, 7)));
        documents.observe(&ShadowCommand::Apply {
            request: ApplyDocumentEditsAndDerive {
                context: sync("file:///a.rs", 1, 2, 7).context,
                base_version: 1,
                edits: Vec::new(),
            },
            fallback: sync("file:///a.rs", 1, 2, 7),
        });

        let latest = documents.current.get("file:///a.rs").unwrap();
        assert_eq!(latest.context.content_version, 2);
        assert_eq!(latest.text, "version 2");

        documents.observe(&ShadowCommand::Close(CloseDocument {
            context: latest.context.clone(),
        }));
        assert!(documents.current.is_empty());
    }

    #[test]
    fn artifact_replacement_updates_all_same_path_documents_and_requests_restart() {
        let mut documents = OpenDocuments::default();
        assert_eq!(
            documents.observe(&ShadowCommand::Sync(sync("file:///a.rs", 1, 1, 7))),
            Observation::Accepted {
                artifact_replaced: false
            }
        );
        assert_eq!(
            documents.observe(&ShadowCommand::Sync(sync("file:///b.rs", 1, 1, 7))),
            Observation::Accepted {
                artifact_replaced: false
            }
        );

        let mut replacement = sync("file:///b.rs", 1, 2, 7);
        replacement.parser_path = "/private/sha256-rust-v2.so".into();
        let replacement_path = replacement.parser_path.clone();
        replacement.artifact_digest = "sha256:rust-v2".into();
        replacement.context.configuration_generation = 4;
        assert_eq!(
            documents.observe(&ShadowCommand::Sync(replacement)),
            Observation::Accepted {
                artifact_replaced: true
            }
        );

        for document in documents.current.values() {
            assert_eq!(document.artifact_digest, "sha256:rust-v2");
            assert_eq!(document.parser_path, replacement_path);
            assert_eq!(document.context.configuration_generation, 4);
        }

        let mut alias = sync("file:///c.rs", 1, 1, 7);
        alias.artifact_digest = "sha256:rust-v2".into();
        alias.context.configuration_generation = 4;
        assert_eq!(
            documents.observe(&ShadowCommand::Sync(alias)),
            Observation::Accepted {
                artifact_replaced: false
            }
        );
    }

    #[test]
    fn stale_configuration_cannot_roll_back_a_newer_artifact_selection() {
        let mut documents = OpenDocuments::default();
        let mut current = sync("file:///a.rs", 1, 2, 7);
        current.context.configuration_generation = 5;
        current.parser_path = "/private/new.so".into();
        current.artifact_digest = "sha256:new".into();
        assert!(matches!(
            documents.observe(&ShadowCommand::Sync(current)),
            Observation::Accepted { .. }
        ));

        let stale = sync("file:///b.rs", 1, 3, 7);
        assert_eq!(
            documents.observe(&ShadowCommand::Sync(stale)),
            Observation::Stale
        );
        assert_eq!(documents.current.len(), 1);
        assert_eq!(
            documents.current["file:///a.rs"].artifact_digest,
            "sha256:new"
        );
    }

    #[test]
    fn command_retagging_fences_queued_work_to_replacement_generation() {
        let mut command = ShadowCommand::Apply {
            request: ApplyDocumentEditsAndDerive {
                context: sync("file:///a.rs", 1, 2, 7).context,
                base_version: 1,
                edits: Vec::new(),
            },
            fallback: sync("file:///a.rs", 1, 2, 7),
        };
        retag_command(&mut command, 11);

        let ShadowCommand::Apply { request, fallback } = command else {
            unreachable!();
        };
        assert_eq!(request.context.worker_generation, 11);
        assert_eq!(fallback.context.worker_generation, 11);
    }

    #[test]
    fn exact_command_is_already_satisfied_after_full_resync() {
        let command = ShadowCommand::Sync(sync("file:///a.rs", 1, 2, 7));
        let mut replicas = HashMap::new();
        replicas.insert(
            "file:///a.rs".into(),
            ReplicaIdentity::from_sync(command_sync(&command).unwrap()),
        );
        assert!(replica_satisfies_command(&replicas, &command));

        let newer = ShadowCommand::Sync(sync("file:///a.rs", 1, 3, 7));
        assert!(!replica_satisfies_command(&replicas, &newer));
    }

    #[test]
    fn configuration_mirroring_publishes_a_complete_registry_before_enqueue() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let shadow = shadow(sender);
        let uri_a = url::Url::parse("file:///a.rs").unwrap();
        let uri_b = url::Url::parse("file:///b.rs").unwrap();
        let mut grammar_a = grammar();
        grammar_a.parser_path = "/private/new.so".into();
        grammar_a.artifact_digest = "sha256:new".into();
        grammar_a.configuration_generation = 9;
        let mut grammar_b = grammar_a.clone();
        grammar_b.grammar_symbol = "rust_derived".into();

        shadow.mirror_configuration(
            9,
            vec![
                MirrorDocument {
                    uri: uri_a,
                    incarnation: 1,
                    content_version: 2,
                    language: "rust".into(),
                    grammar: Some(grammar_a),
                    text: "fn a() {}".into(),
                },
                MirrorDocument {
                    uri: uri_b,
                    incarnation: 2,
                    content_version: 3,
                    language: "rust_derived".into(),
                    grammar: Some(grammar_b),
                    text: "fn b() {}".into(),
                },
            ],
        );

        let registry = shadow
            .open_documents
            .lock()
            .recover_poison("configuration mirror test");
        assert_eq!(registry.current.len(), 2);
        assert_eq!(registry.current["file:///a.rs"].grammar_symbol, "rust");
        assert_eq!(
            registry.current["file:///b.rs"].grammar_symbol,
            "rust_derived"
        );
        assert_eq!(
            shadow
                .pending_configuration_generation
                .load(Ordering::Acquire),
            9
        );
        drop(registry);
        assert!(matches!(receiver.try_recv(), Ok(ShadowCommand::Sync(_))));
        assert!(matches!(receiver.try_recv(), Ok(ShadowCommand::Sync(_))));
    }

    #[test]
    fn restart_budgets_are_independent_and_session_bounded() {
        let mut systemic = RestartBudget::default();
        assert!(systemic.consume(FailureClass::Systemic));
        assert!(systemic.consume(FailureClass::Systemic));
        assert!(systemic.consume(FailureClass::Systemic));
        assert!(!systemic.consume(FailureClass::Systemic));
        assert_eq!(systemic.tokens_remaining(), MAX_SESSION_RESTARTS - 3);

        let mut native = RestartBudget::default();
        for _ in 0..MAX_NATIVE_RESTARTS {
            assert!(native.consume(FailureClass::NativeEvidenced));
        }
        assert!(!native.consume(FailureClass::NativeEvidenced));

        assert_eq!(systemic.backoff(), Duration::from_secs(1));
    }

    #[test]
    fn healthy_intervals_reset_fast_budgets_but_restore_long_tokens_slowly() {
        let started = Instant::now();
        let mut state = SupervisorState {
            client: None,
            worker_generation: 1,
            replicas: HashMap::new(),
            open_documents: Arc::new(Mutex::new(OpenDocuments::default())),
            quarantined: HashSet::new(),
            restart_budget: RestartBudget::default(),
            breaker: BreakerState::HalfOpen {
                probe_generation: 7,
            },
            healthy_since: Some(started),
            long_healthy_since: Some(started),
        };
        for _ in 0..MAX_SYSTEMIC_RESTARTS {
            assert!(state.restart_budget.consume(FailureClass::Systemic));
        }
        assert_eq!(
            state.restart_budget.tokens_remaining(),
            MAX_SESSION_RESTARTS - 3
        );

        observe_healthy_service(&mut state, started + FAST_HEALTHY_INTERVAL);
        assert_eq!(state.breaker, BreakerState::Closed);
        assert!(state.restart_budget.consume(FailureClass::Systemic));
        assert_eq!(
            state.restart_budget.tokens_remaining(),
            MAX_SESSION_RESTARTS - 4
        );
        assert_eq!(state.restart_budget.backoff(), Duration::from_secs(2));

        observe_healthy_service(
            &mut state,
            started + FAST_HEALTHY_INTERVAL + LONG_HEALTHY_INTERVAL,
        );
        assert_eq!(
            state.restart_budget.tokens_remaining(),
            MAX_SESSION_RESTARTS - 3
        );
        assert_eq!(state.restart_budget.backoff(), INITIAL_RESTART_BACKOFF);
    }

    #[test]
    fn breaker_half_open_requires_the_matching_cooldown() {
        let started = Instant::now();
        let fast = BreakerState::Open {
            opened_at: started,
            baseline_generation: 3,
            cooldown: FAST_HEALTHY_INTERVAL,
        };
        assert!(!breaker_probe_allowed(
            fast,
            4,
            started + Duration::from_secs(59)
        ));
        assert!(breaker_probe_allowed(
            fast,
            4,
            started + FAST_HEALTHY_INTERVAL
        ));
        assert!(!breaker_probe_allowed(
            fast,
            3,
            started + FAST_HEALTHY_INTERVAL
        ));
    }

    #[test]
    fn configuration_change_reaches_an_open_breaker() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        shadow.disabled.store(true, Ordering::Release);

        shadow.configuration_changed(9);

        assert_eq!(
            shadow
                .pending_configuration_generation
                .load(Ordering::Acquire),
            9
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
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
    fn replayed_shadow_for_an_already_completed_version_is_ignored() {
        let store = ComparisonStore::default();
        let uri = "file:///a.rs";
        store.open(uri, 1);
        store.record(
            uri,
            1,
            2,
            ComparisonSide::Authoritative(summary("source_file")),
        );
        store.record(uri, 1, 2, ComparisonSide::Shadow(summary("source_file")));

        store.record(uri, 1, 2, ComparisonSide::Shadow(summary("source_file")));

        assert_eq!(store.matched.load(Ordering::Relaxed), 1);
        assert_eq!(store.superseded.load(Ordering::Relaxed), 0);
        assert_eq!(store.pending_count(), 0);
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
