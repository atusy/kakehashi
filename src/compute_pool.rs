//! Bounded compute pool for all synchronous tree-CPU.
//!
//! The parse-snapshot architecture (see
//! `docs/architecture-decisions/parse-snapshot-architecture.md` §4) requires
//! that no tree-sitter CPU — parsing, injection populate, captures/node walks,
//! or the semantic-token fan-out — ever executes on a tokio worker thread:
//! inline bursts saturate the async runtime, defeating its timer wheel and
//! starving unrelated documents' handlers. This module provides the single
//! dedicated Rayon pool those work-units run on, bridged back to async callers
//! by a `oneshot`.

use crate::cancel::CancelToken;

const TRACE_TARGET: &str = "kakehashi::compute_pool";
static NEXT_WORK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct DocumentIdentity {
    uri: String,
    incarnation: Option<u64>,
    content_version: Option<u64>,
}

struct ComputeWorkTrace {
    kind: &'static str,
    identity: Option<DocumentIdentity>,
}

/// Attribution attached to one bounded-pool work unit.
///
/// Document identity is materialized only while debug logging for the compute
/// target is enabled. The production fast path is a pointer-sized `None` and
/// does not carry the kind or allocate a URI string.
pub(crate) struct ComputeWork(Option<Box<ComputeWorkTrace>>);

impl ComputeWork {
    #[cfg(test)]
    pub(crate) fn anonymous(kind: &'static str) -> Self {
        Self(
            log::log_enabled!(target: TRACE_TARGET, log::Level::Debug).then(|| {
                Box::new(ComputeWorkTrace {
                    kind,
                    identity: None,
                })
            }),
        )
    }

    pub(crate) fn document(
        kind: &'static str,
        uri: &url::Url,
        incarnation: Option<u64>,
        content_version: Option<u64>,
    ) -> Self {
        Self(
            log::log_enabled!(target: TRACE_TARGET, log::Level::Debug).then(|| {
                Box::new(ComputeWorkTrace {
                    kind,
                    identity: Some(DocumentIdentity {
                        uri: uri.to_string(),
                        incarnation,
                        content_version,
                    }),
                })
            }),
        )
    }

    #[cfg(test)]
    fn enabled_for_test(
        kind: &'static str,
        uri: &str,
        incarnation: Option<u64>,
        content_version: Option<u64>,
    ) -> Self {
        Self(Some(Box::new(ComputeWorkTrace {
            kind,
            identity: Some(DocumentIdentity {
                uri: uri.to_owned(),
                incarnation,
                content_version,
            }),
        })))
    }
}

struct ComputeEvent<'a> {
    work_id: u64,
    work: &'a ComputeWorkTrace,
    event: &'static str,
    queue_us: u128,
    run_us: Option<u128>,
    elapsed_us: u128,
    cancelled: bool,
}

impl std::fmt::Display for ComputeEvent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let identity = self.work.identity.as_ref();
        write!(
            f,
            "work_id={} kind={} uri={} incarnation={} content_version={} event={} queue_us={} run_us={} elapsed_us={} cancelled={}",
            self.work_id,
            self.work.kind,
            identity.map_or("-", |identity| identity.uri.as_str()),
            identity
                .and_then(|identity| identity.incarnation)
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            identity
                .and_then(|identity| identity.content_version)
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            self.event,
            self.queue_us,
            self.run_us
                .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            self.elapsed_us,
            self.cancelled,
        )
    }
}

/// The dedicated Rayon pool all synchronous tree-CPU runs on.
///
/// Sized strictly below `available_parallelism` (reserving cores for the tokio
/// workers + timer driver) so tree work can never occupy every core.
pub(crate) struct ComputePool {
    pool: rayon::ThreadPool,
}

impl ComputePool {
    pub(crate) fn thread_count(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Build the pool at the ADR-specified size:
    /// `available_parallelism - 2`, at least 1.
    pub(crate) fn new() -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .saturating_sub(2)
            .max(1);
        Self::with_threads(threads)
    }

    fn with_threads(threads: usize) -> Self {
        // `build()` fails only when the OS refuses to spawn the worker
        // threads (resource exhaustion at startup) — the server cannot run
        // without its compute pool, so aborting is the correct response.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("kakehashi-compute-{i}"))
            .build()
            .expect("OS refused to spawn compute-pool threads at startup");
        Self { pool }
    }

