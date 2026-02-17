//! Document color method for Kakehashi.

use tower_lsp_server::jsonrpc::{Id, Result};
use tower_lsp_server::ls_types::{ColorInformation, DocumentColorParams, MessageType};

use crate::language::InjectionResolver;
use crate::lsp::bridge::UpstreamId;
use crate::lsp::get_current_request_id;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn document_color_impl(
        &self,
        params: DocumentColorParams,
    ) -> Result<Vec<ColorInformation>> {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in documentColor: {}", lsp_uri.as_str());
            return Ok(vec![]);
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!("documentColor called for {}", uri),
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
            return Ok(Vec::new());
        }
        let snapshot = snapshot.expect("snapshot set when missing_message is None");

        // Get the language for this document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            log::debug!(target: "kakehashi::document_color", "No language detected");
            return Ok(Vec::new());
        };

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.get_injection_query(&language_name) else {
            return Ok(Vec::new());
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
            return Ok(Vec::new());
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = match get_current_request_id() {
            Some(Id::Number(n)) => Some(UpstreamId::Number(n)),
            Some(Id::String(s)) => Some(UpstreamId::String(s)),
            None | Some(Id::Null) => None,
        };

        // Collect colors from all injection regions
        let mut all_colors: Vec<ColorInformation> = Vec::new();

        for resolved in all_regions {
            // Get bridge server config for this language
            // The bridge filter is checked inside get_bridge_config_for_language
            let Some(resolved_config) =
                self.get_bridge_config_for_language(&language_name, &resolved.injection_language)
            else {
                continue; // No bridge configured for this language
            };

            // Send document color request via language server pool
            let response = self
                .bridge
                .pool()
                .send_document_color_request(
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
                Ok(colors) => {
                    all_colors.extend(colors);
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::ERROR,
                            format!("Bridge document color request failed: {}", e),
                        )
                        .await;
                }
            }
        }

        Ok(all_colors)
    }
}
