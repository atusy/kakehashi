//! Code action request handling for bridge connections (#568).
//!
//! Edit-carrying and lazy (resolve-deferred) actions are both bridged:
//! `codeAction/resolve` is routed back to the origin server via the
//! `CodeActionEnvelope` in `CodeAction.data` (PR 4), or eagerly resolved
//! downstream when the upstream client lacks `dataSupport`/`resolveSupport`.
//! Only `Command` execution remains unbridged — command-carrying actions
//! surface as `disabled: { reason }` when the client supports it and are
//! dropped otherwise (LSP 3.16 `disabledSupport`).
//!
//! Every bridged action title gets the `"{title} — {server}"` suffix so
//! users can see which downstream server each action comes from.

use std::io;
use std::sync::Arc;

use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::{BridgeServerConfig, WorkspaceSettings};
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::text::PositionMapper;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionContext, CodeActionDisabled, CodeActionOrCommand, CodeActionParams,
    CodeActionResponse, NumberOrString, PartialResultParams, Position, Range,
    TextDocumentIdentifier, Uri, WorkDoneProgressParams,
};
use url::Url;

use super::super::pool::{ConnectionHandle, LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, host_position_within_region,
    response_has_jsonrpc_error, transform_workspace_edit_to_host, translate_host_range_to_virtual,
    translate_virtual_position_to_host, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use super::completion::EnvelopeOffset;

/// Wrapper key inside `CodeAction.data` that identifies the origin server.
const ENVELOPE_KEY: &str = "kakehashi";

/// Envelope stored in `CodeAction.data` for routing `codeAction/resolve`
/// (#568 PR 4), mirroring [`CodeLensEnvelope`](super::code_lens::CodeLensEnvelope).
///
/// Carries everything resolve-time routing and coordinate translation need:
/// the origin server, the host document + region the action came from, the
/// injection language (to reconstruct the virtual URI for edit re-keying), the
/// offset snapshot, the action's ORIGINAL (unsuffixed) title (servers may
/// match a resolve request by title/content, so it must be restored before
/// forwarding), and the downstream's own `data` preserved verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CodeActionEnvelope {
    /// Server name identifying which downstream produced the action.
    pub(crate) origin: String,
    /// Host document URI the action belongs to (resolve params carry no
    /// textDocument, so the envelope must).
    pub(crate) host_uri: String,
    /// ULID of the injection region the action came from; the resolve handler's
    /// freshness gate looks it up in the node tracker (fail-soft when stale).
    pub(crate) region_id: String,
    /// Injection language of the region — needed together with `region_id` +
    /// `host_uri` to rebuild the virtual URI so a resolved edit's `changes`
    /// map can be re-keyed to the host document.
    pub(crate) injection_language: String,
    /// Region offset snapshot for coordinate translation at resolve time.
    pub(crate) offset: EnvelopeOffset,
    /// The action's original (unsuffixed) title, restored before forwarding a
    /// resolve so a server matching by title still matches.
    pub(crate) original_title: String,
    /// The downstream server's original `data` value (preserved verbatim).
    pub(crate) inner: Option<Value>,
}

/// Everything needed to wrap an action's `data` in a routing envelope.
pub(crate) struct CodeActionEnvelopeContext<'a> {
    server_name: &'a str,
    host_uri: &'a str,
    region_id: &'a str,
    injection_language: &'a str,
    offset: &'a RegionOffset,
}

/// Wrap `action.data` in a Kakehashi envelope for origin tracking, capturing
/// the CURRENT (unsuffixed) title as `original_title`. Call before suffixing.
fn envelope_action_data(action: &mut CodeAction, ctx: &CodeActionEnvelopeContext) {
    let inner = action.data.take();
    let envelope = CodeActionEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        injection_language: ctx.injection_language.to_string(),
        offset: EnvelopeOffset::from(ctx.offset),
        original_title: action.title.clone(),
        inner,
    };
    action.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

