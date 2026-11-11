//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Color, ColorPresentation, ColorPresentationParams, Range, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::lsp_impl::bridge_context::{Sourced, Verdict, parse_host_verbatim};

const METHOD: &str = "textDocument/colorPresentation";

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        // Before either layer reads the host's text (see `PreparedRace`).
        let race = self.prepared_race(&lsp_uri);
        let virt = async {
            let presentations = self
                .color_presentation_virt_layer(
                    &lsp_uri,
                    params.range,
                    params.color,
                    race.revision(),
                )
                .await?;
            Ok(presentations.map(ColorAnswer::from_layer))
        };
        let host = async {
            let presentations = self
                .color_presentation_host_layer(&lsp_uri, raw_params)
                .await?;
            Ok(presentations.map(ColorAnswer::unprepared))
        };
        let chosen = self
            .walk_layers_by_strategy(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                // Under `preferred`, read as the race decides: a prepared
                // answer no longer current is no answer of its layer.
                |answer: &ColorAnswer| {
                    answer.has_result(|source| race.keeps(std::iter::once(source)))
                },
                ColorAnswer::combine,
            )
            .await;
        // The final answer chosen, by the rule every prepared answer
        // follows (see `PreparedRace`): a prepared part's edits checked once
        // more against the host, which may have moved while it was held;
        // the parts left, or another layer's standing in for one passed
        // over, answering only while the host is still the text the request
        // read; and what is dropped with nothing left `ContentModified`.
        let chosen = match chosen {
            Ok(chosen) => chosen,
            Err(error)
                if race.passed_over().is_none()
                    || error.code == tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled =>
            {
                return Err(error);
            }
            Err(_) => None,
        };
        let judged = match chosen {
            Some(answer) => answer.final_presentations(
                || self.prepared_request_revision(&lsp_uri),
                race.passed_over(),
                || race.unchanged(),
            ),
            None => race.passed_over().map_or(Ok(Vec::new()), Err),
        };
        judged.map_err(|verdict| {
            crate::lsp::lsp_impl::bridge_context::dropped_answer_error(
                &lsp_uri,
                "colorPresentation",
                verdict,
                log::Level::Debug,
            )
        })
    }

    async fn color_presentation_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        color: Color,
        revision: Option<crate::lsp::bridge::HostRevision>,
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
    /// as it is `now` (see `Sourced::verdict`), or the verdict that drops
    /// the whole answer:
    ///
    /// - The prepared part dropped, the parts left (the host layer's, under
    ///   `concatenated`) were read for the text the request read: they
    ///   answer only while the host is still that text (`unchanged`). So
    ///   does an answer standing in for a prepared one a `preferred` race
    ///   passed over (`passed_over`).
    /// - Dropped with nothing left, it is dropped by the part's verdict.
    fn final_presentations(
        self,
        now: impl FnOnce() -> Option<crate::lsp::bridge::HostRevision>,
        passed_over: Option<Verdict>,
        unchanged: impl Fn() -> bool,
    ) -> std::result::Result<Vec<ColorPresentation>, Verdict> {
        let verdict = self
            .prepared
            .as_ref()
            .map_or(Verdict::Current, |(source, _)| source.verdict(now()));
        let presentations = self.still_current(|_| verdict.keeps());
        let dropped = Some(verdict).filter(|verdict| !verdict.keeps());
        match dropped.or(passed_over) {
            None => Ok(presentations),
            Some(verdict) if presentations.is_empty() => Err(verdict),
            Some(_) if !unchanged() => Err(Verdict::Moved),
            Some(_) => Ok(presentations),
        }
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
            answer()
                .final_presentations(|| at(1), None, || unreachable!())
                .map(labels),
            Ok(vec!["virt".to_string()])
        );
        assert_eq!(
            answer().final_presentations(|| at(2), None, || false),
            Err(Verdict::Moved)
        );
    }

    /// Under `preferred`, the host layer's answer chosen after a prepared
    /// one was passed over stands in for it only while the host is still
    /// the text the request read.
    #[test]
    fn a_host_answer_stands_in_for_one_passed_over_only_on_unchanged_text() {
        let host = || ColorAnswer::unprepared(vec![presentation("host")]);
        assert_eq!(
            host()
                .final_presentations(|| unreachable!(), Some(Verdict::Stale), || true)
                .map(labels),
            Ok(vec!["host".to_string()])
        );
        // The host moved: the host layer's was read for the text it moved
        // from too.
        assert_eq!(
            host().final_presentations(|| unreachable!(), Some(Verdict::Moved), || false),
            Err(Verdict::Moved)
        );
        // Nothing passed over, the host layer's answer goes out as it came.
        assert_eq!(
            host()
                .final_presentations(|| unreachable!(), None, || unreachable!())
                .map(labels),
            Ok(vec!["host".to_string()])
        );
    }

    /// Under `concatenated`, a dropped prepared part leaves the host
    /// layer's, which answers only while the host is still the text the
    /// request read; nothing left, the answer is dropped by the part's
    /// verdict.
    #[test]
    fn a_merged_host_part_outlives_a_dropped_prepared_part_only_on_unchanged_text() {
        let (map, revocation) = revocable_map();
        let virt = || {
            ColorAnswer::from_layer(Sourced::prepared_for_test(
                vec![presentation("virt")],
                &map,
                1,
                1,
            ))
        };
        let merged = || ColorAnswer::unprepared(vec![presentation("host")]).combine(virt());
        assert_eq!(
            merged()
                .final_presentations(|| at(1), None, || unreachable!())
                .map(labels),
            Ok(vec!["host".to_string(), "virt".to_string()])
        );
        // Moved: every part was read for the text the host moved from.
        assert_eq!(
            merged().final_presentations(|| at(2), None, || false),
            Err(Verdict::Moved)
        );
        // Revoked, the host unmoved: the prepared part alone goes.
        revocation.revoke(u64::MAX);
        assert_eq!(
            merged()
                .final_presentations(|| at(1), None, || true)
                .map(labels),
            Ok(vec!["host".to_string()])
        );
        assert_eq!(
            virt().final_presentations(|| at(1), None, || unreachable!()),
            Err(Verdict::Stale)
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
