//! Document link method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentLink, DocumentLinkParams};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn document_link_impl(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        self.whole_document_preferred_fan_out(
            &params.text_document.uri,
            "textDocument/documentLink",
            |t| async move {
                t.pool
                    .send_document_link_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
        )
        .await
    }
}
