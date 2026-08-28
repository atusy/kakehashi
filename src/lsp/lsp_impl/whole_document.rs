//! Shared fan-out for whole-document bridged requests.
//!
//! documentLink, documentColor, foldingRange, and codeLens all follow the same shape: no
//! position parameter, so the request fans out to every resolved bridge
//! virtual document, uses the preferred strategy within each document, and
//! concatenates those results. This module hosts that shape once; the per-method
//! handlers supply only the LSP method name and the downstream send call.
//!
//! The fan-out is the virt layer of the resolved layer order
//! (cross-layer-aggregation); the host layer (host-document-bridge) bridges
//! the host document itself with the real URI and the response verbatim.
//! `preferred` returns the highest-priority non-empty layer, while
//! `concatenated` merges every selected layer's list in priority order.

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{NumberOrString, Uri};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, dispatch_host_preferred, dispatch_preferred,
    dispatch_preferred_with_tokens, expand_priorities, mint_region_progress_source,
    truncate_entries,
};
use crate::lsp::bridge::{
    ClientProgressAggregator, ClientProgressDeregisterGuard, HostDocument, ResolvedServerConfig,
};

use super::bridge_context::DocumentRequestContext;
use super::{Kakehashi, uri_to_url};

pub(super) struct HostWholeDocumentResponse<T> {
    pub(super) items: Vec<T>,
    pub(super) server_name: String,
    pub(super) host_uri: url::Url,
    pub(super) host_text: Arc<str>,
    pub(super) incarnation: Option<u64>,
    pub(super) connection_generation: u64,
    pub(super) handle: Arc<crate::lsp::bridge::ConnectionHandle>,
}

#[derive(Clone, Copy)]
pub(super) struct WholeDocumentSnapshotIdentity {
    pub(super) incarnation: u64,
    pub(super) parsed_version: u64,
    pub(super) generation: u64,
}

