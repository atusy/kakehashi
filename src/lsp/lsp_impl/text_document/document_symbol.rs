//! Document symbol method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges every resolved virtual document and concatenates their symbols,
//! the host layer (host-document-bridge) bridges the host document itself
//! with the real URI and the response verbatim. The first layer producing a
//! non-empty result wins (`preferred`).

use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, Location, NumberOrString,
    SymbolInformation, Uri,
};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, dispatch_preferred, dispatch_preferred_with_tokens,
    mint_region_progress_source,
};
use crate::lsp::bridge::{ClientProgressAggregator, ClientProgressDeregisterGuard};
use crate::lsp::lsp_impl::bridge_context::{DocumentRequestContext, parse_host_verbatim};

use super::super::{Kakehashi, uri_to_url};

const METHOD: &str = "textDocument/documentSymbol";

impl Kakehashi {
    pub(crate) async fn document_symbol_impl(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token.clone();
        let lsp_uri = params.text_document.uri;

        let virt = self.document_symbol_virt_layer(&lsp_uri, work_done_token);
        self.walk_layers(
            &lsp_uri,
            METHOD,
            METHOD,
            raw_params,
            virt,
            parse_host_verbatim::<DocumentSymbolResponse>,
            |resp: &DocumentSymbolResponse| match resp {
                DocumentSymbolResponse::Flat(symbols) => !symbols.is_empty(),
                DocumentSymbolResponse::Nested(symbols) => !symbols.is_empty(),
            },
        )
        .await
    }

    /// Virt layer: bridge every resolved virtual document and concatenate symbols.
    async fn document_symbol_virt_layer(
        &self,
        lsp_uri: &Uri,
        work_done_token: Option<NumberOrString>,
    ) -> Result<Option<DocumentSymbolResponse>> {
        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!("Invalid URI in documentSymbol: {}", lsp_uri.as_str());
            return Ok(None);
        };

        log::debug!("documentSymbol called for {}", uri);

        // Ensure a fresh tree before snapshotting: `didChange` clears the tree and
        // reparses off-ingress, so without this the (tree-driven) virt layer drops
        // injection-region symbols for the whole reparse window after every edit.
        self.ensure_document_parsed(&uri).await;

        // Get document snapshot (minimizes lock duration)
        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("documentSymbol: No document found for {}", uri);
                return Ok(None);
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!("documentSymbol: Document not fully initialized for {}", uri);
                    return Ok(None);
                }
                Some(snapshot) => snapshot,
            },
            // doc automatically dropped here, lock released
        };

        // Get the language for this document
        let Some(language_name) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::document_symbol", "No language detected");
            return Ok(None);
        };

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(None);
        };

        // Collect all injection regions
        let all_regions = match self
            .documents
            .current_resolved_regions(&uri, self.cache.semantic_token_generation())
        {
            Some(regions) => regions,
            None => std::sync::Arc::new(InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                &uri,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
                snapshot.incarnation(),
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
        let mut outer_join_set: JoinSet<(usize, Option<Vec<DocumentSymbol>>)> = JoinSet::new();

        // Shared client progress: a whole-document request fans out over several
        // injection regions (one `dispatch_preferred` each), so they share ONE
        // aggregator + ONE teardown guard. The aggregator's winner rule shows the
        // first region to begin as one coherent `Begin → … → End` on the editor's
        // token (ls-bridge-client-progress). `None` when no `workDoneToken`.
        let shared_cp = work_done_token.map(|client_token| {
            (
                Arc::new(Mutex::new(ClientProgressAggregator::new(client_token))),
                Arc::clone(pool.client_progress_registry()),
            )
        });
        let mut cp_minted: Vec<NumberOrString> = Vec::new();

        // Drop servers already known (a live, `Ready` connection) NOT to support
        // this method before the per-region fan-out spawns their tasks
        // (capability-prefilter-fanout). One pool query for the whole request.
        let incapable_servers = self
            .incapable_virt_servers(
                &language_name,
                all_regions.iter().map(|r| r.injection_language.as_str()),
                METHOD,
            )
            .await;

        for (region_index, resolved) in all_regions.iter().enumerate() {
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
                METHOD,
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

            // Mint this region's tracked-source token into the shared aggregator
            // (the per-region map dispatch hands to its winning downstream).
            let region_cp_tokens = shared_cp.as_ref().and_then(|(aggregator, registry)| {
                mint_region_progress_source(&region_ctx, registry, aggregator)
            });
            if let Some(map) = &region_cp_tokens {
                cp_minted.extend(map.values().cloned());
            }

            let pool = Arc::clone(&pool);

            outer_join_set.spawn(async move {
                let send = |t: FanOutTask| async move {
                    t.pool
                        .send_document_symbol_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                            t.client_progress_token,
                        )
                        .await
                };
                let is_nonempty =
                    |opt: &Option<Vec<DocumentSymbol>>| matches!(opt, Some(v) if !v.is_empty());
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
                        dispatch_preferred(&region_ctx, pool.clone(), send, is_nonempty, None).await
                    }
                };
                let symbols = match result {
                    FanInResult::Done(symbols) => symbols,
                    FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                };
                (region_index, symbols)
            });
        }

        // One teardown guard for the whole request, held across the region
        // collection so the synthetic terminal `End` fires once, after every
        // region settles (or on cancel, when `collect` drops the JoinSet).
        let _cp_guard = shared_cp.map(|(aggregator, registry)| {
            ClientProgressDeregisterGuard::new(registry, cp_minted, aggregator, pool.upstream_tx())
        });

        // Collect results, aborting early if $/cancelRequest arrives.
        let result = crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_rx,
            |acc, region_items: (usize, Option<Vec<DocumentSymbol>>)| acc.push(region_items),
        )
        .await;

        let all_symbols =
            crate::lsp::lsp_impl::whole_document::flatten_ordered_region_items(result?);

        Ok(format_document_symbol_response(
            all_symbols,
            lsp_uri,
            self.settings_manager
                .supports_hierarchical_document_symbol(),
        ))
    }
}

