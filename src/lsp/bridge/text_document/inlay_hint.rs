//! Inlay hint request handling for bridge connections.
//!
//! This module provides inlay hint request functionality for downstream language servers,
//! handling the bidirectional coordinate transformation between host and virtual documents.
//!
//! Unlike position-based requests, inlay hints use a range parameter in the request
//! that specifies the visible document range. Both request range (host->virtual) and
//! response positions/textEdits (virtual->host) need transformation. A hint
//! whose textEdits are unsafe for the injection region (escape it, break
//! per-line `> ` prefixes, or merge content into the closing fence) is served
//! without them (see the safety guard in the response transform).
//!
//! # Resolve routing
//!
//! A hint from an origin that advertises `inlayHint/resolve` (or whose own
//! `data` occupies the reserved key) leaves with its `data` wrapped in an
//! [`InlayHintEnvelope`] naming the producing connection (key and
//! generation), the host incarnation, the content version and the region
//! offset; every other hint leaves bare, and a client resolve of it passes
//! through unchanged. `inlayHint/resolve` strips the envelope, refuses
//! anything but the exact producer, reverses the hint into virtual
//! coordinates for the request, translates the reply back, and merges only
//! the protocol's lazy fields into the original hint.
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;
use std::sync::Arc;

use crate::config::settings::BridgeServerConfig;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{InlayHint, InlayHintLabel, Position, Range, Uri};
use url::Url;

use super::super::pool::{ConnectionKey, LanguageServerPool, UpstreamId};
use tower_lsp_server::ls_types::{
    InlayHintParams, NumberOrString, TextDocumentIdentifier, WorkDoneProgressParams,
};

use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, decode_command, encode_command,
    host_position_within_region_bounds, response_has_jsonrpc_error, text_edit_safe_in_region,
    translate_host_position_to_virtual, translate_host_range_to_virtual,
    translate_virtual_position_to_host, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use super::completion::EnvelopeOffset;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::envelope::{
    ENVELOPE_KEY, nests_reserved_key, should_envelope, wrap_envelope,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct InlayHintEnvelope {
    pub(crate) origin: String,
    pub(crate) host_uri: String,
    pub(crate) region_id: String,
    pub(crate) injection_language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) incarnation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) connection_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) connection_key: Option<ConnectionKey>,
    pub(crate) offset: EnvelopeOffset,
    /// The downstream's own `data`, nested here. Absent — not null — when the
    /// hint carried none, so a bare hint costs nothing extra on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) inner: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) host_layer: bool,
    /// Indices of the label parts whose `location` was translated from the
    /// virtual document when the hint was minted. Only those are reversed on
    /// resolve: a host-document location the downstream returned as such is
    /// a real reference to the host file and must reach it again untouched.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) translated_locations: Vec<usize>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl InlayHintEnvelope {
    pub(crate) fn is_host_layer(&self) -> bool {
        self.host_layer && self.region_id.is_empty()
    }
}

/// What translating a virtual-layer resolve reply back into host coordinates
/// needs: the virtual URI the request went out under (same-region label
/// locations come back under it), the editor-facing host URI, and the live
/// region end for the edit guard.
struct VirtualResolveTarget {
    virtual_uri: String,
    host_lsp_uri: Uri,
    region_end: Position,
}

pub(crate) struct InlayHintDocumentRevision {
    pub(crate) incarnation: Option<u64>,
    pub(crate) content_version: u64,
}

struct InlayHintEnvelopeContext<'a> {
    server_name: &'a str,
    host_uri: &'a str,
    region_id: &'a str,
    injection_language: &'a str,
    incarnation: Option<u64>,
    content_version: Option<u64>,
    connection_generation: u64,
    connection_key: &'a ConnectionKey,
    offset: &'a RegionOffset,
    host_layer: bool,
}

fn envelope_hint_data(
    hint: &mut InlayHint,
    ctx: &InlayHintEnvelopeContext<'_>,
    translated_locations: Vec<usize>,
) {
    let inner = hint.data.take();
    let envelope = InlayHintEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        injection_language: ctx.injection_language.to_string(),
        incarnation: ctx.incarnation,
        content_version: ctx.content_version,
        connection_generation: Some(ctx.connection_generation),
        connection_key: Some(ctx.connection_key.clone()),
        offset: EnvelopeOffset::from(ctx.offset),
        inner: None,
        host_layer: ctx.host_layer,
        translated_locations,
    };
    hint.data = Some(wrap_envelope(&envelope, inner));
}

pub(crate) fn extract_inlay_hint_envelope(hint: &InlayHint) -> Option<InlayHintEnvelope> {
    serde_json::from_value(hint.data.as_ref()?.get(ENVELOPE_KEY)?.clone()).ok()
}

fn strip_inlay_hint_envelope(hint: &mut InlayHint) -> Option<InlayHintEnvelope> {
    let mut envelope = extract_inlay_hint_envelope(hint)?;
    hint.data = envelope.inner.take();
    Some(envelope)
}

/// Restore the envelope around the hint's current `data` (the downstream's
/// payload that `strip_inlay_hint_envelope` put back), so a later resolve
/// still routes.
fn re_envelope_hint(hint: &mut InlayHint, envelope: &InlayHintEnvelope) {
    debug_assert!(
        envelope.inner.is_none(),
        "a stripped envelope carries no inner of its own"
    );
    hint.data = Some(wrap_envelope(envelope, hint.data.take()));
}

pub(crate) fn envelope_host_inlay_hints(
    hints: &mut [InlayHint],
    server_name: &str,
    host_uri: &str,
    revision: InlayHintDocumentRevision,
    connection_generation: u64,
    connection_key: &ConnectionKey,
    server_resolves: bool,
) {
    let offset = RegionOffset::new(0, 0);
    let ctx = InlayHintEnvelopeContext {
        server_name,
        host_uri,
        region_id: "",
        injection_language: "",
        incarnation: revision.incarnation,
        content_version: Some(revision.content_version),
        connection_generation,
        connection_key,
        offset: &offset,
        host_layer: true,
    };
    for hint in hints {
        encode_inlay_hint_commands(hint, connection_key);
        if should_envelope(hint.data.as_ref(), server_resolves) {
            // Host coordinates are forwarded verbatim; nothing is translated.
            envelope_hint_data(hint, &ctx, Vec::new());
        }
    }
}

