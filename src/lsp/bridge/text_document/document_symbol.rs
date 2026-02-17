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

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{DocumentSymbol, DocumentSymbolResponse, SymbolInformation, Uri};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_whole_document_request};

impl LanguageServerPool {
    /// Send a document symbol request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
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
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<DocumentSymbolResponse>> {
        self.execute_bridge_request(
            server_name,
            server_config,
            host_uri,
            injection_language,
            region_id,
            region_start_line,
            virtual_content,
            upstream_request_id,
            build_document_symbol_request,
            |response, ctx| {
                transform_document_symbol_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.region_start_line,
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
) -> serde_json::Value {
    build_whole_document_request(virtual_uri, request_id, "textDocument/documentSymbol")
}

/// Transform a document symbol response from virtual to host document coordinates.
///
/// DocumentSymbol responses can be in two formats per LSP spec:
/// - DocumentSymbol[] (hierarchical with range, selectionRange, and optional children)
/// - SymbolInformation[] (flat with location.uri + location.range)
///
/// For DocumentSymbol format:
/// - range: The full scope of the symbol (e.g., entire function body)
/// - selectionRange: The identifier/name of the symbol (e.g., function name)
/// - children: Optional nested symbols (recursively processed)
///
/// For SymbolInformation format:
/// - location.uri: The symbol's document URI (needs transformation if virtual)
/// - location.range: The symbol's location range (needs transformation)
///
/// This function handles three cases for SymbolInformation URIs:
/// 1. **Real file URI** (not a virtual URI): Preserved as-is with original coordinates
/// 2. **Same virtual URI as request**: Transformed using request's context
/// 3. **Different virtual URI** (cross-region): Filtered out from results
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `request_virtual_uri` - The virtual URI from the request
/// * `host_uri` - The host URI to replace virtual URIs with
/// * `region_start_line` - The starting line of the injection region in the host document
fn transform_document_symbol_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) -> Option<DocumentSymbolResponse> {
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
        // Return empty Nested variant for consistency
        return Some(DocumentSymbolResponse::Nested(vec![]));
    }

    // Detect format by checking only the first element: the LSP spec defines
    // the response as either DocumentSymbol[] OR SymbolInformation[], never mixed.
    // SymbolInformation has "location"; DocumentSymbol has "range" + "selectionRange".
    if items.first().and_then(|i| i.get("location")).is_some() {
        // SymbolInformation[] format
        transform_symbol_information_response(
            result,
            request_virtual_uri,
            host_uri,
            region_start_line,
        )
    } else {
        // DocumentSymbol[] format
        transform_document_symbol_nested_response(result, region_start_line)
    }
}

/// Transform a SymbolInformation[] response to typed format.
///
/// Deserializes into typed structs first, then filters cross-region virtual URIs,
/// transforms same-region URIs to host, and preserves real file URIs unchanged.
fn transform_symbol_information_response(
    result: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) -> Option<DocumentSymbolResponse> {
    let mut symbols: Vec<SymbolInformation> = serde_json::from_value(result).ok()?;

    symbols.retain_mut(|symbol| {
        let is_virtual = VirtualDocumentUri::is_virtual_uri(symbol.location.uri.as_str());

        // Case 1: Real file URI → preserve as-is
        if !is_virtual {
            return true;
        }

        // Case 2: Same virtual URI → transform to host coordinates
        if symbol.location.uri.as_str() == request_virtual_uri {
            symbol.location.uri = host_uri.clone();
            symbol.location.range.start.line = symbol
                .location
                .range
                .start
                .line
                .saturating_add(region_start_line);
            symbol.location.range.end.line = symbol
                .location
                .range
                .end
                .line
                .saturating_add(region_start_line);
            return true;
        }

        // Case 3: Different virtual URI (cross-region) → filter out
        false
    });

    Some(DocumentSymbolResponse::Flat(symbols))
}

/// Transform a DocumentSymbol[] response to typed format.
///
/// Deserializes into typed structs first, then recursively transforms range
/// and selectionRange in all items and their children.
fn transform_document_symbol_nested_response(
    result: serde_json::Value,
    region_start_line: u32,
) -> Option<DocumentSymbolResponse> {
    let mut symbols: Vec<DocumentSymbol> = serde_json::from_value(result).ok()?;
    for symbol in &mut symbols {
        transform_document_symbol_ranges(symbol, region_start_line);
    }
    Some(DocumentSymbolResponse::Nested(symbols))
}

/// Recursively transform a single DocumentSymbol's ranges from virtual to host coordinates.
///
/// Uses saturating_add to prevent overflow for large line numbers.
fn transform_document_symbol_ranges(symbol: &mut DocumentSymbol, region_start_line: u32) {
    symbol.range.start.line = symbol.range.start.line.saturating_add(region_start_line);
    symbol.range.end.line = symbol.range.end.line.saturating_add(region_start_line);
    symbol.selection_range.start.line = symbol
        .selection_range
        .start
        .line
        .saturating_add(region_start_line);
    symbol.selection_range.end.line = symbol
        .selection_range
        .end
        .line
        .saturating_add(region_start_line);

    if let Some(children) = &mut symbol.children {
        for child in children {
            transform_document_symbol_ranges(child, region_start_line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A dummy URI for tests where the host URI is not used (nested/early-return paths).
    fn dummy_uri() -> Uri {
        "file:///unused".parse().unwrap()
    }

    // ==========================================================================
    // Document symbol request tests
    // ==========================================================================

    #[test]
    fn document_symbol_request_uses_virtual_uri() {
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_document_symbol_request(&virtual_uri, RequestId::new(42));

        let uri_str = request["params"]["textDocument"]["uri"].as_str().unwrap();
        assert!(
            VirtualDocumentUri::is_virtual_uri(uri_str),
            "Request should use a virtual URI: {}",
            uri_str
        );
        assert!(
            uri_str.ends_with(".lua"),
            "Virtual URI should have .lua extension: {}",
            uri_str
        );
    }

    #[test]
    fn document_symbol_request_has_correct_method_and_no_position() {
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_document_symbol_request(&virtual_uri, RequestId::new(123));

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 123);
        assert_eq!(request["method"], "textDocument/documentSymbol");
        assert!(
            request["params"].get("position").is_none(),
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

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 3);

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Nested(symbols) => {
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0].range.start.line, 3);
                assert_eq!(symbols[0].range.end.line, 8);
                assert_eq!(symbols[0].selection_range.start.line, 3);
                assert_eq!(symbols[0].selection_range.end.line, 3);
                assert_eq!(symbols[0].name, "myFunction");
            }
            _ => panic!("Expected Nested variant"),
        }
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

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 5);

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Nested(symbols) => {
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
            _ => panic!("Expected Nested variant"),
        }
    }

    #[test]
    fn document_symbol_response_transforms_symbol_information_location_range() {
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
        let host_uri: Uri = "file:///doc.md".parse().unwrap();
        let transformed = transform_document_symbol_response_to_host(
            response,
            "file:///project/kakehashi-virtual-uri-region-0.lua",
            &host_uri,
            7,
        );

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Flat(symbols) => {
                assert_eq!(symbols.len(), 2);
                // Real file URI preserved, range NOT transformed
                assert_eq!(symbols[0].location.uri.as_str(), real_file_uri);
                assert_eq!(symbols[0].location.range.start.line, 2);
                assert_eq!(symbols[0].location.range.end.line, 2);
                assert_eq!(symbols[0].name, "myVariable");

                assert_eq!(symbols[1].location.uri.as_str(), real_file_uri);
                assert_eq!(symbols[1].location.range.start.line, 5);
                assert_eq!(symbols[1].location.range.end.line, 10);
                assert_eq!(symbols[1].name, "myFunction");
            }
            _ => panic!("Expected Flat variant"),
        }
    }

    #[test]
    fn document_symbol_response_with_null_result_returns_none() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn document_symbol_response_with_empty_array_returns_empty_nested() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 5);
        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Nested(symbols) => {
                assert!(symbols.is_empty());
            }
            _ => panic!("Expected Nested variant"),
        }
    }

    #[test]
    fn document_symbol_response_transforms_symbol_information_location_uri_to_host_uri() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let host_uri: Uri = "file:///project/doc.md".parse().unwrap();
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

        let transformed =
            transform_document_symbol_response_to_host(response, virtual_uri, &host_uri, 7);

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Flat(symbols) => {
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0].location.uri.as_str(), host_uri.as_str());
                assert_eq!(symbols[0].location.range.start.line, 9);
                assert_eq!(symbols[0].location.range.end.line, 9);
            }
            _ => panic!("Expected Flat variant"),
        }
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

        let host_uri: Uri = "file:///doc.md".parse().unwrap();
        let transformed =
            transform_document_symbol_response_to_host(response, request_virtual_uri, &host_uri, 5);

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Flat(symbols) => {
                assert!(
                    symbols.is_empty(),
                    "Cross-region SymbolInformation should be filtered out"
                );
            }
            _ => panic!("Expected Flat variant"),
        }
    }

    #[test]
    fn document_symbol_response_preserves_real_file_uri_in_symbol_information() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let real_file_uri = "file:///real/path/module.lua";
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "name": "externalSymbol",
                    "kind": 12,
                    "location": {
                        "uri": real_file_uri,
                        "range": {
                            "start": { "line": 10, "character": 0 },
                            "end": { "line": 15, "character": 3 }
                        }
                    }
                }
            ]
        });

        let host_uri: Uri = "file:///doc.md".parse().unwrap();
        let transformed =
            transform_document_symbol_response_to_host(response, virtual_uri, &host_uri, 5);

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Flat(symbols) => {
                assert_eq!(symbols.len(), 1);
                assert_eq!(symbols[0].location.uri.as_str(), real_file_uri);
                assert_eq!(symbols[0].location.range.start.line, 10);
                assert_eq!(symbols[0].location.range.end.line, 15);
            }
            _ => panic!("Expected Flat variant"),
        }
    }

    #[test]
    fn document_symbol_response_mixed_symbol_information_filters_only_cross_region() {
        let request_virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let cross_region_uri = "file:///project/kakehashi-virtual-uri-region-1.lua";
        let real_file_uri = "file:///real/module.lua";
        let host_uri: Uri = "file:///doc.md".parse().unwrap();

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

        let transformed =
            transform_document_symbol_response_to_host(response, request_virtual_uri, &host_uri, 5);

        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Flat(symbols) => {
                assert_eq!(
                    symbols.len(),
                    2,
                    "Should have 2 items (cross-region filtered out)"
                );
                assert_eq!(symbols[0].name, "localSymbol");
                assert_eq!(symbols[0].location.uri.as_str(), host_uri.as_str());
                assert_eq!(symbols[0].location.range.start.line, 5);

                assert_eq!(symbols[1].name, "externalSymbol");
                assert_eq!(symbols[1].location.uri.as_str(), real_file_uri);
                assert_eq!(symbols[1].location.range.start.line, 20);
            }
            _ => panic!("Expected Flat variant"),
        }
    }

    #[test]
    fn document_symbol_response_with_no_result_key_returns_none() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn document_symbol_response_with_malformed_result_returns_none() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_an_array"
        });

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 5);
        assert!(transformed.is_none());
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

        let transformed =
            transform_document_symbol_response_to_host(response, "unused", &dummy_uri(), 10);
        let result = transformed.unwrap();
        match result {
            DocumentSymbolResponse::Nested(symbols) => {
                assert_eq!(symbols.len(), 1);
                assert_eq!(
                    symbols[0].range.start.line,
                    u32::MAX,
                    "Overflow should saturate at u32::MAX, not panic"
                );
            }
            _ => panic!("Expected Nested variant"),
        }
    }
}
