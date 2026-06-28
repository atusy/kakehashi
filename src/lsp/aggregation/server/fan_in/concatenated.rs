//! Concatenated-strategy fan-in for concurrent bridge requests.
//!
//! [`concatenated()`] collects all successful results from a `JoinSet<TaggedResult<T>>`,
//! returning `FanInResult<Vec<T>>`. Results are ordered by the caller's
//! `priorities` list — dispatch passes the flattened priority expansion, which
//! covers every spawned server (aggregation-priorities-wildcard), so the
//! unlisted-server fallback below is defensive only.

use indexmap::IndexMap;
use tokio::task::JoinSet;

use super::FanInResult;
use crate::lsp::aggregation::server::fan_out::TaggedResult;
use crate::lsp::request_id::CancelReceiver;

/// Reorder tagged results according to the priority list.
///
/// Walks `priorities` in order, draining matching entries from the map.
/// Remaining (unlisted) entries are appended in insertion order — unreachable
/// via dispatch (the expansion names every spawned server) but kept so a
/// stray result is surfaced rather than dropped.
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

/// Wait for **every** task in the JoinSet and return all successful values
/// (unlike [`super::preferred::preferred()`], which short-circuits on the
/// first non-empty hit). When `priorities` is non-empty, results sort
/// priority-first with unlisted servers appended in arrival order.
///
/// `Done(vec)` on any success (empty JoinSet → `Done(vec![])`),
/// `NoResult{errors}` when every task fails, `Cancelled` if a cancel arrives
/// first. Callers MUST follow up with `pool.unregister_all_for_upstream_id()`
/// to clean up entries left behind by aborted tasks.
///
/// `panic_sink`, when set, is incremented once per task that **panicked**
/// (a `JoinError` with `is_panic()`). This is surfaced even on `Done` — unlike
/// the local `errors` tally, which the partial-success `Done` arm discards —
/// because a concatenated caller counts I/O failures in-task but a panic unwinds
/// before that runs, so the sink is the only path that lets a panicking server
/// drive CLI exit 2 (#506). Only panics feed it: I/O `Err`s are left to the
/// caller's in-task counting, and a cancellation `JoinError` (`is_cancelled()`)
/// is deliberately excluded — an aborted task was cancelled, not failed, and
/// must not drive exit 2 (matching `RequestErrorSink`'s contract).
pub(crate) async fn concatenated<T: Send + 'static>(
    join_set: &mut JoinSet<TaggedResult<T>>,
    priorities: &[String],
    cancel_rx: Option<CancelReceiver>,
    log_target: Option<&str>,
    panic_sink: Option<&std::sync::atomic::AtomicUsize>,
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
                        // Surface a PANIC to the caller's sink even though a
                        // partial-success run returns `Done` and drops `errors`.
                        // Only panics feed the sink: a panicking task unwinds
                        // before the dispatch's in-task error counting runs,
                        // whereas an I/O `Err` is already counted in-task — so
                        // adding I/O errors here too would double-count (#506). A
                        // cancellation `JoinError` is excluded: an aborted task
                        // was cancelled, not failed, and must not drive exit 2.
                        if join_err.is_panic()
                            && let Some(sink) = panic_sink
                        {
                            sink.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        log::warn!(target: log_target, "bridge task did not complete: {join_err}");
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::lsp::aggregation::server::fan_in::test_helpers::*;

    #[tokio::test]
    async fn concatenated_surfaces_panic_count_to_sink_even_on_partial_success() {
        // A downstream-request task that PANICS unwinds before its in-task
        // error counting runs, and a partial-success run returns `Done`
        // (dropping the local `errors` tally), so without the panic sink a
        // panicking server is invisible to CLI exit-2 accounting (#506).
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(42));
        spawn_panicking(&mut join_set);

        let sink = AtomicUsize::new(0);
        let result = concatenated(&mut join_set, &[], None, None, Some(&sink)).await;

        // Partial success: the surviving result is still returned...
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
        // ...and the panic is surfaced to the sink so it can drive exit 2.
        assert_eq!(sink.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn concatenated_panic_sink_counts_only_panics_not_io_errors() {
        // The panic sink is fed ONLY by `JoinError` panics. I/O failures are
        // counted in-task by the concatenated dispatch's send closure, so the
        // fan-in must not double-count them here.
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(42));
        spawn_tagged(&mut join_set, Err(io::Error::other("io fail")));

        let sink = AtomicUsize::new(0);
        let result = concatenated(&mut join_set, &[], None, None, Some(&sink)).await;

        assert_eq!(assert_done(result), vec![42]);
        assert_eq!(
            sink.load(Ordering::Relaxed),
            0,
            "I/O errors are counted in-task, never via the panic sink"
        );
    }

    #[tokio::test]
    async fn concatenated_surfaces_panic_count_to_sink_when_all_fail() {
        // All tasks panic → `NoResult`. The concatenated dispatch does not
        // count `NoResult.errors`, so the panic sink is the only path that
        // surfaces an all-panic run as exit 2.
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_panicking(&mut join_set);
        spawn_panicking(&mut join_set);

        let sink = AtomicUsize::new(0);
        let result = concatenated(&mut join_set, &[], None, None, Some(&sink)).await;

        assert_eq!(assert_no_result(result), 2);
        assert_eq!(sink.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn concatenated_handles_panic_without_a_sink() {
        // The `panic_sink = None` path (the configuration LSP-mode callers use)
        // must still drain the panicking task to `NoResult` without itself
        // panicking.
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_panicking(&mut join_set);

        let result = concatenated(&mut join_set, &[], None, None, None).await;
        assert_eq!(assert_no_result(result), 1);
    }

    #[tokio::test]
    async fn concatenated_returns_all_successful_results() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(1));
        spawn_tagged(&mut join_set, Ok(2));
        spawn_tagged(&mut join_set, Ok(3));

        let result = concatenated(&mut join_set, &[], None, None, None).await;
        let mut values = assert_done(result);
        values.sort();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn concatenated_returns_no_result_when_all_fail() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 1")));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 2")));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 3")));

        let result = concatenated(&mut join_set, &[], None, None, None).await;
        assert_eq!(
            assert_no_result(result),
            3,
            "all 3 tasks should be counted as errors"
        );
    }

    #[tokio::test]
    async fn concatenated_includes_successes_despite_errors() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(42));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 1")));
        spawn_tagged(&mut join_set, Err(io::Error::other("fail 2")));

        let result = concatenated(&mut join_set, &[], None, None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
    }

    #[tokio::test]
    async fn concatenated_returns_done_empty_for_empty_join_set() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();

        let result = concatenated(&mut join_set, &[], None, None, None).await;
        let values = assert_done(result);
        assert!(
            values.is_empty(),
            "empty JoinSet should produce Done(vec![])"
        );
    }

    #[tokio::test]
    async fn concatenated_returns_cancelled_on_cancel() {
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

        let result = concatenated(&mut join_set, &[], Some(rx), None, None).await;
        assert_cancelled(result);
    }

    #[tokio::test]
    async fn concatenated_returns_done_before_cancel() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged(&mut join_set, Ok(42));

        let result = concatenated(&mut join_set, &[], Some(rx), None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![42]);
    }

    #[tokio::test]
    async fn concatenated_orders_results_by_priority() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_c", Ok(3));
        spawn_tagged_named(&mut join_set, "server_a", Ok(1));
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));

        let priorities = vec![
            "server_a".to_string(),
            "server_b".to_string(),
            "server_c".to_string(),
        ];
        let result = concatenated(&mut join_set, &priorities, None, None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn concatenated_appends_unlisted_servers_after_prioritized() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_x", Ok(99));
        spawn_tagged_named(&mut join_set, "server_a", Ok(1));
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));

        // Only server_a and server_b are prioritized; server_x is unlisted
        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = concatenated(&mut join_set, &priorities, None, None, None).await;
        let values = assert_done(result);
        // Prioritized servers first in order, then unlisted
        assert_eq!(&values[..2], &[1, 2]);
        assert_eq!(values[2], 99);
    }

    #[tokio::test]
    async fn concatenated_with_mixed_priorities_and_errors() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail")));
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));
        spawn_tagged_named(&mut join_set, "server_x", Ok(99));

        // server_a is prioritized but fails; server_b succeeds; server_x is unlisted
        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = concatenated(&mut join_set, &priorities, None, None, None).await;
        let values = assert_done(result);
        // server_b (prioritized, succeeded) first, then server_x (unlisted)
        assert_eq!(values, vec![2, 99]);
    }

    #[tokio::test]
    async fn concatenated_skips_absent_priority_servers() {
        let mut join_set: JoinSet<TaggedResult<i32>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_b", Ok(2));

        // server_a is in priorities but has no task in the JoinSet
        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = concatenated(&mut join_set, &priorities, None, None, None).await;
        let values = assert_done(result);
        assert_eq!(values, vec![2]);
    }
}
