//! Development-only process boundary for the tree-worker prototype.
//!
//! The protocol deliberately returns owned, serializable tree-derived data.
//! Tree-sitter pointer-bearing values never cross this module's transport.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

pub const PROTOCOL_VERSION: u32 = 3;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DOCUMENT_REPLICAS: usize = 4_096;
pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RETAINED_DOCUMENT_BYTES: usize = 512 * 1024 * 1024;
pub const BUILD_ID: &str = concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestContext {
    pub request_id: u64,
    pub worker_generation: u64,
    pub uri: String,
    pub incarnation: u64,
    pub content_version: u64,
    pub configuration_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Handshake {
    pub protocol_version: u32,
    pub build_id: String,
    pub worker_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveSnapshot {
    pub context: RequestContext,
    pub language: String,
    pub grammar_symbol: String,
    pub parser_path: PathBuf,
    pub artifact_digest: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncDocument {
    pub context: RequestContext,
    pub language: String,
    pub grammar_symbol: String,
    pub parser_path: PathBuf,
    pub artifact_digest: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByteEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplyDocumentEdits {
    pub context: RequestContext,
    pub base_version: u64,
    /// Ordered edits whose byte ranges address the text produced by all prior
    /// entries, matching the sequential semantics of LSP content changes.
    pub edits: Vec<ByteEdit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplyDocumentEditsAndDerive {
    pub context: RequestContext,
    pub base_version: u64,
    /// Ordered edits whose byte ranges address the text produced by all prior
    /// entries, matching the sequential semantics of LSP content changes.
    pub edits: Vec<ByteEdit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveDocumentSnapshot {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CloseDocument {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentAck {
    pub context: RequestContext,
    pub incremental: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentClosed {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerRestartRequired {
    pub context: RequestContext,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Handshake(Handshake),
    DeriveSnapshot(DeriveSnapshot),
    SyncDocument(SyncDocument),
    ApplyDocumentEdits(ApplyDocumentEdits),
    ApplyDocumentEditsAndDerive(ApplyDocumentEditsAndDerive),
    DeriveDocumentSnapshot(DeriveDocumentSnapshot),
    CloseDocument(CloseDocument),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HandshakeReady {
    pub protocol_version: u32,
    pub build_id: String,
    pub worker_generation: u64,
    pub compute_threads: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DerivedSnapshot {
    pub context: RequestContext,
    pub language: String,
    pub root_kind: String,
    pub root_start_byte: usize,
    pub root_end_byte: usize,
    pub has_error: bool,
    pub named_node_count: usize,
    pub parser_cache_hit: Option<bool>,
    pub queue_wait_ns: u64,
    pub compute_ns: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerError {
    pub context: Option<RequestContext>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HandshakeReady(HandshakeReady),
    DocumentAck(DocumentAck),
    DocumentClosed(DocumentClosed),
    WorkerRestartRequired(WorkerRestartRequired),
    Snapshot(DerivedSnapshot),
    Error(WorkerError),
}

struct DocumentReplica {
    context: RequestContext,
    language: String,
    grammar_key: GrammarKey,
    text: String,
    tree: tree_sitter::Tree,
}

impl DocumentReplica {
    fn sync(
        request: SyncDocument,
        parser: &mut tree_sitter::Parser,
    ) -> Result<(Self, DocumentAck), String> {
        validate_document_size(request.text.len())?;
        let tree = parser
            .parse(request.text.as_bytes(), None)
            .ok_or_else(|| "parser returned no tree during document sync".to_string())?;
        let ack = DocumentAck {
            context: request.context.clone(),
            incremental: false,
        };
        let grammar_key = GrammarKey::from_sync(&request);
        Ok((
            Self {
                context: request.context,
                language: request.language,
                grammar_key,
                text: request.text,
                tree,
            },
            ack,
        ))
    }

    fn apply(
        &mut self,
        request: ApplyDocumentEdits,
        parser: &mut tree_sitter::Parser,
        retained_bytes: Option<&AtomicUsize>,
    ) -> Result<DocumentAck, String> {
        self.validate_identity(&request.context)?;
        if request.base_version != self.context.content_version {
            return Err(format!(
                "base version {} does not match {}",
                request.base_version, self.context.content_version
            ));
        }
        if request.context.content_version <= request.base_version {
            return Err("target content version must advance".into());
        }
        let mut text = self.text.clone();
        let mut edited_tree = self.tree.clone();
        for edit in request.edits {
            if edit.start_byte > edit.old_end_byte
                || edit.old_end_byte > text.len()
                || !text.is_char_boundary(edit.start_byte)
                || !text.is_char_boundary(edit.old_end_byte)
            {
                return Err("edit range is not a valid UTF-8 byte range".into());
            }
            let start_position = point_at_byte(&text, edit.start_byte);
            let old_end_position = point_at_byte(&text, edit.old_end_byte);
            let new_end_byte = edit
                .start_byte
                .checked_add(edit.new_text.len())
                .ok_or_else(|| "edit length overflows byte offsets".to_string())?;
            text.replace_range(edit.start_byte..edit.old_end_byte, &edit.new_text);
            validate_document_size(text.len())?;
            let new_end_position = point_at_byte(&text, new_end_byte);
            edited_tree.edit(&tree_sitter::InputEdit {
                start_byte: edit.start_byte,
                old_end_byte: edit.old_end_byte,
                new_end_byte,
                start_position,
                old_end_position,
                new_end_position,
            });
        }
        let tree = parser
            .parse(text.as_bytes(), Some(&edited_tree))
            .ok_or_else(|| "parser returned no tree during incremental parse".to_string())?;
        if let Some(retained_bytes) = retained_bytes {
            replace_retained_bytes(retained_bytes, self.text.len(), text.len())?;
        }
        self.text = text;
        self.tree = tree;
        self.context = request.context;
        Ok(DocumentAck {
            context: self.context.clone(),
            incremental: true,
        })
    }

    fn derive(&self, context: RequestContext) -> Result<DerivedSnapshot, String> {
        self.derive_with_telemetry(context, None, Duration::ZERO, Instant::now())
    }

    fn derive_with_telemetry(
        &self,
        context: RequestContext,
        parser_cache_hit: Option<bool>,
        queue_wait: Duration,
        started: Instant,
    ) -> Result<DerivedSnapshot, String> {
        self.validate_identity(&context)?;
        if context.content_version != self.context.content_version {
            return Err(format!(
                "content version {} does not match {}",
                context.content_version, self.context.content_version
            ));
        }
        let root = self.tree.root_node();
        Ok(DerivedSnapshot {
            context,
            language: self.language.clone(),
            root_kind: root.kind().into(),
            root_start_byte: root.start_byte(),
            root_end_byte: root.end_byte(),
            has_error: root.has_error(),
            named_node_count: named_node_count(root),
            parser_cache_hit,
            queue_wait_ns: duration_ns(queue_wait),
            compute_ns: duration_ns(started.elapsed()),
        })
    }

    fn validate_identity(&self, context: &RequestContext) -> Result<(), String> {
        self.validate_lifetime(context)?;
        if context.configuration_generation != self.context.configuration_generation {
            return Err("document identity does not match worker replica".into());
        }
        Ok(())
    }

    fn validate_lifetime(&self, context: &RequestContext) -> Result<(), String> {
        if context.worker_generation != self.context.worker_generation
            || context.uri != self.context.uri
            || context.incarnation != self.context.incarnation
        {
            return Err("document lifetime does not match worker replica".into());
        }
        Ok(())
    }

    fn validate_replacement(
        &self,
        context: &RequestContext,
        language: &str,
        grammar_key: &GrammarKey,
    ) -> Result<(), String> {
        if context.worker_generation != self.context.worker_generation
            || context.uri != self.context.uri
        {
            return Err("document identity does not match worker replica".into());
        }
        if context.incarnation < self.context.incarnation {
            return Err("full sync targets a stale document incarnation".into());
        }
        if context.incarnation == self.context.incarnation {
            if language != self.language {
                return Err("language change requires a new document incarnation".into());
            }
            if grammar_key != &self.grammar_key
                && context.configuration_generation <= self.context.configuration_generation
            {
                return Err("grammar change requires a newer configuration generation".into());
            }
            if context.configuration_generation < self.context.configuration_generation {
                return Err("full sync targets a stale configuration generation".into());
            }
            if context.content_version < self.context.content_version {
                return Err("full sync targets a stale content version".into());
            }
            if context.configuration_generation == self.context.configuration_generation
                && context.content_version == self.context.content_version
            {
                return Err("full sync must advance the content version".into());
            }
        }
        Ok(())
    }
}

fn validate_document_size(bytes: usize) -> Result<(), String> {
    if bytes > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "document exceeds retained size limit {MAX_DOCUMENT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn replace_retained_bytes(
    retained_bytes: &AtomicUsize,
    old_bytes: usize,
    new_bytes: usize,
) -> Result<(), String> {
    let mut retained = retained_bytes.load(Ordering::Relaxed);
    loop {
        let next = retained
            .checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(new_bytes))
            .ok_or_else(|| "retained document byte accounting overflowed".to_string())?;
        if next > MAX_RETAINED_DOCUMENT_BYTES {
            return Err(format!(
                "worker retained document budget {MAX_RETAINED_DOCUMENT_BYTES} bytes exceeded"
            ));
        }
        match retained_bytes.compare_exchange_weak(
            retained,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(current) => retained = current,
        }
    }
}

fn reserve_retained_growth(
    retained_bytes: &AtomicUsize,
    old_bytes: usize,
    new_bytes: usize,
) -> Result<bool, String> {
    if new_bytes <= old_bytes {
        return Ok(false);
    }
    replace_retained_bytes(retained_bytes, old_bytes, new_bytes)?;
    Ok(true)
}

fn point_at_byte(text: &str, byte: usize) -> tree_sitter::Point {
    let prefix = &text.as_bytes()[..byte];
    let row = prefix.iter().filter(|&&value| value == b'\n').count();
    let column = prefix
        .iter()
        .rposition(|&value| value == b'\n')
        .map_or(prefix.len(), |index| prefix.len() - index - 1);
    tree_sitter::Point::new(row, column)
}

pub fn encode_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn decode_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    let mut length = [0_u8; 4];
    match reader.read(&mut length[..1])? {
        0 => return Ok(None),
        1 => reader.read_exact(&mut length[1..])?,
        _ => unreachable!("one-byte read returned more than one byte"),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn named_node_count(root: tree_sitter::Node<'_>) -> usize {
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

pub fn derive_snapshot_with_language(
    request: DeriveSnapshot,
    language: tree_sitter::Language,
) -> Response {
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&language) {
        return Response::Error(WorkerError {
            context: Some(request.context),
            message: format!("incompatible grammar: {error}"),
        });
    }
    derive_snapshot_with_parser(request, &mut parser, false, Duration::ZERO)
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn derive_snapshot_with_parser(
    request: DeriveSnapshot,
    parser: &mut tree_sitter::Parser,
    parser_cache_hit: bool,
    queue_wait: Duration,
) -> Response {
    let started = Instant::now();
    let Some(tree) = parser.parse(request.text.as_bytes(), None) else {
        return Response::Error(WorkerError {
            context: Some(request.context),
            message: "parser returned no tree".into(),
        });
    };
    let root = tree.root_node();
    Response::Snapshot(DerivedSnapshot {
        context: request.context,
        language: request.language,
        root_kind: root.kind().into(),
        root_start_byte: root.start_byte(),
        root_end_byte: root.end_byte(),
        has_error: root.has_error(),
        named_node_count: named_node_count(root),
        parser_cache_hit: Some(parser_cache_hit),
        queue_wait_ns: duration_ns(queue_wait),
        compute_ns: duration_ns(started.elapsed()),
    })
}

#[derive(Clone, Debug)]
struct GrammarKey {
    parser_path: PathBuf,
    grammar_symbol: String,
    artifact_digest: String,
}

impl GrammarKey {
    fn from_sync(request: &SyncDocument) -> Self {
        Self {
            parser_path: request
                .parser_path
                .canonicalize()
                .unwrap_or_else(|_| request.parser_path.clone()),
            grammar_symbol: request.grammar_symbol.clone(),
            artifact_digest: request.artifact_digest.clone(),
        }
    }

    fn from_derive(request: &DeriveSnapshot) -> Self {
        Self {
            parser_path: request
                .parser_path
                .canonicalize()
                .unwrap_or_else(|_| request.parser_path.clone()),
            grammar_symbol: request.grammar_symbol.clone(),
            artifact_digest: request.artifact_digest.clone(),
        }
    }
}

impl PartialEq for GrammarKey {
    fn eq(&self, other: &Self) -> bool {
        self.grammar_symbol == other.grammar_symbol && self.artifact_digest == other.artifact_digest
    }
}

impl Eq for GrammarKey {}

impl std::hash::Hash for GrammarKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.grammar_symbol.hash(state);
        self.artifact_digest.hash(state);
    }
}

struct WorkerThreadState {
    loader: crate::language::loader::ParserLoader,
    languages: HashMap<GrammarKey, tree_sitter::Language>,
    parsers: HashMap<GrammarKey, tree_sitter::Parser>,
}

pub struct LocalDeriver {
    state: WorkerThreadState,
}

pub struct LocalDocumentReplica {
    state: WorkerThreadState,
    replica: Option<DocumentReplica>,
}

impl Default for LocalDeriver {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalDeriver {
    pub fn new() -> Self {
        Self {
            state: WorkerThreadState::new(),
        }
    }

    pub fn derive(&mut self, request: DeriveSnapshot) -> Response {
        self.state.derive(request, Duration::ZERO)
    }
}

impl Default for LocalDocumentReplica {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalDocumentReplica {
    pub fn new() -> Self {
        Self {
            state: WorkerThreadState::new(),
            replica: None,
        }
    }

    pub fn sync_document(&mut self, request: SyncDocument) -> Response {
        let context = request.context.clone();
        let grammar_key = GrammarKey::from_sync(&request);
        let Self { state, replica } = self;
        if let Some(existing) = replica.as_ref()
            && let Err(message) =
                existing.validate_replacement(&context, &request.language, &grammar_key)
        {
            return Response::Error(WorkerError {
                context: Some(context),
                message,
            });
        }
        state.with_parser(
            grammar_key,
            context.clone(),
            |parser, _| match DocumentReplica::sync(request, parser) {
                Ok((document, ack)) => {
                    *replica = Some(document);
                    Response::DocumentAck(ack)
                }
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            },
        )
    }

    pub fn apply_document_edits(&mut self, request: ApplyDocumentEdits) -> Response {
        let context = request.context.clone();
        let Some(grammar_key) = self
            .replica
            .as_ref()
            .map(|replica| replica.grammar_key.clone())
        else {
            return Response::Error(WorkerError {
                context: Some(context),
                message: "document replica is missing; full sync required".into(),
            });
        };
        let Self { state, replica } = self;
        state.with_parser(grammar_key, context.clone(), |parser, _| {
            match replica
                .as_mut()
                .expect("replica presence checked before parser checkout")
                .apply(request, parser, None)
            {
                Ok(ack) => Response::DocumentAck(ack),
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            }
        })
    }

    pub fn derive_document_snapshot(&self, request: DeriveDocumentSnapshot) -> Response {
        let context = request.context;
        let Some(replica) = &self.replica else {
            return Response::Error(WorkerError {
                context: Some(context),
                message: "document replica is missing; full sync required".into(),
            });
        };
        match replica.derive(context.clone()) {
            Ok(snapshot) => Response::Snapshot(snapshot),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        }
    }
}

impl WorkerThreadState {
    fn new() -> Self {
        Self {
            loader: crate::language::loader::ParserLoader::new(),
            languages: HashMap::new(),
            parsers: HashMap::new(),
        }
    }

    fn derive(&mut self, request: DeriveSnapshot, queue_wait: Duration) -> Response {
        let request_context = request.context.clone();
        let key = GrammarKey::from_derive(&request);
        self.with_parser(key, request_context, |parser, parser_cache_hit| {
            derive_snapshot_with_parser(request, parser, parser_cache_hit, queue_wait)
        })
    }

    fn with_parser(
        &mut self,
        key: GrammarKey,
        request_context: RequestContext,
        operation: impl FnOnce(&mut tree_sitter::Parser, bool) -> Response,
    ) -> Response {
        let language = match self.languages.get(&key).cloned() {
            Some(language) => language,
            None => match self
                .loader
                .load_language(&key.parser_path, &key.grammar_symbol)
            {
                Ok(language) => {
                    self.languages.insert(key.clone(), language.clone());
                    language
                }
                Err(error) => {
                    return Response::Error(WorkerError {
                        context: Some(request_context),
                        message: error.to_string(),
                    });
                }
            },
        };
        let parser_cache_hit = self.parsers.contains_key(&key);
        let mut parser = if let Some(parser) = self.parsers.remove(&key) {
            parser
        } else {
            let mut parser = tree_sitter::Parser::new();
            if let Err(error) = parser.set_language(&language) {
                return Response::Error(WorkerError {
                    context: Some(request_context),
                    message: format!("incompatible grammar: {error}"),
                });
            }
            parser
        };
        let response = operation(&mut parser, parser_cache_hit);
        self.parsers.insert(key, parser);
        response
    }
}

thread_local! {
    static WORKER_THREAD_STATE: RefCell<WorkerThreadState> =
        RefCell::new(WorkerThreadState::new());
}

fn derive_snapshot(request: DeriveSnapshot, queue_wait: Duration) -> Response {
    WORKER_THREAD_STATE.with(|state| state.borrow_mut().derive(request, queue_wait))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DocumentKey {
    uri: String,
}

impl From<&RequestContext> for DocumentKey {
    fn from(context: &RequestContext) -> Self {
        Self {
            uri: context.uri.clone(),
        }
    }
}

type DocumentStore = Arc<dashmap::DashMap<DocumentKey, Arc<Mutex<DocumentReplica>>>>;
type ClosedDocuments = Arc<dashmap::DashMap<DocumentKey, u64>>;
type DocumentCapacity = Arc<Mutex<usize>>;
type RetainedDocumentBytes = Arc<AtomicUsize>;
type LaneJob = Box<dyn FnOnce() -> LaneAction + Send + 'static>;

#[derive(Clone, Copy)]
enum LaneAction {
    Keep,
    Retire,
}

struct DocumentLane {
    state: Mutex<DocumentLaneState>,
    key: Option<DocumentKey>,
    registry: Weak<LaneRegistry>,
}

#[derive(Default)]
struct DocumentLaneState {
    running: bool,
    retire_when_idle: bool,
    pending: VecDeque<LaneJob>,
}

impl DocumentLane {
    fn new(key: DocumentKey, registry: Weak<LaneRegistry>) -> Self {
        Self {
            state: Mutex::new(DocumentLaneState {
                retire_when_idle: true,
                ..DocumentLaneState::default()
            }),
            key: Some(key),
            registry,
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            state: Mutex::new(DocumentLaneState::default()),
            key: None,
            registry: Weak::new(),
        }
    }

    fn submit(self: &Arc<Self>, pool: &Arc<rayon::ThreadPool>, job: LaneJob) {
        let should_start = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.pending.push_back(job);
            if state.running {
                false
            } else {
                state.running = true;
                true
            }
        };
        if should_start {
            let lane = Arc::clone(self);
            pool.spawn(move || lane.drain());
        }
    }

    fn drain(&self) {
        loop {
            let job = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match state.pending.pop_front() {
                    Some(job) => job,
                    None => {
                        state.running = false;
                        let retire = state.retire_when_idle;
                        drop(state);
                        if retire {
                            self.remove_if_retired();
                        }
                        return;
                    }
                }
            };
            let action = job();
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match action {
                LaneAction::Keep => state.retire_when_idle = false,
                LaneAction::Retire => state.retire_when_idle = true,
            }
        }
    }

    fn remove_if_retired(&self) {
        let (Some(key), Some(registry)) = (&self.key, self.registry.upgrade()) else {
            return;
        };
        if let dashmap::mapref::entry::Entry::Occupied(entry) = registry.entry(key.clone()) {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.running
                && state.pending.is_empty()
                && state.retire_when_idle
                && std::ptr::eq(Arc::as_ptr(entry.get()), self)
            {
                drop(state);
                entry.remove();
            }
        }
    }
}

type LaneRegistry = dashmap::DashMap<DocumentKey, Arc<DocumentLane>>;
type DocumentLanes = Arc<LaneRegistry>;

fn submit_document_job(
    lanes: &DocumentLanes,
    key: DocumentKey,
    pool: &Arc<rayon::ThreadPool>,
    job: LaneJob,
) {
    let lane = {
        let entry = lanes
            .entry(key.clone())
            .or_insert_with(|| Arc::new(DocumentLane::new(key, Arc::downgrade(lanes))));
        Arc::clone(entry.value())
    };
    lane.submit(pool, job);
}

fn handle_work(
    request: Request,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    document_capacity: &DocumentCapacity,
    retained_document_bytes: &RetainedDocumentBytes,
    queue_wait: Duration,
) -> Response {
    let started = Instant::now();
    match request {
        Request::DeriveSnapshot(request) => derive_snapshot(request, queue_wait),
        Request::SyncDocument(request) => sync_document(
            request,
            documents,
            closed_documents,
            document_capacity,
            retained_document_bytes,
        ),
        Request::ApplyDocumentEdits(request) => {
            apply_document_edits(request, documents, retained_document_bytes)
        }
        Request::ApplyDocumentEditsAndDerive(request) => apply_document_edits_and_derive(
            request,
            documents,
            retained_document_bytes,
            queue_wait,
            started,
        ),
        Request::DeriveDocumentSnapshot(request) => {
            derive_document_snapshot(request, documents, queue_wait, started)
        }
        Request::CloseDocument(request) => close_document(
            request,
            documents,
            closed_documents,
            retained_document_bytes,
        ),
        Request::Handshake(_) => Response::Error(WorkerError {
            context: None,
            message: "handshake may only be sent once".into(),
        }),
    }
}

fn request_context(request: &Request) -> Option<&RequestContext> {
    match request {
        Request::Handshake(_) => None,
        Request::DeriveSnapshot(request) => Some(&request.context),
        Request::SyncDocument(request) => Some(&request.context),
        Request::ApplyDocumentEdits(request) => Some(&request.context),
        Request::ApplyDocumentEditsAndDerive(request) => Some(&request.context),
        Request::DeriveDocumentSnapshot(request) => Some(&request.context),
        Request::CloseDocument(request) => Some(&request.context),
    }
}

fn document_key(request: &Request) -> Option<DocumentKey> {
    match request {
        Request::SyncDocument(request) => Some(DocumentKey::from(&request.context)),
        Request::ApplyDocumentEdits(request) => Some(DocumentKey::from(&request.context)),
        Request::ApplyDocumentEditsAndDerive(request) => Some(DocumentKey::from(&request.context)),
        Request::DeriveDocumentSnapshot(request) => Some(DocumentKey::from(&request.context)),
        Request::CloseDocument(request) => Some(DocumentKey::from(&request.context)),
        Request::Handshake(_) | Request::DeriveSnapshot(_) => None,
    }
}

fn poisoned_document_restart(context: RequestContext) -> Response {
    Response::WorkerRestartRequired(WorkerRestartRequired {
        context,
        reason: "document replica lock is poisoned; restart and resync".into(),
    })
}

fn sync_document(
    request: SyncDocument,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    document_capacity: &DocumentCapacity,
    retained_document_bytes: &RetainedDocumentBytes,
) -> Response {
    let context = request.context.clone();
    let request_text_len = request.text.len();
    if let Err(message) = validate_document_size(request_text_len) {
        return Response::Error(WorkerError {
            context: Some(context),
            message,
        });
    }
    let document_key = DocumentKey::from(&context);
    let grammar_key = GrammarKey::from_sync(&request);
    let replaced_bytes = if let Some(existing) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    {
        match existing.lock() {
            Ok(replica) => {
                match replica.validate_replacement(&context, &request.language, &grammar_key) {
                    Ok(()) => replica.text.len(),
                    Err(message) => {
                        return Response::Error(WorkerError {
                            context: Some(context),
                            message,
                        });
                    }
                }
            }
            Err(_) => {
                return poisoned_document_restart(context);
            }
        }
    } else {
        if let Some(closed_incarnation) = closed_documents.get(&document_key)
            && context.incarnation <= *closed_incarnation
        {
            return Response::Error(WorkerError {
                context: Some(context),
                message: "full sync targets a closed document incarnation".into(),
            });
        }
        0
    };
    let reserves_slot =
        !documents.contains_key(&document_key) && !closed_documents.contains_key(&document_key);
    if reserves_slot {
        let mut known_documents = document_capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *known_documents >= MAX_DOCUMENT_REPLICAS {
            return Response::WorkerRestartRequired(WorkerRestartRequired {
                context,
                reason: format!(
                    "document identity limit {MAX_DOCUMENT_REPLICAS} reached; restart and resync"
                ),
            });
        }
        *known_documents += 1;
    }
    let reserved_growth =
        match reserve_retained_growth(retained_document_bytes, replaced_bytes, request_text_len) {
            Ok(reserved) => reserved,
            Err(message) => {
                if reserves_slot {
                    let mut known_documents = document_capacity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *known_documents = known_documents.saturating_sub(1);
                }
                return Response::Error(WorkerError {
                    context: Some(context),
                    message,
                });
            }
        };
    let response = WORKER_THREAD_STATE.with(|state| {
        state
            .borrow_mut()
            .with_parser(
                grammar_key,
                context.clone(),
                |parser, _| match DocumentReplica::sync(request, parser) {
                    Ok((replica, ack)) => {
                        documents.insert(document_key, Arc::new(Mutex::new(replica)));
                        closed_documents.remove(&DocumentKey::from(&ack.context));
                        Response::DocumentAck(ack)
                    }
                    Err(message) => Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    }),
                },
            )
    });
    if reserves_slot && !matches!(&response, Response::DocumentAck(_)) {
        let mut known_documents = document_capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *known_documents = known_documents.saturating_sub(1);
    }
    if matches!(&response, Response::DocumentAck(_)) && request_text_len < replaced_bytes {
        replace_retained_bytes(retained_document_bytes, replaced_bytes, request_text_len)
            .expect("committing a document shrink cannot exceed the retained-byte budget");
    } else if !matches!(&response, Response::DocumentAck(_)) && reserved_growth {
        replace_retained_bytes(retained_document_bytes, request_text_len, replaced_bytes)
            .expect("rolling back retained growth cannot exceed the retained-byte budget");
    }
    response
}

fn apply_document_edits(
    request: ApplyDocumentEdits,
    documents: &DocumentStore,
    retained_document_bytes: &RetainedDocumentBytes,
) -> Response {
    let context = request.context.clone();
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    let grammar_key = match replica.lock() {
        Ok(replica) => replica.grammar_key.clone(),
        Err(_) => {
            return poisoned_document_restart(context);
        }
    };
    WORKER_THREAD_STATE.with(|state| {
        state
            .borrow_mut()
            .with_parser(grammar_key, context.clone(), |parser, _| {
                let Ok(mut replica) = replica.lock() else {
                    return poisoned_document_restart(context);
                };
                match replica.apply(request, parser, Some(retained_document_bytes)) {
                    Ok(ack) => Response::DocumentAck(ack),
                    Err(message) => Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    }),
                }
            })
    })
}

fn apply_document_edits_and_derive(
    request: ApplyDocumentEditsAndDerive,
    documents: &DocumentStore,
    retained_document_bytes: &RetainedDocumentBytes,
    queue_wait: Duration,
    started: Instant,
) -> Response {
    let context = request.context.clone();
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    let grammar_key = match replica.lock() {
        Ok(replica) => replica.grammar_key.clone(),
        Err(_) => {
            return poisoned_document_restart(context);
        }
    };
    WORKER_THREAD_STATE.with(|state| {
        state
            .borrow_mut()
            .with_parser(grammar_key, context.clone(), |parser, parser_cache_hit| {
                let Ok(mut replica) = replica.lock() else {
                    return poisoned_document_restart(context);
                };
                let edits = ApplyDocumentEdits {
                    context: request.context,
                    base_version: request.base_version,
                    edits: request.edits,
                };
                if let Err(message) = replica.apply(edits, parser, Some(retained_document_bytes)) {
                    return Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    });
                }
                match replica.derive_with_telemetry(
                    context.clone(),
                    Some(parser_cache_hit),
                    queue_wait,
                    started,
                ) {
                    Ok(snapshot) => Response::Snapshot(snapshot),
                    Err(message) => Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    }),
                }
            })
    })
}

fn derive_document_snapshot(
    request: DeriveDocumentSnapshot,
    documents: &DocumentStore,
    queue_wait: Duration,
    started: Instant,
) -> Response {
    let context = request.context;
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(replica) => {
            match replica.derive_with_telemetry(context.clone(), None, queue_wait, started) {
                Ok(snapshot) => Response::Snapshot(snapshot),
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            }
        }
        Err(_) => poisoned_document_restart(context),
    }
}

fn close_document(
    request: CloseDocument,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    retained_document_bytes: &RetainedDocumentBytes,
) -> Response {
    let context = request.context;
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing".into(),
        });
    };
    let removed_bytes = match replica.lock() {
        Ok(replica) => {
            if let Err(message) = replica.validate_lifetime(&context) {
                return Response::Error(WorkerError {
                    context: Some(context),
                    message,
                });
            }
            replica.text.len()
        }
        Err(_) => return poisoned_document_restart(context),
    };
    documents.remove(&document_key);
    replace_retained_bytes(retained_document_bytes, removed_bytes, 0)
        .expect("removing a document cannot exceed the retained-byte budget");
    closed_documents.insert(document_key, context.incarnation);
    Response::DocumentClosed(DocumentClosed { context })
}

pub fn run<R, W>(reader: R, writer: W, compute_threads: usize) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_build_id(reader, writer, compute_threads, BUILD_ID)
}

