//! Code action request handling for bridge connections (#568).
//!
//! Edit-carrying and lazy (resolve-deferred) actions are both bridged:
//! `codeAction/resolve` is routed back to the origin server via the
//! `CodeActionEnvelope` in `CodeAction.data` (PR 4), or eagerly resolved
//! downstream when the upstream client lacks `dataSupport`/`resolveSupport`.
//! Command-carrying actions are executable: the command name is rewritten to
//! encode its origin server + host document, so the bridged
//! `workspace/executeCommand` routes it back (PR 6, see
//! [`command_routing`](super::super::protocol)).
//!
//! Every bridged action title gets the `"{title} — {server}"` suffix so
//! users can see which downstream server each action comes from.

use std::io;
use std::sync::Arc;

use futures::StreamExt;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::{BridgeServerConfig, WorkspaceSettings};
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionContext, CodeActionDisabled, CodeActionOrCommand, CodeActionParams,
    CodeActionResponse, DocumentChangeOperation, DocumentChanges, NumberOrString,
    PartialResultParams, Position, Range, TextDocumentIdentifier, Uri, WorkDoneProgressParams,
    WorkspaceEdit,
};
use url::Url;

use super::super::pool::{ConnectionHandle, LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, encode_command,
    host_position_within_region, region_host_end, response_has_jsonrpc_error,
    strip_bridge_local_versions, transform_workspace_edit_to_host, translate_host_range_to_virtual,
    translate_virtual_range_to_host, virtual_uri_to_lsp_uri, workspace_edit_has_effect,
    workspace_edit_preserves_line_prefixes, workspace_edit_within_region,
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
    /// Host-layer action (`bridge._self`): its edit/data are already in host
    /// coordinates, so resolve routes to the host server VERBATIM — no virtual
    /// URI, region, or offset translation. `region_id`/`injection_language`/
    /// `offset` are unused for these. Defaults to `false` (virt layer) so
    /// existing enveloped actions deserialize unchanged.
    #[serde(default)]
    pub(crate) host_layer: bool,
}

impl CodeActionEnvelope {
    /// Whether this is a genuine HOST-layer envelope: `host_layer` set AND no
    /// region identity. A host envelope is minted with an empty `region_id`
    /// ([`envelope_host_action`]), so requiring both here means a CONFORMING
    /// client that merely flips `host_layer = true` on a VIRT envelope (which
    /// keeps its `region_id`) still can't route it through the translation-free
    /// host path and skip the region freshness / coordinate validation. This is
    /// not a security boundary: the envelope round-trips through client-supplied
    /// `CodeAction.data` and is not integrity-protected, so a client could clear
    /// `region_id` too — the check guards against ACCIDENTAL bypass, not a
    /// malicious one (which the translation-free host path also fails soft on).
    pub(crate) fn is_host_layer(&self) -> bool {
        self.host_layer && self.region_id.is_empty()
    }
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
        host_layer: false,
    };
    action.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

/// Wrap a HOST-layer action's `data` in a routing envelope so a later
/// `codeAction/resolve` routes back to the host server. The host document is
/// forwarded verbatim, so no region/offset is captured (`host_layer = true`
/// tells the resolve path to skip all coordinate translation). Captures the
/// CURRENT (unsuffixed) title as `original_title`; call before suffixing.
fn envelope_host_action(action: &mut CodeAction, server_name: &str, host_uri: &str) {
    let inner = action.data.take();
    let envelope = CodeActionEnvelope {
        origin: server_name.to_string(),
        host_uri: host_uri.to_string(),
        region_id: String::new(),
        injection_language: String::new(),
        // Unused on the host path (resolve forwards verbatim); a zero offset.
        offset: EnvelopeOffset {
            line: 0,
            column: 0,
            line_column_offsets: None,
        },
        original_title: action.title.clone(),
        inner,
        host_layer: true,
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
            host_layer: envelope.host_layer,
        }
    }));
}

/// Shape a host server's `codeAction/resolve` RESPONSE for the client (host
/// layer — coordinates are already host-relative, so no translation). `resolved`
/// is the downstream's resolved action; `action`/`envelope` are the original
/// unresolved action + its envelope (for fail-soft and lazy re-envelope);
/// `suffixed_title` is the "` — {server}`"-suffixed title to re-apply.
///
/// Policy (mirrors the virt path): a disabled resolve is surfaced disabled; a
/// materialized command is name-encoded for `executeCommand` routing (dropped
/// if unencodable); a result with no command and no applicable edit is a no-op
/// and is disabled as `REASON_RESOLVE`; a still-lazy result is re-enveloped for
/// a further resolve. Without `disabledSupport` every disable path fails soft to
/// the unresolved action instead.
fn finalize_host_resolved_action(
    mut resolved: CodeAction,
    mut action: CodeAction,
    mut envelope: CodeActionEnvelope,
    suffixed_title: String,
    upstream_caps: UpstreamCodeActionCaps,
) -> CodeAction {
    // Clone the origin so later `envelope` mutation (lazy re-envelope) doesn't
    // conflict with the `&str` borrows below; resolve is a cold path.
    let server_name = envelope.origin.clone();

    // Disabled resolve → surface disabled (strip payload) with disabledSupport,
    // else fail soft to the unresolved action.
    if resolved.disabled.is_some() {
        if !upstream_caps.disabled_support {
            re_envelope_action(&mut action, &envelope);
            return action;
        }
        resolved.title = resuffix_resolved_title(
            std::mem::take(&mut resolved.title),
            suffixed_title,
            &server_name,
        );
        resolved.edit = None;
        resolved.command = None;
        resolved.data = None;
        resolved.is_preferred = None;
        return resolved;
    }

    // A materialized command must route back to the host server via
    // executeCommand; drop it if the routing name can't be encoded. Remember
    // whether a command was actually DROPPED (present but unencodable) — that is
    // distinct from a resolve that never carried a command. Encode under the
    // mutable borrow, then clear afterwards so the borrow is out of scope.
    let mut command_dropped = false;
    if let Some(command) = resolved.command.as_mut() {
        match encode_command(&server_name, &envelope.host_uri, &command.command) {
            Some(encoded) => command.command = encoded,
            None => command_dropped = true,
        }
    }
    if command_dropped {
        resolved.command = None;
    }

    // A resolved action with no command and no applicable edit is a no-op —
    // disable it as unresolvable (`REASON_RESOLVE`) rather than hand the client
    // an enabled action that applies nothing (mirrors the virt path's empty-edit
    // policy). Two shapes qualify: a `Some`-but-empty edit (the server resolved
    // to nothing), and a command-only action whose routing name couldn't be
    // encoded above (command dropped, edit absent). A resolve that NEVER carried
    // a command and has no edit is NOT a no-op — it is a still-lazy staged
    // resolve, re-enveloped for a further pass below. Without `disabledSupport`,
    // fail soft to the unresolved action.
    if resolved.command.is_none()
        && (resolved.edit.as_ref().is_some_and(workspace_edit_is_empty)
            || (command_dropped && resolved.edit.is_none()))
    {
        if !upstream_caps.disabled_support {
            re_envelope_action(&mut action, &envelope);
            return action;
        }
        resolved.title = resuffix_resolved_title(
            std::mem::take(&mut resolved.title),
            suffixed_title,
            &server_name,
        );
        resolved.edit = None;
        resolved.data = None;
        resolved.is_preferred = None;
        resolved.disabled = Some(CodeActionDisabled {
            reason: REASON_RESOLVE.to_string(),
        });
        return resolved;
    }

    if !upstream_caps.is_preferred_support {
        resolved.is_preferred = None;
    }

    // The resolved edit is already in host coordinates — kept verbatim (no
    // cross-region / translation concern the virt path guards against),
    // except versions, which are bridge-local and must not reach the editor.
    if let Some(edit) = &mut resolved.edit {
        strip_bridge_local_versions(edit);
    }
    let server_title = std::mem::take(&mut resolved.title);
    resolved.title = resuffix_resolved_title(server_title.clone(), suffixed_title, &server_name);

    // Materialized (edit or command) → strip the envelope; still lazy →
    // re-envelope for a further resolve, syncing a server-changed title.
    if resolved.edit.is_some() || resolved.command.is_some() {
        resolved.data = None;
    } else {
        if !server_title.is_empty() {
            envelope.original_title = server_title;
        }
        if resolved.data.is_none() {
            resolved.data = action.data.take();
        }
        re_envelope_action(&mut resolved, &envelope);
    }
    resolved
}