/// Recursively flatten `DocumentSymbol` trees into `SymbolInformation` list.
///
/// Each `DocumentSymbol` becomes a `SymbolInformation` with:
/// - `location` = `{ uri, range }` (uses the symbol's `range`, not `selectionRange`)
/// - `container_name` = parent symbol's name (None for top-level symbols)
/// - `tags` and `deprecated` propagated from the original
///
/// Children are recursively flattened with the parent's name as `container_name`.
#[allow(deprecated)]
fn flatten_document_symbols(symbols: Vec<DocumentSymbol>, uri: &Uri) -> Vec<SymbolInformation> {
    let mut result = Vec::new();
    flatten_recursive(&symbols, uri, None, &mut result);
    result
}

#[allow(deprecated)]
fn flatten_recursive(
    symbols: &[DocumentSymbol],
    uri: &Uri,
    container_name: Option<&str>,
    result: &mut Vec<SymbolInformation>,
) {
    for symbol in symbols {
        result.push(SymbolInformation {
            name: symbol.name.clone(),
            kind: symbol.kind,
            tags: symbol.tags.clone(),
            deprecated: symbol.deprecated,
            location: Location {
                uri: uri.clone(),
                range: symbol.range,
            },
            container_name: container_name.map(|s| s.to_string()),
        });

        if let Some(children) = &symbol.children {
            flatten_recursive(children, uri, Some(&symbol.name), result);
        }
    }
}

