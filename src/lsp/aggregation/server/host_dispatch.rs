//! Host-bridge aggregation dispatch (host-document-bridge).
//!
//! Mirrors [`super::dispatch`] for the host path: one task per selected
//! host-capable server, the same priority expansion
//! (aggregation-priorities-wildcard) feeding both fan-out and fan-in, and the
//! `preferred` strategy. The host path has no injection region — tasks carry
//! the real URI and the host text verbatim.

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::config::settings::BridgeServerConfig;
use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
use crate::lsp::lsp_impl::bridge_context::HostRequestContext;
use crate::lsp::request_id::CancelReceiver;

use super::fan_in::{FanInResult, concatenated, preferred};
use super::fan_out::TaggedResult;
use super::priority::{entry_names, expand_priorities, truncate_entries};

/// Per-server arguments for a host bridge request.
///
/// The host counterpart of [`super::fan_out::FanOutTask`]: no injection
/// region, no offsets — the real URI and host text travel verbatim.
pub(crate) struct HostFanOutTask {
    pub(crate) pool: Arc<LanguageServerPool>,
    pub(crate) server_name: String,
    pub(crate) server_config: Arc<BridgeServerConfig>,
    pub(crate) uri: url::Url,
    pub(crate) language_id: String,
    pub(crate) text: Arc<str>,
    pub(crate) upstream_id: Option<UpstreamId>,
}

/// Host-bridge aggregation entry point using the preferred strategy.
///
/// Fans out one task per selected host server (allowlist + `"*"` expansion
/// against `ctx.configs`) and returns the highest-priority non-empty result.
pub(crate) async fn dispatch_host_preferred<T, F, Fut>(
    ctx: &HostRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    is_nonempty: impl Fn(&T) -> bool,
    cancel_rx: Option<CancelReceiver>,
) -> FanInResult<T>
where
    T: Send + 'static,
    F: Fn(HostFanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let (mut join_set, entries) = host_fan_out(ctx, pool, f);
    preferred::preferred(&mut join_set, is_nonempty, &entries, cancel_rx).await
}

/// Host-bridge aggregation entry point using the concatenated strategy
/// (cross-layer-aggregation diagnostics): every selected host server's
/// result is collected, ordered by the priority walk. The host counterpart
/// of [`super::dispatch::dispatch_concatenated`].
pub(crate) async fn dispatch_host_concatenated<T, F, Fut>(
    ctx: &HostRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    cancel_rx: Option<CancelReceiver>,
    log_target: Option<&str>,
) -> FanInResult<Vec<T>>
where
    T: Send + 'static,
    F: Fn(HostFanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let (mut join_set, entries) = host_fan_out(ctx, pool, f);
    let ordering = entry_names(&entries);
    concatenated::concatenated(&mut join_set, &ordering, cancel_rx, log_target).await
}

/// Shared host fan-out: allowlist + `"*"` expansion against `ctx.configs`,
/// one spawned task per selected server.
fn host_fan_out<T, F, Fut>(
    ctx: &HostRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
) -> (
    JoinSet<TaggedResult<T>>,
    Vec<super::priority::PriorityEntry>,
)
where
    T: Send + 'static,
    F: Fn(HostFanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let entries = truncate_entries(
        expand_priorities(&ctx.priorities, &ctx.configs),
        ctx.max_fan_out,
    );
    let selected = super::fan_out::select_servers(&ctx.configs, &entries);

    let mut join_set = JoinSet::new();
    for config in &selected {
        let server_name = config.server_name.clone();
        let task = HostFanOutTask {
            pool: Arc::clone(&pool),
            server_name: server_name.clone(),
            server_config: Arc::clone(&config.config),
            uri: ctx.uri.clone(),
            language_id: ctx.language_id.clone(),
            text: Arc::clone(&ctx.text),
            upstream_id: ctx.upstream_request_id.clone(),
        };
        let fut = f(task);
        join_set.spawn(async move {
            TaggedResult {
                server_name,
                value: fut.await,
            }
        });
    }
    (join_set, entries)
}
