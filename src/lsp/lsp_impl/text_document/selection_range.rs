//! Selection range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{SelectionRange, SelectionRangeParams};

use crate::analysis::handle_selection_range;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn selection_range_impl(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let lsp_uri = params.text_document.uri;
        let positions = params.positions;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in selectionRange: {}", lsp_uri.as_str());
            return Ok(None);
        };

        // Get language for document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            return Ok(None);
        };

        // Ensure language is loaded (handles race condition with didOpen)
        let load_result = self.language.ensure_language_loaded(&language_name);
        if !load_result.success {
            return Ok(None);
        }

        // Get document
        let Some(doc) = self.documents.get(&uri) else {
            return Ok(None);
        };

        // Check if document has a tree, if not parse it on-demand
        if doc.tree().is_none() {
            let text = doc.text().to_string();
            drop(doc); // Release lock before acquiring parser pool

            let text_clone = text.clone();

            let sync_parse_result = self
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    let parse_result = parser.parse(&text_clone, None);
                    (parser, parse_result)
                })
                .await;

            if let Some(tree) = sync_parse_result {
                self.documents
                    .update_document(uri.clone(), text, Some(tree));
            } else {
                return Ok(None);
            }

            // Re-acquire document after update
            let Some(doc) = self.documents.get(&uri) else {
                return Ok(None);
            };

            // Use full injection parsing handler with coordinator and parser pool
            let mut pool = self.parser_pool.lock().await;
            let result = handle_selection_range(&doc, &positions, &self.language, &mut pool);

            return Ok(Some(result));
        }

        // Use full injection parsing handler with coordinator and parser pool
        let mut pool = self.parser_pool.lock().await;
        let result = handle_selection_range(&doc, &positions, &self.language, &mut pool);

        Ok(Some(result))
    }
}
