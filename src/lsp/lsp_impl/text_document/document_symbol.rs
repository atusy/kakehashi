//! Document symbol method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, Location, MessageType,
    SymbolInformation, Uri,
};

use crate::language::InjectionResolver;
use crate::lsp::bridge::UpstreamId;
use crate::lsp::get_current_request_id;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn document_symbol_impl(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in documentSymbol: {}", lsp_uri.as_str());
            return Ok(None);
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("documentSymbol called for {}", uri),
            )
            .await;

        // Get document snapshot (minimizes lock duration)
        let (snapshot, missing_message) = match self.documents.get(&uri) {
            None => (None, Some("No document found")),
            Some(doc) => match doc.snapshot() {
                None => (None, Some("Document not fully initialized")),
                Some(snapshot) => (Some(snapshot), None),
            },
            // doc automatically dropped here, lock released
        };
        if let Some(message) = missing_message {
            self.client.log_message(MessageType::INFO, message).await;
            return Ok(None);
        }
        let snapshot = snapshot.expect("snapshot set when missing_message is None");

        // Get the language for this document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            log::debug!(target: "kakehashi::document_symbol", "No language detected");
            return Ok(None);
        };

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.get_injection_query(&language_name) else {
            return Ok(None);
        };

        // Collect all injection regions
        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.region_id_tracker(),
            &uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Ok(None);
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = match get_current_request_id() {
            Some(tower_lsp_server::jsonrpc::Id::Number(n)) => Some(UpstreamId::Number(n)),
            Some(tower_lsp_server::jsonrpc::Id::String(s)) => Some(UpstreamId::String(s)),
            None | Some(tower_lsp_server::jsonrpc::Id::Null) => None,
        };

        // Collect document symbols from all injection regions.
        // The bridge normalizes both DocumentSymbol[] and SymbolInformation[]
        // responses to Vec<DocumentSymbol>, so we use a single accumulator.
        let mut all_symbols: Vec<DocumentSymbol> = Vec::new();

        for resolved in all_regions {
            // Get bridge server config for this language
            // The bridge filter is checked inside get_bridge_config_for_language
            let Some(resolved_config) =
                self.get_bridge_config_for_language(&language_name, &resolved.injection_language)
            else {
                continue; // No bridge configured for this language
            };

            // Send document symbol request via language server pool
            let response = self
                .bridge
                .pool()
                .send_document_symbol_request(
                    &resolved_config.server_name,
                    &resolved_config.config,
                    &uri,
                    &resolved.injection_language,
                    &resolved.region.region_id,
                    resolved.region.line_range.start,
                    &resolved.virtual_content,
                    upstream_request_id.clone(),
                )
                .await;

            match response {
                Ok(Some(symbols)) => {
                    all_symbols.extend(symbols);
                }
                Ok(None) => {}
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Bridge document symbol request failed: {}", e),
                        )
                        .await;
                }
            }
        }

        Ok(format_document_symbol_response(
            all_symbols,
            &lsp_uri,
            self.supports_hierarchical_document_symbol(),
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
