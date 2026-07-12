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
    pub(crate) async fn run<T, F>(&self, cancel: Option<CancelToken>, work: F) -> Option<T>
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
        self.pool.spawn(move || {
            let queued_for = enqueued.elapsed();
            if queued_for.as_millis() > 100 {
                log::debug!(
                    target: "kakehashi::compute_pool",
                    "work unit waited {}ms in the pool queue",
                    queued_for.as_millis()
                );
            }
            if crate::cancel::is_cancelled(cancel_for_work.as_ref()) {
                return; // dropping tx resolves the awaiter with None
            }
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
                Ok(value) => {
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

    #[tokio::test]
    async fn run_executes_work_and_returns_result() {
        let pool = ComputePool::new();
        let result = pool.run(None, || 42).await;
        assert_eq!(result, Some(42));
    }

    #[tokio::test]
    async fn run_skips_work_when_already_cancelled() {
        let pool = ComputePool::with_threads(1);
        let token = CancelToken::default();
        token.cancel();
        let result = pool.run(Some(token), || 42).await;
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
                pool.run(None, move || {
                    started_tx.send(()).expect("test receiver should wait");
                    release_rx.recv().expect("test should release blocker");
                })
                .await
            })
        };
        started_rx.await.expect("blocker should start");

        let token = CancelToken::default();
        let queued = pool.run(Some(token.clone()), || 42);
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
        let result: Option<i32> = pool.run(None, || panic!("boom")).await;
        assert_eq!(result, None);
        // The pool thread survives the panic and keeps serving work.
        assert_eq!(pool.run(None, || 7).await, Some(7));
    }
}
