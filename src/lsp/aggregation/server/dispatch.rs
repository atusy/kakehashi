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
/// `workDoneToken`: create the aggregator, mint+register a bridge token per
/// selected server, and return the `server → token` map (handed to `fan_out`)
/// plus a guard that deregisters the tokens when the dispatch returns
/// (ls-bridge-client-progress). Returns `None` when there is no client token.
fn setup_client_progress(
    ctx: &DocumentRequestContext,
    pool: &LanguageServerPool,
    selected: &[ResolvedServerConfig],
) -> Option<(
    HashMap<String, NumberOrString>,
    ClientProgressDeregisterGuard,
)> {
    let client_token = ctx.client_progress_token.clone()?;
    // Stage 1 bridges client progress only for the single-downstream case. With
    // more than one server we mint nothing — those downstreams get no
    // `workDoneToken` and emit no `$/progress` — so multi-server fan-out stays
    // exactly as today (no wasted traffic) until the preferred/concatenated
    // stages land.
    if selected.len() != 1 {
        return None;
    }
    let aggregator = Arc::new(Mutex::new(ClientProgressAggregator::new(
        client_token,
        selected.len(),
    )));
    let registry = Arc::clone(pool.client_progress_registry());
    let mut map = HashMap::new();
    let mut minted = Vec::new();
    for config in selected {
        let token = registry.register(Arc::clone(&aggregator));
        minted.push(token.clone());
        map.insert(config.server_name.clone(), token);
    }
    let guard =
        ClientProgressDeregisterGuard::new(registry, minted, aggregator, pool.upstream_tx());
    Some((map, guard))
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
    // Stage 1: `setup_client_progress` bridges client progress only for the
    // single-downstream case (it returns `None` for N>1, keeping fan-out
    // non-regressing) and only when the request carries a `workDoneToken`.
    let client_progress = setup_client_progress(ctx, &pool, &selected);
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
