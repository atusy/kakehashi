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
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

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
    pub request_id: Option<u64>,
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
            request_id: Some(request.context.request_id),
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
            request_id: Some(request.context.request_id),
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
        let request_id = request.context.request_id;
        let key = GrammarKey {
            parser_path: request.parser_path.clone(),
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
                        request_id: Some(request_id),
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
                    request_id: Some(request_id),
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

pub fn run<R, W>(mut reader: R, writer: W, compute_threads: usize) -> io::Result<()>
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
    let (responses, response_rx) = mpsc::channel::<Response>();
    let writer_thread = std::thread::spawn(move || -> io::Result<()> {
        let mut writer = writer;
        for response in response_rx {
            encode_frame(&mut writer, &response)?;
        }
        Ok(())
    });

    let Some(Request::Handshake(handshake)) = decode_frame::<_, Request>(&mut reader)? else {
        drop(responses);
        return join_writer(writer_thread);
    };
    if handshake.protocol_version != PROTOCOL_VERSION || handshake.build_id != BUILD_ID {
        responses
            .send(Response::Error(WorkerError {
                request_id: None,
                message: "worker protocol/build identity mismatch".into(),
            }))
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
        .send(Response::HandshakeReady(HandshakeReady {
            protocol_version: PROTOCOL_VERSION,
            build_id: BUILD_ID.into(),
            worker_generation,
            compute_threads,
        }))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;

    let (completed, completed_rx) = mpsc::channel();
    let mut pending = 0_usize;
    while let Some(request) = decode_frame::<_, Request>(&mut reader)? {
        let Request::DeriveSnapshot(request) = request else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake may only be sent once",
            ));
        };
        if request.context.worker_generation != worker_generation {
            responses
                .send(Response::Error(WorkerError {
                    request_id: Some(request.context.request_id),
                    message: "stale worker generation".into(),
                }))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
            continue;
        }
        pending += 1;
        let responses = responses.clone();
        let completed = completed.clone();
        let enqueued = Instant::now();
        pool.spawn(move || {
            let _ = responses.send(derive_snapshot(request, enqueued.elapsed()));
            let _ = completed.send(());
        });
    }
    drop(completed);
    for _ in 0..pending {
        completed_rx.recv().map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "worker compute task stopped")
        })?;
    }
    drop(responses);
    join_writer(writer_thread)
}

fn join_writer(thread: std::thread::JoinHandle<io::Result<()>>) -> io::Result<()> {
    thread
        .join()
        .map_err(|_| io::Error::other("worker writer panicked"))?
}

pub fn run_stdio(compute_threads: usize) -> io::Result<()> {
    run(std::io::stdin(), std::io::stdout(), compute_threads)
}