impl LanguageServerPool {
    /// Send an inlay hint request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing inlay-hint-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_inlay_hint_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_range: Range,
        region_end: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        content_version: u64,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
    ) -> io::Result<Option<Vec<InlayHint>>> {
        let host_incarnation = self.current_host_incarnation(host_uri);
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/inlayHint") {
            return Ok(None);
        }
        // `origin` outlives the request: its key is the envelope's connection
        // key, and resolve support is read from it when the RESPONSE arrives,
        // as the host layer does — a dynamic `resolveProvider` registration
        // can land between send and reply, and the envelope decision should
        // see it.
        let origin = Arc::clone(&handle);
        let connection_key = origin.key();
        let connection_generation = self.document_connection_generation(connection_key);
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_inlay_hint_request(
                    virtual_uri,
                    host_range,
                    &offset,
                    request_id,
                    client_progress_token,
                )
            },
            |response, ctx| {
                transform_inlay_hint_response_to_host_and_envelope(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.offset,
                    region_end,
                    &InlayHintEnvelopeContext {
                        server_name,
                        host_uri: host_uri.as_str(),
                        region_id,
                        injection_language,
                        incarnation: host_incarnation,
                        content_version: Some(content_version),
                        connection_generation,
                        connection_key,
                        offset: ctx.offset,
                        host_layer: false,
                    },
                    origin.has_capability("inlayHint/resolve"),
                )
            },
        )
        .await
    }

    pub(crate) async fn dispatch_inlay_hint_resolve(
        &self,
        mut hint: InlayHint,
        settings: &crate::config::settings::WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
        region_end: Option<Position>,
    ) -> InlayHint {
        let Some(envelope) = strip_inlay_hint_envelope(&mut hint) else {
            return hint;
        };
        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        ) else {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        };
        self.send_inlay_hint_resolve_request(&config, hint, envelope, upstream_id, region_end)
            .await
    }

    async fn send_inlay_hint_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut hint: InlayHint,
        envelope: InlayHintEnvelope,
        upstream_id: Option<UpstreamId>,
        region_end: Option<Position>,
    ) -> InlayHint {
        let server_name = &envelope.origin;
        // One function serves both layers; tag the host one like the
        // completion, codeLens and documentLink paths do.
        let layer = if envelope.is_host_layer() {
            " (host)"
        } else {
            ""
        };
        let Ok(host_uri) = Url::parse(&envelope.host_uri) else {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        };
        if envelope
            .incarnation
            .is_some_and(|expected| self.current_host_incarnation(&host_uri) != Some(expected))
        {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        }
        // The producer stamps generation, key and incarnation on every
        // envelope; a `None` here can only come back from a client-mangled or
        // legacy echo and fails soft like any other stale one.
        let Some(expected_generation) = envelope.connection_generation else {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        };
        let Some(connection_key) = envelope
            .connection_key
            .as_ref()
            .filter(|key| key.server() == server_name)
        else {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        };
        // Lock-free fail-soft for the steady-state stale hint (a generation
        // the key has long moved past); the lookup below re-compares under
        // the `connections` lock, so a purge racing this check cannot hand
        // back the replacement it installed.
        if self.document_connection_generation(connection_key) != expected_generation {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        }
        let handle = match self
            .ready_producer_by_key(connection_key, Some(server_config), expected_generation)
            .await
        {
            Some(handle) => handle,
            None => {
                warn!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve{layer}: failed to connect to {server_name}: producer connection is no longer live"
                );
                re_envelope_hint(&mut hint, &envelope);
                return hint;
            }
        };
        if !handle.has_capability("inlayHint/resolve") {
            // Two ways here. A payload that nests the reserved key was wrapped
            // only to keep it out of the routing metadata, regardless of
            // resolve support, so landing here is its steady state. Anything
            // else was enveloped because the origin resolved at mint time and
            // the capability has since gone (respawn, dynamic unregister).
            if nests_reserved_key(hint.data.as_ref()) {
                debug!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve{layer}: {server_name:?} does not advertise resolveProvider; the hint was \
                     enveloped only to nest a reserved-key payload; returning unresolved"
                );
            } else {
                warn!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve{layer}: {server_name:?} no longer advertises resolveProvider; returning unresolved"
                );
            }
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        }

        // Build the outgoing hint before anything is registered, so a
        // virtual-layer request that cannot be reversed fails soft with
        // nothing to clean up.
        let connection_key = handle.key();
        let mut outgoing = hint.clone();
        if let InlayHintLabel::LabelParts(parts) = &mut outgoing.label {
            for part in parts {
                if let Some(command) = &mut part.command
                    && let Some(route) = decode_command(&command.command)
                    && &route.key == connection_key
                {
                    command.command = route.command.to_string();
                }
            }
        }
        let virt = if envelope.is_host_layer() {
            None
        } else {
            let Some(region_end) = region_end else {
                re_envelope_hint(&mut hint, &envelope);
                return hint;
            };
            // The virtual path needs the editor-facing form of the host URI.
            // `url::Url` accepts characters RFC 3986 forbids (`|`, `^`, a
            // backslash the file scheme folds into `/`), so the `Url` parse
            // above does not vouch for this one, and the envelope is
            // client-echoed data: fail soft.
            let Ok(host_lsp_uri) = envelope.host_uri.parse::<Uri>() else {
                re_envelope_hint(&mut hint, &envelope);
                return hint;
            };
            let virtual_uri =
                reverse_hint_into_virtual(&mut outgoing, &envelope, &host_lsp_uri, region_end);
            Some(VirtualResolveTarget {
                virtual_uri,
                host_lsp_uri,
                region_end,
            })
        };

        // Hold the host lifecycle through enqueue for BOTH layers. A virtual
        // connection can remain live across a host close/reopen, so pointer +
        // connection generation alone do not prevent a stale resolve from
        // being queued after the virtual document's didClose.
        let Some(expected_incarnation) = envelope.incarnation else {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        };
        let _host_lifecycle = match self
            .request_host_lifecycle_for_incarnation(&host_uri, expected_incarnation)
            .await
        {
            Ok(lifecycle) => lifecycle,
            Err(_) => {
                re_envelope_hint(&mut hint, &envelope);
                return hint;
            }
        };

        if let Some(ref id) = upstream_id {
            self.register_upstream_request_for_handle(id.clone(), &handle);
        }
        let (request_id, response_rx) = match handle
            .register_request_with_upstream(upstream_id.clone())
        {
            Ok(pair) => pair,
            Err(error) => {
                warn!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve{layer}: failed to register request for {server_name}: {error}"
                );
                if let Some(ref id) = upstream_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                re_envelope_hint(&mut hint, &envelope);
                return hint;
            }
        };

        let request = build_inlay_hint_resolve_request(&outgoing, request_id);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);
        let send_result = {
            let connections = self.connections().await;
            let producer_is_live = connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle));
            let generation_matches =
                self.document_connection_generation(connection_key) == expected_generation;
            if !producer_is_live || !generation_matches {
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "producer connection was replaced before resolve send",
                ))
            } else {
                handle.send_request(request, request_id).map_err(Into::into)
            }
        };
        if let Err(error) = send_result {
            warn!(
                target: "kakehashi::bridge",
                "inlayHint/resolve{layer}: failed to send request for {server_name}: {error}"
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, connection_key);
            }
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        }

        // FIFO admission is complete once the request is queued. Do not hold
        // didClose/reopen behind a slow downstream response wait.
        drop(_host_lifecycle);
        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, connection_key);
        }
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                warn!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve{layer} failed for server {server_name}: {error}"
                );
                re_envelope_hint(&mut hint, &envelope);
                return hint;
            }
        };
        // The reply came from the exact producer the send-time check bound
        // this request to; a replacement landing after it answered does not
        // make the answer wrong (positions are the hint's own, edits are
        // guarded against the live region below), so, like the sibling
        // resolves, the reply is kept rather than re-checked.
        let Some(mut resolved) = parse_inlay_hint_resolve_response(response) else {
            re_envelope_hint(&mut hint, &envelope);
            return hint;
        };
        match &virt {
            Some(virt) => {
                transform_inlay_hint_to_host(
                    &mut resolved,
                    &virt.virtual_uri,
                    &virt.host_lsp_uri,
                    &RegionOffset::from(&envelope.offset),
                    virt.region_end,
                    connection_key,
                    "inlayHint/resolve",
                );
            }
            None => encode_inlay_hint_commands(&mut resolved, connection_key),
        }
        let mut resolved = merge_resolved_inlay_hint(hint, resolved);
        re_envelope_hint(&mut resolved, &envelope);
        resolved
    }
}

