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

use super::super::pool::{
    ConnectionHandle, ConnectionState, LanguageServerPool, NotificationSendResult,
};
use super::super::protocol::{JsonRpcNotification, VirtualDocumentUri};

fn enqueue_did_save_if_content_synced(
    did_change: Option<NotificationSendResult>,
    enqueue_did_save: impl FnOnce() -> NotificationSendResult,
) -> Option<NotificationSendResult> {
    if did_change.is_some_and(|outcome| !matches!(outcome, NotificationSendResult::Queued)) {
        return None;
    }
    Some(enqueue_did_save())
}

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
            |handle| handle.has_capability("textDocument/willSave"),
            |virtual_uri| {
                serde_json::json!({ "textDocument": { "uri": virtual_uri }, "reason": reason })
            },
        )
        .await;
    }

    /// Bring every eligible open virtual document to `injections`' current
    /// content and enqueue its textless didSave under the same per-document
    /// transition. A failed didChange enqueue suppresses didSave for that
    /// target, so a queue becoming writable between the two cannot expose a
    /// stale save hook.
    pub(crate) async fn sync_and_forward_did_save_to_virtual_docs(
        &self,
        host_uri: &Url,
        incarnation: u64,
        injections: &[crate::lsp::bridge::coordinator::BridgeInjection],
    ) {
        let Ok(host_uri_lsp) = crate::lsp::lsp_impl::url_to_uri(host_uri) else {
            return;
        };

        for injection in injections {
            self.record_latest_virtual_content(
                host_uri,
                incarnation,
                &injection.language,
                &injection.region_id,
                &injection.content,
            );
            let virtual_uri =
                VirtualDocumentUri::new(&host_uri_lsp, &injection.language, &injection.region_id);
            let connection_keys = self.connections_opening_or_opened(&virtual_uri);

            for connection_key in connection_keys {
                let connections = self.connections().await;
                let Some(handle) = connections
                    .get(&connection_key)
                    .filter(|handle| {
                        handle.state() == ConnectionState::Ready
                            && handle.accepts_textless_did_save()
                    })
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

                let did_change = if let Some(version) = self
                    .increment_version_if_content_changed(
                        &virtual_uri,
                        &connection_key,
                        &injection.content,
                    )
                    .await
                {
                    let outcome = Self::send_didchange_for_virtual_doc(
                        &handle,
                        &virtual_uri.to_uri_string(),
                        &injection.content,
                        version,
                    );
                    if matches!(outcome, NotificationSendResult::Queued) {
                        self.record_sent_content_fingerprint(
                            &virtual_uri,
                            &connection_key,
                            &injection.content,
                        )
                        .await;
                    }
                    Some(outcome)
                } else {
                    None
                };

                let virtual_uri = virtual_uri.to_uri_string();
                let notification = JsonRpcNotification::new(
                    "textDocument/didSave",
                    serde_json::json!({ "textDocument": { "uri": virtual_uri } }),
                );
                let _ = enqueue_did_save_if_content_synced(did_change, || {
                    handle.send_notification(notification)
                });
                drop(transition_guard);
            }
        }
    }

    /// Shared fan-out: snapshot the host's open virtual docs and send `method`
    /// to each live/Ready connection accepted by `supports`, with the params
    /// built per virtual document by `build_params` (the virtual URI is the
    /// document the downstream server actually knows).
    ///
    /// The `host_to_virtual` list is snapshotted under its own lock, which is
    /// released before `connections` is taken — so each send is guarded against
    /// a stale target by holding `connections` across the liveness recheck AND
    /// the send (order `connections` → tracker, matching the respawn purge in
    /// `pool.rs`):
    /// - `connections` is taken once after the snapshot and held across the
    ///   whole loop — no `.await` happens inside it. While it is held a respawn
    ///   purge cannot interleave, so a replacement process can never be installed
    ///   between the recheck and the send (a reverse-index check alone would NOT
    ///   close this — the purge could swap in a fresh Ready handle that never
    ///   opened the doc);
    /// - the handle must be the current `Ready` one and pass `supports` (cheap
    ///   checks, done first);
    /// - the `(virtual_uri, connection)` pair must STILL be in the **live**
    ///   reverse index ([`Self::is_virtual_doc_open_on_connection`]) — dropping a
    ///   doc a concurrent `didClose` removed (best-effort, the same accepted
    ///   TOCTOU `forward_didchange_to_opened_docs` has). This (cheap, but a
    ///   tracker lookup) runs last so it is skipped for un-Ready/unsupported docs.
    ///
    /// `send_notification` is a non-blocking queue write (FIFO single-writer
    /// loop, ls-bridge-message-ordering), so holding `connections` across the
    /// batch is cheap — the same discipline as `notify_host_will_save`.
    async fn forward_save_notification_to_virtual_docs(
        &self,
        host_uri: &Url,
        method: &'static str,
        supports: impl Fn(&ConnectionHandle) -> bool,
        build_params: impl Fn(&str) -> serde_json::Value,
    ) {
        let docs = self.host_virtual_docs(host_uri).await;
        // Common path (saving a file with no open injections): skip the
        // `connections` lock entirely.
        if docs.is_empty() {
            return;
        }
        let connections = self.connections().await;
        for doc in docs {
            // Cheap checks first: the current Ready handle that supports this
            // notification (all under the held `connections` lock, purge excluded).
            let Some(handle) = connections.get(&doc.connection_key) else {
                continue;
            };
            if handle.state() != ConnectionState::Ready || !supports(handle) {
                continue;
            }
            // Compute the virtual URI string once and reuse it for both the
            // liveness recheck and the notification params.
            let virtual_uri = doc.virtual_uri.to_uri_string();
            // Then the liveness recheck: only send if this connection STILL has
            // this virtual doc open (membership test, no reverse-index Vec clone).
            if !self.is_virtual_doc_open_on_connection(&virtual_uri, &doc.connection_key) {
                continue;
            }
            let notification = JsonRpcNotification::new(method, build_params(&virtual_uri));
            handle.send_notification(notification);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn queue_full_did_change_suppresses_textless_did_save() {
        let did_save_enqueued = Cell::new(false);
        let result =
            enqueue_did_save_if_content_synced(Some(NotificationSendResult::QueueFull), || {
                did_save_enqueued.set(true);
                NotificationSendResult::Queued
            });

        assert!(result.is_none());
        assert!(
            !did_save_enqueued.get(),
            "didSave must not be attempted after its prerequisite didChange was dropped"
        );
    }
}
