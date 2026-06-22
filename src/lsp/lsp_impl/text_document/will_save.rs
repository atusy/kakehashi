//! willSave notification handler for Kakehashi (#357).

use tower_lsp_server::ls_types::WillSaveTextDocumentParams;

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    /// Handle `textDocument/willSave`.
    ///
    /// Forwards the notification to **both** bridge layers (#357):
    /// - **host-bridge** servers that already have the host document open
    ///   (verbatim: real URI + reason);
    /// - **virt-bridge** servers that have a virtual document open for this
    ///   host, with the URI rewritten to the virtual document and the reason
    ///   forwarded verbatim.
    ///
    /// Each layer notifies only servers that already have the document open and
    /// advertise `willSave` — the per-server opt-in is what keeps a virt server
    /// from being told about a fragment "save" it never asked to hear about.
    /// `willSaveWaitUntil` stays host-only: its returned edits would need
    /// virtual→host translation and cross-region aggregation overlapping the
    /// concatenated formatting pipeline. Fire-and-forget: no response is awaited
    /// and no connection is lazily spawned.
    pub(crate) async fn will_save_impl(&self, params: WillSaveTextDocumentParams) {
        let Ok(uri) = uri_to_url(&params.text_document.uri) else {
            log::warn!(
                "Invalid URI in willSave: {}",
                params.text_document.uri.as_str()
            );
            return;
        };

        let pool = self.bridge.pool_arc();
        pool.notify_host_will_save(&uri, &params).await;
        pool.forward_will_save_to_virtual_docs(&uri, params.reason)
            .await;
    }
}
