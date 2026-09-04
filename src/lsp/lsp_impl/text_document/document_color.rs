//! Document color method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{ColorInformation, DocumentColorParams};

use super::super::Kakehashi;
use super::super::bridge_context::parse_host_verbatim;

impl Kakehashi {
    pub(crate) async fn document_color_impl(
        &self,
        params: DocumentColorParams,
    ) -> Result<Vec<ColorInformation>> {
        // Experimental (KAKEHASHI_EXPERIMENTAL=true): without the opt-in the
        // capability is not advertised, so answer a compliant empty result to
        // any client that calls regardless.
        if !self.experimental_enabled() {
            return Ok(Vec::new());
        }
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        self.whole_document_fan_out(
            &params.text_document.uri,
            "textDocument/documentColor",
            raw_params,
            None,
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
                let colors = t
                    .pool
                    .send_document_color_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await?;
                Ok((!colors.is_empty()).then_some(colors))
            },
            |value| Ok(parse_host_verbatim::<Vec<ColorInformation>>(value)),
            |won| Ok(Some(won.items)),
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
        .map(Option::unwrap_or_default)
    }
}