/// Extract the envelope from an action's `data` without modifying the action.
pub(crate) fn extract_code_action_envelope(action: &CodeAction) -> Option<CodeActionEnvelope> {
    let data = action.data.as_ref()?;
    let wrapper = data.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

/// Extract the envelope and restore the downstream's original `data` value.
///
/// On success, `action.data` is set back to `inner` (the action's title is
/// left suffixed — the dispatch path restores `original_title` explicitly
/// before forwarding). Returns `None` if not an envelope (action unchanged).
fn strip_code_action_envelope(action: &mut CodeAction) -> Option<CodeActionEnvelope> {
    let mut envelope = extract_code_action_envelope(action)?;
    action.data = envelope.inner.take();
    Some(envelope)
}

/// Restore the envelope into an action's `data` field (fail-soft return path).
///
/// The action's CURRENT `data` becomes the new `inner` (mirrors
/// `re_envelope_lens`): `strip` moved the downstream's original data back into
/// `action.data`, and after a resolve the field may hold the server's updated
/// data — either way it is what a subsequent resolve must receive. The
/// `original_title` is taken from the envelope (the action's live title is the
/// suffixed one), so it survives repeated strip/re-envelope cycles.
fn re_envelope_action(action: &mut CodeAction, envelope: &CodeActionEnvelope) {
    let inner = action.data.take();
    action.data = Some(serde_json::json!({
        ENVELOPE_KEY: CodeActionEnvelope {
            origin: envelope.origin.clone(),
            host_uri: envelope.host_uri.clone(),
            region_id: envelope.region_id.clone(),
            injection_language: envelope.injection_language.clone(),
            offset: envelope.offset.clone(),
            original_title: envelope.original_title.clone(),
            inner,
        }
    }));
}

/// The exclusive host-document end of an injection region: the position just
/// past its `virtual_content`, mapped back to host coordinates. Bounds the
/// in-region diagnostic filter position-precisely (line AND column), so a
/// diagnostic after an inline same-line injection is dropped, not leaked.
fn region_host_end(virtual_content: &str, offset: &RegionOffset) -> Position {
    let mut end = PositionMapper::new(virtual_content)
        .byte_to_position(virtual_content.len())
        .unwrap_or(Position {
            line: 0,
            character: 0,
        });
    translate_virtual_position_to_host(&mut end, offset);
    end
}

impl LanguageServerPool {
    /// Send a code action request and wait for the response.
    ///
    /// The request's range and `context.diagnostics` ranges are translated
    /// host→virtual (diagnostic `data`/`source`/`code` stay byte-identical so
    /// the downstream can match them against what it published); the
    /// response's edits and action diagnostics are translated back.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_code_action_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_range: Range,
        context: CodeActionContext,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> io::Result<Option<CodeActionResponse>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/codeAction") {
            return Ok(None);
        }

        // Phase 1: send the request and parse the raw actions (still in virtual
        // coordinates, no policy applied) — the bridge policy is deferred to
        // phase 3 so phase 2 can eager-resolve lazy actions asynchronously.
        let region_end = region_host_end(virtual_content, &offset);
        let raw = self
            .execute_bridge_request_with_handle(
                Arc::clone(&handle),
                host_uri,
                injection_language,
                region_id,
                &offset,
                virtual_content,
                upstream_request_id.clone(),
                |virtual_uri, request_id| {
                    build_code_action_request(
                        virtual_uri,
                        host_range,
                        context,
                        &offset,
                        region_end,
                        request_id,
                        client_progress_token,
                    )
                },
                |response, _ctx| parse_code_action_response_raw(response),
            )
            .await?;
        let Some(mut actions) = raw else {
            return Ok(None);
        };

        // Phase 2: eager-resolve fallback. A client without resolve+data
        // support can never complete a lazy action and may strip our routing
        // envelope, so resolve those actions downstream now (bounded: only the
        // lazy ones). Failures fall through to the phase-3 REASON_RESOLVE path.
        if !upstream_caps.can_envelope() {
            self.eager_resolve_lazy_actions(&handle, &mut actions, upstream_request_id)
                .await;
        }

        // Phase 3: apply the bridge policy (coordinate translation, title
        // suffix, disable/drop, and — for envelope-capable clients — the
        // resolve routing envelope).
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(host_uri)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let virtual_uri_string =
            VirtualDocumentUri::new(&host_uri_lsp, injection_language, region_id).to_uri_string();
        let virt = VirtLayerContext {
            request_virtual_uri: &virtual_uri_string,
            host_uri: &host_uri_lsp,
            offset: &offset,
            region_id,
            injection_language,
            host_uri_string: host_uri.as_str(),
            server_name,
        };
        Ok(Some(bridge_code_actions(
            actions,
            server_name,
            upstream_caps,
            handle.has_capability("codeAction/resolve"),
            Some(&virt),
        )))
    }

    /// Eager-resolve the lazy (data-only) actions in place: for a client that
    /// cannot resolve them itself, materialize their `edit`/`command` via a
    /// `codeAction/resolve` round-trip on the SAME connection while it is still
    /// live. Actions that fail to resolve stay lazy and are disabled/dropped by
    /// the phase-3 policy. Only lazy actions are touched (bounded fan-out).
    async fn eager_resolve_lazy_actions(
        &self,
        handle: &Arc<ConnectionHandle>,
        actions: &mut [CodeActionOrCommand],
        upstream_id: Option<UpstreamId>,
    ) {
        if !handle.has_capability("codeAction/resolve") {
            return;
        }
        for item in actions.iter_mut() {
            let CodeActionOrCommand::CodeAction(action) = item else {
                continue;
            };
            // Lazy = no edit/command, not server-disabled. `data` is NOT
            // required: this loop already gated on the server advertising
            // resolve, so a title-only action here is a resolvable lazy action
            // (LSP 3.18), not a no-op.
            let is_lazy =
                action.edit.is_none() && action.command.is_none() && action.disabled.is_none();
            if !is_lazy {
                continue;
            }
            if let Some(resolved) = self
                .send_code_action_resolve_on_handle(handle, action.clone(), upstream_id.clone())
                .await
            {
                *action = resolved;
            }
        }
    }

    /// Route a `codeAction/resolve` request to the origin downstream server,
    /// identified by the envelope in `action.data` (#568 PR 4). An action
    /// without an envelope (host-layer or foreign) passes through unchanged.
    /// Fails soft at every step: any failure returns the action unresolved
    /// with its envelope restored (mirrors `dispatch_code_lens_resolve`).
    pub(crate) async fn dispatch_code_action_resolve(
        &self,
        mut action: CodeAction,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> CodeAction {
        let Some(envelope) = strip_code_action_envelope(&mut action) else {
            return action;
        };

        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            re_envelope_action(&mut action, &envelope);
            return action;
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        ) else {
            re_envelope_action(&mut action, &envelope);
            return action;
        };

        self.send_code_action_resolve_request(&config, action, envelope, upstream_id)
            .await
    }

    /// Reconnect to the origin `(server, root)`, restore the original title,
    /// translate coordinates back to virtual, forward `codeAction/resolve`,
    /// then translate the resolved edit/diagnostics host-ward, re-suffix, and
    /// re-envelope. Every failure path returns the action unresolved.
    async fn send_code_action_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut action: CodeAction,
        envelope: CodeActionEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> CodeAction {
        let server_name = &envelope.origin;
        let host_url = Url::parse(&envelope.host_uri).ok();
        let handle = match self
            .get_or_create_connection(server_name, server_config, host_url.as_ref())
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "codeAction/resolve: failed to connect to {server_name}: {e}"
                );
                re_envelope_action(&mut action, &envelope);
                return action;
            }
        };
        if !handle.has_capability("codeAction/resolve") {
            re_envelope_action(&mut action, &envelope);
            return action;
        }

        let offset = RegionOffset::from(&envelope.offset);
        let host_uri_lsp = host_url
            .as_ref()
            .and_then(|u| crate::lsp::lsp_impl::url_to_uri(u).ok());

        // Forward with the ORIGINAL (unsuffixed) title restored and any
        // client-supplied ranges translated back to virtual coordinates. Keep
        // the suffixed title to re-apply to the resolved response.
        let mut outgoing = action.clone();
        let suffixed_title =
            std::mem::replace(&mut outgoing.title, envelope.original_title.clone());
        translate_action_ranges_host_to_virtual(&mut outgoing, &offset);

        let Some(mut resolved) = self
            .send_code_action_resolve_on_handle(&handle, outgoing, upstream_id)
            .await
        else {
            re_envelope_action(&mut action, &envelope);
            return action;
        };

        // A resolve that materializes a `command` (or edit+command) can't be
        // honored — command execution is unbridged, and applying only the edit
        // half is worse than none (the initial codeAction path disables such
        // actions). Fail soft: return the original action unresolved rather
        // than hand the editor an executable downstream command.
        if resolved.command.is_some() {
            re_envelope_action(&mut action, &envelope);
            return action;
        }

        // Translate the resolved edit host-ward; a cross-region edit that
        // cannot be represented in the host document fails soft (return the
        // original action unresolved rather than an unappliable edit).
        match (resolved.edit.as_mut(), host_uri_lsp.as_ref()) {
            (Some(edit), Some(host_uri_lsp)) => {
                let virtual_uri = VirtualDocumentUri::new(
                    host_uri_lsp,
                    &envelope.injection_language,
                    &envelope.region_id,
                )
                .to_uri_string();
                if !transform_workspace_edit_to_host(edit, &virtual_uri, host_uri_lsp, &offset) {
                    re_envelope_action(&mut action, &envelope);
                    return action;
                }
            }
            // The server resolved an edit, but the host URI couldn't be rebuilt
            // to translate it (a `url`/`Uri` parser divergence). Shipping the
            // untranslated virtual-coordinate edit would be unappliable — fail
            // soft, same as the cross-region case.
            (Some(_), None) => {
                re_envelope_action(&mut action, &envelope);
                return action;
            }
            (None, _) => {}
        }
        if let Some(diagnostics) = &mut resolved.diagnostics {
            for diagnostic in diagnostics {
                translate_virtual_range_to_host(&mut diagnostic.range, &offset);
            }
        }

        // Re-apply the "{title} — {server}" suffix (the server echoed the
        // original title back).
        resolved.title = suffixed_title;

        // Once resolve has materialized an edit, the action is complete and
        // its edit is host-translated. Re-enveloping it would let a second
        // resolve forward that host-coordinate edit back downstream (no
        // inverse transform) — so strip the data instead, mirroring the
        // initial-response policy for edit-carrying actions. Only a still-lazy
        // resolved action (no edit) keeps a routing envelope for a further
        // resolve.
        if resolved.edit.is_some() {
            resolved.data = None;
        } else {
            if resolved.data.is_none() {
                resolved.data = action.data.take();
            }
            re_envelope_action(&mut resolved, &envelope);
        }
        resolved
    }

    /// Send a `codeAction/resolve` request on an already-connected handle and
    /// parse the response into a resolved `CodeAction`. Returns `None` on any
    /// failure (register/send/wait/parse) so callers can fail soft.
    async fn send_code_action_resolve_on_handle(
        &self,
        handle: &Arc<ConnectionHandle>,
        action: CodeAction,
        upstream_id: Option<UpstreamId>,
    ) -> Option<CodeAction> {
        let connection_key = handle.key();
        if let Some(ref id) = upstream_id {
            self.register_upstream_request(id.clone(), connection_key);
        }
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(
                        target: "kakehashi::bridge",
                        "codeAction/resolve: failed to register request: {e}"
                    );
                    if let Some(ref id) = upstream_id {
                        self.unregister_upstream_request(id, connection_key);
                    }
                    return None;
                }
            };

        let request = JsonRpcRequest::new(request_id.as_i64(), "codeAction/resolve", &action);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        if let Err(e) = handle.send_request(request, request_id) {
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: failed to send request: {e}"
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, connection_key);
            }
            return None;
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, connection_key);
        }

        parse_code_action_resolve_response(response.ok()?)
    }
}

