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
//! - **Writers** (`didOpen` / `didChange` / `didSave` / `didClose`) take the next ticket
//!   and run only after the previous writer for the same document finished,
//!   so opens, edits, saves, and closes apply in strict wire order. Treating
//!   `didSave` as an ordering fence keeps a later unsaved edit from overtaking
//!   the save-time host/virtual synchronization. Gating `didOpen`
//!   (#374) closes the two residual first-poll-order races: a `didChange`
//!   first-polled before `didOpen` no longer misses the document and discards
//!   its edit (open→edit), and a reopen no longer inserts ahead of a gated
//!   `didClose` that then evicts it (close→reopen). The historical deadlock
//!   rationale for leaving `didOpen` ungated — its handler awaiting
//!   downstream spawn that itself waits on the client — no longer applies:
//!   downstream spawn is fire-and-forget and the open path never *awaits* a
//!   server→client request (its editor-facing awaits are one-way log/show
//!   notifications, and the one awaited bridge call, `process_injections`,
//!   only forwards a didChange on the edit path, not on open), so the handler
//!   can never block on a client response. A slow auto-install still runs
//!   inside the handler and so holds the writer ticket — its network/git
//!   stages are timeout-bounded, but parser *compilation* is not, so a
//!   pathologically hung compiler can hold the ticket indefinitely and wedge
//!   later same-URI readers/writers. Moving auto-install to a spawned task
//!   (#480) is therefore a liveness fix, not just a latency one; until then
//!   the exposure is one-time, first open of an uninstalled parser.
//! - **Readers** (the `semanticTokens` family, the `kakehashi/captures`
//!   triple, the edit-producing formatting/rename/color-presentation requests, and pull
//!   diagnostics) snapshot the current
//!   tail ticket at `call` time and run only once that ticket is done, so a
//!   request observes every edit that preceded it on the wire — without
//!   serializing its computation against later edits or other documents.
//! - **Workspace diagnostics** run one at a time in wire order. A downstream
//!   producer can exit after its contribution is accepted and request a refresh
//!   before that response reaches the socket; serializing a refresh-induced pull
//!   behind the accepted handler keeps the newer response from overtaking it.
//!
//! Everything else passes through untouched.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use dashmap::DashMap;
use tokio::sync::watch;
use tower::Service;
use tower_lsp_server::jsonrpc::{Request, Response};

use crate::error::LockResultExt;
use crate::lsp::bridge::envelope::ENVELOPE_KEY;

tokio::task_local! {
    /// The current writer's ingress ticket, scoped around the gated handler
    /// future so the `didOpen` / `didChange` handler can stamp the parse it
    /// schedules with the wire-order ticket. Read via [`current_writer_ticket`].
    ///
    /// Deliberately bridges gate→handler only: the handler reads it and threads
    /// the ticket down to `parse_document` as an explicit value, so the parse can
    /// publish the watermark from inside the per-document actor task later, where
    /// this task-local is no longer in scope.
    static WRITER_TICKET: u64;

    /// The tail writer ticket a reader must observe — the highest ticket issued
    /// for the document at the reader's `call` time. Scoped around the gated
    /// reader handler so it can wait for the parse watermark to reach this ticket.
    /// Read via [`current_reader_tail`].
    static READER_TAIL: u64;

    /// Set by a reclaimable writer handler after it determines that the URI
    /// has no live document. The outer gate then removes idle sequencing state
    /// without resetting tickets for a live document.
    static RECLAIM_WRITER_SEQUENCE: Arc<AtomicBool>;
}

/// The ingress writer ticket of the currently-running gated writer handler, or
/// `None` when called outside a gated writer (e.g. a unit test invoking a
/// handler directly without the middleware).
pub(crate) fn current_writer_ticket() -> Option<u64> {
    WRITER_TICKET.try_with(|ticket| *ticket).ok()
}

/// The tail writer ticket the currently-running gated reader handler must
/// observe, or `None` outside a gated reader or when no writer has been issued
/// for the document (nothing to wait for). Test-only since the snapshot
/// conversion removed the reader-side watermark wait; the READER_TAIL scoping
/// itself is retained (the middleware still stamps it) until Stage 3 prunes
/// the watermark system.
#[cfg(test)]
pub(crate) fn current_reader_tail() -> Option<u64> {
    READER_TAIL.try_with(|ticket| *ticket).ok()
}

/// Ask the ingress gate to remove this writer's per-URI sequencing entry once
/// the handler completes, provided no later writer was already ticketed.
pub(crate) fn reclaim_current_writer_sequence() {
    let _ = RECLAIM_WRITER_SEQUENCE.try_with(|flag| flag.store(true, Ordering::Release));
}

/// Per-document sequencing state.
struct DocSeq {
    /// Last issued writer ticket (tickets start at 1; 0 = none issued).
    tail: u64,
    /// Completion channel + out-of-order ledger shared with issued guards.
    completion: Arc<DocCompletion>,
}

/// Completion tracking shared between a document's entry and its in-flight
/// writer guards.
struct DocCompletion {
    /// Highest **contiguously** completed writer ticket. Waiters subscribe to
    /// this channel; dropping the sender (entry removal after a close) wakes
    /// them with an error, which they treat as "nothing left to wait for".
    done: watch::Sender<u64>,
    /// Tickets completed ahead of a still-running predecessor. `done` only
    /// advances through consecutive tickets, so a hypothetical selectively
    /// cancelled middle writer can never unblock its successor while an
    /// earlier writer is still running — the guarantee holds locally instead
    /// of leaning on the transport never dropping individual notification
    /// futures.
    early: std::sync::Mutex<std::collections::BTreeSet<u64>>,
}

impl DocCompletion {
    /// Record `ticket` as complete, advancing `done` only through
    /// consecutive completions and cascading any tickets that finished early.
    /// `send_if_modified` keeps an out-of-order completion (ledger insert,
    /// `done` unchanged) from spuriously waking every waiter; the ledger lock
    /// lives inside the closure, so it is released before subscribers are
    /// notified.
    fn complete(&self, ticket: u64) {
        self.done.send_if_modified(|done| {
            let mut early = self.early.lock().recover_poison("DocCompletion::complete");
            if ticket == *done + 1 {
                *done = ticket;
                while early.remove(&(*done + 1)) {
                    *done += 1;
                }
                true
            } else {
                if ticket > *done {
                    early.insert(ticket);
                }
                false
            }
        });
    }
}