/// Reverse a host-coordinate hint into the virtual document it was produced
/// for, so the resolve request reaches the downstream in the coordinates it
/// minted: the position and accept edits go back through the region offset,
/// and label locations that point into the region of the host document move
/// onto the virtual URI. A host-document location outside the region cannot
/// have been minted by translation — it is a real reference to the host file
/// (a server that also holds the host document may return one) — and stays
/// as it is. Returns the virtual URI; same-region locations in the reply
/// come back under it.
fn reverse_hint_into_virtual(
    outgoing: &mut InlayHint,
    envelope: &InlayHintEnvelope,
    host_lsp_uri: &Uri,
    region_end: Position,
) -> String {
    let offset = RegionOffset::from(&envelope.offset);
    translate_host_position_to_virtual(&mut outgoing.position, &offset);
    if let Some(edits) = &mut outgoing.text_edits {
        for edit in edits {
            translate_host_range_to_virtual(&mut edit.range, &offset);
        }
    }
    let virtual_uri = VirtualDocumentUri::new(
        host_lsp_uri,
        &envelope.injection_language,
        &envelope.region_id,
    );
    let virtual_lsp_uri = virtual_uri_to_lsp_uri(&virtual_uri);
    if let InlayHintLabel::LabelParts(parts) = &mut outgoing.label {
        for part in parts {
            if let Some(location) = &mut part.location
                && location.uri.as_str() == envelope.host_uri
                && host_position_within_region_bounds(location.range.start, &offset, region_end)
                && host_position_within_region_bounds(location.range.end, &offset, region_end)
            {
                location.uri = virtual_lsp_uri.clone();
                translate_host_range_to_virtual(&mut location.range, &offset);
            }
        }
    }
    virtual_uri.to_uri_string()
}

/// Merge only fields that the LSP permits an inlay-hint resolver to fill lazily.
///
/// A resolver may return a partial-looking hint in practice. Keep the eager
/// identity fields from the original response and retain existing lazy values
/// when the resolver omits them. Label parts are paired by index, so a reply
/// whose label has a different shape or part count cannot be paired and
/// leaves the original parts as they were.
///
/// The resolver's `data` is discarded on purpose, unlike the code-lens
/// resolve, which lets a returned `data` replace the original: the protocol
/// carries `data` from the request INTO the resolve, and a hint the editor
/// keeps showing may be resolved again, so the envelope is restored around
/// the payload it was minted with and routes identically the second time.
fn merge_resolved_inlay_hint(mut original: InlayHint, mut resolved: InlayHint) -> InlayHint {
    original.tooltip = resolved.tooltip.take().or(original.tooltip);
    original.text_edits = resolved.text_edits.take().or(original.text_edits);

    match (&mut original.label, resolved.label) {
        (
            InlayHintLabel::LabelParts(original_parts),
            InlayHintLabel::LabelParts(resolved_parts),
        ) if original_parts.len() == resolved_parts.len() => {
            for (original_part, mut resolved_part) in original_parts.iter_mut().zip(resolved_parts)
            {
                original_part.tooltip = resolved_part
                    .tooltip
                    .take()
                    .or(original_part.tooltip.take());
                original_part.location = resolved_part
                    .location
                    .take()
                    .or(original_part.location.take());
                original_part.command = resolved_part
                    .command
                    .take()
                    .or(original_part.command.take());
            }
        }
        (
            InlayHintLabel::LabelParts(original_parts),
            InlayHintLabel::LabelParts(resolved_parts),
        ) => {
            debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: the resolver returned {} label parts for a hint with {}; keeping the original parts",
                resolved_parts.len(),
                original_parts.len()
            );
        }
        (InlayHintLabel::LabelParts(_), InlayHintLabel::String(_)) => {
            debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: the resolver returned a string label for a hint with label parts; keeping the original parts"
            );
        }
        (InlayHintLabel::String(_), InlayHintLabel::LabelParts(_)) => {
            debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: the resolver returned label parts for a hint with a string label; keeping the original label"
            );
        }
        (InlayHintLabel::String(_), InlayHintLabel::String(_)) => {}
    }

    original
}

fn build_inlay_hint_resolve_request(
    hint: &InlayHint,
    request_id: RequestId,
) -> JsonRpcRequest<&InlayHint> {
    JsonRpcRequest::new(request_id.as_i64(), "inlayHint/resolve", hint)
}

fn parse_inlay_hint_resolve_response(mut response: serde_json::Value) -> Option<InlayHint> {
    if response_has_jsonrpc_error(&response, "inlayHint/resolve") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    (!result.is_null())
        .then(|| serde_json::from_value(result).ok())
        .flatten()
}

/// Build a JSON-RPC inlay hint request for a downstream language server.
///
/// Unlike position-based requests, `InlayHintParams` carries a range (the visible
/// document range), translated here from host to virtual coordinates. Line
/// translation uses `saturating_sub` to avoid panicking on underflow when a
/// concurrent edit has invalidated the region data.
fn build_inlay_hint_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    offset: &RegionOffset,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<InlayHintParams> {
    // Translate range from host to virtual coordinates
    let mut virtual_range = host_range;
    translate_host_range_to_virtual(&mut virtual_range, offset);

    let params = InlayHintParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        range: virtual_range,
        // Carry the bridge-minted token so the downstream's `$/progress` routes to
        // this request's aggregator (ls-bridge-client-progress).
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: client_progress_token,
        },
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/inlayHint", params)
}

/// Translate inlay-hint response items from virtual to host coordinates:
/// `position` and any `textEdits` go through `offset`. For `InlayHintLabelPart`
/// labels, each part's `location` is rewritten with the same URI filter as
/// other handlers — keep real files, translate same-virtual-URI matches and
/// swap to the host URI, drop cross-region virtual URIs.
fn transform_inlay_hint_response_to_host_and_envelope(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
    envelope_ctx: &InlayHintEnvelopeContext<'_>,
    server_resolves: bool,
) -> Option<Vec<InlayHint>> {
    if response_has_jsonrpc_error(&response, "textDocument/inlayHint") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse into typed Vec<InlayHint>
    let mut hints: Vec<InlayHint> = serde_json::from_value(result).ok()?;

    for hint in &mut hints {
        let translated_locations = transform_inlay_hint_to_host(
            hint,
            request_virtual_uri,
            host_uri,
            offset,
            region_end,
            envelope_ctx.connection_key,
            "textDocument/inlayHint",
        );

        if should_envelope(hint.data.as_ref(), server_resolves) {
            envelope_hint_data(hint, envelope_ctx, translated_locations);
        }
    }

    Some(hints)
}

