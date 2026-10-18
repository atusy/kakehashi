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

use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, TextDocumentContentChangeEvent, Uri,
    VersionedTextDocumentIdentifier,
};
use url::Url;

use super::super::pool::{ConnectionState, LanguageServerPool, NotificationSendResult};
use super::super::protocol::{JsonRpcNotification, VirtualDocumentUri};

impl LanguageServerPool {
    /// Send `didChange` to every already-opened virtual document for `host_uri`,
    /// skipping injections that haven't been opened yet (`didOpen` happens on
    /// their first request). Uses full content sync for simplicity. Returns
    /// false if any attempted enqueue fails, leaving its sent fingerprint old
    /// so a synchronization waiter can retry.
    ///
    /// Notifications are recorded and queued together on the connection's
    /// single writer task (`record_and_queue`, ls-bridge-message-ordering)
    /// for FIFO order — fire-and-forget but no reordering.
    // TODO: switch to TextDocumentSyncKind::Incremental for large documents.
    pub(crate) async fn forward_didchange_to_opened_docs(
        &self,
        host_uri: &Url,
        incarnation: u64,
        injections: &[crate::lsp::bridge::coordinator::BridgeInjection],
    ) -> bool {
        // Convert host_uri to lsp_types::Uri for bridge protocol functions
        let host_uri_lsp = match crate::lsp::lsp_impl::url_to_uri(host_uri) {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Failed to convert host URI, skipping didChange: {}",
                    e
                );
                return false;
            }
        };

        let mut synchronized = true;
        // For each injection, check if it's actually opened and send didChange
        for injection in injections {
            // Publish the latest content even when no downstream document is
            // open yet. A first interactive request may still be awaiting the
            // server handshake with an older snapshot; its didOpen consumes
            // this value after taking the per-document transition. Keep the
            // value after didOpen so a respawned server also reopens from the
            // latest host content; unchanged edits avoid replacing it.
            self.record_latest_virtual_content(
                host_uri,
                incarnation,
                &injection.language,
                &injection.region_id,
                injection.sent(),
            );
            let virtual_uri =
                VirtualDocumentUri::new(&host_uri_lsp, &injection.language, &injection.region_id);

            // Include pre-send claims, then serialize behind their didOpen
            // transition. Otherwise an edit in the claim→enqueue window would
            // be dropped and the downstream document could remain stale.
            // Multiple servers may handle the same language (e.g., emmylua and
            // lua_ls), and one server may back several workspace roots (#382).
            let connection_keys = self.connections_opening_or_opened(&virtual_uri);

            for connection_key in connection_keys {
                // Match didOpen's connections→transition order. Hold the map only
                // until this generation owns the transition: a respawn also waits
                // on that transition before purging the old generation, so the
                // cloned handle stays pinned without blocking unrelated map users
                // during the version/fingerprint awaits below.
                let connections = self.connections().await;
                let Some(handle) = connections
                    .get(&connection_key)
                    .filter(|handle| handle.state() == ConnectionState::Ready)
                    .cloned()
                else {
                    continue;
                };
                let transition = self.open_transition_lock(&virtual_uri, &connection_key);
                let transition_guard = transition.lock().await;
                drop(connections);
                if !self.is_document_opened_on_connection(&virtual_uri, &connection_key) {
                    drop(transition_guard);
                    self.remove_open_transition_lock_if_unshared(
                        &virtual_uri,
                        &connection_key,
                        &transition,
                    );
                    continue;
                }

                // Per-connection filter (returns None if this connection hasn't
                // registered the doc) AND content guard: skip the re-send when this
                // region's extracted content is unchanged since the last didChange to
                // this connection, so a position-only host edit doesn't re-analyze
                // every region (#422). Mirrors the host path's fingerprint guard.
                let content = injection.sent();
                let Some(change) = self
                    .increment_version_if_content_changed(&virtual_uri, &connection_key, content)
                    .await
                else {
                    continue;
                };

                // Send didChange notification via single-writer loop (ls-bridge-message-ordering).
                // This is non-blocking and maintains FIFO ordering. A dropped
                // send (full/closed queue) leaves the old fingerprint so the
                // next edit re-sends (#422).
                let send_result = self.send_didchange_recorded(
                    &handle,
                    &virtual_uri,
                    &connection_key,
                    content,
                    change,
                );
                if !matches!(send_result, Some(NotificationSendResult::Queued)) {
                    synchronized = false;
                }
                drop(transition_guard);
            }
        }
        synchronized
    }

    /// The serialized didChange notification for a virtual document; `None`
    /// when it cannot be built: nothing is sent, nor recorded as sent (#422).
    pub(in crate::lsp::bridge) fn didchange_payload(
        virtual_uri: &str,
        content: &str,
        version: i32,
    ) -> Option<serde_json::Value> {
        let uri = match Uri::from_str(virtual_uri) {
            Ok(u) => u,
            Err(e) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "Skipping didChange for invalid URI '{}': {}", virtual_uri, e
                );
                return None;
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
        serde_json::to_value(notification)
            .inspect_err(|e| {
                log::error!(
                    target: "kakehashi::bridge",
                    "Failed to serialize didChange for '{}': {}", virtual_uri, e
                );
            })
            .ok()
    }
}
