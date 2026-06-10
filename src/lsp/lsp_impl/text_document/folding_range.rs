//! Folding range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{FoldingRange, FoldingRangeParams};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        self.whole_document_preferred_fan_out(
            &params.text_document.uri,
            "textDocument/foldingRange",
            |t| async move {
                t.pool
                    .send_folding_range_request(
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
