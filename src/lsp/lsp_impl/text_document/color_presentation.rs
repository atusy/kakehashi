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
        let virt = async {
            let presentations = self
                .color_presentation_virt_layer(&lsp_uri, params.range, params.color)
                .await?;
            Ok(presentations.map(ColorAnswer::from_layer))
        };
        let host = async {
            let presentations = self
                .color_presentation_host_layer(&lsp_uri, raw_params)
                .await?;
            Ok(presentations.map(ColorAnswer::unprepared))
        };
        let answer = self
            .walk_layers_by_strategy(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                |answer: &ColorAnswer| !answer.items.is_empty(),
                ColorAnswer::combine,
            )
            .await?;
        // The final answer chosen: a prepared part's edits checked once
        // more against the host text, which may have moved while it was held.
        let pool = self.bridge.pool();
        Ok(answer.map_or_else(Vec::new, |answer| {
            answer.still_current(|source| source.still_current(pool))
        }))
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

/// The presentations as the layers gave them, in order, with the range of
/// those mapped through a prepared map, if any (the virt layer's, at most
/// one): what a stale check drops alone. Without a prepared part it is the
/// plain vector, combined by appending as ever.
struct ColorAnswer {
    items: Vec<ColorPresentation>,
    prepared: Option<(Sourced<()>, std::ops::Range<usize>)>,
}

impl ColorAnswer {
    fn unprepared(items: Vec<ColorPresentation>) -> Self {
        Self {
            items,
            prepared: None,
        }
    }

    fn from_layer(sourced: Sourced<Vec<ColorPresentation>>) -> Self {
        if !sourced.is_prepared() {
            return Self::unprepared(sourced.value);
        }
        let (items, source) = sourced.into_parts();
        let range = 0..items.len();
        Self {
            items,
            prepared: Some((source, range)),
        }
    }

    fn combine(mut self, mut next: Self) -> Self {
        let shift = self.items.len();
        self.items.append(&mut next.items);
        self.prepared = self.prepared.or(next
            .prepared
            .map(|(source, range)| (source, range.start + shift..range.end + shift)));
        self
    }

    /// The presentations, the prepared part kept only while its edits still
    /// land where they were mapped (`current`): a stale part answers
    /// nothing, the others stay.
    fn still_current(mut self, current: impl Fn(&Sourced<()>) -> bool) -> Vec<ColorPresentation> {
        if let Some((source, range)) = self.prepared
            && !current(&source)
        {
            self.items.drain(range);
        }
        self.items
    }
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
        let host = url::Url::parse("file:///test/color.md").unwrap();
        let map = crate::lsp::bridge::apply_prepare_result(
            "red",
            &crate::lsp::bridge::VirtualLayout::single("red"),
            None,
        )
        .unwrap()
        .map;
        let answer = || {
            ColorAnswer::unprepared(vec![presentation("host")]).combine(ColorAnswer::from_layer(
                Sourced::prepared_for_test(
                    vec![presentation("virt"), presentation("virt2")],
                    &host,
                    "css",
                    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    &map,
                ),
            ))
        };
        let labels = |presentations: Vec<ColorPresentation>| {
            presentations
                .into_iter()
                .map(|presentation| presentation.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(answer().still_current(|_| true)),
            ["host", "virt", "virt2"]
        );
        assert_eq!(labels(answer().still_current(|_| false)), ["host"]);
        // Without a prepared part nothing is read.
        assert_eq!(
            labels(
                ColorAnswer::unprepared(vec![presentation("host")])
                    .still_current(|_| unreachable!())
            ),
            ["host"]
        );
    }
}
