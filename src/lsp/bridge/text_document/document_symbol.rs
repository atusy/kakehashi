//! Document symbol request handling for bridge connections.
//!
//! This module provides document symbol request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! Like document link, document symbol requests operate on the entire document -
//! they don't take a position parameter.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.
//!
//! # Normalization
//!
//! Both DocumentSymbol[] and SymbolInformation[] responses from downstream servers
//! are normalized to `Vec<DocumentSymbol>`. This pushes format awareness to the bridge
//! layer, allowing the lsp_impl handler to work with a single type and decide the
//! final response format based on client capabilities.
//!
//! # Known Limitations
//!
//! When downstream servers return `SymbolInformation[]`, each item's
//! `container_name` is discarded during normalization to `DocumentSymbol`
//! (which has no equivalent field — it uses `children` for hierarchy instead).
//! If the response is later flattened back to `SymbolInformation[]` for clients
//! without hierarchical support, the reconstructed items will have
//! `container_name: None`. This is a rare path because the bridge declares
//! `hierarchicalDocumentSymbolSupport: true` to downstream servers.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{DocumentSymbol, SymbolInformation};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    DocumentParams, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_whole_document_request,
};

impl LanguageServerPool {
    /// Send a document symbol request and wait for the response.
    ///
    /// Returns `Vec<DocumentSymbol>` regardless of whether the downstream server
    /// returned DocumentSymbol[] or SymbolInformation[]. SymbolInformation items
    /// are converted to DocumentSymbol with `selection_range = range`.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing document-symbol-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_document_symbol_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<DocumentSymbol>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/documentSymbol") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            build_document_symbol_request,
            |response, ctx| {
                transform_document_symbol_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.offset,
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC document symbol request for a downstream language server.
///
/// Like DocumentLinkParams, DocumentSymbolParams only has a textDocument field -
/// no position. The request asks for all symbols in the entire document.
fn build_document_symbol_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentParams> {
    build_whole_document_request(virtual_uri, request_id, "textDocument/documentSymbol")
}

/// Transform a document symbol response from virtual to host document coordinates.
///
/// Both DocumentSymbol[] and SymbolInformation[] responses are normalized to
/// `Vec<DocumentSymbol>`. This allows the lsp_impl handler to work with a single
/// type and decide the final response format based on client capabilities.
///
/// DocumentSymbol responses can be in two formats per LSP spec:
/// - DocumentSymbol[] (hierarchical with range, selectionRange, and optional children)
/// - SymbolInformation[] (flat with location.uri + location.range)
///
/// For DocumentSymbol format:
/// - range and selectionRange are translated using the region offset
/// - children are recursively processed
///
/// For SymbolInformation format:
/// - Converted to DocumentSymbol with selection_range = range
/// - Real file URIs are filtered out (per LSP spec, documentSymbol is for the requested document)
/// - Cross-region virtual URIs are filtered out
/// - Same-region virtual URIs are transformed to host coordinates
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `request_virtual_uri` - The virtual URI from the request
/// * `offset` - The region offset for coordinate translation
fn transform_document_symbol_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    offset: &RegionOffset,
) -> Option<Vec<DocumentSymbol>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/documentSymbol: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // DocumentSymbol[] or SymbolInformation[] is an array
    let items = result.as_array()?;

    if items.is_empty() {
        return Some(vec![]);
    }

    // Detect format by checking only the first element: the LSP spec defines
    // the response as either DocumentSymbol[] OR SymbolInformation[], never mixed.
    // SymbolInformation has "location"; DocumentSymbol has "range" + "selectionRange".
    if items.first().and_then(|i| i.get("location")).is_some() {
        // SymbolInformation[] format → convert to Vec<DocumentSymbol>
        transform_symbol_information_response(result, request_virtual_uri, offset)
    } else {
        // DocumentSymbol[] format
        transform_document_symbol_nested_response(result, offset)
    }
}

