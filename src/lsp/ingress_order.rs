//! Tower middleware enforcing per-document wire order between document
//! mutations and tree-reading requests at LSP ingress (#342).
//!
//! tower-lsp-server dispatches handler futures through `buffer_unordered`,
//! which polls them concurrently: a `didChange` received *before* a
//! `semanticTokens` request can be first-polled *after* it, so the request
//! may snapshot a document missing edits that preceded it on the wire. The
//! per-URI `edit_lock` acquired as a handler's first `.await` follows
//! first-poll order — a strong practical mitigation but not a guarantee.
//!
//! `Server::serve` calls `service.call(req)` synchronously in wire order
//! *before* buffering the returned futures, so this middleware can assign
//! per-URI sequence tickets at `call` time:
//!
//! - **Writers** (`didOpen` / `didChange` / `didClose`) take the next ticket
//!   and run only after the previous writer for the same document finished,
//!   so edits and closes apply in strict wire order.
//! - **Readers** (the `semanticTokens` family) snapshot the current tail
//!   ticket at `call` time and run only once that ticket is done, so a
//!   request observes every edit that preceded it on the wire — without
//!   serializing token computation against later edits or other documents.
//!
//! Everything else passes through untouched.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use dashmap::DashMap;
use tokio::sync::watch;
use tower::Service;
use tower_lsp_server::jsonrpc::{Request, Response};

/// Per-document sequencing state.
struct DocSeq {
    /// Last issued writer ticket (tickets start at 1; 0 = none issued).
    tail: u64,
    /// Highest completed writer ticket. Waiters subscribe to this channel;
    /// dropping the sender (entry removal after a close) wakes them with an
    /// error, which they treat as "nothing left to wait for".
    done: watch::Sender<u64>,
}

/// Issues per-document writer tickets and reader barriers in wire order.
///
/// `issue_writer_ticket` / `reader_barrier` are synchronous so they can run
/// inside `Service::call`, which tower-lsp-server invokes in wire order; the
/// returned values are awaited later, inside the buffered handler future.
#[derive(Default)]
pub(crate) struct DocumentSequencer {
    docs: DashMap<String, DocSeq>,
}

impl DocumentSequencer {
    /// Take the next writer ticket for `uri`.
    pub(crate) fn issue_writer_ticket(&self, uri: &str) -> WriterGate {
        let mut entry = self.docs.entry(uri.to_string()).or_insert_with(|| DocSeq {
            tail: 0,
            done: watch::Sender::new(0),
        });
        entry.tail += 1;
        WriterGate {
            ticket: entry.tail,
            rx: entry.done.subscribe(),
            guard: CompletionGuard {
                ticket: entry.tail,
                tx: entry.done.clone(),
            },
        }
    }

    /// Snapshot the barrier a reader of `uri` must wait behind: the writer
    /// ticket tail at call time. Returns `None` when no writer is pending
    /// (no entry, or every issued ticket already completed).
    pub(crate) fn reader_barrier(&self, uri: &str) -> Option<ReaderBarrier> {
        let entry = self.docs.get(uri)?;
        if *entry.done.borrow() >= entry.tail {
            return None;
        }
        Some(ReaderBarrier {
            target: entry.tail,
            rx: entry.done.subscribe(),
        })
    }

    /// Drop `uri`'s sequencing state after a close completed, unless later
    /// writers were already ticketed behind it. Dropping the entry also drops
    /// the `done` sender, waking any stragglers still subscribed.
    pub(crate) fn finish_close(&self, uri: &str, ticket: u64) {
        self.docs.remove_if(uri, |_, seq| seq.tail == ticket);
    }
}

/// A writer's place in its document's queue: await [`WriterGate::wait_turn`]
/// before mutating, then drop the gate (or the whole future) to mark the
/// ticket done — completion-on-drop keeps successors from wedging even if
/// the handler future is cancelled at shutdown.
pub(crate) struct WriterGate {
    ticket: u64,
    rx: watch::Receiver<u64>,
    #[allow(dead_code)] // held for its Drop impl
    guard: CompletionGuard,
}

impl WriterGate {
    pub(crate) fn ticket(&self) -> u64 {
        self.ticket
    }