/// Translate an action's host-space ranges (diagnostics) back to virtual
/// coordinates before forwarding a `codeAction/resolve`. A lazy action being
/// resolved normally carries no `edit`, so only diagnostic ranges are handled;
/// an inverse edit transform is not implemented (see `transform_workspace_edit_to_host`).
fn translate_action_ranges_host_to_virtual(action: &mut CodeAction, offset: &RegionOffset) {
    if let Some(diagnostics) = &mut action.diagnostics {
        for diagnostic in diagnostics {
            translate_host_range_to_virtual(&mut diagnostic.range, offset);
        }
    }
}

/// Parse a JSON-RPC `codeAction/resolve` response into a `CodeAction`.
/// Returns `None` for errors, null results, and deserialization failures.
fn parse_code_action_resolve_response(mut response: serde_json::Value) -> Option<CodeAction> {
    if response_has_jsonrpc_error(&response, "codeAction/resolve") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    serde_json::from_value(result).ok()
}

/// Build a JSON-RPC code action request for a downstream language server.
///
/// Translates the range AND every `context.diagnostics` range host→virtual;
/// `only` and `triggerKind` pass through untouched (dropping `only` would
/// kill save-time flows like `editor.codeActionsOnSave`).
fn build_code_action_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    mut context: CodeActionContext,
    offset: &RegionOffset,
    region_end: Position,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<CodeActionParams> {
    let mut virtual_range = host_range;
    translate_host_range_to_virtual(&mut virtual_range, offset);
    // Out-of-region diagnostics would translate to virtual coordinates that
    // don't exist: above-region ones saturate to (0,0) and can false-match a
    // different diagnostic at the top of the virtual document; ones past the
    // region land beyond the virtual document. Keep only diagnostics whose
    // WHOLE range fits the region — bounding by the region, NOT the (possibly
    // zero-length, cursor) request range, which would wrongly drop a
    // diagnostic sitting at the cursor.
    //
    // `region_end` is `virtual_content`'s end mapped to host coords, i.e. the
    // valid end-of-content LSP position (one past the last char / start of a
    // trailing empty line). `end <= region_end` is therefore INCLUSIVE on
    // purpose: an end-of-content diagnostic (e.g. "missing semicolon") is
    // real and maps to a valid virtual position. For a well-formed diagnostic
    // (`start <= end`), `end <= region_end` already forces any `start` at
    // `region_end` to be zero-length there — still valid, not a leak.
    context.diagnostics.retain(|diagnostic| {
        host_position_within_region(diagnostic.range.start, offset)
            && diagnostic.range.end <= region_end
    });
    for diagnostic in &mut context.diagnostics {
        translate_host_range_to_virtual(&mut diagnostic.range, offset);
    }

    let params = CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        range: virtual_range,
        context,
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: client_progress_token,
        },
        partial_result_params: PartialResultParams::default(),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/codeAction", params)
}

/// Parse a code action response into raw (virtual-coordinate, unpoliced)
/// actions. The bridge policy is applied later so an eager-resolve pass can
/// materialize lazy actions in between (see `send_code_action_request`).
fn parse_code_action_response_raw(
    mut response: serde_json::Value,
) -> Option<Vec<CodeActionOrCommand>> {
    if response_has_jsonrpc_error(&response, "textDocument/codeAction") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    parse_code_actions_leniently(result)
}

/// Parse a code action result value item by item: one malformed action must
/// not nuke the whole server response (a whole-`Vec` parse would return
/// `None` with zero valid actions and no log).
pub(crate) fn parse_code_actions_leniently(
    result: serde_json::Value,
) -> Option<CodeActionResponse> {
    if result.is_null() {
        return None;
    }
    let Ok(items) = serde_json::from_value::<Vec<serde_json::Value>>(result) else {
        log::warn!("textDocument/codeAction: response result is not an array");
        return None;
    };
    Some(
        items
            .into_iter()
            .filter_map(|item| match serde_json::from_value(item) {
                Ok(action) => Some(action),
                Err(error) => {
                    log::warn!("textDocument/codeAction: skipping malformed action: {error}");
                    None
                }
            })
            .collect(),
    )
}

/// Virtual-layer coordinate context for [`bridge_code_actions`]; `None`
/// means the host layer (real URIs and coordinates, nothing to translate).
pub(crate) struct VirtLayerContext<'a> {
    request_virtual_uri: &'a str,
    host_uri: &'a Uri,
    offset: &'a RegionOffset,
    /// Origin routing metadata for the `codeAction/resolve` envelope; carried
    /// alongside the coordinate context so a kept action that still needs
    /// resolve can be enveloped in one pass.
    region_id: &'a str,
    injection_language: &'a str,
    /// The host document URI as a parseable string for the envelope
    /// (`host_uri` above is the `Uri` form used for edit re-keying).
    host_uri_string: &'a str,
    server_name: &'a str,
}