pub struct Client {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    routes: Arc<Mutex<HashMap<u64, mpsc::Sender<io::Result<Response>>>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    ready: HandshakeReady,
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
        let routes = Arc::new(Mutex::new(
            HashMap::<u64, mpsc::Sender<io::Result<Response>>>::new(),
        ));
        let reader_routes = Arc::clone(&routes);
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
                        route_response(&reader_routes, response);
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
        });
        encode_frame(
            &mut stdin,
            &Request::Handshake(Handshake {
                protocol_version: PROTOCOL_VERSION,
                build_id: BUILD_ID.into(),
                worker_generation,
            }),
        )?;
        let ready = match handshake_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(Response::HandshakeReady(ready)))
                if ready.protocol_version == PROTOCOL_VERSION
                    && ready.build_id == BUILD_ID
                    && ready.worker_generation == worker_generation
                    && ready.compute_threads == compute_threads =>
            {
                ready
            }
            Ok(Ok(response)) => {
                terminate(&mut child, Duration::from_secs(1));
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid worker handshake response: {response:?}"),
                ));
            }
            Ok(Err(error)) => {
                terminate(&mut child, Duration::from_secs(1));
                return Err(error);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                terminate(&mut child, Duration::from_secs(1));
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "tree worker handshake timed out",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminate(&mut child, Duration::from_secs(1));
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tree worker handshake channel closed",
                ));
            }
        };
        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            routes,
            reader: Some(reader),
            ready,
        })
    }

    pub fn compute_threads(&self) -> usize {
        self.ready.compute_threads
    }

    pub fn derive(&self, request: DeriveSnapshot) -> io::Result<Response> {
        self.derive_with_timeout(request, Duration::from_secs(60))
    }

    pub fn derive_with_timeout(
        &self,
        request: DeriveSnapshot,
        timeout: Duration,
    ) -> io::Result<Response> {
        if request.context.worker_generation != self.ready.worker_generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request targets a stale worker generation",
            ));
        }
        let request_id = request.context.request_id;
        let (response_tx, response_rx) = mpsc::channel();
        {
            let mut routes = self
                .routes
                .lock()
                .map_err(|_| io::Error::other("worker response router is poisoned"))?;
            match routes.entry(request_id) {
                Entry::Vacant(route) => {
                    route.insert(response_tx);
                }
                Entry::Occupied(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("tree worker request {request_id} is already active"),
                    ));
                }
            }
        }
        {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| io::Error::other("worker stdin lock is poisoned"))?;
            let stdin = stdin
                .as_mut()
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker is shut down"))?;
            if let Err(error) = encode_frame(stdin, &Request::DeriveSnapshot(request)) {
                self.remove_route(request_id);
                return Err(error);
            }
        }
        let response = match response_rx.recv_timeout(timeout) {
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
        let response_id = match &response {
            Response::Snapshot(snapshot) => Some(snapshot.context.request_id),
            Response::Error(error) => error.request_id,
            Response::HandshakeReady(_) => None,
        };
        if response_id != Some(request_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected response for request {response_id:?}"),
            ));
        }
        Ok(response)
    }

    fn remove_route(&self, request_id: u64) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(&request_id);
        }
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.stdin
            .get_mut()
            .map_err(|_| io::Error::other("worker stdin lock is poisoned"))?
            .take();
        let status = wait_until(
            self.child
                .get_mut()
                .map_err(|_| io::Error::other("worker child lock is poisoned"))?,
            Duration::from_secs(2),
        )?;
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

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(stdin) = self.stdin.get_mut() {
            stdin.take();
        }
        if let Ok(child) = self.child.get_mut() {
            terminate(child, Duration::from_secs(1));
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn response_request_id(response: &Response) -> Option<u64> {
    match response {
        Response::Snapshot(snapshot) => Some(snapshot.context.request_id),
        Response::Error(error) => error.request_id,
        Response::HandshakeReady(_) => None,
    }
}

fn route_response(
    routes: &Mutex<HashMap<u64, mpsc::Sender<io::Result<Response>>>>,
    response: Response,
) {
    let Some(request_id) = response_request_id(&response) else {
        fail_routes(
            None,
            routes,
            io::ErrorKind::InvalidData,
            "unexpected response without request id",
        );
        return;
    };
    let route = routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request_id);
    if let Some(route) = route {
        let _ = route.send(Ok(response));
    }
}

fn fail_routes(
    handshake: Option<mpsc::Sender<io::Result<Response>>>,
    routes: &Mutex<HashMap<u64, mpsc::Sender<io::Result<Response>>>>,
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
        let _ = route.send(Err(io::Error::new(kind, message.to_string())));
    }
}

fn wait_until(child: &mut Child, timeout: Duration) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            return child.wait();
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
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    use super::{
        BUILD_ID, DeriveSnapshot, Handshake, HandshakeReady, PROTOCOL_VERSION, Request,
        RequestContext, Response, decode_frame, derive_snapshot_with_language, encode_frame, run,
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
        assert_eq!(error.request_id, Some(7));
    }
}