    /// Wait until the previous writer ticket for this document completed.
    pub(crate) async fn wait_turn(&mut self) {
        let target = self.ticket - 1;
        // Err means the sender was dropped (entry removed after a close):
        // nothing is pending anymore, so proceed.
        let _ = self.rx.wait_for(|done| *done >= target).await;
    }
}

/// Marks a writer ticket complete on drop.
struct CompletionGuard {
    ticket: u64,
    tx: watch::Sender<u64>,
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.tx.send_modify(|done| {
            if self.ticket > *done {
                *done = self.ticket;
            }
        });
    }
}

/// A reader's wait target: the writer tail ticket snapshotted at `call` time.
pub(crate) struct ReaderBarrier {
    target: u64,
    rx: watch::Receiver<u64>,
}

impl ReaderBarrier {
    /// Wait until every writer ticketed before this reader completed.
    pub(crate) async fn wait(mut self) {
        // Err means the sender was dropped (entry removed after a close):
        // every prior writer finished, so proceed.
        let _ = self.rx.wait_for(|done| *done >= self.target).await;
    }
}

/// How a request participates in per-document ordering.
enum Role {
    /// Mutates document state; applies in strict wire order per URI.
    Writer { uri: String, close: bool },
    /// Reads the document tree; waits for writers that preceded it.
    Reader { uri: String },
}

/// Classify a request and extract its `textDocument.uri`, both synchronously.
fn classify(req: &Request) -> Option<Role> {
    let close = match req.method() {
        "textDocument/didOpen" | "textDocument/didChange" => false,
        "textDocument/didClose" => true,
        "textDocument/semanticTokens/full"
        | "textDocument/semanticTokens/full/delta"
        | "textDocument/semanticTokens/range" => {
            let uri = text_document_uri(req)?;
            return Some(Role::Reader { uri });
        }
        _ => return None,
    };
    let uri = text_document_uri(req)?;
    Some(Role::Writer { uri, close })
}

fn text_document_uri(req: &Request) -> Option<String> {
    Some(
        req.params()?
            .get("textDocument")?
            .get("uri")?
            .as_str()?
            .to_string(),
    )
}

/// Tower middleware applying [`DocumentSequencer`] ordering to the LSP
/// request stream. Wraps the outermost service handed to `Server::serve` so
/// ticket assignment happens in wire order.
pub struct IngressOrderGate<S> {
    inner: S,
    sequencer: Arc<DocumentSequencer>,
}

impl<S> IngressOrderGate<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            sequencer: Arc::new(DocumentSequencer::default()),
        }
    }
}

