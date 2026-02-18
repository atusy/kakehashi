//! First-win collection strategy for concurrent bridge requests.
//!
//! [`first_win()`] collects a `JoinSet`, returning the first non-empty success.

use std::io;

use tokio::task::JoinSet;

use crate::lsp::request_id::CancelReceiver;

pub(super) use crate::lsp::aggregation::fan_out::fan_out;

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

impl<T> FirstWinResult<T> {
    /// Handle the common post-dispatch pattern for first-win results.
    ///
    /// - `Winner`: calls `on_winner` to transform the value into the handler's return type.
    /// - `NoWinner`: logs at WARNING (if errors > 0) or LOG (if errors == 0),
    ///   then returns `Ok(no_result)`.
    /// - `Cancelled`: returns `Err(Error::request_cancelled())`.
    pub(super) async fn handle<R>(
        self,
        client: &tower_lsp_server::Client,
        method_name: &str,
        no_result: R,
        on_winner: impl FnOnce(T) -> tower_lsp_server::jsonrpc::Result<R>,
    ) -> tower_lsp_server::jsonrpc::Result<R> {
        match self {
            FirstWinResult::Winner(value) => on_winner(value),
            FirstWinResult::NoWinner { errors } => {
                let level = if errors > 0 {
                    tower_lsp_server::ls_types::MessageType::WARNING
                } else {
                    tower_lsp_server::ls_types::MessageType::LOG
                };
                client
                    .log_message(
                        level,
                        format!("No {method_name} response from any bridge server"),
                    )
                    .await;
                Ok(no_result)
            }
            FirstWinResult::Cancelled => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
        }
    }
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
            // `_` intentionally matches both `Ok(())` (real $/cancelRequest)
            // and `Err(RecvError)` (sender half dropped). The cancel guard
            // (held by the caller) always outlives `first_win()`, so in practice
            // only `Ok` is received here. Matching both avoids a spurious panic
            // if that invariant ever changes.
            _ = &mut cancel_rx => {
                // abort_all() cancels in-flight tasks. Per-connection ResponseRouter
                // entries are cleaned up by the RouterCleanupGuard in
                // execute_bridge_request_with_handle (Drop runs on abort).
                // Pool-level upstream_request_registry entries are cleaned up by
                // the caller's unregister_all_for_upstream_id() call after this
                // function returns.
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

    /// Empty JoinSet without cancel → returns NoWinner with zero errors.
    #[tokio::test]
    async fn first_win_returns_no_winner_for_empty_join_set() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();

        let result = first_win(&mut join_set, |opt| opt.is_some(), None).await;

        assert_eq!(
            assert_no_winner(result),
            0,
            "empty JoinSet should produce NoWinner with zero errors"
        );
    }

    /// Empty JoinSet with cancel_rx → returns NoWinner (not Cancelled).
    #[tokio::test]
    async fn first_win_returns_no_winner_for_empty_join_set_with_cancel() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();

        let result = first_win(&mut join_set, |opt| opt.is_some(), Some(rx)).await;

        assert_eq!(
            assert_no_winner(result),
            0,
            "empty JoinSet with cancel should produce NoWinner with zero errors"
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