/// Transform a SymbolInformation[] response into `Vec<DocumentSymbol>`.
///
/// Each SymbolInformation is converted to a DocumentSymbol with:
/// - `selection_range` = `range` (SymbolInformation has only one range)
/// - `children` = None
/// - `detail` = None
/// - `tags` and `deprecated` propagated from the original
///
/// Filtering rules:
/// - **Real file URI** (not virtual): Filtered out per LSP spec (documentSymbol
///   returns symbols for the requested document only)
/// - **Same virtual URI**: Converted and coordinates transformed to host
/// - **Cross-region virtual URI**: Filtered out
#[allow(deprecated)]
fn transform_symbol_information_response(
    result: serde_json::Value,
    request_virtual_uri: &str,
    offset: &RegionOffset,
) -> Option<Vec<DocumentSymbol>> {
    let symbols: Vec<SymbolInformation> = serde_json::from_value(result).ok()?;

    let converted: Vec<DocumentSymbol> = symbols
        .into_iter()
        .filter(|symbol| {
            let uri_str = symbol.location.uri.as_str();
            let is_virtual = VirtualDocumentUri::is_virtual_uri(uri_str);

            // Real file URI → filter out (not part of the requested document)
            if !is_virtual {
                return false;
            }

            // Same virtual URI → keep for conversion
            if uri_str == request_virtual_uri {
                return true;
            }

            // Cross-region virtual URI → filter out
            false
        })
        .map(|symbol| {
            let mut range = symbol.location.range;
            translate_virtual_range_to_host(&mut range, offset);

            DocumentSymbol {
                name: symbol.name,
                detail: None,
                kind: symbol.kind,
                tags: symbol.tags,
                deprecated: symbol.deprecated,
                range,
                selection_range: range,
                children: None,
            }
        })
        .collect();

    Some(converted)
}

/// Transform a DocumentSymbol[] response to `Vec<DocumentSymbol>`.
///
/// Deserializes into typed structs first, then recursively transforms range
/// and selectionRange in all items and their children.
fn transform_document_symbol_nested_response(
    result: serde_json::Value,
    offset: &RegionOffset,
) -> Option<Vec<DocumentSymbol>> {
    let mut symbols: Vec<DocumentSymbol> = serde_json::from_value(result).ok()?;
    for symbol in &mut symbols {
        transform_document_symbol_ranges(symbol, offset);
    }
    Some(symbols)
}

