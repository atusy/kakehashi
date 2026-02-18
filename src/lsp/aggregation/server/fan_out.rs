//! Fan-out infrastructure for concurrent bridge requests.
//!
//! [`fan_out()`] spawns one task per matching server and returns a `JoinSet`
//! for the caller to pass to a collection strategy.

use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::task::JoinSet;

use crate::config::settings::BridgeServerConfig;
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::bridge::UpstreamId;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

/// Per-server arguments produced by [`fan_out()`].
///
/// Each spawned task receives its own clone of the shared context fields
/// plus the server-specific name and config. Handlers pass this struct
/// into their pool `send_*_request` call.
pub(crate) struct FanOutTask {
    pub(crate) pool: Arc<LanguageServerPool>,
    pub(crate) server_name: String,
    pub(crate) server_config: Arc<BridgeServerConfig>,
    pub(crate) uri: url::Url,
    pub(crate) injection_language: String,
    pub(crate) region_id: String,
    pub(crate) region_start_line: u32,
    pub(crate) virtual_content: String,
    pub(crate) upstream_id: Option<UpstreamId>,
}

/// Spawn one task per matching server, returning a `JoinSet` for collection.
///
/// Centralises the per-server clone boilerplate that was previously duplicated
/// in every fan-out handler. The caller supplies a closure that receives a
/// [`FanOutTask`] and returns the handler-specific future.
#[must_use = "the JoinSet must be passed to a collection strategy"]
pub(crate) fn fan_out<T, F, Fut>(
    ctx: &DocumentRequestContext,
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
