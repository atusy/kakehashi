//! Server-level aggregation dispatch.
//!
//! [`dispatch_aggregation()`] combines [`fan_out()`] with the first-win
//! strategy, giving each handler a single call-site for the common pattern.

use std::future::Future;
use std::io;
use std::sync::Arc;

use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::request_id::CancelReceiver;

use super::fan_in::FanInResult;
use super::fan_in::first_win;
use super::fan_out::{FanOutTask, fan_out};

/// Server-level aggregation entry point using the first-win strategy.
///
/// Fans out one task per matching server and returns the first non-empty
/// result. Handlers call this instead of `fan_out` + `first_win` directly.
pub(crate) async fn dispatch_aggregation<T, F, Fut>(
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
