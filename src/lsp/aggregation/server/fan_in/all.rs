//! All-strategy fan-in for concurrent bridge requests.
//!
//! [`collect_all()`] collects all successful results from a `JoinSet<TaggedResult<T>>`,
//! returning `FanInResult<Vec<T>>`. When `priorities` is non-empty, results are
//! ordered by priority, with unlisted servers appended in insertion order.

use indexmap::IndexMap;
use tokio::task::JoinSet;

use super::FanInResult;
use crate::lsp::aggregation::server::fan_out::TaggedResult;
use crate::lsp::request_id::CancelReceiver;

/// Reorder tagged results according to the priority list.
///
/// Walks `priorities` in order, draining matching entries from the map.
/// Remaining (unlisted) entries are appended in insertion order — consistent
/// with the `IndexMap` convention used in `preferred.rs`.
fn order_by_priority<T>(tagged: Vec<(String, T)>, priorities: &[String]) -> Vec<T> {
    if priorities.is_empty() {
        return tagged.into_iter().map(|(_, v)| v).collect();
    }

    let capacity = tagged.len();
    let mut map: IndexMap<String, Vec<T>> = IndexMap::new();
    for (name, value) in tagged {
        map.entry(name).or_default().push(value);
    }

    let mut ordered = Vec::with_capacity(capacity);
    for name in priorities {
        if let Some(values) = map.shift_remove(name) {
            ordered.extend(values);
        }
    }
    // Append remaining (unlisted) entries in insertion order
    for (_name, values) in map {
        ordered.extend(values);
    }
    ordered
}

/// Collects all successful results from a JoinSet of concurrent bridge requests.
///
/// Unlike [`super::first_win::first_win()`] which short-circuits on the first non-empty
/// result, this waits for **all** tasks and returns every successful value.
///
/// When `priorities` is non-empty, results are ordered by priority (highest first),
/// with unlisted servers appended in insertion (arrival) order.
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
    join_set: &mut JoinSet<TaggedResult<T>>,
    priorities: &[String],
    cancel_rx: Option<CancelReceiver>,
    log_target: Option<&str>,
) -> FanInResult<Vec<T>> {
    let log_target = log_target.unwrap_or(module_path!());
    let mut results: Vec<(String, T)> = Vec::new();
    let mut errors: usize = 0;

    // When no cancel receiver is provided, create one that never fires.
    // Keeping _tx alive prevents the receiver from resolving.
    let (_tx, fallback_rx) = tokio::sync::oneshot::channel::<()>();
    let cancel_rx = cancel_rx.unwrap_or(fallback_rx);
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
                        log::warn!(target: log_target, "bridge task panicked: {join_err}");
                    }
                    Some(Ok(tagged)) => match tagged.value {
                        Err(io_err) => {
                            errors += 1;
                            log::warn!(target: log_target, "bridge request failed ({}): {io_err}", tagged.server_name);
                        }
                        Ok(value) => results.push((tagged.server_name, value)),
                    },
                }
            }
        }
    }

    if results.is_empty() && errors > 0 {
        FanInResult::NoResult { errors }
    } else {
        FanInResult::Done(order_by_priority(results, priorities))
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::lsp::aggregation::server::fan_in::test_helpers::*;

    #[tokio::test]
    async fn collect_all_returns_all_successful_results() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(1));
        spawn_tagged(&mut join_set, Ok(2));
        spawn_tagged(&mut join_set, Ok(3));

        let result = collect_all(&mut join_set, &[], None, None).await;
        let mut values = assert_done(result);
        values.sort();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn collect_all_returns_no_result_when_all_fail() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 1")));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 2")));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 3")));

        let result = collect_all(&mut join_set, &[], None, None).await;
        assert_eq!(
            assert_no_result(result),
            3,
            "all 3 tasks should be counted as errors"
        );
    }

    #[tokio::test]
    async fn collect_all_includes_successes_despite_errors() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(42));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 1")));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 2")));

        let result = collect_all(&mut join_set, &[], None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
    }

    #[tokio::test]
    async fn collect_all_returns_done_empty_for_empty_join_set() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();

        let result = collect_all(&mut join_set, &[], None, None).await;
        let values = assert_done(result);
        assert!(
            values.is_empty(),
            "empty JoinSet should produce Done(vec![])"
        );
    }

    #[tokio::test]
    async fn collect_all_returns_cancelled_on_cancel() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            TaggedResult {
                server_name: "slow".to_string(),
                value: Ok(42),
            }
        });

        tx.send(()).unwrap();

        let result = collect_all(&mut join_set, &[], Some(rx), None).await;
        assert_cancelled(result);
    }

    #[tokio::test]
    async fn collect_all_returns_done_before_cancel() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(42));

        let result = collect_all(&mut join_set, &[], Some(rx), None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
    }

    #[tokio::test]
    async fn collect_all_orders_results_by_priority() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_c", Ok(3));
        spawn_tagged_named(&mut join_set, "server_a", Ok(1));
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));

        let priorities = vec![
            "server_a".to_string(),
            "server_b".to_string(),
            "server_c".to_string(),
        ];
        let result = collect_all(&mut join_set, &priorities, None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn collect_all_appends_unlisted_servers_after_prioritized() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_x", Ok(99));
        spawn_tagged_named(&mut join_set, "server_a", Ok(1));
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));

        // Only server_a and server_b are prioritized; server_x is unlisted
        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = collect_all(&mut join_set, &priorities, None, None).await;
        let values = assert_done(result);
        // Prioritized servers first in order, then unlisted
        assert_eq!(&values[..2], &[1, 2]);
        assert_eq!(values[2], 99);
    }

    #[tokio::test]
    async fn collect_all_with_mixed_priorities_and_errors() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail")));
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));
        spawn_tagged_named(&mut join_set, "server_x", Ok(99));

        // server_a is prioritized but fails; server_b succeeds; server_x is unlisted
        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = collect_all(&mut join_set, &priorities, None, None).await;
        let values = assert_done(result);
        // server_b (prioritized, succeeded) first, then server_x (unlisted)
        assert_eq!(values, vec![2, 99]);
    }

    #[tokio::test]
    async fn collect_all_skips_absent_priority_servers() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));

        // server_a is in priorities but has no task in the JoinSet
        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = collect_all(&mut join_set, &priorities, None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![2]);
    }
}
