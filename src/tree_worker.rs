//! Development-only process boundary for the tree-worker prototype.
//!
//! The protocol deliberately returns owned, serializable tree-derived data.
//! Tree-sitter pointer-bearing values never cross this module's transport.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
    })
}

fn derive_snapshot(request: DeriveSnapshot) -> Response {
    let request_id = request.context.request_id;
    let mut loader = crate::language::loader::ParserLoader::new();
    match loader.load_language(&request.parser_path, &request.grammar_symbol) {
        Ok(language) => derive_snapshot_with_language(request, language),
        Err(error) => Response::Error(WorkerError {
            request_id: Some(request_id),
            message: error.to_string(),
        }),
    }
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
        pool.spawn(move || {
            let _ = responses.send(derive_snapshot(request));
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
    child: Child,
    stdin: Option<ChildStdin>,
    responses: mpsc::Receiver<io::Result<Response>>,
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
        let (responses_tx, responses) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            loop {
                match decode_frame::<_, Response>(&mut stdout) {
                    Ok(Some(response)) => {
                        if responses_tx.send(Ok(response)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = responses_tx.send(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "tree worker closed its response stream",
                        )));
                        break;
                    }
                    Err(error) => {
                        let _ = responses_tx.send(Err(error));
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
        let ready = match responses.recv_timeout(Duration::from_secs(5)) {
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
            child,
            stdin: Some(stdin),
            responses,
            reader: Some(reader),
            ready,
        })
    }

    pub fn compute_threads(&self) -> usize {
        self.ready.compute_threads
    }

    pub fn derive(&mut self, request: DeriveSnapshot) -> io::Result<Response> {
        self.derive_with_timeout(request, Duration::from_secs(60))
    }

    pub fn derive_with_timeout(
        &mut self,
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
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker is shut down"))?;
        encode_frame(stdin, &Request::DeriveSnapshot(request))?;
        let response = match self.responses.recv_timeout(timeout) {
            Ok(response) => response?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                terminate(&mut self.child, Duration::from_secs(1));
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

    pub fn shutdown(mut self) -> io::Result<()> {
        self.stdin.take();
        let status = wait_until(&mut self.child, Duration::from_secs(2))?;
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
        self.stdin.take();
        terminate(&mut self.child, Duration::from_secs(1));
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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
