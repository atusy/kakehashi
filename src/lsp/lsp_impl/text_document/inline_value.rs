//! Inline value method for Kakehashi.
//!
//! The preferred layer wins. Host servers receive the original request
//! verbatim; virtual servers receive both request ranges in virtual coordinates
//! and their result ranges are translated back to the host document.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    InlineValue, InlineValueContext, InlineValueParams, NumberOrString, Range, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/inlineValue";

impl Kakehashi {
    pub(crate) async fn inline_value_impl(
        &self,
        params: InlineValueParams,
    ) -> Result<Option<Vec<InlineValue>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let context = params.context;
        let progress_token = params.work_done_progress_params.work_done_token;

        let virt = self.inline_value_virt_layer(&lsp_uri, range, context, progress_token);
        let host = self.inline_value_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |values: &Vec<InlineValue>| !values.is_empty(),
        )
        .await
    }

    async fn inline_value_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<InlineValue>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let incarnation = ctx.incarnation;
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let fan_in = dispatch_host_preferred(
            &ctx,
            self.bridge.pool_arc(),
            move |t: HostFanOutTask| {
                let params = raw_params.clone();
                async move {
                    let raw = t
                        .pool
                        .send_host_raw_request_for_incarnation(
                            &t.server_name,
                            &t.server_config,
                            &HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                            },
                            METHOD,
                            params,
                            t.upstream_id,
                            incarnation,
                        )
                        .await?;
                    Ok(raw
                        .and_then(|raw| parse_host_verbatim::<Vec<InlineValue>>(raw.value))
                        .filter(|values| !values.is_empty()))
                }
            },
            |opt| matches!(opt, Some(values) if !values.is_empty()),
            cancel_rx,
        )
        .await;
        self.host_layer_result(fan_in, METHOD, |won| won).await
    }

    async fn inline_value_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        context: InlineValueContext,
        progress_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<InlineValue>>> {
        let Some(mut ctx) = self
            .resolve_bridge_contexts_for_range(lsp_uri, range, METHOD)
            .await
        else {
            return Ok(None);
        };
        ctx.document.client_progress_token = progress_token;
        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());
        let range = ctx.range;
        let result = dispatch_preferred(
            &ctx.document,
            self.bridge.pool_arc(),
            |t| {
                let context = context.clone();
                async move {
                    t.pool
                        .send_inline_value_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            range,
                            context,
                            t.region_end(),
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                            t.client_progress_token,
                        )
                        .await
                }
            },
            |opt| matches!(opt, Some(values) if !values.is_empty()),
            cancel_rx,
        )
        .await;

        result
            .handle(&self.notifier(), "inline value", None, Ok)
            .await
    }
}
