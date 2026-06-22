//! willSave notification handler for Kakehashi (#357).

use tower_lsp_server::ls_types::WillSaveTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    /// Handle `textDocument/willSave`.
    ///
    /// Forwards the notification verbatim to host-bridge servers that already
    /// have the host document open (host-document-bridge, #357). willSave
    /// concerns the host document being saved, so only the host layer is
    /// notified — virtual-document servers learn of saves through the existing
    /// didSave forwarding, and propagating save edits from virtual regions
    /// overlaps with the concatenated formatting pipeline (deferred to a
    /// follow-up should a concrete need appear). Fire-and-forget: no response
    /// is awaited and no connection is lazily spawned.
    pub(crate) async fn will_save_impl(&self, params: WillSaveTextDocumentParams) {
        let Ok(uri) = uri_to_url(&params.text_document.uri) else {
            log::warn!(
                "Invalid URI in willSave: {}",
                params.text_document.uri.as_str()
            );
            return;
        };

        self.bridge
            .pool_arc()
            .notify_host_will_save(&uri, &params)
            .await;
    }
}