/// Whether a `WorkspaceEdit` carries no actual change. Checks the INNER edit
/// vectors, not just the outer containers: a `changes` map whose every value
/// is an empty `TextEdit[]`, or `documentChanges` whose every entry has empty
/// `edits`, is still a no-op. A file operation counts as a real change.
fn workspace_edit_is_empty(edit: &WorkspaceEdit) -> bool {
    !workspace_edit_has_effect(edit)
}

/// Translate an action's edit host-ward with resolve-grade validation, in
/// place. Returns `Err(reason)` (leaving the edit partially mutated — the caller
/// must discard the action and disable it with `reason`) when the edit can't be
/// faithfully represented in the host document. The reason distinguishes three
/// permanent failures the client shows to the user:
/// - [`REASON_CROSS_REGION`] — the edit touches another injection region (the
///   shared transform would silently filter those and partially apply the rest),
///   contains a virtual-URI file op (transform returns false), or a translated
///   range escapes the region end (`region_end`) — a stale/malformed downstream
///   edit whose virtual range runs past the region would otherwise land in
///   unrelated host text after the fence.
/// - [`REASON_RESOLVE`] — the edit ended up empty: the server produced nothing
///   applicable, which reads as "could not be resolved to an applicable edit",
///   not "cannot be represented in the host document".
/// - [`REASON_PREFIXED_REGION`] — the translated edit would corrupt the host
///   structure around the region: it spans/inserts lines in a line-prefixed
///   (e.g. blockquote) region whose prefixes the verbatim newText would strip,
///   or it merges content into the closing fence.
///
/// Used by BOTH the initial-response policy and the codeAction/resolve path so
/// these are rejected uniformly, never partially applied.
fn translate_edit_host_ward_strict(
    edit: &mut WorkspaceEdit,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
) -> Result<(), &'static str> {
    if workspace_edit_touches_foreign_region(edit, request_virtual_uri) {
        return Err(REASON_CROSS_REGION);
    }
    if !transform_workspace_edit_to_host(edit, request_virtual_uri, host_uri, offset) {
        return Err(REASON_CROSS_REGION);
    }
    // Reached directly by the client-driven resolve path (which bypasses
    // bridge_code_action's own strip): versions on OTHER real files are
    // bridge-local too and must not reach the editor.
    strip_bridge_local_versions(edit);
    if workspace_edit_is_empty(edit) {
        return Err(REASON_RESOLVE);
    }
    if !workspace_edit_within_region(edit, host_uri, offset, region_end) {
        return Err(REASON_CROSS_REGION);
    }
    if !workspace_edit_preserves_line_prefixes(edit, host_uri, offset, region_end) {
        return Err(REASON_PREFIXED_REGION);
    }
    Ok(())
}