    /// Run `work` on the pool and await its result from async context.
    ///
    /// `cancel` is the work-unit cancellation hook: a token already cancelled
    /// when the work-unit reaches the front of the queue skips the work
    /// entirely (a superseded compute must not burn a pool thread just because
    /// it was queued before its supersession). Long-running work polls the same
    /// token internally at its own checkpoints.
    ///
    /// Returns `None` if the work was skipped by cancellation or panicked
    /// (the panic is contained to the work-unit and logged by the caller side
    /// observing `None`).
    pub(crate) async fn run<T, F>(
        &self,
        attribution: ComputeWork,
        cancel: Option<CancelToken>,
        work: F,
    ) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cancel_for_work = cancel.clone();
        // Scheduling-latency instrumentation: a slow response with a fast
        // compute means the time went to the pool QUEUE (enqueue→start: pool
        // threads busy with other work-units) or to the RESUME (work end→
        // awaiter progress: the tokio runtime not scheduling the handler) —
        // exactly the split a user-supplied debug log needs to attribute a
        // stall (see the 20s-response investigation).
        let enqueued = std::time::Instant::now();
        let work_id = attribution
            .0
            .as_ref()
            .map(|_| NEXT_WORK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        self.pool.spawn(move || {
            let queued_for = enqueued.elapsed();
            if let (Some(work_id), Some(work)) = (work_id, attribution.0.as_deref()) {
                log::debug!(
                    target: TRACE_TARGET,
                    "{}",
                    ComputeEvent {
                        work_id,
                        work,
                        event: "started",
                        queue_us: queued_for.as_micros(),
                        run_us: None,
                        elapsed_us: enqueued.elapsed().as_micros(),
                        cancelled: crate::cancel::is_cancelled(cancel_for_work.as_ref()),
                    }
                );
            }
            if crate::cancel::is_cancelled(cancel_for_work.as_ref()) {
                if let (Some(work_id), Some(work)) = (work_id, attribution.0.as_deref()) {
                    log::debug!(
                        target: TRACE_TARGET,
                        "{}",
                        ComputeEvent {
                            work_id,
                            work,
                            event: "skipped",
                            queue_us: queued_for.as_micros(),
                            run_us: Some(0),
                            elapsed_us: enqueued.elapsed().as_micros(),
                            cancelled: true,
                        }
                    );
                }
                return; // dropping tx resolves the awaiter with None
            }
            // Avoid a second clock read on the production fast path. Queue
            // timing already existed before attribution; execution timing is
            // sampled only when this debug target is enabled.
            let started = work_id.map(|_| std::time::Instant::now());
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            let run_us = started.map(|started| started.elapsed().as_micros());
            match outcome {
                Ok(value) => {
                    if let (Some(work_id), Some(work)) = (work_id, attribution.0.as_deref()) {
                        log::debug!(
                            target: TRACE_TARGET,
                            "{}",
                            ComputeEvent {
                                work_id,
                                work,
                                event: "finished",
                                queue_us: queued_for.as_micros(),
                                run_us,
                                elapsed_us: enqueued.elapsed().as_micros(),
                                cancelled: crate::cancel::is_cancelled(cancel_for_work.as_ref()),
                            }
                        );
                    }
                    let _ = tx.send(value);
                }
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    log::error!(
                        target: "kakehashi::crash_recovery",
                        "compute-pool work unit panicked: {msg}"
                    );
                    if let (Some(work_id), Some(work)) = (work_id, attribution.0.as_deref()) {
                        log::debug!(
                            target: TRACE_TARGET,
                            "{}",
                            ComputeEvent {
                                work_id,
                                work,
                                event: "panicked",
                                queue_us: queued_for.as_micros(),
                                run_us,
                                elapsed_us: enqueued.elapsed().as_micros(),
                                cancelled: crate::cancel::is_cancelled(cancel_for_work.as_ref()),
                            }
                        );
                    }
                }
            }
        });
        // The oneshot resolves the moment the work-unit sends. The other
        // latency half — "work done" to "handler resumed" — cannot be
        // measured from inside this future; the runtime-stall watchdog (see
        // `run_lsp_server`) covers it.
        match cancel {
            Some(cancel) => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    result = rx => result.ok(),
                }
            }
            None => rx.await.ok(),
        }
    }
}