/// Recursively transform a single DocumentSymbol's ranges from virtual to host coordinates.
///
/// Uses saturating_add to prevent overflow for large line numbers.
fn transform_document_symbol_ranges(symbol: &mut DocumentSymbol, offset: &RegionOffset) {
    translate_virtual_range_to_host(&mut symbol.range, offset);
    translate_virtual_range_to_host(&mut symbol.selection_range, offset);

    if let Some(children) = &mut symbol.children {
        for child in children {
            transform_document_symbol_ranges(child, offset);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    // ==========================================================================
    // Document symbol request tests
    // ==========================================================================

    #[test]
    fn document_symbol_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_document_symbol_request(&virtual_uri, RequestId::new(42));

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn document_symbol_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_document_symbol_request(&virtual_uri, RequestId::new(123));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/documentSymbol");
        assert!(
            json["params"].get("position").is_none(),
            "DocumentSymbol request should not have position parameter"
        );
    }

    // ==========================================================================
    // Document symbol response transformation tests
    // ==========================================================================

    #[test]
    fn document_symbol_response_transforms_range_and_selection_range_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "myFunction",
                    "kind": 12,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 5, "character": 3 }
                    },
                    "selectionRange": {
                        "start": { "line": 0, "character": 9 },
                        "end": { "line": 0, "character": 19 }
                    }
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            "unused",
            &RegionOffset::new(3, 0),
        )
        .unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].range.start.line, 3);
        assert_eq!(symbols[0].range.end.line, 8);
        assert_eq!(symbols[0].selection_range.start.line, 3);
        assert_eq!(symbols[0].selection_range.end.line, 3);
        assert_eq!(symbols[0].name, "myFunction");
    }

    #[test]
    fn document_symbol_response_recursively_transforms_nested_children() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "myModule",
                    "kind": 2,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 10, "character": 3 }
                    },
                    "selectionRange": {
                        "start": { "line": 0, "character": 7 },
                        "end": { "line": 0, "character": 15 }
                    },
                    "children": [
                        {
                            "name": "innerFunc",
                            "kind": 12,
                            "range": {
                                "start": { "line": 2, "character": 2 },
                                "end": { "line": 5, "character": 5 }
                            },
                            "selectionRange": {
                                "start": { "line": 2, "character": 11 },
                                "end": { "line": 2, "character": 20 }
                            },
                            "children": [
                                {
                                    "name": "deeplyNested",
                                    "kind": 13,
                                    "range": {
                                        "start": { "line": 3, "character": 4 },
                                        "end": { "line": 4, "character": 7 }
                                    },
                                    "selectionRange": {
                                        "start": { "line": 3, "character": 10 },
                                        "end": { "line": 3, "character": 22 }
                                    }
                                }
                            ]
                        }
                    ]
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            "unused",
            &RegionOffset::new(5, 0),
        )
        .unwrap();

        assert_eq!(symbols[0].range.start.line, 5);
        assert_eq!(symbols[0].range.end.line, 15);

        let children = symbols[0].children.as_ref().unwrap();
        assert_eq!(children[0].range.start.line, 7);
        assert_eq!(children[0].range.end.line, 10);
        assert_eq!(children[0].selection_range.start.line, 7);

        let grandchildren = children[0].children.as_ref().unwrap();
        assert_eq!(grandchildren[0].range.start.line, 8);
        assert_eq!(grandchildren[0].range.end.line, 9);
        assert_eq!(grandchildren[0].selection_range.start.line, 8);
        assert_eq!(grandchildren[0].name, "deeplyNested");
    }

    #[test]
    fn document_symbol_response_filters_out_real_file_uri_symbol_information() {
        // Per LSP spec, documentSymbol returns symbols for the requested document only.
        // Real file URIs from downstream servers refer to external files and should
        // be filtered out when converting SymbolInformation to DocumentSymbol.
        let real_file_uri = "file:///test.lua";
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "myVariable",
                    "kind": 13,
                    "location": {
                        "uri": real_file_uri,
                        "range": {
                            "start": { "line": 2, "character": 6 },
                            "end": { "line": 2, "character": 16 }
                        }
                    }
                },
                {
                    "name": "myFunction",
                    "kind": 12,
                    "location": {
                        "uri": real_file_uri,
                        "range": {
                            "start": { "line": 5, "character": 0 },
                            "end": { "line": 10, "character": 3 }
                        }
                    }
                }
            ]
        });
        let symbols = transform_document_symbol_response_to_host(
            response,
            "file:///project/kakehashi-virtual-uri-region-0.lua",
            &RegionOffset::new(7, 0),
        )
        .unwrap();

        // Real file URIs are filtered out per LSP spec
        assert!(
            symbols.is_empty(),
            "Real file URI symbols should be filtered out"
        );
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn document_symbol_response_returns_none_for_invalid_response(
        #[case] response: serde_json::Value,
    ) {
        let transformed = transform_document_symbol_response_to_host(
            response,
            "unused",
            &RegionOffset::new(5, 0),
        );
        assert!(transformed.is_none());
    }

    #[test]
    fn document_symbol_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let symbols = transform_document_symbol_response_to_host(
            response,
            "unused",
            &RegionOffset::new(5, 0),
        )
        .unwrap();
        assert!(symbols.is_empty());
    }

    #[test]
    fn document_symbol_response_converts_same_virtual_uri_symbol_information_to_document_symbol() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "myVariable",
                    "kind": 13,
                    "location": {
                        "uri": virtual_uri,
                        "range": {
                            "start": { "line": 2, "character": 6 },
                            "end": { "line": 2, "character": 16 }
                        }
                    }
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            virtual_uri,
            &RegionOffset::new(7, 0),
        )
        .unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "myVariable");
        // Coordinates transformed: line 2 + 7 = 9
        assert_eq!(symbols[0].range.start.line, 9);
        assert_eq!(symbols[0].range.end.line, 9);
        // selection_range should equal range for converted SymbolInformation
        assert_eq!(symbols[0].selection_range.start.line, 9);
        assert_eq!(symbols[0].selection_range.end.line, 9);
        assert_eq!(symbols[0].selection_range.start.character, 6);
        assert_eq!(symbols[0].selection_range.end.character, 16);
        assert!(symbols[0].children.is_none());
        assert!(symbols[0].detail.is_none());
    }

    #[test]
    fn document_symbol_response_filters_out_cross_region_symbol_information() {
        let request_virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let cross_region_uri = "file:///project/kakehashi-virtual-uri-region-1.lua";
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "crossRegionSymbol",
                    "kind": 13,
                    "location": {
                        "uri": cross_region_uri,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        }
                    }
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            request_virtual_uri,
            &RegionOffset::new(5, 0),
        )
        .unwrap();

        assert!(
            symbols.is_empty(),
            "Cross-region SymbolInformation should be filtered out"
        );
    }

    #[test]
    fn document_symbol_response_mixed_symbol_information_keeps_only_same_region() {
        let request_virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let cross_region_uri = "file:///project/kakehashi-virtual-uri-region-1.lua";
        let real_file_uri = "file:///real/module.lua";

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "localSymbol",
                    "kind": 13,
                    "location": {
                        "uri": request_virtual_uri,
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 10 } }
                    }
                },
                {
                    "name": "crossRegionSymbol",
                    "kind": 12,
                    "location": {
                        "uri": cross_region_uri,
                        "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 5, "character": 15 } }
                    }
                },
                {
                    "name": "externalSymbol",
                    "kind": 6,
                    "location": {
                        "uri": real_file_uri,
                        "range": { "start": { "line": 20, "character": 0 }, "end": { "line": 25, "character": 3 } }
                    }
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            request_virtual_uri,
            &RegionOffset::new(5, 0),
        )
        .unwrap();

        // Only the same-region virtual URI symbol should remain.
        // Cross-region and real file URIs are both filtered out.
        assert_eq!(
            symbols.len(),
            1,
            "Should have 1 item (cross-region and real-file filtered out)"
        );
        assert_eq!(symbols[0].name, "localSymbol");
        assert_eq!(symbols[0].range.start.line, 5); // 0 + 5
    }

    #[test]
    fn document_symbol_response_transformation_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "overflowSymbol",
                    "kind": 12,
                    "range": {
                        "start": { "line": u32::MAX, "character": 0 },
                        "end": { "line": u32::MAX, "character": 5 }
                    },
                    "selectionRange": {
                        "start": { "line": u32::MAX, "character": 0 },
                        "end": { "line": u32::MAX, "character": 5 }
                    }
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            "unused",
            &RegionOffset::new(10, 0),
        )
        .unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(
            symbols[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }

    #[test]
    fn symbol_information_to_document_symbol_propagates_tags_and_deprecated() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "deprecatedSymbol",
                    "kind": 13,
                    "tags": [1],
                    "deprecated": true,
                    "location": {
                        "uri": virtual_uri,
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        }
                    }
                }
            ]
        });

        let symbols = transform_document_symbol_response_to_host(
            response,
            virtual_uri,
            &RegionOffset::new(0, 0),
        )
        .unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].tags.as_ref().unwrap().len(), 1);
        #[allow(deprecated)]
        {
            assert_eq!(symbols[0].deprecated, Some(true));
        }
    }
}
