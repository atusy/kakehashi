//! willSave / didSave notification fan-out to virtual-document servers (#357).
//!
//! willSave and didSave concern the *host* document, but virt-bridge servers
//! only know the per-injection virtual documents projected from it. We forward
//! these save-time notifications to every virtual document currently open for
//! the host, rewriting the URI to the **virtual** one, so a virt server that
//! opts into save hooks can react to the host save.
//!
//! Notifications only — `willSaveWaitUntil` stays host-only: its returned edits
//! would need virtual→host translation and cross-region aggregation that overlap
//! the concatenated formatting pipeline (see host-document-bridge).
//!
//! Fire-and-forget: only servers that already have the virtual document open and
//! that advertise the capability are notified; no lazy spawn.

use std::sync::Arc;

use tower_lsp_server::ls_types::TextDocumentSaveReason;
use url::Url;

use super::super::pool::{ConnectionState, LanguageServerPool};
use super::super::protocol::JsonRpcNotification;

impl LanguageServerPool {
    /// Forward `textDocument/willSave` to every open virtual document of
    /// `host_uri`, on each live server that advertises `willSave` (#357). The
    /// host `reason` is forwarded verbatim; only the URI is rewritten to the
    /// virtual document the downstream server knows.
    pub(crate) async fn forward_will_save_to_virtual_docs(
        &self,
        host_uri: &Url,
        reason: TextDocumentSaveReason,
    ) {
        self.forward_save_notification_to_virtual_docs(
            host_uri,
            "textDocument/willSave",
            "textDocument/willSave",
            |virtual_uri| {
                serde_json::json!({ "textDocument": { "uri": virtual_uri }, "reason": reason })
            },
        )
        .await;
    }

    /// Forward `textDocument/didSave` to every open virtual document of
    /// `host_uri`, on each live server that advertises `save` (#357).
    ///
    /// The notification carries no `text` even if the downstream server
    /// requested `save.includeText = true`: the virtual document is already
    /// current from the didChange stream, so the saved text would be redundant.
    /// `text` is optional in `DidSaveTextDocumentParams`, and re-deriving each
    /// region's content here is not worth it for a fire-and-forget save hook.
    pub(crate) async fn forward_did_save_to_virtual_docs(&self, host_uri: &Url) {
        self.forward_save_notification_to_virtual_docs(
            host_uri,
            "textDocument/didSave",
            "textDocument/didSave",
            |virtual_uri| serde_json::json!({ "textDocument": { "uri": virtual_uri } }),
        )
        .await;
    }

    /// Shared fan-out: snapshot the host's open virtual docs and send `method`
    /// to each live/Ready connection that advertises `capability`, with the
    /// params built per virtual document by `build_params` (the virtual URI is
    /// the document the downstream server actually knows).
    ///
    /// Mirrors [`Self::forward_didchange_to_opened_docs`]'s two-stage liveness
    /// guard so the (necessarily lock-free) snapshot can't send to a stale
    /// target:
    /// 1. the `(virtual_uri, connection)` pair is re-checked against the **live**
    ///    reverse index ([`Self::get_all_connections_for_virtual_uri`]) — a
    ///    concurrent `didClose`/respawn that closed or purged the doc drops it
    ///    here (didChange gets the same gate from `increment_document_version`
    ///    returning `None`);
    /// 2. the handle is re-fetched per iteration under the `connections` lock
    ///    and must be `Ready`, so a respawned connection is never sent to via a
    ///    stale handle.
    ///
    /// `send_notification` is a non-blocking queue write (FIFO single-writer
    /// loop, ls-bridge-message-ordering).
    async fn forward_save_notification_to_virtual_docs(
        &self,
        host_uri: &Url,
        method: &'static str,
        capability: &'static str,
        build_params: impl Fn(&str) -> serde_json::Value,
    ) {
        let docs = self.host_virtual_docs(host_uri).await;
        for doc in docs {
            // Stage 1: the snapshot can go stale — only send if this connection
            // STILL has this virtual document open per the live reverse index.
            if !self
                .get_all_connections_for_virtual_uri(&doc.virtual_uri)
                .contains(&doc.connection_key)
            {
                continue;
            }
            // Stage 2: re-fetch the live handle and require Ready.
            let handle = {
                let connections = self.connections().await;
                match connections.get(&doc.connection_key) {
                    Some(handle) if handle.state() == ConnectionState::Ready => Arc::clone(handle),
                    _ => continue,
                }
            };
            if !handle.has_capability(capability) {
                continue;
            }
            let virtual_uri = doc.virtual_uri.to_uri_string();
            let notification = JsonRpcNotification::new(method, build_params(&virtual_uri));
            handle.send_notification(notification);
        }
    }
}
