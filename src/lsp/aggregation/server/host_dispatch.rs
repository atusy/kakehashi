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
use crate::lsp::bridge::{LanguageServerPool, ResolvedServerConfig, UpstreamId};
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
    // Counts panicking tasks even on partial-success `Done`, so a panicking
    // host server still drives CLI exit 2 (#506). The caller still counts I/O
    // failures in-task; only panics feed this.
    panic_sink: Option<&std::sync::atomic::AtomicUsize>,
) -> FanInResult<Vec<T>>
where
    T: Send + 'static,
    F: Fn(HostFanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let (mut join_set, entries) = host_fan_out(ctx, pool, f);
    let ordering = entry_names(&entries);
    concatenated::concatenated(&mut join_set, &ordering, cancel_rx, log_target, panic_sink).await
}

/// The host servers a request over `ctx` fans out to: allowlist + `"*"`
/// expansion against `ctx.configs`, `max_fan_out`-truncated, in walk order
/// (aggregation-priorities-wildcard).
///
/// Exposed for senders that have no fan-in — a forwarded notification
/// (custom-method-host-forwarding) goes to every selected server and
/// collects nothing — so they select exactly the servers a request would.
pub(crate) fn select_host_servers(ctx: &HostRequestContext) -> Vec<ResolvedServerConfig> {
    let entries = truncate_entries(
        expand_priorities(&ctx.priorities, &ctx.configs),
        ctx.max_fan_out,
    );
    super::fan_out::select_servers(&ctx.configs, &entries)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::AggregationStrategy;

    fn config(name: &str) -> ResolvedServerConfig {
        ResolvedServerConfig {
            server_name: name.to_string(),
            config: Arc::new(BridgeServerConfig {
                cmd: Some(vec![name.to_string()]),
                languages: None,
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                force_start: None,
                enabled: None,
                settings: None,
            }),
        }
    }

    fn ctx(priorities: &[&str], max_fan_out: Option<usize>) -> HostRequestContext {
        HostRequestContext {
            uri: url::Url::parse("file:///doc.md").unwrap(),
            language_id: "markdown".to_string(),
            text: Arc::from("# doc"),
            configs: vec![config("a"), config("b"), config("c")],
            priorities: priorities.iter().map(|p| (*p).to_string()).collect(),
            strategy: AggregationStrategy::Preferred,
            max_fan_out,
            upstream_request_id: None,
        }
    }

    fn names(selected: &[ResolvedServerConfig]) -> Vec<&str> {
        selected.iter().map(|c| c.server_name.as_str()).collect()
    }

    /// The delivery plan a forwarded notification follows
    /// (custom-method-host-forwarding): walk order, `"*"` expansion, the
    /// `maxFanOut` cap, and the `[]` kill switch — pinned here because the
    /// e2e can only observe arrivals, never a delivery that did NOT happen.
    #[test]
    fn select_host_servers_follows_priorities_cap_and_kill_switch() {
        assert_eq!(
            names(&select_host_servers(&ctx(&["b", "a"], None))),
            ["b", "a"]
        );
        assert_eq!(
            names(&select_host_servers(&ctx(&["b", "a"], Some(1)))),
            ["b"],
            "maxFanOut = 1 keeps only the first in priority order"
        );
        assert_eq!(
            names(&select_host_servers(&ctx(&["c", "*"], None))),
            ["c", "a", "b"],
            "`*` expands to the unlisted rest"
        );
        assert!(select_host_servers(&ctx(&[], None)).is_empty());
        assert!(select_host_servers(&ctx(&["b", "a"], Some(0))).is_empty());
    }
}
