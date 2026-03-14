//! didChange notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidChangeTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};
use crate::language::region_id_tracker::EditInfo;
use crate::lsp::text_sync::apply_content_changes_with_edits;

impl Kakehashi {
    pub(crate) async fn did_change_impl(&self, params: DidChangeTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didChange: {}", lsp_uri.as_str());
            return;
        };

        self.notifier()
            .log_trace(format!("[DID_CHANGE] START uri={}", uri))
            .await;

        // Retrieve the stored document info
        let (language_id, old_text) = {
            let doc = self.documents.get(&uri);
            match doc {
                Some(d) => (d.language_id().map(|s| s.to_string()), d.text().to_string()),
                None => {
                    self.notifier()
                        .log_warning("Document not found for change event")
                        .await;
                    return;
                }
            }
        };

        // Apply content changes and build tree-sitter edits
        let (text, edits) = apply_content_changes_with_edits(&old_text, params.content_changes);

        // ADR-0019: Apply START-priority invalidation to region ID tracker.
        // Use InputEdits directly for precise invalidation when available,
        // fall back to diff-based approach for full document sync.
        //
        // This must be called AFTER content changes are applied (so we have new text)
        // but BEFORE parse_document (so position sync happens before new tree is built).
        let invalidated_ulids = if edits.is_empty() {
            // Full document sync: no InputEdits available, reconstruct from diff
            self.bridge.apply_text_diff(&uri, &old_text, &text)
        } else {
            // Incremental sync: use InputEdits directly (precise, no over-invalidation)
            let edit_infos: Vec<EditInfo> = edits.iter().map(EditInfo::from).collect();
            self.bridge.apply_input_edits(&uri, &edit_infos)
        };

        // Invalidate injection caches for regions overlapping with edits (AC4/AC5)
        // Must be called BEFORE parse_document which updates the injection_map
        self.cache.invalidate_for_edits(&uri, &edits);

        // Parse the updated document with edit information
        self.parse_document(uri.clone(), text, language_id.as_deref(), edits)
            .await;

        // NOTE: We intentionally do NOT invalidate the semantic token cache here.
        // The cached tokens (with their result_id) are needed for delta calculations.
        // When semanticTokens/full/delta arrives with previousResultId, we look up
        // the cached tokens to compute the delta. If we invalidated here, the delta
        // request would always fall back to full tokenization.
        //
        // The cache is validated at lookup time via result_id matching, so stale
        // tokens won't be returned for mismatched result_ids.

        // ADR-0019: Close invalidated virtual documents.
        // Send didClose notifications to downstream LSs for orphaned docs.
        self.close_invalidated_virtual_docs(&uri, &invalidated_ulids)
            .await;

        // Forward didChange to opened virtual documents + process injected languages.
        // Injection data is resolved once and reused for:
        // 1. didChange forwarding to already-opened virtual documents
        // 2. Auto-install missing parsers
        // 3. Eager server spawn + didOpen for virtual documents
        // Must be called AFTER parse_document so we have access to the updated AST.
        self.process_injections(&uri, true).await;

        // ADR-0020 Phase 3: Schedule debounced diagnostic push on didChange.
        // After 500ms of no changes, diagnostics will be collected and published.
        // This provides near-real-time feedback while avoiding excessive requests during typing.
        self.schedule_debounced_diagnostic(uri, lsp_uri);

        // NOTE: We intentionally do NOT call semantic_tokens_refresh() here.
        // LSP clients already request new tokens after didChange (via semanticTokens/full/delta).
        // Calling refresh would be redundant and can cause deadlocks with synchronous clients
        // like vim-lsp on Vim, which cannot respond to server requests while processing.

        self.notifier().log_info("file changed!").await;
    }
}
