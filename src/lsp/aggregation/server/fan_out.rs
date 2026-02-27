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
use crate::lsp::bridge::RegionOffset;
use crate::lsp::bridge::ResolvedServerConfig;
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
    pub(crate) offset: RegionOffset,
    pub(crate) virtual_content: String,
    pub(crate) upstream_id: Option<UpstreamId>,
}

/// Result tagged with the originating server name.
///
/// Enables priority-aware fan-in strategies by preserving which server
/// produced each result. The `io::Result` is inside the tag so that
/// `JoinError` (panic) is the only outer error from the JoinSet.
pub(crate) struct TaggedResult<T> {
    pub(crate) server_name: String,
    pub(crate) value: io::Result<T>,
}

/// Select which servers to fan out to, respecting priority ordering and max fan-out limit.
///
/// When `max_fan_out` is `None`, all configs are returned (in priority order).
/// When `max_fan_out` is `Some(0)`, an empty list is returned (fan-out disabled).
/// When `max_fan_out` is `Some(n)`, at most `n` configs are returned.
///
/// Priority servers appear first (in the order listed in `priorities`),
/// followed by remaining servers in their original order.
pub(crate) fn select_servers(
    configs: &[ResolvedServerConfig],
    priorities: &[String],
    max_fan_out: Option<usize>,
) -> Vec<ResolvedServerConfig> {
    let mut result: Vec<ResolvedServerConfig> = Vec::with_capacity(configs.len());
    let mut added: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(configs.len());

    // Build a map for O(1) lookup of configs by server name
    let config_map: std::collections::HashMap<&str, &ResolvedServerConfig> = configs
        .iter()
        .map(|c| (c.server_name.as_str(), c))
        .collect();

    // Add priority servers in priority order (skip unknown/duplicate names)
    for name in priorities {
        if let Some(cfg) = config_map.get(name.as_str())
            && added.insert(&cfg.server_name)
        {
            result.push((*cfg).clone());
        }
    }

    // Add remaining servers in their original order
    for cfg in configs {
        if !added.contains(cfg.server_name.as_str()) {
            result.push(cfg.clone());
        }
    }

    // Truncate to max_fan_out if specified
    if let Some(limit) = max_fan_out {
        result.truncate(limit);
    }
    result
}

/// Spawn one task per matching server, returning a `JoinSet` for collection.
///
/// Centralises the per-server clone boilerplate that was previously duplicated
/// in every fan-out handler. The caller supplies a closure that receives a
/// [`FanOutTask`] and returns the handler-specific future.
///
/// Each task result is wrapped in [`TaggedResult`] so fan-in strategies
/// can identify which server produced each result.
#[must_use = "the JoinSet must be passed to a collection strategy"]
pub(crate) fn fan_out<T, F, Fut>(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
) -> JoinSet<TaggedResult<T>>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let mut join_set = JoinSet::new();
    for config in &ctx.configs {
        let server_name = config.server_name.clone();
        let task = FanOutTask {
            pool: Arc::clone(&pool),
            server_name: server_name.clone(),
            server_config: Arc::clone(&config.config),
            uri: ctx.uri.clone(),
            injection_language: ctx.resolved.injection_language.clone(),
            region_id: ctx.resolved.region.region_id.clone(),
            offset: RegionOffset::with_per_line_offsets(
                ctx.resolved.region.line_range.start,
                ctx.resolved.line_column_offsets.clone(),
            ),
            virtual_content: ctx.resolved.virtual_content.clone(),
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
    join_set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(name: &str) -> ResolvedServerConfig {
        ResolvedServerConfig {
            server_name: name.to_string(),
            config: Arc::new(BridgeServerConfig {
                cmd: vec![name.to_string()],
                languages: vec![],
                initialization_options: None,
                workspace_type: None,
            }),
        }
    }

    fn names(configs: &[ResolvedServerConfig]) -> Vec<&str> {
        configs.iter().map(|c| c.server_name.as_str()).collect()
    }

    #[test]
    fn select_servers_no_limit_returns_all() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let result = select_servers(&configs, &[], None);
        assert_eq!(names(&result), &["alpha", "beta", "gamma"]);
    }

    #[test]
    fn select_servers_zero_returns_empty() {
        let configs = vec![make_config("alpha"), make_config("beta")];
        let result = select_servers(&configs, &[], Some(0));
        assert!(result.is_empty());
    }

    #[test]
    fn select_servers_truncates_to_n() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let result = select_servers(&configs, &[], Some(2));
        assert_eq!(names(&result), &["alpha", "beta"]);
    }

    #[test]
    fn select_servers_priority_servers_first() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let priorities = vec!["gamma".to_string(), "alpha".to_string()];
        let result = select_servers(&configs, &priorities, None);
        assert_eq!(names(&result), &["gamma", "alpha", "beta"]);
    }

    #[test]
    fn select_servers_non_priority_order_preserved() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
            make_config("delta"),
        ];
        let priorities = vec!["gamma".to_string()];
        let result = select_servers(&configs, &priorities, None);
        // gamma first (priority), then remaining in original order
        assert_eq!(names(&result), &["gamma", "alpha", "beta", "delta"]);
    }

    #[test]
    fn select_servers_truncate_after_priority_reordering() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let priorities = vec!["gamma".to_string()];
        let result = select_servers(&configs, &priorities, Some(2));
        assert_eq!(names(&result), &["gamma", "alpha"]);
    }

    #[test]
    fn select_servers_limit_larger_than_configs_returns_all() {
        let configs = vec![make_config("alpha"), make_config("beta")];
        let result = select_servers(&configs, &[], Some(10));
        assert_eq!(names(&result), &["alpha", "beta"]);
    }

    #[test]
    fn select_servers_unknown_priority_ignored() {
        let configs = vec![make_config("alpha"), make_config("beta")];
        let priorities = vec!["unknown".to_string(), "alpha".to_string()];
        let result = select_servers(&configs, &priorities, None);
        assert_eq!(names(&result), &["alpha", "beta"]);
    }

    #[test]
    fn select_servers_empty_configs_returns_empty() {
        let result = select_servers(&[], &["alpha".to_string()], Some(5));
        assert!(result.is_empty());
    }

    #[test]
    fn select_servers_duplicate_priority_added_only_once() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let priorities = vec!["alpha".to_string(), "alpha".to_string(), "beta".to_string()];
        let result = select_servers(&configs, &priorities, None);
        // alpha should appear only once despite being in priorities twice
        assert_eq!(names(&result), &["alpha", "beta", "gamma"]);
    }

    #[test]
    fn select_servers_all_in_priorities_uses_priority_order() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let priorities = vec!["gamma".to_string(), "beta".to_string(), "alpha".to_string()];
        let result = select_servers(&configs, &priorities, None);
        assert_eq!(names(&result), &["gamma", "beta", "alpha"]);
    }
}
