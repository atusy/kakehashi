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
        // Before either layer reads the text: what a stand-in for an answer
        // passed over must still be (see `ColorAnswer::final_presentations`).
        let revision = self.prepared_request_revision(&lsp_uri);
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
        let passed_over = PassedOver::default();
        let current = |source: &Sourced<()>| self.sourced_still_current(source, &lsp_uri);
        let chosen = self
            .walk_layers_by_strategy(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                // Under `preferred`, read as the race decides (see
                // `PassedOver::answers`).
                |answer: &ColorAnswer| passed_over.answers(answer, current),
                ColorAnswer::combine,
            )
            .await?;
        // The final answer chosen: a prepared part's edits checked once
        // more against the host, which may have moved while it was held.
        let Some(answer) = chosen else {
            return Ok(Vec::new());
        };
        Ok(answer.final_presentations(
            || self.prepared_request_revision(&lsp_uri),
            passed_over.passed(),
            // Read only once a prepared answer was passed over or dropped,
            // which only a pool that prepares has.
            || revision.is_some() && self.prepared_request_revision(&lsp_uri) == revision,
        ))
    }

    async fn color_presentation_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        color: Color,
    ) -> Result<Option<Sourced<Vec<ColorPresentation>>>> {
        // Before the text is read: see `Sourced::from_region`.
        let revision = self.prepared_request_revision(lsp_uri);
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
        Ok(presentations.map(|items| Sourced::from_region(&ctx.document, items, revision)))
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
#[derive(Clone)]
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

    /// Whether the answer is one a `preferred` race may choose: it has
    /// presentations, and a prepared part is still `current`: its edits
    /// still land where they were mapped.
    fn has_result(&self, current: impl Fn(&Sourced<()>) -> bool) -> bool {
        !self.items.is_empty()
            && self
                .prepared
                .as_ref()
                .is_none_or(|(source, _)| current(source))
    }

    fn combine(mut self, mut next: Self) -> Self {
        let shift = self.items.len();
        self.items.append(&mut next.items);
        self.prepared = self.prepared.or(next
            .prepared
            .map(|(source, range)| (source, range.start + shift..range.end + shift)));
        self
    }

    /// [`Self::still_current`], the prepared part judged against the host
    /// as it is `now` (see `Sourced::verdict`). Dropped, the parts left
    /// (the host layer's, under `concatenated`) were read for the text the
    /// request read: they answer only while the host is still that text
    /// (`unchanged`). So does an answer standing in for a prepared one a
    /// `preferred` race passed over (`stands_in`).
    fn final_presentations(
        self,
        now: impl FnOnce() -> Option<crate::lsp::bridge::HostRevision>,
        stands_in: bool,
        unchanged: impl Fn() -> bool,
    ) -> Vec<ColorPresentation> {
        let keeps = self
            .prepared
            .as_ref()
            .is_none_or(|(source, _)| source.still_current(now));
        let presentations = self.still_current(|_| keeps);
        if (!keeps || stands_in) && !presentations.is_empty() && !unchanged() {
            return Vec::new();
        }
        presentations
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

/// Whether a `preferred` race passed over a prepared answer no longer
/// current: the answer it then chose stands in for that one, and answers
/// only while the host is still the text the request read.
#[derive(Default)]
struct PassedOver(std::sync::atomic::AtomicBool);

impl PassedOver {
    /// Whether `answer` wins the race: it has presentations and its
    /// prepared part, if any, is still `current`. One that is not is no
    /// answer of its layer, and is noted as passed over.
    fn answers(&self, answer: &ColorAnswer, current: impl Fn(&Sourced<()>) -> bool) -> bool {
        if answer.has_result(current) {
            return true;
        }
        if !answer.items.is_empty() {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        false
    }

    fn passed(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::HostRevision;

    fn presentation(label: &str) -> ColorPresentation {
        ColorPresentation {
            label: label.to_string(),
            text_edit: None,
            additional_text_edits: None,
        }
    }

    fn labels(presentations: Vec<ColorPresentation>) -> Vec<String> {
        presentations
            .into_iter()
            .map(|presentation| presentation.label)
            .collect()
    }

    /// A map whose text its peer can fail, and the handle that revokes it.
    fn revocable_map() -> (
        std::sync::Arc<crate::lsp::bridge::PreparedMap>,
        std::sync::Arc<crate::lsp::bridge::Revocation>,
    ) {
        let mut prepared = crate::lsp::bridge::apply_prepare_result(
            "red",
            &crate::lsp::bridge::VirtualLayout::single("red"),
            None,
        )
        .unwrap();
        let revocation = std::sync::Arc::new(crate::lsp::bridge::Revocation::default());
        std::sync::Arc::get_mut(&mut prepared.map)
            .unwrap()
            .set_revocation(std::sync::Arc::clone(&revocation));
        (prepared.map, revocation)
    }

    fn at(content_version: u64) -> Option<HostRevision> {
        Some(HostRevision {
            incarnation: 1,
            content_version,
        })
    }

    /// A presentation is judged at once by the revision its request read:
    /// kept while the host is that revision, whatever the lifecycle pass is
    /// doing meanwhile (a picker asks once per interaction, so one answered
    /// right after an edit to another fence must not be lost), dropped once
    /// the host moved.
    #[test]
    fn a_presentation_is_kept_while_the_host_is_the_revision_its_request_read() {
        let (map, _) = revocable_map();
        let answer = || {
            ColorAnswer::from_layer(Sourced::prepared_for_test(
                vec![presentation("virt")],
                &map,
                1,
                1,
            ))
        };
        assert_eq!(
            labels(answer().final_presentations(|| at(1), false, || unreachable!())),
            ["virt"]
        );
        assert!(
            answer()
                .final_presentations(|| at(2), false, || false)
                .is_empty()
        );
    }

    /// Under `preferred`, a prepared answer no longer current is no answer
    /// of its layer: the race goes on to the host layer's, which stands in
    /// for it only while the host is still the text the request read.
    #[test]
    fn a_host_answer_stands_in_for_one_passed_over_only_on_unchanged_text() {
        let (map, revocation) = revocable_map();
        let virt = ColorAnswer::from_layer(Sourced::prepared_for_test(
            vec![presentation("virt")],
            &map,
            1,
            1,
        ));
        let host = || ColorAnswer::unprepared(vec![presentation("host")]);
        // Current: it wins, nothing passed over.
        let passed_over = PassedOver::default();
        assert!(passed_over.answers(&virt, |source| source.still_current(|| at(1))));
        assert!(!passed_over.passed());
        // Its map revoked, the host unmoved: the host layer's stands in.
        revocation.revoke(u64::MAX);
        assert!(!passed_over.answers(&virt, |source| source.still_current(|| at(1))));
        assert!(passed_over.passed());
        assert!(passed_over.answers(&host(), |_| unreachable!()));
        assert_eq!(
            labels(host().final_presentations(|| unreachable!(), true, || true)),
            ["host"]
        );
        // The host moved: the host layer's was read for the text it moved
        // from too.
        assert!(
            host()
                .final_presentations(|| unreachable!(), true, || false)
                .is_empty()
        );
        // Nothing passed over, the host layer's answer goes out as it came.
        assert_eq!(
            labels(host().final_presentations(|| unreachable!(), false, || unreachable!())),
            ["host"]
        );
    }

    /// Under `concatenated`, a dropped prepared part leaves the host
    /// layer's, which answers only while the host is still the text the
    /// request read.
    #[test]
    fn a_merged_host_part_outlives_a_dropped_prepared_part_only_on_unchanged_text() {
        let (map, revocation) = revocable_map();
        let merged = || {
            ColorAnswer::unprepared(vec![presentation("host")]).combine(ColorAnswer::from_layer(
                Sourced::prepared_for_test(vec![presentation("virt")], &map, 1, 1),
            ))
        };
        assert_eq!(
            labels(merged().final_presentations(|| at(1), false, || unreachable!())),
            ["host", "virt"]
        );
        // Moved: every part was read for the text the host moved from.
        assert!(
            merged()
                .final_presentations(|| at(2), false, || false)
                .is_empty()
        );
        // Revoked, the host unmoved: the prepared part alone goes.
        revocation.revoke(u64::MAX);
        assert_eq!(
            labels(merged().final_presentations(|| at(1), false, || true)),
            ["host"]
        );
    }

    #[test]
    fn a_stale_part_answers_nothing_and_the_others_stay() {
        let (map, _) = revocable_map();
        let answer = || {
            ColorAnswer::unprepared(vec![presentation("host")]).combine(ColorAnswer::from_layer(
                Sourced::prepared_for_test(
                    vec![presentation("virt"), presentation("virt2")],
                    &map,
                    1,
                    0,
                ),
            ))
        };
        assert_eq!(
            labels(answer().still_current(|_| true)),
            ["host", "virt", "virt2"]
        );
        assert_eq!(labels(answer().still_current(|_| false)), ["host"]);
        // Under `preferred` a layer's answer is its own part: a stale
        // prepared one is no answer, which the race falls through.
        let virt_only = || {
            ColorAnswer::from_layer(Sourced::prepared_for_test(
                vec![presentation("virt")],
                &map,
                1,
                0,
            ))
        };
        assert!(virt_only().has_result(|_| true));
        assert!(!virt_only().has_result(|_| false));
        assert!(ColorAnswer::unprepared(vec![presentation("host")]).has_result(|_| unreachable!()));
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