fn run_with_build_id<R, W>(
    mut reader: R,
    writer: W,
    compute_threads: usize,
    build_id: &str,
) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    if compute_threads == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker compute thread count must be positive",
        ));
    }
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(compute_threads)
            .thread_name(|index| format!("kakehashi-tree-worker-{index}"))
            .build()
            .map_err(io::Error::other)?,
    );
    let max_inflight = compute_threads.saturating_mul(2).max(1);
    let (permits, permit_rx) = mpsc::sync_channel(max_inflight);
    for _ in 0..max_inflight {
        permits
            .send(())
            .map_err(|_| io::Error::other("worker admission queue stopped"))?;
    }
    let (responses, response_rx) =
        mpsc::sync_channel::<(Response, Option<AdmissionPermit>)>(max_inflight);
    let writer_thread = std::thread::spawn(move || -> io::Result<()> {
        let mut writer = writer;
        for (response, _permit) in response_rx {
            encode_frame(&mut writer, &response)?;
        }
        Ok(())
    });

    let handshake = match decode_frame::<_, Request>(&mut reader)? {
        Some(Request::Handshake(handshake)) => handshake,
        None => {
            drop(responses);
            return join_writer(writer_thread);
        }
        Some(_) => {
            drop(responses);
            join_writer(writer_thread)?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree worker requires handshake as its first message",
            ));
        }
    };
    if handshake.protocol_version != PROTOCOL_VERSION || handshake.build_id != build_id {
        responses
            .send((
                Response::Error(WorkerError {
                    context: None,
                    message: "worker protocol/build identity mismatch".into(),
                }),
                None,
            ))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
        drop(responses);
        join_writer(writer_thread)?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker protocol/build identity mismatch",
        ));
    }
    let worker_generation = handshake.worker_generation;
    responses
        .send((
            Response::HandshakeReady(HandshakeReady {
                protocol_version: PROTOCOL_VERSION,
                build_id: build_id.into(),
                worker_generation,
                compute_threads,
            }),
            None,
        ))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
    #[cfg(feature = "e2e")]
    inject_idle_worker_crash_once();

    let documents = Arc::new(dashmap::DashMap::new());
    let closed_documents = Arc::new(dashmap::DashMap::new());
    let document_capacity = Arc::new(Mutex::new(0));
    let retained_document_bytes = Arc::new(AtomicUsize::new(0));
    let document_lanes: DocumentLanes = Arc::new(dashmap::DashMap::new());
    while let Some(request) = decode_frame::<_, Request>(&mut reader)? {
        let Some(context) = request_context(&request) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake may only be sent once",
            ));
        };
        if context.worker_generation != worker_generation {
            responses
                .send((
                    Response::Error(WorkerError {
                        context: Some(context.clone()),
                        message: "stale worker generation".into(),
                    }),
                    None,
                ))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
            continue;
        }
        #[cfg(feature = "e2e")]
        if let Some(response) = inject_worker_failure_once(&request) {
            responses
                .send((response, None))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
            continue;
        }
        let enqueued = Instant::now();
        permit_rx
            .recv()
            .map_err(|_| io::Error::other("worker admission queue stopped"))?;
        let responses = responses.clone();
        let permit = AdmissionPermit(permits.clone());
        let documents = Arc::clone(&documents);
        let closed_documents = Arc::clone(&closed_documents);
        let document_capacity = Arc::clone(&document_capacity);
        let retained_document_bytes = Arc::clone(&retained_document_bytes);
        let lane_key = document_key(&request);
        let action_key = lane_key.clone();
        let job: LaneJob = Box::new(move || {
            let response = handle_work(
                request,
                &documents,
                &closed_documents,
                &document_capacity,
                &retained_document_bytes,
                enqueued.elapsed(),
            );
            let action = if action_key
                .as_ref()
                .is_some_and(|key| documents.contains_key(key))
            {
                LaneAction::Keep
            } else {
                LaneAction::Retire
            };
            let _ = responses.send((response, Some(permit)));
            action
        });
        if let Some(lane_key) = lane_key {
            submit_document_job(&document_lanes, lane_key, &pool, job);
        } else {
            pool.spawn(move || {
                let _ = job();
            });
        }
    }
    for _ in 0..max_inflight {
        permit_rx
            .recv()
            .map_err(|_| io::Error::other("worker admission queue stopped"))?;
    }
    drop(responses);
    join_writer(writer_thread)
}