/// Issues per-document writer tickets and reader barriers in wire order.
///
/// `issue_writer_ticket` / `reader_barrier_and_tail` are synchronous so they run
/// inside `Service::call`, which tower-lsp-server invokes in wire order; the
/// returned values are awaited later, inside the buffered handler future.
#[derive(Default)]
pub(crate) struct DocumentSequencer {
    docs: DashMap<String, DocSeq>,
}

impl DocumentSequencer {
    /// Take the next writer ticket for `uri`.
    pub(crate) fn issue_writer_ticket(&self, uri: &str) -> WriterGate {
        // Fast path: rapid didChange streams hit an existing entry, where
        // `get_mut` borrows the key without allocating; only a miss pays for
        // the owned key.
        if let Some(mut entry) = self.docs.get_mut(uri) {
            return Self::next_ticket(&mut entry);
        }
        let mut entry = self.docs.entry(uri.to_string()).or_insert_with(|| DocSeq {
            tail: 0,
            completion: Arc::new(DocCompletion {
                done: watch::Sender::new(0),
                early: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            }),
        });
        Self::next_ticket(&mut entry)
    }

    /// Bump `seq`'s tail and build the gate for the new ticket. Only copies
    /// fields out, so callers' shard guards span just the bump.
    fn next_ticket(seq: &mut DocSeq) -> WriterGate {
        seq.tail += 1;
        let ticket = seq.tail;
        let rx = seq.completion.done.subscribe();
        let completion = Arc::clone(&seq.completion);
        WriterGate {
            ticket,
            rx,
            completion,
        }
    }

    /// Snapshot, in a **single** `docs` lookup (the reader hot path), both of the
    /// things a reader of `uri` needs at call time:
    ///
    /// - the **barrier** it must wait behind — the writer tail ticket — or `None`
    ///   when no writer is pending (no entry, or every issued ticket completed);
    /// - the **tail** writer ticket it must observe via the parse watermark, or
    ///   `None` when no writer was ever ticketed. Unlike the barrier, this is
    ///   reported even when every writer has already completed, because the reader
    ///   waits on the watermark (which a completed ticket has already advanced),
    ///   not on ticket completion.
    pub(crate) fn reader_barrier_and_tail(
        &self,
        uri: &str,
    ) -> (Option<ReaderBarrier>, Option<u64>) {
        let Some(entry) = self.docs.get(uri) else {
            return (None, None);
        };
        let done = *entry.completion.done.borrow();
        let tail = entry.tail;
        let barrier = if done < tail {
            Some(ReaderBarrier {
                target: tail,
                rx: entry.completion.done.subscribe(),
            })
        } else {
            None
        };
        // Release the `docs` read-lock shard before the cheap tail check: this is
        // the reader hot path, and holding it would needlessly contend with
        // writers taking the shard's write lock (issue_writer_ticket / finish_close).
        drop(entry);
        (barrier, (tail > 0).then_some(tail))
    }

