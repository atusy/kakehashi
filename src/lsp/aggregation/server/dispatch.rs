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
    ClientProgressAggregator, ClientProgressDeregisterGuard, ClientProgressRegistry,
    LanguageServerPool, ResolvedServerConfig,
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
    // candidate is suppressed by handing it no `workDoneToken`, so it reports no
    // `$/progress` *for this request* (a downstream may still drive its own
    // server-declared progress — that path is unaffected). With no tracked source
    // (an all-`Rest` N>1 fan-out) nothing is minted and the editor sees no
    // progress for the request.
    let tracked = tracked_progress_source(selected, entries)?;
    let aggregator = Arc::new(Mutex::new(ClientProgressAggregator::new(client_token)));
    let registry = Arc::clone(pool.client_progress_registry());
    let token = registry.register(Arc::clone(&aggregator));
    let mut map = HashMap::new();
    map.insert(tracked.to_string(), token.clone());
    let guard =
        ClientProgressDeregisterGuard::new(registry, vec![token], aggregator, pool.upstream_tx());
    Some((map, guard))
}

/// The server whose `$/progress` the editor sees under *preferred*
/// (ls-bridge-client-progress):
/// - **N = 1**: the sole selected server — its real `Begin` is safe to forward
///   even if wildcard-selected (a single downstream has no racing contender).
/// - **N > 1**: the **anchor** — but only when the **top-priority participating
///   candidate is a *named* server**. `Rest`/wildcard members are never anchors
///   (no safe a-priori title for a latency race), and the winner is *usually* the
///   anchor — so a named server that a `Rest` group **outranks** (e.g. priorities
///   `["*", "zzz"]`, where the wildcard wins first) is not a valid anchor:
///   tracking it would show its progress while a wildcard member delivers the
///   result. In that case, and for an all-`Rest` fan-out, there is no tracked
///   source.
///
/// `None` means no progress is shown for the request. This picks a **single
/// fixed anchor**; dynamic fall-through re-anchoring (re-tracking the next named
/// candidate when this one returns empty) is a later stage — on fall-through the
/// open `Begin` is instead closed by the synthetic terminal `End`.
fn tracked_progress_source<'a>(
    selected: &'a [ResolvedServerConfig],
    entries: &'a [PriorityEntry],
) -> Option<&'a str> {
    if let [only] = selected {
        return Some(&only.server_name);
    }
    let participates = |name: &str| selected.iter().any(|c| c.server_name == name);
    // Walk in priority order to the first *participating* entry: if it is a named
    // server, it outranks any wildcard and is the anchor; if it is a `Rest` group
    // with a participating member, the top contender is a wildcard, so there is no
    // a-priori anchor. (Non-participating entries — e.g. an empty `Rest` — are
    // skipped.)
    for entry in entries {
        match entry {
            PriorityEntry::Server(name) if participates(name) => return Some(name),
            PriorityEntry::Rest(names) if names.iter().any(|n| participates(n)) => return None,
            _ => {}
        }
    }
    None
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

/// Like [`dispatch_preferred`], but routes client progress to an
/// externally-supplied per-server token map instead of minting its own — and owns
/// no teardown guard (the caller does).
///
/// For a multi-region request (e.g. `documentSymbol`), the handler creates ONE
/// shared aggregator + ONE teardown guard for the whole request and mints a token
/// per region with [`mint_region_progress_source`]; each region's dispatch routes
/// its tracked source's `$/progress` to that shared aggregator via `tokens`. The
/// shared aggregator's winner rule then shows one coherent lifecycle across all
/// regions (ls-bridge-client-progress).
pub(crate) async fn dispatch_preferred_with_tokens<T, F, Fut>(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    f: F,
    is_nonempty: impl Fn(&T) -> bool,
    cancel_rx: Option<CancelReceiver>,
    tokens: HashMap<String, NumberOrString>,
) -> FanInResult<T>
where
    T: Send + 'static,
    F: Fn(FanOutTask) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let entries = expanded_entries(ctx);
    let selected = select_servers(&ctx.configs, &entries);
    let mut join_set = fan_out(ctx, pool, f, &selected, Some(&tokens));
    preferred::preferred(&mut join_set, is_nonempty, &entries, cancel_rx).await
}

