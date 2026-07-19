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
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
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
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Handshake(Handshake),
    DeriveSnapshot(DeriveSnapshot),
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
    pub parser_cache_hit: bool,
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
    Snapshot(DerivedSnapshot),
    Error(WorkerError),
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

fn named_node_count(root: tree_sitter::Node<'_>) -> usize {
    let mut count = 0;
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        count += usize::from(node.is_named());
        pending.extend((0..node.child_count() as u32).filter_map(|index| node.child(index)));
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
        parser_cache_hit,
        queue_wait_ns: duration_ns(queue_wait),
        compute_ns: duration_ns(started.elapsed()),
    })
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct GrammarKey {
    parser_path: PathBuf,
    grammar_symbol: String,
}

struct WorkerThreadState {
    loader: crate::language::loader::ParserLoader,
    languages: HashMap<GrammarKey, tree_sitter::Language>,
    parsers: HashMap<GrammarKey, tree_sitter::Parser>,
}

pub struct LocalDeriver {
    state: WorkerThreadState,
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
        let key = GrammarKey {
            parser_path: request
                .parser_path
                .canonicalize()
                .unwrap_or_else(|_| request.parser_path.clone()),
            grammar_symbol: request.grammar_symbol.clone(),
        };
        let language = match self.languages.get(&key).cloned() {
            Some(language) => language,
            None => match self
                .loader
                .load_language(&request.parser_path, &request.grammar_symbol)
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
        let response =
            derive_snapshot_with_parser(request, &mut parser, parser_cache_hit, queue_wait);
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
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(compute_threads)
        .thread_name(|index| format!("kakehashi-tree-worker-{index}"))
        .build()
        .map_err(io::Error::other)?;
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
        Some(Request::DeriveSnapshot(_)) => {
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

    while let Some(request) = decode_frame::<_, Request>(&mut reader)? {
        let Request::DeriveSnapshot(request) = request else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake may only be sent once",
            ));
        };
        if request.context.worker_generation != worker_generation {
            responses
                .send((
                    Response::Error(WorkerError {
                        context: Some(request.context),
                        message: "stale worker generation".into(),
                    }),
                    None,
                ))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
            continue;
        }
        let enqueued = Instant::now();
        permit_rx
            .recv()
            .map_err(|_| io::Error::other("worker admission queue stopped"))?;
        let responses = responses.clone();
        let permit = AdmissionPermit(permits.clone());
        pool.spawn(move || {
            let _ = responses.send((derive_snapshot(request, enqueued.elapsed()), Some(permit)));
        });
    }
    for _ in 0..max_inflight {
        permit_rx
            .recv()
            .map_err(|_| io::Error::other("worker admission queue stopped"))?;
    }
    drop(responses);
    join_writer(writer_thread)
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
        let routes = Arc::new(Mutex::new(HashMap::<u64, Route>::new()));
        let reader_routes = Arc::clone(&routes);
        let reader_child = Arc::clone(&child);
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
                terminate(&mut child, Duration::from_secs(1));
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
        let writer = std::thread::spawn(move || {
            for request in outbound_rx {
                if let Err(error) = encode_frame(&mut stdin, &request) {
                    let kind = error.kind();
                    let message = error.to_string();
                    fail_routes(None, &writer_routes, kind, &message);
                    if let Ok(mut child) = writer_child.lock() {
                        terminate(&mut child, Duration::from_secs(1));
                    }
                    break;
                }
            }
        });
        Ok(Self {
            child,
            outbound: Mutex::new(Some(outbound)),
            routes,
            reader: Some(reader),
            writer: Some(writer),
            ready,
            max_inflight,
        })
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
        let started = Instant::now();
        if request.context.worker_generation != self.ready.worker_generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request targets a stale worker generation",
            ));
        }
        let request_id = request.context.request_id;
        let expected = request.context.clone();
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
            if let Err(error) = outbound.try_send(Request::DeriveSnapshot(request)) {
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
        Response::Snapshot(snapshot) => Some(snapshot.context.request_id),
        Response::Error(error) => error.context.as_ref().map(|context| context.request_id),
        Response::HandshakeReady(_) => None,
    }
}

fn response_context(response: &Response) -> Option<&RequestContext> {
    match response {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::{
        BUILD_ID, DeriveSnapshot, Handshake, HandshakeReady, PROTOCOL_VERSION, Request,
        RequestContext, Response, Route, WorkerError, decode_frame, derive_snapshot_with_language,
        encode_frame, route_response, run,
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
}