    fn pending_writer_barriers(&self) -> Vec<ReaderBarrier> {
        self.docs
            .iter()
            .filter_map(|entry| {
                let done = *entry.completion.done.borrow();
                (done < entry.tail).then(|| ReaderBarrier {
                    target: entry.tail,
                    rx: entry.completion.done.subscribe(),
                })
            })
            .collect()
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
    completion: Arc<DocCompletion>,
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

/// Marks the writer's ticket complete on drop.
impl Drop for WriterGate {
    fn drop(&mut self) {
        self.completion.complete(self.ticket);
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
    Writer {
        uri: String,
        close: bool,
        reclaim_if_missing: bool,
    },
    /// Reads the document tree; waits for writers that preceded it.
    Reader { uri: String },
    /// Produces a workspace-wide diagnostic snapshot; serialized globally.
    WorkspaceDiagnostic,
    /// Mutates the workspace scope observed by workspace-wide requests.
    WorkspaceMutation,
}

/// Classify a request and extract its `textDocument.uri`, both synchronously.
///
/// Readers are the document-snapshotting request families: `semanticTokens`
/// (including `range`, which never had the in-handler `edit_lock` settle),
/// the `kakehashi/captures` triple, which mirrors it, the edit-producing
/// requests (formatting, rename — their edits are applied by the client to
/// its current text, so edits computed against a stale snapshot corrupt the
/// document), and pull diagnostics.
/// `textDocument/codeAction` is in the same class: its
/// `context.diagnostics` and returned edits are computed against the
/// document snapshot (#568), and so is `textDocument/inlayHint`, whose
/// accept-time edits and resolve stamp are computed against it. `codeLens/resolve`, `codeAction/resolve`,
/// `documentLink/resolve`, and `inlayHint/resolve` are readers too (#355, #568):
/// their freshness gates read tracker/document
/// document snapshot (#568). `codeLens/resolve`, `codeAction/resolve`, and
/// `documentLink/resolve`, `inlayHint/resolve`, and
/// `callHierarchy/incomingCalls`, `callHierarchy/outgoingCalls`, and
/// `typeHierarchy/supertypes` and `typeHierarchy/subtypes` are readers too (#355, #568): their freshness gates read tracker/document
/// state, so they must observe every `didChange` that preceded them on the
/// wire — but their params carry no `textDocument`, so the URI comes from the
/// routing envelope (`params.data` for resolve methods and `params.item.data`
/// for hierarchy expansion). Unenveloped items pass through ungated; resolve
/// handlers return their inputs unchanged, while hierarchy expansion returns
/// `null` because it has no producer to expand. The
/// full `kakehashi/node` family is gated too (#698). Its tracker and
/// incarnation checks catch staleness only after a lifecycle writer has run;
/// without this barrier a following node request can overtake a wire-order
/// `didChange`/`didClose` and observe the pre-writer document.
fn classify(req: &Request) -> Option<Role> {
    let method = req.method();
    match method {
        "workspace/didChangeWorkspaceFolders" => Some(Role::WorkspaceMutation),
        "workspace/diagnostic" => Some(Role::WorkspaceDiagnostic),
        "textDocument/didSave" => {
            // Cleanup for a missing save is signalled by the typed handler.
            // Decode the complete params before issuing a ticket so malformed
            // payloads rejected before handler entry cannot retain sequencing
            // state indefinitely.
            serde_json::from_value::<tower_lsp_server::ls_types::DidSaveTextDocumentParams>(
                req.params()?.clone(),
            )
            .ok()?;
            let uri = text_document_uri(req)?;
            Some(Role::Writer {
                uri,
                close: false,
                reclaim_if_missing: true,
            })
        }
        "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didClose" => {
            let uri = text_document_uri(req)?;
            Some(Role::Writer {
                uri,
                close: method == "textDocument/didClose",
                reclaim_if_missing: false,
            })
        }
        "textDocument/semanticTokens/full"
        | "textDocument/semanticTokens/full/delta"
        | "textDocument/semanticTokens/range"
        | "textDocument/formatting"
        | "textDocument/rangeFormatting"
        | "textDocument/rename"
        | "textDocument/prepareRename"
        | "textDocument/codeAction"
        | "textDocument/inlayHint"
        | "textDocument/documentColor"
        | "textDocument/colorPresentation"
        | "textDocument/inlineValue"
        | "textDocument/selectionRange"
        | "textDocument/prepareCallHierarchy"
        | "textDocument/prepareTypeHierarchy"
        | "textDocument/diagnostic"
        | "kakehashi/captures/full"
        | "kakehashi/captures/full/delta"
        | "kakehashi/captures/range" => {
            let uri = text_document_uri(req)?;
            Some(Role::Reader { uri })
        }
        "codeLens/resolve"
        | "codeAction/resolve"
        | "documentLink/resolve"
        | "inlayHint/resolve" => {
            let raw = req.params()?["data"][ENVELOPE_KEY]["host_uri"].as_str()?;
            Some(Role::Reader {
                uri: normalize_uri(raw),
            })
        }
        "callHierarchy/incomingCalls" | "callHierarchy/outgoingCalls" => {
            let raw = req.params()?["item"]["data"]["kakehashi"]["host_uri"].as_str()?;
            Some(Role::Reader {
                uri: normalize_uri(raw),
            })
        }
        "typeHierarchy/supertypes" | "typeHierarchy/subtypes" => {
            let raw = req.params()?["item"]["data"]["kakehashi"]["host_uri"].as_str()?;
            Some(Role::Reader {
                uri: normalize_uri(raw),
            })
        }
        _ if method == "kakehashi/node" || method.starts_with("kakehashi/node/") => {
            let uri = text_document_uri(req)?;
            Some(Role::Reader { uri })
        }
        _ => None,
    }
}

/// Extract `params.textDocument.uri`, normalized through `Url` so spelling
/// variants of the same document (percent-encoding, default ports) sequence
/// together — internal document state is keyed on parsed `Url`s, and the
/// gate's keys must not be finer-grained than that. Invalid URIs are left
/// ungated: typed LSP parameter decoding rejects them before a handler can
/// request cleanup, so issuing a ticket would retain unreachable state.
fn text_document_uri(req: &Request) -> Option<String> {
    // Indexing a missing key or non-object yields Value::Null, whose as_str()
    // returns None — same outcome as get() chains, less noise.
    let raw = req.params()?["textDocument"]["uri"].as_str()?;
    let lsp_uri = raw.parse::<tower_lsp_server::ls_types::Uri>().ok()?;
    url::Url::parse(lsp_uri.as_str()).map(String::from).ok()
}

/// Normalize a URI spelling through `Url` so variants of the same document
/// (percent-encoding, default ports, scheme casing) sequence together.
pub(crate) fn normalize_uri(raw: &str) -> String {
    url::Url::parse(raw)
        .map(String::from)
        .unwrap_or_else(|_| raw.to_string())
}

/// Tower middleware applying [`DocumentSequencer`] ordering to the LSP
/// request stream. Wraps the outermost service handed to `Server::serve` so
/// ticket assignment happens in wire order.
pub struct IngressOrderGate<S> {
    inner: S,
    sequencer: Arc<DocumentSequencer>,
    workspace_diagnostic_sequencer: Arc<DocumentSequencer>,
    cancel_forwarder: Option<crate::lsp::request_id::CancelForwarder>,
}

impl<S> IngressOrderGate<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            sequencer: Arc::new(DocumentSequencer::default()),
            workspace_diagnostic_sequencer: Arc::new(DocumentSequencer::default()),
            cancel_forwarder: None,
        }
    }

