//! didClose notification handling for bridge connections.
//!
//! Cleans up when host documents are closed or regions are invalidated.
//! Notifications are queued via the channel-based writer task (ls-bridge-message-ordering) for
//! FIFO ordering.

use std::io;
use std::sync::Arc;
use ulid::Ulid;
use url::Url;

use super::super::pool::{
    ConnectionState, LanguageServerPool, NotificationSendResult, OpenedVirtualDoc,
};
use super::super::protocol::{VirtualDocumentUri, build_didclose_notification};

impl LanguageServerPool {
    /// Send a didClose notification for a virtual document.
    ///
    /// The connection is NOT closed after sending — it stays available for other
    /// documents. Uses the channel-based single-writer loop (ls-bridge-message-ordering) for FIFO
    /// ordering. Returns `Ok(())` on success or when no connection exists for the
    /// server (nothing to do).
    pub(crate) async fn send_didclose_notification(
        &self,
        virtual_uri: &VirtualDocumentUri,
        server_name: &str,
    ) -> io::Result<()> {
        let uri_string = virtual_uri.to_uri_string();

        // Get the connection for this server (if it exists and is Ready)
        let connections = self.connections().await;
        let Some(handle) = connections.get(server_name) else {
            // No connection for this server - nothing to do
            return Ok(());
        };

        // Only send if connection is Ready
        if handle.state() != ConnectionState::Ready {
            return Ok(());
        }

        let handle = Arc::clone(handle);
        drop(connections); // Release lock before I/O

        // Build and send the didClose notification via single-writer loop (ls-bridge-message-ordering)
        let Some(notification) = build_didclose_notification(&uri_string) else {
            return Ok(());
        };

        match handle.send_notification(notification) {
            NotificationSendResult::Queued => Ok(()),
            NotificationSendResult::QueueFull => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge: didClose notification queue full",
            )),
            NotificationSendResult::ChannelClosed => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bridge: didClose notification channel closed",
            )),
            NotificationSendResult::SerializationFailed => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge: failed to serialize didClose notification",
            )),
        }
    }

    /// Close a single virtual document: send didClose and remove from tracking.
    ///
    /// This is the core cleanup operation used by both `close_host_document`
    /// and `close_invalidated_docs`. Errors are logged but do not prevent
    /// cleanup of the document_versions tracking.
    async fn close_single_virtual_doc(&self, doc: &OpenedVirtualDoc) {
        if let Err(e) = self
            .send_didclose_notification(&doc.virtual_uri, &doc.server_name)
            .await
        {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to send didClose for {}: {}",
                doc.virtual_uri.to_uri_string(), e
            );
        }
        // Use server_name from OpenedVirtualDoc for server-name-based tracking
        self.untrack_document(&doc.virtual_uri, &doc.server_name)
            .await;
    }

    /// Close all virtual documents associated with a host document, returning them.
    ///
    /// The connection to downstream language servers remains open — only the
    /// virtual documents are closed.
    pub(crate) async fn close_host_document(&self, host_uri: &Url) -> Vec<OpenedVirtualDoc> {
        // 1. Remove and get all virtual docs for this host
        let virtual_docs = self.remove_host_virtual_docs(host_uri).await;

        if virtual_docs.is_empty() {
            return vec![];
        }

        // 2. For each virtual doc: send didClose and remove from document_versions
        for doc in &virtual_docs {
            self.close_single_virtual_doc(doc).await;
        }

        virtual_docs
    }

    /// Close invalidated virtual documents (Phase 3).
    ///
    /// When region IDs are invalidated by edits, their virtual documents become
    /// orphaned in downstream LSs. Matching docs are atomically removed from
    /// tracking and sent didClose (best effort); docs never opened are skipped.
    pub(crate) async fn close_invalidated_docs(&self, host_uri: &Url, invalidated_ulids: &[Ulid]) {
        // Atomically remove matching docs from host_to_virtual
        let to_close = self
            .remove_matching_virtual_docs(host_uri, invalidated_ulids)
            .await;

        if to_close.is_empty() {
            // All invalidated ULIDs were never opened - nothing to close
            return;
        }

        // Send didClose and clean up tracking for each closed doc
        for doc in &to_close {
            self.close_single_virtual_doc(doc).await;
        }
    }
}
