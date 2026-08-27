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
            return;
        };

        log::debug!(
            target: "kakehashi::synthetic_diag",
            "didSave received for {}",
            uri
        );

        // Serialize the host save with document edits and snapshot the latest
        // text. Host didSave is textless, so every recipient must first receive
        // any pending full-text didChange on the same downstream queue.
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
        // Each path only touches an already-open document and excludes servers
        // that require save text, which kakehashi does not advertise upstream
        // (#357).
        let pool = self.bridge.pool_arc();
        if let Some((host_text, _, _)) = &saved_document {
            pool.sync_and_notify_host_did_save(&uri, host_text).await;
        }
        drop(edit_guard);

        // A didChange reparses and refreshes virtual documents off-ingress.
        // Settle that pipeline explicitly before the textless virtual didSave;
        // otherwise an immediate save can overtake its projected didChange and
        // run the downstream save hook against stale fragment text. If the
        // bounded settle fails, omit didSave rather than violate that contract.
        if let Some((_, saved_incarnation, saved_content_version)) = saved_document {
            let saved_snapshot_is_current =
                saved_parse_is_current(self, &uri, saved_incarnation, saved_content_version).await;

            if saved_snapshot_is_current {
                let edit_lock = self.documents.edit_lock(&uri);
                let edit_guard = edit_lock.lock().await;
                let still_saved_version = self.documents.get(&uri).is_some_and(|document| {
                    document.incarnation() == saved_incarnation
                        && document.content_version() == saved_content_version
                });
                if still_saved_version
                    && let Some((_, injections)) =
                        self.injection_coordinator().bridge_injections(&uri)
                {
                    pool.sync_and_forward_did_save_to_virtual_docs(
                        &uri,
                        saved_incarnation,
                        &injections,
                    )
                    .await;
                }
                drop(edit_guard);
            }
        }

        // Ensure a fresh tree before the synthetic task snapshots it: a save
        // batched right after an edit (autosave / format-on-save) races the
        // off-ingress reparse, and `prepare_diagnostic_snapshot` returns `None`
        // without a tree — making the synthetic diagnostic a no-op for the virt
        // layer.
        let _ = tokio::time::timeout(
            VIRTUAL_SAVE_SETTLE_BUDGET,
            self.ensure_document_parsed(&uri),
        )
        .await;

        // Spawn background task for synthetic diagnostic collection
        self.diagnostic_scheduler()
            .spawn_synthetic_diagnostic_task(uri);

        self.notifier().log_info("file saved!").await;
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
            started.elapsed() <= VIRTUAL_SAVE_SETTLE_BUDGET * 2,
            "both save-time waits must remain hard-bounded"
        );
    }
}
