use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;
use tower::Service;
use tower_lsp_server::jsonrpc::{Id, Request, Response};

use crate::error::LockResultExt;

const HEADER_LIMIT: usize = 8 * 1024;
const BODY_PREFIX_LIMIT: usize = 512;
const BODY_TAIL_LIMIT: usize = 256;

#[derive(Clone, Debug)]
struct ReadyEvent {
    sequence: u64,
    method: String,
    at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct FrameMetric {
    ready_sequence: Option<u64>,
    method: Option<String>,
    id: Option<Id>,
    body_bytes: usize,
    frame_bytes: usize,
    ready_us: Option<u64>,
    write_start_us: u64,
    last_byte_us: u64,
    flush_complete_us: Option<u64>,
}

#[derive(Default)]
struct MetricsState {
    next_sequence: u64,
    response_ready: HashMap<Id, VecDeque<ReadyEvent>>,
    frames: Vec<FrameMetric>,
}

#[doc(hidden)]
pub struct StdoutMetrics {
    origin: Instant,
    state: Mutex<MetricsState>,
}

impl StdoutMetrics {
    pub fn new(origin: Instant) -> Self {
        Self {
            origin,
            state: Mutex::new(MetricsState::default()),
        }
    }

    fn record_response_ready(&self, id: Id, method: String, at: Instant) {
        let mut state = self
            .state
            .lock()
            .recover_poison("StdoutMetrics::record_response_ready");
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state
            .response_ready
            .entry(id)
            .or_default()
            .push_back(ReadyEvent {
                sequence,
                method,
                at,
            });
    }

    #[cfg(test)]
    fn frames(&self) -> Vec<FrameMetric> {
        self.state
            .lock()
            .expect("stdout metrics lock poisoned")
            .frames
            .clone()
    }

    fn finish_frame(&self, mut frame: FrameMetric, id: Option<Id>) {
        let mut state = self
            .state
            .lock()
            .recover_poison("StdoutMetrics::finish_frame");
        if frame.method.is_none()
            && let Some(id) = id.as_ref()
            && let Some(queue) = state.response_ready.get_mut(id)
            && let Some(ready) = queue.pop_front()
        {
            frame.ready_sequence = Some(ready.sequence);
            frame.method = Some(ready.method);
            frame.ready_us = Some(self.timestamp_us(ready.at));
            if queue.is_empty() {
                state.response_ready.remove(id);
            }
        }
        state.frames.push(frame);
    }

    fn timestamp_us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.origin).as_micros() as u64
    }

    pub fn write_jsonl(&self, mut writer: impl io::Write) -> io::Result<()> {
        let state = self
            .state
            .lock()
            .recover_poison("StdoutMetrics::write_jsonl");
        for frame in &state.frames {
            serde_json::to_writer(
                &mut writer,
                &serde_json::json!({"event": "stdout_frame", "frame": frame}),
            )?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }
}

struct FrameObserver {
    metrics: Arc<StdoutMetrics>,
    decode: DecodeState,
    awaiting_flush: Vec<(FrameMetric, Option<Id>)>,
}

#[doc(hidden)]
pub struct MeasuredStdout<W> {
    inner: W,
    observer: FrameObserver,
}

#[doc(hidden)]
pub struct ResponseReadyService<S> {
    inner: S,
    metrics: Arc<StdoutMetrics>,
}

impl<S> ResponseReadyService<S> {
    pub fn new(inner: S, metrics: Arc<StdoutMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl<S> Service<Request> for ResponseReadyService<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let method = request.method().to_string();
        let future = self.inner.call(request);
        let metrics = Arc::clone(&self.metrics);
        Box::pin(async move {
            let response = future.await?;
            if let Some(response) = response.as_ref() {
                metrics.record_response_ready(response.id().clone(), method, Instant::now());
            }
            Ok(response)
        })
    }
}

impl<W> MeasuredStdout<W> {
    pub fn new(inner: W, metrics: Arc<StdoutMetrics>) -> Self {
        Self {
            inner,
            observer: FrameObserver::new(metrics),
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for MeasuredStdout<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, bytes);
        if let Poll::Ready(Ok(accepted)) = result
            && accepted > 0
        {
            self.observer.accepted(&bytes[..accepted], Instant::now());
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.observer.flushed(Instant::now());
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.observer.flushed(Instant::now());
        }
        result
    }
}

enum DecodeState {
    Header {
        bytes: Vec<u8>,
        write_start: Option<Instant>,
    },
    Body {
        header_bytes: usize,
        content_length: usize,
        seen: usize,
        prefix: Vec<u8>,
        tail: VecDeque<u8>,
        write_start: Instant,
    },
    Disabled,
}

