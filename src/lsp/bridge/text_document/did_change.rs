//! didChange notification handling for bridge connections.
//!
//! This module provides didChange notification forwarding for downstream language servers,
//! propagating document changes from host documents to their virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_notification()` to queue didChange notifications via the
//! channel-based writer task. This replaces the previous `tokio::spawn` fire-and-forget
//! pattern that could violate FIFO ordering.

use std::sync::Arc;
use url::Url;

use super::super::pool::{ConnectionHandle, ConnectionState, LanguageServerPool};
use super::super::protocol::VirtualDocumentUri;

impl LanguageServerPool {
    /// Forward didChange notifications to all opened virtual documents for a host document.
    ///
    /// When the host document (e.g., markdown file) changes, this method:
    /// 1. Gets the list of opened virtual documents for the host
    /// 2. For each injection that has an opened virtual document, sends didChange
    /// 3. Skips injections that haven't been opened yet (didOpen will be sent on first request)
    ///
    /// Uses full content sync (TextDocumentSyncKind::Full) for simplicity.
    ///
    /// # Single-Writer Loop (ADR-0015)
    ///
    /// All didChange notifications are queued via `send_notification()` which ensures
    /// FIFO ordering. This is non-blocking (fire-and-forget semantics) but maintains
    /// proper message ordering, unlike the previous `tokio::spawn` approach.
    ///
    /// # Arguments
    /// * `host_uri` - The host document URI
    /// * `injections` - All injection regions in the host document
    ///
    // TODO: Support incremental didChange (TextDocumentSyncKind::Incremental) for better
    // performance with large documents. Currently uses full sync for simplicity.
    pub(crate) async fn forward_didchange_to_opened_docs(
        &self,
        host_uri: &Url,
        injections: &[crate::lsp::bridge::coordinator::InjectionRegion],
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

            // Check if this virtual doc has been claimed or opened on a downstream server.
            // The claim happens before the actual didOpen send (see try_claim_for_open),
            // but FIFO ordering via the single-writer loop ensures didChange arrives
            // after didOpen on the wire.
            if !self.is_document_opened(&virtual_uri) {
                // Not opened yet - didOpen will be sent on first request
                continue;
            }

            // Look up ALL server names that have this virtual doc open.
            // Multiple servers may handle the same language (e.g., emmylua and lua_ls).
            let server_names = self.get_all_servers_for_virtual_uri(&virtual_uri);

            for server_name in server_names {
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
                    let Some(handle) = connections.get(&server_name) else {
                        continue;
                    };

                    if handle.state() != ConnectionState::Ready {
                        continue;
                    }

                    Arc::clone(handle)
                };

                // increment_document_version acts as per-server filter:
                // returns None if this server hasn't registered the doc.
                let Some(version) = self
                    .increment_document_version(&virtual_uri, &server_name)
                    .await
                else {
                    continue;
                };

                // Send didChange notification via single-writer loop (ADR-0015).
                // This is non-blocking and maintains FIFO ordering.
                Self::send_didchange_for_virtual_doc(
                    &handle,
                    &virtual_uri.to_uri_string(),
                    &injection.content,
                    version,
                );
            }
        }
    }

    /// Send a didChange notification for a virtual document.
    ///
    /// Uses the channel-based single-writer loop (ADR-0015) to send the notification.
    /// This is non-blocking - if the queue is full, the notification is dropped
    /// with a warning log.
    ///
    /// # Arguments
    /// * `handle` - The connection handle
    /// * `virtual_uri` - The virtual document URI string
    /// * `content` - The new content for the virtual document
    /// * `version` - The document version number
    fn send_didchange_for_virtual_doc(
        handle: &Arc<ConnectionHandle>,
        virtual_uri: &str,
        content: &str,
        version: i32,
    ) {
        // Build the didChange notification
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": virtual_uri,
                    "version": version
                },
                "contentChanges": [
                    {
                        "text": content
                    }
                ]
            }
        });

        // Send via the single-writer loop (non-blocking, fire-and-forget)
        handle.send_notification(notification);
    }
}
