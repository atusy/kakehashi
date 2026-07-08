//! Code action method for Kakehashi (#568). Edit-carrying and lazy actions
//! are both bridged; `code_action_resolve_impl` routes `codeAction/resolve`
//! back to the origin server via the `data` envelope.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the requested range, the host layer
//! (host-document-bridge) bridges the host document itself. Both apply the
//! `"{title} — {server}"` suffix, so the host arm cannot use the generic
//! verbatim raw-value walk — it dispatches typed per server to keep the
//! server name ([`Kakehashi::walk_layer_futures`]).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionContext, CodeActionParams, CodeActionResponse, MessageType,
    NumberOrString, Position, Range, Uri,
};
use ulid::Ulid;
use url::Url;

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

/// Flatten the per-server results of a `concatenated` dispatch into one response
/// in priority order (`dispatch_concatenated` yields them ordered). Each server
/// contributes `Option<CodeActionResponse>`; drop the `None`s, concatenate the
/// action lists, and collapse an all-empty merge to `None` so the layer counts
/// as "no result" for cross-layer aggregation (#568 PR 7).
fn concat_merge(vecs: Vec<Option<CodeActionResponse>>) -> Option<CodeActionResponse> {
    let merged: CodeActionResponse = vecs.into_iter().flatten().flatten().collect();
    (!merged.is_empty()).then_some(merged)
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
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |actions: &CodeActionResponse| !actions.is_empty(),
        )
        .await
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
        let Some(region_end) = self.code_action_region_end_if_fresh(&envelope) else {
            log::debug!(
                target: "kakehashi::bridge",
                "codeAction/resolve: region {} is stale; returning action unresolved",
                envelope.region_id
            );
            return Ok(action);
        };

        let settings = self.settings_manager.load_settings();
        let upstream_caps = self.upstream_code_action_caps();
        let upstream_id = crate::lsp::current_upstream_id();
        let pool = self.bridge.pool_arc();
        Ok(pool
            .dispatch_code_action_resolve(action, &settings, upstream_caps, upstream_id, region_end)
            .await)
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
    fn code_action_region_end_if_fresh(&self, envelope: &CodeActionEnvelope) -> Option<Position> {
        let host_url = Url::parse(&envelope.host_uri).ok()?;
        let ulid = envelope.region_id.parse::<Ulid>().ok()?;
        // Re-resolve the region from the LIVE parse (same construction as the
        // goto/showDocument offset path), yielding the current per-line offset
        // AND the content-precise host byte range.
        let (start_byte, _end, _kind, _layer) =
            self.bridge.node_tracker().lookup_node(&host_url, &ulid)?;
        let snapshot = self.documents.get(&host_url)?.snapshot()?;
        let language_name = detect_document_language(&self.language, &self.documents, &host_url)?;
        let injection_query = self.language.injection_query(&language_name)?;
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &self.language,
            self.bridge.node_tracker(),
            &host_url,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
            start_byte,
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
        let Some(mut ctx) = self
            .resolve_bridge_contexts_for_range_overlap(lsp_uri, range, METHOD)
            .await
        else {
            return Ok(None);
        };
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        // One fan-out closure, dispatched by the within-layer strategy
        // (`bridge.<lang>.aggregation.<method>.strategy`): `preferred` returns
        // the highest-priority non-empty server; `concatenated` merges every
        // server's actions in priority order (#568 PR 7). The closure moves into
        // exactly one arm.
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
        let result = match ctx.document.strategy {
            AggregationStrategy::Preferred => {
                dispatch_preferred(
                    &ctx.document,
                    pool.clone(),
                    f,
                    |opt| matches!(opt, Some(v) if !v.is_empty()),
                    cancel_rx,
                )
                .await
                .handle(&self.client, "code action", None, Ok)
                .await
            }
            AggregationStrategy::Concatenated => {
                dispatch_concatenated(&ctx.document, pool.clone(), f, cancel_rx, None, None)
                    .await
                    .handle(&self.client, "code action", None, |vecs| {
                        Ok(concat_merge(vecs))
                    })
                    .await
            }
        };
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result
    }

    /// Host layer: forward the params verbatim to the host language's own
    /// servers, then apply the bridge action policy (suffix, command
    /// disabling) per winning server. Edits stay verbatim — real URIs and
    /// coordinates need no translation.
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
                Ok(raw.and_then(|value| {
                    let actions = parse_code_actions_leniently(value)?;
                    Some(bridge_code_actions(
                        actions,
                        &t.server_name,
                        t.uri.as_str(),
                        upstream_caps,
                        // Host-layer codeAction/resolve is not bridged, so
                        // host lazy actions are never enveloped/resolved.
                        false,
                        None,
                    ))
                }))
            }
        };
        let result = match ctx.strategy {
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
        };
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
        result
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
