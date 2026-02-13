//! First-win fan-in utility for concurrent bridge requests.
//!
//! Provides `first_win()` which returns the first non-empty successful result
//! from a `JoinSet` of concurrent bridge requests. Used by position-based LSP
//! handlers to fan-out to multiple downstream servers and return the first
//! useful response.

use std::io;

use tokio::task::JoinSet;

/// Returns the first non-empty successful result from a JoinSet of concurrent bridge requests.
///
/// Iterates through completed futures in arrival order. Returns the first result where:
/// - The task didn't panic (`JoinError`)
/// - The bridge request didn't fail (`io::Error`)
/// - The response passes the `is_nonempty` predicate
///
/// On success, aborts remaining in-flight tasks and returns the winning value.
/// Returns `None` if all tasks fail, error, or produce empty results.
///
/// # Abort semantics
///
/// Aborted tasks may leave stale entries in UpstreamRequestRegistry and ResponseRouter.
/// Both systems handle orphaned entries gracefully (see pool.rs design notes).
pub(super) async fn first_win<T: Send + 'static>(
    join_set: &mut JoinSet<io::Result<T>>,
    is_nonempty: impl Fn(&T) -> bool,
) -> Option<T> {
    while let Some(result) = join_set.join_next().await {
        if let Ok(Ok(value)) = result
            && is_nonempty(&value)
        {
            join_set.abort_all();
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// first_win returns the first non-empty result, skipping errors.
    #[tokio::test]
    async fn first_win_returns_first_nonempty_result() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail")) });
        join_set.spawn(async { Ok(None) });
        join_set.spawn(async { Ok(Some(42)) });

        let result = first_win(&mut join_set, |opt| opt.is_some()).await;

        assert_eq!(result, Some(Some(42)));
    }

    /// first_win returns None when all tasks return errors.
    #[tokio::test]
    async fn first_win_returns_none_when_all_fail() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail 1")) });
        join_set.spawn(async { Err(io::Error::other("fail 2")) });
        join_set.spawn(async { Err(io::Error::other("fail 3")) });

        let result = first_win(&mut join_set, |opt| opt.is_some()).await;

        assert_eq!(result, None);
    }

    /// first_win returns None when all tasks return empty results.
    #[tokio::test]
    async fn first_win_returns_none_when_all_empty() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Ok(None) });
        join_set.spawn(async { Ok(None) });
        join_set.spawn(async { Ok(None) });

        let result = first_win(&mut join_set, |opt| opt.is_some()).await;

        assert_eq!(result, None);
    }

    /// first_win skips errors and returns a later success.
    #[tokio::test]
    async fn first_win_skips_errors_and_returns_later_success() {
        let mut join_set: JoinSet<io::Result<Option<i32>>> = JoinSet::new();
        join_set.spawn(async { Err(io::Error::other("fail 1")) });
        join_set.spawn(async { Err(io::Error::other("fail 2")) });
        join_set.spawn(async { Ok(Some(42)) });

        let result = first_win(&mut join_set, |opt| opt.is_some()).await;

        assert_eq!(result, Some(Some(42)));
    }

    /// first_win uses the is_nonempty predicate to filter results.
    #[tokio::test]
    async fn first_win_uses_is_nonempty_predicate() {
        let mut join_set: JoinSet<io::Result<Option<Vec<i32>>>> = JoinSet::new();
        join_set.spawn(async { Ok(Some(vec![])) }); // empty vec — should be skipped
        join_set.spawn(async { Ok(Some(vec![1])) }); // non-empty — should win

        let result = first_win(&mut join_set, |opt| matches!(opt, Some(v) if !v.is_empty())).await;

        assert_eq!(result, Some(Some(vec![1])));
    }
}