#[cfg(feature = "e2e")]
fn inject_worker_failure_once(request: &Request) -> Option<Response> {
    let Request::SyncDocument(sync) = request else {
        return None;
    };
    if let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_CRASH_ONCE_FILE")
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    {
        std::process::exit(86);
    }
    if let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_RESTART_ONCE_FILE")
        && std::env::var("KAKEHASHI_TREE_WORKER_RESTART_URI_SUFFIX")
            .ok()
            .is_none_or(|suffix| sync.context.uri.ends_with(&suffix))
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    {
        return Some(Response::WorkerRestartRequired(WorkerRestartRequired {
            context: sync.context.clone(),
            reason: "injected systemic restart".into(),
        }));
    }
    if std::env::var("KAKEHASHI_TREE_WORKER_ERROR_URI_SUFFIX")
        .ok()
        .is_some_and(|suffix| sync.context.uri.ends_with(&suffix))
    {
        return Some(Response::Error(WorkerError {
            context: Some(sync.context.clone()),
            message: "injected document rejection".into(),
        }));
    }
    None
}

#[cfg(feature = "e2e")]
fn inject_idle_worker_crash_once() {
    let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_IDLE_CRASH_ONCE_FILE") else {
        return;
    };
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        std::process::exit(87);
    });
}

fn join_writer(thread: std::thread::JoinHandle<io::Result<()>>) -> io::Result<()> {
    thread
        .join()
        .map_err(|_| io::Error::other("worker writer panicked"))?
}

