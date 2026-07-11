//! Code action method for Kakehashi (#568). Edit-carrying and lazy actions
//! are both bridged; `code_action_resolve_impl` routes `codeAction/resolve`
//! back to the origin server via the `data` envelope.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the requested range, the host layer
//! (host-document-bridge) bridges the host document itself. Both apply the
//! `"{title} — {server}"` suffix, so the host arm cannot use the generic
//! verbatim raw-value walk — it dispatches typed per server to keep the
//! server name ([`Kakehashi::walk_layers_by_strategy`], the strategy-aware walk
//! codeAction uses instead of the generic `walk_layer_futures`).
//!
//! codeAction defaults to `concatenated` at both aggregation levels (#568 PR 7):
//! within a layer, every server's actions are merged (`concat_merge`); across
//! layers, [`Kakehashi::walk_layers_by_strategy`] merges virt+host+native in
//! priority order (the native layer is wired but contributes nothing yet — it
//! resolves to `None`). Whichever cross-layer strategy applies, the final menu has
//! its cross-source `isPreferred` collision collapsed once
//! ([`resolve_preferred_collision`]).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionContext, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    MessageType, NumberOrString, Position, Range, Uri,
};
use ulid::Ulid;
use url::Url;

use super::super::bridge_context::RangeRequestContext;
use super::super::{Kakehashi, detect_document_language};
use crate::config::settings::AggregationStrategy;
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, HostFanOutTask, dispatch_concatenated, dispatch_host_concatenated,
    dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{
    CodeActionEnvelope, HostDocument, RegionOffset, UpstreamCodeActionCaps, bridge_code_actions,
    extract_code_action_envelope, parse_code_actions_leniently,
};
use crate::text::PositionMapper;

const METHOD: &str = "textDocument/codeAction";

/// Why [`Kakehashi::code_action_region_end_if_fresh`] produced no region end,
/// so the caller logs staleness once and skips it for already-warned
/// malformed envelopes.
enum RegionEndUnavailable {
    MalformedEnvelope,
    Stale,
}

/// Flatten the per-server results of a `concatenated` dispatch into one response
/// in priority order (`dispatch_concatenated` yields them ordered). Each server
/// contributes `Option<CodeActionResponse>`; drop the `None`s, concatenate the
/// action lists, and collapse an all-empty merge to `None` so the layer counts
/// as "no result" for cross-layer aggregation (#568 PR 7).
fn concat_merge(vecs: Vec<Option<CodeActionResponse>>) -> Option<CodeActionResponse> {
    let merged: CodeActionResponse = vecs.into_iter().flatten().flatten().collect();
    (!merged.is_empty()).then_some(merged)
}

/// Collapse the cross-server `isPreferred` collision on a concatenated menu:
/// the client's auto-fix keybinding picks the FIRST `isPreferred: true`, so once
/// several servers each mark their action preferred, keep only the first (the
/// merge is in priority order, so that's the highest-priority server) and drop
/// the rest to `false` (checklist §4). Per-action `isPreferredSupport` stripping
/// already happened in `bridge_code_action`; this is the cross-set collapse.
fn resolve_preferred_collision(actions: &mut CodeActionResponse) {
    let mut kept = false;
    for item in actions.iter_mut() {
        if let CodeActionOrCommand::CodeAction(action) = item
            && action.is_preferred == Some(true)
        {
            if kept {
                action.is_preferred = Some(false);
            } else {
                kept = true;
            }
        }
    }
}

impl Kakehashi {
    pub(crate) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token;
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let context = params.context;
        let upstream_caps = self.upstream_code_action_caps();

