//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Color, ColorPresentation, ColorPresentationParams, Range, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/colorPresentation";

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        // Experimental (KAKEHASHI_EXPERIMENTAL=true): without the opt-in the
        // capability is not advertised, so answer a compliant empty result to
        // any client that calls regardless.
        if !self.experimental_enabled() {
            return Ok(Vec::new());
        }
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        let virt = self.color_presentation_virt_layer(&lsp_uri, params.range, params.color);
        let host = self.color_presentation_host_layer(&lsp_uri, raw_params);
        self.walk_layers_by_strategy(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |presentations: &Vec<ColorPresentation>| !presentations.is_empty(),
            |mut acc, mut next| {
                acc.append(&mut next);
                acc
            },
        )
        .await
        .map(Option::unwrap_or_default)
    }

    async fn color_presentation_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        color: Color,
    ) -> Result<Option<Vec<ColorPresentation>>> {
        let Some(ctx) = self
            .resolve_bridge_contexts_for_range(lsp_uri, range, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out color presentation requests to all matching servers
        let pool = self.bridge.pool_arc();
        // The virtual arm owns its downstream registry entries. Clean them on
        // every exit, including a dropped layer future (dispatch_preferred
        // aborts losers without joining).
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            pool.clone(),
            ctx.document.upstream_request_id.clone(),
        );
        let range = ctx.range;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_color_presentation_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        range,
                        t.region_end(),
                        color,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
            |presentations| !presentations.is_empty(),
            cancel_rx,
        )
        .await;

        result
            .handle(&self.notifier(), "color presentation", None, |items| {
                Ok((!items.is_empty()).then_some(items))
            })
            .await
    }

    async fn color_presentation_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<ColorPresentation>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let incarnation = ctx.incarnation;
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let fan_in = dispatch_host_preferred(
            &ctx,
            pool,
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
                    let Some(raw) = raw else {
                        return Ok(None);
                    };
                    Ok(parse_host_verbatim::<Vec<ColorPresentation>>(raw.value)
                        .filter(|items| !items.is_empty()))
                }
            },
            |opt| matches!(opt, Some(items) if !items.is_empty()),
            cancel_rx,
        )
        .await;
        self.host_layer_result(fan_in, METHOD, |won| won).await
    }
}
