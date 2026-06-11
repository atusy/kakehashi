//! Preferred fan-in strategy for concurrent bridge requests.
//!
//! [`preferred()`] implements priority-aware collection over the expanded
//! [`PriorityEntry`] walk (aggregation-priorities-wildcard): named entries
//! are strict priority positions, and a [`PriorityEntry::Rest`] group (the
//! `"*"` element) is first-win — the earliest non-empty arrival among its
//! members wins the position.

use std::collections::HashSet;

use indexmap::IndexMap;

use tokio::task::JoinSet;

use super::FanInResult;
use crate::lsp::aggregation::server::fan_out::TaggedResult;
use crate::lsp::aggregation::server::priority::PriorityEntry;
use crate::lsp::request_id::CancelReceiver;

/// Take the earliest-arrived buffered win belonging to `names`, if any.
///
/// `buffered_wins` is insertion-ordered (`IndexMap`), so the first matching
/// key is the earliest arrival — the "first win" of the group.
fn take_first_group_win<T>(buffered_wins: &mut IndexMap<String, T>, names: &[String]) -> Option<T> {
    let key = buffered_wins
        .keys()
        .find(|key| names.iter().any(|name| name == *key))
        .cloned()?;
    buffered_wins.shift_remove(&key)
}

/// Pick the best result from buffered wins after the JoinSet drained,
/// walking the entries in priority order.
///
/// The trailing index-0 fallback covers a buffered win from a server outside
/// every entry — unreachable via dispatch (fan-out spawns only entry
/// members), kept as defense in depth.
fn pick_best_buffered<T>(
    buffered_wins: &mut IndexMap<String, T>,
    entries: &[PriorityEntry],
) -> Option<T> {
    for entry in entries {
        let win = match entry {
            PriorityEntry::Server(name) => buffered_wins.shift_remove(name.as_str()),
            PriorityEntry::Rest(names) => take_first_group_win(buffered_wins, names),
        };
        if win.is_some() {
            return win;
        }
    }
    buffered_wins.shift_remove_index(0).map(|(_, v)| v)
}