/// Whether a `WorkspaceEdit` touches a virtual document OTHER than
/// `request_virtual_uri` (a cross-region edit). The shared host transform
/// silently filters such text edits; the resolve path uses this to fail soft
/// instead, since a partially-applied resolved edit is worse than none.
///
/// A foreign key that carries ZERO edits is a no-op — the transform strips it
/// cleanly and it applies nothing — so it does NOT count as touching a foreign
/// region (consistent with `workspace_edit_is_empty`'s inner-vector check).
/// Only foreign entries with real edits would be silently dropped, and those
/// are what must reject the whole action.
fn workspace_edit_touches_foreign_region(edit: &WorkspaceEdit, request_virtual_uri: &str) -> bool {
    let is_foreign =
        |uri: &str| VirtualDocumentUri::is_virtual_uri(uri) && uri != request_virtual_uri;
    if let Some(changes) = &edit.changes
        && changes
            .iter()
            .any(|(k, edits)| is_foreign(k.as_str()) && !edits.is_empty())
    {
        return true;
    }
    match &edit.document_changes {
        None => false,
        Some(DocumentChanges::Edits(edits)) => edits
            .iter()
            .any(|e| is_foreign(e.text_document.uri.as_str()) && !e.edits.is_empty()),
        Some(DocumentChanges::Operations(ops)) => ops.iter().any(|op| match op {
            DocumentChangeOperation::Edit(e) => {
                is_foreign(e.text_document.uri.as_str()) && !e.edits.is_empty()
            }
            // File ops on virtual URIs are rejected by the transform itself
            // (it returns false), so they need no foreign check here.
            DocumentChangeOperation::Op(_) => false,
        }),
    }
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
        // NOTE: a `url_to_uri` failure here is already unreachable — the shared
        // `execute_bridge_request_observed` converts the same `host_uri` first
        // (pool/execute.rs) and hard-fails there — so a local fail-soft would be
        // dead code. Making the host-URI conversion fail-soft end-to-end is a
        // shared-helper change tracked in #615.
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(host_uri)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let virtual_uri_string =
            VirtualDocumentUri::new(&host_uri_lsp, injection_language, region_id).to_uri_string();
        let virt = VirtLayerContext {
            request_virtual_uri: &virtual_uri_string,
            host_uri: &host_uri_lsp,
            offset: &offset,
            // Reuse the value computed for phase 1 (line ~272): deterministic in
            // `(virtual_content, offset)`, which are unchanged here.
            region_end,
            region_id,
            injection_language,
            host_uri_string: host_uri.as_str(),
            server_name,
        };
        Ok(Some(bridge_code_actions(
            actions,
            server_name,
            host_uri.as_str(),
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
        // Collect the lazy actions' indices + clones, resolve them CONCURRENTLY
        // on the shared connection (its ResponseRouter multiplexes by
        // request_id), then splice results back by index. Sequential awaits
        // would block the codeAction response for the SUM of resolve times when
        // a server returns several lazy actions (organize-imports / batch
        // quickfix). Concurrency is CAPPED (`buffer_unordered`) so a server that
        // returns a very large lazy-action list can't burst an unbounded number
        // of in-flight resolves onto one connection. Lazy = no edit/command, not
        // server-disabled; `data` is NOT required (already gated on the server
        // advertising resolve, so a title-only action here is a resolvable lazy
        // action per LSP 3.18).
        let pending: Vec<(usize, CodeAction)> = actions
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| match item {
                CodeActionOrCommand::CodeAction(action)
                    if action.edit.is_none()
                        && action.command.is_none()
                        && action.disabled.is_none() =>
                {
                    Some((idx, action.clone()))
                }
                _ => None,
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        /// Max concurrent eager `codeAction/resolve`s on one connection.
        const MAX_CONCURRENT_EAGER_RESOLVES: usize = 8;
        let resolved: Vec<(usize, Option<CodeAction>)> =
            futures::stream::iter(pending.into_iter().map(|(idx, action)| {
                let upstream_id = upstream_id.clone();
                async move {
                    (
                        idx,
                        self.send_code_action_resolve_on_handle(handle, action, upstream_id)
                            .await,
                    )
                }
            }))
            .buffer_unordered(MAX_CONCURRENT_EAGER_RESOLVES)
            .collect()
            .await;
        for (idx, outcome) in resolved {
            if let Some(materialized) = outcome
                && let CodeActionOrCommand::CodeAction(slot) = &mut actions[idx]
            {
                *slot = materialized;
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
        upstream_caps: UpstreamCodeActionCaps,
        upstream_id: Option<UpstreamId>,
        region_end: Position,
    ) -> CodeAction {
        let Some(envelope) = strip_code_action_envelope(&mut action) else {
            return action;
        };

        // resolve is USER-initiated (they selected the action): fail soft,
        // but never silently — a server removed/renamed since the action was
        // minted lands here.
        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: origin '{}' is not spawnable (removed or \
                 misconfigured since the action was produced); returning unresolved",
                envelope.origin
            );
            re_envelope_action(&mut action, &envelope);
            return action;
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        ) else {
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: origin '{}' has no resolvable config; returning unresolved",
                envelope.origin
            );
            re_envelope_action(&mut action, &envelope);
            return action;
        };

        // Host-layer actions are already in host coordinates — route their
        // resolve to the host server VERBATIM (no translation, #627). A genuine
        // host envelope has no region identity; requiring that blocks a client
        // flipping `host_layer` on a virt envelope to skip translation.
        if envelope.is_host_layer() {
            return self
                .send_host_code_action_resolve(
                    &config,
                    action,
                    envelope,
                    upstream_caps,
                    upstream_id,
                )
                .await;
        }

        self.send_code_action_resolve_request(
            &config,
            action,
            envelope,
            upstream_caps,
            upstream_id,
            region_end,
        )
        .await
    }

    /// Route a HOST-layer `codeAction/resolve` back to its host server VERBATIM:
    /// the action is already in host coordinates, so nothing is translated. Same
    /// policy as [`Self::send_code_action_resolve_request`] (restore title →
    /// forward → disable / command-route / isPreferred / suffix / strip-or-
    /// re-envelope) MINUS coordinate translation and the host-ward edit
    /// validation (a host edit needs neither). Fails soft (returns the action
    /// unresolved, envelope restored) at every step.
    async fn send_host_code_action_resolve(
        &self,
        server_config: &BridgeServerConfig,
        mut action: CodeAction,
        envelope: CodeActionEnvelope,
        upstream_caps: UpstreamCodeActionCaps,
        upstream_id: Option<UpstreamId>,
    ) -> CodeAction {
        let server_name = &envelope.origin;
        // `host_uri` comes from client-supplied `data` (the resolve params echo
        // the action's envelope), so a malformed value must fail soft, NOT fall
        // through to `get_or_create_connection(.., None)` — a `None` document
        // hint routes to a rootless client-fallback / shared key that could run
        // the resolve against the wrong workspace. The bridge only ever mints a
        // valid `Url::as_str()` here, so a parse failure means a corrupt/foreign
        // envelope.
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve (host): envelope host_uri '{}' is not a valid URL; ignoring",
                envelope.host_uri
            );
            re_envelope_action(&mut action, &envelope);
            return action;
        };
        let handle = match self
            .get_or_create_connection(server_name, server_config, Some(&host_url))
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "codeAction/resolve (host): failed to connect to {server_name}: {e}"
                );
                re_envelope_action(&mut action, &envelope);
                return action;
            }
        };
        if !handle.has_capability("codeAction/resolve") {
            // Anomalous: the envelope was only minted because the origin
            // advertised resolve, so reaching here means a respawn changed
            // capabilities (or the handle is still initializing).
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: {} no longer advertises resolveProvider; returning unresolved",
                envelope.origin
            );
            re_envelope_action(&mut action, &envelope);
            return action;
        }

        // Forward with the ORIGINAL (unsuffixed) title restored; keep the
        // suffixed title to re-apply. Host coordinates: no range translation.
        let mut outgoing = action.clone();
        let suffixed_title =
            std::mem::replace(&mut outgoing.title, envelope.original_title.clone());

        let Some(resolved) = self
            .send_code_action_resolve_on_handle(&handle, outgoing, upstream_id)
            .await
        else {
            re_envelope_action(&mut action, &envelope);
            return action;
        };

        finalize_host_resolved_action(resolved, action, envelope, suffixed_title, upstream_caps)
    }

    /// Reconnect to the origin `(server, root)`, restore the original title,
    /// translate coordinates back to virtual, forward `codeAction/resolve`,
    /// then translate the resolved edit/diagnostics host-ward, re-suffix, and
    /// re-envelope. Every failure path returns the action unresolved.
    async fn send_code_action_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut action: CodeAction,
        mut envelope: CodeActionEnvelope,
        upstream_caps: UpstreamCodeActionCaps,
        upstream_id: Option<UpstreamId>,
        region_end: Position,
    ) -> CodeAction {
        let server_name = &envelope.origin;
        // Client-supplied `host_uri` (see the host path): a malformed value must
        // fail soft, not connect with a `None` document hint that routes to a
        // rootless client-fallback / shared key. The bridge only mints valid
        // URLs here, so a parse failure means a corrupt/foreign envelope.
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: envelope host_uri '{}' is not a valid URL; ignoring",
                envelope.host_uri
            );
            re_envelope_action(&mut action, &envelope);
            return action;
        };
        let handle = match self
            .get_or_create_connection(server_name, server_config, Some(&host_url))
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
            // Anomalous: the envelope was only minted because the origin
            // advertised resolve, so reaching here means a respawn changed
            // capabilities (or the handle is still initializing).
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: {} no longer advertises resolveProvider; returning unresolved",
                envelope.origin
            );
            re_envelope_action(&mut action, &envelope);
            return action;
        }

        let offset = RegionOffset::from(&envelope.offset);
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(&host_url).ok();

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

        // Translate the resolved action's own diagnostics host-ward first, so
        // every surfaced action — including a disabled one returned below —
        // carries host coordinates (mirrors the initial-path order).
        if let Some(diagnostics) = &mut resolved.diagnostics {
            for diagnostic in diagnostics {
                translate_virtual_range_to_host(&mut diagnostic.range, &offset);
            }
        }

        // A resolve that marks the action `disabled` makes it non-executable —
        // handle it BEFORE the command/edit policy (mirrors the initial path,
        // so a disabled+command resolve is surfaced disabled, not failed soft).
        // Without upstream `disabledSupport` the client can't render it, so
        // fail soft (return the original, already-sanitized action unresolved);
        // with it, strip the payload (a disabled action must never carry an
        // executable edit/command) and surface it disabled with reason + suffix.
        if resolved.disabled.is_some() {
            if !upstream_caps.disabled_support {
                re_envelope_action(&mut action, &envelope);
                return action;
            }
            resolved.title = resuffix_resolved_title(
                std::mem::take(&mut resolved.title),
                suffixed_title,
                server_name,
            );
            resolved.edit = None;
            resolved.command = None;
            resolved.data = None;
            resolved.is_preferred = None;
            return resolved;
        }

        // A resolve that materializes a `command` (or edit+command) is now
        // executable: rewrite the command name to encode the origin server so
        // the bridged executeCommand routes back to it (mirrors the initial
        // codeAction path). An edit+command action keeps both — the edit is
        // translated below, then the client executes the command.
        // Drop the command if its routing name can't be encoded (unroutable);
        // an accompanying edit is still applied.
        if resolved
            .command
            .as_mut()
            .and_then(|command| {
                encode_command(server_name, &envelope.host_uri, &command.command)
                    .map(|encoded| command.command = encoded)
            })
            .is_none()
        {
            resolved.command = None;
        }

        // isPreferred is its own client capability (LSP 3.15); the downstream
        // baseline advertises it unconditionally, so strip it for clients that
        // did not opt in.
        if !upstream_caps.is_preferred_support {
            resolved.is_preferred = None;
        }

        // Translate the resolved edit host-ward. When it can't be faithfully
        // represented in the host document — a virtual-URI file op (transform
        // returns false); ANY cross-region edit (the shared transform would
        // silently filter it, partially applying the rest); or an edit that
        // ends up empty — disable the action (or, without disabledSupport, fail
        // soft to the unresolved action). Any of these is worse than a disabled
        // action: the user would apply it and get a partial or no-op change.
        // This is a PERMANENT failure, so `resolve_untranslatable_edit` disables
        // rather than transient-fail-softing.
        let disable_reason = match (resolved.edit.as_mut(), host_uri_lsp.as_ref()) {
            (Some(edit), Some(host_uri_lsp)) => {
                let virtual_uri = VirtualDocumentUri::new(
                    host_uri_lsp,
                    &envelope.injection_language,
                    &envelope.region_id,
                )
                .to_uri_string();
                translate_edit_host_ward_strict(
                    edit,
                    &virtual_uri,
                    host_uri_lsp,
                    &offset,
                    region_end,
                )
                .err()
            }
            // The server resolved an edit, but the host URI couldn't be rebuilt
            // to translate it (a `url`/`Uri` parser divergence). Shipping the
            // untranslated virtual-coordinate edit would be unappliable — same
            // permanent failure as the cross-region case.
            (Some(_), None) => Some(REASON_CROSS_REGION),
            (None, _) => None,
        };
        if let Some(reason) = disable_reason {
            return resolve_untranslatable_edit(
                resolved,
                action,
                &envelope,
                suffixed_title,
                server_name,
                upstream_caps,
                reason,
            );
        }

        // Re-apply the "{title} — {server}" suffix. The server normally echoes
        // the restored original title back, but if it changed the title during
        // resolve (allowed by LSP) that change is kept and re-suffixed. Capture
        // the raw (unsuffixed) server title first — the still-lazy path below
        // needs it to keep the envelope's title in sync.
        let server_title = std::mem::take(&mut resolved.title);
        resolved.title = resuffix_resolved_title(server_title.clone(), suffixed_title, server_name);

        // Once resolve has materialized an edit OR a command, the action is
        // complete (the edit is host-translated; the command name is routed).
        // Re-enveloping it would let a second resolve forward that host-
        // coordinate edit back downstream (no inverse transform) — so strip the
        // data instead, mirroring the initial-response policy. Only a still-lazy
        // resolved action (no edit, no command) keeps a routing envelope for a
        // further resolve.
        if resolved.edit.is_some() || resolved.command.is_some() {
            resolved.data = None;
        } else {
            // Still lazy: a future resolve restores `envelope.original_title`
            // and forwards it downstream. If the server changed the title on
            // THIS resolve, track the new (unsuffixed) title so a title-matching
            // server sees the title it last advertised, not the stale initial
            // one. (The envelope exists precisely for match-by-title servers.)
            if !server_title.is_empty() {
                envelope.original_title = server_title;
            }
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
                        "codeAction/resolve: failed to register request on {connection_key:?}: {e}"
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
                "codeAction/resolve: failed to send request on {connection_key:?}: {e}"
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

        // Fail soft, but not silently: surface timeouts / channel-closed like the
        // sibling codeLens/completion resolve paths so resolve-time issues are
        // debuggable (qodo review finding).
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "codeAction/resolve failed for {connection_key:?}: {e}"
                );
                return None;
            }
        };
        parse_code_action_resolve_response(response)
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
    match serde_json::from_value(result) {
        Ok(resolved) => Some(resolved),
        // A present, non-null result that doesn't parse: without a log this
        // is undebuggable in the field (the action just stays unresolved).
        // Mirrors parse_code_actions_leniently's per-item warn.
        Err(e) => {
            warn!(
                target: "kakehashi::bridge",
                "codeAction/resolve: malformed resolve result: {e}"
            );
            None
        }
    }
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
    /// The region's exclusive host-document end: bounds an edit-carrying
    /// action's translated ranges so a range past the region can't land in
    /// unrelated host text after the fence.
    region_end: Position,
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
    pub(crate) fn can_envelope(&self) -> bool {
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
    host_uri: &str,
    upstream_caps: UpstreamCodeActionCaps,
    server_resolves: bool,
    virt: Option<&VirtLayerContext<'_>>,
) -> Vec<CodeActionOrCommand> {
    actions
        .into_iter()
        .filter_map(|item| {
            bridge_code_action(
                item,
                server_name,
                host_uri,
                upstream_caps,
                server_resolves,
                virt,
            )
        })
        .collect()
}