impl Kakehashi {
    /// Fan out a whole-document bridged request to all injection regions.
    ///
    /// Within the virt layer, each region uses the preferred server strategy
    /// and the region results are concatenated because regions are disjoint.
    /// The virt arm returns `None` when there are no injection regions, no
    /// configured virt servers, or every region returned empty. The final
    /// cross-layer result still follows the configured layer strategy, so the
    /// host layer can answer when the virt arm is empty and `concatenated`
    /// can merge non-empty virt and host lists.
    ///
    /// `client_progress_token` is the editor's `workDoneToken`, if any: when
    /// `Some`, one shared aggregator relays the first region to begin as a single
    /// `Begin → … → End` on that token (ls-bridge-client-progress); `None` (the
    /// fast methods that don't advertise `workDoneProgress`) keeps prior behavior.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn whole_document_fan_out<T, N, F, Fut, P, H, R, M>(
        &self,
        lsp_uri: &Uri,
        method_name: &'static str,
        raw_params: serde_json::Value,
        client_progress_token: Option<NumberOrString>,
        expected_snapshot: Option<WholeDocumentSnapshotIdentity>,
        bridge_attempted: Option<Arc<std::sync::atomic::AtomicBool>>,
        require_all_layers: bool,
        preserve_empty: bool,
        nested_regions_first: bool,
        native: N,
        send: F,
        parse_host: P,
        on_host_winner: H,
        merge_regions: R,
        merge_layers: M,
    ) -> Result<Option<Vec<T>>>
    where
        T: Send + 'static,
        N: Future<Output = Result<Option<Vec<T>>>>,
        F: Fn(FanOutTask) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = io::Result<Option<Vec<T>>>> + Send + 'static,
        P: Fn(serde_json::Value) -> Option<Vec<T>> + Clone + Send + 'static,
        H: Fn(HostWholeDocumentResponse<T>) -> Option<Vec<T>> + Clone + Send + 'static,
        R: Fn(Vec<T>, Vec<T>) -> Vec<T> + Copy + Send,
        M: Fn(Vec<T>, Vec<T>) -> Vec<T>,
    {
        let host_bridge_attempted = bridge_attempted;
        let virt = async {
            // Convert ls_types::Uri to url::Url for internal use
            let Ok(uri) = uri_to_url(lsp_uri) else {
                log::warn!("Invalid URI in {}: {}", method_name, lsp_uri.as_str());
                return Ok(None);
            };

            log::debug!("{} called for {}", method_name, uri);

            // Resolve a CURRENT parse snapshot with a bounded wait (parse-snapshot
            // ADR §3). This family is nominally serve-stale, but the region
            // resolution below MINTS tracker ULIDs — a live-position index — so a
            // stale snapshot must not feed it (a stale read never mints); until the
            // snapshot-owned region ordinals land, staleness degrades to `Ok(None)`
            // (the native/empty fallback), self-correcting on the client's next
            // request. The former parse-on-demand fallback is gone: readers never
            // parse inline.
            let snapshot = match self
                .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
                .await
            {
                crate::lsp::lsp_impl::snapshot_read::SnapshotWait::Current(snapshot) => snapshot,
                _ => {
                    log::debug!("{}: no current parse snapshot for {}", method_name, uri);
                    return Ok(None);
                }
            };
            if expected_snapshot.is_some_and(|expected| {
                snapshot.incarnation != expected.incarnation
                    || snapshot.parsed_version != expected.parsed_version
                    || self.cache.semantic_token_generation() != expected.generation
            }) {
                return Ok(None);
            }
            let Some(language_name) = snapshot.language.clone() else {
                log::debug!("{}: No language detected", method_name);
                return Ok(None);
            };
            let Some(snapshot_tree) = snapshot.tree.as_ref() else {
                log::debug!("{}: no tree (parser unavailable) for {}", method_name, uri);
                return Ok(None);
            };

            // Get injection query to detect injection regions
            let Some(injection_query) = self.language.injection_query(&language_name) else {
                return Ok(None);
            };

            // Collect all injection regions — from THIS snapshot's own
            // resolved_regions (generation-gated), never a store re-read: a
            // parse publishing between the wait above and a store lookup
            // could pair this snapshot's tree/text with a NEWER snapshot's
            // regions. Snapshot immutability makes tree, text, and regions
            // one value; absent/reload-stale falls back inline over the same
            // tree.
            let all_regions = match snapshot
                .resolved_regions
                .as_ref()
                .filter(|(stamped, _)| *stamped == self.cache.semantic_token_generation())
            {
                Some((_, regions)) => std::sync::Arc::clone(regions),
                None => std::sync::Arc::new(InjectionResolver::resolve_all(
                    &self.language,
                    self.bridge.node_tracker(),
                    &uri,
                    snapshot_tree,
                    &snapshot.text,
                    injection_query.as_ref(),
                    snapshot.incarnation,
                )),
            };

            if all_regions.is_empty() {
                return Ok(None);
            }
            let region_ranges = all_regions
                .iter()
                .map(|resolved| resolved.region.byte_range.clone())
                .collect::<Vec<_>>();
            let region_merge_order = region_merge_order(&region_ranges, nested_regions_first);

            // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
            let upstream_request_id = crate::lsp::current_upstream_id();

            // Subscribe to cancel notifications so we can abort early on $/cancelRequest.
            // _cancel_guard ensures automatic unsubscribe when this scope exits.
            let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

            let pool = self.bridge.pool_arc();

            // Outer JoinSet: one task per injection region, all in parallel
            let mut outer_join_set: JoinSet<(usize, Option<Vec<T>>)> = JoinSet::new();

            // Shared client progress across all regions: one aggregator + one
            // teardown guard for the whole request. The winner rule shows the first
            // region to begin as one coherent Begin → … → End on the editor's token
            // (ls-bridge-client-progress, #455). `None` (no advertised token) keeps
            // the prior behavior — used by the fast helper methods that don't
            // advertise `workDoneProgress`.
            let shared_cp = client_progress_token.map(|client_token| {
                (
                    Arc::new(Mutex::new(ClientProgressAggregator::new(client_token))),
                    Arc::clone(pool.client_progress_registry()),
                )
            });
            let mut cp_minted: Vec<NumberOrString> = Vec::new();

            // Drop servers already known (a live, `Ready` connection) NOT to
            // support this method before the per-region fan-out spawns their
            // tasks (capability-prefilter-fanout). One pool query for the whole
            // request; the resulting set is a cheap per-region lookup.
            let incapable_servers = self
                .incapable_virt_servers(
                    &language_name,
                    all_regions.iter().map(|r| r.injection_language.as_str()),
                    method_name,
                )
                .await;

            for (region_index, resolved) in all_regions.iter().enumerate() {
                // A combined injection concatenates disjoint host spans into one
                // virtual document. Full semantic-token responses only carry
                // delta coordinates, so the single-offset projection cannot map
                // tokens after a removed gap back to the host safely.
                if method_name == "textDocument/semanticTokens/full" && !resolved.contiguous {
                    continue;
                }
                // Get ALL bridge server configs for this injection language
                let mut configs = self.bridge_configs_for_injection_language(
                    &language_name,
                    &resolved.injection_language,
                );
                if !incapable_servers.is_empty() {
                    configs.retain(|c| !incapable_servers.contains(&c.server_name));
                }
                if configs.is_empty() {
                    continue;
                }
                let agg = self.resolve_aggregation_config(
                    &language_name,
                    &resolved.injection_language,
                    method_name,
                );
                if !request_selects_servers(&agg.priorities, &configs, agg.max_fan_out) {
                    continue;
                }
                let region_ctx = DocumentRequestContext {
                    uri: uri.clone(),
                    resolved: resolved.clone(),
                    region_end: None,
                    configs,
                    upstream_request_id: upstream_request_id.clone(),
                    priorities: agg.priorities,
                    strategy: agg.strategy,
                    max_fan_out: agg.max_fan_out,
                    client_progress_token: None,
                };
                // Mint this region's tracked-source token into the shared
                // aggregator (no-op when there's no client token).
                let region_cp_tokens = shared_cp.as_ref().and_then(|(aggregator, registry)| {
                    mint_region_progress_source(&region_ctx, registry, aggregator)
                });
                if let Some(map) = &region_cp_tokens {
                    cp_minted.extend(map.values().cloned());
                }

                let pool = Arc::clone(&pool);
                let send = send.clone();
                let merge_order = region_merge_order[region_index];

                outer_join_set.spawn(async move {
                    let is_nonempty =
                        |opt: &Option<Vec<T>>| matches!(opt, Some(v) if !v.is_empty());
                    let result = match region_cp_tokens {
                        Some(tokens) => {
                            dispatch_preferred_with_tokens(
                                &region_ctx,
                                pool.clone(),
                                send,
                                is_nonempty,
                                None,
                                tokens,
                            )
                            .await
                        }
                        None => {
                            dispatch_preferred(&region_ctx, pool.clone(), send, is_nonempty, None)
                                .await
                        }
                    };
                    let items = match result {
                        FanInResult::Done(items) => items,
                        FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                    };
                    (merge_order, items)
                });
            }

            // One teardown guard for the whole request, held across the region
            // collection so the synthetic terminal End fires once, after every
            // region settles (or on cancel).
            let _cp_guard = shared_cp.map(|(aggregator, registry)| {
                ClientProgressDeregisterGuard::new(
                    registry,
                    cp_minted,
                    aggregator,
                    pool.upstream_tx(),
                )
            });

            // Collect results, aborting early if $/cancelRequest arrives.
            // Completion order, NOT source order — each entry carries its
            // region index and the flatten below sorts by it.
            let completion_order_items =
                crate::lsp::aggregation::region::collect_region_results_with_cancel(
                    outer_join_set,
                    cancel_rx,
                    |acc, region_items: (usize, Option<Vec<T>>)| {
                        acc.push(region_items);
                    },
                )
                .await;

            Ok(nonempty_whole_document_items(
                flatten_ordered_region_items_with(completion_order_items?, merge_regions),
            ))
        };

        let host = async {
            let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, method_name) else {
                return Ok(None);
            };
            if expected_snapshot.is_some_and(|expected| ctx.incarnation != expected.incarnation) {
                return Ok(None);
            }
            if !request_selects_servers(&ctx.priorities, &ctx.configs, ctx.max_fan_out) {
                return Ok(None);
            }
            let (cancel_rx, _cancel_guard) =
                self.subscribe_cancel(ctx.upstream_request_id.as_ref());
            let incarnation = ctx.incarnation;
            #[cfg(feature = "e2e")]
            wait_for_host_admission_release().await;
            let pool = self.bridge.pool_arc();
            let fan_in = dispatch_host_preferred(
                &ctx,
                pool.clone(),
                move |t| {
                    let params = raw_params.clone();
                    let parse_host = parse_host.clone();
                    let on_host_winner = on_host_winner.clone();
                    let attempted = host_bridge_attempted.clone();
                    async move {
                        let raw = match attempted {
                            Some(attempted) => {
                                t.pool
                                    .send_host_raw_request_for_incarnation_with_attempt_marker(
                                        &t.server_name,
                                        &t.server_config,
                                        &HostDocument {
                                            uri: &t.uri,
                                            language_id: &t.language_id,
                                            text: &t.text,
                                        },
                                        method_name,
                                        params,
                                        t.upstream_id,
                                        incarnation,
                                        attempted,
                                    )
                                    .await?
                            }
                            None => {
                                t.pool
                                    .send_host_raw_request_for_incarnation(
                                        &t.server_name,
                                        &t.server_config,
                                        &HostDocument {
                                            uri: &t.uri,
                                            language_id: &t.language_id,
                                            text: &t.text,
                                        },
                                        method_name,
                                        params,
                                        t.upstream_id,
                                        incarnation,
                                    )
                                    .await?
                            }
                        };
                        let Some(raw) = raw else {
                            return Ok(None);
                        };
                        let Some(items) = parse_host(raw.value) else {
                            return Ok(None);
                        };
                        Ok(on_host_winner(HostWholeDocumentResponse {
                            items,
                            server_name: t.server_name,
                            host_uri: t.uri,
                            host_text: t.text,
                            incarnation: Some(raw.incarnation),
                            connection_generation: raw.connection_generation,
                            handle: raw.handle,
                        }))
                    }
                },
                |opt| matches!(opt, Some(items) if !items.is_empty()),
                cancel_rx,
            )
            .await;
            self.host_layer_result(fan_in, method_name, |won| won).await
        };

        let result = if require_all_layers {
            self.walk_layers_concatenated(
                lsp_uri,
                method_name,
                method_name,
                virt,
                host,
                native,
                merge_layers,
            )
            .await?
        } else {
            self.walk_layers_by_strategy(
                lsp_uri,
                method_name,
                method_name,
                virt,
                host,
                native,
                |items: &Vec<T>| !items.is_empty(),
                merge_layers,
            )
            .await?
        };

        Ok(if preserve_empty {
            result
        } else {
            result.and_then(nonempty_whole_document_items)
        })
    }
}