/// Priority-aware fan-in: buffer results by server name, and on each arrival
/// walk the entries highest-first — return the first non-empty buffered
/// result, skipping servers that failed/returned empty, and waiting on
/// servers that haven't responded yet. A [`PriorityEntry::Rest`] group is
/// decided first-win: any buffered member wins the position immediately,
/// without waiting for the other members.
///
/// Empty `entries` (the `priorities = []` kill switch) pairs with an empty
/// JoinSet from fan-out and yields `NoResult { errors: 0 }`.
///
/// `JoinError` can't be attributed to a server, so a panicked prioritized
/// server stays "pending" until the JoinSet drains, then is treated as failed.
pub(crate) async fn preferred<T: Send + 'static>(
    join_set: &mut JoinSet<TaggedResult<T>>,
    is_nonempty: impl Fn(&T) -> bool,
    entries: &[PriorityEntry],
    cancel_rx: Option<CancelReceiver>,
) -> FanInResult<T> {
    let mut buffered_wins: IndexMap<String, T> = IndexMap::new();
    let mut failed_servers: HashSet<String> = HashSet::new();
    let mut errors: usize = 0;

    // Helper closure to check if we can make a decision
    let try_decide =
        |buffered_wins: &mut IndexMap<String, T>, failed_servers: &HashSet<String>| -> Option<T> {
            for entry in entries {
                match entry {
                    PriorityEntry::Server(name) => {
                        if let Some(value) = buffered_wins.shift_remove(name.as_str()) {
                            return Some(value);
                        }
                        if failed_servers.contains(name.as_str()) {
                            continue; // This priority server failed, try next
                        }
                        // This priority server hasn't responded yet — can't decide
                        return None;
                    }
                    PriorityEntry::Rest(names) => {
                        // First-win group: any buffered member wins now.
                        if let Some(value) = take_first_group_win(buffered_wins, names) {
                            return Some(value);
                        }
                        if names
                            .iter()
                            .all(|name| failed_servers.contains(name.as_str()))
                        {
                            continue; // Whole group failed, try next entry
                        }
                        // A group member is still pending — can't decide
                        return None;
                    }
                }
            }
            // All entries exhausted (failed or absent). Defensive fallback for
            // a buffered win outside every entry (unreachable via dispatch).
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
                    None => {
                        // JoinSet drained — walk entries for best buffered result.
                        if let Some(value) = pick_best_buffered(&mut buffered_wins, entries) {
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

    fn servers(names: &[&str]) -> Vec<PriorityEntry> {
        names
            .iter()
            .map(|n| PriorityEntry::Server(n.to_string()))
            .collect()
    }

    fn rest(names: &[&str]) -> PriorityEntry {
        PriorityEntry::Rest(names.iter().map(|n| n.to_string()).collect())
    }

    #[tokio::test]
    async fn preferred_skips_empty_results_in_rest_group() {
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Ok(None));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(42)));

        let entries = vec![rest(&["server_a", "server_b"])];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_done(result), Some(42));
    }

    #[tokio::test]
    async fn preferred_returns_highest_priority_server_result() {
        // server_a is priority 1, server_b is priority 2
        // Both return results, but server_a should win
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Ok(Some(1)));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(2)));

        let entries = servers(&["server_a", "server_b"]);
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

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

        let entries = servers(&["server_a", "server_b"]);

        // Release server_a after a brief moment
        let barrier_release = barrier.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            barrier_release.wait().await;
        });

        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        // server_a should win even though server_b arrived first
        assert_eq!(assert_done(result), Some(1));
    }

    #[tokio::test]
    async fn preferred_rest_group_is_first_win_without_waiting_for_members() {
        // Both members are in one '*' group: the earliest non-empty arrival
        // wins immediately, even while the other member is still pending.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_fast", Ok(Some(1)));
        join_set.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            TaggedResult {
                server_name: "server_slow".to_string(),
                value: Ok(Some(2)),
            }
        });

        let entries = vec![rest(&["server_fast", "server_slow"])];
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            preferred(&mut join_set, |opt| opt.is_some(), &entries, None),
        )
        .await
        .expect("group first-win must not wait for the slow member");

        assert_eq!(assert_done(result), Some(1));
    }

    #[tokio::test]
    async fn preferred_server_listed_after_rest_is_demoted_below_the_group() {
        // ["*", "zzz"]: the rest group outranks zzz — zzz only wins when the
        // whole group comes up empty.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "zzz", Ok(Some(99)));
        spawn_tagged_named(&mut join_set, "server_a", Ok(Some(1)));

        let entries = vec![rest(&["server_a"]), PriorityEntry::Server("zzz".into())];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_done(result), Some(1));
    }

    #[tokio::test]
    async fn preferred_pending_rest_group_blocks_buffered_demoted_server() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        // ["*", "zzz"]: zzz's win is already buffered, but a group member is
        // still in flight — the walk must WAIT for the group, not hand the
        // position to the demoted server.
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();

        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "zzz", Ok(Some(99)));
        join_set.spawn(async move {
            barrier_clone.wait().await;
            TaggedResult {
                server_name: "server_a".to_string(),
                value: Ok(Some(1)),
            }
        });

        let barrier_release = barrier.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            barrier_release.wait().await;
        });

        let entries = vec![rest(&["server_a"]), PriorityEntry::Server("zzz".into())];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(
            assert_done(result),
            Some(1),
            "the pending group member must win over the already-buffered demoted server"
        );
    }

    #[tokio::test]
    async fn preferred_demoted_server_wins_when_rest_group_is_empty_results() {
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "zzz", Ok(Some(99)));
        spawn_tagged_named(&mut join_set, "server_a", Ok(None));

        let entries = vec![rest(&["server_a"]), PriorityEntry::Server("zzz".into())];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_done(result), Some(99));
    }

    #[tokio::test]
    async fn preferred_falls_back_on_priority_error() {
        // server_a (priority 1) fails, server_b (priority 2) succeeds
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail")));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(42)));

        let entries = servers(&["server_a", "server_b"]);
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_done(result), Some(42));
    }

    #[tokio::test]
    async fn preferred_falls_back_to_rest_group_when_all_priority_servers_fail() {
        // Both named servers fail, the trailing '*' group's server_c succeeds
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail a")));
        spawn_tagged_named(&mut join_set, "server_b", Err(io::Error::other("fail b")));
        spawn_tagged_named(&mut join_set, "server_c", Ok(Some(99)));

        let mut entries = servers(&["server_a", "server_b"]);
        entries.push(rest(&["server_c"]));
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

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

        let entries = vec![rest(&["slow"])];
        tx.send(()).unwrap();

        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, Some(rx)).await;

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

        let entries = servers(&["server_a", "server_b"]);
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, Some(rx)).await;

        assert_cancelled(result);
    }

    #[tokio::test]
    async fn preferred_returns_no_result_when_all_fail() {
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail a")));
        spawn_tagged_named(&mut join_set, "server_b", Err(io::Error::other("fail b")));

        let entries = vec![
            PriorityEntry::Server("server_a".into()),
            rest(&["server_b"]),
        ];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_no_result(result), 2);
    }

    #[tokio::test]
    async fn preferred_returns_no_result_for_empty_entries_and_join_set() {
        // priorities = [] (the kill switch): fan-out spawns nothing, fan-in
        // reports NoResult with zero errors.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        let result = preferred(&mut join_set, |opt| opt.is_some(), &[], None).await;
        assert_eq!(assert_no_result(result), 0);
    }

    #[tokio::test]
    async fn preferred_falls_back_when_priority_server_panics() {
        // A priority server panics (JoinError) — can't be attributed to a name.
        // The algorithm keeps it as "pending" until the JoinSet drains, then
        // falls back to the rest group's buffered result.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        join_set.spawn(async {
            panic!("simulated panic in priority server");
        });
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(42)));

        let entries = vec![
            PriorityEntry::Server("server_a".into()),
            rest(&["server_b"]),
        ];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_done(result), Some(42));
    }

    #[tokio::test]
    async fn preferred_respects_priority_in_buffered_wins_after_panic_drain() {
        // server_a (priority 1) panics, server_b (priority 2) and server_c (rest) succeed.
        // After drain, server_b should win over server_c because it's higher in the walk.
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        join_set.spawn(async {
            panic!("simulated panic in priority server");
        });
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(2)));
        spawn_tagged_named(&mut join_set, "server_c", Ok(Some(3)));

        let mut entries = servers(&["server_a", "server_b"]);
        entries.push(rest(&["server_c"]));
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        // server_b must win — it's named and server_a (higher priority) panicked
        assert_eq!(assert_done(result), Some(2));
    }

    #[tokio::test]
    async fn preferred_treats_rest_group_as_lowest_priority() {
        // server_a is named and fails; server_b sits in the trailing '*' group
        let mut join_set: JoinSet<TaggedResult<Option<i32>>> = JoinSet::new();
        spawn_tagged_named(&mut join_set, "server_a", Err(io::Error::other("fail")));
        spawn_tagged_named(&mut join_set, "server_b", Ok(Some(77)));

        let entries = vec![
            PriorityEntry::Server("server_a".into()),
            rest(&["server_b"]),
        ];
        let result = preferred(&mut join_set, |opt| opt.is_some(), &entries, None).await;

        assert_eq!(assert_done(result), Some(77));
    }
}