const REASON_RESOLVE: &str = "this action could not be resolved to an applicable edit";
const REASON_CROSS_REGION: &str = "the edit cannot be represented in the host document";
const REASON_PREFIXED_REGION: &str = "the edit would break the host document's structure \
     around the injected region (its line prefixes, e.g. a blockquote's, or the \
     closing fence)";

fn bridge_code_action(
    item: CodeActionOrCommand,
    server_name: &str,
    host_uri: &str,
    upstream_caps: UpstreamCodeActionCaps,
    server_resolves: bool,
    virt: Option<&VirtLayerContext<'_>>,
) -> Option<CodeActionOrCommand> {
    let mut action = match item {
        // A bare Command is executable via workspace/executeCommand once its
        // name encodes the origin server + host document (the client sends only
        // command+arguments on execute — no data envelope to route by).
        // Arguments stay verbatim (checklist §10); suffix the title like every
        // bridged action.
        CodeActionOrCommand::Command(mut command) => {
            // Drop a bare command we can't mint an unambiguous routing name for
            // (pathological: a separator in the server name) — routed back it
            // would reach the wrong server, and un-routed it executes to nothing.
            command.command = encode_command(server_name, host_uri, &command.command)?;
            command.title = suffix_title(command.title, server_name);
            return Some(CodeActionOrCommand::Command(command));
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

    // A command makes the action executable via workspace/executeCommand.
    // Rewrite the command name to encode its origin server so the bridged
    // executeCommand routes it back (the client sends only command+arguments on
    // execute — no data envelope). Arguments pass through verbatim (they are the
    // downstream's own coordinate system, checklist §10). An edit+command action
    // keeps BOTH: the client applies the (translated) edit, then executes.
    let has_command = action
        .command
        .as_mut()
        .and_then(|command| {
            encode_command(server_name, host_uri, &command.command)
                .map(|encoded| command.command = encoded)
        })
        .is_some();
    // A command whose name couldn't be encoded (unroutable) is dropped; an
    // accompanying edit is still applied below, else the action is dropped as a
    // no-op. Assigning None when there was no command is a harmless no-op.
    if !has_command {
        action.command = None;
    }

    match &mut action.edit {
        Some(edit) => {
            // Versions in a downstream's edit are bridge-local (its own didOpen
            // counter) and would read as stale to version-checking editors —
            // null them on every layer before the edit can reach the client.
            strip_bridge_local_versions(edit);
            // Virt layer: translate host-ward with resolve-grade validation
            // (the same check the client-driven resolve path uses), so an
            // edit touching another region — including an eager-resolved lazy
            // action's edit, which flows through here — is disabled, never
            // partially applied.
            if let Some(virt) = virt
                && let Err(reason) = translate_edit_host_ward_strict(
                    edit,
                    virt.request_virtual_uri,
                    virt.host_uri,
                    virt.offset,
                    virt.region_end,
                )
            {
                return disable_action(action, reason, server_name, upstream_caps);
            }
        }
        None => {
            // Command-only action: executable via the bridged executeCommand
            // (its name was rewritten above), no edit to translate. Strip the
            // resolve `data` and surface it suffixed.
            if has_command {
                action.data = None;
                action.title = suffix_title(action.title, server_name);
                return Some(CodeActionOrCommand::CodeAction(action));
            }
            // No edit, no command: is this a lazy (resolve-deferred) action?
            //
            // Virt layer: the *bridge* issues the `codeAction/resolve`, so the
            // origin server must advertise `resolveProvider` — `data` alone
            // doesn't make it lazy (the bridge could never complete it), and
            // LSP 3.18 allows a lazy action to be title-only, so the server's
            // capability is the deciding factor.
            //
            // Host layer (`virt` is None): host-layer `codeAction/resolve` is now
            // bridged (#627), so a host lazy action from a resolving server is
            // enveloped for resolve-routing (below); one from a non-resolving
            // server is surfaced as a `REASON_RESOLVE` disabled placeholder so
            // `disabledSupport` clients see why it can't run.
            //
            // Now that host-layer resolve is bridged (#627), the host gate
            // mirrors the virt one: a title-only action (LSP 3.18, no `data`)
            // from a host server advertising `resolveProvider` is a resolvable
            // lazy action, not clutter — `server_resolves` recognizes it. A
            // `data`-carrying action stays lazy regardless (an intrinsic lazy
            // signal), so a resolving server can complete it and a non-resolving
            // one still surfaces it disabled below (#615 item 2).
            let is_lazy = match virt {
                Some(_) => server_resolves,
                None => action.data.is_some() || server_resolves,
            };
            if is_lazy {
                // Virt layer: envelope if the client can resolve the edit, so a
                // later `codeAction/resolve` routes back to the origin (an
                // eager-resolve pass already ran for non-envelope clients).
                if let Some(virt) = virt.filter(|_| upstream_caps.can_envelope()) {
                    envelope_action_data(&mut action, &virt.envelope_ctx());
                    action.title = suffix_title(action.title, server_name);
                    return Some(CodeActionOrCommand::CodeAction(action));
                }
                // Host layer: if the host server advertises `resolveProvider`
                // and the client can envelope, route resolve back to it VERBATIM
                // (host coordinates, no translation — #627). Otherwise it can
                // never be completed here, so disable it.
                if virt.is_none() && server_resolves && upstream_caps.can_envelope() {
                    envelope_host_action(&mut action, server_name, host_uri);
                    action.title = suffix_title(action.title, server_name);
                    return Some(CodeActionOrCommand::CodeAction(action));
                }
                return disable_action(action, REASON_RESOLVE, server_name, upstream_caps);
            }
            // Nothing actionable (no edit, no command, and either no `data` or a
            // non-resolving virt origin): selecting it would do nothing — drop
            // it rather than clutter the menu.
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

/// Re-apply the `"— {server}"` suffix after a `codeAction/resolve` round-trip.
/// The server normally echoes the original (restored) title back, but LSP lets
/// it change the title during resolve — keep that change and re-suffix it.
/// Fall back to the pre-resolve suffixed title only if the server cleared the
/// title entirely (a suffix-only `"— {server}"` would be meaningless).
fn resuffix_resolved_title(
    server_title: String,
    suffixed_fallback: String,
    server_name: &str,
) -> String {
    if server_title.is_empty() {
        suffixed_fallback
    } else {
        suffix_title(server_title, server_name)
    }
}

/// A resolved edit that cannot be represented in the host document
/// (cross-region, a virtual-URI file op, or an empty edit) is a PERMANENT
/// failure — re-requesting the resolve yields the identical result. Unlike the
/// TRANSIENT fail-softs (stale region, connect/send errors, where the client
/// re-requests and the window is short), the honest outcome here is to disable
/// the action with the `reason` the classifier returned (`REASON_CROSS_REGION`,
/// or `REASON_RESOLVE` for an empty resolved edit), mirroring the initial
/// codeAction path — otherwise the client shows an enabled action that applies
/// nothing.
///
/// `resolved` is the resolve RESPONSE (carrying the unusable edit plus any
/// server-provided title change and already-host-translated diagnostics);
/// `original` is the pre-resolve action (its `data` holds the inner payload,
/// and it carries no edit). The disabled outcome is built from `resolved` so
/// the user sees the server's most accurate title/diagnostics; only a client
/// without `disabledSupport` (which can't render a disabled action, and resolve
/// must return a `CodeAction` — it can't drop like the initial path) falls back
/// to the unresolved-with-envelope `original`, which must never carry the
/// untranslatable edit.
fn resolve_untranslatable_edit(
    mut resolved: CodeAction,
    mut original: CodeAction,
    envelope: &CodeActionEnvelope,
    suffixed_title: String,
    server_name: &str,
    upstream_caps: UpstreamCodeActionCaps,
    reason: &'static str,
) -> CodeAction {
    if !upstream_caps.disabled_support {
        re_envelope_action(&mut original, envelope);
        return original;
    }
    // Disable the RESOLVED action: keep its (resuffixed) title and its
    // already-host-translated diagnostics — both retrieved successfully — and
    // strip only the unusable payload. Mirrors the server-`disabled` branch.
    resolved.title = resuffix_resolved_title(
        std::mem::take(&mut resolved.title),
        suffixed_title,
        server_name,
    );
    resolved.edit = None;
    resolved.command = None;
    resolved.data = None;
    resolved.is_preferred = None;
    resolved.disabled = Some(CodeActionDisabled {
        reason: reason.to_string(),
    });
    resolved
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
        // Without disabledSupport the action vanishes from the menu — the
        // only trace of a server bug (e.g. an out-of-region edit) is this log.
        log::debug!(
            target: "kakehashi::bridge",
            "codeAction '{}' from {server_name} dropped ({reason}); the client \
             lacks disabledSupport to show it disabled",
            action.title
        );
        return None;
    }
    log::debug!(
        target: "kakehashi::bridge",
        "codeAction '{}' from {server_name} disabled: {reason}",
        action.title
    );
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
        transform_impl_with_offset(
            result,
            upstream_caps,
            server_resolves,
            RegionOffset::new(10, 0),
        )
    }

    fn transform_impl_with_offset(
        result: serde_json::Value,
        upstream_caps: UpstreamCodeActionCaps,
        server_resolves: bool,
        offset: RegionOffset,
    ) -> Option<CodeActionResponse> {
        let host_uri = make_host_uri();
        let virtual_uri = make_virtual_uri_string();
        let actions = parse_code_action_response_raw(json!({
            "jsonrpc": "2.0", "id": 42, "result": result
        }))?;
        let virt = VirtLayerContext {
            request_virtual_uri: &virtual_uri,
            host_uri: &host_uri,
            offset: &offset,
            // Generous end: these tests exercise the envelope/title/cross-region
            // policy, not the region-bounds guard (which has its own test), so
            // the region-end must not reject their in-region edits.
            region_end: Position {
                line: u32::MAX,
                character: u32::MAX,
            },
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            injection_language: "lua",
            host_uri_string: "file:///test.md",
            server_name: "ruff",
        };
        Some(bridge_code_actions(
            actions,
            "ruff",
            "file:///test.md",
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
    fn bare_command_is_surfaced_with_a_routed_name() {
        use crate::lsp::bridge::decode_command;
        let actions = transform(
            json!([{ "title": "Run organize imports", "command": "ruff.organizeImports" }]),
            caps(true),
        )
        .unwrap();

        let CodeActionOrCommand::Command(command) = &actions[0] else {
            panic!("Expected an executable Command");
        };
        assert_eq!(command.title, "Run organize imports — ruff");
        // The command name encodes the origin server + host document so
        // executeCommand routes back to it; arguments (none here) stay verbatim.
        let route = decode_command(&command.command).expect("routed name");
        assert_eq!(route.origin, "ruff");
        assert_eq!(route.host_uri, "file:///test.md");
        assert_eq!(route.command, "ruff.organizeImports");
    }

    #[test]
    fn bare_command_is_surfaced_even_without_disabled_support() {
        // A command is executable, not disabled, so disabledSupport is
        // irrelevant — it must still be surfaced.
        let actions = transform(
            json!([{ "title": "Run organize imports", "command": "ruff.organizeImports" }]),
            caps(false),
        )
        .unwrap();
        assert!(matches!(actions[0], CodeActionOrCommand::Command(_)));
    }

    #[test]
    fn edit_command_action_keeps_both_with_a_routed_command() {
        use crate::lsp::bridge::decode_command;
        let mut action = edit_carrying_action("Fix all");
        action["command"] = json!({ "title": "post", "command": "ruff.postFix" });
        let actions = transform(json!([action]), caps(true)).unwrap();

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Fix all — ruff");
        // The client applies the (translated) edit, then executes the routed
        // command — both halves survive.
        assert!(action.edit.is_some(), "the translated edit is kept");
        let route = decode_command(&action.command.as_ref().unwrap().command).expect("routed name");
        assert_eq!(route.origin, "ruff");
        assert_eq!(route.command, "ruff.postFix");
    }

    #[test]
    fn lazy_data_only_action_is_disabled() {
        // Server resolves but the client can't envelope (no data/resolve
        // support); the sync path disables it (eager-resolve covers prod).
        let actions = transform_server_resolves(
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
    fn empty_edit_is_disabled_as_unresolvable_not_cross_region() {
        // An edit that ends up empty (an empty `TextEdit[]` on the request's OWN
        // region — not foreign, not a file op) is a "could not be resolved to an
        // applicable edit" outcome, so it disables with REASON_RESOLVE. Using
        // REASON_CROSS_REGION here (the pre-fix behavior) would misleadingly
        // claim a host-representation problem where none exists (#615 item 1).
        let action = json!({
            "title": "Organize imports",
            "edit": { "changes": { make_virtual_uri_string(): [] } }
        });
        let actions = transform(json!([action]), caps(true)).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.disabled.as_ref().unwrap().reason,
            REASON_RESOLVE,
            "empty edit must disable as unresolvable, not cross-region"
        );
        assert!(action.edit.is_none(), "unusable edit must not leak");
    }

    #[test]
    fn prefix_breaking_edit_disables_the_action() {
        // A blockquote region (`> ` prefix, width 2, on every line): a
        // multi-line replacement's host range contains the interior lines'
        // prefixes, but its verbatim newText carries none — applying it would
        // strip the prefixes and break the blockquote. The action must be
        // disabled with the prefix-specific reason, not applied.
        let action = json!({
            "title": "Organize imports",
            "edit": { "changes": { make_virtual_uri_string(): [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 1, "character": 3 }
                },
                "newText": "import a\nimport b"
            }] } }
        });
        let actions = transform_impl_with_offset(
            json!([action]),
            caps(true),
            false,
            RegionOffset::with_per_line_offsets(10, vec![2, 2]),
        )
        .unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.disabled.as_ref().unwrap().reason,
            REASON_PREFIXED_REGION
        );
        assert!(action.edit.is_none(), "prefix-breaking edit must not leak");
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
    fn host_layer_policy_keeps_edit_uris_and_ranges_untranslated() {
        // Host layer (virt = None): real URIs/coordinates, no translation —
        // but the suffix and command policy still apply (and versions are
        // nulled, pinned separately by host_layer_edit_versions_are_stripped).
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

        let bridged = bridge_code_actions(
            actions,
            "marksman",
            "file:///test.md",
            caps(true),
            false,
            None,
        );
        assert_eq!(bridged.len(), 2);
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Sort imports — marksman");
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let edits = changes.get(&make_host_uri()).unwrap();
        assert_eq!(edits[0].range.start.line, 50, "host edits stay verbatim");
        // The bare command is surfaced executable (host layer routes back to
        // the host server, whose arguments reference the real URI).
        let CodeActionOrCommand::Command(command) = &bridged[1] else {
            panic!("Expected an executable Command");
        };
        assert_eq!(command.title, "Run linter — marksman");
        let route = crate::lsp::bridge::decode_command(&command.command).expect("routed name");
        assert_eq!(route.origin, "marksman");
        assert_eq!(route.command, "lint.run");
    }

    #[test]
    fn host_lazy_action_is_enveloped_for_host_resolve_when_server_resolves() {
        // A data-carrying host lazy action + a host server advertising resolve +
        // an envelope-capable client → enveloped with `host_layer = true` so its
        // resolve routes back to the host server, NOT disabled (#627).
        let actions: Vec<CodeActionOrCommand> =
            serde_json::from_value(json!([{ "title": "Organize imports", "data": { "id": 1 } }]))
                .unwrap();
        let bridged = bridge_code_actions(
            actions,
            "marksman",
            "file:///test.md",
            caps_resolve(),
            true, // host server advertises codeAction/resolve
            None, // host layer
        );
        assert_eq!(bridged.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        assert!(
            action.disabled.is_none(),
            "a resolvable host lazy action must be enveloped, not disabled"
        );
        assert_eq!(action.title, "Organize imports — marksman");
        let env = extract_code_action_envelope(action).expect("host envelope present");
        assert!(
            env.host_layer,
            "host_layer must be set for host-resolve routing"
        );
        assert_eq!(env.origin, "marksman");
        assert_eq!(env.host_uri, "file:///test.md");
        assert_eq!(env.original_title, "Organize imports");
    }

    #[test]
    fn host_lazy_action_is_disabled_when_host_server_does_not_resolve() {
        // Same action, but the host server does NOT advertise resolve → the
        // bridge can't complete it, so it is disabled (pre-#627 behavior).
        let actions: Vec<CodeActionOrCommand> =
            serde_json::from_value(json!([{ "title": "Organize imports", "data": { "id": 1 } }]))
                .unwrap();
        let bridged = bridge_code_actions(
            actions,
            "marksman",
            "file:///test.md",
            caps_resolve(),
            false, // host server does NOT advertise resolve
            None,
        );
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_RESOLVE);
        assert!(extract_code_action_envelope(action).is_none());
    }

    #[test]
    fn envelope_without_host_layer_field_defaults_to_virt() {
        // Backward compat: an envelope serialized before the field existed must
        // deserialize as a virt-layer envelope (host_layer = false).
        let env: CodeActionEnvelope = serde_json::from_value(json!({
            "origin": "ruff",
            "host_uri": "file:///x.md",
            "region_id": "REGION",
            "injection_language": "python",
            "offset": { "line": 0, "column": 0 },
            "original_title": "t",
            "inner": null
        }))
        .expect("deserializes without host_layer");
        assert!(!env.host_layer);
    }

    #[test]
    fn title_only_host_lazy_action_is_enveloped_when_server_resolves() {
        // #615 item 2: a TITLE-ONLY host action (no data/edit/command) from a
        // host server advertising resolve is a resolvable lazy action —
        // enveloped for host-resolve routing, not dropped as clutter.
        let actions: Vec<CodeActionOrCommand> =
            serde_json::from_value(json!([{ "title": "Fix all" }])).unwrap();
        let bridged = bridge_code_actions(
            actions,
            "marksman",
            "file:///test.md",
            caps_resolve(),
            true,
            None,
        );
        assert_eq!(bridged.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        assert!(action.disabled.is_none(), "must be enveloped, not disabled");
        let env = extract_code_action_envelope(action).expect("host envelope present");
        assert!(env.host_layer);
        assert_eq!(env.original_title, "Fix all");
    }

    #[test]
    fn title_only_host_action_is_dropped_when_server_does_not_resolve() {
        // When the host server does not advertise `codeAction/resolve`, a
        // title-only host action can never do anything, so it is dropped (not
        // surfaced as menu clutter).
        let actions: Vec<CodeActionOrCommand> =
            serde_json::from_value(json!([{ "title": "Fix all" }])).unwrap();
        let bridged = bridge_code_actions(
            actions,
            "marksman",
            "file:///test.md",
            caps_resolve(),
            false,
            None,
        );
        assert!(
            bridged.is_empty(),
            "a title-only non-resolvable host action is dropped"
        );
    }

    // ==========================================================================
    // PR 4: envelope + codeAction/resolve
    // ==========================================================================

    #[test]
    fn lazy_action_is_enveloped_for_resolve_capable_client() {
        // With data+resolve support (client) AND a resolve-capable server, a
        // lazy (data-only) action is kept and its `data` wrapped in the
        // routing envelope instead of being disabled.
        let actions = transform_server_resolves(
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
    fn data_only_action_dropped_when_server_cannot_resolve() {
        // A data-carrying action from a server that does NOT advertise resolve
        // can never be resolved (data is only the resolve round-trip payload),
        // so it must be dropped, not enveloped forever.
        let actions = transform(
            json!([{ "title": "Lazy fix", "data": { "id": 7 } }]),
            caps_resolve(),
        )
        .unwrap();
        assert!(actions.is_empty(), "got: {actions:?}");
    }

    #[test]
    fn workspace_edit_is_empty_detects_no_edits() {
        assert!(workspace_edit_is_empty(&WorkspaceEdit::default()));
        assert!(workspace_edit_is_empty(&WorkspaceEdit {
            changes: Some(Default::default()),
            ..Default::default()
        }));
        // A changes map whose only value is an empty TextEdit[] is still empty.
        let vacuous: WorkspaceEdit = serde_json::from_value(json!({
            "changes": { "file:///x.lua": [] }
        }))
        .unwrap();
        assert!(
            workspace_edit_is_empty(&vacuous),
            "a changes map of empty edit vectors is a no-op"
        );
        // A non-empty changes map is not empty.
        let edit: WorkspaceEdit = serde_json::from_value(json!({
            "changes": { "file:///x.lua": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "newText": "y"
            }] }
        }))
        .unwrap();
        assert!(!workspace_edit_is_empty(&edit));
    }

    #[test]
    fn workspace_edit_touches_foreign_region_detects_other_virtual_uris() {
        let host_uri = make_host_uri();
        let own = VirtualDocumentUri::new(&host_uri, "lua", "region-0").to_uri_string();
        let other = VirtualDocumentUri::new(&host_uri, "lua", "region-1").to_uri_string();

        // Own region + a real file: not foreign.
        let clean: WorkspaceEdit = serde_json::from_value(json!({
            "changes": {
                own.clone(): [{ "range": {"start": {"line":0,"character":0}, "end": {"line":0,"character":1}}, "newText": "y" }],
                "file:///real.lua": []
            }
        }))
        .unwrap();
        assert!(!workspace_edit_touches_foreign_region(&clean, &own));

        // A different region's virtual URI carrying a REAL edit is foreign.
        let cross: WorkspaceEdit = serde_json::from_value(json!({
            "changes": {
                own.clone(): [],
                other.clone(): [{ "range": {"start": {"line":0,"character":0}, "end": {"line":0,"character":1}}, "newText": "z" }]
            }
        }))
        .unwrap();
        assert!(workspace_edit_touches_foreign_region(&cross, &own));

        // A foreign key carrying ZERO edits is a vacuous no-op — the transform
        // strips it cleanly — so a mix of a real own-region edit and an EMPTY
        // foreign entry is NOT cross-region (the action must not be rejected).
        let mixed: WorkspaceEdit = serde_json::from_value(json!({
            "changes": {
                own.clone(): [{ "range": {"start": {"line":0,"character":0}, "end": {"line":0,"character":1}}, "newText": "y" }],
                other: []
            }
        }))
        .unwrap();
        assert!(!workspace_edit_touches_foreign_region(&mixed, &own));
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
        let actions = transform_server_resolves(
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

    // -- host resolve finalization -------------------------------------------

    #[test]
    fn host_layer_edit_versions_are_stripped() {
        // Host-layer edits are verbatim EXCEPT versions: the downstream's
        // document versions live in the bridge's version space (its own
        // didOpen counter), so relaying them verbatim makes version-checking
        // editors (Neovim) skip the edit as stale.
        let actions: Vec<CodeActionOrCommand> = serde_json::from_value(json!([
            {
                "title": "Sort imports",
                "edit": {
                    "documentChanges": [{
                        "textDocument": { "uri": "file:///test.md", "version": 2 },
                        "edits": [{
                            "range": {
                                "start": { "line": 50, "character": 0 },
                                "end": { "line": 50, "character": 5 }
                            },
                            "newText": "sorted"
                        }]
                    }]
                }
            }
        ]))
        .unwrap();

        let bridged = bridge_code_actions(
            actions,
            "marksman",
            "file:///test.md",
            caps(true),
            false,
            None,
        );
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        match action.edit.as_ref().unwrap().document_changes.as_ref() {
            Some(DocumentChanges::Edits(edits)) => {
                assert_eq!(
                    edits[0].text_document.version, None,
                    "bridge-local version must not reach the editor"
                );
            }
            other => panic!("Expected Edits variant, got {other:?}"),
        }
    }

    #[test]
    fn strict_translate_strips_bridge_local_versions_on_real_files() {
        // The client-driven virt resolve path reaches the shared strict
        // translate directly (not via bridge_code_action), so the strip must
        // live inside it too: a resolved multi-file edit's OTHER real files
        // carry bridge-local versions that would read as stale to the editor.
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let mut edit: WorkspaceEdit = serde_json::from_value(json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": virtual_uri, "version": 3 },
                    "edits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "x"
                    }]
                },
                {
                    "textDocument": { "uri": "file:///other.lua", "version": 7 },
                    "edits": [{
                        "range": {
                            "start": { "line": 1, "character": 0 },
                            "end": { "line": 1, "character": 5 }
                        },
                        "newText": "x"
                    }]
                }
            ]
        }))
        .unwrap();

        translate_edit_host_ward_strict(
            &mut edit,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
            Position {
                line: u32::MAX,
                character: u32::MAX,
            },
        )
        .expect("edit must translate");

        match edit.document_changes.unwrap() {
            DocumentChanges::Edits(edits) => {
                assert!(
                    edits.iter().all(|e| e.text_document.version.is_none()),
                    "no bridge-local version may reach the editor"
                );
            }
            DocumentChanges::Operations(_) => panic!("Expected Edits variant"),
        }
    }

    #[test]
    fn host_resolve_strips_bridge_local_edit_versions() {
        let resolved: CodeAction = serde_json::from_value(json!({
            "title": "Sort imports",
            "edit": { "documentChanges": [{
                "textDocument": { "uri": "file:///test.md", "version": 3 },
                "edits": [{
                    "range": {
                        "start": { "line": 1, "character": 0 },
                        "end": { "line": 1, "character": 5 }
                    },
                    "newText": "sorted"
                }]
            }] }
        }))
        .unwrap();
        let action: CodeAction = serde_json::from_value(json!({
            "title": "Sort imports — srv", "data": { "id": 1 }
        }))
        .unwrap();
        let envelope: CodeActionEnvelope = serde_json::from_value(json!({
            "origin": "srv",
            "host_uri": "file:///test.md",
            "region_id": "",
            "injection_language": "",
            "offset": { "line": 0, "column": 0 },
            "original_title": "Sort imports",
            "inner": null,
            "host_layer": true
        }))
        .unwrap();

        let out = finalize_host_resolved_action(
            resolved,
            action,
            envelope,
            "Sort imports — srv".to_string(),
            caps_resolve(),
        );

        match out.edit.as_ref().unwrap().document_changes.as_ref() {
            Some(DocumentChanges::Edits(edits)) => {
                assert_eq!(
                    edits[0].text_document.version, None,
                    "bridge-local version must not reach the editor"
                );
            }
            other => panic!("Expected Edits variant, got {other:?}"),
        }
    }

    #[test]
    fn host_resolve_disables_command_only_action_when_routing_name_unencodable() {
        // A command-only resolved action whose routing name can't be encoded
        // (origin contains the routing separator) must NOT surface as an enabled
        // no-op: the command is dropped and, with no edit, the action is
        // disabled as REASON_RESOLVE (regression for the dropped-command gap).
        let resolved: CodeAction = serde_json::from_value(json!({
            "title": "Run fix",
            "command": { "title": "Run fix", "command": "server.fix" }
        }))
        .unwrap();
        let action: CodeAction = serde_json::from_value(json!({
            "title": "Run fix — srv", "data": { "id": 1 }
        }))
        .unwrap();
        let envelope: CodeActionEnvelope = serde_json::from_value(json!({
            "origin": "sr\u{1f}v", // contains SEP → encode_command fails
            "host_uri": "file:///test.md",
            "region_id": "",
            "injection_language": "",
            "offset": { "line": 0, "column": 0 },
            "original_title": "Run fix",
            "inner": null,
            "host_layer": true
        }))
        .unwrap();

        let out = finalize_host_resolved_action(
            resolved,
            action,
            envelope,
            "Run fix — sr\u{1f}v".to_string(),
            caps_resolve(),
        );

        assert_eq!(
            out.disabled
                .as_ref()
                .expect("must be disabled, not an enabled no-op")
                .reason,
            REASON_RESOLVE
        );
        assert!(out.command.is_none(), "unencodable command must be dropped");
        assert!(out.edit.is_none());
    }

    #[test]
    fn host_resolve_keeps_command_only_action_when_routing_name_encodable() {
        // The happy path for the same shape: an encodable origin keeps the
        // command (name-encoded) and stays enabled.
        let resolved: CodeAction = serde_json::from_value(json!({
            "title": "Run fix",
            "command": { "title": "Run fix", "command": "server.fix" }
        }))
        .unwrap();
        let action: CodeAction = serde_json::from_value(json!({
            "title": "Run fix — srv", "data": { "id": 1 }
        }))
        .unwrap();
        let envelope: CodeActionEnvelope = serde_json::from_value(json!({
            "origin": "srv",
            "host_uri": "file:///test.md",
            "region_id": "",
            "injection_language": "",
            "offset": { "line": 0, "column": 0 },
            "original_title": "Run fix",
            "inner": null,
            "host_layer": true
        }))
        .unwrap();

        let out = finalize_host_resolved_action(
            resolved,
            action,
            envelope,
            "Run fix — srv".to_string(),
            caps_resolve(),
        );

        assert!(
            out.disabled.is_none(),
            "an encodable command must stay enabled"
        );
        let command = out.command.expect("command kept");
        let route = crate::lsp::bridge::decode_command(&command.command).expect("routed name");
        assert_eq!(route.origin, "srv");
        assert_eq!(route.command, "server.fix");
    }

    #[test]
    fn host_resolve_re_envelopes_still_lazy_result_with_no_command_and_no_edit() {
        // A resolve that returns NEITHER a command NOR an edit (but never carried
        // a command to begin with) is a still-lazy staged resolve, NOT a no-op:
        // it must be re-enveloped for a further resolve pass, not disabled. This
        // pins the #615 title-only host-lazy path against the dropped-command
        // no-op guard.
        let resolved: CodeAction =
            serde_json::from_value(json!({ "title": "Organize imports" })).unwrap();
        let action: CodeAction = serde_json::from_value(json!({
            "title": "Organize imports — srv", "data": { "id": 1 }
        }))
        .unwrap();
        let envelope: CodeActionEnvelope = serde_json::from_value(json!({
            "origin": "srv",
            "host_uri": "file:///test.md",
            "region_id": "",
            "injection_language": "",
            "offset": { "line": 0, "column": 0 },
            "original_title": "Organize imports",
            "inner": null,
            "host_layer": true
        }))
        .unwrap();

        let out = finalize_host_resolved_action(
            resolved,
            action,
            envelope,
            "Organize imports — srv".to_string(),
            caps_resolve(),
        );

        assert!(
            out.disabled.is_none(),
            "a still-lazy (no-command, no-edit) resolve must be re-enveloped, not disabled"
        );
        let env = extract_code_action_envelope(&out).expect("re-enveloped for a further resolve");
        assert!(env.host_layer);
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
            .dispatch_code_action_resolve(
                action.clone(),
                &settings,
                caps_resolve(),
                None,
                Position::default(),
            )
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
            .dispatch_code_action_resolve(
                action,
                &settings,
                caps_resolve(),
                None,
                Position::default(),
            )
            .await;
        let envelope = extract_code_action_envelope(&result).expect("envelope restored");
        assert_eq!(envelope.origin, "ruff");
        assert_eq!(envelope.inner, Some(json!({ "id": 1 })));
        assert!(result.edit.is_none(), "action stays unresolved");
    }
}
