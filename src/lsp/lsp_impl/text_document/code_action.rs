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
    NumberOrString, Position, Range, Uri,
};
use ulid::Ulid;
use url::Url;

use super::super::Kakehashi;
use super::super::bridge_context::{RangeRequestContext, Sourced};
use super::super::region_offset::resolve_region_offset;
use crate::config::settings::AggregationStrategy;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, HostFanOutTask, dispatch_concatenated, dispatch_host_concatenated,
    dispatch_host_preferred, dispatch_preferred, preferred_among,
};
use crate::lsp::bridge::{
    CodeActionEnvelope, HostDocument, UpstreamCodeActionCaps, bridge_code_actions,
    extract_code_action_envelope, parse_code_actions_leniently,
};

const METHOD: &str = "textDocument/codeAction";

/// A layer's actions, part by part: each region's (or the host layer's),
/// with the prepared region it was mapped through, if any, for the final
/// check.
type ActionParts = Vec<(CodeActionResponse, Option<Sourced<()>>)>;

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

/// Whether `actions` answer a `preferred` walk, winning over a server or
/// layer ranked below: any action at all — but on a prepared document not
/// one whose edit the map refused (it comes back disabled), where a server
/// ranked below may map. An action its server disabled answers as on any
/// document.
fn actions_answer(actions: &[CodeActionOrCommand], prepared: bool) -> bool {
    if !prepared {
        return !actions.is_empty();
    }
    actions.iter().any(|action| match action {
        CodeActionOrCommand::Command(_) => true,
        CodeActionOrCommand::CodeAction(action) => {
            !crate::lsp::bridge::refused_by_prepared_map(action)
        }
    })
}

/// Whether a layer's `parts` answer a cross-layer `preferred` walk (see
/// [`actions_answer`]): a prepared part only by an enabled action, and
/// only while it may still be kept (`may_keep`, read as the race decides):
/// one moved or stale is no answer, and the race falls through to the next
/// layer's rather than choosing actions the final check drops.
fn parts_answer(parts: &ActionParts, may_keep: impl Fn(&Sourced<()>) -> bool) -> bool {
    parts.iter().any(|(actions, source)| {
        actions_answer(actions, source.is_some()) && source.as_ref().is_none_or(&may_keep)
    })
}

/// What a cross-layer `preferred` walk answers when no layer's actions
/// answer it: the highest-ranked layer's actions that came, though none
/// answered (on a prepared document, all disabled: refused by its map) —
/// as the walk would choose them were the document unprepared, so a client
/// showing disabled actions shows why the fix is refused.
#[derive(Default)]
struct PassedOver(std::sync::Mutex<Option<ActionParts>>);

impl PassedOver {
    /// [`parts_answer`], keeping `parts` aside when they hold actions, do
    /// not answer, and may still be kept (`may_keep`): only refused ones,
    /// never a moved or stale answer the race passed over for that, which
    /// the final check would drop. Asked in priority order, never of a
    /// layer before the ones above it have come, the first kept is the
    /// highest-ranked.
    fn answers(&self, parts: &ActionParts, may_keep: impl Fn(&Sourced<()>) -> bool) -> bool {
        if parts_answer(parts, &may_keep) {
            return true;
        }
        let mut kept = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if kept.is_none()
            && parts.iter().any(|(actions, _)| !actions.is_empty())
            && parts.iter().all(|(actions, source)| {
                actions.is_empty() || source.as_ref().is_none_or(&may_keep)
            })
        {
            *kept = Some(parts.clone());
        }
        false
    }

    /// The walk's `result`, or, when it found no answer (nothing, or only
    /// a layer's error, which an answer would have outranked), the actions
    /// kept aside, with the error they displace: the answer still if they
    /// are dropped at the final check (see [`judged_or_displaced`]). A
    /// cancel stays a cancel.
    fn or_kept(
        self,
        result: Result<Option<ActionParts>>,
    ) -> (
        Result<Option<ActionParts>>,
        Option<tower_lsp_server::jsonrpc::Error>,
    ) {
        let kept = self
            .0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (result, kept) {
            (Ok(None), Some(parts)) => (Ok(Some(parts)), None),
            (Err(error), Some(parts))
                if error.code != tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled =>
            {
                (Ok(Some(parts)), Some(error))
            }
            (result, _) => (result, None),
        }
    }
}

/// The answer once the actions chosen are judged: those kept, or, none
/// left, the error kept-aside actions displaced (`displaced`), if any —
/// they never turn a layer's error into an empty answer.
fn judged_or_displaced(
    judged: std::result::Result<CodeActionResponse, ()>,
    displaced: Option<tower_lsp_server::jsonrpc::Error>,
) -> Result<Option<CodeActionResponse>> {
    match (judged, displaced) {
        (Ok(actions), _) => Ok(Some(actions)),
        (Err(()), Some(error)) => Err(error),
        (Err(()), None) => Ok(None),
    }
}

/// The actions of `parts` judged kept, in order: `Err` when judging emptied
/// the answer (none left of a stale region's actions). An answer that came
/// empty stays the empty answer it was (a concatenated walk's `[]`).
fn current_actions(
    parts: Vec<(CodeActionResponse, bool)>,
) -> std::result::Result<CodeActionResponse, ()> {
    let mut kept: CodeActionResponse = Vec::new();
    let mut dropped = false;
    for (actions, keep) in parts {
        if keep {
            kept.extend(actions);
        } else {
            dropped = true;
        }
    }
    if dropped && kept.is_empty() {
        return Err(());
    }
    Ok(kept)
}

