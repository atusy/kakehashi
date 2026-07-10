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
    ConnectionKey, ConnectionState, LanguageServerPool, NotificationSendResult, OpenedVirtualDoc,
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
        connection_key: &ConnectionKey,
    ) -> io::Result<()> {
        let uri_string = virtual_uri.to_uri_string();

        // Get the connection (if it exists and is Ready)
        let connections = self.connections().await;
        let Some(handle) = connections.get(connection_key) else {
            // No connection for this key - nothing to do
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

    /// Close a single scratch virtual document by URI: send didClose and remove
    /// it from all tracking state for `connection_key` (the `host_to_virtual`
    /// registration, plus the version/opened/reverse-index entries that
    /// `untrack_document` clears). A scratch URI is only ever opened against the
    /// one connection formatting that step, so per-connection untracking is complete.
    ///
    /// Used by the concatenated formatting pipeline, which opens a throwaway
    /// scratch virtual document per step (a unique URI carrying the accumulated
    /// text). The handler closes these from a single cancel-safe cleanup guard
    /// (a `Drop` that runs on a detached task) rather than per step — deferring
    /// keeps cancellation leak-free — so they never orphan tracking state,
    /// accumulate downstream documents, or leak diagnostics for the throwaway URI
    /// (concatenated-formatting-pipeline
    /// Decision point 7).
    ///
    /// The scratch URI is addressed directly (by URI + host) rather than looked
    /// up via the `host_to_virtual` ULID scan that
    /// [`close_invalidated_docs`](Self::close_invalidated_docs) uses. But it IS
    /// registered under its host document: `ensure_document_opened` calls
    /// `register_opened_document`, which inserts the scratch URI into
    /// `host_to_virtual` (alongside `document_versions`, `opened_documents`, and
    /// the reverse index). `untrack_document` alone does NOT remove the
    /// `host_to_virtual` entry, so we must also call `unregister_virtual_doc` —
    /// otherwise the scratch URI lingers in `host_to_virtual` until the host doc
    /// closes and gets a redundant second didClose then. Best-effort: a didClose
    /// failure is logged but never surfaced, since the pipeline must not fail on
    /// cleanup.
    pub(crate) async fn close_scratch_document(
        &self,
        host_uri: &Url,
        scratch_uri: &VirtualDocumentUri,
        connection_key: &ConnectionKey,
    ) {
        let transition = self.open_transition_lock(scratch_uri, connection_key);
        let transition_guard = transition.lock().await;
        // Idempotent close: if the scratch document is no longer open, a
        // concurrent cleanup (e.g. `close_host_document`) already closed and
        // untracked it, so another `didClose` would be a redundant notification
        // the downstream server may warn about. Nothing left to do.
        if !self.is_document_opened(scratch_uri) {
            drop(transition_guard);
            self.remove_open_transition_lock_if_unshared(scratch_uri, connection_key, &transition);
            return;
        }
        // Remove from ALL tracking BEFORE awaiting the didClose send, to narrow
        // the window in which a concurrent `close_host_document` finds this
        // scratch URI in `host_to_virtual` and issues a redundant double-close.
        // (It only narrows, not eliminates: if `close_host_document` already
        // snapshotted the host's virtual-doc list before this removal, it can
        // still send a second didClose — a harmless, best-effort redundancy the
        // `is_document_opened` guard above also helps avoid.) `untrack_document`
        // does not touch `host_to_virtual`, so we remove that registration (added
        // by `ensure_document_opened`) explicitly.
        //
        // Untrack-first (rather than didClose-first) is safe against cancel-drop
        // because the concatenated pipeline's `ScratchCleanupGuard` runs this on a
        // DETACHED task that is not a child of the aborted dispatch future — so the
        // didClose await below always completes even when the caller is cancelled
        // mid-flight. The ordering is therefore kept to preserve the
        // double-close-race fix without reintroducing an orphaned-downstream-doc
        // leak on cancel.
        self.unregister_virtual_doc(host_uri, scratch_uri).await;
        self.untrack_document(scratch_uri, connection_key).await;
        if let Err(e) = self
            .send_didclose_notification(scratch_uri, connection_key)
            .await
        {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to send didClose for scratch document {}: {}",
                scratch_uri.to_uri_string(), e
            );
        }
        drop(transition_guard);
        self.remove_open_transition_lock_if_unshared(scratch_uri, connection_key, &transition);
    }

    /// Close a single virtual document: send didClose and remove from tracking.
    ///
    /// This is the core cleanup operation used by both `close_host_document`
    /// and `close_invalidated_docs`. Errors are logged but do not prevent
    /// cleanup of the document_versions tracking.
    async fn close_single_virtual_doc(&self, doc: &OpenedVirtualDoc) {
        let transition = self.open_transition_lock(&doc.virtual_uri, &doc.connection_key);
        let transition_guard = transition.lock().await;
        if let Err(e) = self
            .send_didclose_notification(&doc.virtual_uri, &doc.connection_key)
            .await
        {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to send didClose for {}: {}",
                doc.virtual_uri.to_uri_string(), e
            );
        }
        // Use the connection key from OpenedVirtualDoc for per-connection tracking
        self.untrack_document(&doc.virtual_uri, &doc.connection_key)
            .await;
        drop(transition_guard);
        self.remove_open_transition_lock_if_unshared(
            &doc.virtual_uri,
            &doc.connection_key,
            &transition,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::pool::test_helpers::url_to_uri;

    /// `close_scratch_document` untracks a directly-addressed scratch virtual
    /// document.
    ///
    /// The concatenated formatting pipeline opens a throwaway scratch document
    /// per step (a unique URI registered in `host_to_virtual` under its host,
    /// like any virtual doc) and must be able to close + untrack it by URI alone
    /// — without going through the `host_to_virtual` ULID scan that
    /// `close_host_document` uses. This guards the
    /// scratch lifecycle that isolates the pipeline's speculative state
    /// (concatenated-formatting-pipeline Decision point 7).
    #[tokio::test]
    async fn close_scratch_document_untracks_directly_addressed_doc() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        // A scratch URI uses a step-suffixed region_id, distinct from the
        // canonical region's virtual document.
        let scratch_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "REGION-scratch-0");

        // Simulate the per-step didOpen having registered the scratch doc.
        pool.register_opened_document(&host_uri, &scratch_uri, &ConnectionKey::for_server("black"))
            .await;
        assert!(
            pool.is_document_opened(&scratch_uri),
            "scratch doc should be tracked after register"
        );

        // No connection exists for "black", so the didClose send is a no-op,
        // but the untrack must still happen so the scratch doc never lingers.
        pool.close_scratch_document(&host_uri, &scratch_uri, &ConnectionKey::for_server("black"))
            .await;

        assert!(
            !pool.is_document_opened(&scratch_uri),
            "scratch doc must be untracked after close_scratch_document"
        );

        // Idempotent: a second close (the scratch doc is already gone, e.g. a
        // concurrent close_host_document beat the per-step/sweep cleanup) is a
        // safe no-op rather than a redundant didClose.
        pool.close_scratch_document(&host_uri, &scratch_uri, &ConnectionKey::for_server("black"))
            .await;
        assert!(
            !pool.is_document_opened(&scratch_uri),
            "second close must remain a no-op"
        );
    }

    /// `close_scratch_document` removes the scratch URI from `host_to_virtual`.
    ///
    /// `ensure_document_opened` registers every virtual URI (scratch included)
    /// in `host_to_virtual` via `register_opened_document`. `untrack_document`
    /// alone does not clear that map, so without the extra `unregister_virtual_doc`
    /// the scratch URI would linger under its host until the host doc closes —
    /// where it would receive a redundant second didClose. This guards that the
    /// scratch lifecycle leaves no `host_to_virtual` residue.
    #[tokio::test]
    async fn close_scratch_document_removes_host_to_virtual_registration() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let scratch_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "REGION-scratch-0");

        // Same path ensure_document_opened takes: register under the host.
        pool.register_opened_document(&host_uri, &scratch_uri, &ConnectionKey::for_server("black"))
            .await;
        assert_eq!(
            pool.get_all_connections_for_virtual_uri(&scratch_uri),
            vec![ConnectionKey::for_server("black")],
            "scratch doc should be reachable via host_to_virtual after register"
        );

        pool.close_scratch_document(&host_uri, &scratch_uri, &ConnectionKey::for_server("black"))
            .await;

        // After close, the host_to_virtual registration is gone: closing the
        // host document must NOT surface the scratch doc again (no double-close).
        let remaining = pool.close_host_document(&host_uri).await;
        assert!(
            remaining
                .iter()
                .all(|d| d.virtual_uri.to_uri_string() != scratch_uri.to_uri_string()),
            "scratch doc must not linger in host_to_virtual after close_scratch_document"
        );
    }

    /// A scratch URI is distinct from the canonical region URI, so an
    /// already-open canonical document does NOT suppress the scratch `didOpen`.
    ///
    /// This is the crux of the stale-content fix: when the canonical region
    /// document is already open for a server (e.g. after a prior hover), the
    /// formatting step targets a *different* URI, so `ensure_document_opened`
    /// (gated on `is_document_opened`) still sends a fresh `didOpen` carrying the
    /// current accumulated text rather than reusing the stale open document.
    #[tokio::test]
    async fn scratch_uri_is_not_suppressed_by_open_canonical_document() {
        let pool = LanguageServerPool::new();
        let host_uri = Url::parse("file:///project/doc.md").unwrap();
        let canonical_uri = VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "REGION");
        let scratch_uri =
            VirtualDocumentUri::new(&url_to_uri(&host_uri), "python", "REGION-scratch-0");

        // Canonical region document already open downstream (the bug trigger).
        pool.register_opened_document(
            &host_uri,
            &canonical_uri,
            &ConnectionKey::for_server("black"),
        )
        .await;

        assert!(
            pool.is_document_opened(&canonical_uri),
            "canonical doc is open"
        );
        assert!(
            !pool.is_document_opened(&scratch_uri),
            "scratch doc must be seen as NOT open so a fresh didOpen carries the current text"
        );
    }
}