fn request_selects_servers(
    priorities: &[String],
    configs: &[ResolvedServerConfig],
    max_fan_out: Option<usize>,
) -> bool {
    !truncate_entries(expand_priorities(priorities, configs), max_fan_out).is_empty()
}

fn region_merge_order(ranges: &[std::ops::Range<usize>], nested_regions_first: bool) -> Vec<usize> {
    region_merge_order_with_observer(ranges, nested_regions_first, || {})
}

fn region_merge_order_with_observer(
    ranges: &[std::ops::Range<usize>],
    nested_regions_first: bool,
    mut inspect: impl FnMut(),
) -> Vec<usize> {
    let mut indices = (0..ranges.len()).collect::<Vec<_>>();
    if nested_regions_first {
        // Count strict containing ranges as a two-dimensional dominance query:
        // process starts ascending and ends descending, then query how many
        // previously inserted ends are >= this end. Equal ranges are handled as
        // one group and inserted only after their shared depth is read.
        let mut ends = ranges.iter().map(|range| range.end).collect::<Vec<_>>();
        ends.sort_unstable();
        ends.dedup();
        indices.sort_unstable_by_key(|&index| {
            (ranges[index].start, std::cmp::Reverse(ranges[index].end))
        });
        let mut tree = vec![0usize; ends.len() + 1];
        let mut inserted = 0usize;
        let mut depths = vec![0usize; ranges.len()];
        let mut group_start = 0usize;
        while group_start < indices.len() {
            let range = &ranges[indices[group_start]];
            let mut group_end = group_start + 1;
            while group_end < indices.len() && ranges[indices[group_end]] == *range {
                group_end += 1;
            }
            let end_index = ends
                .binary_search(&range.end)
                .expect("range end came from the compressed set");
            let mut prefix_before = 0usize;
            let mut cursor = end_index;
            while cursor > 0 {
                inspect();
                prefix_before += tree[cursor];
                cursor &= cursor - 1;
            }
            let depth = inserted - prefix_before;
            for &index in &indices[group_start..group_end] {
                depths[index] = depth;
            }
            for _ in group_start..group_end {
                let mut cursor = end_index + 1;
                while cursor < tree.len() {
                    inspect();
                    tree[cursor] += 1;
                    cursor += cursor & cursor.wrapping_neg();
                }
                inserted += 1;
            }
            group_start = group_end;
        }
        indices.sort_unstable_by_key(|&index| (std::cmp::Reverse(depths[index]), index));
    }
    let mut order = vec![0; ranges.len()];
    for (rank, index) in indices.into_iter().enumerate() {
        order[index] = rank;
    }
    order
}

