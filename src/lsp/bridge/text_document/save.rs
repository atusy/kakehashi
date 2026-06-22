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
    /// The notification carries no `text`. Servers that demand
    /// `save.includeText = true` are filtered out by the capability gate
    /// (`has_capability("textDocument/didSave")`) rather than served a textless
    /// didSave: kakehashi advertises `includeText = false` upstream and so never
    /// receives the editor's saved bytes to forward. Servers that accept
    /// textless didSave already have current content from the didChange stream.
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
    /// The `host_to_virtual` list is necessarily snapshotted lock-free, so each
    /// send is guarded against a stale target by holding `connections` across
    /// the liveness recheck AND the send (order `connections` → tracker,
    /// matching the respawn purge in `pool.rs`):
    /// - `connections` is taken once after the snapshot and held across the
    ///   whole loop — no `.await` happens inside it. While it is held a respawn
    ///   purge cannot interleave, so a replacement process can never be installed
    ///   between the recheck and the send (a reverse-index check alone would NOT
    ///   close this — the purge could swap in a fresh Ready handle that never
    ///   opened the doc);
    /// - the handle must be the current `Ready` one and advertise the capability
    ///   (cheap checks, done first);
    /// - the `(virtual_uri, connection)` pair must STILL be in the **live**
    ///   reverse index ([`Self::is_virtual_doc_open_on_connection`]) — dropping a
    ///   doc a concurrent `didClose` removed (best-effort, the same accepted
    ///   TOCTOU `forward_didchange_to_opened_docs` has). This (cheap, but a
    ///   tracker lookup) runs last so it is skipped for un-Ready/incapable docs.
    ///
    /// `send_notification` is a non-blocking queue write (FIFO single-writer
    /// loop, ls-bridge-message-ordering), so holding `connections` across the
    /// batch is cheap — the same discipline as `notify_host_will_save`.
    async fn forward_save_notification_to_virtual_docs(
        &self,
        host_uri: &Url,
        method: &'static str,
        capability: &'static str,
        build_params: impl Fn(&str) -> serde_json::Value,
    ) {
        let docs = self.host_virtual_docs(host_uri).await;
        let connections = self.connections().await;
        for doc in docs {
            // Cheap checks first: the current Ready handle that advertises the
            // capability (all under the held `connections` lock, purge excluded).
            let Some(handle) = connections.get(&doc.connection_key) else {
                continue;
            };
            if handle.state() != ConnectionState::Ready || !handle.has_capability(capability) {
                continue;
            }
            // Then the liveness recheck: only send if this connection STILL has
            // this virtual doc open (membership test, no reverse-index Vec clone).
            if !self.is_virtual_doc_open_on_connection(&doc.virtual_uri, &doc.connection_key) {
                continue;
            }
            let virtual_uri = doc.virtual_uri.to_uri_string();
            let notification = JsonRpcNotification::new(method, build_params(&virtual_uri));
            handle.send_notification(notification);
        }
    }
}
