//! Color presentation method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Color, ColorPresentation, ColorPresentationParams, Range, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::HostDocument;
use crate::lsp::lsp_impl::bridge_context::{
    Sourced, Verdict, parse_host_verbatim, poll_verdict, sources_verdict,
};

const METHOD: &str = "textDocument/colorPresentation";

impl Kakehashi {
    pub(crate) async fn color_presentation_impl(
        &self,
        params: ColorPresentationParams,
    ) -> Result<Vec<ColorPresentation>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        // Before either layer reads the text: what a stand-in for an answer
        // kept aside must still be (see `PassedOver::resolve`).
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
        let pool = self.bridge.pool();
        let tracker = self.bridge.node_tracker();
        let current_version = || self.host_content_version(&lsp_uri);
        let passed_over = PassedOver::default();
        let race = self.walk_layers_by_strategy(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            // Under `preferred`, read as the race decides (see
            // `PassedOver::answers`).
            |answer: &ColorAnswer| passed_over.answers(answer, pool, tracker, current_version),
            ColorAnswer::combine,
        );
        // The final answer chosen: a prepared part's edits checked once
        // more against the host text, which may have moved while it was
        // held, once the host's last edit settles (a picker asks once per
        // interaction, not on every keystroke).
        passed_over
            .resolve(race, pool, tracker, current_version, || {
                // Read only once an answer was kept aside, which only a pool
                // that prepares keeps.
                revision.is_some() && self.prepared_request_revision(&lsp_uri) == revision
            })
            .await
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
    /// last edit settles (see `bridge_context::sources_verdict`).
    async fn settled_presentations(
        self,
        pool: &crate::lsp::bridge::LanguageServerPool,
        tracker: &crate::language::NodeTracker,
        current_version: impl Fn() -> Option<u64>,
    ) -> Vec<ColorPresentation> {
        let keeps = match &self.prepared {
            Some((source, _)) => {
                sources_verdict(pool, tracker, current_version, std::iter::once(source))
                    .await
                    .keeps()
            }
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

/// A prepared answer a `preferred` race passed over while its host's last
/// edit was still settling: a part the final check may keep or drop, so
/// the race goes on to the next layer's answer, which stands in for it
/// when it is dropped, instead of none. A current one wins at once.
#[derive(Default)]
struct PassedOver {
    kept: std::sync::Mutex<Option<ColorAnswer>>,
    /// Signalled as an answer is kept aside, so its settle is waited on
    /// while the race goes on.
    kept_aside: tokio::sync::Notify,
}

impl PassedOver {
    /// Whether `answer` wins the race: it has presentations and its
    /// prepared part, if any, is current. One that may still be kept once
    /// the host's last edit settles is kept aside instead: asked in
    /// priority order, never of a layer before the ones above it have
    /// come, the one kept outranks any answer the race goes on to choose.
    fn answers(
        &self,
        answer: &ColorAnswer,
        pool: &crate::lsp::bridge::LanguageServerPool,
        tracker: &crate::language::NodeTracker,
        current_version: impl Fn() -> Option<u64>,
    ) -> bool {
        // Current read first: a settle landing between the two reads keeps
        // aside an answer then current, which the final check keeps.
        if answer.has_result(|source| source.still_current(pool, tracker, &current_version)) {
            return true;
        }
        if answer.has_result(|source| prepared_may_keep(source, pool, tracker, &current_version)) {
            let mut kept = self
                .kept
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if kept.is_none() {
                *kept = Some(answer.clone());
                // A permit stored: the wait never misses it.
                self.kept_aside.notify_one();
            }
        }
        false
    }

    /// The presentations of the answer kept aside, judged as the host's
    /// last edit settles, while `race` goes on: kept any, they answer at
    /// once, the race's answers dropped unawaited; else those of the
    /// race's chosen answer (none, or a layer's error, which the answer
    /// kept aside outranked). A cancel the race returns first stays a
    /// cancel.
    ///
    /// Such a stand-in waited for the settle, and answers only while the
    /// host is still the text the request read (`unchanged`): neither
    /// edited nor closed and reopened meanwhile, as its edits were read for
    /// that text. Nothing kept aside, nothing is waited for, and the
    /// race's answer goes out as it came.
    async fn resolve(
        &self,
        race: impl Future<Output = Result<Option<ColorAnswer>>>,
        pool: &crate::lsp::bridge::LanguageServerPool,
        tracker: &crate::language::NodeTracker,
        current_version: impl Fn() -> Option<u64>,
        unchanged: impl Fn() -> bool,
    ) -> Result<Vec<ColorPresentation>> {
        let mut race = std::pin::pin!(race);
        let mut settled = std::pin::pin!(self.settled(pool, tracker, &current_version));
        let chosen = tokio::select! {
            chosen = &mut race => {
                let cancelled = matches!(
                    &chosen,
                    Err(error) if error.code == tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled
                );
                if !cancelled && self.has_kept() {
                    let presentations = settled.await;
                    if !presentations.is_empty() {
                        return Ok(presentations);
                    }
                }
                chosen
            }
            presentations = &mut settled => {
                if !presentations.is_empty() {
                    return Ok(presentations);
                }
                race.await
            }
        };
        match chosen? {
            Some(_) if self.has_kept() && !unchanged() => Ok(Vec::new()),
            Some(answer) => Ok(answer
                .settled_presentations(pool, tracker, &current_version)
                .await),
            None => Ok(Vec::new()),
        }
    }

    fn has_kept(&self) -> bool {
        self.kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// The presentations of the answer kept aside, judged once the host's
    /// last edit settles: pending until one is kept aside.
    async fn settled(
        &self,
        pool: &crate::lsp::bridge::LanguageServerPool,
        tracker: &crate::language::NodeTracker,
        current_version: impl Fn() -> Option<u64>,
    ) -> Vec<ColorPresentation> {
        self.kept_aside.notified().await;
        let kept = self
            .kept
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match kept {
            Some(kept) => {
                kept.settled_presentations(pool, tracker, current_version)
                    .await
            }
            None => Vec::new(),
        }
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
    matches!(
        poll_verdict(
            || source.still_current(pool, tracker, &current_version),
            || source.moved(current_version()),
            || source.settling(pool, tracker),
        ),
        None | Some(Verdict::Current)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race keeps a prepared part current or still settling, and
    /// passes over one moved or stale: only a part the final check may
    /// still keep outranks the host layer's answer.
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

    /// Under a cross-layer `preferred`, a prepared presentation still
    /// settling as the race decides outranks the host layer's, and is the
    /// answer when the settle keeps it; dropped then (re-prepared to other
    /// coordinates as it settled), the host layer's presentations answer,
    /// not none, unless the host changed meanwhile: they were read for the
    /// text it left. A current one, or one kept as it settles, answers
    /// without waiting on the host layer.
    #[tokio::test(start_paused = true)]
    async fn a_settling_presentation_dropped_at_settle_falls_back_to_the_host_layers() {
        use crate::config::settings::LayerSource::{Host, Virt};
        use crate::lsp::bridge::{VirtualLayout, apply_prepare_result};
        use crate::lsp::lsp_impl::bridge_context::race_layers_preferred;
        use std::sync::Arc;
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
        let presentation = |label: &str| ColorPresentation {
            label: label.to_string(),
            text_edit: None,
            additional_text_edits: None,
        };
        for case in [
            "current",
            "settling, kept",
            "settling, kept, host hung",
            "settling, dropped",
            "settling, dropped, host reopened",
        ] {
            let prepared = prepare("  red\n");
            let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::preparing());
            let host = url::Url::parse("file:///test/color-fallback.md").unwrap();
            let tracker = crate::language::NodeTracker::new();
            tracker.open_incarnation(&host, 1);
            let region = tracker
                .get_or_create_for_incarnation(&host, 0, 6, "block", 1)
                .unwrap()
                .to_string();
            pool.open_host_incarnation(&host, 1).await;
            pool.record_latest_prepared_for_test(&host, 1, "css", &region, &prepared);
            if case != "current" {
                pool.note_host_edited(&host, 1, 1);
                let (pool, host, region) = (Arc::clone(&pool), host.clone(), region.clone());
                let dropped = case.contains("dropped");
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if dropped {
                        let other = prepare("  blue\n");
                        pool.record_latest_prepared_for_test(&host, 1, "css", &region, &other);
                    }
                    pool.note_host_settled(&host, 1, 1);
                });
            }
            let virt = ColorAnswer::from_layer(Sourced::prepared_for_test(
                vec![presentation("virt")],
                &host,
                "css",
                &region,
                &prepared.map,
                1,
                1,
            ));
            let host_answer = async {
                if case == "current" || case.ends_with("host hung") {
                    // A host layer hung until the request would time out:
                    // a current answer, or one kept once it settles, never
                    // waits on it.
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Ok(Some(ColorAnswer::unprepared(vec![presentation("host")])))
            };
            let passed_over = PassedOver::default();
            let started = tokio::time::Instant::now();
            let race = race_layers_preferred(
                &[Virt, Host],
                std::future::ready(Ok(Some(virt))),
                host_answer,
                std::future::ready(Ok(None)),
                |answer: &ColorAnswer| passed_over.answers(answer, &pool, &tracker, || Some(1)),
            );
            let labels = passed_over
                .resolve(
                    race,
                    &pool,
                    &tracker,
                    || Some(1),
                    || case != "settling, dropped, host reopened",
                )
                .await
                .unwrap()
                .into_iter()
                .map(|presentation| presentation.label)
                .collect::<Vec<_>>();
            let expected: &[&str] = match case {
                // Read for the text the host left while the prepared answer
                // settled: no stand-in.
                "settling, dropped, host reopened" => &[],
                "settling, dropped" => &["host"],
                _ => &["virt"],
            };
            assert_eq!(labels, expected, "{case}");
            match case {
                "current" => assert_eq!(started.elapsed(), std::time::Duration::ZERO, "{case}"),
                // Settled at 500ms, judged as the settle is seen.
                "settling, kept, host hung" => assert!(
                    started.elapsed() < std::time::Duration::from_secs(1),
                    "{case}: answered as it settled, not after {:?}",
                    started.elapsed()
                ),
                _ => {}
            }
        }
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