        let virt =
            self.code_action_virt_layer(&lsp_uri, range, context, work_done_token, upstream_caps);
        let host = self.code_action_host_layer(&lsp_uri, raw_params, upstream_caps);
        // Cross-layer strategy (`layers.aggregation."textDocument/codeAction"`),
        // resolved once inside the walk: `preferred` returns the highest-priority
        // non-empty layer; `concatenated` merges every layer's actions in priority
        // order into one menu (#568 PR 7).
        let result = self
            .walk_layers_by_strategy(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                std::future::ready(Ok(None)),
                |actions: &CodeActionResponse| !actions.is_empty(),
                |mut acc: CodeActionResponse, next| {
                    acc.extend(next);
                    acc
                },
            )
            .await;
        // Collapse the cross-source isPreferred collision on the FINAL menu,
        // regardless of the cross-layer strategy: WITHIN-layer concatenation can
        // merge several servers' actions into one layer even when the cross-layer
        // strategy is `preferred` (that layer still wins as a unit), so several
        // `isPreferred: true` actions can reach here without a cross-layer merge.
        result.map(|maybe| {
            maybe.map(|mut actions| {
                resolve_preferred_collision(&mut actions);
                actions
            })
        })
    }

    /// The upstream client's codeAction capabilities that gate the bridge
    /// policy: `disabledSupport` (LSP 3.16) drives disable-vs-drop for
    /// actions the bridge cannot execute yet; `isPreferredSupport`
    /// (LSP 3.15) gates the isPreferred passthrough.
    fn upstream_code_action_caps(&self) -> UpstreamCodeActionCaps {
        let code_action = self
            .settings_manager
            .client_capabilities_lock()
            .get()
            .and_then(|caps| caps.text_document.as_ref())
            .and_then(|td| td.code_action.as_ref());
        UpstreamCodeActionCaps {
            disabled_support: code_action
                .and_then(|ca| ca.disabled_support)
                .unwrap_or(false),
            is_preferred_support: code_action
                .and_then(|ca| ca.is_preferred_support)
                .unwrap_or(false),
            data_support: code_action.and_then(|ca| ca.data_support).unwrap_or(false),
            // The client can lazily resolve `edit` only if its resolveSupport
            // properties list includes "edit". A client advertising
            // resolveSupport WITHOUT "edit" cannot materialize a lazy action's
            // edit, so it must not be handed one (eager-resolve instead).
            resolve_edit_support: code_action
                .and_then(|ca| ca.resolve_support.as_ref())
                .map(|rs| rs.properties.iter().any(|p| p == "edit"))
                .unwrap_or(false),
        }
    }

    /// `codeAction/resolve`: route the action back to the downstream server
    /// that produced it, identified by the envelope in `action.data` (#568
    /// PR 4). Fails soft at every step: an action without an envelope
    /// (host-layer or foreign) passes through unchanged, and a stale region
    /// returns the action unresolved with its envelope intact — clients
    /// re-request actions on change, so the staleness window is short.
    pub(crate) async fn code_action_resolve_impl(&self, action: CodeAction) -> Result<CodeAction> {
        let Some(envelope) = extract_code_action_envelope(&action) else {
            return Ok(action);
        };

        // Fail-soft staleness gate: resolving against a moved or invalidated
        // region would translate a resolved edit with a stale offset and bind
        // it to content the user has since edited. The same live lookup yields
        // the region's current host-document end, which bounds the resolved
        // edit so a stale/malformed one can't escape the region into unrelated
        // host text (see `translate_edit_host_ward_strict`).
        //
        // A genuine host-layer action carries no region (verbatim, no
        // translation), so the gate — which ULID-parses `region_id` and would
        // fail on the empty host one — is skipped; `region_end` is unused on the
        // host resolve path. `is_host_layer` additionally requires the empty
        // `region_id`, so a CONFORMING client can't skip the gate merely by
        // toggling `host_layer` on a virt envelope. It is not a security boundary
        // (the envelope round-trips through unprotected client `data`, so a client
        // could clear `region_id` too) — it guards against accidental bypass, and
        // the host path fails soft anyway.
        let region_end = if envelope.is_host_layer() {
            Position::default()
        } else {
            match self.code_action_region_end_if_fresh(&envelope) {
                Ok(region_end) => region_end,
                Err(RegionEndUnavailable::MalformedEnvelope) => {
                    // Already warned with the malformed field; no stale-debug.
                    return Ok(action);
                }
                Err(RegionEndUnavailable::Stale) => {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "codeAction/resolve: region {} of {} (origin {:?}) is stale; \
                         returning action unresolved",
                        envelope.region_id,
                        envelope.host_uri,
                        envelope.origin
                    );
                    return Ok(action);
                }
            }
        };

        let settings = self.settings_manager.load_settings();
        let upstream_caps = self.upstream_code_action_caps();
        let upstream_id = crate::lsp::current_upstream_id();
        // Propagate a client $/cancelRequest as RequestCancelled instead of
        // masking it as an unresolved-action success (the cancel is forwarded
        // downstream; the -32800 answer would otherwise be collapsed by the
        // fail-soft parsing) — mirrors the multi-region codeAction walk.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let pool = self.bridge.pool_arc();
        let dispatch = pool.dispatch_code_action_resolve(
            action,
            &settings,
            upstream_caps,
            upstream_id,
            region_end,
        );
        // The cancel arm DROPS the in-flight dispatch, which then never
        // reaches its own refcounted unregister — an RAII sweep (dropped at
        // function exit) covers that, and unlike a trailing statement it also
        // runs when this whole handler future is dropped (client disconnect /
        // shutdown). Idempotent after normal completion, where the dispatch
        // cleaned up itself. The CAPTURED id, not a re-read of the task-local:
        // the sweep must target exactly the id the dispatch registered under.
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                resolved = dispatch => Ok(resolved),
            },
            None => Ok(dispatch.await),
        }
    }

    /// The region's current content-precise host-document END position if the
    /// region is still FRESH — i.e. re-resolving it from the live parse yields
    /// the SAME offset the action was minted with — else `None`.
    ///
    /// The resolve path translates the resolved edit using the envelope's
    /// SNAPSHOT offset (`RegionOffset::from(&envelope.offset)`). Re-resolve the
    /// live offset and compare the WHOLE thing, not just the start: if any
    /// per-line column offset diverged (e.g. an interior blockquote-prefix edit
    /// left the start line intact) translating with the stale offset would bind
    /// the edit to wrong host columns — corruption. On any divergence fail soft
    /// (the client re-requests fresh actions), mirroring the stale-region case.
    /// The same live resolution yields the content-precise region end used to
    /// bound the edit (`resolved.region.byte_range.end`, matching applyEdit).
    ///
    /// Known limitation (shared with `code_lens_region_is_fresh`): for
    /// injections whose queries apply `#offset!` (today only YAML/TOML
    /// frontmatter in the bundled markdown queries), the envelope offset is
    /// `#offset!`-adjusted while the live resolution isn't, so the comparison
    /// never matches and resolve always fails soft for those regions. That errs
    /// in the safe direction (the action stays unresolved) and frontmatter code
    /// actions have no known real-world producer; revisit if one appears.
    fn code_action_region_end_if_fresh(
        &self,
        envelope: &CodeActionEnvelope,
    ) -> std::result::Result<Position, RegionEndUnavailable> {
        // A malformed envelope is NOT staleness — the bridge only mints valid
        // URLs/ULIDs, so warn (mirroring the bridge-side host_uri warn) rather
        // than letting it hide under the "stale region" debug line; the
        // caller skips its stale-debug for this classification.
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            log::warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: envelope host_uri {:?} is not a valid URL",
                envelope.host_uri
            );
            return Err(RegionEndUnavailable::MalformedEnvelope);
        };
        let Ok(ulid) = envelope.region_id.parse::<Ulid>() else {
            log::warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: envelope region_id {:?} is not a valid ULID",
                envelope.region_id
            );
            return Err(RegionEndUnavailable::MalformedEnvelope);
        };
        self.code_action_region_end_live(envelope, &host_url, ulid)
            .ok_or(RegionEndUnavailable::Stale)
    }

    /// The live-lookup half of [`Self::code_action_region_end_if_fresh`]:
    /// every `None` here is the STALE class (region moved, invalidated, or
    /// diverged), never a malformed envelope.
    fn code_action_region_end_live(
        &self,
        envelope: &CodeActionEnvelope,
        host_url: &Url,
        ulid: Ulid,
    ) -> Option<Position> {
        // Re-resolve the region from the LIVE parse (same construction as the
        // goto/showDocument offset path), yielding the current per-line offset
        // AND the content-precise host byte range.
        let (start_byte, _end, _kind, _layer) =
            self.bridge.node_tracker().lookup_node(host_url, &ulid)?;
        let snapshot = self.documents.get(host_url)?.snapshot()?;
        let language_name = detect_document_language(&self.language, &self.documents, host_url)?;
        let injection_query = self.language.injection_query(&language_name)?;
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &self.language,
            self.bridge.node_tracker(),
            host_url,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
            start_byte,
            snapshot.incarnation(),
        )?;
        // The tracker byte and the freshly-resolved region can disagree after an
        // edit (the byte now falls in a different live region); a mismatched
        // region_id means we'd bound/translate against the wrong region.
        if resolved.region.region_id != envelope.region_id {
            return None;
        }
        // Content-precise host end (matches applyEdit) — compute before moving
        // `line_column_offsets` out of `resolved`.
        let region_end = PositionMapper::new(snapshot.text())
            .byte_to_position(resolved.region.byte_range.end)?;
        let live_offset = RegionOffset::with_per_line_offsets(
            resolved.region.line_range.start,
            resolved.line_column_offsets,
        );
        // Compare the WHOLE offset, not just the start: a diverged interior
        // per-line column offset (e.g. a blockquote-prefix edit that left the
        // start line intact) would translate the resolved edit to wrong host
        // columns. Fail soft on any divergence.
        if live_offset != RegionOffset::from(&envelope.offset) {
            return None;
        }
        Some(region_end)
    }

    /// Virt layer: bridge the injection region under the requested range.
    async fn code_action_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        context: CodeActionContext,
        client_progress_token: Option<NumberOrString>,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Result<Option<CodeActionResponse>> {
        // EVERY injection region the range overlaps (usually one). A range
        // spanning several fences dispatches to each and the results are merged
        // into one menu (#628 multi-region), in document order. Latency scales
        // with the overlapped-region count (bounded by the document's injection
        // count); the common case is a range within a single fence.
        let contexts = self
            .resolve_bridge_contexts_for_all_overlapping_regions(lsp_uri, range, METHOD)
            .await;
        if contexts.is_empty() {
            return Ok(None);
        }

        // All overlapping regions share THIS request's upstream id. Pool
        // registration cleanup is NOT done here: the host layer runs
        // concurrently under the same upstream id (`race_layers_concatenated`
        // is codeAction's default), so a walk-level `unregister_all` would
        // wipe the sibling layer's live cancel registrations the moment this
        // layer finishes. Completed dispatches unregister themselves
        // (refcounted), and the caller (`walk_layers_by_strategy` →
        // `run_layer_race`) sweeps whatever a dropped arm left behind after
        // the WHOLE race. The cancel subscription here is usually a redundant
        // duplicate of that caller's — kept so the walk stays self-cancelling
        // even if ever driven outside the race.
        let upstream_id = contexts[0].document.upstream_request_id.clone();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());

        let mut progress_token = client_progress_token;
        let walk = async {
            let mut merged: CodeActionResponse = Vec::new();
            for mut ctx in contexts {
                // The work-done progress token is single-use → first region only.
                ctx.document.client_progress_token = progress_token.take();
                if let Some(actions) = self
                    .dispatch_virt_region_code_action(ctx, &context, upstream_caps)
                    .await?
                {
                    merged.extend(actions);
                }
            }
            Ok((!merged.is_empty()).then_some(merged))
        };

        // Cancel aborts the walk (dropping the in-flight region's dispatch) and
        // surfaces `RequestCancelled` (-32800), matching how the outer
        // `run_layer_race` reports cancellation — so a client `$/cancelRequest`
        // is never masked as an empty result.
        match cancel_rx {
            Some(rx) => tokio::select! {
                r = walk => r,
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
            },
            None => walk.await,
        }
    }

    /// Fan out `textDocument/codeAction` across the servers bridging ONE
    /// injection region, applying that region's within-layer strategy
    /// (`bridge.<lang>.aggregation.<method>.strategy`): `preferred` returns the
    /// highest-priority non-empty server; `concatenated` merges every server's
    /// actions in priority order (#568 PR 7). One region of the multi-region
    /// virt walk; cancellation and the upstream-id registration are held by the
    /// caller for the whole walk, so this passes no `cancel_rx` of its own.
    async fn dispatch_virt_region_code_action(
        &self,
        ctx: RangeRequestContext,
        context: &CodeActionContext,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Result<Option<CodeActionResponse>> {
        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let f = |t: FanOutTask| {
            let context = context.clone();
            async move {
                t.pool
                    .send_code_action_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        range,
                        context,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                        t.client_progress_token,
                        upstream_caps,
                    )
                    .await
            }
        };
        // No `cancel_rx` here: the caller holds a single cancel subscription for
        // the whole multi-region walk and aborts it on cancel.
        match ctx.document.strategy {
            AggregationStrategy::Preferred => {
                dispatch_preferred(
                    &ctx.document,
                    pool.clone(),
                    f,
                    |opt| matches!(opt, Some(v) if !v.is_empty()),
                    None,
                )
                .await
                .handle(&self.client, "code action", None, Ok)
                .await
            }
            AggregationStrategy::Concatenated => {
                dispatch_concatenated(&ctx.document, pool.clone(), f, None, None, None)
                    .await
                    .handle(&self.client, "code action", None, |vecs| {
                        Ok(concat_merge(vecs))
                    })
                    .await
            }
        }
    }

    /// Host layer: forward the params verbatim to the host language's own
    /// servers, then apply the bridge action policy (suffix, command
    /// disabling) per winning server. Edit URIs and coordinates stay
    /// untranslated (already real); only bridge-local
    /// `TextDocumentEdit.version`s are nulled.
    async fn code_action_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Result<Option<CodeActionResponse>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        // One fan-out closure, dispatched by the within-host-layer strategy
        // (`bridge._self.aggregation.<method>.strategy`); moves into one arm.
        let f = move |t: HostFanOutTask| {
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
                        },
                        METHOD,
                        params,
                        t.upstream_id,
                    )
                    .await?;
                let Some(value) = raw else {
                    return Ok(None);
                };
                let Some(actions) = parse_code_actions_leniently(value) else {
                    return Ok(None);
                };
                // Whether this host server advertises `codeAction/resolve`, so a
                // host lazy action can be enveloped for resolve-routing back to
                // it (#627) rather than disabled. Queried on the just-opened
                // connection — but only when the answer can actually change the
                // outcome, so the probe (and its marker-root resolution + pool
                // lock) is skippable on this hot `textDocument/codeAction` path:
                //
                // - `server_resolves` is read by `bridge_code_action` ONLY for a
                //   possibly-lazy action (no edit, no command, and not already
                //   disabled — a disabled action returns early, before the lazy
                //   gate). No such action ⇒ never consulted.
                // - Even with such an action, a HOST lazy action needs the CLIENT
                //   to either envelope it (`can_envelope`) or show it disabled
                //   (`disabled_support`); with NEITHER, it is dropped regardless
                //   of `server_resolves` (envelope path gated on `can_envelope`,
                //   `disable_action` returns `None` without `disabled_support`).
                let client_can_use_resolve =
                    upstream_caps.can_envelope() || upstream_caps.disabled_support;
                let maybe_lazy = |item: &CodeActionOrCommand| match item {
                    CodeActionOrCommand::CodeAction(a) => {
                        a.edit.is_none() && a.command.is_none() && a.disabled.is_none()
                    }
                    CodeActionOrCommand::Command(_) => false,
                };
                let server_resolves = client_can_use_resolve
                    && actions.iter().any(maybe_lazy)
                    && t.pool
                        .host_server_advertises(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            "codeAction/resolve",
                        )
                        .await;
                Ok(Some(bridge_code_actions(
                    actions,
                    &t.server_name,
                    t.uri.as_str(),
                    upstream_caps,
                    server_resolves,
                    None,
                )))
            }
        };
        // No layer-level `unregister_all` here: the virt layer runs
        // concurrently under the SAME upstream id, so wiping the registry on
        // this layer's completion would drop the sibling's live cancel
        // registrations. Completed dispatches unregister themselves
        // (refcounted); `run_layer_race` sweeps after the whole race.
        match ctx.strategy {
            AggregationStrategy::Preferred => {
                let fan_in = dispatch_host_preferred(
                    &ctx,
                    pool.clone(),
                    f,
                    |opt| matches!(opt, Some(v) if !v.is_empty()),
                    cancel_rx,
                )
                .await;
                self.host_code_action_result(fan_in, |v| v).await
            }
            AggregationStrategy::Concatenated => {
                let fan_in =
                    dispatch_host_concatenated(&ctx, pool.clone(), f, cancel_rx, None, None).await;
                self.host_code_action_result(fan_in, concat_merge).await
            }
        }
    }

    /// Fold a host-layer fan-in result into the handler's return, with the
    /// host-arm quieting: an all-empty host layer is the normal outcome
    /// whenever virt answers, so it stays SILENT (unlike [`FanInResult::handle`],
    /// which logs a LOG-level message) and warns only on real failures.
    async fn host_code_action_result<T>(
        &self,
        result: FanInResult<T>,
        on_done: impl FnOnce(T) -> Option<CodeActionResponse>,
    ) -> Result<Option<CodeActionResponse>> {
        match result {
            FanInResult::Done(value) => Ok(on_done(value)),
            FanInResult::NoResult { errors } => {
                if errors > 0 {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("No {METHOD} response from any host bridge server"),
                        )
                        .await;
                }
                Ok(None)
            }
            FanInResult::Cancelled => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Command;

    fn action(title: &str, preferred: Option<bool>) -> CodeActionOrCommand {
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            is_preferred: preferred,
            ..CodeAction::default()
        })
    }

    fn command(title: &str) -> CodeActionOrCommand {
        CodeActionOrCommand::Command(Command {
            title: title.to_string(),
            command: "c".to_string(),
            arguments: None,
        })
    }

    #[test]
    fn concat_merge_flattens_in_order_and_drops_none() {
        let merged = concat_merge(vec![
            Some(vec![action("a", None)]),
            None,
            Some(vec![action("b", None), action("c", None)]),
        ])
        .expect("non-empty");
        let titles: Vec<_> = merged
            .iter()
            .filter_map(|i| match i {
                CodeActionOrCommand::CodeAction(a) => Some(a.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, ["a", "b", "c"], "priority order preserved");
    }

    #[test]
    fn concat_merge_all_empty_is_none() {
        assert!(concat_merge(vec![None, Some(vec![]), None]).is_none());
    }

    #[test]
    fn preferred_collision_keeps_only_the_first() {
        // Two servers each marked their action preferred; after the collapse
        // only the FIRST (highest-priority in the merged order) stays true.
        let mut actions = vec![
            action("x", Some(true)),
            command("cmd"),
            action("y", Some(true)),
            action("z", None),
        ];
        resolve_preferred_collision(&mut actions);
        let prefs: Vec<Option<bool>> = actions
            .iter()
            .filter_map(|i| match i {
                CodeActionOrCommand::CodeAction(a) => Some(a.is_preferred),
                _ => None,
            })
            .collect();
        assert_eq!(prefs, [Some(true), Some(false), None]);
    }

    #[test]
    fn preferred_collision_leaves_a_single_preferred_untouched() {
        let mut actions = vec![action("x", None), action("y", Some(true))];
        resolve_preferred_collision(&mut actions);
        let CodeActionOrCommand::CodeAction(y) = &actions[1] else {
            unreachable!()
        };
        assert_eq!(y.is_preferred, Some(true));
    }
}