struct AdmissionPermit(mpsc::SyncSender<()>);

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

pub fn run_stdio(compute_threads: usize) -> io::Result<()> {
    let build_id = executable_digest(&std::env::current_exe()?)?;
    run_with_build_id(
        std::io::stdin(),
        std::io::stdout(),
        compute_threads,
        &build_id,
    )
}

fn executable_digest(path: &std::path::Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub struct Client {
    child: Arc<Mutex<Child>>,
    terminated_by_transport: Arc<AtomicBool>,
    outbound: Mutex<Option<mpsc::SyncSender<Request>>>,
    routes: Arc<Mutex<HashMap<u64, Route>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    ready: HandshakeReady,
    max_inflight: usize,
}

struct Route {
    expected: RequestContext,
    sender: mpsc::Sender<io::Result<Response>>,
}

impl Client {
    pub fn spawn(
        executable: &std::path::Path,
        compute_threads: usize,
        worker_generation: u64,
    ) -> io::Result<Self> {
        if compute_threads == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker compute thread count must be positive",
            ));
        }
        let build_id = executable_digest(executable)?;
        let mut child = Command::new(executable)
            .args(["__tree-worker", "--threads", &compute_threads.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("worker stdin was not piped"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("worker stdout was not piped"))?;
        let child = Arc::new(Mutex::new(child));
        let terminated_by_transport = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(Mutex::new(HashMap::<u64, Route>::new()));
        let reader_routes = Arc::clone(&routes);
        let reader_child = Arc::clone(&child);
        let reader_terminated_by_transport = Arc::clone(&terminated_by_transport);
        let (handshake_tx, handshake_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut handshake_tx = Some(handshake_tx);
            loop {
                match decode_frame::<_, Response>(&mut stdout) {
                    Ok(Some(response)) => {
                        if let Some(handshake) = handshake_tx.take() {
                            let _ = handshake.send(Ok(response));
                            continue;
                        }
                        if let Err(error) = route_response(&reader_routes, response) {
                            let kind = error.kind();
                            let message = error.to_string();
                            fail_routes(None, &reader_routes, kind, &message);
                            break;
                        }
                    }
                    Ok(None) => {
                        fail_routes(
                            handshake_tx.take(),
                            &reader_routes,
                            io::ErrorKind::UnexpectedEof,
                            "tree worker closed its response stream",
                        );
                        break;
                    }
                    Err(error) => {
                        let kind = error.kind();
                        let message = error.to_string();
                        fail_routes(handshake_tx.take(), &reader_routes, kind, &message);
                        break;
                    }
                }
            }
            if let Ok(mut child) = reader_child.lock() {
                terminate_by_transport(
                    &mut child,
                    &reader_terminated_by_transport,
                    Duration::from_secs(1),
                );
            }
        });
        if let Err(error) = encode_frame(
            &mut stdin,
            &Request::Handshake(Handshake {
                protocol_version: PROTOCOL_VERSION,
                build_id: build_id.clone(),
                worker_generation,
            }),
        ) {
            return failed_spawn(child, stdin, reader, error);
        }
        let ready = match handshake_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(Response::HandshakeReady(ready)))
                if ready.protocol_version == PROTOCOL_VERSION
                    && ready.build_id == build_id
                    && ready.worker_generation == worker_generation
                    && ready.compute_threads == compute_threads =>
            {
                ready
            }
            Ok(Ok(response)) => {
                return failed_spawn(
                    child,
                    stdin,
                    reader,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid worker handshake response: {response:?}"),
                    ),
                );
            }
            Ok(Err(error)) => {
                return failed_spawn(child, stdin, reader, error);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return failed_spawn(
                    child,
                    stdin,
                    reader,
                    io::Error::new(io::ErrorKind::TimedOut, "tree worker handshake timed out"),
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return failed_spawn(
                    child,
                    stdin,
                    reader,
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "tree worker handshake channel closed",
                    ),
                );
            }
        };
        let max_inflight = compute_threads.saturating_mul(4).max(1);
        let (outbound, outbound_rx) = mpsc::sync_channel::<Request>(max_inflight);
        let writer_routes = Arc::clone(&routes);
        let writer_child = Arc::clone(&child);
        let writer_terminated_by_transport = Arc::clone(&terminated_by_transport);
        let writer = std::thread::spawn(move || {
            for request in outbound_rx {
                if let Err(error) = encode_frame(&mut stdin, &request) {
                    let kind = error.kind();
                    let message = error.to_string();
                    fail_routes(None, &writer_routes, kind, &message);
                    if let Ok(mut child) = writer_child.lock() {
                        terminate_by_transport(
                            &mut child,
                            &writer_terminated_by_transport,
                            Duration::from_secs(1),
                        );
                    }
                    break;
                }
            }
        });
        Ok(Self {
            child,
            terminated_by_transport,
            outbound: Mutex::new(Some(outbound)),
            routes,
            reader: Some(reader),
            writer: Some(writer),
            ready,
            max_inflight,
        })
    }

    pub fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child
            .lock()
            .map_err(|_| io::Error::other("worker child lock is poisoned"))?
            .try_wait()
    }

    pub fn was_terminated_by_transport(&self) -> bool {
        self.terminated_by_transport.load(Ordering::Acquire)
    }

    pub fn compute_threads(&self) -> usize {
        self.ready.compute_threads
    }

    pub fn derive(&self, request: DeriveSnapshot) -> io::Result<Response> {
        self.derive_with_timeout(request, Duration::from_secs(60))
    }

    /// Sends one request with a process-level recovery deadline.
    ///
    /// A timeout terminates the shared worker because native parser code may be
    /// non-cooperative. All concurrent requests consequently fail, and this
    /// client cannot be reused; the supervisor must create a new generation.
    pub fn derive_with_timeout(
        &self,
        request: DeriveSnapshot,
        timeout: Duration,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(Request::DeriveSnapshot(request), context, timeout)
    }

    pub fn sync_document(&self, request: SyncDocument) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(
            Request::SyncDocument(request),
            context,
            Duration::from_secs(60),
        )
    }

    pub fn apply_document_edits(&self, request: ApplyDocumentEdits) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(
            Request::ApplyDocumentEdits(request),
            context,
            Duration::from_secs(60),
        )
    }

    pub fn apply_document_edits_and_derive(
        &self,
        request: ApplyDocumentEditsAndDerive,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(
            Request::ApplyDocumentEditsAndDerive(request),
            context,
            Duration::from_secs(60),
        )
    }

    pub fn derive_document_snapshot(
        &self,
        request: DeriveDocumentSnapshot,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(
            Request::DeriveDocumentSnapshot(request),
            context,
            Duration::from_secs(60),
        )
    }

    pub fn close_document(&self, request: CloseDocument) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(
            Request::CloseDocument(request),
            context,
            Duration::from_secs(60),
        )
    }

    fn request_with_timeout(
        &self,
        request: Request,
        expected: RequestContext,
        timeout: Duration,
    ) -> io::Result<Response> {
        let started = Instant::now();
        if expected.worker_generation != self.ready.worker_generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request targets a stale worker generation",
            ));
        }
        let request_id = expected.request_id;
        let (response_tx, response_rx) = mpsc::channel();
        {
            let mut routes = self
                .routes
                .lock()
                .map_err(|_| io::Error::other("worker response router is poisoned"))?;
            if routes.contains_key(&request_id) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("tree worker request {request_id} is already active"),
                ));
            }
            if routes.len() >= self.max_inflight {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "tree worker in-flight limit reached",
                ));
            }
            routes.insert(
                request_id,
                Route {
                    expected,
                    sender: response_tx,
                },
            );
        }
        {
            let outbound = self
                .outbound
                .lock()
                .map_err(|_| io::Error::other("worker outbound lock is poisoned"))?;
            let outbound = outbound
                .as_ref()
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker is shut down"))?;
            if let Err(error) = outbound.try_send(request) {
                self.remove_route(request_id);
                return Err(match error {
                    mpsc::TrySendError::Full(_) => io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "tree worker outbound queue is full",
                    ),
                    mpsc::TrySendError::Disconnected(_) => {
                        io::Error::new(io::ErrorKind::BrokenPipe, "tree worker writer stopped")
                    }
                });
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let response = match response_rx.recv_timeout(remaining) {
            Ok(response) => response?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.remove_route(request_id);
                if let Ok(mut child) = self.child.lock() {
                    terminate(&mut child, Duration::from_secs(1));
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("tree worker request {request_id} timed out"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tree worker response channel closed",
                ));
            }
        };
        Ok(response)
    }

    fn remove_route(&self, request_id: u64) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(&request_id);
        }
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.outbound
            .get_mut()
            .map_err(|_| io::Error::other("worker outbound lock is poisoned"))?
            .take();
        let status = {
            let mut child = self
                .child
                .lock()
                .map_err(|_| io::Error::other("worker child lock is poisoned"))?;
            wait_until(&mut child, Duration::from_secs(2))?
        };
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "tree worker exited with {status}"
            )))
        }
    }
}

