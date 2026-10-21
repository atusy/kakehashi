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
        let pool = self.bridge.pool();
        let tracker = self.bridge.node_tracker();
        let may_keep = |source: &Sourced<()>| {
            prepared_may_keep(source, pool, tracker, || {
                self.host_content_version(&lsp_uri)
            })
        };
        let answer = self
            .walk_layers_by_strategy(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                // Under `preferred`, read as the race decides: a prepared
                // answer moved or stale is no answer of its layer, and the
                // race falls through to the next layer's; one still settling
                // wins, and the final check waits on it.
                |answer: &ColorAnswer| answer.has_result(may_keep),
                ColorAnswer::combine,
            )
            .await?;
        // The final answer chosen: a prepared part's edits checked once
        // more against the host text, which may have moved while it was
        // held, once the host's last edit settles (a picker asks once per
        // interaction, not on every keystroke).
        let Some(answer) = answer else {
            return Ok(Vec::new());
        };
        Ok(answer
            .settled_presentations(pool, tracker, || self.host_content_version(&lsp_uri))
            .await)
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
        Ok(presentations.map(|items| {
            Sourced::from_region(&ctx.document, items, self.bridge.node_tracker(), revision)
        }))
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

    /// Whether the answer is one a `preferred` race may choose: it has
    /// presentations, and a prepared part may still be kept (`may_keep`):
    /// its edits still land where they were mapped, or may once the host's
    /// last edit settles.
    fn has_result(&self, may_keep: impl Fn(&Sourced<()>) -> bool) -> bool {
        !self.items.is_empty()
            && self
                .prepared
                .as_ref()
                .is_none_or(|(source, _)| may_keep(source))
    }

    fn combine(mut self, mut next: Self) -> Self {
        let shift = self.items.len();
        self.items.append(&mut next.items);
        self.prepared = self.prepared.or(next
            .prepared
            .map(|(source, range)| (source, range.start + shift..range.end + shift)));
        self
    }

    /// [`Self::still_current`], the prepared part judged once the host's
    /// last edit settles (see `formatting::sources_verdict`).
    async fn settled_presentations(
        self,
        pool: &crate::lsp::bridge::LanguageServerPool,
        tracker: &crate::language::NodeTracker,
        current_version: impl Fn() -> Option<u64>,
    ) -> Vec<ColorPresentation> {
        let keeps = match &self.prepared {
            Some((source, _)) => super::formatting::sources_verdict(
                pool,
                tracker,
                current_version,
                std::iter::once(source),
            )
            .await
            .keeps(),
            None => true,
        };
        self.still_current(|_| keeps)
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

/// Whether a prepared part may still be kept, read at once for the
/// layers' race: current now, or its host's last edit still settling and
/// the host unedited since the request read its text. `false` once moved
/// or stale, which no wait brings back; the final check still judges the
/// answer chosen.
fn prepared_may_keep(
    source: &Sourced<()>,
    pool: &crate::lsp::bridge::LanguageServerPool,
    tracker: &crate::language::NodeTracker,
    current_version: impl Fn() -> Option<u64>,
) -> bool {
    // Read before `current`: a settle landing between the two reads then
    // shows as current, never as stale.
    let settling = source.settling(pool);
    if source.still_current(pool, tracker, &current_version) {
        return true;
    }
    let moved = current_version().is_some_and(|now| {
        source
            .request_content_version()
            .is_some_and(|version| version != now)
    });
    settling && !moved
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race keeps a prepared part current or still settling, and
    /// passes over one moved or stale: only a part the final check may
    /// still keep wins over the host layer's answer.
    #[tokio::test]
    async fn a_prepared_part_may_be_kept_while_current_or_settling_only() {
        use crate::lsp::bridge::{VirtualLayout, apply_prepare_result};
        let prepare = |text: &str| {
            let answer = serde_json::from_value(serde_json::json!({"segments": [{
                "type": "content", "changes": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
                    "newText": ""
                }]
            }]}))
            .unwrap();
            apply_prepare_result(text, &VirtualLayout::single(text), Some(answer)).unwrap()
        };
        let prepared = prepare("  red\n");
        for case in ["current", "settling", "settling, moved", "moved", "stale"] {
            let pool = crate::lsp::bridge::LanguageServerPool::preparing();
            let host = url::Url::parse("file:///test/color-keep.md").unwrap();
            let tracker = crate::language::NodeTracker::new();
            tracker.open_incarnation(&host, 1);
            let region = tracker
                .get_or_create_for_incarnation(&host, 0, 6, "block", 1)
                .unwrap()
                .to_string();
            pool.open_host_incarnation(&host, 1).await;
            pool.record_latest_prepared_for_test(&host, 1, "css", &region, &prepared);
            if case.starts_with("settling") {
                pool.note_host_edited(&host, 1, 1);
            }
            if case == "stale" {
                // Re-prepared to other coordinates, the host unedited.
                let other = prepare("  blue\n");
                pool.record_latest_prepared_for_test(&host, 1, "css", &region, &other);
            }
            let version = if case.ends_with("moved") { 2 } else { 1 };
            let source = Sourced::prepared_for_test((), &host, "css", &region, &prepared.map, 1, 1);
            assert_eq!(
                prepared_may_keep(&source, &pool, &tracker, || Some(version)),
                matches!(case, "current" | "settling"),
                "{case}"
            );
        }
    }

    /// A presentation whose reply came before the host's last edit settled
    /// (another fence edited, this region's text unchanged) is kept once
    /// the edit settles, not dropped: a picker asks once per interaction.
    #[tokio::test(start_paused = true)]
    async fn a_presentation_is_kept_once_the_hosts_last_edit_settles() {
        use crate::lsp::bridge::{VirtualLayout, apply_prepare_result};
        use std::sync::Arc;
        let answer = serde_json::from_value(serde_json::json!({"segments": [{
            "type": "content", "changes": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
                "newText": ""
            }]
        }]}))
        .unwrap();
        let prepared =
            apply_prepare_result("  red\n", &VirtualLayout::single("  red\n"), Some(answer))
                .unwrap();
        let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::preparing());
        let host = url::Url::parse("file:///test/color-settle.md").unwrap();
        let tracker = crate::language::NodeTracker::new();
        tracker.open_incarnation(&host, 1);
        let region = tracker
            .get_or_create_for_incarnation(&host, 0, 6, "block", 1)
            .unwrap()
            .to_string();
        pool.open_host_incarnation(&host, 1).await;
        pool.record_latest_prepared_for_test(&host, 1, "css", &region, &prepared);
        pool.note_host_edited(&host, 1, 1);
        let presentation = ColorPresentation {
            label: "red".to_string(),
            text_edit: None,
            additional_text_edits: None,
        };
        let answer = ColorAnswer::from_layer(Sourced::prepared_for_test(
            vec![presentation.clone()],
            &host,
            "css",
            &region,
            &prepared.map,
            1,
            1,
        ));
        {
            let (pool, host) = (Arc::clone(&pool), host.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                pool.note_host_settled(&host, 1, 1);
            });
        }
        assert_eq!(
            answer
                .settled_presentations(&pool, &tracker, || Some(1))
                .await,
            vec![presentation]
        );
    }

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
                    1,
                    0,
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
        // Under `preferred` a layer's answer is its own part: a stale
        // prepared one is no answer, which the race falls through.
        let virt_only = || {
            ColorAnswer::from_layer(Sourced::prepared_for_test(
                vec![presentation("virt")],
                &host,
                "css",
                "01ARZ3NDEKTSV4RRFFQ69G5FAV",
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