    pub fn with_cancel_forwarder(
        inner: S,
        cancel_forwarder: crate::lsp::request_id::CancelForwarder,
    ) -> Self {
        Self {
            inner,
            sequencer: Arc::new(DocumentSequencer::default()),
            workspace_diagnostic_sequencer: Arc::new(DocumentSequencer::default()),
            cancel_forwarder: Some(cancel_forwarder),
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
        let request_id = req.id().cloned();
        // The inner call must stay synchronous inside `call` so nested
        // middleware (e.g. RequestIdCapture) keeps seeing wire order too.
        let inner_fut = self.inner.call(req);
        match role {
            None => Box::pin(inner_fut),
            Some(Role::WorkspaceMutation) => {
                let mut gate = self
                    .workspace_diagnostic_sequencer
                    .issue_writer_ticket("workspace/diagnostic");
                Box::pin(async move {
                    gate.wait_turn().await;
                    let result = inner_fut.await;
                    drop(gate);
                    result
                })
            }
            Some(Role::WorkspaceDiagnostic) => {
                let document_barriers = self.sequencer.pending_writer_barriers();
                let mut gate = self
                    .workspace_diagnostic_sequencer
                    .issue_writer_ticket("workspace/diagnostic");
                let cancellation = self.cancel_forwarder.as_ref().and_then(|forwarder| {
                    let upstream_id = match request_id.as_ref()? {
                        tower_lsp_server::jsonrpc::Id::Number(id) => {
                            crate::lsp::bridge::UpstreamId::Number(*id)
                        }
                        tower_lsp_server::jsonrpc::Id::String(id) => {
                            crate::lsp::bridge::UpstreamId::String(id.clone())
                        }
                        tower_lsp_server::jsonrpc::Id::Null => return None,
                    };
                    forwarder
                        .subscribe(upstream_id.clone())
                        .ok()
                        .map(|receiver| (forwarder.clone(), upstream_id, receiver))
                });
                Box::pin(async move {
                    if let Some((forwarder, upstream_id, mut cancel_rx)) = cancellation {
                        let wait_for_document_writers = async {
                            for barrier in document_barriers {
                                barrier.wait().await;
                            }
                        };
                        tokio::select! {
                            biased;
                            _ = &mut cancel_rx => {
                                forwarder.unsubscribe(&upstream_id);
                                drop(inner_fut);
                                drop(gate);
                                return Ok(request_id.map(|id| {
                                    Response::from_error(
                                        id,
                                        tower_lsp_server::jsonrpc::Error::request_cancelled(),
                                    )
                                }));
                            }
                            _ = wait_for_document_writers => {}
                        }
                        tokio::select! {
                            biased;
                            _ = &mut cancel_rx => {
                                forwarder.unsubscribe(&upstream_id);
                                drop(inner_fut);
                                drop(gate);
                                return Ok(request_id.map(|id| {
                                    Response::from_error(
                                        id,
                                        tower_lsp_server::jsonrpc::Error::request_cancelled(),
                                    )
                                }));
                            }
                            _ = gate.wait_turn() => {}
                        }
                        if forwarder.unsubscribe_and_take_cancelled(&upstream_id) {
                            drop(inner_fut);
                            drop(gate);
                            return Ok(request_id.map(|id| {
                                Response::from_error(
                                    id,
                                    tower_lsp_server::jsonrpc::Error::request_cancelled(),
                                )
                            }));
                        }
                    } else {
                        for barrier in document_barriers {
                            barrier.wait().await;
                        }
                        gate.wait_turn().await;
                    }
                    let result = inner_fut.await;
                    drop(gate);
                    result
                })
            }
            Some(Role::Writer {
                uri,
                close,
                reclaim_if_missing,
            }) => {
                let sequencer = Arc::clone(&self.sequencer);
                let mut gate = sequencer.issue_writer_ticket(&uri);
                let ticket = gate.ticket();
                Box::pin(async move {
                    gate.wait_turn().await;
                    // Scope the ticket around the handler so it can stamp its
                    // scheduled parse with this wire-order ticket.
                    let reclaim = reclaim_if_missing.then(|| Arc::new(AtomicBool::new(false)));
                    let result = if let Some(flag) = &reclaim {
                        RECLAIM_WRITER_SEQUENCE
                            .scope(Arc::clone(flag), WRITER_TICKET.scope(ticket, inner_fut))
                            .await
                    } else {
                        WRITER_TICKET.scope(ticket, inner_fut).await
                    };
                    // Mark done before any cleanup decision.
                    drop(gate);
                    if close || reclaim.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                        sequencer.finish_close(&uri, ticket);
                    }
                    result
                })
            }
            Some(Role::Reader { uri }) => {
                // Snapshot the barrier and the tail ticket at `call` time (wire
                // order) in one lookup, exposing the tail to the handler so a
                // virt/native reader can wait for the parse watermark to reach it.
                // The barrier only reports a pending writer; the tail is needed
                // even when all writers completed, because the watermark — not
                // ticket completion — gates the read.
                let (barrier, tail) = self.sequencer.reader_barrier_and_tail(&uri);
                Box::pin(async move {
                    if let Some(barrier) = barrier {
                        barrier.wait().await;
                    }
                    match tail {
                        Some(tail) => READER_TAIL.scope(tail, inner_fut).await,
                        None => inner_fut.await,
                    }
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

        let barrier = seq.reader_barrier_and_tail(URI).0.expect("writer pending");

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
            seq.reader_barrier_and_tail(URI).0.is_none(),
            "no entry means no waiting"
        );

        let first = seq.issue_writer_ticket(URI);
        drop(first);
        assert!(
            seq.reader_barrier_and_tail(URI).0.is_none(),
            "all tickets done means no waiting"
        );
    }

    #[tokio::test]
    async fn reader_tail_reports_highest_ticket_even_after_completion() {
        let seq = DocumentSequencer::default();
        assert_eq!(
            seq.reader_barrier_and_tail(URI).1,
            None,
            "no writer issued → nothing to wait on"
        );

        let first = seq.issue_writer_ticket(URI);
        let second = seq.issue_writer_ticket(URI);
        assert_eq!(
            seq.reader_barrier_and_tail(URI).1,
            Some(2),
            "tail is the highest issued ticket"
        );

        // Unlike the barrier, the tail is still reported once every writer has
        // completed — the reader waits on the parse watermark, which a completed
        // ticket has already advanced.
        drop(first);
        drop(second);
        let (barrier, tail) = seq.reader_barrier_and_tail(URI);
        assert!(barrier.is_none(), "all writers done → no barrier");
        assert_eq!(
            tail,
            Some(2),
            "but the tail is still observable for the watermark wait"
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
    async fn cancelled_middle_writer_does_not_unblock_successor_prematurely() {
        let seq = DocumentSequencer::default();
        let first = seq.issue_writer_ticket(URI);
        let second = seq.issue_writer_ticket(URI);
        let mut third = seq.issue_writer_ticket(URI);

        // The middle writer's future is dropped while the first is still
        // running: `done` must stay contiguous, so the third writer keeps
        // waiting for the first.
        drop(second);
        {
            let mut third_wait = std::pin::pin!(third.wait_turn());
            assert!(
                pending(&mut third_wait),
                "third writer must keep waiting for the first even after the second is cancelled"
            );
        }

        // First completing cascades through the early-completed second.
        drop(first);
        third.wait_turn().await;
    }

    #[tokio::test]
    async fn finish_close_removes_state_and_wakes_stragglers() {
        let seq = DocumentSequencer::default();
        let close = seq.issue_writer_ticket(URI);
        let barrier = seq.reader_barrier_and_tail(URI).0.expect("close pending");

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

    struct MissingSaveInner;

    impl Service<Request> for MissingSaveInner {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request) -> Self::Future {
            Box::pin(async move {
                if req.method() == "textDocument/didSave" {
                    reclaim_current_writer_sequence();
                }
                Ok(None)
            })
        }
    }

    #[tokio::test]
    async fn missing_save_reclaims_idle_sequence_state() {
        let mut gate = IngressOrderGate::new(MissingSaveInner);

        gate.call(notification("textDocument/didSave", URI))
            .await
            .unwrap();

        assert!(!gate.sequencer.docs.contains_key(URI));
    }

    #[tokio::test]
    async fn late_missing_save_reclaims_state_preserved_by_close() {
        let mut gate = IngressOrderGate::new(MissingSaveInner);

        let close = gate.call(notification("textDocument/didClose", URI));
        let late_save = gate.call(notification("textDocument/didSave", URI));
        close.await.unwrap();
        assert!(
            gate.sequencer.docs.contains_key(URI),
            "close must retain state while the late save ticket is pending"
        );
        late_save.await.unwrap();

        assert!(!gate.sequencer.docs.contains_key(URI));
    }

    #[tokio::test]
    async fn malformed_save_reclaims_sequence_state_through_the_real_handler() {
        let (service, _socket) = tower_lsp_server::LspService::new(crate::lsp::Kakehashi::new);
        let mut gate = IngressOrderGate::new(service);
        // WHATWG `url::Url` accepts and percent-encodes this space, while the
        // typed LSP URI parser rejects it before `did_save_impl` is entered.
        let malformed = "file:///test/a b";

        gate.call(notification("textDocument/didSave", malformed))
            .await
            .unwrap();

        assert!(gate.sequencer.docs.is_empty());
        assert!(classify(&notification("textDocument/didSave", malformed)).is_none());
    }

    #[tokio::test]
    async fn malformed_save_params_do_not_create_sequence_state() {
        let (service, _socket) = tower_lsp_server::LspService::new(crate::lsp::Kakehashi::new);
        let mut gate = IngressOrderGate::new(service);
        let malformed = Request::build("textDocument/didSave")
            .params(serde_json::json!({
                "textDocument": { "uri": URI },
                "text": 1,
            }))
            .finish();

        assert!(classify(&malformed).is_none());
        gate.call(malformed).await.unwrap();

        assert!(gate.sequencer.docs.is_empty());
    }

    #[test]
    fn classify_routes_methods() {
        assert!(matches!(
            classify(&notification("workspace/diagnostic", URI)),
            Some(Role::WorkspaceDiagnostic)
        ));

        let writer = classify(&notification("textDocument/didChange", URI));
        assert!(matches!(writer, Some(Role::Writer { close: false, .. })));

        let close = classify(&notification("textDocument/didClose", URI));
        assert!(matches!(close, Some(Role::Writer { close: true, .. })));

        // didOpen is a non-close writer (#374): it must serialize ahead of a
        // following didChange (so the change sees the inserted+parsed document)
        // and behind a preceding didClose (so a reopen lands after the close).
        let open = classify(&notification("textDocument/didOpen", URI));
        assert!(
            matches!(open, Some(Role::Writer { close: false, .. })),
            "didOpen must be a non-close writer"
        );

        let save = classify(&notification("textDocument/didSave", URI));
        assert!(
            matches!(save, Some(Role::Writer { close: false, .. })),
            "didSave must fence later didChange writers"
        );

        for method in [
            "textDocument/semanticTokens/full",
            "textDocument/semanticTokens/full/delta",
            "textDocument/semanticTokens/range",
            "textDocument/formatting",
            "textDocument/rangeFormatting",
            "textDocument/rename",
            "textDocument/prepareRename",
            "textDocument/codeAction",
            "textDocument/inlayHint",
            "textDocument/documentColor",
            "textDocument/colorPresentation",
            "textDocument/inlineValue",
            "textDocument/selectionRange",
            "textDocument/diagnostic",
            "kakehashi/captures/full",
            "kakehashi/captures/full/delta",
            "kakehashi/captures/range",
            "kakehashi/node",
            "kakehashi/node/text",
            "kakehashi/node/children",
            "kakehashi/node/descendantForPointRange",
        ] {
            let reader = classify(&notification(method, URI));
            assert!(
                matches!(reader, Some(Role::Reader { .. })),
                "{method} must be a reader"
            );
        }

        assert!(
            classify(&notification("textDocument/hover", URI)).is_none(),
            "unrelated methods pass through"
        );
        assert!(
            classify(&Request::build("textDocument/didChange").finish()).is_none(),
            "missing params pass through rather than panic"
        );

        // codeLens/resolve carries no textDocument; the URI comes from the
        // routing envelope (#355). Enveloped lenses gate as readers,
        // unenveloped ones pass through.
        let enveloped = Request::build("codeLens/resolve")
            .params(serde_json::json!({
                "range": { "start": { "line": 0, "character": 0 },
                           "end": { "line": 0, "character": 5 } },
                "data": { "kakehashi": { "host_uri": URI } }
            }))
            .finish();
        let role = classify(&enveloped);
        assert!(
            matches!(role, Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped codeLens/resolve must be a reader keyed by the envelope's host_uri"
        );

        let unenveloped = Request::build("codeLens/resolve")
            .params(serde_json::json!({
                "range": { "start": { "line": 0, "character": 0 },
                           "end": { "line": 0, "character": 5 } },
                "data": { "custom": true }
            }))
            .finish();
        assert!(
            classify(&unenveloped).is_none(),
            "unenveloped codeLens/resolve passes through ungated"
        );

        let enveloped_link = Request::build("documentLink/resolve")
            .params(serde_json::json!({
                "range": { "start": { "line": 0, "character": 0 },
                           "end": { "line": 0, "character": 5 } },
                "data": { "kakehashi": { "host_uri": URI } }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_link), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped documentLink/resolve must be keyed by the envelope's host_uri"
        );

        let enveloped_hint = Request::build("inlayHint/resolve")
            .params(serde_json::json!({
                "position": { "line": 0, "character": 1 },
                "label": ": number",
                "data": { "kakehashi": { "host_uri": URI } }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_hint), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped inlayHint/resolve must be keyed by the envelope's host_uri"
        );

        let enveloped_call = Request::build("callHierarchy/incomingCalls")
            .params(serde_json::json!({
                "item": {
                    "name": "f",
                    "kind": 12,
                    "uri": URI,
                    "range": { "start": { "line": 0, "character": 0 },
                               "end": { "line": 0, "character": 1 } },
                    "selectionRange": { "start": { "line": 0, "character": 0 },
                                        "end": { "line": 0, "character": 1 } },
                    "data": { "kakehashi": { "host_uri": URI } }
                }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_call), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped callHierarchy/incomingCalls must be keyed by item.data host_uri"
        );

        let enveloped_outgoing_call = Request::build("callHierarchy/outgoingCalls")
            .params(serde_json::json!({
                "item": {
                    "data": { "kakehashi": { "host_uri": URI } }
                }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_outgoing_call), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped callHierarchy/outgoingCalls must be keyed by item.data host_uri"
        );

        let unenveloped_call = Request::build("callHierarchy/incomingCalls")
            .params(serde_json::json!({ "item": { "data": { "custom": true } } }))
            .finish();
        assert!(
            classify(&unenveloped_call).is_none(),
            "unenveloped callHierarchy/incomingCalls passes through ungated"
        );

        let enveloped_supertype = Request::build("typeHierarchy/supertypes")
            .params(serde_json::json!({
                "item": { "data": { "kakehashi": { "host_uri": URI } } }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_supertype), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped typeHierarchy/supertypes must be keyed by item.data host_uri"
        );
        let unenveloped_supertype = Request::build("typeHierarchy/supertypes")
            .params(serde_json::json!({ "item": { "data": { "custom": true } } }))
            .finish();
        assert!(
            classify(&unenveloped_supertype).is_none(),
            "unenveloped typeHierarchy/supertypes passes through ungated"
        );

        let enveloped_subtype = Request::build("typeHierarchy/subtypes")
            .params(serde_json::json!({
                "item": { "data": { "kakehashi": { "host_uri": URI } } }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_subtype), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped typeHierarchy/subtypes must be keyed by item.data host_uri"
        );
        let unenveloped_subtype = Request::build("typeHierarchy/subtypes")
            .params(serde_json::json!({ "item": { "data": { "custom": true } } }))
            .finish();
        assert!(
            classify(&unenveloped_subtype).is_none(),
            "unenveloped typeHierarchy/subtypes passes through ungated"
        );

        // codeAction/resolve is classified the same way (#568): the URI comes
        // from the routing envelope, enveloped actions gate as readers.
        let enveloped_action = Request::build("codeAction/resolve")
            .params(serde_json::json!({
                "title": "Fix — ruff",
                "data": { "kakehashi": { "host_uri": URI } }
            }))
            .finish();
        assert!(
            matches!(classify(&enveloped_action), Some(Role::Reader { ref uri }) if uri == URI),
            "enveloped codeAction/resolve must be a reader keyed by the envelope's host_uri"
        );
        let unenveloped_action = Request::build("codeAction/resolve")
            .params(serde_json::json!({ "title": "Fix", "data": { "custom": true } }))
            .finish();
        assert!(
            classify(&unenveloped_action).is_none(),
            "unenveloped codeAction/resolve passes through ungated"
        );
    }

    #[test]
    fn classify_normalizes_uri_spellings() {
        // Url::parse normalizes scheme casing and dot segments — the same
        // normalization the internal document store applies when parsing, so
        // these spellings must share one sequence.
        let a = classify(&notification("textDocument/didChange", "FILE:///tmp/a.md"));
        let b = classify(&notification(
            "textDocument/didChange",
            "file:///tmp/./a.md",
        ));
        let (Some(Role::Writer { uri: uri_a, .. }), Some(Role::Writer { uri: uri_b, .. })) = (a, b)
        else {
            panic!("both must classify as writers");
        };
        assert_eq!(
            uri_a, uri_b,
            "spelling variants of one document must share a sequence"
        );
    }

    /// Inner mock: the call whose method equals `stall_method` parks on a
    /// oneshot; every other call resolves immediately. Each call logs a label
    /// derived from its method, so a test can assert completion order.
    struct MockInner {
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
        stall_method: &'static str,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    /// Map a method to a stable label for completion-order assertions.
    fn mock_label(method: &str) -> &'static str {
        match method {
            "textDocument/didOpen" => "open",
            "textDocument/didChange" => "change",
            "textDocument/didClose" => "close",
            "workspace/didChangeWorkspaceFolders" => "folders",
            _ => "reader",
        }
    }

    impl Service<Request> for MockInner {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request) -> Self::Future {
            let log = Arc::clone(&self.log);
            let label = mock_label(req.method());
            if req.method() == self.stall_method
                && let Some(release) = self.release.take()
            {
                Box::pin(async move {
                    let _ = release.await;
                    log.lock().recover_poison("MockInner::call").push(label);
                    Ok(None)
                })
            } else {
                Box::pin(async move {
                    log.lock().recover_poison("MockInner::call").push(label);
                    Ok(None)
                })
            }
        }
    }

    #[tokio::test]
    async fn gate_runs_reader_only_after_preceding_writer() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didChange",
            release: Some(release_rx),
        });

        // Wire order: didChange, then semanticTokens/full.
        let writer_fut = gate.call(notification("textDocument/didChange", URI));
        let reader_fut = gate.call(notification("textDocument/semanticTokens/full", URI));

        let mut writer = tokio_test::task::spawn(writer_fut);
        let mut reader = tokio_test::task::spawn(reader_fut);

        // The reader must not run while the earlier writer is in flight,
        // regardless of poll order (this is the buffer_unordered reorder).
        assert!(reader.poll().is_pending());
        assert!(writer.poll().is_pending(), "writer stalls on the oneshot");
        assert!(reader.poll().is_pending(), "reader still gated");

        release_tx.send(()).expect("writer is waiting");
        assert!(writer.poll().is_ready());
        assert!(reader.is_woken(), "writer completion must wake the reader");
        assert!(reader.poll().is_ready());

        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["change", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_serializes_workspace_diagnostic_responses() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "workspace/diagnostic",
            release: Some(release_rx),
        });

        let first_fut = gate.call(notification("workspace/diagnostic", URI));
        let second_fut = gate.call(notification("workspace/diagnostic", URI));
        let mut first = tokio_test::task::spawn(first_fut);
        let mut second = tokio_test::task::spawn(second_fut);

        assert!(first.poll().is_pending());
        assert!(
            second.poll().is_pending(),
            "a refresh-induced pull must not complete before the accepted response"
        );
        release_tx.send(()).expect("first pull is waiting");
        assert!(first.poll().is_ready());
        assert!(second.is_woken());
        assert!(second.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["reader", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_workspace_diagnostic_after_preceding_folder_change() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "workspace/didChangeWorkspaceFolders",
            release: Some(release_rx),
        });

        let folders_fut = gate.call(notification("workspace/didChangeWorkspaceFolders", URI));
        let diagnostic_fut = gate.call(notification("workspace/diagnostic", URI));
        let mut folders = tokio_test::task::spawn(folders_fut);
        let mut diagnostic = tokio_test::task::spawn(diagnostic_fut);

        assert!(diagnostic.poll().is_pending());
        assert!(folders.poll().is_pending());
        assert!(diagnostic.poll().is_pending());
        release_tx.send(()).expect("folder change is waiting");
        assert!(folders.poll().is_ready());
        assert!(diagnostic.is_woken());
        assert!(diagnostic.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["folders", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_workspace_diagnostic_after_all_preceding_document_writes() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didChange",
            release: Some(release_rx),
        });

        let writer_fut = gate.call(notification("textDocument/didChange", URI));
        let diagnostic_fut = gate.call(notification("workspace/diagnostic", URI));
        let mut writer = tokio_test::task::spawn(writer_fut);
        let mut diagnostic = tokio_test::task::spawn(diagnostic_fut);

        assert!(diagnostic.poll().is_pending());
        assert!(writer.poll().is_pending());
        assert!(diagnostic.poll().is_pending());
        release_tx.send(()).expect("didChange is waiting");
        assert!(writer.poll().is_ready());
        assert!(diagnostic.is_woken());
        assert!(diagnostic.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["change", "reader"]
        );
    }

    #[tokio::test]
    async fn cancelled_queued_workspace_diagnostic_leaves_the_ingress_queue() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let forwarder = crate::lsp::request_id::CancelForwarder::new(Arc::new(
            crate::lsp::bridge::LanguageServerPool::new(),
        ));
        let inner = crate::lsp::request_id::RequestIdCapture::with_cancel_forwarder(
            MockInner {
                log,
                stall_method: "workspace/diagnostic",
                release: Some(release_rx),
            },
            forwarder.clone(),
        );
        let mut gate = IngressOrderGate::with_cancel_forwarder(inner, forwarder);

        let first_fut = gate.call(
            Request::build("workspace/diagnostic")
                .id(1)
                .params(serde_json::json!({}))
                .finish(),
        );
        let second_fut = gate.call(
            Request::build("workspace/diagnostic")
                .id(2)
                .params(serde_json::json!({}))
                .finish(),
        );
        let mut first = tokio_test::task::spawn(first_fut);
        let mut second = tokio_test::task::spawn(second_fut);
        assert!(first.poll().is_pending());
        assert!(second.poll().is_pending());

        gate.call(
            Request::build("$/cancelRequest")
                .params(serde_json::json!({ "id": 2 }))
                .finish(),
        )
        .await
        .unwrap();

        // Make the predecessor and cancellation ready before polling the
        // queued request again. The biased handoff must retain cancellation.
        release_tx.send(()).expect("first pull is waiting");
        assert!(first.poll().is_ready());
        tokio::task::yield_now().await;

        let mut outcome = None;
        for _ in 0..3 {
            match second.poll() {
                Poll::Ready(ready) => {
                    outcome = Some(ready);
                    break;
                }
                Poll::Pending => tokio::task::yield_now().await,
            }
        }
        let response = outcome
            .expect("the cancelled request must win a simultaneous gate handoff")
            .expect("the ingress service must not fail")
            .expect("the cancelled request must produce an error response");
        assert_eq!(response.id(), &tower_lsp_server::jsonrpc::Id::Number(2));
        assert_eq!(
            response.error().expect("request must fail").code,
            tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled
        );
    }

    #[tokio::test]
    async fn gate_runs_color_presentation_only_after_preceding_change() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didChange",
            release: Some(release_rx),
        });

        let writer_fut = gate.call(notification("textDocument/didChange", URI));
        let presentation_fut = gate.call(notification("textDocument/colorPresentation", URI));
        let mut writer = tokio_test::task::spawn(writer_fut);
        let mut presentation = tokio_test::task::spawn(presentation_fut);

        assert!(presentation.poll().is_pending());
        assert!(
            writer.poll().is_pending(),
            "didChange stalls on the oneshot"
        );
        assert!(
            presentation.poll().is_pending(),
            "colorPresentation must remain behind the preceding didChange"
        );

        release_tx.send(()).expect("didChange is waiting");
        assert!(writer.poll().is_ready());
        assert!(presentation.is_woken());
        assert!(presentation.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["change", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_document_color_only_after_preceding_change() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didChange",
            release: Some(release_rx),
        });

        let writer_fut = gate.call(notification("textDocument/didChange", URI));
        let color_fut = gate.call(notification("textDocument/documentColor", URI));
        let mut writer = tokio_test::task::spawn(writer_fut);
        let mut color = tokio_test::task::spawn(color_fut);

        assert!(color.poll().is_pending());
        assert!(writer.poll().is_pending());
        assert!(color.poll().is_pending());

        release_tx.send(()).expect("didChange is waiting");
        assert!(writer.poll().is_ready());
        assert!(color.is_woken());
        assert!(color.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["change", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_inline_value_only_after_preceding_change() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didChange",
            release: Some(release_rx),
        });

        let writer_fut = gate.call(notification("textDocument/didChange", URI));
        let inline_value_fut = gate.call(notification("textDocument/inlineValue", URI));
        let mut writer = tokio_test::task::spawn(writer_fut);
        let mut inline_value = tokio_test::task::spawn(inline_value_fut);

        assert!(inline_value.poll().is_pending());
        assert!(
            writer.poll().is_pending(),
            "didChange stalls on the oneshot"
        );
        assert!(
            inline_value.poll().is_pending(),
            "inlineValue must remain behind the preceding didChange"
        );

        release_tx.send(()).expect("didChange is waiting");
        assert!(writer.poll().is_ready());
        assert!(inline_value.is_woken());
        assert!(inline_value.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["change", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_selection_range_only_after_preceding_change_or_close() {
        async fn assert_order(writer_method: &'static str, expected: Vec<&'static str>) {
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let mut gate = IngressOrderGate::new(MockInner {
                log: Arc::clone(&log),
                stall_method: writer_method,
                release: Some(release_rx),
            });
            let writer_fut = gate.call(notification(writer_method, URI));
            let selection_fut = gate.call(notification("textDocument/selectionRange", URI));
            let mut writer = tokio_test::task::spawn(writer_fut);
            let mut selection = tokio_test::task::spawn(selection_fut);

            assert!(selection.poll().is_pending());
            assert!(writer.poll().is_pending());
            assert!(selection.poll().is_pending());
            release_tx.send(()).expect("writer is waiting");
            assert!(writer.poll().is_ready());
            assert!(selection.is_woken());
            assert!(selection.poll().is_ready());
            assert_eq!(*log.lock().recover_poison("ingress_order::tests"), expected);
        }

        assert_order("textDocument/didChange", vec!["change", "reader"]).await;
        assert_order("textDocument/didClose", vec!["close", "reader"]).await;
    }

    #[tokio::test]
    async fn gate_runs_node_text_only_after_preceding_close() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didClose",
            release: Some(release_rx),
        });

        let close_fut = gate.call(notification("textDocument/didClose", URI));
        let node_fut = gate.call(notification("kakehashi/node/text", URI));
        let mut close = tokio_test::task::spawn(close_fut);
        let mut node = tokio_test::task::spawn(node_fut);

        assert!(node.poll().is_pending(), "node/text must wait for didClose");
        assert!(close.poll().is_pending(), "didClose stalls on the oneshot");
        assert!(node.poll().is_pending(), "node/text remains gated");
        release_tx.send(()).expect("didClose is waiting");
        assert!(close.poll().is_ready());
        assert!(node.is_woken());
        assert!(node.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["close", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_node_navigation_only_after_preceding_change() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didChange",
            release: Some(release_rx),
        });

        let change_fut = gate.call(notification("textDocument/didChange", URI));
        let node_fut = gate.call(notification("kakehashi/node/children", URI));
        let mut change = tokio_test::task::spawn(change_fut);
        let mut node = tokio_test::task::spawn(node_fut);

        assert!(
            node.poll().is_pending(),
            "node navigation must wait for didChange"
        );
        assert!(
            change.poll().is_pending(),
            "didChange stalls on the oneshot"
        );
        assert!(node.poll().is_pending(), "node navigation remains gated");
        release_tx.send(()).expect("didChange is waiting");
        assert!(change.poll().is_ready());
        assert!(node.is_woken());
        assert!(node.poll().is_ready());
        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["change", "reader"]
        );
    }

    #[tokio::test]
    async fn gate_runs_change_only_after_preceding_open() {
        // open→change race (#374): a didChange first-polled before didOpen
        // misses the document and discards the edit. With didOpen gated as a
        // writer, the change waits for the open's ticket, so it sees the
        // inserted + parsed document.
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didOpen",
            release: Some(release_rx),
        });

        // Wire order: didOpen, then didChange.
        let open_fut = gate.call(notification("textDocument/didOpen", URI));
        let change_fut = gate.call(notification("textDocument/didChange", URI));

        let mut open = tokio_test::task::spawn(open_fut);
        let mut change = tokio_test::task::spawn(change_fut);

        assert!(change.poll().is_pending(), "change gated behind open");
        assert!(open.poll().is_pending(), "open stalls on the oneshot");
        assert!(change.poll().is_pending(), "change still gated");

        release_tx.send(()).expect("open is waiting");
        assert!(open.poll().is_ready());
        assert!(change.is_woken(), "open completion must wake the change");
        assert!(change.poll().is_ready());

        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["open", "change"]
        );
    }

    #[tokio::test]
    async fn gate_runs_reopen_only_after_preceding_close() {
        // close→reopen race (#374): a reopen's insert can land before a gated
        // didClose removes the document, after which the close evicts the
        // reopened doc. With didOpen gated as a writer behind didClose, the
        // reopen runs only after the close completes.
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut gate = IngressOrderGate::new(MockInner {
            log: Arc::clone(&log),
            stall_method: "textDocument/didClose",
            release: Some(release_rx),
        });

        // Wire order: didClose, then didOpen (reopen).
        let close_fut = gate.call(notification("textDocument/didClose", URI));
        let reopen_fut = gate.call(notification("textDocument/didOpen", URI));

        let mut close = tokio_test::task::spawn(close_fut);
        let mut reopen = tokio_test::task::spawn(reopen_fut);

        assert!(reopen.poll().is_pending(), "reopen gated behind close");
        assert!(close.poll().is_pending(), "close stalls on the oneshot");
        assert!(reopen.poll().is_pending(), "reopen still gated");

        release_tx.send(()).expect("close is waiting");
        assert!(close.poll().is_ready());
        assert!(reopen.is_woken(), "close completion must wake the reopen");
        assert!(reopen.poll().is_ready());

        assert_eq!(
            *log.lock().recover_poison("ingress_order::tests"),
            vec!["close", "open"]
        );
    }

    /// Inner mock that records the ingress task-locals **as observed inside the
    /// gated handler future** — the only place they are in scope. This is the
    /// discriminator the behavior-preservation tests cannot give: because the
    /// watermark wait is instantly satisfied in the sync world, an inert
    /// plumbing (task-local never propagating, so the handlers read `None` and
    /// silently skip publish/consume) produces identical observable behavior. A
    /// direct assertion that the handler sees `Some(expected)` is what proves the
    /// plumbing is effective rather than dead scaffolding.
    struct TaskLocalProbe {
        writer_ticket: Arc<std::sync::Mutex<Option<u64>>>,
        reader_tail: Arc<std::sync::Mutex<Option<u64>>>,
    }

    impl Service<Request> for TaskLocalProbe {
        type Response = Option<Response>;
        type Error = std::convert::Infallible;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request) -> Self::Future {
            let writer_ticket = Arc::clone(&self.writer_ticket);
            let reader_tail = Arc::clone(&self.reader_tail);
            // Read the task-locals *inside* the returned future: the gate scopes
            // them around this future when it is awaited, not around `call`.
            Box::pin(async move {
                *writer_ticket.lock().recover_poison("TaskLocalProbe") = current_writer_ticket();
                *reader_tail.lock().recover_poison("TaskLocalProbe") = current_reader_tail();
                Ok(None)
            })
        }
    }

    #[tokio::test]
    async fn gate_propagates_writer_ticket_into_the_handler() {
        let writer_ticket = Arc::new(std::sync::Mutex::new(None));
        let reader_tail = Arc::new(std::sync::Mutex::new(None));
        let mut gate = IngressOrderGate::new(TaskLocalProbe {
            writer_ticket: Arc::clone(&writer_ticket),
            reader_tail: Arc::clone(&reader_tail),
        });

        // The first writer for the document is ticket 1; the handler must see it.
        gate.call(notification("textDocument/didChange", URI))
            .await
            .unwrap();

        assert_eq!(
            *writer_ticket.lock().recover_poison("ingress_order::tests"),
            Some(1),
            "the writer handler must observe its ingress ticket, not None (inert plumbing)"
        );
        assert_eq!(
            *reader_tail.lock().recover_poison("ingress_order::tests"),
            None,
            "a writer handler is not a reader and sees no reader tail"
        );
    }

    #[tokio::test]
    async fn gate_propagates_reader_tail_into_the_handler() {
        let writer_ticket = Arc::new(std::sync::Mutex::new(None));
        let reader_tail = Arc::new(std::sync::Mutex::new(None));
        let mut gate = IngressOrderGate::new(TaskLocalProbe {
            writer_ticket: Arc::clone(&writer_ticket),
            reader_tail: Arc::clone(&reader_tail),
        });

        // A writer issues ticket 1, then a reader follows on the wire: the reader
        // handler must observe tail ticket 1 (so it can wait the watermark to it).
        gate.call(notification("textDocument/didChange", URI))
            .await
            .unwrap();
        gate.call(notification("textDocument/semanticTokens/full", URI))
            .await
            .unwrap();

        assert_eq!(
            *reader_tail.lock().recover_poison("ingress_order::tests"),
            Some(1),
            "the reader handler must observe the tail ticket, not None (inert plumbing)"
        );
        assert_eq!(
            *writer_ticket.lock().recover_poison("ingress_order::tests"),
            None,
            "a reader handler is not a writer and sees no writer ticket"
        );
    }
}