/// [`current_actions`], each prepared part judged by the same final check
/// as the other answers mapped through prepared regions
/// ([`sources_verdict`](super::super::bridge_context::sources_verdict)): kept when
/// current, which it reads at once in the common case; waited on only
/// while the host's last edit is settling or its region held for its
/// answer (an edit to another fence just before the request, say), all
/// parts within one prepare timeout; dropped at once when moved or stale.
/// Every part kept is read again once the last wait ends, together: one
/// judged current before another's wait may no longer be (the host
/// edited, a region re-prepared to other coordinates). The others are kept
/// as they are.
async fn judged_actions(
    pool: &crate::lsp::bridge::LanguageServerPool,
    tracker: &crate::language::NodeTracker,
    current_version: impl Fn() -> Option<u64>,
    parts: ActionParts,
) -> std::result::Result<CodeActionResponse, ()> {
    // One wait for all parts, not one for each: what is left of it bounds
    // each part's.
    let deadline = tokio::time::Instant::now() + crate::lsp::bridge::PREPARE_TIMEOUT;
    let mut judged = Vec::with_capacity(parts.len());
    for (actions, source) in parts {
        let keep = match &source {
            None => true,
            Some(source) => super::super::bridge_context::sources_verdict_within(
                pool,
                tracker,
                &current_version,
                std::iter::once(source),
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            )
            .await
            .keeps(),
        };
        judged.push((actions, source, keep));
    }
    // A part judged current before another was waited on may not be by
    // now (the host edited during that wait): every part kept is read
    // once more, together, with no await between that read and the answer.
    let judged = judged
        .into_iter()
        .map(|(actions, source, keep)| {
            let keep = keep
                && source
                    .as_ref()
                    .is_none_or(|source| source.still_current(pool, tracker, &current_version));
            (actions, keep)
        })
        .collect();
    current_actions(judged)
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
        let host = async {
            let actions = self
                .code_action_host_layer(&lsp_uri, raw_params, upstream_caps)
                .await?;
            Ok(actions.map(|actions| vec![(actions, None)]))
        };
        let pool = self.bridge.pool();
        let tracker = self.bridge.node_tracker();
        let passed_over = PassedOver::default();
        let may_keep = |source: &Sourced<()>| {
            super::formatting::source_may_keep(source, pool, tracker, || {
                self.host_content_version(&lsp_uri)
            })
        };
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
                |parts: &ActionParts| passed_over.answers(parts, may_keep),
                |mut parts: ActionParts, next| {
                    parts.extend(next);
                    parts
                },
            )
            .await;
        let (result, displaced) = passed_over.or_kept(result);
        // The final answer chosen: each prepared region's actions checked
        // once more against the host text, which may have moved while they
        // were held (see `judged_actions`). A stale region's actions are
        // dropped; the others in the answer chosen stay (under
        // `concatenated`, the host layer's and other regions'). Under a
        // cross-layer `preferred` a virt answer already moved or stale as
        // the race decided lost it to the host layer's (`parts_answer`);
        // the host layer's answer is not in the one chosen, so a virt
        // answer that was settling then and is dropped here, or that the
        // host moved under since, leaves none, until the editor asks again.
        let result = match result {
            Ok(Some(parts)) => judged_or_displaced(
                judged_actions(
                    self.bridge.pool(),
                    self.bridge.node_tracker(),
                    || self.host_content_version(&lsp_uri),
                    parts,
                )
                .await
                .inspect_err(|()| {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "codeAction results for {} dropped: a prepared virtual document they were mapped through is no longer the host's text",
                        lsp_uri.as_str()
                    );
                }),
                displaced,
            ),
            other => other.map(|_| None),
        };
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
    /// PR 4). Foreign actions pass through unchanged. Unavailable/stale actions
    /// and replies without an edit, command, or disabled reason report request
    /// failure, so clients can request fresh actions instead of silently doing
    /// nothing. Cancellation remains RequestCancelled.
    pub(crate) async fn code_action_resolve_impl(&self, action: CodeAction) -> Result<CodeAction> {
        let Some(envelope) = extract_code_action_envelope(&action) else {
            return Ok(action);
        };
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            log::warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: envelope host_uri {:?} is not a valid URL",
                envelope.host_uri
            );
            return Err(code_action_resolve_failed());
        };
        // Both layers: the action was computed against one text revision, and
        // a lazily materialized edit for another must not be applied to this
        // one even when the region's geometry still matches.
        if !self.document_revision_is_current(&host_url, envelope.content_version) {
            log::debug!(
                target: "kakehashi::bridge",
                "codeAction/resolve: {} was revised since the action was produced; refusing resolve",
                envelope.host_uri
            );
            return Err(code_action_resolve_failed());
        }
        // Before the region gate waits for a parse: an action from a closed
        // and reopened document would otherwise park on the reopened
        // lifetime's first parse only to be refused.
        if !self.host_incarnation_is_current(&host_url, envelope.incarnation) {
            log::debug!(
                target: "kakehashi::bridge",
                "codeAction/resolve: {} was reopened since the action was produced; refusing resolve",
                envelope.host_uri
            );
            return Err(code_action_resolve_failed());
        }

        // Staleness gate: resolving against a moved or invalidated
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
            match self.code_action_region_end_if_fresh(&envelope).await {
                Ok(region_end) => region_end,
                Err(RegionEndUnavailable::MalformedEnvelope) => {
                    // Already warned with the malformed field; no stale-debug.
                    return Err(code_action_resolve_failed());
                }
                Err(RegionEndUnavailable::Stale) => {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "codeAction/resolve: region {} of {} (origin {:?}) is stale; \
                         refusing resolve",
                        envelope.region_id,
                        envelope.host_uri,
                        envelope.origin
                    );
                    return Err(code_action_resolve_failed());
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
        let read_host = |uri: &url::Url| self.host_resolve_snapshot(uri);
        let dispatch = pool.dispatch_code_action_resolve(
            action,
            &settings,
            upstream_caps,
            upstream_id,
            region_end,
            &read_host,
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
        let resolved = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                resolved = dispatch => Ok(resolved),
            },
            None => Ok(dispatch.await),
        }?;
        // A later didChange/didClose is allowed to proceed once the resolve
        // was enqueued. Revalidate after the response so an edit computed
        // against the content this resolve observed is not surfaced into a
        // revised or reopened document.
        if !self.code_action_response_is_current(&host_url, &envelope) {
            log::debug!(
                target: "kakehashi::bridge",
                "codeAction/resolve: {} was revised or reopened while resolving; refusing resolve",
                envelope.host_uri
            );
            return Err(code_action_resolve_failed());
        }
        // A lazy response cannot execute. Returning it as a successful resolve
        // makes a selected action silently do nothing. Completion can keep its
        // basic insertion on resolve failure; an action needs an edit/command
        // or an explicit disabled reason. Let the client present request failure
        // when appropriate instead of emitting an unsolicited notification.
        if resolved.edit.is_none() && resolved.command.is_none() && resolved.disabled.is_none() {
            return Err(code_action_resolve_failed());
        }
        Ok(resolved)
    }

    /// Whether a resolved action still belongs where its envelope was
    /// minted: the document is neither revised nor reopened, nor its region
    /// prepared meanwhile (an epoch move gaining its pair a peer), whose edit
    /// is in a prepared text's coordinates the envelope's offset does not map.
    fn code_action_response_is_current(
        &self,
        host_url: &Url,
        envelope: &CodeActionEnvelope,
    ) -> bool {
        self.document_revision_is_current(host_url, envelope.content_version)
            && self.host_incarnation_is_current(host_url, envelope.incarnation)
            && (envelope.is_host_layer()
                // Minted from a prepared document, it was mapped through
                // the map its server held, checked as it answered.
                || envelope.offset.prepared
                || !self.bridge.region_coordinates_may_be_prepared(
                    host_url,
                    &envelope.injection_language,
                    &envelope.region_id,
                ))
    }

    /// The region's current content-precise host-document END position if the
    /// region is still FRESH — i.e. re-resolving it from the live parse yields
    /// the SAME offset the action was minted with — else `RegionEndUnavailable`.
    ///
    /// The resolve path translates the resolved edit using the envelope's
    /// SNAPSHOT offset (`RegionOffset::from(&envelope.offset)`). Re-resolve the
    /// live offset and compare the WHOLE thing, not just the start: if any
    /// per-line column offset diverged (e.g. an interior blockquote-prefix edit
    /// left the start line intact) translating with the stale offset would bind
    /// the edit to wrong host columns — corruption. On any divergence reject
    /// resolve so the client can request fresh actions, as for a stale region.
    /// The same live resolution yields the content-precise region end used to
    /// bound the edit (the exact virtual-content end mapped through the live
    /// per-line offset, matching applyEdit).
    ///
    /// The live resolution applies the injection query's `#offset!` / `#trim!`
    /// directives exactly as minting did, so a region under one of them
    /// (markdown frontmatter, blockquote prose) compares equal while unchanged
    /// and its actions resolve.
    async fn code_action_region_end_if_fresh(
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
        if envelope.region_id.parse::<Ulid>().is_err() {
            log::warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: envelope region_id {:?} is not a valid ULID",
                envelope.region_id
            );
            return Err(RegionEndUnavailable::MalformedEnvelope);
        }
        // A resolve issued while the post-edit reparse is still running would
        // find no snapshot and read as a stale region; wait (bounded) for the
        // current parse before rebuilding the region.
        self.wait_for_resolve_parse(&host_url).await;
        self.code_action_region_end_live(envelope, &host_url)
            .ok_or(RegionEndUnavailable::Stale)
    }

    /// The live-lookup half of [`Self::code_action_region_end_if_fresh`]:
    /// every `None` here is the STALE class (region moved, invalidated, or
    /// diverged), never a malformed envelope.
    fn code_action_region_end_live(
        &self,
        envelope: &CodeActionEnvelope,
        host_url: &Url,
    ) -> Option<Position> {
        // Re-resolve the region from the LIVE parse (the same construction the
        // goto/showDocument/applyEdit offset paths use), yielding the current
        // per-line offset and the exact mapped host end.
        // An action minted from a prepared document is mapped through the
        // map its server holds as it resolves (see the bridge's resolve):
        // only the region's geometry is compared here. Any other must find
        // the region as is.
        let (live_offset, region_end, contiguous, live_language) = if envelope.offset.prepared {
            super::super::region_offset::resolved_region_geometry(
                super::super::region_offset::resolve_region(
                    &self.documents,
                    &self.language,
                    &self.bridge,
                    host_url,
                    &envelope.region_id,
                )?,
            )
        } else {
            resolve_region_offset(
                &self.documents,
                &self.language,
                &self.bridge,
                host_url,
                &envelope.region_id,
            )?
        };
        // The action may have been created while a single combined capture was
        // contiguous, then resolved after another capture introduced a masked
        // host gap. The stable first-region ID/offset alone cannot detect that
        // geometry change, so re-check live edit safety here.
        if !contiguous {
            return None;
        }
        // The region may have been re-routed (a shebang edit under an
        // `unknown` injection) without moving, and a client can echo the
        // envelope with another language: an action belongs to the language
        // it was produced for, and dispatch builds the virtual route from the
        // echoed value.
        if live_language != envelope.injection_language {
            return None;
        }
        // Compare the WHOLE offset, not just the start: a diverged interior
        // per-line column offset (e.g. a blockquote-prefix edit that left the
        // start line intact) would translate the resolved edit to wrong host
        // columns. Fail soft on any divergence.
        let describes = if envelope.offset.prepared {
            crate::lsp::bridge::RegionOffset::from(&envelope.offset) == live_offset
        } else {
            envelope.offset.describes(&live_offset)
        };
        if !describes {
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
    ) -> Result<Option<ActionParts>> {
        // Before the text is read: see `Sourced::from_region`.
        let prepared_revision = self.prepared_request_revision(lsp_uri);
        // EVERY injection region the range overlaps (usually one). A range
        // spanning several fences dispatches to each and the results are merged
        // into one menu (#628 multi-region), in document order. Latency scales
        // with the overlapped-region count (bounded by the document's injection
        // count); the common case is a range within a single fence.
        // Read before the preamble snapshots the document, so the stamp can
        // only be older than the content the actions were computed on, never
        // newer: an edit landing in between makes resolve reject these actions,
        // so the editor must request fresh ones.
        let Some(revision) = url::Url::parse(lsp_uri.as_str()).ok().and_then(|uri| {
            self.documents
                .get(&uri)
                .map(|doc| crate::lsp::bridge::HostRevision {
                    incarnation: doc.incarnation(),
                    content_version: doc.content_version(),
                })
        }) else {
            return Ok(None);
        };
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
            // Each region's actions, with where they were mapped through
            // when prepared, for the final check.
            let mut parts: ActionParts = Vec::new();
            for mut ctx in contexts {
                // The work-done progress token is single-use → first region only.
                ctx.document.client_progress_token = progress_token.take();
                let source = Sourced::from_region(
                    &ctx.document,
                    (),
                    self.bridge.node_tracker(),
                    prepared_revision,
                );
                if let Some(actions) = self
                    .dispatch_virt_region_code_action(ctx, &context, upstream_caps, revision)
                    .await?
                {
                    // An unprepared region's actions are current as they
                    // are: nothing kept for them.
                    parts.push((actions, source.is_prepared().then_some(source)));
                }
            }
            Ok(parts
                .iter()
                .any(|(actions, _)| !actions.is_empty())
                .then_some(parts))
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
        revision: crate::lsp::bridge::HostRevision,
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
                        t.region_end(),
                        t.offset,
                        &t.virtual_content,
                        revision,
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
            AggregationStrategy::Preferred if ctx.document.prepared.is_none() => {
                dispatch_preferred(
                    &ctx.document,
                    pool.clone(),
                    f,
                    |opt| matches!(opt, Some(v) if !v.is_empty()),
                    None,
                )
                .await
                .handle(&self.notifier(), "code action", None, Ok)
                .await
            }
            AggregationStrategy::Preferred => {
                // A prepared document's answers of refused actions only,
                // passed over in the fan-in, as they arrived: the answer
                // when no server's actions answer.
                let passed_over =
                    std::sync::Arc::new(std::sync::Mutex::new(
                        Vec::<(String, CodeActionResponse)>::new(),
                    ));
                let kept = std::sync::Arc::clone(&passed_over);
                let f = move |t: FanOutTask| {
                    let server_name = t.server_name.clone();
                    let kept = std::sync::Arc::clone(&kept);
                    let answer = f(t);
                    async move {
                        let answer = answer.await;
                        if let Ok(Some(actions)) = &answer
                            && !actions.is_empty()
                            && !actions_answer(actions, true)
                        {
                            kept.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push((server_name, actions.clone()));
                        }
                        answer
                    }
                };
                let result = dispatch_preferred(
                    &ctx.document,
                    pool.clone(),
                    f,
                    |opt| matches!(opt, Some(v) if actions_answer(v, true)),
                    None,
                )
                .await;
                if matches!(result, FanInResult::NoResult { .. }) {
                    let arrived = std::mem::take(
                        &mut *passed_over
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                    if let Some(actions) = preferred_among(&ctx.document, arrived) {
                        return Ok(Some(actions));
                    }
                }
                result
                    .handle(&self.notifier(), "code action", None, Ok)
                    .await
            }
            AggregationStrategy::Concatenated => {
                dispatch_concatenated(&ctx.document, pool.clone(), f, None, None, None)
                    .await
                    .handle(&self.notifier(), "code action", None, |vecs| {
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
        // The lifetime the text was read under travels with the actions and a
        // reply synchronized under another one is refused: a close and reopen
        // racing the request would otherwise answer for the closed text under
        // the reopened document's incarnation.
        let expected_incarnation = ctx.incarnation;
        let host_revision = crate::lsp::bridge::HostRevision {
            incarnation: expected_incarnation,
            content_version: ctx.content_version,
        };
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
                            revision: Some(host_revision),
                        },
                        METHOD,
                        params,
                        t.upstream_id,
                    )
                    .await?;
                let Some(raw) = raw else {
                    return Ok(None);
                };
                if raw.incarnation != expected_incarnation {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "textDocument/codeAction (host): {} was reopened while {} answered; discarding actions computed on the closed text",
                        t.uri,
                        t.server_name
                    );
                    return Ok(None);
                }
                let answering = raw.handle;
                let connection_key = answering.key().clone();
                let Some(actions) = parse_code_actions_leniently(raw.value) else {
                    return Ok(None);
                };
                // Whether this host server advertises `codeAction/resolve`, so a
                // host lazy action can be enveloped for resolve-routing back to
                // it (#627) rather than disabled. Read off the connection that
                // ANSWERED — which is both free (no re-resolution, so no marker
                // walk or pool lock on this hot `textDocument/codeAction` path,
                // which some editors fire on cursor hold) and more accurate than
                // a fresh lookup, since a re-resolve could answer for a
                // replacement spawned since these actions were produced.
                //
                // The two cheap terms still short-circuit ahead of it, because
                // they also say the answer cannot change the outcome:
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
                    && answering.has_capability("codeAction/resolve");
                Ok(Some(bridge_code_actions(
                    actions,
                    &connection_key,
                    t.uri.as_str(),
                    upstream_caps,
                    server_resolves,
                    None,
                    Some(host_revision),
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
                self.host_layer_result(fan_in, &ctx, METHOD, |v| v).await
            }
            AggregationStrategy::Concatenated => {
                let fan_in =
                    dispatch_host_concatenated(&ctx, pool.clone(), f, cancel_rx, None, None).await;
                self.host_layer_result(fan_in, &ctx, METHOD, concat_merge)
                    .await
            }
        }
    }
}

fn code_action_resolve_failed() -> tower_lsp_server::jsonrpc::Error {
    tower_lsp_server::jsonrpc::Error {
        code: tower_lsp_server::jsonrpc::ErrorCode::ServerError(
            tower_lsp_server::ls_types::error_codes::REQUEST_FAILED,
        ),
        message: "Could not resolve this code action. Request code actions again.".into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Command;

    /// A region's actions judged current before another region's were
    /// waited on (held for its prepare answer) are judged again once that
    /// wait ends: the host edited during it, or the first region
    /// re-prepared to other coordinates (an epoch move, no edit), the first
    /// region's are not kept. Every part waits within one prepare timeout.
    #[tokio::test(start_paused = true)]
    async fn actions_judged_before_another_regions_wait_are_judged_again_after_it() {
        use crate::lsp::bridge::{VirtualLayout, apply_prepare_result};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
        let dedented = |text: &str| {
            let answer = serde_json::from_value(serde_json::json!({"segments": [{
                "type": "content", "changes": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
                    "newText": ""
                }]
            }]}))
            .unwrap();
            apply_prepare_result(text, &VirtualLayout::single(text), Some(answer)).unwrap()
        };
        let prepared = Arc::new(dedented("  red\n"));
        // What happens while the held regions' part waits: the host edited,
        // the first region re-prepared and the held one answered, or
        // nothing at all.
        for case in ["edited", "re-prepared", "never answered"] {
            let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::preparing());
            // The prepare layer's table, which holds are kept in.
            pool.prepare_failures_for_test();
            let host = url::Url::parse("file:///test/regions.md").unwrap();
            let tracker = crate::language::NodeTracker::new();
            tracker.open_incarnation(&host, 1);
            let regions: Vec<String> = [(0, 6), (10, 16), (20, 26)]
                .into_iter()
                .map(|(start, end)| {
                    tracker
                        .get_or_create_for_incarnation(&host, start, end, "block", 1)
                        .unwrap()
                        .to_string()
                })
                .collect();
            pool.open_host_incarnation(&host, 1).await;
            for region in &regions {
                pool.record_latest_prepared_for_test(&host, 1, "lua", region, &prepared);
            }
            // The second and third held for their answers.
            for region in &regions[1..] {
                pool.hold_latest_virtual_content(&host, 1, "lua", region);
            }
            let part = |region: &str| {
                (
                    vec![CodeActionOrCommand::CodeAction(CodeAction {
                        title: region.into(),
                        ..Default::default()
                    })],
                    Some(
                        crate::lsp::lsp_impl::bridge_context::Sourced::prepared_for_test(
                            (),
                            &host,
                            "lua",
                            region,
                            &prepared.map,
                            1,
                            1,
                        ),
                    ),
                )
            };
            let version = Arc::new(AtomicU64::new(1));
            let during = {
                let (pool, host, version, regions, prepared) = (
                    Arc::clone(&pool),
                    host.clone(),
                    Arc::clone(&version),
                    regions.clone(),
                    Arc::clone(&prepared),
                );
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    match case {
                        "edited" => version.store(2, Ordering::SeqCst),
                        "re-prepared" => {
                            let other = dedented("  blue\n");
                            pool.record_latest_prepared_for_test(
                                &host,
                                1,
                                "lua",
                                &regions[0],
                                &other,
                            );
                            for region in &regions[1..] {
                                pool.record_latest_prepared_for_test(
                                    &host, 1, "lua", region, &prepared,
                                );
                            }
                        }
                        _ => {}
                    }
                })
            };
            let started = tokio::time::Instant::now();
            let answer = judged_actions(
                &pool,
                &tracker,
                || Some(version.load(Ordering::SeqCst)),
                regions.iter().map(|region| part(region)).collect(),
            )
            .await;
            during.await.unwrap();
            let titles = answer.map(|actions| {
                actions
                    .into_iter()
                    .map(|action| match action {
                        CodeActionOrCommand::CodeAction(action) => action.title,
                        CodeActionOrCommand::Command(command) => command.title,
                    })
                    .collect::<Vec<_>>()
            });
            match case {
                "edited" => assert_eq!(titles, Err(()), "{case}"),
                "re-prepared" => assert_eq!(titles, Ok(regions[1..].to_vec()), "{case}"),
                // The held ones never current: the first's stay.
                _ => assert_eq!(titles, Ok(regions[..1].to_vec()), "{case}"),
            }
            // One wait for all, however many parts wait.
            assert!(
                started.elapsed()
                    <= crate::lsp::bridge::PREPARE_TIMEOUT + std::time::Duration::from_millis(50),
                "{case}: waited {:?}",
                started.elapsed()
            );
        }
    }

    /// Under `preferred`, a prepared document's actions whose edits the map
    /// refused do not win over a server ranked below; one its server
    /// disabled does, as an unprepared one's answer counts whatever it
    /// holds, as on main.
    #[test]
    fn only_actions_the_map_refused_give_way_on_a_prepared_document() {
        let server_disabled = CodeActionOrCommand::CodeAction(CodeAction {
            title: "not here".into(),
            disabled: Some(tower_lsp_server::ls_types::CodeActionDisabled {
                reason: "the server says no".into(),
            }),
            ..Default::default()
        });
        assert!(actions_answer(std::slice::from_ref(&server_disabled), true));
        let disabled_action = || {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: "refused".into(),
                disabled: Some(crate::lsp::bridge::prepared_map_refusal_for_test()),
                ..Default::default()
            })
        };
        let disabled = disabled_action();
        let enabled = CodeActionOrCommand::CodeAction(CodeAction {
            title: "fix".into(),
            ..Default::default()
        });
        assert!(!actions_answer(std::slice::from_ref(&disabled), true));
        assert!(actions_answer(std::slice::from_ref(&disabled), false));
        assert!(actions_answer(&[disabled, enabled], true));
        assert!(!actions_answer(&[], false));
        // Across layers, by each part's source.
        let source = || Some(crate::lsp::lsp_impl::bridge_context::Sourced::unprepared(()));
        assert!(!parts_answer(
            &vec![(vec![disabled_action()], source())],
            |_| true
        ));
        assert!(parts_answer(&vec![(vec![disabled_action()], None)], |_| {
            true
        }));
        assert!(parts_answer(
            &vec![
                (vec![disabled_action()], source()),
                (vec![], None),
                (
                    vec![CodeActionOrCommand::CodeAction(CodeAction::default())],
                    source()
                ),
            ],
            |_| true
        ));
    }

    /// Under a cross-layer `preferred`, a prepared region's actions that may
    /// no longer be kept (moved, stale) are no answer of the virt layer: the
    /// race falls through to the host layer's, waited for when it is still
    /// running. Ones that may be (current, or settling and waited on after)
    /// win as ever.
    #[tokio::test]
    async fn actions_that_may_not_be_kept_fall_through_to_the_host_layers() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        use crate::lsp::lsp_impl::bridge_context::race_layers_preferred;
        let action = |title: &str| {
            vec![CodeActionOrCommand::CodeAction(CodeAction {
                title: title.into(),
                ..Default::default()
            })]
        };
        let prepared_source = || {
            let host = url::Url::parse("file:///test/race.md").unwrap();
            let map = crate::lsp::bridge::apply_prepare_result(
                "a",
                &crate::lsp::bridge::VirtualLayout::single("a"),
                None,
            )
            .unwrap()
            .map;
            Some(
                crate::lsp::lsp_impl::bridge_context::Sourced::prepared_for_test(
                    (),
                    &host,
                    "lua",
                    "r",
                    &map,
                    1,
                    0,
                ),
            )
        };
        for (may_keep, host_delay_ms) in [(true, 0), (false, 0), (false, 20)] {
            let virt: ActionParts = vec![(action("virt"), prepared_source())];
            let host = async move {
                tokio::time::sleep(std::time::Duration::from_millis(host_delay_ms)).await;
                Ok(Some(vec![(action("host"), None)]))
            };
            let won = race_layers_preferred(
                &[Virt, Host, Native],
                std::future::ready(Ok(Some(virt))),
                host,
                std::future::ready(Ok(None)),
                |parts: &ActionParts| parts_answer(parts, |_| may_keep),
            )
            .await
            .unwrap()
            .unwrap();
            let titles: Vec<_> = won
                .iter()
                .flat_map(|(actions, _)| actions)
                .map(|action| match action {
                    CodeActionOrCommand::CodeAction(action) => action.title.clone(),
                    CodeActionOrCommand::Command(command) => command.title.clone(),
                })
                .collect();
            assert_eq!(
                titles,
                [if may_keep { "virt" } else { "host" }],
                "{may_keep} {host_delay_ms}"
            );
        }
        // An unprepared part is not read.
        assert!(parts_answer(
            &vec![(action("as is"), None)],
            |_| unreachable!()
        ));
    }

    /// Under a cross-layer `preferred`, a prepared region's answer of only
    /// disabled actions does not win over a layer ranked below that answers,
    /// but is the walk's answer when none does (nothing, or an error): a
    /// client showing disabled actions shows why the fix was refused. A
    /// cancel stays a cancel.
    #[tokio::test]
    async fn a_prepared_answer_of_refused_actions_answers_when_nothing_else_does() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        use crate::lsp::lsp_impl::bridge_context::race_layers_preferred;
        use tower_lsp_server::jsonrpc::{Error, ErrorCode};
        let action = |title: &str, disabled: bool| {
            vec![CodeActionOrCommand::CodeAction(CodeAction {
                title: title.into(),
                disabled: disabled.then(crate::lsp::bridge::prepared_map_refusal_for_test),
                ..Default::default()
            })]
        };
        let host_uri = url::Url::parse("file:///test/refused.md").unwrap();
        let map = crate::lsp::bridge::apply_prepare_result(
            "a",
            &crate::lsp::bridge::VirtualLayout::single("a"),
            None,
        )
        .unwrap()
        .map;
        let refused = || -> ActionParts {
            vec![(
                action("refused", true),
                Some(
                    crate::lsp::lsp_impl::bridge_context::Sourced::prepared_for_test(
                        (),
                        &host_uri,
                        "lua",
                        "r",
                        &map,
                        1,
                        0,
                    ),
                ),
            )]
        };
        let titles = |result: Result<Option<ActionParts>>| {
            result.map(|parts| {
                parts
                    .into_iter()
                    .flatten()
                    .flat_map(|(actions, _)| actions)
                    .map(|action| match action {
                        CodeActionOrCommand::CodeAction(action) => action.title,
                        CodeActionOrCommand::Command(command) => command.title,
                    })
                    .collect::<Vec<_>>()
            })
        };
        let walk = |host: Result<Option<ActionParts>>| {
            let passed_over = PassedOver::default();
            let virt = refused();
            async move {
                let result = race_layers_preferred(
                    &[Virt, Host, Native],
                    std::future::ready(Ok(Some(virt))),
                    std::future::ready(host),
                    std::future::ready(Ok(None)),
                    |parts: &ActionParts| passed_over.answers(parts, |_| true),
                )
                .await;
                passed_over.or_kept(result).0
            }
        };
        // The host layer answering: its actions.
        assert_eq!(
            titles(walk(Ok(Some(vec![(action("host", false), None)]))).await),
            Ok(vec!["host".to_string()])
        );
        // Nothing else, or only an error: the refused ones.
        assert_eq!(
            titles(walk(Ok(None)).await),
            Ok(vec!["refused".to_string()])
        );
        assert_eq!(
            titles(walk(Err(Error::internal_error())).await),
            Ok(vec!["refused".to_string()])
        );
        // Cancelled: cancelled.
        assert_eq!(
            walk(Err(Error::request_cancelled()))
                .await
                .map(|_| ())
                .unwrap_err()
                .code,
            ErrorCode::RequestCancelled
        );
        // Nothing kept, the walk's result as it was.
        assert_eq!(
            titles(PassedOver::default().or_kept(Ok(None)).0),
            Ok(vec![])
        );
    }

    /// A virt answer passed over as moved or stale is not kept aside: with
    /// the host layer erring, the walk answers that error, not actions the
    /// final check drops. Kept-aside actions dropped there leave the error
    /// they displaced.
    #[tokio::test]
    async fn a_stale_answer_passed_over_never_displaces_a_layers_error() {
        use crate::config::settings::LayerSource::{Host, Native, Virt};
        use crate::lsp::lsp_impl::bridge_context::race_layers_preferred;
        use tower_lsp_server::jsonrpc::Error;
        let host_uri = url::Url::parse("file:///test/stale-error.md").unwrap();
        let map = crate::lsp::bridge::apply_prepare_result(
            "a",
            &crate::lsp::bridge::VirtualLayout::single("a"),
            None,
        )
        .unwrap()
        .map;
        let virt: ActionParts = vec![(
            vec![CodeActionOrCommand::CodeAction(CodeAction {
                title: "stale".into(),
                ..Default::default()
            })],
            Some(
                crate::lsp::lsp_impl::bridge_context::Sourced::prepared_for_test(
                    (),
                    &host_uri,
                    "lua",
                    "r",
                    &map,
                    1,
                    0,
                ),
            ),
        )];
        let passed_over = PassedOver::default();
        let result = race_layers_preferred(
            &[Virt, Host, Native],
            std::future::ready(Ok(Some(virt))),
            std::future::ready(Err(Error::internal_error())),
            std::future::ready(Ok(None)),
            |parts: &ActionParts| passed_over.answers(parts, |_| false),
        )
        .await;
        let (result, displaced) = passed_over.or_kept(result);
        assert_eq!(result.map(|_| ()).unwrap_err(), Error::internal_error());
        assert!(displaced.is_none());
        // Kept aside, then dropped: the error they displaced.
        assert_eq!(
            judged_or_displaced(Err(()), Some(Error::internal_error())).unwrap_err(),
            Error::internal_error()
        );
        assert_eq!(judged_or_displaced(Err(()), None).unwrap(), None);
        assert_eq!(judged_or_displaced(Ok(vec![]), None).unwrap(), Some(vec![]));
    }

    /// The final check drops only a stale prepared region's actions; an
    /// answer that came empty stays the empty answer it was.
    #[test]
    fn the_final_check_drops_only_stale_regions_actions() {
        let action = |title: &str| {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: title.into(),
                ..Default::default()
            })
        };
        let titles = |answer: std::result::Result<CodeActionResponse, ()>| {
            answer.map(|actions| {
                actions
                    .into_iter()
                    .map(|action| match action {
                        CodeActionOrCommand::CodeAction(action) => action.title,
                        CodeActionOrCommand::Command(command) => command.title,
                    })
                    .collect::<Vec<_>>()
            })
        };
        // A host layer answering `[]`, nothing else: `[]`.
        assert_eq!(titles(current_actions(vec![(vec![], true)])), Ok(vec![]));
        // A stale region beside the host layer's actions: those stay.
        assert_eq!(
            titles(current_actions(vec![
                (vec![action("virt")], false),
                (vec![action("host")], true)
            ])),
            Ok(vec!["host".to_string()])
        );
        // Only stale actions: none.
        assert_eq!(
            titles(current_actions(vec![(vec![action("virt")], false)])),
            Err(())
        );
    }

    /// A region's actions answered before the host's last edit (to another
    /// fence, the region's text unchanged) settled are kept once it
    /// settles, not dropped; those the host moved under are dropped.
    #[tokio::test(start_paused = true)]
    async fn actions_answered_before_the_hosts_last_edit_settles_are_judged_once_it_does() {
        use crate::lsp::bridge::{VirtualLayout, apply_prepare_result};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};
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
        for moves in [false, true] {
            let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::preparing());
            let host = url::Url::parse("file:///test/settle.md").unwrap();
            let tracker = crate::language::NodeTracker::new();
            tracker.open_incarnation(&host, 1);
            let region = tracker
                .get_or_create_for_incarnation(&host, 0, 6, "block", 1)
                .unwrap()
                .to_string();
            pool.open_host_incarnation(&host, 1).await;
            pool.record_latest_prepared_for_test(&host, 1, "lua", &region, &prepared);
            pool.note_host_edited(&host, 1, 1);
            let source = crate::lsp::lsp_impl::bridge_context::Sourced::prepared_for_test(
                (),
                &host,
                "lua",
                &region,
                &prepared.map,
                1,
                1,
            );
            let version = Arc::new(AtomicU64::new(1));
            let settle = {
                let (pool, host, version) = (Arc::clone(&pool), host.clone(), Arc::clone(&version));
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    if moves {
                        version.store(2, Ordering::SeqCst);
                    } else {
                        pool.note_host_settled(&host, 1, 1);
                    }
                })
            };
            let answer = judged_actions(
                &pool,
                &tracker,
                || Some(version.load(Ordering::SeqCst)),
                vec![(
                    vec![CodeActionOrCommand::CodeAction(CodeAction {
                        title: "fix".into(),
                        ..Default::default()
                    })],
                    Some(source),
                )],
            )
            .await;
            settle.await.unwrap();
            assert_eq!(
                answer.map(|actions| actions.len()),
                if moves { Err(()) } else { Ok(1) }
            );
        }
    }

    /// An action resolved while its region went from as is to prepared (an
    /// epoch move gaining its pair a peer) is refused: its edit was computed
    /// for the prepared text, which the envelope's offset does not map.
    #[tokio::test]
    async fn an_action_resolved_as_is_is_refused_once_its_region_is_prepared() {
        use std::sync::Arc;
        let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::preparing());
        let (service, _socket) = tower_lsp_server::LspService::new(|client| {
            crate::lsp::lsp_impl::Kakehashi::with_cancel_forwarder(
                client,
                Arc::clone(&pool),
                crate::lsp::CancelForwarder::new(Arc::clone(&pool)),
            )
        });
        let server = service.inner();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), language.clone());
        server.language.query_store().insert_injection_query(
            "rust".into(),
            Arc::new(
                tree_sitter::Query::new(
                    &language,
                    r#"((string_literal) @injection.content (#set! injection.language "rust"))"#,
                )
                .unwrap(),
            ),
        );
        server
            .bridge
            .prepare_settings_changed(&Arc::new(crate::config::WorkspaceSettings {
                auto_install: false,
                language_servers: [(
                    "peer".to_string(),
                    crate::lsp::bridge::test_helpers::devnull_config_for_language("rust"),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            }));
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let uri = url::Url::parse("file:///hinted.rs").unwrap();
        let text = r#"fn main() { let x = "a"; }"#;
        server.documents.insert(
            uri.clone(),
            text.into(),
            Some("rust".into()),
            parser.parse(text, None),
        );
        let region = server
            .injection_coordinator()
            .resolve_injection_data(&uri, "rust")
            .unwrap()
            .remove(0);
        let mut envelope: crate::lsp::bridge::CodeActionEnvelope =
            serde_json::from_value(serde_json::json!({
                "origin": "rust-ls",
                "host_uri": uri.to_string(),
                "region_id": region.region_id,
                "injection_language": "rust",
                "offset": { "line": 0, "column": 0 },
                "original_title": "fix",
                "inner": null
            }))
            .unwrap();

        let (incarnation, content_version) = server
            .documents
            .get(&uri)
            .map(|document| (document.incarnation(), document.content_version()))
            .unwrap();
        envelope.incarnation = Some(incarnation);
        envelope.content_version = Some(content_version);
        server
            .bridge
            .pool_arc()
            .open_host_incarnation(&uri, incarnation)
            .await;

        // As is: its resolved edit surfaces.
        assert!(server.code_action_response_is_current(&uri, &envelope));

        // Prepared since (the pair gained its peer): not rebuilt.
        let target = server.bridge.prepare_target("rust", "rust").unwrap();
        let answer =
            serde_json::from_value(serde_json::json!({"segments": [{"type": "content"}]})).unwrap();
        server.bridge.answer_prepare_for_test(
            &target,
            crate::lsp::bridge::PrepareInput {
                host_uri: &uri,
                host_language: "rust",
                injection_language: "rust",
                region_id: &region.region_id,
                virtual_text: &region.content,
                gaps: &[],
            },
            crate::lsp::bridge::PrepareAnswer::Prepared(Arc::new(
                crate::lsp::bridge::apply_prepare_result(
                    &region.content,
                    &crate::lsp::bridge::VirtualLayout::single(&region.content),
                    Some(answer),
                )
                .unwrap(),
            )),
            true,
        );
        // Prepared while resolving: the edit is in the prepared text's
        // coordinates, which the envelope's offset does not map.
        assert!(!server.code_action_response_is_current(&uri, &envelope));
    }

    #[rstest::rstest]
    #[case::removed_server("server")]
    #[case::edited_document("edit")]
    #[case::reopened_document("reopen")]
    #[tokio::test]
    async fn lazy_resolve_failure_is_reported_to_the_client(#[case] failure: &str) {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///resolve-failure.rs").unwrap();
        let incarnation = server.documents.insert(
            uri.clone(),
            "fn main() {}".into(),
            Some("rust".into()),
            None,
        );
        server
            .bridge
            .pool_arc()
            .open_host_incarnation(&uri, incarnation)
            .await;
        let action: CodeAction = serde_json::from_value(serde_json::json!({
            "title": "Fix it — removed-server",
            "data": { "kakehashi": {
                "origin": "removed-server", "host_uri": uri.as_str(),
                "region_id": "", "injection_language": "",
                "offset": { "line": 0, "column": 0, "line_column_offsets": [] },
                "original_title": "Fix it", "inner": { "id": 7 },
                "content_version": 0, "incarnation": incarnation, "host_layer": true
            }}
        }))
        .unwrap();
        assert!(extract_code_action_envelope(&action).is_some());
        if failure == "edit" {
            server
                .documents
                .update_document(uri.clone(), "fn updated() {}".into(), None);
        } else if failure == "reopen" {
            let newer = server.documents.insert(
                uri.clone(),
                "fn main() {}".into(),
                Some("rust".into()),
                None,
            );
            server
                .bridge
                .pool_arc()
                .open_host_incarnation(&uri, newer)
                .await;
        }
        let error = server
            .code_action_resolve_impl(action)
            .await
            .expect_err("an unavailable action must not be returned as a successful no-op");
        assert_eq!(
            error.code,
            tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32803)
        );
        assert!(error.message.contains("Request code actions again"));
    }

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
