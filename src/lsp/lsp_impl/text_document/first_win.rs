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
        match result {
            Err(join_err) => {
                log::warn!("bridge task panicked: {join_err}");
            }
            Ok(Err(io_err)) => {
                log::debug!("bridge request failed: {io_err}");
            }
            Ok(Ok(value)) if is_nonempty(&value) => {
                join_set.abort_all();
                return Some(value);
            }
            Ok(Ok(_)) => {} // empty result — try next
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