fn failed_spawn(
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    reader: std::thread::JoinHandle<()>,
    error: io::Error,
) -> io::Result<Client> {
    drop(stdin);
    if let Ok(mut child) = child.lock() {
        terminate(&mut child, Duration::from_secs(1));
    }
    let _ = reader.join();
    Err(error)
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(outbound) = self.outbound.get_mut() {
            outbound.take();
        }
        if let Ok(mut child) = self.child.lock() {
            terminate(&mut child, Duration::from_secs(1));
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn response_request_id(response: &Response) -> Option<u64> {
    match response {
        Response::DocumentAck(ack) => Some(ack.context.request_id),
        Response::DocumentClosed(closed) => Some(closed.context.request_id),
        Response::WorkerRestartRequired(required) => Some(required.context.request_id),
        Response::Snapshot(snapshot) => Some(snapshot.context.request_id),
        Response::Error(error) => error.context.as_ref().map(|context| context.request_id),
        Response::HandshakeReady(_) => None,
    }
}

fn response_context(response: &Response) -> Option<&RequestContext> {
    match response {
        Response::DocumentAck(ack) => Some(&ack.context),
        Response::DocumentClosed(closed) => Some(&closed.context),
        Response::WorkerRestartRequired(required) => Some(&required.context),
        Response::Snapshot(snapshot) => Some(&snapshot.context),
        Response::Error(error) => error.context.as_ref(),
        Response::HandshakeReady(_) => None,
    }
}

fn route_response(routes: &Mutex<HashMap<u64, Route>>, response: Response) -> io::Result<()> {
    let Some(request_id) = response_request_id(&response) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected response without request id",
        ));
    };
    let route = routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request_id)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response has unknown request id {request_id}"),
            )
        })?;
    if response_context(&response) != Some(&route.expected) {
        let error = io::Error::new(
            io::ErrorKind::InvalidData,
            format!("response context mismatch for request {request_id}"),
        );
        let _ = route
            .sender
            .send(Err(io::Error::new(error.kind(), error.to_string())));
        return Err(error);
    }
    let _ = route.sender.send(Ok(response));
    Ok(())
}