/// Choose the response format based on client capability.
///
/// Returns `None` when `symbols` is empty. Otherwise:
/// - `hierarchical = true` → `DocumentSymbolResponse::Nested` (preserves hierarchy)
/// - `hierarchical = false` → `DocumentSymbolResponse::Flat` (backwards compatibility)
///
/// Per LSP 3.18, when `hierarchicalDocumentSymbolSupport` is true the server
/// should return `DocumentSymbol[]`; otherwise `SymbolInformation[]`.
fn format_document_symbol_response(
    symbols: Vec<DocumentSymbol>,
    uri: &Uri,
    hierarchical: bool,
) -> Option<DocumentSymbolResponse> {
    if symbols.is_empty() {
        return None;
    }

    if hierarchical {
        Some(DocumentSymbolResponse::Nested(symbols))
    } else {
        Some(DocumentSymbolResponse::Flat(flatten_document_symbols(
            symbols, uri,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range, SymbolKind};

    fn make_range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Range {
        Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        }
    }

    #[allow(deprecated)]
    fn make_symbol(
        name: &str,
        kind: SymbolKind,
        range: Range,
        selection_range: Range,
        children: Option<Vec<DocumentSymbol>>,
    ) -> DocumentSymbol {
        DocumentSymbol {
            name: name.to_string(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range,
            children,
        }
    }

    #[test]
    fn flatten_empty_symbols_returns_empty() {
        let uri: Uri = "file:///test.md".parse().unwrap();
        let result = flatten_document_symbols(vec![], &uri);
        assert!(result.is_empty());
    }

    #[test]
    fn flatten_single_symbol_without_children() {
        let uri: Uri = "file:///test.md".parse().unwrap();
        let symbol = make_symbol(
            "myFunc",
            SymbolKind::FUNCTION,
            make_range(3, 0, 8, 3),
            make_range(3, 9, 3, 15),
            None,
        );

        let result = flatten_document_symbols(vec![symbol], &uri);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "myFunc");
        assert_eq!(result[0].kind, SymbolKind::FUNCTION);
        assert_eq!(result[0].location.uri.as_str(), uri.as_str());
        // Uses range (not selectionRange) for SymbolInformation.location.range
        assert_eq!(result[0].location.range.start.line, 3);
        assert_eq!(result[0].location.range.end.line, 8);
        assert!(result[0].container_name.is_none());
    }

    #[test]
    fn flatten_symbol_with_children_sets_container_name() {
        let uri: Uri = "file:///test.md".parse().unwrap();

        let child = make_symbol(
            "innerFunc",
            SymbolKind::FUNCTION,
            make_range(5, 2, 7, 5),
            make_range(5, 11, 5, 20),
            None,
        );

        let parent = make_symbol(
            "myModule",
            SymbolKind::MODULE,
            make_range(3, 0, 10, 3),
            make_range(3, 7, 3, 15),
            Some(vec![child]),
        );

        let result = flatten_document_symbols(vec![parent], &uri);

        assert_eq!(result.len(), 2);

        // Parent: no container_name
        assert_eq!(result[0].name, "myModule");
        assert!(result[0].container_name.is_none());

        // Child: container_name = parent's name
        assert_eq!(result[1].name, "innerFunc");
        assert_eq!(result[1].container_name.as_deref(), Some("myModule"));
    }

    #[test]
    fn flatten_deeply_nested_symbols() {
        let uri: Uri = "file:///test.md".parse().unwrap();

        let grandchild = make_symbol(
            "deepVar",
            SymbolKind::VARIABLE,
            make_range(6, 4, 6, 20),
            make_range(6, 10, 6, 17),
            None,
        );

        let child = make_symbol(
            "innerFunc",
            SymbolKind::FUNCTION,
            make_range(5, 2, 7, 5),
            make_range(5, 11, 5, 20),
            Some(vec![grandchild]),
        );

        let parent = make_symbol(
            "myModule",
            SymbolKind::MODULE,
            make_range(3, 0, 10, 3),
            make_range(3, 7, 3, 15),
            Some(vec![child]),
        );

        let result = flatten_document_symbols(vec![parent], &uri);

        assert_eq!(result.len(), 3);

        assert_eq!(result[0].name, "myModule");
        assert!(result[0].container_name.is_none());

        assert_eq!(result[1].name, "innerFunc");
        assert_eq!(result[1].container_name.as_deref(), Some("myModule"));

        assert_eq!(result[2].name, "deepVar");
        assert_eq!(result[2].container_name.as_deref(), Some("innerFunc"));
    }

    #[test]
    fn flatten_multiple_top_level_symbols() {
        let uri: Uri = "file:///test.md".parse().unwrap();

        let sym1 = make_symbol(
            "func1",
            SymbolKind::FUNCTION,
            make_range(3, 0, 5, 3),
            make_range(3, 9, 3, 14),
            None,
        );
        let sym2 = make_symbol(
            "func2",
            SymbolKind::FUNCTION,
            make_range(7, 0, 9, 3),
            make_range(7, 9, 7, 14),
            None,
        );

        let result = flatten_document_symbols(vec![sym1, sym2], &uri);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "func1");
        assert_eq!(result[1].name, "func2");
        assert!(result[0].container_name.is_none());
        assert!(result[1].container_name.is_none());
    }

    #[allow(deprecated)]
    #[test]
    fn flatten_propagates_tags_and_deprecated() {
        let uri: Uri = "file:///test.md".parse().unwrap();

        let symbol = DocumentSymbol {
            name: "oldFunc".to_string(),
            detail: Some("deprecated function".to_string()),
            kind: SymbolKind::FUNCTION,
            tags: Some(vec![tower_lsp_server::ls_types::SymbolTag::DEPRECATED]),
            deprecated: Some(true),
            range: make_range(3, 0, 5, 3),
            selection_range: make_range(3, 9, 3, 16),
            children: None,
        };

        let result = flatten_document_symbols(vec![symbol], &uri);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tags.as_ref().unwrap().len(), 1);
        assert_eq!(result[0].deprecated, Some(true));
    }

    // ==========================================================================
    // format_document_symbol_response tests
    // ==========================================================================

    #[test]
    fn format_empty_symbols_returns_none() {
        let uri: Uri = "file:///test.md".parse().unwrap();
        assert!(format_document_symbol_response(vec![], &uri, true).is_none());
        assert!(format_document_symbol_response(vec![], &uri, false).is_none());
    }

    #[test]
    fn format_hierarchical_returns_nested_variant() {
        let uri: Uri = "file:///test.md".parse().unwrap();
        let symbol = make_symbol(
            "myFunc",
            SymbolKind::FUNCTION,
            make_range(3, 0, 8, 3),
            make_range(3, 9, 3, 15),
            None,
        );

        let response = format_document_symbol_response(vec![symbol], &uri, true).unwrap();

        match response {
            DocumentSymbolResponse::Nested(symbols) => {
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0].name, "myFunc");
            }
            DocumentSymbolResponse::Flat(_) => panic!("Expected Nested variant"),
        }
    }

    #[test]
    fn format_flat_returns_flat_variant() {
        let uri: Uri = "file:///test.md".parse().unwrap();
        let symbol = make_symbol(
            "myFunc",
            SymbolKind::FUNCTION,
            make_range(3, 0, 8, 3),
            make_range(3, 9, 3, 15),
            None,
        );

        let response = format_document_symbol_response(vec![symbol], &uri, false).unwrap();

        match response {
            DocumentSymbolResponse::Flat(infos) => {
                assert_eq!(infos.len(), 1);
                assert_eq!(infos[0].name, "myFunc");
                assert_eq!(infos[0].kind, SymbolKind::FUNCTION);
                assert_eq!(infos[0].location.uri.as_str(), uri.as_str());
                assert_eq!(infos[0].location.range.start.line, 3);
                assert_eq!(infos[0].location.range.end.line, 8);
                assert!(infos[0].container_name.is_none());
            }
            DocumentSymbolResponse::Nested(_) => panic!("Expected Flat variant"),
        }
    }

    #[allow(deprecated)]
    #[test]
    fn format_flat_with_children_produces_container_name() {
        let uri: Uri = "file:///test.md".parse().unwrap();

        let child = make_symbol(
            "innerFunc",
            SymbolKind::FUNCTION,
            make_range(5, 2, 7, 5),
            make_range(5, 11, 5, 20),
            None,
        );
        let parent = make_symbol(
            "myModule",
            SymbolKind::MODULE,
            make_range(3, 0, 10, 3),
            make_range(3, 7, 3, 15),
            Some(vec![child]),
        );

        let response = format_document_symbol_response(vec![parent], &uri, false).unwrap();

        match response {
            DocumentSymbolResponse::Flat(infos) => {
                assert_eq!(infos.len(), 2);
                assert_eq!(infos[0].name, "myModule");
                assert!(infos[0].container_name.is_none());
                assert_eq!(infos[1].name, "innerFunc");
                assert_eq!(infos[1].container_name.as_deref(), Some("myModule"));
            }
            DocumentSymbolResponse::Nested(_) => panic!("Expected Flat variant"),
        }
    }
}
