//! didSave notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidSaveTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};
use crate::lsp::lsp_impl::snapshot_read::SnapshotWait;

const VIRTUAL_SAVE_SETTLE_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

async fn saved_parse_is_current(
    server: &Kakehashi,
    uri: &url::Url,
    incarnation: u64,
    content_version: u64,
) -> bool {
    let settle = tokio::time::timeout(
        VIRTUAL_SAVE_SETTLE_BUDGET,
        server.wait_for_current_snapshot(uri, VIRTUAL_SAVE_SETTLE_BUDGET),
    )
    .await;
    matches!(
        settle,
        Ok(SnapshotWait::Current(snapshot))
            if snapshot.incarnation == incarnation
                && snapshot.parsed_version == content_version
    )
}

impl Kakehashi {
    /// Handle textDocument/didSave notification.
    ///
    /// pull-first-diagnostic-forwarding Phase 2: Triggers synthetic diagnostic push.
    /// Collects diagnostics from downstream servers and publishes via publishDiagnostics.
    pub(crate) async fn did_save_impl(&self, params: DidSaveTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didSave: {}", lsp_uri.as_str());
            crate::lsp::reclaim_current_writer_sequence();
            return;
        };

        log::debug!(
            target: "kakehashi::synthetic_diag",
            "didSave received for {}",
            uri
        );

        // Serialize the host save with document edits and snapshot the latest
        // text. Every recipient must first receive any pending full-text
        // didChange on the same downstream queue.
        let edit_lock = self.documents.edit_lock(&uri);
        let edit_guard = edit_lock.lock().await;
        let saved_document = self.documents.get(&uri).map(|document| {
            (
                document.text_arc(),
                document.incarnation(),
                document.content_version(),
            )
        });

        // Forward didSave to both bridge layers, in host-before-virt order.
        // Each path only touches an already-open document. The downstream
        // notification includes this tracked saved text when requested (#357).
        let pool = self.bridge.pool_arc();
        if let Some((host_text, _, _)) = &saved_document {
            pool.sync_and_notify_host_did_save(&uri, host_text).await;
        }
        drop(edit_guard);
        if saved_document.is_none() {
            self.documents
                .remove_edit_lock_if_unshared(&uri, &edit_lock);
            crate::lsp::reclaim_current_writer_sequence();
        }

        // A didChange reparses and refreshes virtual documents off-ingress.
        // Settle that pipeline explicitly before the virtual didSave;
        // otherwise an immediate save can overtake its projected didChange and
        // run the downstream save hook against stale fragment text. If the
        // bounded settle fails, omit didSave rather than violate that contract.
        if let Some((_, saved_incarnation, saved_content_version)) = &saved_document
            && saved_parse_is_current(self, &uri, *saved_incarnation, *saved_content_version).await
        {
            self.forward_virtual_did_save_for_saved_version(
                &pool,
                &uri,
                *saved_incarnation,
                *saved_content_version,
            )
            .await;
        }

        // Register diagnostic collection immediately, but keep its parse wait
        // off-ingress. This preserves the saved-version trigger even when
        // parsing exceeds the virtual forwarding budget, while a later save,
        // edit, close, or shutdown can still supersede the background task.
        if let Some((_, saved_incarnation, saved_content_version)) = saved_document {
            self.diagnostic_scheduler()
                .spawn_synthetic_diagnostic_task_when_current(
                    uri,
                    saved_incarnation,
                    saved_content_version,
                );
        }

        self.notifier().log_info("file saved!").await;
    }

    /// Forward the save to `uri`'s virtual documents, but only while the
    /// document still carries the saved lineage. The edit lock is held across
    /// the check and the downstream enqueue so a concurrent edit cannot slip
    /// between them and turn the projected content stale.
    async fn forward_virtual_did_save_for_saved_version(
        &self,
        pool: &std::sync::Arc<crate::lsp::LanguageServerPool>,
        uri: &url::Url,
        saved_incarnation: u64,
        saved_content_version: u64,
    ) {
        let edit_lock = self.documents.edit_lock(uri);
        let edit_guard = edit_lock.lock().await;
        let current_lineage = self
            .documents
            .get(uri)
            .map(|document| (document.incarnation(), document.content_version()));
        if current_lineage == Some((saved_incarnation, saved_content_version))
            && let Some((_, injections)) = self.injection_coordinator().bridge_injections(uri)
        {
            pool.sync_and_forward_did_save_to_virtual_docs(uri, saved_incarnation, &injections)
                .await;
        }
        drop(edit_guard);
        // `edit_lock` get-or-inserts, so a close during the settle leaves this
        // path holding an entry the closed document will never reclaim.
        if current_lineage.is_none() {
            self.documents.remove_edit_lock_if_unshared(uri, &edit_lock);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::LspService;

    #[tokio::test(start_paused = true)]
    async fn virtual_save_settle_obeys_the_real_time_budget_when_unparsed() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = url::Url::parse("file:///test/unparsed-save.md").unwrap();
        server.documents.insert(
            uri.clone(),
            "# unparsed".to_string(),
            Some("markdown".to_string()),
            None,
        );
        let document = server.documents.get(&uri).unwrap();
        let incarnation = document.incarnation();
        let content_version = document.content_version();
        drop(document);

        let started = tokio::time::Instant::now();
        assert!(
            !saved_parse_is_current(server, &uri, incarnation, content_version).await,
            "an unparsed document cannot vouch for virtual save content"
        );
        assert_eq!(started.elapsed(), VIRTUAL_SAVE_SETTLE_BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn unparsed_did_save_handler_has_a_bounded_total_settle_time() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = url::Url::parse("file:///test/unparsed-handler-save.md").unwrap();
        server.documents.insert(
            uri.clone(),
            "# unparsed".to_string(),
            Some("markdown".to_string()),
            None,
        );
        let params = DidSaveTextDocumentParams {
            text_document: tower_lsp_server::ls_types::TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).unwrap(),
            },
            text: None,
        };

        let started = tokio::time::Instant::now();
        server.did_save_impl(params).await;
        assert!(
            started.elapsed() <= VIRTUAL_SAVE_SETTLE_BUDGET,
            "diagnostic parse waiting must stay off the ingress writer"
        );
    }

    #[tokio::test]
    async fn closed_document_virtual_save_reclaims_its_edit_lock() {
        // A close that lands after the settle vouched for the saved parse
        // leaves this path minting an edit-lock entry for a URI that no
        // longer has a document, and no other path reclaims it: the
        // background diagnostic waiter bails out before its own reclaim, and
        // `remove_preserving_edit_lock` retains the entry on purpose.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = url::Url::parse("file:///test/closed-virtual-save.md").unwrap();
        let pool = server.bridge.pool_arc();

        server
            .forward_virtual_did_save_for_saved_version(&pool, &uri, 1, 1)
            .await;

        assert!(
            !server.documents.has_edit_lock(&uri),
            "a save for a closed document must not retain its edit-lock entry"
        );
    }

    #[tokio::test]
    async fn missing_document_did_save_reclaims_its_edit_lock() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = url::Url::parse("file:///test/missing-save.md").unwrap();
        let params = DidSaveTextDocumentParams {
            text_document: tower_lsp_server::ls_types::TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).unwrap(),
            },
            text: None,
        };

        server.did_save_impl(params).await;

        assert!(!server.documents.has_edit_lock(&uri));
    }
}
