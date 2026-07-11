//! didClose notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidCloseTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    #[cfg(test)]
    pub(in crate::lsp::lsp_impl) async fn clear_document_state_on_close(&self, uri: &url::Url) {
        let edit_lock = self.documents.edit_lock(uri);
        let edit_guard = edit_lock.lock().await;
        self.clear_document_state_on_close_locked(uri);
        drop(edit_guard);
        self.documents.remove_edit_lock_if_unshared(uri, &edit_lock);
    }

    fn clear_document_state_on_close_locked(&self, uri: &url::Url) {
        // Captures lineage and walk memos are installed under this same lock,
        // so a store that won first is cleared while one arriving later sees
        // the document gone and refuses to install obsolete state.
        self.captures_cache.retain(|key, _| key.0 != *uri);
        self.captures_walk_cache.retain(|key, _| key.0 != *uri);
        self.cancel_captures_walks_for_document(uri);
        self.documents.remove_preserving_edit_lock(uri);
        self.cache.remove_document(uri);
        // The match cache uses insert-then-verify. Clearing after document
        // removal ensures a straggler either inserted before this clear or
        // observes the closed document and self-removes.
        self.captures_match_cache.clear_document(uri);
    }

    async fn close_document_lifecycle(
        &self,
        uri: &url::Url,
        after_remove: impl std::future::Future<Output = ()>,
    ) {
        let edit_lock = self.documents.edit_lock(uri);
        let edit_guard = edit_lock.lock().await;
        self.clear_document_state_on_close_locked(uri);
        after_remove.await;

        self.bridge
            .cleanup(uri, || self.documents.latest_snapshot(uri).is_some());
        self.synthetic_diagnostics.remove_document(uri);
        self.debounced_diagnostics.cancel(uri);
        self.bridge.cancel_eager_open(uri);
        self.bridge.cancel_host_eager_open(uri);

        let closed_docs = self.bridge.close_host_document(uri).await;
        if !closed_docs.is_empty() {
            log::debug!(
                target: "kakehashi::bridge",
                "Closed {} virtual documents for host {}",
                closed_docs.len(),
                uri
            );
        }

        super::super::coordinator::DiagnosticPublisher::new(self)
            .clear_host(uri)
            .await;
        self.bridge.pool_arc().close_host_bridge_document(uri).await;

        drop(edit_guard);
        self.documents.remove_edit_lock_if_unshared(uri, &edit_lock);
    }

    pub(crate) async fn did_close_impl(&self, params: DidCloseTextDocumentParams) {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in didClose: {}", lsp_uri.as_str());
            return;
        };

        // Remove the document from the store when it's closed.
        // This ensures that reopening the file will properly reinitialize everything.
        //
        // Serialize the removal behind any in-flight edit by acquiring this
        // document's edit lock first (the same lock `did_change` holds across its
        // reparse). Without it, `remove` could drop the lock entry while an edit
        // still holds the old `Arc`, so a later edit would create a *fresh* lock
        // and stop serializing with the in-flight one. The retained lock stays
        // held through every URI-scoped teardown below; didOpen takes the same
        // lock before inserting the next lifetime.
        self.close_document_lifecycle(&uri, std::future::ready(()))
            .await;

        self.notifier().log_info("file closed!").await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::WorkspaceSettings;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{DidOpenTextDocumentParams, TextDocumentItem};
    use url::Url;

    #[tokio::test]
    async fn fast_reopen_waits_for_complete_did_close_cleanup() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = Arc::new(service);
        let server = service.inner();
        server.settings_manager.apply_settings(WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        });
        let uri = Url::parse("file:///test/close-cleanup-fast-reopen").unwrap();
        server
            .documents
            .insert(uri.clone(), "old".to_string(), None, None);

        let cleanup_paused = Arc::new(Notify::new());
        let release_cleanup = Arc::new(Notify::new());
        let close = {
            let service = Arc::clone(&service);
            let uri = uri.clone();
            let cleanup_paused = Arc::clone(&cleanup_paused);
            let release_cleanup = Arc::clone(&release_cleanup);
            tokio::spawn(async move {
                service
                    .inner()
                    .close_document_lifecycle(&uri, async move {
                        cleanup_paused.notify_one();
                        release_cleanup.notified().await;
                    })
                    .await;
            })
        };
        cleanup_paused.notified().await;
        assert!(server.documents.get(&uri).is_none());

        let reopen = {
            let service = Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .did_open_impl(DidOpenTextDocumentParams {
                        text_document: TextDocumentItem {
                            uri: crate::lsp::lsp_impl::url_to_uri(&uri).unwrap(),
                            language_id: "plaintext".to_string(),
                            version: 1,
                            text: "new".to_string(),
                        },
                    })
                    .await;
            })
        };
        tokio::task::yield_now().await;
        assert!(
            server.documents.get(&uri).is_none(),
            "reopen must not expose new state during old-lifetime cleanup"
        );

        release_cleanup.notify_one();
        timeout(Duration::from_secs(1), async {
            loop {
                if server
                    .documents
                    .get(&uri)
                    .is_some_and(|doc| doc.text() == "new")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reopen should proceed after cleanup releases the lock");

        close.abort();
        reopen.abort();
    }
}
