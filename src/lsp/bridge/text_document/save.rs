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
    /// The notification carries no `text`: kakehashi advertises
    /// `save.includeText = false`, and the virt server's document is already
    /// current from the didChange stream, so the bare identifier is enough.
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
    /// Locking mirrors [`Self::forward_didchange_to_opened_docs`]: the doc list
    /// is snapshotted once, then the live handle is re-fetched per iteration
    /// under the `connections` lock and checked for `Ready` — so a connection
    /// that respawned after the snapshot can never be sent to via a stale
    /// handle. `send_notification` is a non-blocking queue write (FIFO
    /// single-writer loop, ls-bridge-message-ordering).
    async fn forward_save_notification_to_virtual_docs(
        &self,
        host_uri: &Url,
        method: &'static str,
        capability: &'static str,
        build_params: impl Fn(&str) -> serde_json::Value,
    ) {
        let docs = self.host_virtual_docs(host_uri).await;
        for doc in docs {
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