/// Mint the client-progress token for one region of a multi-region request: pick
/// the region's tracked source (sole server / named anchor; `None` for an
/// all-`Rest` fan-out) and register a bridge token to the request's shared
/// `aggregator`. Returns the `server → token` map for
/// [`dispatch_preferred_with_tokens`]. The caller must deregister the returned
/// token via its shared teardown guard.
pub(crate) fn mint_region_progress_source(
    ctx: &DocumentRequestContext,
    registry: &ClientProgressRegistry,
    aggregator: &Arc<Mutex<ClientProgressAggregator>>,
) -> Option<HashMap<String, NumberOrString>> {
    let entries = expanded_entries(ctx);
    let selected = select_servers(&ctx.configs, &entries);
    let tracked = tracked_progress_source(&selected, &entries)?;
    let token = registry.register(Arc::clone(aggregator));
    Some(HashMap::from([(tracked.to_string(), token)]))
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
    // Counts panicking tasks (`JoinError`) even on partial-success `Done`, so a
    // panicking server still drives CLI exit 2 (#506). The caller still counts
    // I/O failures in-task; only panics feed this.
    panic_sink: Option<&std::sync::Arc<std::sync::atomic::AtomicUsize>>,
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
    concatenated::concatenated(&mut join_set, &ordering, cancel_rx, log_target, panic_sink).await
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
            Some("rust-analyzer")
        );
    }

    #[test]
    fn tracked_source_is_the_named_anchor_at_n_gt_1() {
        let selected = [config("ra"), config("typos")];
        let entries = [
            PriorityEntry::Server("ra".to_string()),
            PriorityEntry::Rest(vec!["typos".to_string()]),
        ];
        assert_eq!(tracked_progress_source(&selected, &entries), Some("ra"));
    }

    #[test]
    fn first_named_anchor_wins_over_a_later_named_one() {
        let selected = [config("ra"), config("clangd")];
        let entries = [
            PriorityEntry::Server("ra".to_string()),
            PriorityEntry::Server("clangd".to_string()),
        ];
        assert_eq!(tracked_progress_source(&selected, &entries), Some("ra"));
    }

    #[test]
    fn no_anchor_when_a_rest_group_outranks_the_named_candidate() {
        // Walk order `[Rest, Server]` (the `["*", "zzz"]` config shape): the
        // wildcard group is the top-priority contender (it wins first), so the
        // lower-priority named server is NOT a valid anchor — tracking it would
        // show its progress while a wildcard member delivers the result.
        let selected = [config("a"), config("zzz")];
        let entries = [
            PriorityEntry::Rest(vec!["a".to_string()]),
            PriorityEntry::Server("zzz".to_string()),
        ];
        assert_eq!(tracked_progress_source(&selected, &entries), None);
    }

    #[test]
    fn no_tracked_source_for_all_rest_at_n_gt_1() {
        // Rest members are never anchors → no progress shown.
        let selected = [config("a"), config("b")];
        let entries = [PriorityEntry::Rest(vec!["a".to_string(), "b".to_string()])];
        assert_eq!(tracked_progress_source(&selected, &entries), None);
    }

    /// A `DocumentRequestContext` carrying `token`, with otherwise inert region
    /// metadata — `setup_client_progress` reads only the token, and `fan_out`
    /// reads the region fields but does not depend on their values here.
    fn ctx_with_token(token: Option<NumberOrString>) -> DocumentRequestContext {
        use crate::config::settings::AggregationStrategy;
        use crate::language::injection::{CacheableInjectionRegion, ResolvedInjection};

        DocumentRequestContext {
            uri: url::Url::parse("file:///suppression.md").expect("valid URI"),
            resolved: ResolvedInjection {
                region: CacheableInjectionRegion {
                    language: "rust".to_string(),
                    byte_range: 0..0,
                    line_range: 0..0,
                    start_column: 0,
                    region_id: "r0".to_string(),
                    content_hash: 0,
                },
                injection_language: "rust".to_string(),
                virtual_content: String::new(),
                line_column_offsets: vec![],
            },
            configs: vec![],
            upstream_request_id: None,
            priorities: vec![],
            strategy: AggregationStrategy::Preferred,
            max_fan_out: None,
            client_progress_token: token,
        }
    }

    /// Suppression regression (#442): under *preferred*, `setup_client_progress`
    /// mints **exactly one** bridge token — for the named anchor only — and
    /// `fan_out` hands that token to the anchor while every other candidate gets
    /// `None`, so a suppressed server reports no `$/progress` for the request.
    #[tokio::test]
    async fn setup_client_progress_tracks_only_the_anchor_and_fan_out_suppresses_the_rest() {
        use crate::error::LockResultExt;

        let pool = Arc::new(LanguageServerPool::new());
        let ctx = ctx_with_token(Some(NumberOrString::String("editor-wd".to_string())));
        // N>1: named anchor `ra` ahead of a `Rest` member `typos`.
        let selected = [config("ra"), config("typos")];
        let entries = [
            PriorityEntry::Server("ra".to_string()),
            PriorityEntry::Rest(vec!["typos".to_string()]),
        ];

        let (map, _guard) = setup_client_progress(&ctx, &pool, &selected, &entries)
            .expect("a workDoneToken + a named anchor yields a tracked source");

        assert_eq!(map.len(), 1, "exactly one bridge token is minted");
        assert!(map.contains_key("ra"), "the named anchor is tracked");
        assert!(!map.contains_key("typos"), "the Rest member is suppressed");
        assert!(
            pool.client_progress_registry().route(&map["ra"]).is_some(),
            "the minted token is registered in the routing table"
        );

        // fan_out hands the anchor its token and every other server `None`.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_closure = Arc::clone(&seen);
        let mut join_set = fan_out(
            &ctx,
            Arc::clone(&pool),
            move |task: FanOutTask| {
                let seen = Arc::clone(&seen_for_closure);
                async move {
                    seen.lock()
                        .recover_poison("suppression test seen")
                        .push((task.server_name.clone(), task.client_progress_token.clone()));
                    Ok::<(), io::Error>(())
                }
            },
            &selected,
            Some(&map),
        );
        while join_set.join_next().await.is_some() {}

        let got = seen.lock().recover_poison("suppression test seen").clone();
        let anchor = got
            .iter()
            .find(|(name, _)| name == "ra")
            .expect("the anchor fanned out");
        let suppressed = got
            .iter()
            .find(|(name, _)| name == "typos")
            .expect("the Rest member fanned out");
        assert_eq!(
            anchor.1.as_ref(),
            map.get("ra"),
            "the anchor receives exactly the minted bridge progress token"
        );
        assert!(
            suppressed.1.is_none(),
            "the suppressed server receives no progress token"
        );
    }
}
