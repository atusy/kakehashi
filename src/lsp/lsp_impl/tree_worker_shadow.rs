use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use serde_json::Value;

use crate::error::LockResultExt;
use crate::language::coordinator::WorkerGrammarDescriptor;
use crate::lsp::text_sync::SequentialByteEdit;
use crate::tree_worker::{
    ApplyDocumentEdits, ApplyDocumentEditsAndDerive, ByteEdit, CapturesResult, Client,
    CloseDocument, ConfigureLanguages, DeriveDocumentSnapshot, DeriveInjectionRegions,
    DeriveNativeBindings, DeriveSelectionRanges, DeriveSemanticTokens, DerivedSemanticTokens,
    InjectionRegionsResult, NativeBindingsFacts, NativeBindingsResult, NavigateNode,
    NodeNavigation, NodeResult, NodeScalarOperation, NodeScalarValue, OpaqueNodeId, RequestContext,
    ResolveNode, Response, RunCaptures, RunNodeScalar, SelectionRangesResult, SyncDocument,
    WirePosition, WireRange, WorkerLanguageCatalog, named_node_count,
};

use super::Kakehashi;

const SHADOW_ENV: &str = "KAKEHASHI_TREE_WORKER_SHADOW";
const MODE_ENV: &str = "KAKEHASHI_TREE_WORKER_MODE";
const SHADOW_THREADS_ENV: &str = "KAKEHASHI_TREE_WORKER_THREADS";
const SHADOW_QUEUE_CAPACITY: usize = 256;
const SHADOW_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SYSTEMIC_RESTARTS: u8 = 3;
const MAX_NATIVE_RESTARTS: u8 = 8;
const MAX_SESSION_RESTARTS: u8 = 16;
const INITIAL_RESTART_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(300);
const WORKER_LIVENESS_POLL: Duration = Duration::from_millis(250);
const WORKER_READ_READY_TIMEOUT: Duration = Duration::from_secs(15);
const FAST_HEALTHY_INTERVAL: Duration = Duration::from_secs(60);
const LONG_HEALTHY_INTERVAL: Duration = Duration::from_secs(10 * 60);
static NEXT_WORKER_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_CATALOG_REQUEST_ID: AtomicU64 = AtomicU64::new(1_u64 << 63);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeWorkerMode {
    Legacy,
    Shadow,
    Authoritative,
}

/// Cancels worker-side reader work when the async LSP request that owns it is
/// superseded, explicitly cancelled, or dropped during shutdown.
struct WorkerRequestCancellation {
    client: Arc<Client>,
    request_id: u64,
    armed: bool,
}

