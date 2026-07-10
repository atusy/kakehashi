//! Document link method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentLink, DocumentLinkParams};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn document_link_impl(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        self.whole_document_fan_out(
            &params.text_document.uri,
            "textDocument/documentLink",
            raw_params,
            // documentLink is fast; not advertised for client progress (#437), so
            // no token is carried.
            None,
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