#[cfg(feature = "e2e")]
async fn wait_for_host_admission_release() {
    let Ok(dir) = std::env::var("KAKEHASHI_E2E_WHOLE_DOCUMENT_HOST_BARRIER_DIR") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err()
        || std::fs::write(dir.join("captured"), b"captured").is_err()
    {
        return;
    }
    let release = dir.join("release");
    for _ in 0..300 {
        if release.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
fn concat_whole_document_items<T>(mut acc: Vec<T>, next: Vec<T>) -> Vec<T> {
    acc.extend(next);
    acc
}

fn nonempty_whole_document_items<T>(items: Vec<T>) -> Option<Vec<T>> {
    if items.is_empty() { None } else { Some(items) }
}

/// Flatten per-region result vectors back into ONE list in region source
/// order (the region index recorded at fan-out time), regardless of task
/// completion order.
pub(super) fn flatten_ordered_region_items<T>(
    region_items: Vec<(usize, Option<Vec<T>>)>,
) -> Vec<T> {
    flatten_ordered_region_items_with(region_items, |mut acc, mut next| {
        acc.append(&mut next);
        acc
    })
}

fn flatten_ordered_region_items_with<T, M>(
    mut region_items: Vec<(usize, Option<Vec<T>>)>,
    merge: M,
) -> Vec<T>
where
    M: Fn(Vec<T>, Vec<T>) -> Vec<T>,
{
    region_items.sort_unstable_by_key(|(region_index, _)| *region_index);
    let total_len = region_items
        .iter()
        .filter_map(|(_, items)| items.as_ref())
        .map(Vec::len)
        .sum::<usize>();
    let mut ordered_items = region_items
        .into_iter()
        .filter_map(|(_, items)| items)
        .filter(|items| !items.is_empty());
    let Some(mut flattened) = ordered_items.next() else {
        return Vec::new();
    };
    flattened.reserve(total_len - flattened.len());
    for items in ordered_items {
        flattened = merge(flattened, items);
    }
    flattened
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceSettings;
    use crate::config::settings::BridgeServerConfig;
    use crate::config::settings::{
        AggregationStrategy, LanguageSettings, LayerAggregationConfig, LayerSource, LayersConfig,
    };
    use std::collections::HashMap;
    use std::future::ready;
    use tower_lsp_server::LspService;
    use url::Url;

    #[test]
    fn concatenates_whole_document_layer_items() {
        assert_eq!(
            concat_whole_document_items(vec![1, 2], vec![3, 4]),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn empty_whole_document_layer_items_are_absent() {
        assert_eq!(nonempty_whole_document_items::<i32>(vec![]), None);
        assert_eq!(nonempty_whole_document_items(vec![1]), Some(vec![1]));
    }

    #[test]
    fn disabled_fan_out_selects_no_bridge_server() {
        let configs = vec![ResolvedServerConfig {
            server_name: "tokens".into(),
            config: Arc::new(BridgeServerConfig::default()),
        }];

        assert!(!request_selects_servers(&[], &configs, None));
        assert!(!request_selects_servers(
            &[crate::config::settings::PRIORITIES_WILDCARD.into()],
            &configs,
            Some(0),
        ));
        assert!(request_selects_servers(
            &[crate::config::settings::PRIORITIES_WILDCARD.into()],
            &configs,
            None,
        ));
    }

    #[tokio::test]
    async fn whole_document_walk_honors_concatenated_layer_strategy() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        let mut aggregation = HashMap::new();
        aggregation.insert(
            "textDocument/documentLink".to_string(),
            LayerAggregationConfig {
                priorities: Some(vec![LayerSource::Virt, LayerSource::Host]),
                strategy: Some(AggregationStrategy::Concatenated),
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "rust".to_string(),
            LanguageSettings {
                layers: Some(LayersConfig {
                    aggregation: Some(aggregation),
                }),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            languages,
            auto_install: false,
            ..Default::default()
        });

        let uri = Url::parse("file:///test/whole_document.rs").expect("valid test URI");
        server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        let lsp_uri = crate::lsp::lsp_impl::url_to_uri(&uri).expect("URI should convert");

        let result = server
            .walk_layers_by_strategy(
                &lsp_uri,
                "textDocument/documentLink",
                "textDocument/documentLink",
                ready(Ok(Some(vec!["virt"]))),
                ready(Ok(Some(vec!["host"]))),
                ready(Ok(None)),
                |items: &Vec<&str>| !items.is_empty(),
                concat_whole_document_items,
            )
            .await
            .expect("layer walk should succeed");

        assert_eq!(result, Some(vec!["virt", "host"]));
    }
}

#[cfg(test)]
mod ordered_region_tests {
    use super::*;
    use tower_lsp_server::ls_types::SemanticToken;

    #[test]
    fn flattens_region_results_by_source_order() {
        let flattened = flatten_ordered_region_items(vec![
            (2, Some(vec!["late"])),
            (0, Some(vec!["early"])),
            (1, None),
            (3, Some(vec!["last"])),
        ]);

        assert_eq!(flattened, vec!["early", "late", "last"]);
    }

    #[test]
    fn rebases_independently_encoded_semantic_token_regions() {
        let token = |delta_line| SemanticToken {
            delta_line,
            delta_start: 2,
            length: 4,
            token_type: 1,
            token_modifiers_bitset: 0,
        };

        let flattened = flatten_ordered_region_items_with(
            vec![(1, Some(vec![token(10)])), (0, Some(vec![token(3)]))],
            crate::lsp::bridge::merge_semantic_token_layers,
        );

        assert_eq!(flattened, vec![token(3), token(7)]);
    }

    #[test]
    fn nested_semantic_region_overlays_its_outer_region() {
        let orders = region_merge_order(&[0..10, 2..5], true);
        let token = |delta_start, length, token_type| SemanticToken {
            delta_line: 0,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        };

        let flattened = flatten_ordered_region_items_with(
            vec![
                (orders[0], Some(vec![token(0, 10, 1)])),
                (orders[1], Some(vec![token(2, 3, 2)])),
            ],
            crate::lsp::bridge::merge_semantic_token_layers,
        );

        assert_eq!(
            flattened,
            vec![token(0, 2, 1), token(2, 3, 2), token(3, 5, 1)]
        );
    }

    #[test]
    fn nested_region_order_scales_with_ordered_sweep() {
        let disjoint = (0..10_000)
            .map(|index| index * 2..index * 2 + 1)
            .collect::<Vec<_>>();
        let mut inspections = 0usize;
        let order = region_merge_order_with_observer(&disjoint, true, || inspections += 1);

        assert_eq!(order, (0..disjoint.len()).collect::<Vec<_>>());
        let nested = (0..10_000)
            .map(|index| index..20_000 - index)
            .collect::<Vec<_>>();
        let nested_order = region_merge_order_with_observer(&nested, true, || inspections += 1);
        assert_eq!(nested_order[9_999], 0);
        assert!(
            inspections < (disjoint.len() + nested.len()) * 40,
            "region depth calculation must stay O(n log n), got {inspections} tree operations"
        );
    }

    #[test]
    fn ordered_region_depth_matches_quadratic_containment_contract() {
        let oracle = |ranges: &[std::ops::Range<usize>]| {
            let mut indices = (0..ranges.len()).collect::<Vec<_>>();
            let depth = |index: usize| {
                ranges
                    .iter()
                    .enumerate()
                    .filter(|(other_index, outer)| {
                        *other_index != index
                            && outer.start <= ranges[index].start
                            && ranges[index].end <= outer.end
                            && *outer != &ranges[index]
                    })
                    .count()
            };
            indices.sort_by_key(|&index| (std::cmp::Reverse(depth(index)), index));
            let mut order = vec![0; ranges.len()];
            for (rank, index) in indices.into_iter().enumerate() {
                order[index] = rank;
            }
            order
        };
        let cases = [
            vec![],
            vec![0..1, 1..2, 2..3],
            vec![0..10, 2..8, 3..4],
            vec![0..5, 2..8, 4..6],
            vec![0..10, 0..8, 0..8, 2..8, 2..10],
            vec![0..10, 0..10, 2..5, 2..5],
        ];
        for ranges in cases {
            assert_eq!(region_merge_order(&ranges, true), oracle(&ranges));
            assert_eq!(
                region_merge_order(&ranges, false),
                (0..ranges.len()).collect::<Vec<_>>()
            );
        }
    }
}
