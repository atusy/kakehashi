//! Server-level aggregation dispatch.
//!
//! [`dispatch_first_win()`] combines [`fan_out()`] with the first-win
//! strategy, giving each handler a single call-site for the common pattern.
//!
//! [`dispatch_collect_all()`] combines [`fan_out()`] with the collect-all
//! strategy, collecting every successful result from all matching servers.

use std::future::Future;
use std::io;
use std::sync::Arc;

use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::request_id::CancelReceiver;

use super::fan_in::FanInResult;
use super::fan_in::{all, first_win};
use super::fan_out::{FanOutTask, fan_out};

/// Server-level aggregation entry point using the first-win strategy.
///
/// Fans out one task per matching server and returns the first non-empty
/// result. Handlers call this instead of `fan_out` + `first_win` directly.
pub(crate) async fn dispatch_first_win<T, F, Fut>(
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
    let mut join_set = fan_out(ctx, pool, f);
    first_win::first_win(&mut join_set, is_nonempty, cancel_rx).await
}

/// Server-level aggregation entry point using the collect-all strategy.
///
/// Fans out one task per matching server and collects every successful result.
/// Returns `Done(vec)` when at least one succeeds, `NoResult` when all fail,
/// or `Cancelled` on cancel notification.
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
    let mut join_set = fan_out(ctx, pool, f);
    all::collect_all(&mut join_set, cancel_rx).await
}
