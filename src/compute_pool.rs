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
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("kakehashi-compute-{i}"))
            .build()
            .expect("compute pool construction cannot fail: no stack size or start handler is set");
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
        self.pool.spawn(move || {
            if crate::cancel::is_cancelled(cancel.as_ref()) {
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
        rx.await.ok()
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
    async fn run_contains_a_panicking_work_unit() {
        let pool = ComputePool::with_threads(1);
        let result: Option<i32> = pool.run(None, || panic!("boom")).await;
        assert_eq!(result, None);
        // The pool thread survives the panic and keeps serving work.
        assert_eq!(pool.run(None, || 7).await, Some(7));
    }
}
