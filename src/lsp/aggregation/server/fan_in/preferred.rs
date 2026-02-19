//! Preferred fan-in strategy for concurrent bridge requests.
//!
//! [`preferred()`] implements priority-aware collection: results from higher-priority
//! servers are preferred over lower-priority ones, with fallback to first-win for
//! unprioritized servers. When `priorities` is empty, degrades to pure first-win.

use std::collections::HashSet;

use indexmap::IndexMap;

use tokio::task::JoinSet;

use super::FanInResult;
use crate::lsp::aggregation::server::fan_out::TaggedResult;
use crate::lsp::request_id::CancelReceiver;

/// Pick the best result from buffered wins, respecting priority order.
///
/// Walks the priority list first; if no priority server has a buffered result,
/// falls back to any unprioritized server.
fn pick_best_buffered<T>(
    buffered_wins: &mut IndexMap<String, T>,
    priorities: &[String],
) -> Option<T> {
    for name in priorities {
        if let Some(value) = buffered_wins.shift_remove(name.as_str()) {
            return Some(value);
        }
    }
    let first_key = buffered_wins.keys().next().cloned();
    first_key.and_then(|k| buffered_wins.shift_remove(&k))
}

/// Priority-aware collection of concurrent bridge results.
///
/// Buffers results by server name and walks the priority list after each arrival.
/// Returns the highest-priority non-empty result, or falls back to first-win
/// for servers not in the priority list.
///
/// # Priority resolution
///
/// After each result arrives:
/// 1. Walk `priorities` in order (highest first)
/// 2. If server has a buffered non-empty result → return it (abort rest)
/// 3. If server failed/empty → skip to next priority
/// 4. If server hasn't responded → wait (can't decide yet)
/// 5. If all priority servers exhausted → check unprioritized buffer
///
/// # Panicked servers
///
/// JoinError (panic) can't be attributed to a specific server. Panicked priority
/// servers remain "pending" until the JoinSet drains, then are treated as failed.
pub(crate) async fn preferred<T: Send + 'static>(
    join_set: &mut JoinSet<TaggedResult<T>>,
    is_nonempty: impl Fn(&T) -> bool,
    priorities: &[String],
    cancel_rx: Option<CancelReceiver>,
) -> FanInResult<T> {
    let mut buffered_wins: IndexMap<String, T> = IndexMap::new();
    let mut failed_servers: HashSet<String> = HashSet::new();
    let mut errors: usize = 0;

    // Helper closure to check if we can make a decision
    let try_decide =
        |buffered_wins: &mut IndexMap<String, T>, failed_servers: &HashSet<String>| -> Option<T> {
            // Walk priority list in order
            for name in priorities {
                if let Some(value) = buffered_wins.shift_remove(name.as_str()) {
                    return Some(value);
                }
                if failed_servers.contains(name.as_str()) {
                    continue; // This priority server failed, try next
                }
                // This priority server hasn't responded yet — can't decide
                return None;
            }
            // All priority servers exhausted (failed or absent) — check unprioritized buffer
            // Return first buffered win from any unprioritized server
            let first_key = buffered_wins.keys().next().cloned();
            first_key.and_then(|k| buffered_wins.shift_remove(&k))
        };

    let process_result = |tagged: TaggedResult<T>,
                          is_nonempty: &dyn Fn(&T) -> bool,
                          buffered_wins: &mut IndexMap<String, T>,
                          failed_servers: &mut HashSet<String>,
                          errors: &mut usize| {
        match tagged.value {
            Err(io_err) => {
                *errors += 1;
                failed_servers.insert(tagged.server_name.clone());
                log::warn!("bridge request failed ({}): {io_err}", tagged.server_name);
            }
            Ok(value) if is_nonempty(&value) => {
                buffered_wins.insert(tagged.server_name, value);
            }
            Ok(_) => {
                // Empty result — treat as failed for priority purposes
                failed_servers.insert(tagged.server_name);
            }
        }
    };

    // No cancel support — simple loop
    let Some(cancel_rx) = cancel_rx else {
        while let Some(result) = join_set.join_next().await {
            match result {
                Err(join_err) => {
                    errors += 1;
                    log::warn!("bridge task panicked: {join_err}");
                }
                Ok(tagged) => {
                    process_result(
                        tagged,
                        &is_nonempty,
                        &mut buffered_wins,
                        &mut failed_servers,
                        &mut errors,
                    );
                    if let Some(value) = try_decide(&mut buffered_wins, &failed_servers) {
                        join_set.abort_all();
                        return FanInResult::Done(value);
                    }
                }
            }
        }
        // JoinSet drained — all remaining priority servers that never responded were panics.
        // Walk priorities to pick the best buffered result.
        if let Some(value) = pick_best_buffered(&mut buffered_wins, priorities) {
            return FanInResult::Done(value);
        }
        return FanInResult::NoResult { errors };
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
                    None => {
                        // JoinSet drained — walk priorities for best buffered result.
                        if let Some(value) = pick_best_buffered(&mut buffered_wins, priorities) {
                            return FanInResult::Done(value);
                        }
                        return FanInResult::NoResult { errors };
                    }
                    Some(Err(join_err)) => {
                        errors += 1;
                        log::warn!("bridge task panicked: {join_err}");
                    }
                    Some(Ok(tagged)) => {
                        process_result(tagged, &is_nonempty, &mut buffered_wins, &mut failed_servers, &mut errors);
                        if let Some(value) = try_decide(&mut buffered_wins, &failed_servers) {
                            join_set.abort_all();
                            return FanInResult::Done(value);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::lsp::aggregation::server::fan_in::test_helpers::*;

    #[tokio::test]
    async fn preferred_skips_empty_results_with_no_priorities() {
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Ok(None));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(42)));

        let result = preferred(&mut join_set, |opt| opt.is_some(), &[], None).await;

        assert_eq!(assert_done(result), Some(42));
    }

    #[tokio::test]
    async fn preferred_returns_highest_priority_server_result() {
        // server_a is priority 1, server_b is priority 2
        // Both return results, but server_a should win
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Ok(Some(1)));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(2)));

        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        assert_eq!(assert_done(result), Some(1));
    }

    #[tokio::test]
    async fn preferred_waits_for_higher_priority_even_if_lower_arrives_first() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        // server_b responds immediately, server_a responds after a delay
        // preferred should wait for server_a since it's higher priority
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();

        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();

        // server_b responds immediately
        join_set.spawn(async move {
            TaggedResult {
                server_name: "server_b".to_string(),
                value: Ok(Some(2)),
            }
        });

        // server_a responds after barrier
        join_set.spawn(async move {
            barrier_clone.wait().await;
            TaggedResult {
                server_name: "server_a".to_string(),
                value: Ok(Some(1)),
            }
        });

        let priorities = vec!["server_a".to_string(), "server_b".to_string()];

        // Release server_a after a brief moment
        let barrier_release = barrier.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            barrier_release.wait().await;
        });

        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        // server_a should win even though server_b arrived first
        assert_eq!(assert_done(result), Some(1));
    }

    #[tokio::test]
    async fn preferred_falls_back_on_priority_error() {
        // server_a (priority 1) fails, server_b (priority 2) succeeds
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail")));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(42)));

        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        assert_eq!(assert_done(result), Some(42));
    }

    #[tokio::test]
    async fn preferred_falls_back_to_first_win_when_all_priority_servers_fail() {
        // Both priority servers fail, unprioritized server_c succeeds
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail a")));
        spawn_tagged_named(&mut join_set, "server_b", Err(io::Error::other("fail b")));
        spawn_tagged_named(&mut join_set, "server_c", Ok(Some(99)));

        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        assert_eq!(assert_done(result), Some(99));
    }

    #[tokio::test]
    async fn preferred_handles_cancel() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            TaggedResult {
                server_name: "slow".to_string(),
                value: Ok(Some(42)),
            }
        });

        tx.send(()).unwrap();

        let result = preferred(&mut join_set, |opt| opt.is_some(), &[], Some(rx)).await;

        assert_cancelled(result);
    }

    #[tokio::test]
    async fn preferred_handles_cancel_with_non_empty_priorities() {
        // Cancel arrives while waiting for a higher-priority server to respond.
        // The cancel path should win regardless of pending priority state.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            TaggedResult {
                server_name: "server_a".to_string(),
                value: Ok(Some(1)),
            }
        });
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            TaggedResult {
                server_name: "server_b".to_string(),
                value: Ok(Some(2)),
            }
        });

        tx.send(()).unwrap();

        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, Some(rx)).await;

        assert_cancelled(result);
    }

    #[tokio::test]
    async fn preferred_returns_no_result_when_all_fail() {
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail a")));
        spawn_tagged_named(&mut join_set, "server_b", Err(io::Error::other("fail b")));

        let priorities = vec!["server_a".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        assert_eq!(assert_no_result(result), 2);
    }

    #[tokio::test]
    async fn preferred_falls_back_when_priority_server_panics() {
        // A priority server panics (JoinError) — can't be attributed to a name.
        // The algorithm keeps it as "pending" until the JoinSet drains, then
        // falls back to the unprioritized server_b's buffered result.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        join_set.spawn(async {
            panic!("simulated panic in priority server");
        });
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(42)));

        let priorities = vec!["server_a".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        assert_eq!(assert_done(result), Some(42));
    }

    #[tokio::test]
    async fn preferred_respects_priority_in_buffered_wins_after_panic_drain() {
        // server_a (priority 1) panics, server_b (priority 2) and server_c (unprioritized) succeed.
        // After drain, server_b should win over server_c because it's higher in the priority list.
        // Without the fix, HashMap iteration order could return server_c instead.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        join_set.spawn(async {
            panic!("simulated panic in priority server");
        });
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(2)));
        spawn_tagged_named(&mut join_set, "server_c", Ok(Some(3)));

        let priorities = vec!["server_a".to_string(), "server_b".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        // server_b must win — it's in the priority list and server_a (higher priority) panicked
        assert_eq!(assert_done(result), Some(2));
    }

    #[tokio::test]
    async fn preferred_treats_unlisted_servers_as_lowest_priority() {
        // server_a is prioritized and fails; server_b is not listed (lowest priority) and succeeds
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail")));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(77)));

        let priorities = vec!["server_a".to_string()];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &priorities, None).await;

        assert_eq!(assert_done(result), Some(77));
    }
}