impl VirtLayerContext<'_> {
    fn envelope_ctx(&self) -> CodeActionEnvelopeContext<'_> {
        CodeActionEnvelopeContext {
            server_name: self.server_name,
            host_uri: self.host_uri_string,
            region_id: self.region_id,
            injection_language: self.injection_language,
            offset: self.offset,
        }
    }
}

/// Upstream client capabilities that gate the bridge action policy
/// (LSP 3.15/3.16 `isPreferredSupport` / `disabledSupport`): fields the
/// client did not advertise must not reach it.
#[derive(Clone, Copy)]
pub(crate) struct UpstreamCodeActionCaps {
    pub(crate) disabled_support: bool,
    pub(crate) is_preferred_support: bool,
    /// LSP 3.16 `dataSupport`: the client round-trips `data` between
    /// `textDocument/codeAction` and `codeAction/resolve`.
    pub(crate) data_support: bool,
    /// LSP 3.16 `resolveSupport` includes `"edit"`: the client can lazily
    /// resolve an action's `edit` (not merely issue `codeAction/resolve`).
    pub(crate) resolve_edit_support: bool,
}

impl UpstreamCodeActionCaps {
    /// Whether a lazy (data-only) action can be handed to the client as-is
    /// with a routing envelope: needs BOTH `dataSupport` (the client preserves
    /// our envelope) and `resolveSupport` advertising `"edit"` (the client
    /// will actually resolve the edit). Otherwise the bridge must
    /// eager-resolve downstream instead.
    fn can_envelope(&self) -> bool {
        self.data_support && self.resolve_edit_support
    }
}

/// Apply the bridge policy to a downstream server's actions: title suffix,
/// command/lazy-action disabling, and (virt layer) coordinate translation.
///
/// `server_resolves` is whether the origin server advertises
/// `codeAction/resolve`: it decides whether a no-edit/no-command action with
/// no `data` is a resolvable lazy action (LSP 3.18 allows a title-only lazy
/// action) or a no-op to drop.
pub(crate) fn bridge_code_actions(
    actions: Vec<CodeActionOrCommand>,
    server_name: &str,
    upstream_caps: UpstreamCodeActionCaps,
    server_resolves: bool,
    virt: Option<&VirtLayerContext<'_>>,
) -> Vec<CodeActionOrCommand> {
    actions
        .into_iter()
        .filter_map(|item| {
            bridge_code_action(item, server_name, upstream_caps, server_resolves, virt)
        })
        .collect()
}

const REASON_COMMANDS: &str = "kakehashi does not bridge command execution yet";
const REASON_RESOLVE: &str = "this action could not be resolved to an applicable edit";
const REASON_CROSS_REGION: &str = "the edit cannot be represented in the host document";

fn bridge_code_action(
    item: CodeActionOrCommand,
    server_name: &str,
    upstream_caps: UpstreamCodeActionCaps,
    server_resolves: bool,
    virt: Option<&VirtLayerContext<'_>>,
) -> Option<CodeActionOrCommand> {
    let mut action = match item {
        // A bare Command cannot carry `disabled`; represent it as a disabled
        // CodeAction so the user still sees it exists (checklist: never
        // silently drop when the client can render why).
        CodeActionOrCommand::Command(command) => {
            return disabled_placeholder(
                command.title,
                REASON_COMMANDS,
                server_name,
                upstream_caps,
            );
        }
        CodeActionOrCommand::CodeAction(action) => action,
    };

    // The action's own diagnostics came back in virtual coordinates —
    // translate them before any early return so every surfaced action
    // (including a disabled one) carries host coordinates.
    if let Some(virt) = virt
        && let Some(diagnostics) = &mut action.diagnostics
    {
        for diagnostic in diagnostics {
            translate_virtual_range_to_host(&mut diagnostic.range, virt.offset);
        }
    }

    // A server-side disabled action needs no further policy: its own reason
    // already explains why it can't run, and it's only representable with
    // disabledSupport. Suffix it, strip any payload (we can't execute it and
    // a virt edit here would still be in untranslated coordinates), and keep
    // the server's own reason — never overwrite it with a generic one.
    if action.disabled.is_some() {
        if !upstream_caps.disabled_support {
            return None;
        }
        action.title = suffix_title(action.title, server_name);
        action.edit = None;
        action.command = None;
        action.data = None;
        action.is_preferred = None;
        return Some(CodeActionOrCommand::CodeAction(action));
    }

    // isPreferred is its own client capability (LSP 3.15); the downstream
    // baseline advertises it unconditionally, so strip it for clients that
    // did not opt in.
    if !upstream_caps.is_preferred_support {
        action.is_preferred = None;
    }

    // Commands are not executable until executeCommand is bridged; applying
    // only the edit half of an edit+command action would be worse than
    // disabling the whole action.
    if action.command.is_some() {
        return disable_action(action, REASON_COMMANDS, server_name, upstream_caps);
    }

    match &mut action.edit {
        Some(edit) => {
            if let Some(virt) = virt
                && !transform_workspace_edit_to_host(
                    edit,
                    virt.request_virtual_uri,
                    virt.host_uri,
                    virt.offset,
                )
            {
                return disable_action(action, REASON_CROSS_REGION, server_name, upstream_caps);
            }
        }
        None => {
            // No edit, no command: a lazy action to be completed via
            // `codeAction/resolve`. LSP 3.18 allows `data` to be absent (a
            // title-only lazy action matched by title), so it is lazy when it
            // carries `data` OR the origin server advertises resolve.
            if action.data.is_some() || server_resolves {
                // Envelope it (virt layer) if the client can resolve the edit,
                // so a later `codeAction/resolve` routes back to the origin;
                // otherwise it can never be completed — disable it (an
                // eager-resolve pass already ran for non-envelope clients).
                if let Some(virt) = virt.filter(|_| upstream_caps.can_envelope()) {
                    envelope_action_data(&mut action, &virt.envelope_ctx());
                    action.title = suffix_title(action.title, server_name);
                    return Some(CodeActionOrCommand::CodeAction(action));
                }
                return disable_action(action, REASON_RESOLVE, server_name, upstream_caps);
            }
            // No edit, no command, no data, and the server can't resolve:
            // selecting it would do nothing — drop it rather than clutter the
            // menu.
            return None;
        }
    }

    // Kept edit-carrying action: strip its `data`. We deliberately do NOT
    // envelope an edit-carrying action for resolve — its `edit` has already
    // been translated to host coordinates, and forwarding a later
    // `codeAction/resolve` would send that host-coordinate edit back to a
    // server that produced virtual coordinates (there is no inverse edit
    // transform). The edit is the payload; resolving supplementary fields on
    // an already-complete action is not supported. Untranslated `data` can
    // also embed virtual URIs, so it must not leak.
    action.data = None;
    action.title = suffix_title(action.title, server_name);
    Some(CodeActionOrCommand::CodeAction(action))
}

