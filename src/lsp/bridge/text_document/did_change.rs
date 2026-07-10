//! didChange notification handling for bridge connections.
//!
//! This module provides didChange notification forwarding for downstream language servers,
//! propagating document changes from host documents to their virtual documents.
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_notification()` to queue didChange notifications via the
//! channel-based writer task. This replaces the previous `tokio::spawn` fire-and-forget
//! pattern that could violate FIFO ordering.

use std::str::FromStr;
use std::sync::Arc;

use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, TextDocumentContentChangeEvent, Uri,
    VersionedTextDocumentIdentifier,
};
use url::Url;

use super::super::pool::{
    ConnectionHandle, ConnectionState, LanguageServerPool, NotificationSendResult,
};
use super::super::protocol::{JsonRpcNotification, VirtualDocumentUri};

impl LanguageServerPool {
    /// Send `didChange` to every already-opened virtual document for `host_uri`,
    /// skipping injections that haven't been opened yet (`didOpen` happens on
    /// their first request). Uses full content sync for simplicity.
    ///
    /// Notifications go through `send_notification()` (single writer task,
    /// ls-bridge-message-ordering) for FIFO order — fire-and-forget but no reordering.
    // TODO: switch to TextDocumentSyncKind::Incremental for large documents.
    pub(crate) async fn forward_didchange_to_opened_docs(
        &self,
        host_uri: &Url,
        injections: &[crate::lsp::bridge::coordinator::BridgeInjection],
    ) {
        // Convert host_uri to lsp_types::Uri for bridge protocol functions
        let host_uri_lsp = match crate::lsp::lsp_impl::url_to_uri(host_uri) {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Failed to convert host URI, skipping didChange: {}",
                    e
                );
                return;
            }
        };

        // For each injection, check if it's actually opened and send didChange
        for injection in injections {
            let virtual_uri =
                VirtualDocumentUri::new(&host_uri_lsp, &injection.language, &injection.region_id);

            // Only sent/open documents appear here. A pre-send eager-open claim
            // stays out of the reverse index, so didChange can never overtake its
            // didOpen merely because both eventually use the same FIFO.
            if !self.is_document_opened(&virtual_uri) {
                // Not opened yet - didOpen will be sent on first request
                continue;
            }

            // Look up ALL connections that have this virtual doc open.
            // Multiple servers may handle the same language (e.g., emmylua and
            // lua_ls), and one server may back several workspace roots (#382).
            let connection_keys = self.get_all_connections_for_virtual_uri(&virtual_uri);

            for connection_key in connection_keys {
                // Check connection state BEFORE incrementing version.
                // Non-Ready servers shouldn't consume version numbers.
                //
                // TOCTOU note: The connection state is checked under the `connections`
                // lock, but the lock is released before `increment_document_version`
                // acquires a separate lock. A server could transition away from Ready
                // between these two operations. This is acceptable: worst case is a
                // wasted version number and a silently-failed send (the channel write
                // is non-blocking fire-and-forget).
                let handle = {
                    let connections = self.connections().await;
                    let Some(handle) = connections.get(&connection_key) else {
                        continue;
                    };

                    if handle.state() != ConnectionState::Ready {
                        continue;
                    }

                    Arc::clone(handle)
                };

                // Per-connection filter (returns None if this connection hasn't
                // registered the doc) AND content guard: skip the re-send when this
                // region's extracted content is unchanged since the last didChange to
                // this connection, so a position-only host edit doesn't re-analyze
                // every region (#422). Mirrors the host path's fingerprint guard.
                let Some(version) = self
                    .increment_version_if_content_changed(
                        &virtual_uri,
                        &connection_key,
                        &injection.content,
                    )
                    .await
                else {
                    continue;
                };

                // Send didChange notification via single-writer loop (ls-bridge-message-ordering).
                // This is non-blocking and maintains FIFO ordering.
                let send_result = Self::send_didchange_for_virtual_doc(
                    &handle,
                    &virtual_uri.to_uri_string(),
                    &injection.content,
                    version,
                );
                // Record the fingerprint ONLY on a confirmed enqueue (#422): a dropped
                // send (full/closed queue) must leave the old fingerprint so the next
                // edit re-sends, rather than marking content the server never received.
                if matches!(send_result, NotificationSendResult::Queued) {
                    self.record_sent_content_fingerprint(
                        &virtual_uri,
                        &connection_key,
                        &injection.content,
                    )
                    .await;
                }
            }
        }
    }

    /// Send a didChange notification for a virtual document, returning the
    /// enqueue outcome.
    ///
    /// Uses the channel-based single-writer loop (ls-bridge-message-ordering): non-blocking, and
    /// if the queue is full the notification is dropped with a warning log. The
    /// returned [`NotificationSendResult`] lets the caller record the content
    /// fingerprint only on a confirmed `Queued` enqueue (#422).
    fn send_didchange_for_virtual_doc(
        handle: &Arc<ConnectionHandle>,
        virtual_uri: &str,
        content: &str,
        version: i32,
    ) -> NotificationSendResult {
        let uri = match Uri::from_str(virtual_uri) {
            Ok(u) => u,
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Skipping didChange for invalid URI '{}': {}", virtual_uri, e
                );
                // Nothing was enqueued; treat as a non-`Queued` outcome so the caller
                // does not record a fingerprint for content the server never received.
                return NotificationSendResult::SerializationFailed;
            }
        };

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier::new(uri, version),
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: content.to_string(),
            }],
        };

        let notification = JsonRpcNotification::new("textDocument/didChange", params);

        // Send via the single-writer loop (non-blocking, fire-and-forget).
        handle.send_notification(notification)
    }
}
