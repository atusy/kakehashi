use std::collections::{HashMap, VecDeque};
use std::fmt;
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
const BODY_TAIL_LIMIT: usize = 1024;

#[derive(Clone, Debug)]
struct ReadyEvent {
    sequence: u64,
    method: String,
    at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct FrameMetric {
    ready_sequence: Option<u64>,
    write_sequence: u64,
    method: Option<String>,
    id: Option<Id>,
    body_bytes: usize,
    frame_bytes: usize,
    ready_us: Option<u64>,
    write_start_us: u64,
    last_byte_us: u64,
    flush_complete_us: Option<u64>,
    id_unattributed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PartialFrameMetric {
    phase: &'static str,
    expected_frame_bytes: Option<usize>,
    accepted_frame_bytes: usize,
    write_start_us: u64,
    last_byte_us: u64,
}

#[derive(Clone, Copy)]
struct WriteAttempt {
    at: Instant,
    sequence: u64,
}

#[derive(Default)]
struct MetricsState {
    next_sequence: u64,
    response_ready: HashMap<Id, VecDeque<ReadyEvent>>,
    frames: Vec<FrameMetric>,
    partial_frames: Vec<PartialFrameMetric>,
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

    #[cfg(test)]
    fn partial_frames(&self) -> Vec<PartialFrameMetric> {
        self.state
            .lock()
            .expect("stdout metrics lock poisoned")
            .partial_frames
            .clone()
    }

    #[cfg(test)]
    fn ready_count(&self) -> usize {
        self.state
            .lock()
            .expect("stdout metrics lock poisoned")
            .response_ready
            .values()
            .map(VecDeque::len)
            .sum()
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

    fn next_event_sequence(&self) -> u64 {
        let mut state = self
            .state
            .lock()
            .recover_poison("StdoutMetrics::next_event_sequence");
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        sequence
    }

    fn record_partial_frame(&self, frame: PartialFrameMetric) {
        self.state
            .lock()
            .recover_poison("StdoutMetrics::record_partial_frame")
            .partial_frames
            .push(frame);
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
        for frame in &state.partial_frames {
            serde_json::to_writer(
                &mut writer,
                &serde_json::json!({"event": "stdout_partial_frame", "frame": frame}),
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
    poll_write_attempt: Option<WriteAttempt>,
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

#[doc(hidden)]
#[derive(Debug)]
pub enum ResponseReadyError<E> {
    Inner(E),
    Task(tokio::task::JoinError),
}

impl<E: fmt::Display> fmt::Display for ResponseReadyError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inner(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ResponseReadyError<E> {}

impl<S> ResponseReadyService<S> {
    pub fn new(inner: S, metrics: Arc<StdoutMetrics>) -> Self {
        Self { inner, metrics }
    }
}

impl<S> Service<Request> for ResponseReadyService<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Send + 'static,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    type Response = S::Response;
    type Error = ResponseReadyError<S::Error>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(ResponseReadyError::Inner)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let method = request.method().to_string();
        let future = self.inner.call(request);
        let metrics = Arc::clone(&self.metrics);
        Box::pin(async move {
            let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                let response = future.await.map_err(ResponseReadyError::Inner)?;
                if let Some(response) = response.as_ref() {
                    metrics.record_response_ready(response.id().clone(), method, Instant::now());
                }
                Ok(response)
            }));
            task.await.map_err(ResponseReadyError::Task)?
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
        if !bytes.is_empty() {
            self.observer.write_attempt(Instant::now());
        }
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
        write_start: Option<WriteAttempt>,
        last_byte: Option<Instant>,
    },
    Body {
        header_bytes: usize,
        content_length: usize,
        seen: usize,
        prefix: Vec<u8>,
        tail: Vec<u8>,
        write_start: WriteAttempt,
        last_byte: Instant,
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
                last_byte: None,
            },
            awaiting_flush: Vec::new(),
            poll_write_attempt: None,
        }
    }

    fn write_attempt(&mut self, at: Instant) {
        let attempt = WriteAttempt {
            at,
            sequence: self.metrics.next_event_sequence(),
        };
        self.poll_write_attempt = Some(attempt);
        if let DecodeState::Header { write_start, .. } = &mut self.decode {
            write_start.get_or_insert(attempt);
        }
    }

    fn accepted(&mut self, mut bytes: &[u8], at: Instant) {
        let poll_write_attempt = self
            .poll_write_attempt
            .take()
            .unwrap_or_else(|| WriteAttempt {
                at,
                sequence: self.metrics.next_event_sequence(),
            });
        while !bytes.is_empty() {
            match &mut self.decode {
                DecodeState::Header {
                    bytes: header,
                    write_start,
                    last_byte,
                } => {
                    write_start.get_or_insert(poll_write_attempt);
                    *last_byte = Some(at);
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
                        let Some(write_start) = *write_start else {
                            self.decode = DecodeState::Disabled;
                            return;
                        };
                        if content_length == 0 {
                            self.decode = DecodeState::Disabled;
                            return;
                        }
                        self.decode = DecodeState::Body {
                            header_bytes: header.len(),
                            content_length,
                            seen: 0,
                            prefix: Vec::with_capacity(content_length.min(BODY_PREFIX_LIMIT)),
                            tail: Vec::with_capacity(content_length.min(BODY_TAIL_LIMIT)),
                            write_start,
                            last_byte: at,
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
                    last_byte,
                } => {
                    let take = (*content_length - *seen).min(bytes.len());
                    let accepted = &bytes[..take];
                    bytes = &bytes[take..];
                    if prefix.len() < BODY_PREFIX_LIMIT {
                        let prefix_take = (BODY_PREFIX_LIMIT - prefix.len()).min(accepted.len());
                        prefix.extend_from_slice(&accepted[..prefix_take]);
                    }
                    retain_tail(tail, accepted);
                    *seen += take;
                    *last_byte = at;
                    if *seen == *content_length {
                        let id = response_id(tail);
                        let method = method_name(prefix);
                        let id_unattributed = method.is_none() && id.is_none();
                        self.awaiting_flush.push((
                            FrameMetric {
                                ready_sequence: None,
                                write_sequence: write_start.sequence,
                                method,
                                id: id.clone(),
                                body_bytes: *content_length,
                                frame_bytes: *header_bytes + *content_length,
                                ready_us: None,
                                write_start_us: self.metrics.timestamp_us(write_start.at),
                                last_byte_us: self.metrics.timestamp_us(at),
                                flush_complete_us: None,
                                id_unattributed,
                            },
                            id,
                        ));
                        self.decode = DecodeState::Header {
                            bytes: Vec::new(),
                            write_start: (!bytes.is_empty()).then_some(poll_write_attempt),
                            last_byte: None,
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
        let partial = match &self.decode {
            DecodeState::Header {
                bytes,
                write_start: Some(write_start),
                last_byte,
            } => Some(PartialFrameMetric {
                phase: "header",
                expected_frame_bytes: None,
                accepted_frame_bytes: bytes.len(),
                write_start_us: self.metrics.timestamp_us(write_start.at),
                last_byte_us: self
                    .metrics
                    .timestamp_us(last_byte.unwrap_or(write_start.at)),
            }),
            DecodeState::Body {
                header_bytes,
                content_length,
                seen,
                write_start,
                last_byte,
                ..
            } => Some(PartialFrameMetric {
                phase: "body",
                expected_frame_bytes: Some(*header_bytes + *content_length),
                accepted_frame_bytes: *header_bytes + *seen,
                write_start_us: self.metrics.timestamp_us(write_start.at),
                last_byte_us: self.metrics.timestamp_us(*last_byte),
            }),
            _ => None,
        };
        if let Some(partial) = partial {
            self.metrics.record_partial_frame(partial);
        }
    }
}

fn retain_tail(tail: &mut Vec<u8>, accepted: &[u8]) {
    if accepted.len() >= BODY_TAIL_LIMIT {
        tail.clear();
        tail.extend_from_slice(&accepted[accepted.len() - BODY_TAIL_LIMIT..]);
        return;
    }
    let overflow = tail
        .len()
        .saturating_add(accepted.len())
        .saturating_sub(BODY_TAIL_LIMIT);
    if overflow > 0 {
        tail.copy_within(overflow.., 0);
        tail.truncate(tail.len() - overflow);
    }
    tail.extend_from_slice(accepted);
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
                write_sequence: 0,
                method: None,
                id: Some(response.id().clone()),
                body_bytes: 0,
                frame_bytes: 0,
                ready_us: None,
                write_start_us: 0,
                last_byte_us: 0,
                flush_complete_us: None,
                id_unattributed: false,
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

    #[tokio::test]
    async fn service_records_completion_before_its_response_is_consumed() {
        let metrics = Arc::new(StdoutMetrics::new(Instant::now()));
        let mut service = ResponseReadyService::new(EchoService, Arc::clone(&metrics));
        let mut response_future = tower::Service::call(
            &mut service,
            Request::build("textDocument/semanticTokens/full")
                .id(7i64)
                .finish(),
        );

        tokio::task::yield_now().await;
        assert_eq!(metrics.ready_count(), 0, "call alone does not admit work");

        assert!(futures::poll!(response_future.as_mut()).is_pending());
        tokio::task::yield_now().await;

        assert_eq!(metrics.ready_count(), 1);
        assert!(response_future.await.unwrap().is_some());
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
                write_sequence: 1,
                method: Some("textDocument/semanticTokens/full".to_string()),
                id: Some(Id::Number(7)),
                body_bytes: body.len(),
                frame_bytes: frame.len(),
                ready_us: Some(10),
                write_start_us: 20,
                last_byte_us: 30,
                flush_complete_us: Some(40),
                id_unattributed: false,
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

    #[test]
    fn batched_frames_share_the_poll_write_attempt_timestamp() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        let body = br#"{"jsonrpc":"2.0","method":"window/logMessage"}"#;
        let frame = [
            format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let batch = [frame.clone(), frame].concat();
        let mut observer = FrameObserver::new(Arc::clone(&metrics));

        observer.write_attempt(origin + Duration::from_micros(10));
        observer.accepted(&batch, origin + Duration::from_micros(20));
        observer.flushed(origin + Duration::from_micros(30));

        assert_eq!(
            metrics
                .frames()
                .iter()
                .map(|frame| frame.write_start_us)
                .collect::<Vec<_>>(),
            vec![10, 10]
        );
        assert_eq!(
            metrics
                .frames()
                .iter()
                .map(|frame| frame.write_sequence)
                .collect::<Vec<_>>(),
            vec![0, 0]
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

    #[test]
    fn long_string_response_id_is_attributed_without_scanning_the_full_body() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        let id = "x".repeat(300);
        metrics.record_response_ready(
            Id::String(id.clone()),
            "textDocument/semanticTokens/full".to_string(),
            origin,
        );
        let body = format!(r#"{{"jsonrpc":"2.0","result":null,"id":"{id}"}}"#);
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut observer = FrameObserver::new(Arc::clone(&metrics));

        observer.accepted(frame.as_bytes(), origin + Duration::from_micros(10));
        observer.flushed(origin + Duration::from_micros(20));

        assert_eq!(metrics.frames()[0].id, Some(Id::String(id)));
        assert!(!metrics.frames()[0].id_unattributed);
    }

    #[test]
    fn oversized_string_response_id_is_explicitly_unattributed() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        let id = "x".repeat(BODY_TAIL_LIMIT + 100);
        let body = format!(r#"{{"jsonrpc":"2.0","result":null,"id":"{id}"}}"#);
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut observer = FrameObserver::new(Arc::clone(&metrics));

        observer.accepted(frame.as_bytes(), origin + Duration::from_micros(10));
        observer.flushed(origin + Duration::from_micros(20));

        assert_eq!(metrics.frames()[0].id, None);
        assert!(metrics.frames()[0].id_unattributed);
    }

    #[test]
    fn partial_body_is_reported_as_a_censored_frame_on_drop() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        let body = br#"{"jsonrpc":"2.0","result":null,"id":7}"#;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());

        {
            let mut observer = FrameObserver::new(Arc::clone(&metrics));
            observer.write_attempt(origin + Duration::from_micros(5));
            observer.accepted(header.as_bytes(), origin + Duration::from_micros(10));
            observer.accepted(&body[..5], origin + Duration::from_micros(15));
        }

        assert_eq!(
            metrics.partial_frames(),
            vec![PartialFrameMetric {
                phase: "body",
                expected_frame_bytes: Some(header.len() + body.len()),
                accepted_frame_bytes: header.len() + 5,
                write_start_us: 5,
                last_byte_us: 15,
            }]
        );
    }

    #[test]
    fn pending_first_write_attempt_is_reported_as_a_censored_header() {
        let origin = Instant::now();
        let metrics = Arc::new(StdoutMetrics::new(origin));
        {
            let mut observer = FrameObserver::new(Arc::clone(&metrics));
            observer.write_attempt(origin + Duration::from_micros(5));
        }

        assert_eq!(
            metrics.partial_frames(),
            vec![PartialFrameMetric {
                phase: "header",
                expected_frame_bytes: None,
                accepted_frame_bytes: 0,
                write_start_us: 5,
                last_byte_us: 5,
            }]
        );
    }
}
