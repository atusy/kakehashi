//! Fan-out / first-win utilities for concurrent bridge requests.
//!
//! Two helpers work together:
//! - [`fan_out()`] spawns one task per matching server, cloning shared context
//! - [`first_win()`] collects the `JoinSet`, returning the first non-empty success

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::config::settings::BridgeServerConfig;
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::bridge::UpstreamId;
use crate::lsp::lsp_impl::bridge_context::MultiBridgeRequestContext;
use crate::lsp::request_id::CancelReceiver;

/// Per-server arguments produced by [`fan_out()`].
///
/// Each spawned task receives its own clone of the shared context fields
/// plus the server-specific name and config. Handlers pass this struct
/// into their pool `send_*_request` call.
pub(super) struct FanOutTask {
    pub(super) pool: Arc<LanguageServerPool>,
    pub(super) server_name: String,
    pub(super) server_config: Arc<BridgeServerConfig>,
    pub(super) uri: url::Url,
    pub(super) injection_language: String,
    pub(super) region_id: String,
    pub(super) region_start_line: u32,
    pub(super) virtual_content: String,
    pub(super) upstream_id: UpstreamId,
}

/// Spawn one task per matching server, returning a `JoinSet` for `first_win()`.
///
/// Centralises the per-server clone boilerplate that was previously duplicated
/// in every fan-out handler. The caller supplies a closure that receives a
/// [`FanOutTask`] and returns the handler-specific future.
pub(super) fn fan_out<T, F, Fut>(
    ctx: &MultiBridgeRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
) -> JoinSet<io::Result<T>>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let mut join_set = JoinSet::new();
    for config in &ctx.configs {
        let task = FanOutTask {
            pool: Arc::clone(&pool),
            server_name: config.server_name.clone(),
            server_config: Arc::clone(&config.config),
            uri: ctx.uri.clone(),
            injection_language: ctx.resolved.injection_language.clone(),
            region_id: ctx.resolved.region.region_id.clone(),
            region_start_line: ctx.resolved.region.line_range.start,
            virtual_content: ctx.resolved.virtual_content.clone(),
            upstream_id: ctx.upstream_request_id.clone(),
        };
        join_set.spawn(f(task));
    }
    join_set
}

/// Result of [`first_win()`] dispatch.
#[derive(Debug)]
#[must_use]
pub(super) enum FirstWinResult<T> {
    /// A non-empty response was received from a downstream server.
    Winner(T),
    /// All tasks completed without producing a non-empty response.
    ///
    /// `errors` counts tasks that failed with panics (`JoinError`) or I/O errors.
    /// Handlers use this to choose log severity: `WARNING` when `errors > 0`
    /// (real failures), `LOG` when `errors == 0` (all servers returned empty — normal).
    NoWinner {
        /// Number of tasks that failed with errors (panics or IO errors).
        errors: usize,
    },
    /// The upstream client cancelled the request via `$/cancelRequest`.
    Cancelled,
}

