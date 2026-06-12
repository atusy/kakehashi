//! Code lens method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CodeLens, CodeLensParams};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn code_lens_impl(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        self.whole_document_preferred_fan_out(
            &params.text_document.uri,
            "textDocument/codeLens",
            raw_params,
            |t| async move {
                t.pool
                    .send_code_lens_request(
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