impl<S> Service<Request> for IngressOrderGate<S>
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

    fn call(&mut self, req: Request) -> Self::Future {
        let role = classify(&req);
        // The inner call must stay synchronous inside `call` so nested
        // middleware (e.g. RequestIdCapture) keeps seeing wire order too.
        let inner_fut = self.inner.call(req);
        match role {
            None => Box::pin(inner_fut),
            Some(Role::Writer { uri, close }) => {
                let sequencer = Arc::clone(&self.sequencer);
                let mut gate = sequencer.issue_writer_ticket(&uri);
                Box::pin(async move {
                    gate.wait_turn().await;
                    let result = inner_fut.await;
                    let ticket = gate.ticket();
                    // Mark done before any cleanup decision.
                    drop(gate);
                    if close {
                        sequencer.finish_close(&uri, ticket);
                    }
                    result
                })
            }
            Some(Role::Reader { uri }) => {
                let barrier = self.sequencer.reader_barrier(&uri);
                Box::pin(async move {
                    if let Some(barrier) = barrier {
                        barrier.wait().await;
                    }
                    inner_fut.await
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "file:///test/doc.md";

    fn pending<F: Future>(fut: &mut Pin<&mut F>) -> bool {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        fut.as_mut().poll(&mut cx).is_pending()
    }

    #[tokio::test]
    async fn writers_run_in_ticket_order() {
        let seq = DocumentSequencer::default();
        let mut first = seq.issue_writer_ticket(URI);
        let mut second = seq.issue_writer_ticket(URI);

        // First writer proceeds immediately.
        {
            let mut first_wait = std::pin::pin!(first.wait_turn());
            assert!(!pending(&mut first_wait), "ticket 1 must not wait");
        }

        // Second writer is blocked until the first completes.
        {
            let mut second_wait = std::pin::pin!(second.wait_turn());
            assert!(pending(&mut second_wait), "ticket 2 must wait for ticket 1");
        }

        drop(first); // completion-on-drop
        second.wait_turn().await; // must resolve now
    }

    #[tokio::test]
    async fn reader_waits_for_writers_before_it_only() {
        let seq = DocumentSequencer::default();
        let first = seq.issue_writer_ticket(URI);

        let barrier = seq.reader_barrier(URI).expect("writer pending");

        // A writer arriving AFTER the reader must not extend its wait.
        let second = seq.issue_writer_ticket(URI);

        let mut wait = std::pin::pin!(barrier.wait());
        assert!(pending(&mut wait), "reader must wait for ticket 1");

        drop(first);
        wait.await; // resolves even though ticket 2 is still pending

        drop(second);
    }

    #[tokio::test]
    async fn reader_with_no_pending_writers_does_not_wait() {
        let seq = DocumentSequencer::default();
        assert!(
            seq.reader_barrier(URI).is_none(),
            "no entry means no waiting"
        );

        let first = seq.issue_writer_ticket(URI);
        drop(first);
        assert!(
            seq.reader_barrier(URI).is_none(),
            "all tickets done means no waiting"
        );
    }

    #[tokio::test]
    async fn documents_sequence_independently() {
        let seq = DocumentSequencer::default();
        let _other = seq.issue_writer_ticket("file:///test/other.md");

        let mut here = seq.issue_writer_ticket(URI);
        let mut wait = std::pin::pin!(here.wait_turn());
        assert!(
            !pending(&mut wait),
            "a pending writer on another document must not block this one"
        );
    }

    #[tokio::test]
    async fn cancelled_writer_still_unblocks_successor() {
        let seq = DocumentSequencer::default();
        let first = seq.issue_writer_ticket(URI);
        let mut second = seq.issue_writer_ticket(URI);

        // Simulate the first writer's future being dropped without running
        // (e.g. shutdown): the completion guard must still mark it done.
        drop(first);

        second.wait_turn().await;
    }

    #[tokio::test]
    async fn finish_close_removes_state_and_wakes_stragglers() {
        let seq = DocumentSequencer::default();
        let close = seq.issue_writer_ticket(URI);
        let barrier = seq.reader_barrier(URI).expect("close pending");

        let ticket = close.ticket();
        drop(close);
        seq.finish_close(URI, ticket);
        assert!(!seq.docs.contains_key(URI), "entry removed after close");

        // A barrier subscribed before removal must still resolve.
        barrier.wait().await;
    }

    #[tokio::test]
    async fn finish_close_keeps_state_for_later_writers() {
        let seq = DocumentSequencer::default();
        let close = seq.issue_writer_ticket(URI);
        let ticket = close.ticket();
        let reopen = seq.issue_writer_ticket(URI); // reopen ticketed behind the close

        drop(close);
        seq.finish_close(URI, ticket);
        assert!(
            seq.docs.contains_key(URI),
            "entry must survive while later tickets are pending"
        );
        drop(reopen);
    }

    fn notification(method: &'static str, uri: &str) -> Request {
        Request::build(method)
            .params(serde_json::json!({ "textDocument": { "uri": uri } }))
            .finish()
    }

    #[test]
    fn classify_routes_methods() {
        let writer = classify(&notification("textDocument/didChange", URI));
        assert!(matches!(writer, Some(Role::Writer { close: false, .. })));

        let close = classify(&notification("textDocument/didClose", URI));
        assert!(matches!(close, Some(Role::Writer { close: true, .. })));

        let open = classify(&notification("textDocument/didOpen", URI));
        assert!(matches!(open, Some(Role::Writer { close: false, .. })));

        let reader = classify(&notification("textDocument/semanticTokens/full", URI));
        assert!(matches!(reader, Some(Role::Reader { .. })));

        assert!(
            classify(&notification("textDocument/hover", URI)).is_none(),
            "unrelated methods pass through"
        );
        assert!(
            classify(&Request::build("textDocument/didChange").finish()).is_none(),
            "missing params pass through rather than panic"
        );
    }
}