/// `"{title} — {server}"`: applied to every bridged action, unconditionally.
fn suffix_title(title: String, server_name: &str) -> String {
    format!("{title} — {server_name}")
}

/// Turn an action the bridge cannot execute into a `disabled` entry: the
/// unusable payload (edit/command/data) is stripped so a client that applies
/// it anyway cannot act on untranslated coordinates.
fn disable_action(
    mut action: CodeAction,
    reason: &str,
    server_name: &str,
    upstream_caps: UpstreamCodeActionCaps,
) -> Option<CodeActionOrCommand> {
    if !upstream_caps.disabled_support {
        return None;
    }
    action.title = suffix_title(action.title, server_name);
    action.edit = None;
    action.command = None;
    action.data = None;
    // A disabled action must not steer the client's auto-fix keybinding.
    action.is_preferred = None;
    // `disable_action` is only reached for actions the SERVER did not disable:
    // `bridge_code_action` handles a server-disabled action (keeping its own
    // reason) and returns before ever calling here. So the bridge always owns
    // the reason on this path — assign it unconditionally.
    action.disabled = Some(CodeActionDisabled {
        reason: reason.to_string(),
    });
    Some(CodeActionOrCommand::CodeAction(action))
}

fn disabled_placeholder(
    title: String,
    reason: &str,
    server_name: &str,
    upstream_caps: UpstreamCodeActionCaps,
) -> Option<CodeActionOrCommand> {
    if !upstream_caps.disabled_support {
        return None;
    }
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: suffix_title(title, server_name),
        disabled: Some(CodeActionDisabled {
            reason: reason.to_string(),
        }),
        ..CodeAction::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;
    use tower_lsp_server::ls_types::{CodeActionKind, CodeActionTriggerKind, Diagnostic, Position};

    fn make_host_uri() -> Uri {
        crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        VirtualDocumentUri::new(&make_host_uri(), "lua", "region-0").to_uri_string()
    }

    fn range(start_line: u32, end_line: u32) -> Range {
        Range {
            start: Position {
                line: start_line,
                character: 0,
            },
            end: Position {
                line: end_line,
                character: 5,
            },
        }
    }

    fn diagnostic_at(line: u32) -> Diagnostic {
        Diagnostic {
            range: range(line, line),
            message: "unused import".to_string(),
            code: Some(tower_lsp_server::ls_types::NumberOrString::String(
                "F401".to_string(),
            )),
            data: Some(json!({"fix": "remove"})),
            ..Diagnostic::default()
        }
    }

    // ==========================================================================
    // Request builder
    // ==========================================================================

    #[test]
    fn code_action_request_translates_range_and_diagnostics() {
        // Host line 5, region starts at line 3 → virtual line 2.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let context = CodeActionContext {
            diagnostics: vec![diagnostic_at(5)],
            only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            trigger_kind: Some(CodeActionTriggerKind::INVOKED),
        };
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            context,
            &RegionOffset::new(3, 0),
            Position {
                line: 13,
                character: 0,
            },
            RequestId::new(1),
            None,
        );

        assert_uses_virtual_uri(&request, "lua");
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["range"]["start"]["line"], 2);
        let diag = &json["params"]["context"]["diagnostics"][0];
        assert_eq!(diag["range"]["start"]["line"], 2);
        // Identity fields must survive byte-identical for downstream matching.
        assert_eq!(diag["code"], "F401");
        assert_eq!(diag["data"], json!({"fix": "remove"}));
        // `only` + `triggerKind` pass through.
        assert_eq!(
            json["params"]["context"]["only"][0],
            "source.organizeImports"
        );
        assert_eq!(json["params"]["context"]["triggerKind"], 1);
    }

    #[test]
    fn code_action_request_drops_diagnostic_straddling_region_end() {
        // A diagnostic that starts in-region but ends past the region end
        // would translate to a virtual end beyond the virtual document — the
        // whole range must fit the region, so it's dropped.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let straddling = Diagnostic {
            range: Range {
                start: Position {
                    line: 5,
                    character: 0,
                },
                end: Position {
                    line: 14,
                    character: 0,
                },
            },
            ..Diagnostic::default()
        };
        let context = CodeActionContext {
            diagnostics: vec![straddling],
            ..CodeActionContext::default()
        };
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            context,
            &RegionOffset::new(3, 0),
            Position {
                line: 13,
                character: 0,
            },
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        assert!(
            json["params"]["context"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty(),
            "a diagnostic ending past the region end must be dropped"
        );
    }

    #[test]
    fn code_action_request_drops_out_of_region_diagnostics() {
        // A diagnostic above the region clamps to virtual (0,0) and could
        // false-match a different diagnostic at the top of the virtual
        // document; one below the (region-clamped) request range lands past
        // the virtual document's end — both must be dropped, not clamped.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let context = CodeActionContext {
            diagnostics: vec![diagnostic_at(0), diagnostic_at(5), diagnostic_at(40)],
            ..CodeActionContext::default()
        };
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            context,
            &RegionOffset::new(3, 0),
            // Region spans host lines 3..13; line-5 diagnostic is in, line-40 is out.
            Position {
                line: 13,
                character: 0,
            },
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        let diags = json["params"]["context"]["diagnostics"].as_array().unwrap();
        assert_eq!(
            diags.len(),
            1,
            "diagnostics above and below the region must be dropped"
        );
        assert_eq!(diags[0]["range"]["start"]["line"], 2, "5 - region start 3");
    }

    #[test]
    fn code_action_request_keeps_in_region_diagnostic_for_cursor_request() {
        // A zero-length (cursor) request: a diagnostic exactly at the cursor
        // is in-region and must survive — the region span, not the request
        // range end, bounds the filter.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let cursor = Position {
            line: 5,
            character: 3,
        };
        let context = CodeActionContext {
            diagnostics: vec![Diagnostic {
                range: Range {
                    start: cursor,
                    end: cursor,
                },
                ..Diagnostic::default()
            }],
            ..CodeActionContext::default()
        };
        let request = build_code_action_request(
            &virtual_uri,
            Range {
                start: cursor,
                end: cursor,
            },
            context,
            &RegionOffset::new(3, 0),
            Position {
                line: 13,
                character: 0,
            },
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        let diags = json["params"]["context"]["diagnostics"].as_array().unwrap();
        assert_eq!(diags.len(), 1, "the cursor diagnostic must survive");
        assert_eq!(diags[0]["range"]["start"]["line"], 2, "5 - region start 3");
    }

    #[test]
    fn code_action_request_carries_work_done_token() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            CodeActionContext::default(),
            &RegionOffset::new(3, 0),
            Position {
                line: 13,
                character: 0,
            },
            test_request_id(),
            Some(NumberOrString::String("wd-1".to_string())),
        );
        assert_eq!(
            serde_json::to_value(&request).unwrap()["params"]["workDoneToken"],
            "wd-1"
        );
    }

    // ==========================================================================
    // Response transform
    // ==========================================================================

    /// Caps WITHOUT data/resolve support: lazy actions are disabled and kept
    /// actions have their `data` stripped (no routing envelope).
    fn caps(disabled_support: bool) -> UpstreamCodeActionCaps {
        UpstreamCodeActionCaps {
            disabled_support,
            is_preferred_support: true,
            data_support: false,
            resolve_edit_support: false,
        }
    }

    /// Caps WITH data + resolve support: actions carrying `data` are kept and
    /// wrapped in the routing envelope.
    fn caps_resolve() -> UpstreamCodeActionCaps {
        UpstreamCodeActionCaps {
            disabled_support: true,
            is_preferred_support: true,
            data_support: true,
            resolve_edit_support: true,
        }
    }

    /// The full sync response transform (parse + bridge policy), matching the
    /// production virt-layer path minus the async eager-resolve step.
    /// Assumes the origin server does NOT advertise resolve (title-only lazy
    /// actions drop); use [`transform_server_resolves`] for the resolve case.
    fn transform(
        result: serde_json::Value,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Option<CodeActionResponse> {
        transform_impl(result, upstream_caps, false)
    }

    /// Like [`transform`] but with the origin server advertising resolve, so a
    /// title-only action is a resolvable lazy action rather than a no-op.
    fn transform_server_resolves(
        result: serde_json::Value,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Option<CodeActionResponse> {
        transform_impl(result, upstream_caps, true)
    }

    fn transform_impl(
        result: serde_json::Value,
        upstream_caps: UpstreamCodeActionCaps,
        server_resolves: bool,
    ) -> Option<CodeActionResponse> {
        let host_uri = make_host_uri();
        let virtual_uri = make_virtual_uri_string();
        let offset = RegionOffset::new(10, 0);
        let actions = parse_code_action_response_raw(json!({
            "jsonrpc": "2.0", "id": 42, "result": result
        }))?;
        let virt = VirtLayerContext {
            request_virtual_uri: &virtual_uri,
            host_uri: &host_uri,
            offset: &offset,
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            injection_language: "lua",
            host_uri_string: "file:///test.md",
            server_name: "ruff",
        };
        Some(bridge_code_actions(
            actions,
            "ruff",
            upstream_caps,
            server_resolves,
            Some(&virt),
        ))
    }

    fn edit_carrying_action(title: &str) -> serde_json::Value {
        json!({
            "title": title,
            "kind": "quickfix",
            "edit": {
                "changes": {
                    make_virtual_uri_string(): [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "fixed"
                    }]
                }
            }
        })
    }

    #[test]
    fn null_result_returns_none() {
        assert!(transform(serde_json::Value::Null, caps(true)).is_none());
    }

    #[test]
    fn malformed_action_is_skipped_not_fatal() {
        // One malformed item (no title) must not nuke the whole server
        // response — the valid action survives.
        let actions = transform(
            json!([
                { "kind": "quickfix" },
                edit_carrying_action("Remove unused import")
            ]),
            caps(true),
        )
        .unwrap();
        assert_eq!(actions.len(), 1, "valid action must survive: {actions:?}");
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Remove unused import — ruff");
    }

    #[test]
    fn edit_carrying_action_is_translated_and_suffixed() {
        let actions = transform(
            json!([edit_carrying_action("Remove unused import")]),
            caps(true),
        )
        .unwrap();

        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Remove unused import — ruff");
        assert!(action.disabled.is_none());
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let edits = changes
            .get(&make_host_uri())
            .expect("edit must be re-keyed to the host URI");
        assert_eq!(edits[0].range.start.line, 10, "0 + region offset 10");
    }

    #[test]
    fn action_diagnostics_are_translated_back_to_host() {
        let mut action = edit_carrying_action("Fix");
        action["diagnostics"] = json!([{
            "range": {
                "start": { "line": 2, "character": 0 },
                "end": { "line": 2, "character": 5 }
            },
            "message": "unused"
        }]);
        let actions = transform(json!([action]), caps(true)).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.diagnostics.as_ref().unwrap()[0].range.start.line,
            12,
            "2 + region offset 10"
        );
    }

    #[test]
    fn bare_command_becomes_disabled_placeholder() {
        let actions = transform(
            json!([{ "title": "Run organize imports", "command": "ruff.organizeImports" }]),
            caps(true),
        )
        .unwrap();

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction placeholder");
        };
        assert_eq!(action.title, "Run organize imports — ruff");
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_COMMANDS);
        assert!(action.command.is_none() && action.edit.is_none());
    }

    #[test]
    fn bare_command_is_dropped_without_disabled_support() {
        let actions = transform(
            json!([{ "title": "Run organize imports", "command": "ruff.organizeImports" }]),
            caps(false),
        )
        .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn command_carrying_action_is_disabled_with_payload_stripped() {
        let mut action = edit_carrying_action("Fix all");
        action["command"] = json!({ "title": "post", "command": "ruff.postFix" });
        let actions = transform(json!([action]), caps(true)).unwrap();

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_COMMANDS);
        assert!(
            action.edit.is_none() && action.command.is_none(),
            "an edit+command action must not ship a half-appliable payload"
        );
        assert_eq!(action.title, "Fix all — ruff");
    }

    #[test]
    fn lazy_data_only_action_is_disabled() {
        let actions = transform(
            json!([{ "title": "Lazy fix", "data": { "id": 7 } }]),
            caps(true),
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_RESOLVE);
        assert!(action.data.is_none());
    }

    #[test]
    fn kept_action_strips_untranslated_data() {
        let mut action = edit_carrying_action("Fix");
        action["data"] = json!({ "virtualUri": make_virtual_uri_string() });
        let actions = transform(json!([action]), caps(true)).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert!(action.edit.is_some(), "edit must survive");
        assert!(
            action.data.is_none(),
            "for a client without data/resolve support the untranslated data \
             is stripped (the routing envelope is only added for envelope-capable \
             clients — see edit_carrying_action_with_data_is_enveloped_not_stripped)"
        );
    }

    #[test]
    fn is_preferred_stripped_without_client_support() {
        let mut action = edit_carrying_action("Fix");
        action["isPreferred"] = json!(true);
        let no_pref = UpstreamCodeActionCaps {
            disabled_support: true,
            is_preferred_support: false,
            data_support: false,
            resolve_edit_support: false,
        };
        let actions = transform(json!([action.clone()]), no_pref).unwrap();
        let CodeActionOrCommand::CodeAction(bridged) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            bridged.is_preferred, None,
            "isPreferred must not reach a client without isPreferredSupport"
        );

        // With support it passes through.
        let actions = transform(json!([action]), caps(true)).unwrap();
        let CodeActionOrCommand::CodeAction(bridged) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(bridged.is_preferred, Some(true));
    }

    #[test]
    fn server_disabled_command_action_keeps_server_reason() {
        let actions = transform(
            json!([{
                "title": "Not here",
                "command": { "title": "x", "command": "mock.x" },
                "disabled": { "reason": "wrong scope" },
                "isPreferred": true
            }]),
            caps(true),
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.disabled.as_ref().unwrap().reason,
            "wrong scope",
            "the server's own disabled reason must not be overwritten"
        );
        assert_eq!(
            action.is_preferred, None,
            "a disabled action must not steer auto-fix"
        );
    }

    #[test]
    fn server_disabled_action_diagnostics_are_translated_to_host() {
        // A disabled action can still carry diagnostics; on the virt layer
        // those are in virtual coordinates and must reach the client in host
        // coordinates even though the action short-circuits the policy.
        let actions = transform(
            json!([{
                "title": "Not here",
                "disabled": { "reason": "wrong scope" },
                "diagnostics": [{
                    "range": {
                        "start": { "line": 2, "character": 0 },
                        "end": { "line": 2, "character": 5 }
                    },
                    "message": "unused"
                }]
            }]),
            caps(true),
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.diagnostics.as_ref().unwrap()[0].range.start.line,
            12,
            "2 + region offset 10 — disabled actions get translated too"
        );
        assert_eq!(action.disabled.as_ref().unwrap().reason, "wrong scope");
    }

    #[test]
    fn payloadless_action_is_dropped() {
        // No edit, no command, no data, not server-disabled: selecting it can
        // do nothing, so it must not clutter the menu.
        let actions = transform(json!([{ "title": "Ghost" }]), caps(true)).unwrap();
        assert!(actions.is_empty(), "got: {actions:?}");
    }

    #[test]
    fn unrepresentable_edit_disables_the_action() {
        // A file op on the virtual URI makes the edit unrepresentable in host
        // coordinates (see workspace_edit.rs); the whole action is disabled
        // and the edit stripped.
        let action = json!({
            "title": "Extract to new file",
            "edit": {
                "documentChanges": [
                    { "kind": "create", "uri": make_virtual_uri_string() }
                ]
            }
        });
        let actions = transform(json!([action]), caps(true)).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.disabled.as_ref().unwrap().reason,
            REASON_CROSS_REGION
        );
        assert!(action.edit.is_none(), "untranslated edit must not leak");
    }

    #[test]
    fn unrepresentable_edit_drops_action_without_disabled_support() {
        let action = json!({
            "title": "Extract to new file",
            "edit": {
                "documentChanges": [
                    { "kind": "create", "uri": make_virtual_uri_string() }
                ]
            }
        });
        let actions = transform(json!([action]), caps(false)).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn server_disabled_action_dropped_without_disabled_support() {
        let actions = transform(
            json!([{ "title": "Not applicable here", "disabled": { "reason": "wrong scope" } }]),
            caps(false),
        )
        .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn host_layer_policy_keeps_edit_verbatim() {
        // Host layer (virt = None): real URIs/coordinates, no translation —
        // but the suffix and command policy still apply.
        let actions: Vec<CodeActionOrCommand> = serde_json::from_value(json!([
            {
                "title": "Sort imports",
                "edit": {
                    "changes": {
                        "file:///test.md": [{
                            "range": {
                                "start": { "line": 50, "character": 0 },
                                "end": { "line": 50, "character": 5 }
                            },
                            "newText": "sorted"
                        }]
                    }
                }
            },
            { "title": "Run linter", "command": "lint.run" }
        ]))
        .unwrap();

        let bridged = bridge_code_actions(actions, "marksman", caps(true), false, None);
        assert_eq!(bridged.len(), 2);
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Sort imports — marksman");
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let edits = changes.get(&make_host_uri()).unwrap();
        assert_eq!(edits[0].range.start.line, 50, "host edits stay verbatim");
        let CodeActionOrCommand::CodeAction(placeholder) = &bridged[1] else {
            panic!("Expected disabled placeholder");
        };
        assert_eq!(
            placeholder.disabled.as_ref().unwrap().reason,
            REASON_COMMANDS
        );
    }

    // ==========================================================================
    // PR 4: envelope + codeAction/resolve
    // ==========================================================================

    #[test]
    fn lazy_action_is_enveloped_for_resolve_capable_client() {
        // With data+resolve support, a lazy (data-only) action is kept and its
        // `data` wrapped in the routing envelope instead of being disabled.
        let actions = transform(
            json!([{ "title": "Lazy fix", "data": { "id": 7 } }]),
            caps_resolve(),
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert!(action.disabled.is_none(), "lazy action must be kept");
        assert_eq!(action.title, "Lazy fix — ruff", "suffix still applies");
        let envelope = extract_code_action_envelope(action).expect("envelope present");
        assert_eq!(envelope.origin, "ruff");
        assert_eq!(envelope.region_id, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(envelope.injection_language, "lua");
        assert_eq!(
            envelope.original_title, "Lazy fix",
            "the UNSUFFIXED title is stored for resolve-time restore"
        );
        assert_eq!(envelope.inner, Some(json!({ "id": 7 })));
        assert_eq!(envelope.offset.line, 10);
    }

    #[test]
    fn title_only_action_is_lazy_when_server_resolves() {
        // LSP 3.18: a lazy action may be title-only (no data) when the origin
        // server advertises resolve — it must be enveloped, not dropped.
        let actions = transform_server_resolves(
            json!([{ "title": "Lazy organize imports" }]),
            caps_resolve(),
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction, got: {actions:?}");
        };
        let envelope = extract_code_action_envelope(action).expect("envelope present");
        assert_eq!(envelope.origin, "ruff");
        assert_eq!(
            envelope.inner, None,
            "a title-only action has no inner data"
        );
        assert_eq!(envelope.original_title, "Lazy organize imports");
    }

    #[test]
    fn title_only_action_is_dropped_when_server_cannot_resolve() {
        // Same shape, but the server does NOT advertise resolve → the action
        // is a no-op and must be dropped, not enveloped.
        let actions = transform(json!([{ "title": "Ghost" }]), caps_resolve()).unwrap();
        assert!(actions.is_empty(), "got: {actions:?}");
    }

    #[test]
    fn lazy_action_not_enveloped_when_resolve_support_lacks_edit() {
        // dataSupport is on but resolveSupport does NOT advertise "edit" — the
        // client cannot materialize the edit lazily, so the action must not be
        // handed out enveloped; it's disabled (an eager-resolve pass covers the
        // real request path).
        let no_edit_resolve = UpstreamCodeActionCaps {
            disabled_support: true,
            is_preferred_support: true,
            data_support: true,
            resolve_edit_support: false,
        };
        let actions = transform(
            json!([{ "title": "Lazy fix", "data": { "id": 7 } }]),
            no_edit_resolve,
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.disabled.as_ref().unwrap().reason,
            REASON_RESOLVE,
            "a client that can't resolve `edit` must not receive an envelope"
        );
    }

    #[test]
    fn edit_carrying_action_with_data_is_stripped_not_enveloped() {
        // An edit+data action keeps the host-translated edit but has its data
        // STRIPPED, never enveloped: the edit is already complete, and
        // forwarding a resolve would send the host-coordinate edit back to a
        // server expecting virtual coordinates (no inverse edit transform).
        let mut action = edit_carrying_action("Fix");
        action["data"] = json!({ "server": "state" });
        let actions = transform(json!([action]), caps_resolve()).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        assert_eq!(
            changes.get(&make_host_uri()).unwrap()[0].range.start.line,
            10,
            "edit still host-translated"
        );
        assert!(
            action.data.is_none(),
            "an edit-carrying action's data must be stripped, not enveloped: {action:?}"
        );
    }

    #[test]
    fn dataless_action_is_not_enveloped() {
        // An edit-only action has nothing to resolve; no envelope is attached.
        let actions = transform(json!([edit_carrying_action("Fix")]), caps_resolve()).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert!(
            action.data.is_none(),
            "an action with no data needs no routing envelope"
        );
    }

    // -- envelope round-trip -------------------------------------------------

    fn envelope_ctx_for_test<'a>(offset: &'a RegionOffset) -> CodeActionEnvelopeContext<'a> {
        CodeActionEnvelopeContext {
            server_name: "ruff",
            host_uri: "file:///test.md",
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            injection_language: "lua",
            offset,
        }
    }

    #[test]
    fn strip_restores_data_and_captures_original_title() {
        let offset = RegionOffset::new(3, 0);
        let mut action = CodeAction {
            title: "Organize imports".to_string(),
            data: Some(json!({ "kind": "organize" })),
            ..CodeAction::default()
        };
        envelope_action_data(&mut action, &envelope_ctx_for_test(&offset));
        // Suffix happens after enveloping in production; simulate it.
        action.title = suffix_title(action.title, "ruff");

        let envelope = strip_code_action_envelope(&mut action).expect("envelope present");
        assert_eq!(
            action.data,
            Some(json!({ "kind": "organize" })),
            "strip restores the downstream's original data"
        );
        assert_eq!(
            envelope.original_title, "Organize imports",
            "the pre-suffix title is preserved for downstream title matching"
        );
        // strip moves `inner` back into `action.data` (asserted above), so the
        // returned envelope's own `inner` is drained — matching the code_lens
        // strip contract.
        assert_eq!(envelope.inner, None);
    }

    #[test]
    fn re_envelope_round_trips_after_strip() {
        let offset = RegionOffset::new(3, 0);
        let mut action = CodeAction {
            title: "Fix".to_string(),
            data: Some(json!({ "id": 1 })),
            ..CodeAction::default()
        };
        envelope_action_data(&mut action, &envelope_ctx_for_test(&offset));
        // The full envelope before the round-trip; strip drains inner into
        // action.data and re_envelope must restore every field identically.
        let original = extract_code_action_envelope(&action).expect("enveloped");
        let envelope = strip_code_action_envelope(&mut action).expect("envelope");
        re_envelope_action(&mut action, &envelope);
        let extracted = extract_code_action_envelope(&action).expect("re-enveloped");
        assert_eq!(
            extracted, original,
            "re-envelope must preserve EVERY field (a dropped field would leak here)"
        );
    }

    #[test]
    fn strip_leaves_non_envelope_data_intact() {
        // The mutating strip must not touch a non-envelope `data` (mirrors the
        // code_lens strip contract); only `extract`/`dispatch` cover it otherwise.
        let mut action = CodeAction {
            title: "x".to_string(),
            data: Some(json!({ "custom": true })),
            ..CodeAction::default()
        };
        assert!(strip_code_action_envelope(&mut action).is_none());
        assert_eq!(
            action.data,
            Some(json!({ "custom": true })),
            "non-envelope data must survive a strip attempt untouched"
        );
    }

    #[test]
    fn extract_returns_none_for_non_envelope_data() {
        let action = CodeAction {
            title: "x".to_string(),
            data: Some(json!({ "custom": true })),
            ..CodeAction::default()
        };
        assert!(extract_code_action_envelope(&action).is_none());
    }

    // -- resolve response parsing --------------------------------------------

    #[test]
    fn parse_resolve_response_happy_path() {
        let response = json!({
            "jsonrpc": "2.0", "id": 7,
            "result": { "title": "Fix", "edit": { "changes": {} } }
        });
        let action = parse_code_action_resolve_response(response).expect("parsed");
        assert_eq!(action.title, "Fix");
        assert!(action.edit.is_some());
    }

    #[test]
    fn parse_resolve_response_none_for_invalid() {
        for response in [
            json!({ "jsonrpc": "2.0", "id": 7, "result": null }),
            json!({ "jsonrpc": "2.0", "id": 7 }),
            json!({ "jsonrpc": "2.0", "id": 7, "error": { "code": -1, "message": "x" } }),
        ] {
            assert!(parse_code_action_resolve_response(response).is_none());
        }
    }

    #[test]
    fn outgoing_resolve_translates_diagnostics_to_virtual() {
        // Before forwarding a resolve, host-space diagnostic ranges are pulled
        // back to virtual coordinates so the downstream matches its own.
        let offset = RegionOffset::new(10, 0);
        let mut action = CodeAction {
            title: "Fix".to_string(),
            diagnostics: Some(vec![tower_lsp_server::ls_types::Diagnostic {
                range: range(12, 12),
                ..Default::default()
            }]),
            ..CodeAction::default()
        };
        translate_action_ranges_host_to_virtual(&mut action, &offset);
        assert_eq!(
            action.diagnostics.unwrap()[0].range.start.line,
            2,
            "12 - region offset 10"
        );
    }

    // -- dispatch fail-soft ---------------------------------------------------

    #[tokio::test]
    async fn dispatch_returns_non_envelope_action_unchanged() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default();
        let action = CodeAction {
            title: "x".to_string(),
            data: Some(json!({ "custom": true })),
            ..CodeAction::default()
        };
        let result = pool
            .dispatch_code_action_resolve(action.clone(), &settings, None)
            .await;
        assert_eq!(result.data, Some(json!({ "custom": true })));
    }

    #[tokio::test]
    async fn dispatch_re_envelopes_when_server_not_configured() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default();
        let offset = RegionOffset::new(3, 0);
        let mut action = CodeAction {
            title: "Fix".to_string(),
            data: Some(json!({ "id": 1 })),
            ..CodeAction::default()
        };
        envelope_action_data(&mut action, &envelope_ctx_for_test(&offset));

        let result = pool
            .dispatch_code_action_resolve(action, &settings, None)
            .await;
        let envelope = extract_code_action_envelope(&result).expect("envelope restored");
        assert_eq!(envelope.origin, "ruff");
        assert_eq!(envelope.inner, Some(json!({ "id": 1 })));
        assert!(result.edit.is_none(), "action stays unresolved");
    }
}