fn transform_inlay_hint_to_host(
    hint: &mut InlayHint,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
    connection_key: &ConnectionKey,
    method: &str,
) -> Vec<usize> {
    translate_virtual_position_to_host(&mut hint.position, offset);

    if let Some(text_edits) = &mut hint.text_edits {
        for edit in text_edits.iter_mut() {
            translate_virtual_range_to_host(&mut edit.range, offset);
        }
        if !text_edits
            .iter()
            .all(|edit| text_edit_safe_in_region(edit, offset, region_end))
        {
            log::warn!(
                target: "kakehashi::bridge",
                "{method}: dropped a hint's textEdits ({}): an edit is unsafe for the injection region (escapes it, breaks line prefixes, or merges content into the closing fence)",
                text_edits.len()
            );
            hint.text_edits = None;
        }
    }

    let mut translated_locations = Vec::new();
    if let InlayHintLabel::LabelParts(parts) = &mut hint.label {
        for (index, part) in parts.iter_mut().enumerate() {
            if let Some(command) = &mut part.command {
                command.command = encode_command(connection_key, &command.command);
            }
            let Some(location) = &mut part.location else {
                continue;
            };
            let uri_str = location.uri.as_str();
            if !VirtualDocumentUri::is_virtual_uri(uri_str) {
                continue;
            }
            if uri_str == request_virtual_uri {
                location.uri = host_uri.clone();
                translate_virtual_range_to_host(&mut location.range, offset);
                translated_locations.push(index);
            } else {
                // The value is display text; only the unrepresentable routing
                // target is discarded for a different virtual region.
                part.location = None;
            }
        }
    }
    translated_locations
}

fn encode_inlay_hint_commands(hint: &mut InlayHint, connection_key: &ConnectionKey) {
    if let InlayHintLabel::LabelParts(parts) = &mut hint.label {
        for part in parts {
            if let Some(command) = &mut part.command {
                command.command = encode_command(connection_key, &command.command);
            }
        }
    }
}

