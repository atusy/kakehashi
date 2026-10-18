//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Color, ColorPresentation, ColorPresentationParams, Range, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::lsp_impl::bridge_context::{Sourced, parse_host_verbatim};

const METHOD: &str = "textDocument/colorPresentation";

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        // Each layer's presentations kept apart, in order, so a stale
        // prepared part can be dropped without the others.
        let virt = async {
            let presentations = self
                .color_presentation_virt_layer(&lsp_uri, params.range, params.color)
                .await?;
            Ok(presentations.map(|presentations| vec![presentations]))
        };
        let host = async {
            let presentations = self
                .color_presentation_host_layer(&lsp_uri, raw_params)
                .await?;
            Ok(presentations.map(|presentations| vec![Sourced::unprepared(presentations)]))
        };
        let parts = self
            .walk_layers_by_strategy(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                |parts: &Vec<Sourced<Vec<ColorPresentation>>>| {
                    parts.iter().any(|part| !part.value.is_empty())
                },
                |mut acc, mut next| {
                    acc.append(&mut next);
                    acc
                },
            )
            .await?;
        // The final answer chosen: each part's edits checked once more
        // against the host text, which may have moved while it was held.
        let pool = self.bridge.pool();
        Ok(color_presentations_still_current(
            parts.unwrap_or_default(),
            |part| part.still_current(pool),
        ))
    }

    async fn color_presentation_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        color: Color,
    ) -> Result<Option<Sourced<Vec<ColorPresentation>>>> {
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

        let presentations = result
            .handle(&self.notifier(), "color presentation", None, |items| {
                Ok((!items.is_empty()).then_some(items))
            })
            .await?;
        Ok(presentations.map(|items| Sourced::from_region(&ctx.document, items)))
    }

    async fn color_presentation_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<ColorPresentation>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let revision = crate::lsp::bridge::HostRevision {
            incarnation: ctx.incarnation,
            content_version: ctx.content_version,
        };
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
                        .send_host_raw_request(
                            &t.server_name,
                            &t.server_config,
                            &HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                                revision: Some(revision),
                            },
                            METHOD,
                            params,
                            t.upstream_id,
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
        self.host_layer_result(fan_in, &ctx, METHOD, |won| won)
            .await
    }
}

/// The chosen presentations, each layer's part kept only while its edits
/// still land where they were mapped (`current`): a stale part answers
/// nothing, the others stay.
fn color_presentations_still_current(
    parts: Vec<Sourced<Vec<ColorPresentation>>>,
    current: impl Fn(&Sourced<Vec<ColorPresentation>>) -> bool,
) -> Vec<ColorPresentation> {
    parts
        .into_iter()
        .filter(|part| current(part))
        .flat_map(|part| part.value)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_part_answers_nothing_and_the_others_stay() {
        let presentation = |label: &str| ColorPresentation {
            label: label.to_string(),
            text_edit: None,
            additional_text_edits: None,
        };
        let parts = || {
            vec![
                Sourced::unprepared(vec![presentation("host")]),
                Sourced::unprepared(vec![presentation("virt")]),
            ]
        };
        let labels = |presentations: Vec<ColorPresentation>| {
            presentations
                .into_iter()
                .map(|presentation| presentation.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(color_presentations_still_current(parts(), |_| true)),
            ["host", "virt"]
        );
        assert_eq!(
            labels(color_presentations_still_current(parts(), |part| {
                part.value[0].label != "virt"
            })),
            ["host"]
        );
    }
}