/// Returns the first non-empty successful result from a JoinSet of concurrent bridge requests.
///
/// # Cancel subscription timing
///
/// Handlers subscribe to `$/cancelRequest` **after** `resolve_bridge_contexts()` completes.
/// If a cancel arrives during the preamble (URI resolution, snapshot, injection detection),
/// it is lost. This is a best-effort window — acceptable because the preamble is fast and
/// cancelling during that phase would be a no-op anyway (no downstream work has started).
///
/// Iterates through completed futures in arrival order. Returns the first result where:
/// - The task didn't panic (`JoinError`)
/// - The bridge request didn't fail (`io::Error`)
/// - The response passes the `is_nonempty` predicate
///
/// On success, aborts remaining in-flight tasks and returns the winning value.
/// Returns `NoWinner` if all tasks fail, error, or produce empty results.
/// Returns `Cancelled` if a cancel notification arrives before a winner is found.
///
/// # Abort semantics
///
/// Callers MUST call `pool.unregister_all_for_upstream_id()` after this function returns
/// to clean up stale entries in the UpstreamRequestRegistry left by aborted losers.
pub(super) async fn first_win<T: Send + 'static>(
    join_set: &mut JoinSet<io::Result<T>>,
    is_nonempty: impl Fn(&T) -> bool,
    cancel_rx: Option<CancelReceiver>,
) -> FirstWinResult<T> {
    let mut errors: usize = 0;

    // No cancel support — simple loop
    let Some(cancel_rx) = cancel_rx else {
        while let Some(result) = join_set.join_next().await {
            match result {
                Err(join_err) => {
                    errors += 1;
                    log::warn!("bridge task panicked: {join_err}");
                }
                Ok(Err(io_err)) => {
                    errors += 1;
                    log::warn!("bridge request failed: {io_err}");
                }
                Ok(Ok(value)) if is_nonempty(&value) => {
                    join_set.abort_all();
                    return FirstWinResult::Winner(value);
                }
                Ok(Ok(_)) => {} // empty — try next
            }
        }
        return FirstWinResult::NoWinner { errors };
    };

    // With cancel support — use tokio::select!
    tokio::pin!(cancel_rx);
    loop {
        tokio::select! {
            // `biased;` ensures cancel is checked before task results on each poll.
            // If both a cancel and a ready result arrive in the same poll cycle, cancel
            // wins and the valid result is discarded. This is correct for LSP semantics:
            // the editor has already moved on, so returning a stale result is wasteful.
            biased;
            _ = &mut cancel_rx => {
                // abort_all() cancels in-flight tasks but does NOT clean up
                // per-connection ResponseRouter entries. Those orphaned entries
                // persist until the downstream response arrives (the oneshot send
                // fails silently) or the connection drops. This is an acceptable
                // trade-off: cleaning them eagerly would require iterating all
                // connections, and the silent failure path is already handled.
                join_set.abort_all();
                return FirstWinResult::Cancelled;
            }
            result = join_set.join_next() => {
                match result {
                    None => return FirstWinResult::NoWinner { errors },
                    Some(Err(join_err)) => {
                        errors += 1;
                        log::warn!("bridge task panicked: {join_err}");
                    }
                    Some(Ok(Err(io_err))) => {
                        errors += 1;
                        log::warn!("bridge request failed: {io_err}");
                    }
                    Some(Ok(Ok(value))) if is_nonempty(&value) => {
                        join_set.abort_all();
                        return FirstWinResult::Winner(value);
                    }
                    Some(Ok(Ok(_))) => {} // empty — try next
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to assert Winner variant and extract the value.
    fn assert_winner<T: std::fmt::Debug>(result: FirstWinResult<T>) -> T {
        match result {
            FirstWinResult::Winner(v) => v,
            FirstWinResult::NoWinner { errors } => {
                panic!("expected Winner, got NoWinner {{ errors: {errors} }}")
            }
            FirstWinResult::Cancelled => panic!("expected Winner, got Cancelled"),
        }
    }

    /// Helper to assert NoWinner variant and return the error count.
    fn assert_no_winner<T: std::fmt::Debug>(result: FirstWinResult<T>) -> usize {
        match result {
            FirstWinResult::NoWinner { errors } => errors,
            FirstWinResult::Winner(v) => panic!("expected NoWinner, got Winner({v:?})"),
            FirstWinResult::Cancelled => panic!("expected NoWinner, got Cancelled"),
        }
    }

    /// Helper to assert Cancelled variant.
    fn assert_cancelled<T: std::fmt::Debug>(result: FirstWinResult<T>) {
        match result {
            FirstWinResult::Cancelled => {}
            FirstWinResult::Winner(v) => panic!("expected Cancelled, got Winner({v:?})"),
            FirstWinResult::NoWinner { .. } => panic!("expected Cancelled, got NoWinner"),
        }
    }

    // ============================================================
    // Tests without cancel (cancel_rx = None)
    // ============================================================

    /// first_win returns the first non-empty result, skipping errors.
    #[tokio::test]
    async fn first_win_returns_first_nonempty_result() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail")) });
        join_set.spawn(async { Ok(None) });
        join_set.spawn(async { Ok(Some(42)) });

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(assert_winner(result), Some(42));
    }

    /// first_win returns NoWinner with error count when all tasks return errors.
    #[tokio::test]
    async fn first_win_returns_no_winner_when_all_fail() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail 1")) });
        join_set.spawn(async { Err(io::Error::other("fail 2")) });
        join_set.spawn(async { Err(io::Error::other("fail 3")) });

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(
            assert_no_winner(result),
            3,
            "all 3 tasks should be counted as errors"
        );
    }

    /// first_win returns NoWinner with zero errors when all tasks return empty results.
    #[tokio::test]
    async fn first_win_returns_no_winner_when_all_empty() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Ok(None) });
        join_set.spawn(async { Ok(None) });
        join_set.spawn(async { Ok(None) });

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(assert_no_winner(result), 0, "empty results are not errors");
    }

    /// first_win skips errors and returns a later success.
    #[tokio::test]
    async fn first_win_skips_errors_and_returns_later_success() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail 1")) });
        join_set.spawn(async { Err(io::Error::other("fail 2")) });
        join_set.spawn(async { Ok(Some(42)) });

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(assert_winner(result), Some(42));
    }

    /// first_win uses the is_nonempty predicate to filter results.
    #[tokio::test]
    async fn first_win_uses_is_nonempty_predicate() {
        let mut join_set: JoinSet<io::Result<Option<Vec<i32>>>> = JoinSet::new();
        join_set.spawn(async { Ok(Some(vec![])) }); // empty vec — should be skipped
        join_set.spawn(async { Ok(Some(vec![1])) }); // non-empty — should win

        let result = first_win(
            &mut join_set,
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            None,
        )
        .await;

        assert_eq!(assert_winner(result), Some(vec![1]));
    }

    // ============================================================
    // Tests with cancel (cancel_rx = Some)
    // ============================================================

    /// Cancel fires before any result → returns Cancelled.
    #[tokio::test]
    async fn first_win_returns_cancelled_on_cancel() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        // Task that will never complete (blocked until cancelled)
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(Some(42))
        });

        // Fire cancel immediately
        tx.send(()).unwrap();

        let result = first_win(&mut join_set, |opt| opt.is_some(), Some(rx)).await;

        assert_cancelled(result);
    }

    /// Winner arrives before cancel → returns Winner.
    #[tokio::test]
    async fn first_win_returns_winner_before_cancel() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        // Task that completes immediately
        join_set.spawn(async { Ok(Some(42)) });

        let result = first_win(&mut join_set, |opt| opt.is_some(), Some(rx)).await;

        assert_eq!(assert_winner(result), Some(42));
    }

    /// All tasks fail with cancel_rx provided → returns NoWinner (not Cancelled).
    #[tokio::test]
    async fn first_win_returns_no_winner_with_unused_cancel() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail")) });

        let result = first_win(&mut join_set, |opt| opt.is_some(), Some(rx)).await;

        assert_eq!(assert_no_winner(result), 1);
    }

    // ============================================================
    // Tests for JoinError (task panics)
    // ============================================================

    /// All tasks panic → returns NoWinner with error count (not a panic itself).
    #[tokio::test]
    async fn first_win_returns_no_winner_when_all_tasks_panic() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { panic!("task 1 panicked") });
        join_set.spawn(async { panic!("task 2 panicked") });

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(
            assert_no_winner(result),
            2,
            "both panics should be counted as errors"
        );
    }

    /// Mix of panics and success → the success still wins.
    #[tokio::test]
    async fn first_win_returns_winner_despite_panics() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { panic!("task 1 panicked") });
        join_set.spawn(async { Ok(Some(42)) });
        join_set.spawn(async { panic!("task 3 panicked") });

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(assert_winner(result), Some(42));
    }
}
