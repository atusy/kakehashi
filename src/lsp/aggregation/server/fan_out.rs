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

use super::priority::PriorityEntry;

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
    /// Bridge-minted `workDoneToken` to hand this downstream so its `$/progress`
    /// routes to the request's aggregator (ls-bridge-client-progress); `None`
    /// when the request carries no client `workDoneToken`.
    pub(crate) client_progress_token: Option<tower_lsp_server::ls_types::NumberOrString>,
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

/// Select which servers to fan out to: the expanded priority entries are the
/// membership allowlist and spawn order (aggregation-priorities-wildcard).
///
/// Expansion ([`super::priority::expand_priorities`]) already filtered to
/// configured names, deduplicated, and applied `max_fan_out`
/// ([`super::priority::truncate_entries`]); this is a pure name → config
/// lookup in walk order.
pub(crate) fn select_servers(
    configs: &[ResolvedServerConfig],
    entries: &[PriorityEntry],
) -> Vec<ResolvedServerConfig> {
    // Build a map for O(1) lookup of configs by server name
    let config_map: std::collections::HashMap<&str, &ResolvedServerConfig> = configs
        .iter()
        .map(|c| (c.server_name.as_str(), c))
        .collect();

    entries
        .iter()
        .flat_map(|entry| match entry {
            PriorityEntry::Server(name) => std::slice::from_ref(name),
            PriorityEntry::Rest(names) => names.as_slice(),
        })
        .filter_map(|name| config_map.get(name.as_str()).map(|cfg| (*cfg).clone()))
        .collect()
}

/// Spawn one task per selected server, returning a `JoinSet` for collection.
///
/// `entries` is the expanded priority walk (see
/// [`super::priority::expand_priorities`]) — its flattened names are both the
/// membership allowlist and the spawn order. The caller supplies a closure
/// that receives a [`FanOutTask`] and returns the handler-specific future.
/// Each task result is wrapped in [`TaggedResult`] so fan-in strategies can
/// identify which server produced it.
#[must_use = "the JoinSet must be passed to a collection strategy"]
pub(crate) fn fan_out<T, F, Fut>(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    selected: &[ResolvedServerConfig],
    client_progress_tokens: Option<
        &std::collections::HashMap<String, tower_lsp_server::ls_types::NumberOrString>,
    >,
) -> JoinSet<TaggedResult<T>>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let mut join_set = JoinSet::new();
    for config in selected {
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
            client_progress_token: client_progress_tokens
                .and_then(|m| m.get(&server_name).cloned()),
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
                root_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
            }),
        }
    }

    fn names(configs: &[ResolvedServerConfig]) -> Vec<&str> {
        configs.iter().map(|c| c.server_name.as_str()).collect()
    }

    fn server(name: &str) -> PriorityEntry {
        PriorityEntry::Server(name.to_string())
    }

    fn rest(group: &[&str]) -> PriorityEntry {
        PriorityEntry::Rest(group.iter().map(|n| n.to_string()).collect())
    }

    #[test]
    fn select_servers_follows_entry_walk_order() {
        let configs = vec![
            make_config("alpha"),
            make_config("beta"),
            make_config("gamma"),
        ];
        let entries = vec![server("gamma"), rest(&["alpha", "beta"])];
        let result = select_servers(&configs, &entries);
        assert_eq!(names(&result), &["gamma", "alpha", "beta"]);
    }

    #[test]
    fn select_servers_is_an_allowlist() {
        // Entries without beta: beta must not be selected.
        let configs = vec![make_config("alpha"), make_config("beta")];
        let entries = vec![server("alpha")];
        let result = select_servers(&configs, &entries);
        assert_eq!(names(&result), &["alpha"]);
    }

    #[test]
    fn select_servers_empty_entries_selects_nothing() {
        // priorities = [] (kill switch) expands to no entries.
        let configs = vec![make_config("alpha")];
        let result = select_servers(&configs, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn select_servers_skips_names_without_config() {
        // Defensive: expansion guarantees configured names, but a stale name
        // must not panic the lookup.
        let configs = vec![make_config("alpha")];
        let entries = vec![server("ghost"), server("alpha")];
        let result = select_servers(&configs, &entries);
        assert_eq!(names(&result), &["alpha"]);
    }
}
