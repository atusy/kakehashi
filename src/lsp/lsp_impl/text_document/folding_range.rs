//! Folding range method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{FoldingRange, FoldingRangeParams};

use super::super::Kakehashi;
use super::super::bridge_context::parse_host_verbatim;

impl Kakehashi {
    pub(crate) async fn folding_range_impl(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        self.whole_document_fan_out(
            &params.text_document.uri,
            "textDocument/foldingRange",
            raw_params,
            // foldingRange is fast; not advertised for client progress (#437), so
            // no token is carried.
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            true,
            std::future::ready(Ok(None)),
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
            parse_host_verbatim::<Vec<FoldingRange>>,
            |won| Some(won.items),
            |mut acc, next| {
                acc.extend(next);
                acc
            },
            |mut acc, next| {
                acc.extend(next);
                acc
            },
        )
        .await
    }
}