impl FrameObserver {
    fn new(metrics: Arc<StdoutMetrics>) -> Self {
        Self {
            metrics,
            decode: DecodeState::Header {
                bytes: Vec::new(),
                write_start: None,
            },
            awaiting_flush: Vec::new(),
        }
    }

    fn accepted(&mut self, mut bytes: &[u8], at: Instant) {
        while !bytes.is_empty() {
            match &mut self.decode {
                DecodeState::Header {
                    bytes: header,
                    write_start,
                } => {
                    write_start.get_or_insert(at);
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    header.push(byte);
                    if header.len() > HEADER_LIMIT {
                        self.decode = DecodeState::Disabled;
                        return;
                    }
                    if header.ends_with(b"\r\n\r\n") {
                        let Some(content_length) = content_length(header) else {
                            self.decode = DecodeState::Disabled;
                            return;
                        };
                        self.decode = DecodeState::Body {
                            header_bytes: header.len(),
                            content_length,
                            seen: 0,
                            prefix: Vec::with_capacity(content_length.min(BODY_PREFIX_LIMIT)),
                            tail: VecDeque::with_capacity(content_length.min(BODY_TAIL_LIMIT)),
                            write_start: write_start.expect("header start is set before bytes"),
                        };
                    }
                }
                DecodeState::Body {
                    header_bytes,
                    content_length,
                    seen,
                    prefix,
                    tail,
                    write_start,
                } => {
                    let take = (*content_length - *seen).min(bytes.len());
                    let accepted = &bytes[..take];
                    bytes = &bytes[take..];
                    if prefix.len() < BODY_PREFIX_LIMIT {
                        let prefix_take = (BODY_PREFIX_LIMIT - prefix.len()).min(accepted.len());
                        prefix.extend_from_slice(&accepted[..prefix_take]);
                    }
                    for byte in accepted {
                        if tail.len() == BODY_TAIL_LIMIT {
                            tail.pop_front();
                        }
                        tail.push_back(*byte);
                    }
                    *seen += take;
                    if *seen == *content_length {
                        let tail: Vec<u8> = tail.iter().copied().collect();
                        let id = response_id(&tail);
                        let method = method_name(prefix);
                        self.awaiting_flush.push((
                            FrameMetric {
                                ready_sequence: None,
                                method,
                                id: id.clone(),
                                body_bytes: *content_length,
                                frame_bytes: *header_bytes + *content_length,
                                ready_us: None,
                                write_start_us: self.metrics.timestamp_us(*write_start),
                                last_byte_us: self.metrics.timestamp_us(at),
                                flush_complete_us: None,
                            },
                            id,
                        ));
                        self.decode = DecodeState::Header {
                            bytes: Vec::new(),
                            write_start: None,
                        };
                    }
                }
                DecodeState::Disabled => return,
            }
        }
    }

    fn flushed(&mut self, at: Instant) {
        let flush_complete_us = self.metrics.timestamp_us(at);
        for (mut frame, id) in self.awaiting_flush.drain(..) {
            frame.flush_complete_us = Some(flush_complete_us);
            self.metrics.finish_frame(frame, id);
        }
    }
}

impl Drop for FrameObserver {
    fn drop(&mut self) {
        for (frame, id) in self.awaiting_flush.drain(..) {
            self.metrics.finish_frame(frame, id);
        }
    }
}

