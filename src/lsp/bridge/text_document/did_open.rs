//! Eager didOpen notification handling for bridge connections.
//!
//! This module provides eager opening of virtual documents on downstream
//! language servers when injection regions are detected during `did_open`
//! or `did_change` processing.

use std::sync::Arc;
use std::time::Duration;

use super::super::pool::{ConnectionHandleSender, INIT_TIMEOUT_SECS, LanguageServerPool};
use super::super::protocol::VirtualDocumentUri;

impl LanguageServerPool {
    /// Fire `didOpen` for every injection region's virtual URI so the downstream
    /// server starts analyzing immediately instead of waiting for the first
    /// user request. Fire-and-forget: per-document failures are logged at
    /// debug level and never propagated; one open failing leaves the others
    /// alone.
    pub(crate) async fn eager_open_virtual_documents(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        host_uri: &url::Url,
        host_uri_lsp: &tower_lsp_server::ls_types::Uri,
        expected_incarnation: Option<u64>,
        injections: Vec<crate::lsp::bridge::coordinator::BridgeInjection>,
    ) {
        // Wait for the server to be ready (handshake complete)
        let handle = match self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
                Some(host_uri),
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager open: server {} not ready, skipping didOpen for {} injections: {}",
                    server_name,
                    injections.len(),
                    e
                );
                return;
            }
        };

        let connection_key = handle.key().clone();
        let mut sender = ConnectionHandleSender(&handle);

        for injection in &injections {
            // Hold the host cache guard through didOpen. didClose/reopen replaces
            // this entry, so it either linearizes after this open (and closes the
            // tracked virtual document) or wins first and makes this stale batch
            // stop without opening old content.
            let lifecycle = expected_incarnation.map(|_| self.host_lifecycle_lock(host_uri));
            let _lifecycle_guard = match &lifecycle {
                Some(lifecycle) => Some(lifecycle.lock().await),
                None => None,
            };
            if let Some(expected) = expected_incarnation
                && self
                    .latest_virtual_contents
                    .get(host_uri)
                    .is_none_or(|host| host.incarnation != expected)
            {
                return;
            }
            let virtual_uri =
                VirtualDocumentUri::new(host_uri_lsp, &injection.language, &injection.region_id);

            // Verify `handle` is still the pool's LIVE connection for its key and
            // claim + didOpen this ONE injection under the `connections` lock,
            // then release before the next iteration — the same respawn guard the
            // request (execute.rs) and host (host.rs) paths use, scoped per
            // injection so the (best-effort, possibly many-region) eager open
            // never holds the pool lock across the whole loop and stalls
            // unrelated requests/spawns/cancels. Without the guard, a concurrent
            // respawn (get_or_create's SpawnNew branch) that purged this key's
            // tracker could let a claim through the dead handle, marking the doc
            // open while the fresh process never received the didOpen. Lock order
            // connections → document tracker matches the respawn purge. If a
            // respawn replaced the handle mid-loop, stop: the purge lets the next
            // real request re-open cleanly.
            let connections = self.connections().await;
            if !connections
                .get(&connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager open: connection {} replaced mid-loop; stopping",
                    connection_key
                );
                return;
            }

            if let Err(e) = self
                .ensure_document_opened(
                    &mut sender,
                    host_uri,
                    &virtual_uri,
                    &injection.content,
                    &connection_key,
                )
                .await
            {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager open: failed to open {} on {}: {}",
                    virtual_uri.to_uri_string(),
                    server_name,
                    e
                );
            }
        }
    }

    /// Fire `didOpen` for the real host document on a `_self` host-bridge server
    /// (host-document-bridge), so a push-only host server (no `textDocument/diagnostic`
    /// support) starts analyzing and pushing diagnostics immediately, instead of
    /// only after the first host-bridged request lazily opens it. Fire-and-forget:
    /// failures are logged at debug and never propagated.
    pub(crate) async fn eager_open_host_document(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        host_uri: &url::Url,
        language_id: &str,
        text: &str,
        live_text_reader: Option<&(dyn Fn() -> Option<Arc<str>> + Send + Sync)>,
    ) {
        let handle = match self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
                Some(host_uri),
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager host open: server {} not ready for {}: {}",
                    server_name,
                    host_uri,
                    e
                );
                return;
            }
        };

        // Borrow the key (no clone) — both `connections.get` and `sync_host_document`
        // take it by reference, like `execute_host_request`.
        let connection_key = handle.key();
        // Sync (sends didOpen) under the `connections` + `host_documents` locks in
        // that order, with the live-handle `Arc::ptr_eq` check — identical to
        // `execute_host_request`, so a concurrent respawn purge cannot interleave
        // and leave sync state the replacement never saw.
        let connections = self.connections().await;
        if !connections
            .get(connection_key)
            .is_some_and(|current| Arc::ptr_eq(current, &handle))
        {
            // Replaced by a respawn between wait-ready and here; the new connection
            // will sync lazily on its first request.
            return;
        }
        let mut docs = self.host_documents().await;
        let mut sender = ConnectionHandleSender(&handle);
        let doc = super::host::HostDocument {
            uri: host_uri,
            language_id,
            text,
        };
        if let Err(e) = super::host::sync_host_document(
            &mut sender,
            &mut docs,
            &doc,
            live_text_reader,
            connection_key,
        )
        .await
        {
            log::debug!(
                target: "kakehashi::bridge",
                "Eager host open: didOpen failed for {} on {}: {}",
                host_uri,
                server_name,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::pool::test_helpers::*;
    use super::super::super::pool::{ConnectionState, LanguageServerPool};
    use super::super::super::protocol::VirtualDocumentUri;

    /// Test that eager_open_virtual_documents marks virtual documents as opened.
    ///
    /// Given a ready server and injection data, calling eager_open_virtual_documents
    /// should result in each virtual document being marked as opened in DocumentTracker.
    #[tokio::test]
    async fn eager_open_marks_documents_as_opened() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let server_name = "test-server";

        // Pre-create a ready connection so eager_open_virtual_documents finds it
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            crate::lsp::bridge::ConnectionKey::for_server(server_name),
        )
        .await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("eager_open");
        let host_uri_lsp = url_to_uri(&host_uri);

        use super::super::super::coordinator::BridgeInjection;
        let injections = vec![
            BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_0.to_string(),
                content: "print('hello')".to_string(),
            },
            BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_1.to_string(),
                content: "print('world')".to_string(),
            },
        ];

        pool.eager_open_virtual_documents(
            server_name,
            &config,
            &host_uri,
            &host_uri_lsp,
            None,
            injections,
        )
        .await;

        // Verify both virtual documents are marked as opened
        let vuri_0 = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        let vuri_1 = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_1);

        assert!(
            pool.is_document_opened(&vuri_0),
            "First virtual document should be marked as opened"
        );
        assert!(
            pool.is_document_opened(&vuri_1),
            "Second virtual document should be marked as opened"
        );
    }

    /// Test that eager_open_virtual_documents is idempotent.
    ///
    /// Calling it twice with the same injections should not cause errors or
    /// duplicate didOpen notifications. The second call should be a no-op
    /// for already-opened documents.
    #[tokio::test]
    async fn eager_open_is_idempotent() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let server_name = "test-server";

        let handle = create_handle_with_key(
            ConnectionState::Ready,
            crate::lsp::bridge::ConnectionKey::for_server(server_name),
        )
        .await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("idempotent");
        let host_uri_lsp = url_to_uri(&host_uri);

        use super::super::super::coordinator::BridgeInjection;
        let injections = vec![BridgeInjection {
            language: "lua".to_string(),
            region_id: TEST_ULID_LUA_0.to_string(),
            content: "print('hello')".to_string(),
        }];

        // First call - should open the document
        pool.eager_open_virtual_documents(
            server_name,
            &config,
            &host_uri,
            &host_uri_lsp,
            None,
            injections.clone(),
        )
        .await;

        let vuri = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        assert!(
            pool.is_document_opened(&vuri),
            "Should be opened after first call"
        );

        // Second call - should be a no-op (idempotent)
        pool.eager_open_virtual_documents(
            server_name,
            &config,
            &host_uri,
            &host_uri_lsp,
            None,
            injections,
        )
        .await;

        assert!(
            pool.is_document_opened(&vuri),
            "Should still be opened after second call"
        );
    }
}
