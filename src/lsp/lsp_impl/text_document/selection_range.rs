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
        let Some(language_name) = self.document_language(&uri) else {
            return Ok(None);
        };

        // Ensure language is loaded (handles race condition with didOpen)
        let load_result = self.language.ensure_language_loaded(&language_name);
        if !load_result.success {
            return Ok(None);
        }

        // Take what the on-demand parse needs and drop the Ref at the block's
        // end: a DashMap Ref held across an .await keeps the shard read-locked
        // while this task is parked, stalling didOpen/didChange writers.
        let unparsed_text = {
            let Some(doc) = self.documents.get(&uri) else {
                return Ok(None);
            };
            doc.tree().is_none().then(|| doc.text().to_string())
        };

        // Parse on-demand if the document has no tree yet
        if let Some(text) = unparsed_text {
            let text_clone = text.clone();

            let sync_parse_result = self
                .parse_coordinator()
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    let parse_result = parser.parse(&text_clone, None);
                    (parser, parse_result)
                })
                .await;

            let Some(tree) = sync_parse_result else {
                return Ok(None);
            };

            // Persist under the per-URI edit lock and only if the document is
            // still alive with the text we parsed: a didClose racing the parse
            // must not be undone by re-inserting the document, and a didChange
            // must not be clobbered with the older text/tree. Block-scoped so
            // the edit lock is released before the pool wait below.
            {
                let edit_lock = self.documents.edit_lock(&uri);
                let _guard = edit_lock.lock().await;
                let still_current = {
                    let Some(doc) = self.documents.get(&uri) else {
                        // edit_lock() get-or-inserts; a didClose that raced the
                        // parse already removed the entry, so drop the one we
                        // just recreated rather than leaking it (same miss-path
                        // cleanup as semantic_tokens).
                        drop(_guard);
                        self.documents.remove_edit_lock(&uri);
                        return Ok(None);
                    };
                    doc.text() == text
                };
                if still_current {
                    self.documents
                        .update_document(uri.clone(), text, Some(tree));
                }
            }
        }

        // Snapshot owned pieces of the document (cheap: Arc/Tree refcount bumps)
        // and drop the Ref, then run the synchronous injection-aware walk as one
        // work-unit on the compute pool. The parser-pool sync mutex is acquired
        // only on the pool thread (Stage-1 obligation), and no DashMap shard
        // read-lock is held for the walk's duration.
        let (text, tree, language_id) = {
            let Some(doc) = self.documents.get(&uri) else {
                return Ok(None);
            };
            (
                doc.text_arc(),
                doc.tree().cloned(),
                doc.language_id().map(|s| s.to_string()),
            )
        };

        let language = std::sync::Arc::clone(&self.language);
        let parser_pool = std::sync::Arc::clone(&self.parser_pool);
        let result = self
            .compute_pool
            .run(None, move || {
                use crate::error::LockResultExt;
                let mut pool = parser_pool
                    .lock()
                    .recover_poison("Kakehashi::selection_range_impl");
                handle_selection_range(
                    &text,
                    tree.as_ref(),
                    language_id.as_deref(),
                    &positions,
                    &language,
                    &mut pool,
                )
            })
            .await;

        // None = the work-unit panicked (logged by the pool); serve the
        // no-result fallback rather than an error.
        Ok(result)
    }
}
