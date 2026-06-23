//! Server-level aggregation dispatch.
//!
//! [`dispatch_preferred()`] combines [`fan_out()`] with the preferred
//! strategy, giving each handler a single call-site for the common pattern.
//!
//! [`dispatch_concatenated()`] combines [`fan_out()`] with the concatenated
//! strategy, collecting every successful result from all selected servers.
//!
//! Both expand `ctx.priorities` once (aggregation-priorities-wildcard) and
//! feed the same expansion to fan-out (membership + spawn order) and fan-in
//! (decision order), so the two sides can never disagree.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};

use tower_lsp_server::ls_types::NumberOrString;

use crate::lsp::bridge::{
    ClientProgressAggregator, ClientProgressDeregisterGuard, LanguageServerPool,
    ResolvedServerConfig,
};
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::request_id::CancelReceiver;

use super::fan_in::FanInResult;
use super::fan_in::{concatenated, preferred};
use super::fan_out::{FanOutTask, fan_out, select_servers};
use super::priority::{PriorityEntry, entry_names, expand_priorities, truncate_entries};

/// Set up client-progress aggregation for a fanned-out request that carries a
/// `workDoneToken`: create the aggregator, mint+register a bridge token for the
/// **tracked source** only (the sole server at N=1, or the named anchor at N>1),
/// and return the `server → token` map (handed to `fan_out`) plus a guard that
/// deregisters the token when the dispatch returns (ls-bridge-client-progress).
/// Returns `None` when there is no client token or no tracked source.
fn setup_client_progress(
    ctx: &DocumentRequestContext,
    pool: &LanguageServerPool,
    selected: &[ResolvedServerConfig],
    entries: &[PriorityEntry],
) -> Option<(
    HashMap<String, NumberOrString>,
    ClientProgressDeregisterGuard,
)> {
    let client_token = ctx.client_progress_token.clone()?;
    // Under *preferred*, only the tracked source's progress is shown; every other
    // candidate is suppressed by handing it no `workDoneToken` (so it emits no
    // `$/progress` — no wasted traffic). With no tracked source (an all-`Rest`
    // N>1 fan-out) nothing is minted and the editor sees no progress.
    let tracked = tracked_progress_source(selected, entries)?;
    let aggregator = Arc::new(Mutex::new(ClientProgressAggregator::new(client_token)));
    let registry = Arc::clone(pool.client_progress_registry());
    let token = registry.register(Arc::clone(&aggregator));
    let mut map = HashMap::new();
    map.insert(tracked, token.clone());
    let guard =
        ClientProgressDeregisterGuard::new(registry, vec![token], aggregator, pool.upstream_tx());
    Some((map, guard))
}