#[cfg(test)]
fn transform_inlay_hint_response_to_host(
    response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    region_end: Position,
) -> Option<Vec<InlayHint>> {
    let key = ConnectionKey::shared("test");
    transform_inlay_hint_response_to_host_and_envelope(
        response,
        request_virtual_uri,
        host_uri,
        offset,
        region_end,
        &InlayHintEnvelopeContext {
            server_name: "test",
            host_uri: host_uri.as_str(),
            region_id: "test-region",
            injection_language: "test",
            incarnation: Some(1),
            content_version: Some(1),
            connection_generation: 1,
            connection_key: &key,
            offset,
            host_layer: false,
        },
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    // ==========================================================================
    // Inlay hint request builder tests
    // ==========================================================================

    #[test]
    fn inlay_hint_response_wraps_downstream_data_for_resolve_routing() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let offset = RegionOffset::new(3, 2);
        let key = ConnectionKey::shared("lua-ls");
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": 0, "character": 1 },
                "label": ": number",
                "data": { "token": 7 }
            }]
        });
        let hints = transform_inlay_hint_response_to_host_and_envelope(
            response,
            "file:///kakehashi-virtual-uri-region.lua",
            &host_uri,
            &offset,
            Position {
                line: 4,
                character: 0,
            },
            &InlayHintEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region",
                injection_language: "lua",
                incarnation: Some(2),
                content_version: Some(3),
                connection_generation: 5,
                connection_key: &key,
                offset: &offset,
                host_layer: false,
            },
            true,
        )
        .unwrap();
        let envelope = extract_inlay_hint_envelope(&hints[0]).unwrap();
        assert_eq!(envelope.inner, Some(json!({ "token": 7 })));
        assert_eq!(envelope.connection_key, Some(key));
        assert_eq!(envelope.connection_generation, Some(5));
        assert_eq!(
            hints[0].position,
            Position {
                line: 3,
                character: 3
            }
        );
    }

    #[test]
    fn host_inlay_hints_are_enveloped_only_for_resolvers_or_key_collisions() {
        let key = ConnectionKey::shared("lua-ls");
        let mut resolvable: Vec<InlayHint> = serde_json::from_value(json!([{
            "position": { "line": 0, "character": 1 },
            "label": ": number",
            "data": { "token": 1 }
        }]))
        .unwrap();
        envelope_host_inlay_hints(
            &mut resolvable,
            "lua-ls",
            "file:///test.lua",
            InlayHintDocumentRevision {
                incarnation: Some(2),
                content_version: 3,
            },
            5,
            &key,
            true,
        );
        let envelope = extract_inlay_hint_envelope(&resolvable[0]).unwrap();
        assert!(envelope.is_host_layer());
        assert_eq!(envelope.inner, Some(json!({ "token": 1 })));

        let mut bare: Vec<InlayHint> = serde_json::from_value(json!([{
            "position": { "line": 0, "character": 1 },
            "label": ": number",
            "data": { "token": 2 }
        }]))
        .unwrap();
        envelope_host_inlay_hints(
            &mut bare,
            "lua-ls",
            "file:///test.lua",
            InlayHintDocumentRevision {
                incarnation: Some(2),
                content_version: 3,
            },
            5,
            &ConnectionKey::shared("lua-ls"),
            false,
        );
        assert_eq!(bare[0].data, Some(json!({ "token": 2 })));

        let mut collision: Vec<InlayHint> = serde_json::from_value(json!([{
            "position": { "line": 0, "character": 1 },
            "label": ": number",
            "data": { "kakehashi": { "ownedBy": "downstream" } }
        }]))
        .unwrap();
        envelope_host_inlay_hints(
            &mut collision,
            "lua-ls",
            "file:///test.lua",
            InlayHintDocumentRevision {
                incarnation: Some(2),
                content_version: 3,
            },
            5,
            &ConnectionKey::shared("lua-ls"),
            false,
        );
        assert_eq!(
            extract_inlay_hint_envelope(&collision[0]).unwrap().inner,
            Some(json!({ "kakehashi": { "ownedBy": "downstream" } }))
        );
    }

    /// One way to reach the resolve-side capability miss is an anomaly: the
    /// origin advertised `inlayHint/resolve` when the hint was minted and no
    /// longer does (respawn, dynamic unregister). The hint must come back
    /// unresolved with its payload intact, and the miss must be logged as the
    /// completion, codeLens and documentLink paths log theirs (the
    /// reserved-key way is the sibling test below).
    #[test]
    fn dispatch_warns_and_re_envelopes_when_origin_no_longer_resolves() {
        use crate::lsp::bridge::test_logging::captured_warnings_for;
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let warnings = captured_warnings_for(|| {
            runtime.block_on(async {
                let pool = LanguageServerPool::new();
                let key = ConnectionKey::for_server("lua-ls");
                let handle = crate::lsp::bridge::test_helpers::create_handle_with_key(
                    crate::lsp::bridge::ConnectionState::Ready,
                    key.clone(),
                )
                .await;
                pool.insert_connection(handle).await;
                let host_uri = Url::parse("file:///test.lua").unwrap();
                pool.open_host_incarnation(&host_uri, 2).await;
                let mut settings = crate::config::settings::WorkspaceSettings::default();
                settings.language_servers.insert(
                    "lua-ls".to_string(),
                    BridgeServerConfig {
                        cmd: Some(vec!["lua-language-server".to_string()]),
                        ..Default::default()
                    },
                );
                let mut hints: Vec<InlayHint> = serde_json::from_value(json!([{
                    "position": { "line": 0, "character": 1 },
                    "label": ": number",
                    "data": { "token": 1 }
                }]))
                .unwrap();
                envelope_host_inlay_hints(
                    &mut hints,
                    "lua-ls",
                    "file:///test.lua",
                    InlayHintDocumentRevision {
                        incarnation: Some(2),
                        content_version: 3,
                    },
                    pool.document_connection_generation(&key),
                    &key,
                    true,
                );

                let result = pool
                    .dispatch_inlay_hint_resolve(hints.remove(0), &settings, None, None)
                    .await;
                let envelope = extract_inlay_hint_envelope(&result).expect("envelope restored");
                assert_eq!(envelope.inner, Some(json!({ "token": 1 })));
                assert!(result.tooltip.is_none(), "hint stays unresolved");
            });
        });
        assert!(
            warnings.iter().any(|w| w.contains("inlayHint/resolve")
                && w.contains("no longer advertises resolveProvider")),
            "the capability miss must be logged, not silently swallowed: {warnings:?}"
        );
    }

    /// The other way to reach the capability miss is steady state, not an
    /// anomaly: a NON-resolving origin's payload squatted on the reserved key,
    /// so it was wrapped only to nest it, and every client resolve of that
    /// hint lands here. It must come back unresolved with the payload intact
    /// and must NOT be reported as a lost capability.
    #[test]
    fn dispatch_does_not_warn_for_a_reserved_key_wrap_from_a_non_resolving_origin() {
        use crate::lsp::bridge::test_logging::captured_warnings_for;
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let warnings = captured_warnings_for(|| {
            runtime.block_on(async {
                let pool = LanguageServerPool::new();
                let key = ConnectionKey::for_server("lua-ls");
                let handle = crate::lsp::bridge::test_helpers::create_handle_with_key(
                    crate::lsp::bridge::ConnectionState::Ready,
                    key.clone(),
                )
                .await;
                pool.insert_connection(handle).await;
                let host_uri = Url::parse("file:///test.lua").unwrap();
                pool.open_host_incarnation(&host_uri, 2).await;
                let mut settings = crate::config::settings::WorkspaceSettings::default();
                settings.language_servers.insert(
                    "lua-ls".to_string(),
                    BridgeServerConfig {
                        cmd: Some(vec!["lua-language-server".to_string()]),
                        ..Default::default()
                    },
                );
                let payload = json!({ ENVELOPE_KEY: { "ownedBy": "downstream" } });
                let mut hints: Vec<InlayHint> = serde_json::from_value(json!([{
                    "position": { "line": 0, "character": 1 },
                    "label": ": number",
                    "data": payload
                }]))
                .unwrap();
                envelope_host_inlay_hints(
                    &mut hints,
                    "lua-ls",
                    "file:///test.lua",
                    InlayHintDocumentRevision {
                        incarnation: Some(2),
                        content_version: 3,
                    },
                    pool.document_connection_generation(&key),
                    &key,
                    false,
                );
                let enveloped = hints[0].data.clone();

                let result = pool
                    .dispatch_inlay_hint_resolve(hints.remove(0), &settings, None, None)
                    .await;
                assert_eq!(result.data, enveloped, "wrap restored around the payload");
                assert_eq!(
                    extract_inlay_hint_envelope(&result)
                        .expect("envelope restored")
                        .inner,
                    Some(payload)
                );
            });
        });
        assert!(
            !warnings.iter().any(|w| w.contains("inlayHint/resolve")),
            "a reserved-key wrap is steady state, not a lost capability: {warnings:?}"
        );
    }

    /// The envelope's `host_uri` is data the client echoes back, and
    /// `url::Url` accepts characters RFC 3986 forbids (`|`, `^`, a backslash
    /// that the file scheme folds into `/`), so a host URI that passes the
    /// `Url` parse can still fail the `Uri` parse the virtual-document path
    /// needs. That has to fail soft: a resolve must never take the server
    /// down.
    #[test]
    fn virt_dispatch_fails_soft_when_host_uri_is_not_a_valid_uri() {
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        runtime.block_on(async {
            let pool = LanguageServerPool::new();
            let key = ConnectionKey::for_server("lua-ls");
            let handle =
                crate::lsp::bridge::test_helpers::create_handle_resolving_inlay_hints(key.clone())
                    .await;
            pool.insert_connection(handle).await;
            let host_uri = "file:///test|a.md";
            assert!(
                Url::parse(host_uri).is_ok(),
                "url::Url must accept the raw '|'"
            );
            assert!(
                host_uri.parse::<Uri>().is_err(),
                "the editor-facing Uri must reject the raw '|'"
            );
            pool.open_host_incarnation(&Url::parse(host_uri).unwrap(), 2)
                .await;
            let mut settings = crate::config::settings::WorkspaceSettings::default();
            settings.language_servers.insert(
                "lua-ls".to_string(),
                BridgeServerConfig {
                    cmd: Some(vec!["lua-language-server".to_string()]),
                    ..Default::default()
                },
            );
            let envelope = InlayHintEnvelope {
                origin: "lua-ls".to_string(),
                host_uri: host_uri.to_string(),
                region_id: "region-0".to_string(),
                injection_language: "lua".to_string(),
                incarnation: Some(2),
                content_version: Some(3),
                connection_generation: Some(pool.document_connection_generation(&key)),
                connection_key: Some(key.clone()),
                offset: EnvelopeOffset::from(&RegionOffset::new(1, 0)),
                inner: Some(json!({ "token": 1 })),
                host_layer: false,
                translated_locations: Vec::new(),
            };
            let hint: InlayHint = serde_json::from_value(json!({
                "position": { "line": 1, "character": 1 },
                "label": ": number",
                "data": { ENVELOPE_KEY: envelope }
            }))
            .unwrap();

            let result = pool
                .dispatch_inlay_hint_resolve(hint, &settings, None, Some(Position::new(2, 0)))
                .await;
            assert!(result.tooltip.is_none(), "hint stays unresolved");
            assert_eq!(
                extract_inlay_hint_envelope(&result)
                    .expect("envelope restored")
                    .inner,
                Some(json!({ "token": 1 }))
            );
        });
    }

    #[test]
    fn inlay_hint_resolve_request_carries_original_data() {
        let hint: InlayHint = serde_json::from_value(json!({
            "position": { "line": 1, "character": 2 },
            "label": ": number",
            "data": { "token": 9 }
        }))
        .unwrap();
        let request = build_inlay_hint_resolve_request(&hint, RequestId::new(4));
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["method"], "inlayHint/resolve");
        assert_eq!(value["params"]["data"], json!({ "token": 9 }));
    }

    #[test]
    fn resolved_inlay_hint_updates_only_lazy_fields() {
        let original: InlayHint = serde_json::from_value(json!({
            "position": { "line": 4, "character": 2 },
            "label": [{
                "value": ": original",
                "tooltip": "eager part tooltip",
                "command": { "title": "eager", "command": "eager.command" }
            }],
            "kind": 1,
            "tooltip": "eager tooltip",
            "paddingLeft": true,
            "paddingRight": false,
            "data": { "token": 7 }
        }))
        .unwrap();
        let resolved: InlayHint = serde_json::from_value(json!({
            "position": { "line": 99, "character": 99 },
            "label": [{
                "value": ": replacement",
                "tooltip": "resolved part tooltip"
            }],
            "tooltip": "resolved tooltip",
            "data": { "resolver": "must not replace original data" },
            "textEdits": [{
                "range": {
                    "start": { "line": 4, "character": 0 },
                    "end": { "line": 4, "character": 0 }
                },
                "newText": "resolved"
            }]
        }))
        .unwrap();

        let merged = serde_json::to_value(merge_resolved_inlay_hint(original, resolved)).unwrap();
        assert_eq!(merged["position"], json!({ "line": 4, "character": 2 }));
        assert_eq!(merged["label"][0]["value"], ": original");
        assert_eq!(merged["label"][0]["tooltip"], "resolved part tooltip");
        assert_eq!(merged["label"][0]["command"]["command"], "eager.command");
        assert_eq!(merged["kind"], 1);
        assert_eq!(merged["paddingLeft"], true);
        assert_eq!(merged["paddingRight"], false);
        assert_eq!(merged["tooltip"], "resolved tooltip");
        assert_eq!(merged["textEdits"][0]["newText"], "resolved");
        assert_eq!(merged["data"], json!({ "token": 7 }));
    }

    /// Label parts are paired by index. Give each resolved part a different
    /// lazy field so a reversed or truncated pairing lands on the wrong part.
    #[test]
    fn resolved_label_parts_are_paired_by_index() {
        let original: InlayHint = serde_json::from_value(json!({
            "position": { "line": 0, "character": 0 },
            "label": [{ "value": "first" }, { "value": "second" }]
        }))
        .unwrap();
        let resolved: InlayHint = serde_json::from_value(json!({
            "position": { "line": 0, "character": 0 },
            "label": [
                { "value": "first", "tooltip": "first tooltip" },
                { "value": "second", "location": {
                    "uri": "file:///second.lua",
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 3 } }
                } }
            ]
        }))
        .unwrap();

        let merged = serde_json::to_value(merge_resolved_inlay_hint(original, resolved)).unwrap();
        assert_eq!(merged["label"][0]["tooltip"], "first tooltip");
        assert!(merged["label"][0].get("location").is_none());
        assert!(merged["label"][1].get("tooltip").is_none());
        assert_eq!(merged["label"][1]["location"]["uri"], "file:///second.lua");
    }

    /// A reply whose parts cannot be paired (different count, or a string
    /// label for a parts hint) leaves the original parts untouched rather
    /// than pairing a prefix; the top-level lazy fields still merge.
    #[rstest]
    #[case::fewer_parts(json!([{ "value": "only", "tooltip": "resolved" }]))]
    #[case::string_label(json!("resolved as a string"))]
    fn unpairable_resolved_label_keeps_the_original_parts(#[case] label: serde_json::Value) {
        let original: InlayHint = serde_json::from_value(json!({
            "position": { "line": 0, "character": 0 },
            "label": [
                { "value": "first", "tooltip": "eager first" },
                { "value": "second", "tooltip": "eager second" }
            ]
        }))
        .unwrap();
        let resolved: InlayHint = serde_json::from_value(json!({
            "position": { "line": 0, "character": 0 },
            "label": label,
            "tooltip": "resolved tooltip"
        }))
        .unwrap();

        let merged = serde_json::to_value(merge_resolved_inlay_hint(original, resolved)).unwrap();
        assert_eq!(
            merged["label"],
            json!([
                { "value": "first", "tooltip": "eager first" },
                { "value": "second", "tooltip": "eager second" }
            ])
        );
        assert_eq!(merged["tooltip"], "resolved tooltip");
    }

    /// A resolver that omits a lazy field it is allowed to fill must not
    /// erase the eager value the hint already carried.
    #[test]
    fn omitted_lazy_fields_keep_their_eager_values() {
        let original: InlayHint = serde_json::from_value(json!({
            "position": { "line": 0, "character": 0 },
            "label": [{
                "value": "part",
                "tooltip": "eager part tooltip",
                "location": {
                    "uri": "file:///eager.lua",
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
                },
                "command": { "title": "eager", "command": "eager.command" }
            }],
            "tooltip": "eager tooltip",
            "textEdits": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "newText": "eager"
            }]
        }))
        .unwrap();
        let resolved: InlayHint = serde_json::from_value(json!({
            "position": { "line": 0, "character": 0 },
            "label": [{ "value": "part" }]
        }))
        .unwrap();

        let merged =
            serde_json::to_value(merge_resolved_inlay_hint(original.clone(), resolved)).unwrap();
        assert_eq!(merged, serde_json::to_value(original).unwrap());
    }

    /// The resolve request must reach the downstream in the coordinates it
    /// minted. A blockquote region with a different column on every line is
    /// the shape where a reversal that applies the first line'"'"'s column
    /// everywhere, or drops the column table, shows.
    #[test]
    fn resolve_request_is_reversed_through_per_line_offsets() {
        let envelope = InlayHintEnvelope {
            origin: "lua-ls".to_string(),
            host_uri: "file:///doc.md".to_string(),
            region_id: "region-0".to_string(),
            injection_language: "lua".to_string(),
            incarnation: Some(1),
            content_version: Some(0),
            connection_generation: Some(0),
            connection_key: None,
            offset: EnvelopeOffset {
                line: 4,
                column: 2,
                line_column_offsets: Some(vec![2, 3, 0]),
            },
            inner: None,
            host_layer: false,
            translated_locations: vec![0],
        };
        let mut outgoing: InlayHint = serde_json::from_value(json!({
            "position": { "line": 5, "character": 10 },
            "label": [
                { "value": "same", "location": {
                    "uri": "file:///doc.md",
                    "range": { "start": { "line": 4, "character": 2 }, "end": { "line": 4, "character": 5 } }
                } },
                { "value": "elsewhere", "location": {
                    "uri": "file:///other.lua",
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
                } }
            ],
            "textEdits": [
                {
                    "range": { "start": { "line": 5, "character": 3 }, "end": { "line": 5, "character": 10 } },
                    "newText": "x"
                },
                {
                    "range": { "start": { "line": 6, "character": 4 }, "end": { "line": 6, "character": 6 } },
                    "newText": "y"
                }
            ]
        }))
        .unwrap();
        let host_lsp_uri: Uri = "file:///doc.md".parse().unwrap();

        let virtual_uri =
            reverse_hint_into_virtual(&mut outgoing, &envelope, &host_lsp_uri, Position::new(8, 0));

        let reversed = serde_json::to_value(&outgoing).unwrap();
        assert_eq!(reversed["position"], json!({ "line": 1, "character": 7 }));
        assert_eq!(
            reversed["textEdits"][0]["range"],
            json!({ "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 7 } })
        );
        assert_eq!(
            reversed["textEdits"][1]["range"],
            json!({ "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 6 } })
        );
        assert!(virtual_uri.contains("region-0"), "{virtual_uri}");
        assert_eq!(reversed["label"][0]["location"]["uri"], virtual_uri);
        assert_eq!(
            reversed["label"][0]["location"]["range"],
            json!({ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } })
        );
        assert_eq!(reversed["label"][1]["location"]["uri"], "file:///other.lua");
        assert_eq!(
            reversed["label"][1]["location"]["range"]["start"],
            json!({ "line": 0, "character": 0 })
        );
    }

    /// A label location may legitimately name the host file itself (a
    /// server that also holds the host document): only a host location
    /// inside the region was minted by translating a virtual one, so only
    /// that is reversed; one outside the region is a real reference and
    /// must reach the downstream untouched.
    #[test]
    fn resolve_request_leaves_a_host_location_outside_the_region_alone() {
        let envelope = InlayHintEnvelope {
            origin: "lua-ls".to_string(),
            host_uri: "file:///doc.md".to_string(),
            region_id: "region-0".to_string(),
            injection_language: "lua".to_string(),
            incarnation: Some(1),
            content_version: Some(0),
            connection_generation: Some(0),
            connection_key: None,
            offset: EnvelopeOffset {
                line: 4,
                column: 0,
                line_column_offsets: None,
            },
            inner: None,
            host_layer: false,
            translated_locations: vec![0],
        };
        let mut outgoing: InlayHint = serde_json::from_value(json!({
            "position": { "line": 4, "character": 1 },
            "label": [
                { "value": "inside", "location": {
                    "uri": "file:///doc.md",
                    "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 5, "character": 3 } }
                } },
                { "value": "outside", "location": {
                    "uri": "file:///doc.md",
                    "range": { "start": { "line": 20, "character": 0 }, "end": { "line": 20, "character": 3 } }
                } }
            ]
        }))
        .unwrap();
        let host_lsp_uri: Uri = "file:///doc.md".parse().unwrap();

        let virtual_uri =
            reverse_hint_into_virtual(&mut outgoing, &envelope, &host_lsp_uri, Position::new(8, 0));

        let reversed = serde_json::to_value(&outgoing).unwrap();
        assert_eq!(reversed["label"][0]["location"]["uri"], virtual_uri);
        assert_eq!(
            reversed["label"][0]["location"]["range"]["start"],
            json!({ "line": 1, "character": 0 })
        );
        assert_eq!(reversed["label"][1]["location"]["uri"], "file:///doc.md");
        assert_eq!(
            reversed["label"][1]["location"]["range"]["start"],
            json!({ "line": 20, "character": 0 })
        );
    }

    #[test]
    fn inlay_hint_request_uses_virtual_uri() {
        let host_uri = test_host_uri();
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 10,
                character: 0,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 20,
                character: 0,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(5, 0),
            RequestId::new(1),
            None,
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn inlay_hint_request_translates_range_to_virtual_coordinates() {
        let host_uri = test_host_uri();
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 10,
                character: 5,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 20,
                character: 30,
            },
        };
        let region_start_line = 8;
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(region_start_line, 0),
            RequestId::new(42),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
        assert_eq!(json["method"], "textDocument/inlayHint");
        // Range translated: line 10 - 8 = 2, line 20 - 8 = 12
        assert_eq!(json["params"]["range"]["start"]["line"], 2);
        assert_eq!(json["params"]["range"]["start"]["character"], 5);
        assert_eq!(json["params"]["range"]["end"]["line"], 12);
        assert_eq!(json["params"]["range"]["end"]["character"], 30);
    }

    #[test]
    fn inlay_hint_request_carries_work_done_token_only_when_present() {
        let host_uri = test_host_uri();
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 1,
                character: 0,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 2,
                character: 0,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");

        // With a token: present in params.
        let with = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(0, 0),
            RequestId::new(1),
            Some(NumberOrString::String("cprog-1".to_string())),
        );
        assert_eq!(
            serde_json::to_value(&with).unwrap()["params"]["workDoneToken"],
            "cprog-1"
        );

        // Without a token: the field is omitted (non-regressing).
        let without = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(0, 0),
            RequestId::new(1),
            None,
        );
        assert!(
            serde_json::to_value(&without).unwrap()["params"]
                .get("workDoneToken")
                .is_none(),
            "None omits the token"
        );
    }

    #[test]
    fn inlay_hint_request_first_line_applies_column_offset() {
        let host_uri = test_host_uri();
        // Host range starts on first line of region (line 5), region starts at col 4
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 5,
                character: 10,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 8,
                character: 15,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        // Start: virtual line 0 -> character 10 - 4 = 6
        assert_eq!(json["params"]["range"]["start"]["line"], 0);
        assert_eq!(json["params"]["range"]["start"]["character"], 6);
        // End: virtual line 3 -> character unchanged
        assert_eq!(json["params"]["range"]["end"]["line"], 3);
        assert_eq!(json["params"]["range"]["end"]["character"], 15);
    }

    #[test]
    fn inlay_hint_request_non_first_line_ignores_column_offset() {
        let host_uri = test_host_uri();
        // Host range starts on non-first line of region
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 7,
                character: 10,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 9,
                character: 15,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        // Start: virtual line 2 -> character unchanged
        assert_eq!(json["params"]["range"]["start"]["line"], 2);
        assert_eq!(json["params"]["range"]["start"]["character"], 10);
        // End: virtual line 4 -> character unchanged
        assert_eq!(json["params"]["range"]["end"]["line"], 4);
        assert_eq!(json["params"]["range"]["end"]["character"], 15);
    }

    #[test]
    fn inlay_hint_request_range_saturates_at_zero() {
        let host_uri = test_host_uri();
        // Range starts before region_start_line (race condition scenario)
        let host_range = Range {
            start: tower_lsp_server::ls_types::Position {
                line: 2,
                character: 0,
            },
            end: tower_lsp_server::ls_types::Position {
                line: 5,
                character: 0,
            },
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_inlay_hint_request(
            &virtual_uri,
            host_range,
            &RegionOffset::new(10, 0),
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        // saturating_sub: 2 - 10 = 0, 5 - 10 = 0
        assert_eq!(json["params"]["range"]["start"]["line"], 0);
        assert_eq!(json["params"]["range"]["end"]["line"], 0);
    }

    // ==========================================================================
    // Inlay hint response transformation tests
    // ==========================================================================

    fn make_host_uri() -> Uri {
        use url::Url;
        crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        let host_uri = make_host_uri();
        VirtualDocumentUri::new(&host_uri, "lua", "region-0").to_uri_string()
    }

    #[test]
    fn inlay_hint_response_transforms_positions_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "position": { "line": 0, "character": 10 },
                    "label": "string"
                },
                {
                    "position": { "line": 2, "character": 15 },
                    "label": "number",
                    "kind": 1
                }
            ]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(5, 0),
            TEST_REGION_END,
        );

        let hints = hints.unwrap();
        assert_eq!(hints.len(), 2);
        // line 0 + 5 = 5
        assert_eq!(hints[0].position.line, 5);
        assert_eq!(hints[0].position.character, 10);
        // line 2 + 5 = 7
        assert_eq!(hints[1].position.line, 7);
        assert_eq!(hints[1].position.character, 15);
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::without_result(json!({"jsonrpc": "2.0", "id": 42}))]
    fn inlay_hint_response_returns_none_for_invalid_response(#[case] response: serde_json::Value) {
        let result = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(5, 0),
            TEST_REGION_END,
        );
        assert!(result.is_none());
    }

    #[test]
    fn inlay_hint_response_with_empty_array_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(5, 0),
            TEST_REGION_END,
        );

        assert!(hints.is_some());
        assert!(hints.unwrap().is_empty());
    }

    #[test]
    fn inlay_hint_response_transforms_text_edits_ranges() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": 0, "character": 10 },
                "label": ": string",
                "textEdits": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 10 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "newText": ": string"
                    },
                    {
                        "range": {
                            "start": { "line": 3, "character": 0 },
                            "end": { "line": 4, "character": 5 }
                        },
                        "newText": "second"
                    }
                ]
            }]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(5, 0),
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(hints[0].position.line, 5);
        let edits = hints[0].text_edits.as_ref().unwrap();
        assert_eq!(edits.len(), 2);
        // First edit: line 0 + 5 = 5
        assert_eq!(edits[0].range.start.line, 5);
        assert_eq!(edits[0].range.end.line, 5);
        assert_eq!(edits[0].new_text, ": string");
        // Second edit: line 3 + 5 = 8, line 4 + 5 = 9
        assert_eq!(edits[1].range.start.line, 8);
        assert_eq!(edits[1].range.end.line, 9);
    }

    #[test]
    fn inlay_hint_label_parts_same_virtual_uri_transforms_location() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": 0, "character": 10 },
                "label": [
                    {
                        "value": "SomeType",
                        "command": {
                            "title": "Open",
                            "command": "mock.open"
                        },
                        "location": {
                            "uri": virtual_uri,
                            "range": {
                                "start": { "line": 5, "character": 0 },
                                "end": { "line": 5, "character": 8 }
                            }
                        }
                    }
                ]
            }]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
            TEST_REGION_END,
        )
        .unwrap();

        assert_eq!(hints[0].position.line, 10);
        if let InlayHintLabel::LabelParts(parts) = &hints[0].label {
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0].value, "SomeType");
            let loc = parts[0].location.as_ref().unwrap();
            // URI replaced with host URI
            assert_eq!(loc.uri, host_uri);
            // Range transformed: line 5 + 10 = 15
            assert_eq!(loc.range.start.line, 15);
            assert_eq!(loc.range.end.line, 15);
            let command = parts[0].command.as_ref().unwrap();
            let route = decode_command(&command.command).expect("routed command");
            assert_eq!(route.key, ConnectionKey::shared("test"));
            assert_eq!(route.command, "mock.open");
        } else {
            panic!("Expected LabelParts variant");
        }
    }

    #[test]
    fn inlay_hint_label_parts_real_file_uri_preserved_unchanged() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        let real_file_uri = "file:///usr/local/lib/lua/5.4/types.lua";

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": 0, "character": 10 },
                "label": [
                    {
                        "value": "ExternalType",
                        "location": {
                            "uri": real_file_uri,
                            "range": {
                                "start": { "line": 100, "character": 0 },
                                "end": { "line": 100, "character": 12 }
                            }
                        }
                    }
                ]
            }]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
            TEST_REGION_END,
        )
        .unwrap();

        if let InlayHintLabel::LabelParts(parts) = &hints[0].label {
            assert_eq!(parts.len(), 1);
            let loc = parts[0].location.as_ref().unwrap();
            // Real file URI preserved as-is
            assert_eq!(loc.uri.as_str(), real_file_uri);
            // Range NOT transformed (it's a real file)
            assert_eq!(loc.range.start.line, 100);
        } else {
            panic!("Expected LabelParts variant");
        }
    }

    #[test]
    fn inlay_hint_label_parts_cross_region_drops_only_location() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        // Different region — build from the same host but different region_id
        let different_virtual_uri =
            VirtualDocumentUri::new(&host_uri, "lua", "region-1").to_uri_string();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": 0, "character": 10 },
                "label": [
                    {
                        "value": "SameRegion",
                        "location": {
                            "uri": virtual_uri,
                            "range": {
                                "start": { "line": 2, "character": 0 },
                                "end": { "line": 2, "character": 10 }
                            }
                        }
                    },
                    {
                        "value": "CrossRegion",
                        "location": {
                            "uri": different_virtual_uri,
                            "range": {
                                "start": { "line": 5, "character": 0 },
                                "end": { "line": 5, "character": 11 }
                            }
                        }
                    }
                ]
            }]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
            TEST_REGION_END,
        )
        .unwrap();

        if let InlayHintLabel::LabelParts(parts) = &hints[0].label {
            assert_eq!(parts.len(), 2, "display label parts must be preserved");
            assert_eq!(parts[0].value, "SameRegion");
            let loc = parts[0].location.as_ref().unwrap();
            assert_eq!(loc.uri, host_uri);
            assert_eq!(loc.range.start.line, 12); // 2 + 10
            assert_eq!(parts[1].value, "CrossRegion");
            assert!(parts[1].location.is_none());
        } else {
            panic!("Expected LabelParts variant");
        }
    }

    #[test]
    fn inlay_hint_response_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": u32::MAX, "character": 10 },
                "label": "hint",
                "textEdits": [{
                    "range": {
                        "start": { "line": u32::MAX, "character": 0 },
                        "end": { "line": u32::MAX, "character": 5 }
                    },
                    "newText": "edit"
                }]
            }]
        });
        let region_start_line = 10;

        let hints = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
        );

        assert!(hints.is_some());
        let hints = hints.unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(
            hints[0].position.line,
            u32::MAX,
            "Position line overflow should saturate at u32::MAX, not panic"
        );
        let edits = hints[0].text_edits.as_ref().unwrap();
        assert_eq!(
            edits[0].range.start.line,
            u32::MAX,
            "TextEdit start line overflow should saturate at u32::MAX, not panic"
        );
        assert_eq!(
            edits[0].range.end.line,
            u32::MAX,
            "TextEdit end line overflow should saturate at u32::MAX, not panic"
        );
    }

    #[test]
    fn inlay_hint_label_parts_without_location_preserved() {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();

        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "position": { "line": 0, "character": 10 },
                "label": [
                    {
                        "value": "SimpleHint",
                        "tooltip": "A tooltip"
                    },
                    {
                        "value": " -> ",
                        "command": { "title": "Do something", "command": "action" }
                    }
                ]
            }]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
            TEST_REGION_END,
        )
        .unwrap();

        if let InlayHintLabel::LabelParts(parts) = &hints[0].label {
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0].value, "SimpleHint");
            assert!(parts[0].location.is_none());
            assert_eq!(parts[1].value, " -> ");
            assert!(parts[1].location.is_none());
        } else {
            panic!("Expected LabelParts variant");
        }
    }

    #[test]
    fn inlay_hint_drops_all_text_edits_when_any_breaks_prefixes() {
        // Blockquote region, content host lines 3-4, region end (5, 0). A
        // hint textEdit spanning prefixed lines would strip the `> ` prefix
        // when accept-applied; the accept set applies atomically, so ALL
        // textEdits drop while the (display-only) hint itself is kept.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                {
                    "position": { "line": 1, "character": 4 },
                    "label": ": number",
                    "textEdits": [
                        { "range": { "start": { "line": 0, "character": 0 },
                                     "end": { "line": 1, "character": 2 } },
                          "newText": "a\nb" },
                        { "range": { "start": { "line": 1, "character": 5 },
                                     "end": { "line": 1, "character": 5 } },
                          "newText": ": number" }
                    ]
                }
            ]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &offset,
            region_end,
        )
        .unwrap();

        assert_eq!(hints.len(), 1, "the hint itself is kept");
        assert!(
            hints[0].text_edits.is_none(),
            "one unsafe edit drops the WHOLE accept-edit set (it applies atomically): {:?}",
            hints[0].text_edits
        );
    }

    #[test]
    fn inlay_hint_drops_text_edits_that_escape_the_region() {
        // Plain fenced region (all-zero offsets), region end (5, 0): a
        // textEdit whose stale range translates past the closing fence must
        // drop the accept-edit set even though the prefix rules fast-path.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                {
                    "position": { "line": 1, "character": 4 },
                    "label": ": number",
                    "textEdits": [
                        { "range": { "start": { "line": 0, "character": 0 },
                                     "end": { "line": 9, "character": 0 } },
                          "newText": "x" }
                    ]
                }
            ]
        });

        let hints = transform_inlay_hint_response_to_host(
            response,
            &make_virtual_uri_string(),
            &make_host_uri(),
            &offset,
            region_end,
        )
        .unwrap();

        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].text_edits.is_none(),
            "region-escaping accept edits must drop: {:?}",
            hints[0].text_edits
        );
    }
}
