//! All-strategy fan-in for concurrent bridge requests.
//!
//! [`collect_all()`] collects all successful results from a `JoinSet<io::Result<T>>`,
//! returning `FanInResult<Vec<T>>`.

use std::io;

use tokio::task::JoinSet;

use super::FanInResult;
use crate::lsp::request_id::CancelReceiver;

/// Collects all successful results from a JoinSet of concurrent bridge requests.
///
/// Unlike [`super::first_win::first_win()`] which short-circuits on the first non-empty
/// result, this waits for **all** tasks and returns every successful value.
///
/// Returns:
/// - `Done(vec)` when at least one task succeeds (even if others fail)
/// - `Done(vec![])` when the JoinSet is empty
/// - `NoResult { errors }` when all tasks fail
/// - `Cancelled` when a cancel notification arrives before completion
///
/// # Abort semantics
///
/// Callers MUST call `pool.unregister_all_for_upstream_id()` after this function returns
/// to clean up stale entries in the UpstreamRequestRegistry left by aborted tasks.
pub(crate) async fn collect_all<T: Send + 'static>(
    join_set: &mut JoinSet<io::Result<T>>,
    cancel_rx: Option<CancelReceiver>,
) -> FanInResult<Vec<T>> {
    let mut results: Vec<T> = Vec::new();
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
                Ok(Ok(value)) => results.push(value),
            }
        }
        return if results.is_empty() && errors > 0 {
            FanInResult::NoResult { errors }
        } else {
            FanInResult::Done(results)
        };
    };

    // With cancel support — use tokio::select!
    tokio::pin!(cancel_rx);
    loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                join_set.abort_all();
                return FanInResult::Cancelled;
            }
            result = join_set.join_next() => {
                match result {
                    None => break,
                    Some(Err(join_err)) => {
                        errors += 1;
                        log::warn!("bridge task panicked: {join_err}");
                    }
                    Some(Ok(Err(io_err))) => {
                        errors += 1;
                        log::warn!("bridge request failed: {io_err}");
                    }
                    Some(Ok(Ok(value))) => results.push(value),
                }
            }
        }
    }

    if results.is_empty() && errors > 0 {
        FanInResult::NoResult { errors }
    } else {
        FanInResult::Done(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_done<T: std::fmt::Debug>(result: FanInResult<T>) -> T {
        match result {
            FanInResult::Done(v) => v,
            FanInResult::NoResult { errors } => {
                panic!("expected Done, got NoResult {{ errors: {errors} }}")
            }
            FanInResult::Cancelled => panic!("expected Done, got Cancelled"),
        }
    }

    fn assert_no_result<T: std::fmt::Debug>(result: FanInResult<T>) -> usize {
        match result {
            FanInResult::NoResult { errors } => errors,
            FanInResult::Done(v) => panic!("expected NoResult, got Done({v:?})"),
            FanInResult::Cancelled => panic!("expected NoResult, got Cancelled"),
        }
    }

    fn assert_cancelled<T: std::fmt::Debug>(result: FanInResult<T>) {
        match result {
            FanInResult::Cancelled => {}
            FanInResult::Done(v) => panic!("expected Cancelled, got Done({v:?})"),
            FanInResult::NoResult { .. } => panic!("expected Cancelled, got NoResult"),
        }
    }

    #[tokio::test]
    async fn collect_all_returns_all_successful_results() {
        let mut join_set: JoinSet<io::Result<i32>> = JoinSet::new();
        join_set.spawn(async { Ok(1) });
        join_set.spawn(async { Ok(2) });
        join_set.spawn(async { Ok(3) });

        let result = collect_all(&mut join_set, None).await;
        let mut values = assert_done(result);
        values.sort();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn collect_all_returns_no_result_when_all_fail() {
        let mut join_set: JoinSet<io::Result<i32>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail 1")) });
        join_set.spawn(async { Err(io::Error::other("fail 2")) });
        join_set.spawn(async { Err(io::Error::other("fail 3")) });

        let result = collect_all(&mut join_set, None).await;
        assert_eq!(
            assert_no_result(result),
            3,
            "all 3 tasks should be counted as errors"
        );
    }

    #[tokio::test]
    async fn collect_all_includes_successes_despite_errors() {
        let mut join_set: JoinSet<io::Result<i32>> = JoinSet::new();
        join_set.spawn(async { Ok(42) });
        join_set.spawn(async { Err(io::Error::other("fail 1")) });
        join_set.spawn(async { Err(io::Error::other("fail 2")) });

        let result = collect_all(&mut join_set, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
    }

    #[tokio::test]
    async fn collect_all_returns_done_empty_for_empty_join_set() {
        let mut join_set: JoinSet<io::Result<i32>> = JoinSet::new();

        let result = collect_all(&mut join_set, None).await;
        let values = assert_done(result);
        assert!(
            values.is_empty(),
            "empty JoinSet should produce Done(vec![])"
        );
    }

    #[tokio::test]
    async fn collect_all_returns_cancelled_on_cancel() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<io::Result<i32>> = JoinSet::new();
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            Ok(42)
        });

        tx.send(()).unwrap();

        let result = collect_all(&mut join_set, Some(rx)).await;
        assert_cancelled(result);
    }

    #[tokio::test]
    async fn collect_all_returns_done_before_cancel() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<io::Result<i32>> = JoinSet::new();
        join_set.spawn(async { Ok(42) });

        let result = collect_all(&mut join_set, Some(rx)).await;
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
    }
}