/// The server whose `$/progress` the editor sees under *preferred*
/// (ls-bridge-client-progress):
/// - **N = 1**: the sole selected server — its real `Begin` is safe to forward
///   even if wildcard-selected (a single downstream has no racing contender).
/// - **N > 1**: the **anchor** — the highest-priority *named* candidate in the
///   priority walk. `Rest`/wildcard members are never anchors (no safe a-priori
///   title for a latency race), so an all-`Rest` fan-out has no tracked source.
///
/// `None` means no progress is shown for the request.
fn tracked_progress_source(
    selected: &[ResolvedServerConfig],
    entries: &[PriorityEntry],
) -> Option<String> {
    if let [only] = selected {
        return Some(only.server_name.clone());
    }
    // The first *named* entry that actually participates (is selected). Rest
    // entries are skipped; if no named server is selected the fan-out is
    // all-wildcard and has no anchor.
    entries.iter().find_map(|entry| match entry {
        PriorityEntry::Server(name)
            if selected.iter().any(|config| &config.server_name == name) =>
        {
            Some(name.clone())
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::BridgeServerConfig;

    fn config(name: &str) -> ResolvedServerConfig {
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

    #[test]
    fn tracked_source_is_the_sole_server_at_n1() {
        // N=1 relays the sole server even when wildcard-selected.
        let selected = [config("rust-analyzer")];
        let entries = [PriorityEntry::Rest(vec!["rust-analyzer".to_string()])];
        assert_eq!(
            tracked_progress_source(&selected, &entries),
            Some("rust-analyzer".to_string())
        );
    }

    #[test]
    fn tracked_source_is_the_named_anchor_at_n_gt_1() {
        let selected = [config("ra"), config("typos")];
        let entries = [
            PriorityEntry::Server("ra".to_string()),
            PriorityEntry::Rest(vec!["typos".to_string()]),
        ];
        assert_eq!(
            tracked_progress_source(&selected, &entries),
            Some("ra".to_string())
        );
    }

    #[test]
    fn first_named_anchor_wins_over_a_later_named_one() {
        let selected = [config("ra"), config("clangd")];
        let entries = [
            PriorityEntry::Server("ra".to_string()),
            PriorityEntry::Server("clangd".to_string()),
        ];
        assert_eq!(
            tracked_progress_source(&selected, &entries),
            Some("ra".to_string())
        );
    }

    #[test]
    fn no_tracked_source_for_all_rest_at_n_gt_1() {
        // Rest members are never anchors → no progress shown.
        let selected = [config("a"), config("b")];
        let entries = [PriorityEntry::Rest(vec!["a".to_string(), "b".to_string()])];
        assert_eq!(tracked_progress_source(&selected, &entries), None);
    }
}

/// Expand `ctx.priorities` into the priority walk for this request:
/// allowlist + `"*"` expansion against the configured servers, then the
/// `max_fan_out` cap.
fn expanded_entries(ctx: &DocumentRequestContext) -> Vec<PriorityEntry> {
    truncate_entries(
        expand_priorities(&ctx.priorities, &ctx.configs),
        ctx.max_fan_out,
    )
}

/// Filter `priorities` to names present in `configs`, dropping duplicates
/// (first occurrence wins, order preserved).
///
/// Used by the concatenated formatting pipeline, where `priorities` is the
/// membership allowlist plus application order and the `"*"` wildcard is not
/// permitted (no deterministic expansion order — see
/// aggregation-priorities-wildcard). Callers strip `"*"` before this filter.
///
/// Kept slice-based so the formatting gating decision
/// ([`crate::lsp::lsp_impl::text_document::formatting`]) and its unit tests
/// can reuse the exact allowlist rule without building a full context.
pub(crate) fn effective_priorities_from(
    priorities: &[String],
    configs: &[ResolvedServerConfig],
) -> Vec<String> {
    let configured: HashSet<&str> = configs.iter().map(|c| c.server_name.as_str()).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    priorities
        .iter()
        .filter(|name| configured.contains(name.as_str()) && seen.insert(name.as_str()))
        .cloned()
        .collect()
}

/// Server-level aggregation entry point using the preferred strategy.
///
/// Fans out one task per selected server and returns the highest-priority
/// non-empty result. A `"*"` group decides first-win among its members.
/// `priorities = []` selects nothing and yields `NoResult`.
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
    let entries = expanded_entries(ctx);
    // Compute the selected servers once and share it with both client-progress
    // setup and fan-out (avoids a second `select_servers` on the dispatch path).
    let selected = select_servers(&ctx.configs, &entries);
    // `setup_client_progress` bridges client progress for the tracked source only
    // (sole server at N=1, named anchor at N>1) when the request carries a
    // `workDoneToken`; every other candidate is suppressed (handed no token).
    let client_progress = setup_client_progress(ctx, &pool, &selected, &entries);
    let cp_tokens = client_progress.as_ref().map(|(m, _guard)| m);
    let mut join_set = fan_out(ctx, pool, f, &selected, cp_tokens);
    let result = preferred::preferred(&mut join_set, is_nonempty, &entries, cancel_rx).await;
    // Hold the client-progress guard across the await above (it owns the routing
    // and the synthetic-terminal-End-on-teardown); tear it down only now, after
    // the request has settled. The explicit drop documents that the guard must
    // outlive the fan-in — otherwise a downstream `$/progress` during the request
    // would have nowhere to route.
    drop(client_progress);
    result
}

/// Server-level aggregation entry point using the concatenated strategy.
///
/// Fans out one task per selected server and collects every successful
/// result, ordered by the expanded priority walk (named servers first in
/// list order, then the `"*"` group in config order).
/// Returns `Done(vec)` when at least one succeeds, `NoResult` when all fail,
/// or `Cancelled` on cancel notification.
pub(crate) async fn dispatch_concatenated<T, F, Fut>(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    cancel_rx: Option<CancelReceiver>,
    log_target: Option<&str>,
) -> FanInResult<Vec<T>>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let entries = expanded_entries(ctx);
    let ordering = entry_names(&entries);
    let selected = select_servers(&ctx.configs, &entries);
    // Client progress under concatenated is a later stage; pass no tokens so
    // downstream `$/progress` is dropped (non-regressing).
    let mut join_set = fan_out(ctx, pool, f, &selected, None);
    concatenated::concatenated(&mut join_set, &ordering, cancel_rx, log_target).await
}
