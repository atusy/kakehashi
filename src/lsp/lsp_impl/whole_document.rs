//! Shared fan-out for whole-document bridged requests.
//!
//! documentLink, foldingRange, and codeLens all follow the same shape: no
//! position parameter, so the request fans out to *every* injection region,
//! uses the preferred strategy within each region, and concatenates the
//! per-region results. This module hosts that shape once; the per-method
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
    FanInResult, FanOutTask, dispatch_preferred, dispatch_preferred_with_tokens,
    mint_region_progress_source,
};
use crate::lsp::bridge::{ClientProgressAggregator, ClientProgressDeregisterGuard};

use super::bridge_context::{DocumentRequestContext, parse_host_verbatim};
use super::{Kakehashi, uri_to_url};

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
    pub(super) async fn whole_document_fan_out<T, F, Fut>(
        &self,
        lsp_uri: &Uri,
        method_name: &'static str,
        raw_params: serde_json::Value,
        client_progress_token: Option<NumberOrString>,
        send: F,
    ) -> Result<Option<Vec<T>>>
    where
        T: Send + 'static + serde::de::DeserializeOwned,
        F: Fn(FanOutTask) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = io::Result<Option<Vec<T>>>> + Send + 'static,
    {
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
                )),
            };

            if all_regions.is_empty() {
                return Ok(None);
            }

            // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
            let upstream_request_id = crate::lsp::current_upstream_id();

            // Subscribe to cancel notifications so we can abort early on $/cancelRequest.
            // _cancel_guard ensures automatic unsubscribe when this scope exits.
            let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

            let pool = self.bridge.pool_arc();

            // Outer JoinSet: one task per injection region, all in parallel
            let mut outer_join_set: JoinSet<Option<Vec<T>>> = JoinSet::new();

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

            for resolved in all_regions.iter() {
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
                let region_ctx = DocumentRequestContext {
                    uri: uri.clone(),
                    resolved: resolved.clone(),
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
                    match result {
                        FanInResult::Done(items) => items,
                        FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                    }
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
            let all_items = crate::lsp::aggregation::region::collect_region_results_with_cancel(
                outer_join_set,
                cancel_rx,
                |acc, opt: Option<Vec<T>>| {
                    if let Some(items) = opt {
                        acc.extend(items);
                    }
                },
            )
            .await;

            Ok(nonempty_whole_document_items(all_items?))
        };

        let host = async {
            match self.resolve_host_bridge_context(lsp_uri, method_name) {
                Some(ctx) => Ok(self
                    .host_layer_value_with_ctx(&ctx, method_name, raw_params)
                    .await?
                    .and_then(parse_host_verbatim::<Vec<T>>)),
                None => Ok(None),
            }
        };

        let result = self
            .walk_layers_by_strategy(
                lsp_uri,
                method_name,
                method_name,
                virt,
                host,
                std::future::ready(Ok(None)),
                |items: &Vec<T>| !items.is_empty(),
                concat_whole_document_items,
            )
            .await?;

        Ok(result.and_then(nonempty_whole_document_items))
    }
}

fn concat_whole_document_items<T>(mut acc: Vec<T>, next: Vec<T>) -> Vec<T> {
    acc.extend(next);
    acc
}

fn nonempty_whole_document_items<T>(items: Vec<T>) -> Option<Vec<T>> {
    if items.is_empty() { None } else { Some(items) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceSettings;
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