fn fail_routes(
    handshake: Option<mpsc::Sender<io::Result<Response>>>,
    routes: &Mutex<HashMap<u64, Route>>,
    kind: io::ErrorKind,
    message: &str,
) {
    if let Some(handshake) = handshake {
        let _ = handshake.send(Err(io::Error::new(kind, message.to_string())));
    }
    let routes = std::mem::take(
        &mut *routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for route in routes.into_values() {
        let _ = route
            .sender
            .send(Err(io::Error::new(kind, message.to_string())));
    }
}

fn wait_until(child: &mut Child, timeout: Duration) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let kill_error = child.kill().err();
            let reap_deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                if Instant::now() >= reap_deadline {
                    return Err(kill_error.unwrap_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "tree worker did not exit after kill",
                        )
                    }));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate(child: &mut Child, timeout: Duration) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    let _ = wait_until(child, timeout);
}

fn terminate_by_transport(child: &mut Child, marker: &AtomicBool, timeout: Duration) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    marker.store(true, Ordering::Release);
    terminate(child, timeout);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::{
        ApplyDocumentEdits, BUILD_ID, ByteEdit, CloseDocument, DeriveSnapshot, DocumentKey,
        DocumentLane, DocumentReplica, GrammarKey, Handshake, HandshakeReady, LaneAction,
        LaneRegistry, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_REPLICAS, MAX_RETAINED_DOCUMENT_BYTES,
        PROTOCOL_VERSION, Request, RequestContext, Response, Route, SyncDocument, WorkerError,
        close_document, decode_frame, derive_snapshot_with_language, encode_frame,
        named_node_count, replace_retained_bytes, reserve_retained_growth, route_response, run,
        submit_document_job, sync_document, terminate_by_transport, validate_document_size,
    };

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn transport_termination_marker_excludes_already_exited_workers() {
        let marker = AtomicBool::new(false);
        let mut running = std::process::Command::new("sh")
            .args(["-c", "sleep 10"])
            .spawn()
            .unwrap();
        terminate_by_transport(&mut running, &marker, Duration::from_secs(1));
        assert!(marker.load(Ordering::Acquire));

        let marker = AtomicBool::new(false);
        let mut exited = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        exited.wait().unwrap();
        terminate_by_transport(&mut exited, &marker, Duration::from_secs(1));
        assert!(!marker.load(Ordering::Acquire));
    }

    #[derive(Clone, Default)]
    struct GatedWriter(Arc<(Mutex<GateState>, Condvar)>);

    #[derive(Default)]
    struct GateState {
        bytes: Vec<u8>,
        flushes: usize,
        blocked: bool,
        released: bool,
    }

    impl Write for GatedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let (state, _) = &*self.0;
            state.lock().unwrap().bytes.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let (state, changed) = &*self.0;
            let mut state = state.lock().unwrap();
            state.flushes += 1;
            if state.flushes >= 2 && !state.released {
                state.blocked = true;
                changed.notify_all();
                state = changed.wait_while(state, |state| !state.released).unwrap();
            }
            state.bytes.flush()
        }
    }

    impl GatedWriter {
        fn wait_until_blocked(&self) {
            let (state, changed) = &*self.0;
            let state = state.lock().unwrap();
            let (state, timeout) = changed
                .wait_timeout_while(state, Duration::from_secs(5), |state| !state.blocked)
                .unwrap();
            assert!(
                !timeout.timed_out() && state.blocked,
                "writer never blocked"
            );
        }

        fn release(&self) {
            let (state, changed) = &*self.0;
            state.lock().unwrap().released = true;
            changed.notify_all();
        }
    }

    struct CountingReader {
        cursor: Cursor<Vec<u8>>,
        progress: Arc<(AtomicUsize, Condvar, Mutex<()>)>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.cursor.read(buffer)?;
            self.progress.0.fetch_add(read, Ordering::Relaxed);
            self.progress.1.notify_all();
            Ok(read)
        }
    }

    fn wait_for_read_progress(progress: &Arc<(AtomicUsize, Condvar, Mutex<()>)>, minimum: usize) {
        let guard = progress.2.lock().unwrap();
        let (_guard, timeout) = progress
            .1
            .wait_timeout_while(guard, Duration::from_secs(5), |_| {
                progress.0.load(Ordering::Relaxed) < minimum
            })
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "reader did not reach bounded lookahead"
        );
    }

    #[derive(Default)]
    struct FailAfterFirstFlush {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FailAfterFirstFlush {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.flushes > 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected writer failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn framed(requests: &[Request]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for request in requests {
            encode_frame(&mut bytes, request).unwrap();
        }
        bytes
    }

    fn handshake(version: u32) -> Request {
        Request::Handshake(Handshake {
            protocol_version: version,
            build_id: BUILD_ID.into(),
            worker_generation: 3,
        })
    }

    fn request() -> Request {
        Request::DeriveSnapshot(DeriveSnapshot {
            context: RequestContext {
                request_id: 7,
                worker_generation: 3,
                uri: "file:///example.rs".into(),
                incarnation: 11,
                content_version: 5,
                configuration_generation: 2,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: "/unused/in/unit-test".into(),
            artifact_digest: "sha256:rust-v1".into(),
            text: "fn main() {}".into(),
        })
    }

    #[test]
    fn frame_round_trip_preserves_request() {
        let request = request();
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, &request).unwrap();

        assert_eq!(
            decode_frame(&mut Cursor::new(bytes)).unwrap(),
            Some(request)
        );
    }

    #[test]
    fn oversized_frame_is_rejected_before_payload_read() {
        let bytes = (64_u32 * 1024 * 1024 + 1).to_be_bytes();
        let error = decode_frame::<_, Request>(&mut Cursor::new(bytes)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("frame exceeds"));
    }

    #[test]
    fn derive_snapshot_returns_only_guarded_serializable_data() {
        let Request::DeriveSnapshot(request) = request() else {
            unreachable!()
        };

        let response = derive_snapshot_with_language(request, tree_sitter_rust::LANGUAGE.into());

        let Response::Snapshot(snapshot) = response else {
            panic!("expected snapshot response");
        };
        assert_eq!(snapshot.context.request_id, 7);
        assert_eq!(snapshot.context.content_version, 5);
        assert_eq!(snapshot.language, "rust");
        assert_eq!(snapshot.root_kind, "source_file");
        assert!(!snapshot.has_error);
        assert!(snapshot.named_node_count >= 2);
    }

    #[test]
    fn cursor_named_node_count_visits_each_node_once() {
        fn recursive_count(node: tree_sitter::Node<'_>) -> usize {
            usize::from(node.is_named())
                + (0..node.child_count() as u32)
                    .filter_map(|index| node.child(index))
                    .map(recursive_count)
                    .sum::<usize>()
        }

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser
            .parse(
                "fn main() { let answer = if true { 42 } else { 0 }; }",
                None,
            )
            .unwrap();
        let root = tree.root_node();

        assert_eq!(named_node_count(root), recursive_count(root));
    }

    #[test]
    fn worker_requires_matching_handshake_before_work() {
        let output = SharedWriter::default();
        let captured = output.clone();

        let error = run(
            Cursor::new(framed(&[handshake(PROTOCOL_VERSION + 1)])),
            output,
            2,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let bytes = captured.0.lock().unwrap().clone();
        let response = decode_frame::<_, Response>(&mut Cursor::new(bytes))
            .unwrap()
            .unwrap();
        assert!(matches!(response, Response::Error(_)));
    }

    #[test]
    fn worker_rejects_work_before_handshake() {
        let output = SharedWriter::default();

        let error = run(Cursor::new(framed(&[request()])), output, 2).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("handshake"));
    }

    #[test]
    fn response_router_rejects_unknown_request_id() {
        let routes = Mutex::new(HashMap::new());
        let response = Response::Error(WorkerError {
            context: Some(RequestContext {
                request_id: 99,
                worker_generation: 3,
                uri: "file:///unknown.rs".into(),
                incarnation: 1,
                content_version: 1,
                configuration_generation: 1,
            }),
            message: "unexpected".into(),
        });

        let error = route_response(&routes, response).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unknown request id 99"));
    }

    #[test]
    fn response_router_rejects_stale_context() {
        let expected = RequestContext {
            request_id: 7,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 11,
            content_version: 5,
            configuration_generation: 2,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        let routes = Mutex::new(HashMap::from([(
            7,
            Route {
                expected: expected.clone(),
                sender,
            },
        )]));
        let mut stale = expected;
        stale.content_version += 1;

        let error = route_response(
            &routes,
            Response::Error(WorkerError {
                context: Some(stale),
                message: "stale".into(),
            }),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("context mismatch"));
    }

    #[test]
    fn worker_reports_budget_and_routes_guarded_request_errors() {
        let output = SharedWriter::default();
        let captured = output.clone();

        run(
            Cursor::new(framed(&[handshake(PROTOCOL_VERSION), request()])),
            output,
            2,
        )
        .unwrap();

        let bytes = captured.0.lock().unwrap().clone();
        let mut bytes = Cursor::new(bytes);
        assert_eq!(
            decode_frame::<_, Response>(&mut bytes).unwrap(),
            Some(Response::HandshakeReady(HandshakeReady {
                protocol_version: PROTOCOL_VERSION,
                build_id: BUILD_ID.into(),
                worker_generation: 3,
                compute_threads: 2,
            }))
        );
        let response = decode_frame::<_, Response>(&mut bytes).unwrap().unwrap();
        let Response::Error(error) = response else {
            panic!("missing parser path must return a routed request error");
        };
        assert_eq!(
            error.context.as_ref().map(|context| context.request_id),
            Some(7)
        );
    }

    #[test]
    fn worker_backpressures_until_the_response_writer_resumes() {
        let writer = GatedWriter::default();
        let gate = writer.clone();
        let mut requests = vec![handshake(PROTOCOL_VERSION)];
        for request_id in 1..=12 {
            let Request::DeriveSnapshot(mut request) = request() else {
                unreachable!()
            };
            request.context.request_id = request_id;
            requests.push(Request::DeriveSnapshot(request));
        }
        let admitted_prefix_bytes = framed(&requests[..4]).len();
        let framed_requests = framed(&requests);
        let progress = Arc::new((AtomicUsize::new(0), Condvar::new(), Mutex::new(())));
        let reader = CountingReader {
            cursor: Cursor::new(framed_requests.clone()),
            progress: Arc::clone(&progress),
        };
        let (completed, completed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = completed.send(run(reader, writer, 1));
        });

        gate.wait_until_blocked();
        wait_for_read_progress(&progress, admitted_prefix_bytes);
        assert!(
            progress.0.load(Ordering::Relaxed) <= admitted_prefix_bytes,
            "worker read past two admitted requests plus one bounded lookahead"
        );
        assert!(
            progress.0.load(Ordering::Relaxed) < framed_requests.len(),
            "worker consumed the complete request stream while stdout was stalled"
        );
        assert!(
            completed_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "worker unexpectedly completed while stdout was stalled"
        );

        gate.release();
        completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker did not resume after stdout was released")
            .unwrap();
    }

    #[test]
    fn writer_failure_releases_all_admission_permits() {
        let mut requests = vec![handshake(PROTOCOL_VERSION)];
        for request_id in 1..=8 {
            let Request::DeriveSnapshot(mut request) = request() else {
                unreachable!()
            };
            request.context.request_id = request_id;
            requests.push(Request::DeriveSnapshot(request));
        }
        let (completed, completed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = completed.send(run(
                Cursor::new(framed(&requests)),
                FailAfterFirstFlush::default(),
                1,
            ));
        });

        let error = completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker deadlocked after response writer failure")
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn document_lane_runs_jobs_in_submission_order() {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .build()
                .unwrap(),
        );
        let lane = Arc::new(DocumentLane::detached());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (completed, completed_rx) = std::sync::mpsc::channel();

        let first_gate = Arc::clone(&gate);
        let first_order = Arc::clone(&order);
        lane.submit(
            &pool,
            Box::new(move || {
                let (released, changed) = &*first_gate;
                let released = released.lock().unwrap();
                drop(changed.wait_while(released, |released| !*released).unwrap());
                first_order.lock().unwrap().push(1);
                LaneAction::Retire
            }),
        );
        let second_order = Arc::clone(&order);
        lane.submit(
            &pool,
            Box::new(move || {
                second_order.lock().unwrap().push(2);
                completed.send(()).unwrap();
                LaneAction::Retire
            }),
        );

        assert!(completed_rx.try_recv().is_err());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }

    #[test]
    fn idle_document_lane_removes_its_registry_key() {
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().build().unwrap());
        let registry = Arc::new(LaneRegistry::new());
        let key = DocumentKey {
            uri: "file:///closed.rs".into(),
        };
        let (completed, completed_rx) = std::sync::mpsc::channel();
        submit_document_job(
            &registry,
            key,
            &pool,
            Box::new(move || {
                completed.send(()).unwrap();
                LaneAction::Retire
            }),
        );
        completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !registry.is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(registry.is_empty(), "idle lane retained its URI key");
    }

    #[test]
    fn live_document_lane_stays_registered_between_requests() {
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().build().unwrap());
        let registry = Arc::new(LaneRegistry::new());
        let key = DocumentKey {
            uri: "file:///live.rs".into(),
        };
        let (completed, completed_rx) = std::sync::mpsc::channel();
        submit_document_job(
            &registry,
            key,
            &pool,
            Box::new(move || {
                completed.send(()).unwrap();
                LaneAction::Keep
            }),
        );
        completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while registry
            .iter()
            .any(|entry| entry.value().state.lock().unwrap().running)
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(registry.len(), 1, "live lane was recreated after idle");
    }

    #[test]
    fn document_replica_applies_an_incremental_edit_at_the_expected_base_version() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, sync_ack) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn main() { 1 }".into(),
            },
            &mut parser,
        )
        .unwrap();
        assert!(!sync_ack.incremental);

        let mut edited_context = context;
        edited_context.request_id = 2;
        edited_context.content_version = 2;
        let ack = replica
            .apply(
                ApplyDocumentEdits {
                    context: edited_context.clone(),
                    base_version: 1,
                    edits: vec![ByteEdit {
                        start_byte: 12,
                        old_end_byte: 13,
                        new_text: "value + 2".into(),
                    }],
                },
                &mut parser,
                None,
            )
            .unwrap();

        assert!(ack.incremental);
        assert_eq!(ack.context.content_version, 2);
        let snapshot = replica.derive(edited_context).unwrap();
        assert_eq!(snapshot.root_kind, "source_file");
        assert_eq!(snapshot.root_end_byte, "fn main() { value + 2 }".len());
        assert_eq!(snapshot.parser_cache_hit, None);
        assert!(snapshot.compute_ns > 0);
    }

    #[test]
    fn document_replica_rejects_an_edit_with_a_stale_base_version() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut target = context;
        target.request_id = 2;
        target.content_version = 4;

        let error = replica
            .apply(
                ApplyDocumentEdits {
                    context: target,
                    base_version: 2,
                    edits: Vec::new(),
                },
                &mut parser,
                None,
            )
            .unwrap_err();

        assert!(error.contains("base version 2 does not match 3"));
    }

    #[test]
    fn document_replica_rejects_a_stale_full_sync() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///stale-sync.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn current() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut stale = context;
        stale.request_id = 2;
        stale.content_version = 2;

        assert!(
            replica
                .validate_replacement(&stale, &replica.language, &replica.grammar_key)
                .unwrap_err()
                .contains("stale content version")
        );
    }

    #[test]
    fn document_replica_fences_language_and_grammar_replacement() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///replacement.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn current() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut replacement = context;
        replacement.request_id = 2;
        replacement.content_version = 4;

        assert!(
            replica
                .validate_replacement(&replacement, "python", &replica.grammar_key)
                .unwrap_err()
                .contains("language change")
        );
        let alias_grammar = GrammarKey {
            parser_path: "/unused/other-rust".into(),
            grammar_symbol: "rust".into(),
            artifact_digest: "sha256:rust-v1".into(),
        };
        replica
            .validate_replacement(&replacement, "rust", &alias_grammar)
            .unwrap();

        let different_grammar = GrammarKey {
            parser_path: "/unused/rust".into(),
            grammar_symbol: "rust".into(),
            artifact_digest: "sha256:rust-v2".into(),
        };
        assert!(
            replica
                .validate_replacement(&replacement, "rust", &different_grammar)
                .unwrap_err()
                .contains("grammar change")
        );
    }

    #[test]
    fn document_identity_limit_requests_a_worker_restart() {
        let documents = Arc::new(dashmap::DashMap::new());
        let closed_documents = Arc::new(dashmap::DashMap::new());
        let capacity = Arc::new(Mutex::new(MAX_DOCUMENT_REPLICAS));
        let response = sync_document(
            SyncDocument {
                context: RequestContext {
                    request_id: 1,
                    worker_generation: 3,
                    uri: "file:///over-limit.rs".into(),
                    incarnation: 5_000,
                    content_version: 1,
                    configuration_generation: 0,
                },
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn main() {}".into(),
            },
            &documents,
            &closed_documents,
            &capacity,
            &Arc::new(AtomicUsize::new(0)),
        );

        assert!(matches!(response, Response::WorkerRestartRequired(_)));
    }

    #[test]
    fn poisoned_document_replica_requires_restart_instead_of_reusing_uncertain_state() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///poisoned.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 0,
        };
        let text = "fn main() {}";
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: text.into(),
            },
            &mut parser,
        )
        .unwrap();
        let documents = Arc::new(dashmap::DashMap::new());
        let key = DocumentKey::from(&context);
        let replica = Arc::new(Mutex::new(replica));
        documents.insert(key.clone(), Arc::clone(&replica));
        let poison_target = Arc::clone(&replica);
        let _ = std::thread::spawn(move || {
            let _guard = poison_target.lock().unwrap();
            panic!("poison replica for restart test");
        })
        .join();
        let closed_documents = Arc::new(dashmap::DashMap::new());
        let retained = Arc::new(AtomicUsize::new(text.len()));

        let mut replacement_context = context.clone();
        replacement_context.request_id = 2;
        replacement_context.content_version = 2;
        let sync_response = sync_document(
            SyncDocument {
                context: replacement_context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn replacement() {}".into(),
            },
            &documents,
            &closed_documents,
            &Arc::new(Mutex::new(1)),
            &retained,
        );
        assert!(matches!(sync_response, Response::WorkerRestartRequired(_)));

        let mut close_context = context;
        close_context.request_id = 3;
        close_context.content_version = 2;
        let close_response = close_document(
            CloseDocument {
                context: close_context,
            },
            &documents,
            &closed_documents,
            &retained,
        );
        assert!(matches!(close_response, Response::WorkerRestartRequired(_)));
        assert!(documents.contains_key(&key));
        assert_eq!(retained.load(Ordering::Relaxed), text.len());
    }

    #[test]
    fn document_size_limit_rejects_unbounded_replica_growth() {
        assert!(validate_document_size(MAX_DOCUMENT_BYTES).is_ok());
        assert!(
            validate_document_size(MAX_DOCUMENT_BYTES + 1)
                .unwrap_err()
                .contains("retained size limit")
        );
    }

    #[test]
    fn retained_document_budget_is_transactional() {
        let retained = AtomicUsize::new(MAX_RETAINED_DOCUMENT_BYTES - 8);

        assert!(
            replace_retained_bytes(&retained, 4, 13)
                .unwrap_err()
                .contains("retained document budget")
        );
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES - 8
        );

        replace_retained_bytes(&retained, 4, 12).unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES
        );
    }

    #[test]
    fn retained_document_shrink_does_not_release_budget_before_commit() {
        let retained = AtomicUsize::new(MAX_RETAINED_DOCUMENT_BYTES);

        assert!(!reserve_retained_growth(&retained, 12, 4).unwrap());
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES
        );

        replace_retained_bytes(&retained, 12, 4).unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES - 8
        );
    }

    #[test]
    fn retained_document_growth_reservation_can_always_roll_back() {
        let retained = AtomicUsize::new(MAX_RETAINED_DOCUMENT_BYTES - 8);

        assert!(reserve_retained_growth(&retained, 4, 12).unwrap());
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES
        );

        replace_retained_bytes(&retained, 12, 4).unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES - 8
        );
    }

    #[test]
    fn document_replica_keeps_the_previous_version_when_an_edit_batch_is_invalid() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///transaction.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 0,
        };
        let original = "fn main() { 1 }";
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: original.into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut target = context.clone();
        target.request_id = 2;
        target.content_version = 2;

        replica
            .apply(
                ApplyDocumentEdits {
                    context: target,
                    base_version: 1,
                    edits: vec![
                        ByteEdit {
                            start_byte: 12,
                            old_end_byte: 13,
                            new_text: "2".into(),
                        },
                        ByteEdit {
                            start_byte: 999,
                            old_end_byte: 999,
                            new_text: "invalid".into(),
                        },
                    ],
                },
                &mut parser,
                None,
            )
            .unwrap_err();

        let mut derive_context = context;
        derive_context.request_id = 3;
        let snapshot = replica.derive(derive_context).unwrap();
        assert_eq!(snapshot.root_end_byte, original.len());
        assert_eq!(replica.text, original);
    }

    #[test]
    fn document_replica_applies_edit_ranges_to_each_intermediate_text() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///sequential-edits.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 0,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                text: "fn main() { 1 }".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut target = context;
        target.request_id = 2;
        target.content_version = 2;

        replica
            .apply(
                ApplyDocumentEdits {
                    context: target,
                    base_version: 1,
                    edits: vec![
                        ByteEdit {
                            start_byte: 12,
                            old_end_byte: 12,
                            new_text: "value + ".into(),
                        },
                        ByteEdit {
                            start_byte: 20,
                            old_end_byte: 21,
                            new_text: "2".into(),
                        },
                    ],
                },
                &mut parser,
                None,
            )
            .unwrap();

        assert_eq!(replica.text, "fn main() { value + 2 }");
    }
}