/// Shared pool for tests: building a fresh thread pool per test is wasteful
/// and can exhaust process threads under the full suite's parallelism.
#[cfg(test)]
pub(crate) fn test_pool() -> std::sync::Arc<ComputePool> {
    static POOL: std::sync::OnceLock<std::sync::Arc<ComputePool>> = std::sync::OnceLock::new();
    std::sync::Arc::clone(POOL.get_or_init(|| std::sync::Arc::new(ComputePool::new())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_terminal_event(logs: &[String], terminal: &str, cancelled: &str) {
        let events = logs
            .iter()
            .map(|line| {
                line.split_whitespace()
                    .filter_map(|field| field.split_once('='))
                    .collect::<std::collections::HashMap<_, _>>()
            })
            .filter(|event| event.get("kind") == Some(&"test_lifecycle"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2, "{logs:?}");
        let started = events
            .iter()
            .find(|event| event.get("event") == Some(&"started"))
            .expect("started event");
        let finished = events
            .iter()
            .find(|event| event.get("event") == Some(&terminal))
            .unwrap_or_else(|| panic!("missing {terminal} event: {logs:?}"));
        assert_eq!(started.get("work_id"), finished.get("work_id"));
        for event in [started, finished] {
            assert_eq!(event.get("uri"), Some(&"file:///workspace/main.rs"));
            assert_eq!(event.get("incarnation"), Some(&"7"));
            assert_eq!(event.get("content_version"), Some(&"42"));
            assert_eq!(event.get("cancelled"), Some(&cancelled));
            assert!(event["queue_us"].parse::<u128>().is_ok(), "{event:?}");
            assert!(event["elapsed_us"].parse::<u128>().is_ok(), "{event:?}");
        }
        assert!(finished["run_us"].parse::<u128>().is_ok(), "{finished:?}");
    }

    fn capture_lifecycle(run: impl FnOnce(ComputePool, ComputeWork) + Send) -> Vec<String> {
        crate::lsp::test_logging::captured_logs_for(TRACE_TARGET, log::Level::Debug, || {
            let work = ComputeWork::document(
                "test_lifecycle",
                &url::Url::parse("file:///workspace/main.rs").expect("test URI"),
                Some(7),
                Some(42),
            );
            run(ComputePool::with_threads(1), work);
        })
    }

    #[test]
    fn attributed_event_keeps_document_identity_and_phase_timings() {
        let work =
            ComputeWork::enabled_for_test("parse", "file:///workspace/main.rs", Some(7), Some(42));
        let event = ComputeEvent {
            work_id: 3,
            work: work.0.as_deref().expect("enabled test work"),
            event: "finished",
            queue_us: 11,
            run_us: Some(23),
            elapsed_us: 34,
            cancelled: true,
        };

        assert_eq!(
            event.to_string(),
            "work_id=3 kind=parse uri=file:///workspace/main.rs incarnation=7 content_version=42 event=finished queue_us=11 run_us=23 elapsed_us=34 cancelled=true"
        );
    }

    #[test]
    fn disabled_attribution_is_pointer_sized() {
        assert_eq!(
            std::mem::size_of::<ComputeWork>(),
            std::mem::size_of::<usize>()
        );
    }

    #[tokio::test]
    async fn run_executes_work_and_returns_result() {
        let pool = ComputePool::new();
        let result = pool.run(ComputeWork::anonymous("test"), None, || 42).await;
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn run_skips_work_when_already_cancelled() {
        let pool = ComputePool::with_threads(1);
        let token = CancelToken::default();
        token.cancel();
        let result = pool
            .run(ComputeWork::anonymous("test"), Some(token), || 42)
            .await;
        assert_eq!(result, None, "cancelled work-unit must be skipped");
    }

    #[tokio::test]
    async fn run_releases_queued_awaiter_when_cancelled() {
        let pool = std::sync::Arc::new(ComputePool::with_threads(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = {
            let pool = std::sync::Arc::clone(&pool);
            tokio::spawn(async move {
                pool.run(ComputeWork::anonymous("test"), None, move || {
                    started_tx.send(()).expect("test receiver should wait");
                    release_rx.recv().expect("test should release blocker");
                })
                .await
            })
        };
        started_rx.await.expect("blocker should start");

        let token = CancelToken::default();
        let queued = pool.run(ComputeWork::anonymous("test"), Some(token.clone()), || 42);
        token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), queued)
            .await
            .expect("queued awaiter should release on cancellation");
        assert_eq!(result, None);

        release_tx
            .send(())
            .expect("blocker should still be waiting");
        blocker.await.expect("blocker task should not panic");
    }

    #[tokio::test]
    async fn run_contains_a_panicking_work_unit() {
        let pool = ComputePool::with_threads(1);
        let result: Option<i32> = pool
            .run(ComputeWork::anonymous("test"), None, || panic!("boom"))
            .await;
        assert_eq!(result, None);
        // The pool thread survives the panic and keeps serving work.
        assert_eq!(
            pool.run(ComputeWork::anonymous("test"), None, || 7).await,
            Some(7)
        );
    }

    #[test]
    fn skipped_work_emits_one_joinable_terminal_event() {
        let logs = capture_lifecycle(|pool, work| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                let token = CancelToken::default();
                token.cancel();
                assert_eq!(pool.run(work, Some(token), || 42).await, None);
                // `run` resolves immediately from the cancellation branch;
                // a sentinel on this one-thread pool keeps capture active
                // until the earlier work emits `skipped`.
                assert_eq!(
                    pool.run(ComputeWork::anonymous("sentinel"), None, || ())
                        .await,
                    Some(())
                );
            });
        });
        assert_terminal_event(&logs, "skipped", "true");
    }

    #[test]
    fn panicking_work_emits_one_joinable_terminal_event() {
        let logs = capture_lifecycle(|pool, work| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(async {
                let result: Option<()> = pool.run(work, None, || panic!("boom")).await;
                assert_eq!(result, None);
            });
        });
        assert_terminal_event(&logs, "panicked", "false");
    }
}