fn content_length(header: &[u8]) -> Option<usize> {
    std::str::from_utf8(header).ok()?.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

fn response_id(tail: &[u8]) -> Option<Id> {
    let marker = b",\"id\":";
    let start = tail
        .windows(marker.len())
        .rposition(|window| window == marker)?
        + marker.len();
    let mut deserializer = serde_json::Deserializer::from_slice(&tail[start..]);
    Id::deserialize(&mut deserializer).ok()
}

fn method_name(prefix: &[u8]) -> Option<String> {
    let start = top_level_value_start(prefix, b"method")?;
    let mut deserializer = serde_json::Deserializer::from_slice(&prefix[start..]);
    String::deserialize(&mut deserializer).ok()
}

fn top_level_value_start(json: &[u8], wanted_key: &[u8]) -> Option<usize> {
    let mut depth = 0usize;
    let mut string_start = None;
    let mut escaped = false;

    for (index, byte) in json.iter().copied().enumerate() {
        if let Some(start) = string_start {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string_start = None;
                if depth == 1 && json.get(start..index) == Some(wanted_key) {
                    let colon = json[index + 1..]
                        .iter()
                        .position(|byte| !byte.is_ascii_whitespace())?
                        + index
                        + 1;
                    if json.get(colon) == Some(&b':') {
                        return Some(colon + 1);
                    }
                }
            }
            continue;
        }

        match byte {
            b'"' => string_start = Some(index + 1),
            b'{' | b'[' => depth = depth.saturating_add(1),
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::future::ready;
    use std::time::Duration;
    use tower_lsp_server::jsonrpc::{Request, Response};

    #[derive(Clone)]
    struct EchoService;

    impl tower::Service<Request> for EchoService {
        type Response = Option<Response>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: Request) -> Self::Future {
            ready(Ok(request
                .id()
                .cloned()
                .map(|id| Response::from_ok(id, serde_json::Value::Null))))
        }
    }

    #[tokio::test]
    async fn service_records_a_completed_response_with_its_request_method() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        let mut service = ResponseReadyService::new(EchoService, Arc::clone(&metrics));
        let request = Request::build("textDocument/semanticTokens/full")
            .id(7i64)
            .finish();

        let response = tower::Service::call(&mut service, request)
            .await
            .unwrap()
            .unwrap();
        metrics.finish_frame(
            FrameMetric {
                ready_sequence: None,
                method: None,
                id: Some(response.id().clone()),
                body_bytes: 0,
                frame_bytes: 0,
                ready_us: None,
                write_start_us: 0,
                last_byte_us: 0,
                flush_complete_us: None,
            },
            Some(response.id().clone()),
        );

        let frame = &metrics.frames()[0];
        assert_eq!(frame.ready_sequence, Some(0));
        assert_eq!(
            frame.method.as_deref(),
            Some("textDocument/semanticTokens/full")
        );
        assert!(frame.ready_us.is_some());
    }

    #[test]
    fn split_response_is_reported_only_after_flush() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        metrics.record_response_ready(
            Id::Number(7),
            "textDocument/semanticTokens/full".to_string(),
            origin + Duration::from_micros(10),
        );
        let mut observer = FrameObserver::new(Arc::clone(&metrics));
        let body = br#"{"jsonrpc":"2.0","id":7,"result":null}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();

        observer.accepted(&frame[..9], origin + Duration::from_micros(20));
        observer.accepted(&frame[9..], origin + Duration::from_micros(30));
        assert!(
            metrics.frames().is_empty(),
            "accepted bytes are not flushed"
        );

        observer.flushed(origin + Duration::from_micros(40));

        assert_eq!(
            metrics.frames(),
            vec![FrameMetric {
                ready_sequence: Some(0),
                method: Some("textDocument/semanticTokens/full".to_string()),
                id: Some(Id::Number(7)),
                body_bytes: body.len(),
                frame_bytes: frame.len(),
                ready_us: Some(10),
                write_start_us: 20,
                last_byte_us: 30,
                flush_complete_us: Some(40),
            }]
        );
    }

    #[test]
    fn nested_method_field_does_not_hide_response_attribution() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        metrics.record_response_ready(
            Id::Number(7),
            "textDocument/semanticTokens/full".to_string(),
            origin + Duration::from_micros(10),
        );
        let mut observer = FrameObserver::new(Arc::clone(&metrics));
        let body = br#"{"jsonrpc":"2.0","result":{"method":"nested"},"id":7}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();

        observer.accepted(&frame, origin + Duration::from_micros(20));
        observer.flushed(origin + Duration::from_micros(30));

        assert_eq!(
            metrics.frames()[0].method.as_deref(),
            Some("textDocument/semanticTokens/full")
        );
    }

    #[tokio::test]
    async fn async_writer_reports_the_bytes_accepted_by_its_inner_writer() {
        use tokio::io::AsyncWriteExt;

        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        metrics.record_response_ready(
            Id::String("semantic-7".to_string()),
            "textDocument/semanticTokens/full".to_string(),
            origin,
        );
        let body = br#"{"jsonrpc":"2.0","result":null,"id":"semantic-7"}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let mock = tokio_test::io::Builder::new().write(&frame).build();
        let mut stdout = MeasuredStdout::new(mock, Arc::clone(&metrics));

        stdout.write_all(&frame).await.unwrap();
        assert!(
            metrics.frames().is_empty(),
            "write acceptance is not a flush"
        );
        stdout.flush().await.unwrap();

        let frames = metrics.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].id, Some(Id::String("semantic-7".to_string())));
        assert_eq!(frames[0].body_bytes, body.len());
        assert_eq!(frames[0].frame_bytes, frame.len());
        assert!(frames[0].flush_complete_us.unwrap() >= frames[0].last_byte_us);
    }

    #[test]
    fn completed_frame_without_flush_is_reported_as_censored_on_drop() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        let body = br#"{"jsonrpc":"2.0","method":"window/logMessage"}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();

        {
            let mut observer = FrameObserver::new(Arc::clone(&metrics));
            observer.accepted(&frame, origin + Duration::from_micros(10));
        }

        let frames = metrics.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flush_complete_us, None);
    }
}
