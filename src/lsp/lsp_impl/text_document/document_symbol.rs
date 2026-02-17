//! Document symbol method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse, MessageType, SymbolInformation,
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

        // Collect document symbols from all injection regions
        // We track both formats separately since mixing them isn't meaningful
        let mut all_document_symbols: Vec<DocumentSymbol> = Vec::new();
        let mut all_symbol_information: Vec<SymbolInformation> = Vec::new();

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
                Ok(Some(DocumentSymbolResponse::Nested(symbols))) => {
                    all_document_symbols.extend(symbols);
                }
                Ok(Some(DocumentSymbolResponse::Flat(symbols))) => {
                    all_symbol_information.extend(symbols);
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

        // Return results, preferring DocumentSymbol format if available
        // If we have any DocumentSymbol results, use Nested format (hierarchical)
        // Otherwise fall back to Flat format (SymbolInformation) if available
        if !all_document_symbols.is_empty() {
            Ok(Some(DocumentSymbolResponse::Nested(all_document_symbols)))
        } else if !all_symbol_information.is_empty() {
            Ok(Some(DocumentSymbolResponse::Flat(all_symbol_information)))
        } else {
            Ok(None)
        }
    }
}
