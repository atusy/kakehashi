//! Server-level aggregation dispatch.
//!
//! [`dispatch_preferred()`] combines [`fan_out()`] with the preferred
//! strategy, giving each handler a single call-site for the common pattern.
//! When `ctx.priorities` is empty, it degrades to pure first-win behavior.
//!
//! [`dispatch_collect_all()`] combines [`fan_out()`] with the collect-all
//! strategy, collecting every successful result from all matching servers.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::sync::Arc;

use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::request_id::CancelReceiver;

use super::fan_in::FanInResult;
use super::fan_in::{all, preferred};
use super::fan_out::{FanOutTask, fan_out};

/// Server-level aggregation entry point using the preferred strategy.
///
/// Fans out one task per matching server and returns the highest-priority
/// non-empty result. When `ctx.priorities` is empty, degrades to first-win.
///
/// Pre-filters `ctx.priorities` against `ctx.configs` to ensure only
/// configured server names are passed to the preferred algorithm.
pub(crate) async fn dispatch_preferred<T, F, Fut>(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    is_nonempty: impl Fn(&T) -> bool,
    cancel_rx: Option<CancelReceiver>,
) -> FanInResult<T>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    // Pre-filter priorities: keep only server names that exist in ctx.configs.
    let configured: HashSet<&str> = ctx.configs.iter().map(|c| c.server_name.as_str()).collect();
    let effective_priorities: Vec<String> = ctx
        .priorities
        .iter()
        .filter(|name| configured.contains(name.as_str()))
        .cloned()
        .collect();

    let mut join_set = fan_out(ctx, pool, f);
    preferred::preferred(&mut join_set, is_nonempty, &effective_priorities, cancel_rx).await
}

/// Server-level aggregation entry point using the collect-all strategy.
///
/// Fans out one task per matching server and collects every successful result.
/// When `ctx.priorities` is non-empty, results are ordered by priority, with
/// unlisted servers appended in insertion order.
/// Returns `Done(vec)` when at least one succeeds, `NoResult` when all fail,
/// or `Cancelled` on cancel notification.
///
/// Pre-filters `ctx.priorities` against `ctx.configs` to ensure only
/// configured server names are passed to the collect-all algorithm.
pub(crate) async fn dispatch_collect_all<T, F, Fut>(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    cancel_rx: Option<CancelReceiver>,
) -> FanInResult<Vec<T>>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let configured: HashSet<&str> = ctx.configs.iter().map(|c| c.server_name.as_str()).collect();
    let effective_priorities: Vec<String> = ctx
        .priorities
        .iter()
        .filter(|name| configured.contains(name.as_str()))
        .cloned()
        .collect();

    let mut join_set = fan_out(ctx, pool, f);
    all::collect_all(&mut join_set, &effective_priorities, cancel_rx).await
}