impl WorkerRequestCancellation {
    fn new(client: Arc<Client>, request_id: u64) -> Self {
        Self {
            client,
            request_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkerRequestCancellation {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.client.cancel_request(self.request_id);
        }
    }
}

pub(super) struct WorkerInjectionView {
    pub(super) text: Arc<str>,
    pub(super) language: String,
    pub(super) regions: Arc<Vec<crate::language::injection::ResolvedInjection>>,
}

enum ShadowCommand {
    Sync(SyncDocument),
    Apply {
        request: ApplyDocumentEditsAndDerive,
        fallback: SyncDocument,
        derive_snapshot: bool,
    },
    Close(CloseDocument),
    Forget(RequestContext),
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
    queries: crate::tree_worker::WorkerQuerySources,
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
            queries: request.queries.clone(),
            configuration_generation: request.context.configuration_generation,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
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

#[derive(Clone, Copy)]
struct RestartPolicy {
    initial_backoff: Duration,
    max_backoff: Duration,
    fast_healthy_interval: Duration,
    long_healthy_interval: Duration,
}

impl RestartPolicy {
    fn from_environment() -> Self {
        let policy = Self::default();
        #[cfg(feature = "e2e")]
        let policy = {
            let mut policy = policy;
            if let Some(milliseconds) = std::env::var("KAKEHASHI_TREE_WORKER_FAST_COOLDOWN_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|milliseconds| *milliseconds > 0)
            {
                policy.fast_healthy_interval = Duration::from_millis(milliseconds);
            }
            policy
        };
        policy
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: INITIAL_RESTART_BACKOFF,
            max_backoff: MAX_RESTART_BACKOFF,
            fast_healthy_interval: FAST_HEALTHY_INTERVAL,
            long_healthy_interval: LONG_HEALTHY_INTERVAL,
        }
    }
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
        if self.tokens == 0 {
            return false;
        }
        match class {
            FailureClass::Systemic => {
                if self.systemic >= MAX_SYSTEMIC_RESTARTS {
                    return false;
                }
                self.systemic = self.systemic.saturating_add(1);
                if self.systemic == MAX_SYSTEMIC_RESTARTS {
                    return false;
                }
            }
            FailureClass::NativeEvidenced => {
                if self.native >= MAX_NATIVE_RESTARTS {
                    return false;
                }
                self.native = self.native.saturating_add(1);
            }
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

    fn backoff(&self, policy: RestartPolicy) -> Duration {
        let exponent = self.backoff_count.saturating_sub(1).min(10) as u32;
        policy
            .initial_backoff
            .saturating_mul(1_u32 << exponent)
            .min(policy.max_backoff)
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
        synchronized_epoch: u64,
        reconcile_after: Instant,
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
    closed_incarnations: HashMap<String, u64>,
    acknowledged: HashMap<String, RequestContext>,
    replica_changed: Arc<tokio::sync::Notify>,
    catalog: Option<(u64, WorkerLanguageCatalog)>,
    catalog_preload: Option<(u64, u64)>,
    catalog_acknowledged: Option<(u64, u64)>,
    worker_regions: HashMap<String, WorkerRegionSnapshot>,
}

#[derive(Clone)]
struct WorkerRegionSnapshot {
    context: RequestContext,
    regions: Arc<Vec<crate::language::injection::ResolvedInjection>>,
}

struct SupervisorState {
    client: Option<Arc<Client>>,
    read_client: Arc<ArcSwapOption<Client>>,
    worker_nodes: Arc<dashmap::DashMap<(String, String), OpaqueNodeId>>,
    worker_generation: u64,
    replicas: HashMap<String, ReplicaIdentity>,
    open_documents: Arc<Mutex<OpenDocuments>>,
    quarantined: HashSet<GrammarIdentity>,
    restart_budget: RestartBudget,
    restart_policy: RestartPolicy,
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
    read_client: Arc<ArcSwapOption<Client>>,
    worker_nodes: Arc<dashmap::DashMap<(String, String), OpaqueNodeId>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ResyncStats {
    documents: usize,
    bytes: usize,
}

struct ResyncFailure {
    error: std::io::Error,
    class: FailureClass,
    active_grammars: Vec<GrammarIdentity>,
    active_lease_count: usize,
    snapshot_complete: bool,
}

impl ResyncFailure {
    fn from_worker_loss(error: std::io::Error, failure: WorkerLossEvidence) -> Self {
        Self {
            error,
            class: failure.class,
            active_grammars: failure.active_grammars,
            active_lease_count: failure.active_lease_count,
            snapshot_complete: failure.snapshot_complete,
        }
    }
}

struct WorkerLossEvidence {
    class: FailureClass,
    active_grammars: Vec<GrammarIdentity>,
    active_lease_count: usize,
    snapshot_complete: bool,
}

impl WorkerLossEvidence {
    fn systemic() -> Self {
        Self {
            class: FailureClass::Systemic,
            active_grammars: Vec::new(),
            active_lease_count: 0,
            snapshot_complete: true,
        }
    }

    fn incomplete(active_grammars: Vec<GrammarIdentity>, active_lease_count: usize) -> Self {
        Self {
            class: FailureClass::Systemic,
            active_lease_count,
            active_grammars,
            snapshot_complete: false,
        }
    }

    fn allows_restart(&self) -> bool {
        self.snapshot_complete
    }
}

#[derive(Default)]
struct QuarantineBatch {
    newly_quarantined: usize,
    already_quarantined: usize,
}

impl OpenDocuments {
    fn acknowledge(&mut self, context: RequestContext) {
        self.acknowledged.insert(context.uri.clone(), context);
        self.replica_changed.notify_waiters();
    }

    fn acknowledges(&self, context: &RequestContext) -> bool {
        self.acknowledged
            .get(&context.uri)
            .is_some_and(|acknowledged| {
                acknowledged.worker_generation == context.worker_generation
                    && acknowledged.incarnation == context.incarnation
                    && acknowledged.content_version == context.content_version
                    && acknowledged.configuration_generation == context.configuration_generation
            })
    }

    fn expects(&self, context: &RequestContext) -> bool {
        self.current.get(&context.uri).is_some_and(|current| {
            current.context.incarnation == context.incarnation
                && current.context.content_version == context.content_version
                && current.context.configuration_generation == context.configuration_generation
        })
    }

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
            ShadowCommand::Forget(context) => {
                self.current.get(&context.uri).is_some_and(|current| {
                    current.context.incarnation > context.incarnation
                        || (current.context.incarnation == context.incarnation
                            && (current.context.configuration_generation
                                > context.configuration_generation
                                || (current.context.configuration_generation
                                    == context.configuration_generation
                                    && current.context.content_version > context.content_version)))
                })
            }
            ShadowCommand::Shutdown(_) => false,
        }
    }

    fn observe(&mut self, command: &ShadowCommand) -> Observation {
        if self.supersedes(command)
            && matches!(command, ShadowCommand::Close(_) | ShadowCommand::Forget(_))
        {
            return Observation::Stale;
        }
        let incoming = match command {
            ShadowCommand::Sync(request) => Some(request),
            ShadowCommand::Apply { fallback, .. } => Some(fallback),
            ShadowCommand::Close(_) | ShadowCommand::Forget(_) | ShadowCommand::Shutdown(_) => None,
        };
        if incoming.is_some_and(|incoming| {
            self.closed_incarnations
                .get(&incoming.context.uri)
                .is_some_and(|closed| *closed >= incoming.context.incarnation)
        }) {
            return Observation::Stale;
        }
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
                self.closed_incarnations.remove(&request.context.uri);
                self.worker_regions.remove(&request.context.uri);
                self.current
                    .insert(request.context.uri.clone(), request.clone());
            }
            ShadowCommand::Apply { fallback, .. } => {
                self.closed_incarnations.remove(&fallback.context.uri);
                self.worker_regions.remove(&fallback.context.uri);
                self.current
                    .insert(fallback.context.uri.clone(), fallback.clone());
            }
            ShadowCommand::Close(request) => {
                self.current.remove(&request.context.uri);
                self.acknowledged.remove(&request.context.uri);
                self.worker_regions.remove(&request.context.uri);
                self.closed_incarnations
                    .entry(request.context.uri.clone())
                    .and_modify(|closed| *closed = (*closed).max(request.context.incarnation))
                    .or_insert(request.context.incarnation);
            }
            ShadowCommand::Forget(context) => {
                self.current.remove(&context.uri);
                self.acknowledged.remove(&context.uri);
                self.worker_regions.remove(&context.uri);
            }
            ShadowCommand::Shutdown(_) => {}
        }
        self.replica_changed.notify_waiters();
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
    mode: TreeWorkerMode,
    sender: Option<mpsc::SyncSender<ShadowCommand>>,
    accepting: Arc<AtomicBool>,
    disabled: Arc<AtomicBool>,
    next_request_id: AtomicU64,
    worker_generation: u64,
    comparisons: Arc<ComparisonStore>,
    pending_configuration_generation: Arc<AtomicU64>,
    open_documents: Arc<Mutex<OpenDocuments>>,
    registry_epoch: Arc<AtomicU64>,
    read_client: Arc<ArcSwapOption<Client>>,
    worker_nodes: Arc<dashmap::DashMap<(String, String), OpaqueNodeId>>,
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

#[derive(Debug, Eq, PartialEq)]
struct ComparableInjection<'a> {
    language: &'a str,
    byte_range: std::ops::Range<usize>,
    line_range: std::ops::Range<u32>,
    start_column: u32,
    content_hash: u64,
    virtual_content: &'a str,
    line_column_offsets: &'a [u32],
    contiguous: bool,
}

fn worker_region_to_resolved(
    region: &crate::tree_worker::WireInjectionRegion,
) -> crate::language::injection::ResolvedInjection {
    crate::language::injection::ResolvedInjection {
        region: crate::language::injection::CacheableInjectionRegion {
            language: region.language.clone(),
            byte_range: region.byte_start..region.byte_end,
            line_range: region.line_start..region.line_end,
            start_column: region.start_column,
            region_id: region.region_id.clone(),
            content_hash: region.content_hash,
        },
        injection_language: region.language.clone(),
        virtual_content: region.virtual_content.clone(),
        line_column_offsets: region.line_column_offsets.clone(),
        contiguous: region.contiguous,
    }
}

fn worker_node_info(node: crate::tree_worker::OwnedNode) -> Value {
    serde_json::json!({
        "id": node.id.local_id,
        "kind": node.kind,
    })
}

fn remove_capture_ids(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(remove_capture_ids),
        Value::Object(map) => {
            if let Some(Value::Object(node)) = map.get_mut("node") {
                node.remove("id");
            }
            map.values_mut().for_each(remove_capture_ids);
        }
        _ => {}
    }
}

fn capture_node_ids(value: &Value, ids: &mut Vec<String>) {
    match value {
        Value::Array(values) => values.iter().for_each(|value| capture_node_ids(value, ids)),
        Value::Object(map) => {
            if let Some(Value::Object(node)) = map.get("node")
                && let Some(Value::String(id)) = node.get("id")
            {
                ids.push(id.clone());
            }
            map.values().for_each(|value| capture_node_ids(value, ids));
        }
        _ => {}
    }
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
        let mode = configured_mode();
        let disabled = Arc::new(AtomicBool::new(false));
        let accepting = Arc::new(AtomicBool::new(false));
        let comparisons = Arc::new(ComparisonStore::default());
        let pending_configuration_generation = Arc::new(AtomicU64::new(0));
        let open_documents = Arc::new(Mutex::new(OpenDocuments::default()));
        let registry_epoch = Arc::new(AtomicU64::new(0));
        let read_client = Arc::new(ArcSwapOption::empty());
        let worker_nodes = Arc::new(dashmap::DashMap::new());
        let worker_generation = NEXT_WORKER_GENERATION.fetch_add(1, Ordering::Relaxed);
        if mode == TreeWorkerMode::Legacy {
            return Self {
                mode,
                sender: None,
                accepting,
                disabled,
                next_request_id: AtomicU64::new(1),
                worker_generation,
                comparisons,
                pending_configuration_generation,
                open_documents,
                registry_epoch,
                read_client,
                worker_nodes,
            };
        }
        let executable = match std::env::current_exe() {
            Ok(executable) => executable,
            Err(error) => {
                log::error!(target: "kakehashi::tree_worker_shadow", "cannot resolve worker executable: {error}");
                disabled.store(true, Ordering::Release);
                return Self {
                    mode,
                    sender: None,
                    accepting,
                    disabled,
                    next_request_id: AtomicU64::new(1),
                    worker_generation,
                    comparisons,
                    pending_configuration_generation,
                    open_documents,
                    registry_epoch,
                    read_client,
                    worker_nodes,
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
            read_client: Arc::clone(&read_client),
            worker_nodes: Arc::clone(&worker_nodes),
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
                mode,
                sender: None,
                accepting,
                disabled,
                next_request_id: AtomicU64::new(1),
                worker_generation,
                comparisons,
                pending_configuration_generation,
                open_documents,
                registry_epoch,
                read_client,
                worker_nodes,
            };
        }
        accepting.store(true, Ordering::Release);
        log::info!(
            target: "kakehashi::tree_worker_shadow",
            "enabled one {mode:?} tree worker with {compute_threads} compute threads"
        );
        Self {
            mode,
            sender: Some(sender),
            accepting,
            disabled,
            next_request_id: AtomicU64::new(1),
            worker_generation,
            comparisons,
            pending_configuration_generation,
            open_documents,
            registry_epoch,
            read_client,
            worker_nodes,
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

    pub(super) fn is_authoritative(&self) -> bool {
        self.mode == TreeWorkerMode::Authoritative && self.is_enabled()
    }

    pub(super) fn public_node_result(&self, result: Option<NodeResult>, list: bool) -> Value {
        if !self.is_authoritative() {
            return Value::Null;
        }
        let Some(result) = result else {
            return Value::Null;
        };
        let mut nodes = result.nodes.into_iter().map(worker_node_info);
        if list {
            Value::Array(nodes.collect())
        } else {
            nodes.next().unwrap_or(Value::Null)
        }
    }

    pub(super) fn is_configured(&self) -> bool {
        self.sender.is_some() && self.accepting.load(Ordering::Acquire)
    }

    pub(super) fn needs_document_sync(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        expected_queries: &crate::tree_worker::WorkerQuerySources,
    ) -> bool {
        if !self.is_enabled() {
            return false;
        }
        let documents = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::needs_document_sync");
        let document_stale = documents.current.get(uri.as_str()).is_none_or(|document| {
            document.context.incarnation != incarnation
                || document.context.content_version != content_version
                || document.context.configuration_generation != configuration_generation
                || &document.queries != expected_queries
        });
        let catalog_stale = documents
            .catalog
            .as_ref()
            .is_none_or(|(generation, catalog)| {
                *generation != configuration_generation || catalog.assets.is_empty()
            });
        document_stale || catalog_stale
    }

    pub(super) async fn resolve_node(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        byte_offset: usize,
        layer: crate::tree_worker::NodeLayerSelector,
    ) -> Option<NodeResult> {
        let (client, context) = if layer == crate::tree_worker::NodeLayerSelector::Host {
            let client = self.read_client_wait().await?;
            if !self.is_enabled() {
                return None;
            }
            let context = self
                .synchronized_read_context_wait(
                    &client,
                    uri,
                    incarnation,
                    content_version,
                    configuration_generation,
                )
                .await?;
            (client, context)
        } else {
            self.synchronized_query_read(
                uri,
                incarnation,
                content_version,
                configuration_generation,
            )
            .await?
        };
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.resolve_node(ResolveNode {
                context,
                byte_offset,
                named: false,
                layer,
            })
        })
        .await
        .ok()?
        .ok()?;
        self.admit_node_response(response, &expected)
    }

    pub(super) async fn selection_ranges(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        positions: Vec<WirePosition>,
    ) -> Option<SelectionRangesResult> {
        let (client, context) = self
            .synchronized_query_read(uri, incarnation, content_version, configuration_generation)
            .await?;
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.derive_selection_ranges(DeriveSelectionRanges { context, positions })
        })
        .await
        .ok()?
        .ok()?;
        let Response::SelectionRanges(result) = response else {
            return None;
        };
        if result.context != expected {
            return None;
        }
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::selection_ranges(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) || self
            .read_client
            .load()
            .as_ref()
            .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            return None;
        }
        Some(result)
    }

    pub(super) async fn injection_regions(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> Option<InjectionRegionsResult> {
        let (client, context) = self
            .synchronized_query_read(uri, incarnation, content_version, configuration_generation)
            .await?;
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.derive_injection_regions(DeriveInjectionRegions { context })
        })
        .await
        .ok()?
        .ok()?;
        let Response::InjectionRegions(result) = response else {
            return None;
        };
        if result.context != expected {
            return None;
        }
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::injection_regions(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) || self
            .read_client
            .load()
            .as_ref()
            .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            return None;
        }
        self.cache_worker_regions(uri, &result);
        Some(result)
    }

    fn cache_worker_regions(&self, uri: &url::Url, result: &InjectionRegionsResult) {
        let regions = Arc::new(
            result
                .regions
                .iter()
                .map(worker_region_to_resolved)
                .collect::<Vec<_>>(),
        );
        for region in result.regions.iter() {
            self.worker_nodes.insert(
                (uri.as_str().to_string(), region.region_id.clone()),
                OpaqueNodeId {
                    worker_generation: result.context.worker_generation,
                    local_id: region.region_id.clone(),
                },
            );
        }
        let mut documents = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::cache_worker_regions");
        if documents.acknowledges(&result.context) {
            documents.worker_regions.insert(
                uri.as_str().to_string(),
                WorkerRegionSnapshot {
                    context: result.context.clone(),
                    regions,
                },
            );
        }
    }

    pub(super) fn cached_injection_regions(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> Option<Arc<Vec<crate::language::injection::ResolvedInjection>>> {
        let client = self.read_client.load_full()?;
        let documents = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::cached_injection_regions");
        let snapshot = documents.worker_regions.get(uri.as_str())?;
        (snapshot.context.worker_generation == client.worker_generation()
            && snapshot.context.incarnation == incarnation
            && snapshot.context.content_version == content_version
            && snapshot.context.configuration_generation == configuration_generation)
            .then(|| Arc::clone(&snapshot.regions))
    }

    pub(super) async fn worker_injection_regions(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> Option<Arc<Vec<crate::language::injection::ResolvedInjection>>> {
        if let Some(regions) = self.cached_injection_regions(
            uri,
            incarnation,
            content_version,
            configuration_generation,
        ) {
            return Some(regions);
        }
        self.injection_regions(uri, incarnation, content_version, configuration_generation)
            .await?;
        self.cached_injection_regions(uri, incarnation, content_version, configuration_generation)
    }

    pub(super) async fn semantic_tokens(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        supports_multiline: bool,
        supersede: Option<crate::cancel::CancelToken>,
    ) -> Option<DerivedSemanticTokens> {
        let (client, context) = self
            .synchronized_query_read(uri, incarnation, content_version, configuration_generation)
            .await?;
        let expected = context.clone();
        let started = Instant::now();
        let pending = client
            .begin_derive_semantic_tokens(DeriveSemanticTokens {
                context,
                supports_multiline,
            })
            .ok()?;
        let mut cancellation =
            WorkerRequestCancellation::new(Arc::clone(&client), expected.request_id);
        #[cfg(feature = "e2e")]
        if std::env::var("KAKEHASHI_TREE_WORKER_RESPONSE_WAITER_DELAY_URI_SUFFIX")
            .ok()
            .is_some_and(|suffix| expected.uri.ends_with(&suffix))
        {
            if let Ok(path) =
                std::env::var("KAKEHASHI_TREE_WORKER_RESPONSE_WAITER_DELAY_STARTED_FILE")
            {
                let _ = std::fs::write(path, b"started");
            }
            if let Ok(milliseconds) =
                std::env::var("KAKEHASHI_TREE_WORKER_RESPONSE_WAITER_DELAY_MS")
                    .unwrap_or_default()
                    .parse::<u64>()
            {
                tokio::time::sleep(Duration::from_millis(milliseconds)).await;
            }
        }
        let response = tokio::task::spawn_blocking(move || client.wait_for_response(pending));
        let response = if let Some(supersede) = supersede {
            tokio::select! {
                biased;
                _ = supersede.cancelled() => return None,
                response = response => response,
            }
        } else {
            response.await
        };
        cancellation.disarm();
        let response = response.ok()?.ok()?;
        let Response::SemanticTokens(result) = response else {
            return None;
        };
        if result.context != expected {
            return None;
        }
        log::debug!(
            target: "kakehashi::tree_worker_shadow_metrics",
            "semantic uri={} version={} parent_us={} queue_us={} compute_us={} tokens={} cache_hit={}",
            result.context.uri,
            result.context.content_version,
            started.elapsed().as_micros(),
            result.queue_wait_ns / 1_000,
            result.compute_ns / 1_000,
            result.tokens.len(),
            result.cache_hit,
        );
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::semantic_tokens(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) || self
            .read_client
            .load()
            .as_ref()
            .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            return None;
        }
        Some(result)
    }

    pub(super) fn compare_semantic_tokens(
        &self,
        uri: &url::Url,
        content_version: u64,
        authoritative: &[tower_lsp_server::ls_types::SemanticToken],
        worker: &DerivedSemanticTokens,
    ) {
        let worker = worker
            .tokens
            .iter()
            .map(|token| tower_lsp_server::ls_types::SemanticToken {
                delta_line: token.delta_line,
                delta_start: token.delta_start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            })
            .collect::<Vec<_>>();
        if authoritative != worker {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "semantic token mismatch uri={uri} version={content_version} authoritative={} worker={}",
                authoritative.len(),
                worker.len(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn captures(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        kind: String,
        range: Option<WireRange>,
        injection: bool,
    ) -> Option<CapturesResult> {
        let (client, context) = self
            .synchronized_query_read(uri, incarnation, content_version, configuration_generation)
            .await?;
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.run_captures(RunCaptures {
                context,
                kind,
                range,
                injection,
            })
        })
        .await
        .ok()?
        .ok()?;
        let Response::Captures(result) = response else {
            return None;
        };
        if result.context != expected {
            return None;
        }
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::captures(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) || self
            .read_client
            .load()
            .as_ref()
            .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            return None;
        }
        Some(result)
    }

    pub(super) fn compare_captures(
        &self,
        uri: &url::Url,
        content_version: u64,
        authoritative: Option<&(Vec<Value>, Vec<Value>)>,
        worker: &CapturesResult,
    ) {
        let normalize = |values: &[Value]| {
            let mut values = values.to_vec();
            for value in &mut values {
                remove_capture_ids(value);
            }
            values
        };
        let authoritative_available = authoritative.is_some();
        let authoritative_matches = authoritative
            .map(|(matches, _)| normalize(matches))
            .unwrap_or_default();
        let worker_matches = normalize(&worker.matches);
        let authoritative_skipped = authoritative
            .map(|(_, skipped)| skipped.as_slice())
            .unwrap_or_default();
        let matches = authoritative_available == worker.available
            && authoritative_matches == worker_matches
            && authoritative_skipped == worker.skipped;
        if !matches {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "captures mismatch uri={uri} version={content_version} authoritative={} worker={}",
                authoritative_matches.len(),
                worker_matches.len(),
            );
            return;
        }
        let mut authoritative_ids = Vec::new();
        if let Some((matches, _)) = authoritative {
            matches
                .iter()
                .for_each(|value| capture_node_ids(value, &mut authoritative_ids));
        }
        let mut worker_ids = Vec::new();
        worker
            .matches
            .iter()
            .for_each(|value| capture_node_ids(value, &mut worker_ids));
        for (authoritative_id, worker_id) in authoritative_ids.into_iter().zip(worker_ids) {
            let opaque = OpaqueNodeId {
                worker_generation: worker.context.worker_generation,
                local_id: worker_id.clone(),
            };
            self.worker_nodes
                .insert((uri.as_str().to_string(), authoritative_id), opaque.clone());
            self.worker_nodes
                .insert((uri.as_str().to_string(), worker_id), opaque);
        }
    }

    pub(super) async fn native_bindings(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        position: WirePosition,
    ) -> Option<NativeBindingsResult> {
        let Some((client, context)) = self
            .synchronized_query_read(uri, incarnation, content_version, configuration_generation)
            .await
        else {
            log::debug!(target: "kakehashi::tree_worker_shadow", "native bindings skipped before worker inputs synchronized uri={uri} version={content_version}");
            return None;
        };
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.derive_native_bindings(DeriveNativeBindings { context, position })
        })
        .await;
        let Ok(Ok(response)) = response else {
            log::debug!(target: "kakehashi::tree_worker_shadow", "native bindings worker request failed uri={uri}");
            return None;
        };
        let Response::NativeBindings(result) = response else {
            log::debug!(target: "kakehashi::tree_worker_shadow", "native bindings worker returned a non-bindings response uri={uri}: {response:?}");
            return None;
        };
        if result.context != expected {
            log::debug!(target: "kakehashi::tree_worker_shadow", "native bindings response context mismatch uri={uri}");
            return None;
        }
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::native_bindings(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) || self
            .read_client
            .load()
            .as_ref()
            .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            log::debug!(target: "kakehashi::tree_worker_shadow", "native bindings response became stale uri={uri} version={content_version}");
            return None;
        }
        Some(result)
    }

    pub(super) fn compare_native_bindings(
        &self,
        uri: &url::Url,
        content_version: u64,
        authoritative: Option<&NativeBindingsFacts>,
        worker: &NativeBindingsResult,
    ) {
        if authoritative != worker.facts.as_ref() {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "native bindings mismatch uri={uri} version={content_version} authoritative={authoritative:?} worker={:?}",
                worker.facts,
            );
        } else {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "native bindings matched uri={uri} version={content_version}",
            );
        }
    }

    pub(super) fn compare_injection_regions(
        &self,
        uri: &url::Url,
        content_version: u64,
        authoritative: &[crate::language::injection::ResolvedInjection],
        worker_result: &InjectionRegionsResult,
    ) {
        let comparable_authoritative = authoritative
            .iter()
            .map(|region| ComparableInjection {
                language: &region.injection_language,
                byte_range: region.region.byte_range.clone(),
                line_range: region.region.line_range.clone(),
                start_column: region.region.start_column,
                content_hash: region.region.content_hash,
                virtual_content: &region.virtual_content,
                line_column_offsets: &region.line_column_offsets,
                contiguous: region.contiguous,
            })
            .collect::<Vec<_>>();
        let comparable_worker = worker_result
            .regions
            .iter()
            .map(|region| ComparableInjection {
                language: &region.language,
                byte_range: region.byte_start..region.byte_end,
                line_range: region.line_start..region.line_end,
                start_column: region.start_column,
                content_hash: region.content_hash,
                virtual_content: &region.virtual_content,
                line_column_offsets: &region.line_column_offsets,
                contiguous: region.contiguous,
            })
            .collect::<Vec<_>>();
        if comparable_authoritative != comparable_worker {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "injection region mismatch uri={uri} version={content_version} authoritative={comparable_authoritative:?} worker={comparable_worker:?}",
            );
            return;
        }
        for (authoritative_region, worker_region) in
            authoritative.iter().zip(worker_result.regions.iter())
        {
            let opaque = OpaqueNodeId {
                worker_generation: worker_result.context.worker_generation,
                local_id: worker_region.region_id.clone(),
            };
            self.worker_nodes.insert(
                (
                    uri.as_str().to_string(),
                    authoritative_region.region.region_id.clone(),
                ),
                opaque.clone(),
            );
            self.worker_nodes.insert(
                (uri.as_str().to_string(), worker_region.region_id.clone()),
                opaque,
            );
        }
        log::debug!(
            target: "kakehashi::tree_worker_shadow",
            "injection regions matched uri={uri} version={content_version}",
        );
    }

    pub(super) fn record_node_mapping(
        &self,
        uri: &url::Url,
        authoritative: &Value,
        worker: &crate::tree_worker::OwnedNode,
    ) {
        let Some(id) = authoritative.get("id").and_then(Value::as_str) else {
            return;
        };
        self.worker_nodes.insert(
            (uri.as_str().to_string(), id.to_string()),
            worker.id.clone(),
        );
        self.worker_nodes.insert(
            (uri.as_str().to_string(), worker.id.local_id.clone()),
            worker.id.clone(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn compare_node_navigation(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
        authoritative_input_id: &str,
        operation: NodeNavigation,
        authoritative: &Value,
    ) -> Option<NodeResult> {
        let key = (uri.as_str().to_string(), authoritative_input_id.to_string());
        let node_id = self.worker_nodes.get(&key).map(|entry| entry.clone())?;
        let client = self.read_client.load_full()?;
        if !self.is_enabled() || node_id.worker_generation != client.worker_generation() {
            self.worker_nodes.remove(&key);
            return None;
        }
        let context = self.read_context(
            &client,
            uri,
            incarnation,
            content_version,
            configuration_generation,
        );
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.navigate_node(NavigateNode {
                context,
                node_id,
                operation,
            })
        })
        .await
        .ok()
        .and_then(Result::ok);
        let result = response.and_then(|response| self.admit_node_response(response, &expected))?;
        let authoritative_nodes = match authoritative {
            Value::Null => Vec::new(),
            Value::Object(_) => vec![authoritative],
            Value::Array(nodes) => nodes.iter().collect(),
            _ => return None,
        };
        let authoritative_kinds = authoritative_nodes
            .iter()
            .filter_map(|node| node.get("kind").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let worker_kinds = result
            .nodes
            .iter()
            .map(|node| node.kind.as_str())
            .collect::<Vec<_>>();
        if authoritative_kinds != worker_kinds {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "node navigation mismatch uri={} version={} authoritative={:?} worker={:?}",
                uri,
                content_version,
                authoritative_kinds,
                worker_kinds,
            );
            return Some(result);
        }
        for (authoritative_node, worker_node) in
            authoritative_nodes.into_iter().zip(result.nodes.iter())
        {
            self.record_node_mapping(uri, authoritative_node, worker_node);
        }
        log::debug!(
            target: "kakehashi::tree_worker_shadow",
            "node navigation matched uri={} version={}",
            uri,
            content_version,
        );
        Some(result)
    }

    pub(super) async fn node_scalar(
        &self,
        uri: &url::Url,
        authoritative_input_id: &str,
        operation: NodeScalarOperation,
    ) -> Option<NodeScalarValue> {
        let key = (uri.as_str().to_string(), authoritative_input_id.to_string());
        let node_id = self.worker_nodes.get(&key).map(|entry| entry.clone())?;
        let sync = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::node_scalar(context)")
            .current
            .get(uri.as_str())
            .cloned()?;
        let client = self.read_client.load_full()?;
        if !self.is_enabled() || node_id.worker_generation != client.worker_generation() {
            self.worker_nodes.remove(&key);
            return None;
        }
        let context = self.read_context(
            &client,
            uri,
            sync.context.incarnation,
            sync.context.content_version,
            sync.context.configuration_generation,
        );
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.run_node_scalar(RunNodeScalar {
                context,
                node_id,
                operation,
            })
        })
        .await
        .ok()?
        .ok()?;
        let Response::NodeScalar(result) = response else {
            return None;
        };
        if result.context != expected {
            return None;
        }
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::node_scalar(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) || self
            .read_client
            .load()
            .as_ref()
            .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            return None;
        }
        result.value
    }

    pub(super) async fn node_navigation(
        &self,
        uri: &url::Url,
        authoritative_input_id: &str,
        operation: NodeNavigation,
    ) -> Option<NodeResult> {
        let key = (uri.as_str().to_string(), authoritative_input_id.to_string());
        let node_id = self.worker_nodes.get(&key).map(|entry| entry.clone())?;
        let sync = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::node_navigation(context)")
            .current
            .get(uri.as_str())
            .cloned()?;
        let client = self.read_client.load_full()?;
        if !self.is_enabled() || node_id.worker_generation != client.worker_generation() {
            self.worker_nodes.remove(&key);
            return None;
        }
        let context = self.read_context(
            &client,
            uri,
            sync.context.incarnation,
            sync.context.content_version,
            sync.context.configuration_generation,
        );
        let expected = context.clone();
        let response = tokio::task::spawn_blocking(move || {
            client.navigate_node(NavigateNode {
                context,
                node_id,
                operation,
            })
        })
        .await
        .ok()?
        .ok()?;
        let result = self.admit_node_response(response, &expected)?;
        let current = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::node_navigation(admission)")
            .current
            .get(uri.as_str())
            .map(|current| current.context.clone());
        if current.as_ref().is_none_or(|current| {
            current.incarnation != expected.incarnation
                || current.content_version != expected.content_version
                || current.configuration_generation != expected.configuration_generation
        }) {
            return None;
        }
        Some(result)
    }

    pub(super) async fn node_navigation_with_descendant(
        &self,
        uri: &url::Url,
        authoritative_input_id: &str,
        authoritative_descendant_id: &str,
    ) -> Option<NodeResult> {
        let descendant_id = self
            .worker_nodes
            .get(&(
                uri.as_str().to_string(),
                authoritative_descendant_id.to_string(),
            ))
            .map(|entry| entry.clone())?;
        self.node_navigation(
            uri,
            authoritative_input_id,
            NodeNavigation::ChildWithDescendant { descendant_id },
        )
        .await
    }

    pub(super) fn compare_node_result(
        &self,
        uri: &url::Url,
        operation: &NodeNavigation,
        authoritative: &Value,
        worker: &NodeResult,
    ) {
        self.compare_node_result_label(uri, &format!("{operation:?}"), authoritative, worker);
    }

    pub(super) fn compare_node_result_label(
        &self,
        uri: &url::Url,
        operation: &str,
        authoritative: &Value,
        worker: &NodeResult,
    ) {
        let authoritative_nodes = match authoritative {
            Value::Null => Vec::new(),
            Value::Object(_) => vec![authoritative],
            Value::Array(nodes) => nodes.iter().collect(),
            _ => return,
        };
        let authoritative_kinds = authoritative_nodes
            .iter()
            .filter_map(|node| node.get("kind").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let worker_kinds = worker
            .nodes
            .iter()
            .map(|node| node.kind.as_str())
            .collect::<Vec<_>>();
        if authoritative_kinds != worker_kinds {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "node navigation mismatch operation={operation} uri={uri} authoritative={authoritative_kinds:?} worker={worker_kinds:?}",
            );
            return;
        }
        for (authoritative_node, worker_node) in
            authoritative_nodes.into_iter().zip(worker.nodes.iter())
        {
            self.record_node_mapping(uri, authoritative_node, worker_node);
        }
        log::debug!(
            target: "kakehashi::tree_worker_shadow",
            "node navigation matched operation={operation} uri={uri}",
        );
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
        self.observe_and_submit(ShadowCommand::Apply {
            request,
            fallback,
            derive_snapshot: should_derive_edit_snapshot(self.mode),
        });
    }

    pub(super) fn mirror_close(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) {
        self.comparisons.mark_closed(uri.as_str(), incarnation);
        self.worker_nodes
            .retain(|(mapped_uri, _), _| mapped_uri != uri.as_str());
        self.observe_and_submit(ShadowCommand::Close(CloseDocument {
            context: self.context(uri, incarnation, content_version, configuration_generation),
        }));
    }

    pub(super) fn configuration_changed(&self, generation: u64) {
        self.pending_configuration_generation
            .fetch_max(generation, Ordering::AcqRel);
    }

    #[cfg(test)]
    pub(super) fn mirror_configuration(&self, generation: u64, documents: Vec<MirrorDocument>) {
        self.mirror_configuration_with_catalog(
            generation,
            WorkerLanguageCatalog::default(),
            documents,
        );
    }

    pub(super) fn mirror_configuration_with_catalog(
        &self,
        generation: u64,
        catalog: WorkerLanguageCatalog,
        documents: Vec<MirrorDocument>,
    ) {
        self.remember_catalog(generation, catalog);
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
                ShadowCommand::Forget(self.context(
                    &document.uri,
                    document.incarnation,
                    document.content_version,
                    generation,
                ))
            };
            commands.push(command);
        }
        {
            let mut registry = self
                .open_documents
                .lock()
                .recover_poison("TreeWorkerShadow::mirror_configuration");
            commands.retain(|command| registry.observe(command) != Observation::Stale);
            if !commands.is_empty() {
                self.registry_epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
        for command in &commands {
            if let Some((uri, incarnation)) = command_document(command) {
                if matches!(command, ShadowCommand::Close(_) | ShadowCommand::Forget(_)) {
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

    fn remember_catalog(&self, generation: u64, catalog: WorkerLanguageCatalog) {
        if catalog.assets.is_empty() {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "ignored empty worker language catalog for generation {generation}",
            );
            return;
        }
        log::debug!(
            target: "kakehashi::tree_worker_shadow",
            "remembered {} worker language assets for generation {generation}",
            catalog.assets.len(),
        );
        let mut documents = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::remember_catalog");
        documents.catalog = Some((generation, catalog));
        documents.catalog_preload = None;
        documents.catalog_acknowledged = None;
    }

    fn preload_catalog_async(&self) {
        if !self.is_enabled() {
            return;
        }
        let Some(client) = self.read_client.load_full() else {
            return;
        };
        let worker_generation = client.worker_generation();
        let (configuration_generation, catalog) = {
            let mut documents = self
                .open_documents
                .lock()
                .recover_poison("TreeWorkerShadow::preload_catalog_async");
            let Some((configuration_generation, catalog)) = documents.catalog.clone() else {
                return;
            };
            if documents.catalog_preload == Some((worker_generation, configuration_generation)) {
                return;
            }
            documents.catalog_preload = Some((worker_generation, configuration_generation));
            (configuration_generation, catalog)
        };
        spawn_catalog_preload(
            client,
            configuration_generation,
            catalog,
            Arc::clone(&self.open_documents),
        );
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
            queries: grammar.queries,
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

    fn read_context(
        &self,
        client: &Client,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> RequestContext {
        RequestContext {
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            worker_generation: client.worker_generation(),
            uri: uri.as_str().to_string(),
            incarnation,
            content_version,
            configuration_generation,
        }
    }

    async fn synchronized_query_read(
        &self,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> Option<(Arc<Client>, RequestContext)> {
        let client = self.read_client_wait().await?;
        if !self.is_enabled() {
            return None;
        }
        self.preload_catalog_async();
        if !self
            .catalog_ready_wait(&client, configuration_generation)
            .await
        {
            return None;
        }
        let context = self
            .synchronized_read_context_wait(
                &client,
                uri,
                incarnation,
                content_version,
                configuration_generation,
            )
            .await?;
        Some((client, context))
    }

    async fn read_client_wait(&self) -> Option<Arc<Client>> {
        let changed = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::read_client_wait(notify)")
            .replica_changed
            .clone();
        loop {
            let notified = changed.notified();
            if let Some(client) = self.read_client.load_full() {
                return Some(client);
            }
            if self.disabled.load(Ordering::Acquire)
                || self.sender.is_none()
                || tokio::time::timeout(WORKER_READ_READY_TIMEOUT, notified)
                    .await
                    .is_err()
            {
                return None;
            }
        }
    }

    async fn catalog_ready_wait(&self, client: &Client, configuration_generation: u64) -> bool {
        let expected = (client.worker_generation(), configuration_generation);
        let changed = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::catalog_ready_wait(notify)")
            .replica_changed
            .clone();
        loop {
            let notified = changed.notified();
            {
                let documents = self
                    .open_documents
                    .lock()
                    .recover_poison("TreeWorkerShadow::catalog_ready_wait(check)");
                if documents.catalog_acknowledged == Some(expected) {
                    return true;
                }
                if documents
                    .catalog
                    .as_ref()
                    .is_none_or(|(generation, _)| *generation != configuration_generation)
                {
                    return false;
                }
            }
            if !self.is_enabled()
                || tokio::time::timeout(WORKER_READ_READY_TIMEOUT, notified)
                    .await
                    .is_err()
            {
                return false;
            }
        }
    }

    async fn synchronized_read_context_wait(
        &self,
        client: &Client,
        uri: &url::Url,
        incarnation: u64,
        content_version: u64,
        configuration_generation: u64,
    ) -> Option<RequestContext> {
        let context = self.read_context(
            client,
            uri,
            incarnation,
            content_version,
            configuration_generation,
        );
        let changed = self
            .open_documents
            .lock()
            .recover_poison("TreeWorkerShadow::synchronized_read_context_wait(notify)")
            .replica_changed
            .clone();
        loop {
            let notified = changed.notified();
            {
                let documents = self
                    .open_documents
                    .lock()
                    .recover_poison("TreeWorkerShadow::synchronized_read_context_wait(check)");
                if documents.acknowledges(&context) {
                    return Some(context);
                }
                if !documents.expects(&context) {
                    log::debug!(
                        target: "kakehashi::tree_worker_shadow",
                        "read context is not current expected={context:?} current={:?}",
                        documents
                            .current
                            .get(&context.uri)
                            .map(|document| &document.context),
                    );
                    return None;
                }
            }
            if !self.is_enabled()
                || tokio::time::timeout(WORKER_READ_READY_TIMEOUT, notified)
                    .await
                    .is_err()
            {
                return None;
            }
        }
    }

    fn admit_node_response(
        &self,
        response: Response,
        expected: &RequestContext,
    ) -> Option<NodeResult> {
        let Response::Nodes(result) = response else {
            return None;
        };
        if &result.context != expected
            || self
                .read_client
                .load()
                .as_ref()
                .is_none_or(|client| client.worker_generation() != expected.worker_generation)
        {
            return None;
        }
        Some(result)
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
        let observation = {
            let mut registry = self
                .open_documents
                .lock()
                .recover_poison("TreeWorkerShadow::observe_and_submit");
            let observation = registry.observe(&command);
            if observation != Observation::Stale {
                self.registry_epoch.fetch_add(1, Ordering::AcqRel);
            }
            observation
        };
        if observation == Observation::Stale {
            return;
        }
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

impl Kakehashi {
    /// Resolve injection facts from the worker against one current live-text
    /// identity. Authoritative readers use this instead of waiting for (or
    /// accidentally reviving) a parent-owned parse snapshot after an edit.
    pub(super) async fn current_worker_injection_view(
        &self,
        uri: &url::Url,
    ) -> Option<WorkerInjectionView> {
        let (text, incarnation, content_version, language_id) =
            self.documents.get(uri).map(|document| {
                (
                    document.text_arc(),
                    document.incarnation(),
                    document.content_version(),
                    document.language_id().map(str::to_owned),
                )
            })?;
        let language =
            self.language
                .detect_language(uri.path(), &text, None, language_id.as_deref())?;
        if !self
            .language
            .ensure_language_loaded_async(&language)
            .await
            .success
        {
            return None;
        }
        let current = self.documents.get(uri)?;
        if current.incarnation() != incarnation || current.content_version() != content_version {
            return None;
        }
        drop(current);
        let regions = self
            .worker_injection_regions_for_snapshot(uri, incarnation, content_version, &language)
            .await?;
        let current = self.documents.get(uri)?;
        if current.incarnation() != incarnation || current.content_version() != content_version {
            return None;
        }
        drop(current);
        Some(WorkerInjectionView {
            text,
            language,
            regions,
        })
    }

    pub(super) async fn worker_injection_region_at_snapshot(
        &self,
        uri: &url::Url,
        incarnation: u64,
        parsed_version: u64,
        language: &str,
        byte_offset: usize,
    ) -> Option<crate::language::injection::ResolvedInjection> {
        let configuration_generation = self.language.configuration_generation();
        if let Some(grammar) = self.language.worker_grammar_descriptor(language)
            && self.tree_worker_shadow.needs_document_sync(
                uri,
                incarnation,
                parsed_version,
                configuration_generation,
                &grammar.queries,
            )
        {
            self.refresh_tree_worker_documents(std::slice::from_ref(uri));
        }
        let node = self
            .tree_worker_shadow
            .resolve_node(
                uri,
                incarnation,
                parsed_version,
                configuration_generation,
                byte_offset,
                crate::tree_worker::NodeLayerSelector::Deepest,
            )
            .await?
            .nodes
            .into_iter()
            .next()?;
        if node.layer == 0 {
            return None;
        }
        self.tree_worker_shadow
            .worker_injection_regions(uri, incarnation, parsed_version, configuration_generation)
            .await?
            .iter()
            .find(|resolved| resolved.region.byte_range.contains(&byte_offset))
            .cloned()
    }

    pub(super) async fn worker_injection_regions_for_snapshot(
        &self,
        uri: &url::Url,
        incarnation: u64,
        parsed_version: u64,
        language: &str,
    ) -> Option<Arc<Vec<crate::language::injection::ResolvedInjection>>> {
        let configuration_generation = self.language.configuration_generation();
        if let Some(grammar) = self.language.worker_grammar_descriptor(language)
            && self.tree_worker_shadow.needs_document_sync(
                uri,
                incarnation,
                parsed_version,
                configuration_generation,
                &grammar.queries,
            )
        {
            self.refresh_tree_worker_documents(std::slice::from_ref(uri));
        }
        self.tree_worker_shadow
            .worker_injection_regions(uri, incarnation, parsed_version, configuration_generation)
            .await
    }

    pub(super) async fn shadow_compare_current_injection_regions(
        &self,
        uri: &url::Url,
        incarnation: u64,
        parsed_version: u64,
        authoritative: &[crate::language::injection::ResolvedInjection],
    ) {
        let Some(view) = self.documents.latest_snapshot(uri) else {
            return;
        };
        if view.slot.current_incarnation != incarnation
            || view.content_version != parsed_version
            || view.slot.snapshot.as_ref().is_none_or(|current| {
                current.incarnation != incarnation || current.parsed_version != parsed_version
            })
        {
            return;
        }
        let configuration_generation = self.language.configuration_generation();
        let Some(language) = view
            .slot
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.language.as_deref())
        else {
            return;
        };
        if let Some(grammar) = self.language.worker_grammar_descriptor(language)
            && self.tree_worker_shadow.needs_document_sync(
                uri,
                incarnation,
                parsed_version,
                configuration_generation,
                &grammar.queries,
            )
        {
            self.refresh_tree_worker_documents(std::slice::from_ref(uri));
        }
        let Some(worker_regions) = self
            .tree_worker_shadow
            .injection_regions(uri, incarnation, parsed_version, configuration_generation)
            .await
        else {
            return;
        };
        self.tree_worker_shadow.compare_injection_regions(
            uri,
            parsed_version,
            authoritative,
            &worker_regions,
        );
        if self
            .tree_worker_shadow
            .cached_injection_regions(uri, incarnation, parsed_version, configuration_generation)
            .is_none()
        {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "current worker injection snapshot was not cached uri={uri} version={parsed_version}",
            );
        }
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

fn spawn_catalog_preload(
    client: Arc<Client>,
    configuration_generation: u64,
    catalog: WorkerLanguageCatalog,
    open_documents: Arc<Mutex<OpenDocuments>>,
) {
    let worker_generation = client.worker_generation();
    let context = RequestContext {
        request_id: NEXT_CATALOG_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        worker_generation: client.worker_generation(),
        uri: "kakehashi://language-catalog".into(),
        incarnation: 0,
        content_version: 0,
        configuration_generation,
    };
    let _ = std::thread::Builder::new()
        .name("kakehashi-worker-catalog".into())
        .spawn(move || {
            let outcome = client.configure_languages(ConfigureLanguages { context, catalog });
            let mut documents = open_documents
                .lock()
                .recover_poison("spawn_catalog_preload(completion)");
            if matches!(outcome, Ok(Response::LanguageCatalogAck(_))) {
                documents.catalog_acknowledged =
                    Some((worker_generation, configuration_generation));
                documents.replica_changed.notify_waiters();
            } else {
                if documents.catalog_preload == Some((worker_generation, configuration_generation))
                {
                    documents.catalog_preload = None;
                }
                documents.replica_changed.notify_waiters();
                log::debug!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker catalog preload did not complete: {outcome:?}",
                );
            }
        });
}

fn log_incomplete_shutdown(reason: &str) {
    log::error!(
        target: "kakehashi::tree_worker_shadow_metrics",
        "shadow validation incomplete: {reason}"
    );
}

fn configured_mode() -> TreeWorkerMode {
    parse_mode(
        std::env::var(MODE_ENV).ok().as_deref(),
        std::env::var(SHADOW_ENV).ok().as_deref(),
    )
}

fn should_derive_edit_snapshot(mode: TreeWorkerMode) -> bool {
    mode == TreeWorkerMode::Shadow
}

fn parse_mode(mode: Option<&str>, legacy_shadow: Option<&str>) -> TreeWorkerMode {
    match mode {
        Some("authoritative") => TreeWorkerMode::Authoritative,
        Some("shadow") => TreeWorkerMode::Shadow,
        Some("legacy") => TreeWorkerMode::Legacy,
        Some(value) => {
            log::warn!(
                target: "kakehashi::tree_worker_shadow",
                "unsupported {MODE_ENV}={value:?}; using legacy tree ownership",
            );
            TreeWorkerMode::Legacy
        }
        None if legacy_shadow.is_some_and(|value| matches!(value, "1" | "true" | "TRUE")) => {
            TreeWorkerMode::Shadow
        }
        None => TreeWorkerMode::Legacy,
    }
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
        read_client,
        worker_nodes,
    } = shared;
    let initial_client = match Client::spawn(&executable, compute_threads, worker_generation) {
        Ok(client) => Some(Arc::new(client)),
        Err(error) => {
            log::error!(target: "kakehashi::tree_worker_shadow", "worker spawn failed: {error}");
            None
        }
    };
    let initial_healthy_since = initial_client.as_ref().map(|_| Instant::now());
    read_client.store(initial_client.clone());
    open_documents
        .lock()
        .recover_poison("run_actor(notify initial client)")
        .replica_changed
        .notify_waiters();
    let mut state = SupervisorState {
        client: initial_client,
        read_client,
        worker_nodes,
        worker_generation,
        replicas: HashMap::new(),
        open_documents,
        quarantined: HashSet::new(),
        restart_budget: RestartBudget::default(),
        restart_policy: RestartPolicy::from_environment(),
        breaker: BreakerState::Closed,
        healthy_since: initial_healthy_since,
        long_healthy_since: initial_healthy_since,
    };
    if state.client.is_none()
        && !recover_worker(
            &mut state,
            &executable,
            compute_threads,
            WorkerLossEvidence::systemic(),
            &comparisons,
            RecoveryMode::Normal,
        )
    {
        mark_tree_tier_unavailable(&mut state, &disabled, &pending_configuration_generation);
    }
    let mut shutdown_ack = None;
    loop {
        if disabled.load(Ordering::Acquire) && matches!(state.breaker, BreakerState::Closed) {
            mark_tree_tier_unavailable(&mut state, &disabled, &pending_configuration_generation);
        }
        if disabled.load(Ordering::Acquire) {
            let generation = pending_configuration_generation.load(Ordering::Acquire);
            let probe_allowed = breaker_probe_allowed(state.breaker, generation, Instant::now());
            if probe_allowed {
                let recovery_epoch = registry_epoch.load(Ordering::Acquire);
                state.breaker = BreakerState::HalfOpen {
                    probe_generation: generation,
                    synchronized_epoch: recovery_epoch,
                    reconcile_after: Instant::now(),
                };
                if recover_worker(
                    &mut state,
                    &executable,
                    compute_threads,
                    WorkerLossEvidence::systemic(),
                    &comparisons,
                    RecoveryMode::HalfOpen,
                ) {
                    if finish_half_open_recovery(
                        &mut state,
                        &registry_epoch,
                        &comparisons,
                        &disabled,
                    ) == ReconcileOutcome::Failed
                    {
                        mark_tree_tier_unavailable(
                            &mut state,
                            &disabled,
                            &pending_configuration_generation,
                        );
                        continue;
                    }
                    if !disabled.load(Ordering::Acquire) {
                        log::info!(
                            target: "kakehashi::tree_worker_shadow",
                            "entered shadow worker half-open probation for configuration generation {generation}",
                        );
                    }
                } else {
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                }
            }
        }
        if disabled.load(Ordering::Acquire)
            && matches!(state.breaker, BreakerState::HalfOpen { .. })
            && finish_half_open_recovery(&mut state, &registry_epoch, &comparisons, &disabled)
                == ReconcileOutcome::Failed
        {
            mark_tree_tier_unavailable(&mut state, &disabled, &pending_configuration_generation);
        }
        let mut command = match receiver.recv_timeout(WORKER_LIVENESS_POLL) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let Some(client) = state.client.as_ref() else {
                    continue;
                };
                let status = client.try_wait();
                let loss_error = match status {
                    Ok(None) => {
                        if !disabled.load(Ordering::Acquire) {
                            observe_healthy_service(&mut state, Instant::now());
                        }
                        continue;
                    }
                    Ok(Some(status)) => {
                        log::error!(
                            target: "kakehashi::tree_worker_shadow",
                            "worker generation {} exited while idle: {status}",
                            state.worker_generation,
                        );
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("tree worker exited while idle: {status}"),
                        )
                    }
                    Err(error) => {
                        log::error!(
                            target: "kakehashi::tree_worker_shadow",
                            "worker generation {} liveness check failed: {error}",
                            state.worker_generation,
                        );
                        error
                    }
                };
                let failure = worker_loss_evidence(client, &loss_error);
                if matches!(state.breaker, BreakerState::HalfOpen { .. }) {
                    let _ = apply_worker_loss_evidence(&mut state, failure, &comparisons);
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
                if !recover_worker(
                    &mut state,
                    &executable,
                    compute_threads,
                    failure,
                    &comparisons,
                    RecoveryMode::Normal,
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
        let replica_forgotten = matches!(&command, ShadowCommand::Forget(context) if state.replicas.contains_key(&context.uri));
        if artifact_replaced || replica_forgotten {
            log::info!(
                target: "kakehashi::tree_worker_shadow",
                "performing planned worker restart for parser artifact replacement",
            );
            if matches!(state.breaker, BreakerState::HalfOpen { .. }) {
                if let Err(error) = quiesce_published_client(&mut state) {
                    log::error!(
                        target: "kakehashi::tree_worker_shadow",
                        "half-open planned restart could not quiesce before disabling: {error}",
                    );
                }
                mark_tree_tier_unavailable(
                    &mut state,
                    &disabled,
                    &pending_configuration_generation,
                );
                continue;
            }
            if let Err(error) = quiesce_published_client(&mut state) {
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "disabled tree tier because planned restart could not quiesce: {error}",
                );
                mark_tree_tier_unavailable(
                    &mut state,
                    &disabled,
                    &pending_configuration_generation,
                );
                continue;
            }
            if !recover_worker(
                &mut state,
                &executable,
                compute_threads,
                WorkerLossEvidence::systemic(),
                &comparisons,
                RecoveryMode::Planned,
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
        if matches!(command, ShadowCommand::Forget(_)) {
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
                state
                    .open_documents
                    .lock()
                    .recover_poison("run_actor(acknowledge snapshot)")
                    .acknowledge(snapshot.context.clone());
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
            Ok(Response::DocumentAck(ack)) => {
                if let Some(identity) = synced_identity {
                    state.replicas.insert(ack.context.uri.clone(), identity);
                }
                state
                    .open_documents
                    .lock()
                    .recover_poison("run_actor(acknowledge edit)")
                    .acknowledge(ack.context);
            }
            Ok(Response::WorkerRestartRequired(required)) => {
                log::warn!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {} requested restart: {}",
                    state.worker_generation,
                    required.reason
                );
                if matches!(state.breaker, BreakerState::HalfOpen { .. }) {
                    if let Err(error) = quiesce_published_client(&mut state) {
                        log::error!(
                            target: "kakehashi::tree_worker_shadow",
                            "half-open worker-requested restart could not quiesce before disabling: {error}",
                        );
                    }
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
                if let Err(error) = quiesce_published_client(&mut state) {
                    log::error!(
                        target: "kakehashi::tree_worker_shadow",
                        "disabled tree tier because worker-requested restart could not quiesce: {error}",
                    );
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
                if !recover_worker(
                    &mut state,
                    &executable,
                    compute_threads,
                    WorkerLossEvidence::systemic(),
                    &comparisons,
                    RecoveryMode::Normal,
                ) {
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
                let client = state
                    .client
                    .as_ref()
                    .expect("failed request retains its worker client");
                let worker_was_terminated = client.was_terminated_for_request_deadline();
                let worker_transport_is_live =
                    client.try_wait().is_ok_and(|status| status.is_none());
                if may_continue_after_client_error(
                    &error,
                    worker_was_terminated,
                    worker_transport_is_live,
                ) {
                    log::warn!(
                        target: "kakehashi::tree_worker_shadow",
                        "worker request did not complete without transport loss: {error}",
                    );
                    if let Some((uri, incarnation)) = command_document {
                        comparisons.mark_closed(&uri, incarnation);
                    }
                    continue;
                }
                if is_local_client_error(&error) {
                    log::error!(
                        target: "kakehashi::tree_worker_shadow",
                        "disabled tree tier after local worker request admission failure: {error}",
                    );
                    if let Some((uri, incarnation)) = command_document {
                        comparisons.mark_closed(&uri, incarnation);
                    }
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
                let failure = worker_loss_evidence(
                    state
                        .client
                        .as_ref()
                        .expect("failed request retains its worker client"),
                    &error,
                );
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {} transport failed: {error}",
                    state.worker_generation,
                );
                if matches!(state.breaker, BreakerState::HalfOpen { .. }) {
                    let _ = apply_worker_loss_evidence(&mut state, failure, &comparisons);
                    mark_tree_tier_unavailable(
                        &mut state,
                        &disabled,
                        &pending_configuration_generation,
                    );
                    continue;
                }
                if !recover_worker(
                    &mut state,
                    &executable,
                    compute_threads,
                    failure,
                    &comparisons,
                    RecoveryMode::Normal,
                ) {
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
    if let Err(error) = retire_client(&mut state) {
        log::warn!(target: "kakehashi::tree_worker_shadow", "worker shutdown failed: {error}");
    }
    if let Some(ack) = shutdown_ack {
        let _ = ack.send(());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconcileOutcome {
    Current,
    Pending,
    Failed,
}

fn finish_half_open_recovery(
    state: &mut SupervisorState,
    registry_epoch: &AtomicU64,
    comparisons: &ComparisonStore,
    disabled: &AtomicBool,
) -> ReconcileOutcome {
    let BreakerState::HalfOpen {
        probe_generation,
        mut synchronized_epoch,
        reconcile_after,
    } = state.breaker
    else {
        return ReconcileOutcome::Failed;
    };
    let registry = state
        .open_documents
        .lock()
        .recover_poison("finish_half_open_recovery");
    let current_epoch = registry_epoch.load(Ordering::Acquire);
    if current_epoch == synchronized_epoch {
        let healthy_since = Instant::now();
        state.healthy_since = Some(healthy_since);
        state.long_healthy_since = Some(healthy_since);
        disabled.store(false, Ordering::Release);
        return ReconcileOutcome::Current;
    }
    drop(registry);
    let now = Instant::now();
    if now < reconcile_after {
        return ReconcileOutcome::Pending;
    }
    synchronized_epoch = current_epoch;
    let result = resync_open_documents(
        state
            .client
            .as_ref()
            .expect("successful half-open recovery retains its worker"),
        state.worker_generation,
        &state.open_documents,
        &state.quarantined,
        &mut state.replicas,
        comparisons,
    );
    if let Err(failure) = result {
        let _ = quarantine_grammars(
            state,
            failure.active_grammars,
            failure.class,
            comparisons,
            failure.active_lease_count,
        );
        log::error!(
            target: "kakehashi::tree_worker_shadow",
            "half-open registry reconciliation failed: {}",
            failure.error,
        );
        return ReconcileOutcome::Failed;
    }
    state.breaker = BreakerState::HalfOpen {
        probe_generation,
        synchronized_epoch,
        reconcile_after: Instant::now() + WORKER_LIVENESS_POLL,
    };
    ReconcileOutcome::Pending
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

fn retire_client(state: &mut SupervisorState) -> std::io::Result<()> {
    state.read_client.store(None);
    state.worker_nodes.clear();
    let Some(client) = state.client.take() else {
        return Ok(());
    };
    match Arc::try_unwrap(client) {
        Ok(client) => client.shutdown(),
        Err(client) => {
            client.terminate_shared(SHADOW_SHUTDOWN_TIMEOUT);
            Ok(())
        }
    }
}

fn quiesce_published_client(state: &mut SupervisorState) -> std::io::Result<()> {
    let client = state
        .client
        .as_ref()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, "worker is absent"))?;
    client.close_request_admission()?;
    state.read_client.store(None);
    if let Err(error) = client.wait_for_idle_generation(SHADOW_SHUTDOWN_TIMEOUT) {
        log::warn!(
            target: "kakehashi::tree_worker_shadow",
            "forcibly retiring non-quiescent worker generation during restart: {error}",
        );
        client.terminate_shared(SHADOW_SHUTDOWN_TIMEOUT);
    }
    Ok(())
}

fn mark_tree_tier_unavailable(
    state: &mut SupervisorState,
    disabled: &AtomicBool,
    pending_configuration_generation: &AtomicU64,
) {
    if let Err(error) = retire_client(state) {
        log::warn!(target: "kakehashi::tree_worker_shadow", "failed worker cleanup while opening breaker: {error}");
    }
    state.healthy_since = None;
    state.long_healthy_since = None;
    state.breaker = BreakerState::Open {
        opened_at: Instant::now(),
        baseline_generation: pending_configuration_generation.load(Ordering::Acquire),
        cooldown: if state.restart_budget.tokens_remaining() == 0 {
            state.restart_policy.long_healthy_interval
        } else {
            state.restart_policy.fast_healthy_interval
        },
    };
    disabled.store(true, Ordering::Release);
    #[cfg(feature = "e2e")]
    if let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_BREAKER_OPEN_FILE") {
        let _ = std::fs::write(path, b"open");
    }
}

fn observe_healthy_service(state: &mut SupervisorState, now: Instant) {
    let Some(healthy_since) = state.healthy_since else {
        return;
    };
    if now.saturating_duration_since(healthy_since) >= state.restart_policy.fast_healthy_interval {
        state.restart_budget.reset_fast();
        if let BreakerState::HalfOpen {
            probe_generation, ..
        } = state.breaker
        {
            state.breaker = BreakerState::Closed;
            log::info!(
                target: "kakehashi::tree_worker_shadow",
                "closed shadow worker breaker after configuration generation {probe_generation} completed 60 seconds of healthy service",
            );
            #[cfg(feature = "e2e")]
            if let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_BREAKER_CLOSED_FILE") {
                let _ = std::fs::write(path, b"closed");
            }
        }
    }
    let Some(last_restore) = state.long_healthy_since else {
        return;
    };
    let elapsed = now.saturating_duration_since(last_restore);
    let intervals = elapsed.as_secs() / state.restart_policy.long_healthy_interval.as_secs();
    if intervals == 0 {
        return;
    }
    state.restart_budget.restore_long(intervals);
    state.long_healthy_since = Some(
        last_restore
            + state
                .restart_policy
                .long_healthy_interval
                .saturating_mul(intervals as u32),
    );
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
        ShadowCommand::Apply {
            request,
            fallback,
            derive_snapshot,
        } => {
            let uri = fallback.context.uri.clone();
            let identity = ReplicaIdentity::from_sync(&fallback);
            if replica_requires_sync(replicas, &uri, &identity, request.base_version) {
                (sync_and_derive(client, fallback), Some(identity))
            } else {
                let response = if derive_snapshot {
                    client.apply_document_edits_and_derive(request)
                } else {
                    client.apply_document_edits(ApplyDocumentEdits {
                        context: request.context,
                        base_version: request.base_version,
                        edits: request.edits,
                    })
                };
                match response {
                    Ok(Response::Snapshot(snapshot)) => {
                        (Ok(Response::Snapshot(snapshot)), Some(identity))
                    }
                    Ok(Response::DocumentAck(ack)) => {
                        (Ok(Response::DocumentAck(ack)), Some(identity))
                    }
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
        ShadowCommand::Forget(_) => unreachable!("forget commands restart before execution"),
        ShadowCommand::Shutdown(_) => unreachable!(),
    }
}

fn recover_worker(
    state: &mut SupervisorState,
    executable: &std::path::Path,
    compute_threads: usize,
    failure: WorkerLossEvidence,
    comparisons: &ComparisonStore,
    mode: RecoveryMode,
) -> bool {
    let recovery_started = Instant::now();
    let Some(mut class) = apply_worker_loss_evidence(state, failure, comparisons) else {
        return false;
    };

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
        if let Err(error) = retire_client(state) {
            log::warn!(target: "kakehashi::tree_worker_shadow", "failed worker cleanup: {error}");
        }
        if charged {
            std::thread::sleep(state.restart_budget.backoff(state.restart_policy));
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
        {
            let mut documents = state
                .open_documents
                .lock()
                .recover_poison("recover_worker(clear acknowledgements)");
            documents.acknowledged.clear();
            documents.replica_changed.notify_waiters();
        }
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
                let replacement = Arc::new(replacement);
                state.read_client.store(Some(Arc::clone(&replacement)));
                state
                    .open_documents
                    .lock()
                    .recover_poison("recover_worker(notify replacement client)")
                    .replica_changed
                    .notify_waiters();
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
                #[cfg(feature = "e2e")]
                if let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_RECOVERY_READY_FILE") {
                    let _ = std::fs::write(path, b"ready");
                }
                return true;
            }
            Err(failure) => {
                state.client = Some(Arc::new(replacement));
                let error = failure.error;
                let evidence = WorkerLossEvidence {
                    class: failure.class,
                    active_grammars: failure.active_grammars,
                    active_lease_count: failure.active_lease_count,
                    snapshot_complete: failure.snapshot_complete,
                };
                let Some(next_class) = apply_worker_loss_evidence(state, evidence, comparisons)
                else {
                    return false;
                };
                class = next_class;
                log::error!(
                    target: "kakehashi::tree_worker_shadow",
                    "worker generation {generation} full resync failed: {}",
                    error,
                );
                if mode == RecoveryMode::HalfOpen {
                    return false;
                }
                first_attempt = false;
            }
        }
    }
}

fn apply_worker_loss_evidence(
    state: &mut SupervisorState,
    mut failure: WorkerLossEvidence,
    comparisons: &ComparisonStore,
) -> Option<FailureClass> {
    let quarantine = quarantine_grammars(
        state,
        std::mem::take(&mut failure.active_grammars),
        failure.class,
        comparisons,
        failure.active_lease_count,
    );
    if !failure.allows_restart() {
        log::error!(
            target: "kakehashi::tree_worker_shadow",
            "disabled tree tier because the dead worker hazard snapshot was incomplete",
        );
        return None;
    }
    Some(effective_failure_class(failure.class, &quarantine))
}

fn quarantine_grammars(
    state: &mut SupervisorState,
    mut grammars: Vec<GrammarIdentity>,
    class: FailureClass,
    comparisons: &ComparisonStore,
    active_lease_count: usize,
) -> QuarantineBatch {
    grammars.sort_unstable();
    grammars.dedup();
    let mut batch = QuarantineBatch::default();
    let mut newly_quarantined = Vec::new();
    for grammar in grammars {
        if state.quarantined.insert(grammar.clone()) {
            newly_quarantined.push(grammar);
            batch.newly_quarantined += 1;
        } else {
            batch.already_quarantined += 1;
        }
    }
    if !newly_quarantined.is_empty() {
        let newly_quarantined_set = newly_quarantined.iter().cloned().collect::<HashSet<_>>();
        let open_documents = state
            .open_documents
            .lock()
            .recover_poison("quarantine_grammars(open documents)");
        for request in open_documents.current.values() {
            if newly_quarantined_set.contains(&GrammarIdentity::from_sync(request)) {
                comparisons.mark_closed(&request.context.uri, request.context.incarnation);
            }
        }
        drop(open_documents);
    }
    if !should_log_quarantine_summary(&batch) {
        return batch;
    }
    let effective_class = effective_failure_class(class, &batch);
    for grammar in &newly_quarantined {
        log::error!(
            target: "kakehashi::tree_worker_shadow",
            "quarantined grammar conservatively for this session after {:?} worker loss: symbol={} artifact_digest={}",
            effective_class,
            grammar.grammar_symbol,
            grammar.artifact_digest,
        );
    }
    log::error!(
        target: "kakehashi::tree_worker_shadow",
        "worker loss quarantine summary active_leases={} unique_grammars={} newly_quarantined={} already_quarantined={} class={:?}",
        active_lease_count,
        batch.newly_quarantined + batch.already_quarantined,
        batch.newly_quarantined,
        batch.already_quarantined,
        effective_class,
    );
    batch
}

fn should_log_quarantine_summary(quarantine: &QuarantineBatch) -> bool {
    quarantine.newly_quarantined + quarantine.already_quarantined > 0
}

fn effective_failure_class(class: FailureClass, quarantine: &QuarantineBatch) -> FailureClass {
    if quarantine.already_quarantined > 0 {
        FailureClass::Systemic
    } else {
        class
    }
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
    let current_uris = uris.iter().cloned().collect::<HashSet<_>>();
    let stale_replicas = replicas
        .iter()
        .filter(|(uri, identity)| {
            !current_uris.contains(*uri)
                && replica_was_closed(
                    &open_documents
                        .lock()
                        .recover_poison("resync_open_documents(closed incarnation)"),
                    uri,
                    identity,
                )
        })
        .map(|(uri, identity)| (uri.clone(), identity.clone()))
        .collect::<Vec<_>>();
    for (uri, identity) in stale_replicas {
        let response = client.close_document(CloseDocument {
            context: RequestContext {
                request_id: 0,
                worker_generation: generation,
                uri: uri.clone(),
                incarnation: identity.incarnation,
                content_version: identity.content_version,
                configuration_generation: identity.configuration_generation,
            },
        });
        match response {
            Ok(Response::DocumentClosed(_)) | Ok(Response::Error(_)) => {
                replicas.remove(&uri);
            }
            Ok(response) => {
                return Err(ResyncFailure {
                    error: std::io::Error::other(format!(
                        "unexpected reconciliation close response: {response:?}"
                    )),
                    class: FailureClass::Systemic,
                    active_grammars: Vec::new(),
                    active_lease_count: 0,
                    snapshot_complete: true,
                });
            }
            Err(error) => {
                let failure = worker_loss_evidence(client, &error);
                return Err(ResyncFailure::from_worker_loss(error, failure));
            }
        }
    }
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
        if replicas.get(&request.context.uri) == Some(&identity) {
            continue;
        }
        comparisons.open(&request.context.uri, request.context.incarnation);
        match sync_and_derive(client, request) {
            Ok(Response::Snapshot(snapshot)) => {
                replicas.insert(snapshot.context.uri.clone(), identity);
                open_documents
                    .lock()
                    .recover_poison("resync_open_documents(acknowledge snapshot)")
                    .acknowledge(snapshot.context.clone());
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
                    active_grammars: Vec::new(),
                    active_lease_count: 0,
                    snapshot_complete: true,
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
                    active_grammars: Vec::new(),
                    active_lease_count: 0,
                    snapshot_complete: true,
                });
            }
            Err(error) => {
                let failure = worker_loss_evidence(client, &error);
                return Err(ResyncFailure::from_worker_loss(error, failure));
            }
        }
    }
    Ok(resynced)
}

fn replica_was_closed(
    open_documents: &OpenDocuments,
    uri: &str,
    identity: &ReplicaIdentity,
) -> bool {
    open_documents
        .closed_incarnations
        .get(uri)
        .is_some_and(|closed| *closed >= identity.incarnation)
}

fn retag_command(command: &mut ShadowCommand, generation: u64) {
    match command {
        ShadowCommand::Sync(request) => request.context.worker_generation = generation,
        ShadowCommand::Apply {
            request, fallback, ..
        } => {
            request.context.worker_generation = generation;
            fallback.context.worker_generation = generation;
        }
        ShadowCommand::Close(request) => request.context.worker_generation = generation,
        ShadowCommand::Forget(context) => context.worker_generation = generation,
        ShadowCommand::Shutdown(_) => {}
    }
}

fn worker_loss_evidence(client: &Client, error: &std::io::Error) -> WorkerLossEvidence {
    if client.was_terminated_for_request_deadline() {
        log::error!(
            target: "kakehashi::tree_worker_shadow",
            "worker generation was terminated for a request deadline; treating its loss as systemic",
        );
        return WorkerLossEvidence::systemic();
    }
    if let Err(drain_error) = client.wait_for_reader_drain(SHADOW_SHUTDOWN_TIMEOUT) {
        let active_hazards = client.active_grammar_hazards();
        let active_lease_count = active_hazards.len();
        let active_grammars = unique_active_grammars(&active_hazards);
        log::error!(
            target: "kakehashi::tree_worker_shadow",
            "could not obtain a complete worker failure snapshot; disabling the tree tier with {} currently known grammar(s): {drain_error}",
            active_grammars.len(),
        );
        return WorkerLossEvidence::incomplete(active_grammars, active_lease_count);
    }
    let mut active_hazards = client.active_grammar_hazards();
    active_hazards.sort_unstable_by(|left, right| {
        (
            &left.grammar.grammar_symbol,
            &left.grammar.artifact_digest,
            left.lease_id,
            left.context.request_id,
        )
            .cmp(&(
                &right.grammar.grammar_symbol,
                &right.grammar.artifact_digest,
                right.lease_id,
                right.context.request_id,
            ))
    });
    let active_lease_count = active_hazards.len();
    let active_grammars = unique_active_grammars(&active_hazards);
    if !active_hazards.is_empty() {
        let leases = active_hazards
            .iter()
            .map(|hazard| {
                format!(
                    "{}@{}:lease={}:request={}",
                    hazard.grammar.grammar_symbol,
                    hazard.grammar.artifact_digest,
                    hazard.lease_id,
                    hazard.context.request_id,
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        log::error!(
            target: "kakehashi::tree_worker_shadow",
            "worker loss snapshot committed active grammar hazard(s): active_leases={} unique_grammars={} leases=[{}]",
            active_lease_count,
            active_grammars.len(),
            leases,
        );
    }
    let class = classify_transport_failure(client, error, !active_grammars.is_empty());
    WorkerLossEvidence {
        class,
        active_grammars,
        active_lease_count,
        snapshot_complete: true,
    }
}

fn is_local_client_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::AlreadyExists
    )
}

fn is_nonfatal_client_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

fn may_continue_after_client_error(
    error: &std::io::Error,
    terminated_for_request_deadline: bool,
    worker_transport_is_live: bool,
) -> bool {
    is_nonfatal_client_error(error) && !terminated_for_request_deadline && worker_transport_is_live
}

fn unique_active_grammars(
    active_hazards: &[crate::tree_worker::GrammarHazardArmed],
) -> Vec<GrammarIdentity> {
    let mut active_grammars = active_hazards
        .iter()
        .map(|hazard| GrammarIdentity {
            grammar_symbol: hazard.grammar.grammar_symbol.clone(),
            artifact_digest: hazard.grammar.artifact_digest.clone(),
        })
        .collect::<Vec<_>>();
    active_grammars.sort_unstable();
    active_grammars.dedup();
    active_grammars
}

fn classify_transport_failure(
    client: &Client,
    error: &std::io::Error,
    has_active_hazards: bool,
) -> FailureClass {
    if !has_active_hazards || error.kind() == std::io::ErrorKind::InvalidData {
        return FailureClass::Systemic;
    }
    if error.kind() == std::io::ErrorKind::TimedOut {
        return FailureClass::NativeEvidenced;
    }
    if !client.was_terminated_by_transport()
        && client
            .try_wait()
            .ok()
            .flatten()
            .is_some_and(|status| process_status_is_native_crash(&status))
    {
        return FailureClass::NativeEvidenced;
    }
    FailureClass::Systemic
}

#[cfg(unix)]
fn process_status_is_native_crash(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;

    status.signal().is_some()
}

#[cfg(windows)]
fn process_status_is_native_crash(status: &std::process::ExitStatus) -> bool {
    status.code().is_some_and(windows_exit_code_is_exception)
}

#[cfg(not(any(unix, windows)))]
fn process_status_is_native_crash(_status: &std::process::ExitStatus) -> bool {
    false
}

#[cfg(any(test, windows))]
fn windows_exit_code_is_exception(code: i32) -> bool {
    code.is_negative()
}

fn command_uri(command: &ShadowCommand) -> Option<&str> {
    match command {
        ShadowCommand::Sync(request) => Some(&request.context.uri),
        ShadowCommand::Apply { fallback, .. } => Some(&fallback.context.uri),
        ShadowCommand::Close(request) => Some(&request.context.uri),
        ShadowCommand::Forget(context) => Some(&context.uri),
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
        ShadowCommand::Forget(context) => Some((&context.uri, context.incarnation)),
        ShadowCommand::Shutdown(_) => None,
    }
}

fn command_grammar(command: &ShadowCommand) -> Option<GrammarIdentity> {
    match command {
        ShadowCommand::Sync(request) => Some(GrammarIdentity::from_sync(request)),
        ShadowCommand::Apply { fallback, .. } => Some(GrammarIdentity::from_sync(fallback)),
        ShadowCommand::Close(_) | ShadowCommand::Forget(_) | ShadowCommand::Shutdown(_) => None,
    }
}

fn command_sync(command: &ShadowCommand) -> Option<&SyncDocument> {
    match command {
        ShadowCommand::Sync(request) => Some(request),
        ShadowCommand::Apply { fallback, .. } => Some(fallback),
        ShadowCommand::Close(_) | ShadowCommand::Forget(_) | ShadowCommand::Shutdown(_) => None,
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
    base_version: u64,
) -> bool {
    replicas.get(uri).is_none_or(|current| {
        current.incarnation != desired.incarnation
            || current.content_version != base_version
            || desired.content_version <= base_version
            || current.language != desired.language
            || current.grammar_symbol != desired.grammar_symbol
            || current.source_path != desired.source_path
            || current.parser_path != desired.parser_path
            || current.artifact_digest != desired.artifact_digest
            || current.queries != desired.queries
            || current.configuration_generation != desired.configuration_generation
    })
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

    fn active_hazard(
        lease_id: u64,
        request_id: u64,
        grammar_symbol: &str,
        artifact_digest: &str,
    ) -> crate::tree_worker::GrammarHazardArmed {
        crate::tree_worker::GrammarHazardArmed {
            lease_id,
            context: RequestContext {
                request_id,
                worker_generation: 7,
                uri: format!("file:///{grammar_symbol}-{request_id}"),
                incarnation: 1,
                content_version: 1,
                configuration_generation: 1,
            },
            grammar: crate::tree_worker::WorkerGrammarIdentity {
                grammar_symbol: grammar_symbol.into(),
                artifact_digest: artifact_digest.into(),
            },
        }
    }

    fn shadow(sender: mpsc::SyncSender<ShadowCommand>) -> TreeWorkerShadow {
        TreeWorkerShadow {
            mode: TreeWorkerMode::Shadow,
            sender: Some(sender),
            accepting: Arc::new(AtomicBool::new(true)),
            disabled: Arc::new(AtomicBool::new(false)),
            next_request_id: AtomicU64::new(1),
            worker_generation: 7,
            comparisons: Arc::new(ComparisonStore::default()),
            pending_configuration_generation: Arc::new(AtomicU64::new(0)),
            open_documents: Arc::new(Mutex::new(OpenDocuments::default())),
            registry_epoch: Arc::new(AtomicU64::new(0)),
            read_client: Arc::new(ArcSwapOption::empty()),
            worker_nodes: Arc::new(dashmap::DashMap::new()),
        }
    }

    #[test]
    fn worker_mode_is_explicit_and_legacy_shadow_env_remains_compatible() {
        assert_eq!(
            parse_mode(Some("authoritative"), None),
            TreeWorkerMode::Authoritative
        );
        assert_eq!(parse_mode(Some("shadow"), None), TreeWorkerMode::Shadow);
        assert_eq!(
            parse_mode(Some("legacy"), Some("true")),
            TreeWorkerMode::Legacy
        );
        assert_eq!(parse_mode(None, Some("true")), TreeWorkerMode::Shadow);
        assert_eq!(parse_mode(None, None), TreeWorkerMode::Legacy);
        assert_eq!(
            parse_mode(Some("invalid"), Some("true")),
            TreeWorkerMode::Legacy
        );
        assert!(should_derive_edit_snapshot(TreeWorkerMode::Shadow));
        assert!(!should_derive_edit_snapshot(TreeWorkerMode::Authoritative));
    }

    #[test]
    fn active_hazard_snapshot_is_a_sorted_unique_grammar_set() {
        let hazards = vec![
            active_hazard(3, 30, "rust", "digest-b"),
            active_hazard(1, 10, "lua", "digest-a"),
            active_hazard(2, 20, "rust", "digest-b"),
        ];

        assert_eq!(
            unique_active_grammars(&hazards),
            vec![
                GrammarIdentity {
                    grammar_symbol: "lua".into(),
                    artifact_digest: "digest-a".into(),
                },
                GrammarIdentity {
                    grammar_symbol: "rust".into(),
                    artifact_digest: "digest-b".into(),
                },
            ]
        );
        assert!(unique_active_grammars(&[]).is_empty());
    }

    #[test]
    fn already_quarantined_hazard_escalates_worker_loss_to_systemic() {
        assert_eq!(
            effective_failure_class(
                FailureClass::NativeEvidenced,
                &QuarantineBatch {
                    newly_quarantined: 0,
                    already_quarantined: 1,
                },
            ),
            FailureClass::Systemic,
        );
        assert_eq!(
            effective_failure_class(
                FailureClass::NativeEvidenced,
                &QuarantineBatch {
                    newly_quarantined: 2,
                    already_quarantined: 0,
                },
            ),
            FailureClass::NativeEvidenced,
        );
    }

    #[test]
    fn quarantine_summary_is_emitted_only_for_nonempty_evidence() {
        assert!(!should_log_quarantine_summary(&QuarantineBatch::default()));
        assert!(should_log_quarantine_summary(&QuarantineBatch {
            newly_quarantined: 0,
            already_quarantined: 1,
        }));
    }

    #[test]
    fn incomplete_worker_loss_snapshot_cannot_restart_the_tree_tier() {
        let evidence = WorkerLossEvidence::incomplete(
            vec![GrammarIdentity {
                grammar_symbol: "rust".into(),
                artifact_digest: "digest-rust".into(),
            }],
            2,
        );

        assert!(!evidence.allows_restart());
        assert_eq!(evidence.class, FailureClass::Systemic);
        assert_eq!(evidence.active_grammars.len(), 1);
        assert_eq!(evidence.active_lease_count, 2);
    }

    #[test]
    fn resync_failure_preserves_incomplete_worker_snapshot() {
        let failure = ResyncFailure::from_worker_loss(
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "lost"),
            WorkerLossEvidence::incomplete(Vec::new(), 0),
        );

        assert!(!failure.snapshot_complete);
    }

    #[test]
    fn local_admission_errors_are_not_worker_loss() {
        for kind in [
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::AlreadyExists,
        ] {
            assert!(is_local_client_error(&std::io::Error::new(kind, "local")));
        }
        assert!(!is_local_client_error(&std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "transport",
        )));
        for kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WouldBlock] {
            assert!(is_nonfatal_client_error(&std::io::Error::new(
                kind,
                "retryable",
            )));
        }
    }

    #[test]
    fn nonfatal_timeout_requires_a_confirmed_live_worker() {
        let timeout = std::io::Error::new(std::io::ErrorKind::TimedOut, "deadline");

        assert!(may_continue_after_client_error(&timeout, false, true));
        assert!(!may_continue_after_client_error(&timeout, true, true));
        assert!(!may_continue_after_client_error(&timeout, false, false));
        assert!(!may_continue_after_client_error(
            &std::io::Error::new(std::io::ErrorKind::BrokenPipe, "transport"),
            false,
            true,
        ));
    }

    #[test]
    fn windows_exception_status_is_native_crash_evidence() {
        assert!(windows_exit_code_is_exception(0xc000_0409_u32 as i32));
        assert!(!windows_exit_code_is_exception(86));
    }

    #[test]
    fn closing_document_discards_all_public_to_worker_node_mappings() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let uri = url::Url::parse("file:///mapped.rs").unwrap();
        shadow.record_node_mapping(
            &uri,
            &serde_json::json!({"id": "legacy", "kind": "identifier"}),
            &crate::tree_worker::OwnedNode {
                id: OpaqueNodeId {
                    worker_generation: 7,
                    local_id: "worker".into(),
                },
                kind: "identifier".into(),
                start_byte: 0,
                end_byte: 1,
                named: true,
                has_error: false,
                layer: 0,
            },
        );
        assert_eq!(shadow.worker_nodes.len(), 2);

        shadow.mirror_close(&uri, 1, 1, 0);

        assert!(shadow.worker_nodes.is_empty());
        assert!(matches!(receiver.recv().unwrap(), ShadowCommand::Close(_)));
    }

    #[test]
    fn accepted_worker_regions_are_cached_and_generation_fenced_as_node_ids() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let shadow = shadow(sender);
        let uri = url::Url::parse("file:///regions.md").unwrap();
        let sync = sync(uri.as_str(), 2, 3, 7);
        {
            let mut documents = shadow.open_documents.lock().unwrap();
            documents.current.insert(uri.as_str().into(), sync.clone());
            documents
                .acknowledged
                .insert(uri.as_str().into(), sync.context.clone());
        }
        let result = InjectionRegionsResult {
            context: sync.context,
            regions: vec![crate::tree_worker::WireInjectionRegion {
                language: "lua".into(),
                byte_start: 10,
                byte_end: 20,
                line_start: 2,
                line_end: 3,
                start_column: 0,
                region_id: "worker-region".into(),
                content_hash: 42,
                virtual_content: "local x = 1".into(),
                line_column_offsets: vec![0],
                contiguous: true,
            }],
        };

        shadow.cache_worker_regions(&uri, &result);
        let mut authoritative = worker_region_to_resolved(&result.regions[0]);
        authoritative.region.region_id = "legacy-region".into();
        shadow.compare_injection_regions(&uri, 3, &[authoritative], &result);

        let documents = shadow.open_documents.lock().unwrap();
        let cached = documents.worker_regions.get(uri.as_str()).unwrap();
        assert_eq!(cached.context, result.context);
        assert_eq!(cached.regions[0].region.region_id, "worker-region");
        drop(documents);
        assert_eq!(
            shadow
                .worker_nodes
                .get(&(uri.as_str().into(), "worker-region".into()))
                .map(|entry| entry.clone()),
            Some(OpaqueNodeId {
                worker_generation: 7,
                local_id: "worker-region".into(),
            })
        );
        assert_eq!(
            shadow
                .worker_nodes
                .get(&(uri.as_str().into(), "legacy-region".into()))
                .map(|entry| entry.clone()),
            Some(OpaqueNodeId {
                worker_generation: 7,
                local_id: "worker-region".into(),
            })
        );
    }

    fn grammar() -> WorkerGrammarDescriptor {
        WorkerGrammarDescriptor {
            source_path: "/parser/rust.so".into(),
            parser_path: "/parser/rust.so".into(),
            grammar_symbol: "rust".into(),
            artifact_digest: "sha256:rust-v1".into(),
            queries: Default::default(),
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
            queries: Default::default(),
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
            derive_snapshot: true,
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
    fn document_change_invalidates_cached_worker_regions() {
        let uri = "file:///a.rs";
        let mut documents = OpenDocuments::default();
        let first = sync(uri, 1, 1, 7);
        documents.observe(&ShadowCommand::Sync(first.clone()));
        documents.worker_regions.insert(
            uri.into(),
            WorkerRegionSnapshot {
                context: first.context,
                regions: Arc::new(Vec::new()),
            },
        );

        documents.observe(&ShadowCommand::Apply {
            request: ApplyDocumentEditsAndDerive {
                context: sync(uri, 1, 2, 7).context,
                base_version: 1,
                edits: Vec::new(),
            },
            fallback: sync(uri, 1, 2, 7),
            derive_snapshot: true,
        });

        assert!(!documents.worker_regions.contains_key(uri));
    }

    #[test]
    fn worker_region_conversion_preserves_bridge_geometry_and_identity() {
        let wire = crate::tree_worker::WireInjectionRegion {
            language: "lua".into(),
            byte_start: 10,
            byte_end: 20,
            line_start: 2,
            line_end: 4,
            start_column: 3,
            region_id: "worker-region".into(),
            content_hash: 42,
            virtual_content: "x\ny".into(),
            line_column_offsets: vec![3, 0],
            contiguous: false,
        };

        let resolved = worker_region_to_resolved(&wire);

        assert_eq!(resolved.injection_language, "lua");
        assert_eq!(resolved.region.byte_range, 10..20);
        assert_eq!(resolved.region.line_range, 2..4);
        assert_eq!(resolved.region.start_column, 3);
        assert_eq!(resolved.region.region_id, "worker-region");
        assert_eq!(resolved.region.content_hash, 42);
        assert_eq!(resolved.virtual_content, "x\ny");
        assert_eq!(resolved.line_column_offsets, vec![3, 0]);
        assert!(!resolved.contiguous);
    }

    #[test]
    fn read_requires_the_actor_to_acknowledge_the_exact_version() {
        let mut documents = OpenDocuments::default();
        let version_one = sync("file:///a.rs", 1, 1, 7).context;
        documents
            .acknowledged
            .insert(version_one.uri.clone(), version_one.clone());
        assert!(documents.acknowledges(&version_one));

        let version_two = sync("file:///a.rs", 1, 2, 7).context;
        assert!(!documents.acknowledges(&version_two));

        documents
            .acknowledged
            .insert(version_two.uri.clone(), version_two.clone());
        assert!(documents.acknowledges(&version_two));

        let restarted = sync("file:///a.rs", 1, 2, 8).context;
        assert!(!documents.acknowledges(&restarted));
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
    fn closed_incarnation_tombstone_rejects_a_delayed_configuration_snapshot() {
        let mut documents = OpenDocuments::default();
        let current = sync("file:///a.rs", 4, 2, 7);
        assert!(matches!(
            documents.observe(&ShadowCommand::Sync(current.clone())),
            Observation::Accepted { .. }
        ));
        assert!(matches!(
            documents.observe(&ShadowCommand::Close(CloseDocument {
                context: current.context.clone(),
            })),
            Observation::Accepted { .. }
        ));

        assert_eq!(
            documents.observe(&ShadowCommand::Sync(current)),
            Observation::Stale
        );
        assert!(documents.current.is_empty());

        let reopened = sync("file:///a.rs", 5, 1, 7);
        assert!(matches!(
            documents.observe(&ShadowCommand::Sync(reopened)),
            Observation::Accepted { .. }
        ));
        assert_eq!(documents.current["file:///a.rs"].context.incarnation, 5);
    }

    #[test]
    fn unavailable_grammar_forget_allows_same_incarnation_to_recover() {
        let mut documents = OpenDocuments::default();
        let mut unavailable = sync("file:///a.rs", 4, 2, 7);
        let replica = ReplicaIdentity::from_sync(&unavailable);
        unavailable.context.configuration_generation = 8;
        assert!(matches!(
            documents.observe(&ShadowCommand::Sync(unavailable.clone())),
            Observation::Accepted { .. }
        ));
        assert!(matches!(
            documents.observe(&ShadowCommand::Forget(unavailable.context.clone())),
            Observation::Accepted { .. }
        ));
        assert!(documents.current.is_empty());
        assert!(documents.closed_incarnations.is_empty());
        assert!(!replica_was_closed(&documents, "file:///a.rs", &replica));

        unavailable.context.configuration_generation = 9;
        assert!(matches!(
            documents.observe(&ShadowCommand::Sync(unavailable)),
            Observation::Accepted { .. }
        ));
        assert!(documents.current.contains_key("file:///a.rs"));
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
            derive_snapshot: true,
        };
        retag_command(&mut command, 11);

        let ShadowCommand::Apply {
            request, fallback, ..
        } = command
        else {
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
    fn advancing_version_uses_incremental_replica_when_stable_identity_matches() {
        let current = ReplicaIdentity::from_sync(&sync("file:///a.rs", 1, 1, 7));
        let desired = ReplicaIdentity::from_sync(&sync("file:///a.rs", 1, 2, 7));
        let replicas = HashMap::from([("file:///a.rs".to_string(), current)]);

        assert!(!replica_requires_sync(
            &replicas,
            "file:///a.rs",
            &desired,
            1,
        ));
        assert!(replica_requires_sync(
            &replicas,
            "file:///a.rs",
            &desired,
            0,
        ));
        let mut changed_grammar = desired;
        changed_grammar.artifact_digest = "sha256:rust-v2".into();
        assert!(replica_requires_sync(
            &replicas,
            "file:///a.rs",
            &changed_grammar,
            1,
        ));
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
        assert!(!systemic.consume(FailureClass::Systemic));
        assert_eq!(systemic.tokens_remaining(), MAX_SESSION_RESTARTS - 2);

        let mut native = RestartBudget::default();
        for _ in 0..MAX_NATIVE_RESTARTS {
            assert!(native.consume(FailureClass::NativeEvidenced));
        }
        assert!(!native.consume(FailureClass::NativeEvidenced));

        assert_eq!(
            systemic.backoff(RestartPolicy::default()),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn third_systemic_generation_failure_denies_a_replacement() {
        let mut budget = RestartBudget::default();

        assert!(budget.consume(FailureClass::Systemic));
        assert!(budget.consume(FailureClass::Systemic));
        assert!(!budget.consume(FailureClass::Systemic));
        assert_eq!(budget.systemic, MAX_SYSTEMIC_RESTARTS);
        assert_eq!(budget.tokens_remaining(), MAX_SESSION_RESTARTS - 2);
    }

    #[test]
    fn healthy_intervals_reset_fast_budgets_but_restore_long_tokens_slowly() {
        let started = Instant::now();
        let mut state = SupervisorState {
            client: None,
            read_client: Arc::new(ArcSwapOption::empty()),
            worker_nodes: Arc::new(dashmap::DashMap::new()),
            worker_generation: 1,
            replicas: HashMap::new(),
            open_documents: Arc::new(Mutex::new(OpenDocuments::default())),
            quarantined: HashSet::new(),
            restart_budget: RestartBudget::default(),
            restart_policy: RestartPolicy::default(),
            breaker: BreakerState::HalfOpen {
                probe_generation: 7,
                synchronized_epoch: 0,
                reconcile_after: started,
            },
            healthy_since: Some(started),
            long_healthy_since: Some(started),
        };
        for _ in 0..MAX_SYSTEMIC_RESTARTS - 1 {
            assert!(state.restart_budget.consume(FailureClass::Systemic));
        }
        assert!(!state.restart_budget.consume(FailureClass::Systemic));
        assert_eq!(
            state.restart_budget.tokens_remaining(),
            MAX_SESSION_RESTARTS - 2
        );

        observe_healthy_service(&mut state, started + FAST_HEALTHY_INTERVAL);
        assert_eq!(state.breaker, BreakerState::Closed);
        assert!(state.restart_budget.consume(FailureClass::Systemic));
        assert_eq!(
            state.restart_budget.tokens_remaining(),
            MAX_SESSION_RESTARTS - 3
        );
        assert_eq!(
            state.restart_budget.backoff(state.restart_policy),
            Duration::from_secs(1)
        );

        observe_healthy_service(
            &mut state,
            started + FAST_HEALTHY_INTERVAL + LONG_HEALTHY_INTERVAL,
        );
        assert_eq!(
            state.restart_budget.tokens_remaining(),
            MAX_SESSION_RESTARTS - 2
        );
        assert_eq!(
            state.restart_budget.backoff(state.restart_policy),
            INITIAL_RESTART_BACKOFF
        );
    }

    #[test]
    fn half_open_worker_loss_applies_hazard_evidence_before_refusing_restart() {
        let started = Instant::now();
        let grammar = GrammarIdentity {
            grammar_symbol: "rust".into(),
            artifact_digest: "digest-rust".into(),
        };
        let mut state = SupervisorState {
            client: None,
            read_client: Arc::new(ArcSwapOption::empty()),
            worker_nodes: Arc::new(dashmap::DashMap::new()),
            worker_generation: 7,
            replicas: HashMap::new(),
            open_documents: Arc::new(Mutex::new(OpenDocuments::default())),
            quarantined: HashSet::new(),
            restart_budget: RestartBudget::default(),
            restart_policy: RestartPolicy::default(),
            breaker: BreakerState::HalfOpen {
                probe_generation: 7,
                synchronized_epoch: 0,
                reconcile_after: started,
            },
            healthy_since: Some(started),
            long_healthy_since: Some(started),
        };

        let class = apply_worker_loss_evidence(
            &mut state,
            WorkerLossEvidence {
                class: FailureClass::NativeEvidenced,
                active_grammars: vec![grammar.clone()],
                active_lease_count: 1,
                snapshot_complete: true,
            },
            &ComparisonStore::default(),
        );

        assert_eq!(class, Some(FailureClass::NativeEvidenced));
        assert!(state.quarantined.contains(&grammar));
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

        let ShadowCommand::Apply {
            request, fallback, ..
        } = receiver.recv().unwrap()
        else {
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
        let mut desired = identity.clone();
        desired.content_version = 5;

        assert!(!replica_requires_sync(&replicas, uri.as_str(), &desired, 4,));

        let mut changed = desired.clone();
        changed.language = "bash".into();
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed, 4,));

        changed = desired.clone();
        changed.grammar_symbol = "rust_with_changes".into();
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed, 4,));

        changed = desired.clone();
        changed.configuration_generation += 1;
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed, 4,));

        changed = desired.clone();
        changed.queries.bindings = Some("(identifier) @reference".into());
        assert!(replica_requires_sync(&replicas, uri.as_str(), &changed, 4,));

        replicas.remove(uri.as_str());
        assert!(replica_requires_sync(&replicas, uri.as_str(), &desired, 4,));
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
