//! Call hierarchy request handling for bridge connections (#353).
//!
//! Bridges the three call-hierarchy methods to the downstream language server
//! that owns the injection region under the cursor, translating host/virtual
//! coordinates in both directions:
//!
//! - `textDocument/prepareCallHierarchy` is position-based (like
//!   `textDocument/declaration`): resolve the region at the position, fan out to
//!   the preferred server, and translate the returned items' `uri`, `range`, and
//!   `selectionRange` from virtual → host. Every returned item carries a routing
//!   envelope in its `data` so a later incoming/outgoing call can be routed back
//!   to the origin server (the `codeLens/resolve` envelope pattern).
//! - `callHierarchy/incomingCalls` / `callHierarchy/outgoingCalls` take a
//!   `CallHierarchyItem` param produced by a previous step. The envelope on
//!   `item.data` identifies the origin server and region; the item is translated
//!   host → virtual and forwarded, and the response items are translated
//!   virtual → host (and re-enveloped, since the editor can expand any returned
//!   item again).
//!
//! # Scope (VIRT layer only)
//!
//! Like `textDocument/codeAction`, this bridges only the injection (virt) layer;
//! host-layer fan-out (bridging the host document itself with its real URI) is a
//! follow-up. All three send paths gate on the single
//! `textDocument/prepareCallHierarchy` capability — incoming/outgoing reuse the
//! same connection and have no capability of their own (LSP 3.18).
//!
//! # Coordinate-translation direction
//!
//! Downstream responses are in VIRTUAL coordinates → translated to HOST before
//! returning to the editor. Request params (the item handed to
//! incoming/outgoing) are in HOST coordinates → translated to VIRTUAL before
//! sending downstream.
//!
//! # Cross-region / cross-file items
//!
//! A returned item whose `uri` is not the request's own region URI (a different
//! virtual region, OR a real file) is DROPPED: its coordinates cannot be safely
//! translated with this region's offset, and the envelope round-trip assumes
//! every emitted item belongs to the request's region. This errs in the safe
//! direction (callable items stay within the injection's host document). Keeping
//! cross-file items is a follow-up.
//!
//! # Fail-soft
//!
//! Every failure (missing region/config, connection/parse error, stale region)
//! returns empty/None — virtual coordinates never leak to the editor.

use std::io;
use std::sync::Arc;

use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, NumberOrString, Range,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_text_document_position_params, response_has_jsonrpc_error,
    translate_host_range_to_virtual, translate_virtual_range_to_host,
};
use super::completion::EnvelopeOffset;
use crate::config::settings::{BridgeServerConfig, WorkspaceSettings};
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use tower_lsp_server::ls_types::Position;

/// Wrapper key inside `CallHierarchyItem.data` that identifies the origin server.
const ENVELOPE_KEY: &str = "kakehashi";

/// Envelope stored in `CallHierarchyItem.data` for routing incoming/outgoing
/// calls back to the origin server (#353).
///
/// Carries everything resolve-time routing and freshness checking need: the
/// origin server, the host document and region the item came from, the
/// injection language (needed to rebuild the virtual URI), the offset snapshot
/// for coordinate translation, and the downstream's original `data`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CallHierarchyEnvelope {
    /// Server name identifying which downstream produced the item.
    pub(crate) origin: String,
    /// Host document URI the item belongs to (incoming/outgoing params carry no
    /// textDocument, so the envelope must).
    pub(crate) host_uri: String,
    /// ULID of the injection region the item came from; freshness checks look it
    /// up in the node tracker (fail-soft when invalidated).
    pub(crate) region_id: String,
    /// Injection language name — needed to rebuild the virtual URI for the
    /// send path and the "own region" match in the response filter.
    pub(crate) injection_language: String,
    /// Region offset snapshot for coordinate translation.
    pub(crate) offset: EnvelopeOffset,
    /// The downstream server's original `data` value (preserved verbatim).
    pub(crate) inner: Option<Value>,
}

/// Context needed to create envelopes during response processing.
struct CallHierarchyEnvelopeContext<'a> {
    server_name: &'a str,
    host_uri: &'a str,
    region_id: &'a str,
    injection_language: &'a str,
    offset: &'a RegionOffset,
}

impl CallHierarchyEnvelope {
    /// The virtual URI string this envelope's item belongs to. Used as the
    /// "own region" match target when filtering response items.
    fn virtual_uri_string(&self) -> Option<String> {
        let host_uri = Url::parse(&self.host_uri).ok()?;
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(&host_uri).ok()?;
        Some(
            VirtualDocumentUri::new(&host_uri_lsp, &self.injection_language, &self.region_id)
                .to_uri_string(),
        )
    }
}

/// Wrap `item.data` in a Kakehashi envelope for origin tracking.
fn envelope_item_data(item: &mut CallHierarchyItem, ctx: &CallHierarchyEnvelopeContext) {
    let inner = item.data.take();
    let envelope = CallHierarchyEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        injection_language: ctx.injection_language.to_string(),
        offset: EnvelopeOffset::from(ctx.offset),
        inner,
    };
    item.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

/// Extract the envelope from an item's `data` without modifying the item.
///
/// Returns `None` if `data` is absent or not an envelope.
pub(crate) fn extract_call_hierarchy_envelope(
    item: &CallHierarchyItem,
) -> Option<CallHierarchyEnvelope> {
    let data = item.data.as_ref()?;
    let wrapper = data.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

/// Extract the envelope and restore the original `data` value.
///
/// On success, `item.data` is set back to the downstream's original value
/// (`inner`). Returns `None` if not an envelope (item unchanged).
fn strip_call_hierarchy_envelope(item: &mut CallHierarchyItem) -> Option<CallHierarchyEnvelope> {
    let mut envelope = extract_call_hierarchy_envelope(item)?;
    item.data = envelope.inner.take();
    Some(envelope)
}

/// The host URI as an `ls_types::Uri`, used when translating an own-region item
/// back to the host document.
fn host_uri_lsp(host_uri: &str) -> Option<tower_lsp_server::ls_types::Uri> {
    let url = Url::parse(host_uri).ok()?;
    crate::lsp::lsp_impl::url_to_uri(&url).ok()
}

/// Translate a single returned item from virtual → host coordinates and wrap its
/// `data` in a fresh envelope. Returns `None` to DROP cross-region / cross-file
/// items (see module header).
fn transform_item_to_host(
    mut item: CallHierarchyItem,
    request_virtual_uri: &str,
    ctx: &CallHierarchyEnvelopeContext,
) -> Option<CallHierarchyItem> {
    // Only the request's own region is translatable here; anything else (a
    // different virtual region or a real file) is dropped.
    if item.uri.as_str() != request_virtual_uri {
        return None;
    }
    let host_uri = host_uri_lsp(ctx.host_uri)?;
    item.uri = host_uri;
    translate_virtual_range_to_host(&mut item.range, ctx.offset);
    translate_virtual_range_to_host(&mut item.selection_range, ctx.offset);
    envelope_item_data(&mut item, ctx);
    Some(item)
}

/// Translate every range in a `from_ranges` list virtual → host in place.
fn translate_from_ranges_to_host(ranges: &mut [Range], offset: &RegionOffset) {
    for range in ranges {
        translate_virtual_range_to_host(range, offset);
    }
}

/// Translate a forwarded item HOST → VIRTUAL before sending it downstream:
/// repoint its `uri` at the region's virtual document and translate both ranges
/// with the envelope's offset snapshot. Pure (no I/O) so the request-side
/// coordinate path is unit-testable (the inverse of `transform_item_to_host`).
fn translate_item_to_virtual(item: &mut CallHierarchyItem, envelope: &CallHierarchyEnvelope) {
    let offset = RegionOffset::from(&envelope.offset);
    if let Some(virtual_uri) = envelope.virtual_uri_string()
        && let Ok(uri) = tower_lsp_server::ls_types::Uri::from_str(&virtual_uri)
    {
        item.uri = uri;
    }
    translate_host_range_to_virtual(&mut item.range, &offset);
    translate_host_range_to_virtual(&mut item.selection_range, &offset);
}

impl LanguageServerPool {
    /// Send a `textDocument/prepareCallHierarchy` request and transform the
    /// response to host coordinates (each item enveloped for later routing).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_prepare_call_hierarchy_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/prepareCallHierarchy") {
            return Ok(None);
        }
        let host_uri_string = host_uri.to_string();
        self.execute_position_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            host_position,
            "textDocument/prepareCallHierarchy",
            |virtual_uri, request_id| {
                build_prepare_call_hierarchy_request(
                    virtual_uri,
                    host_position,
                    &offset,
                    request_id,
                    client_progress_token,
                )
            },
            |response, ctx| {
                let envelope_ctx = CallHierarchyEnvelopeContext {
                    server_name,
                    host_uri: &host_uri_string,
                    region_id,
                    injection_language,
                    offset: ctx.offset,
                };
                transform_prepare_response_to_host(response, &ctx.virtual_uri_string, &envelope_ctx)
            },
        )
        .await
    }

    /// Route a `callHierarchy/incomingCalls` request to the origin server.
    ///
    /// Strips the envelope from `item` to identify the origin server and region,
    /// translates the item host → virtual, forwards it, and transforms the
    /// response items virtual → host (re-enveloping each). Fails soft to `None`.
    /// The caller (lsp_impl) has already verified the region is fresh.
    pub(crate) async fn dispatch_incoming_calls(
        &self,
        mut item: CallHierarchyItem,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<CallHierarchyIncomingCall>> {
        let envelope = strip_call_hierarchy_envelope(&mut item)?;
        let config = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        )?;
        self.send_incoming_calls_request(&config, item, envelope, upstream_id)
            .await
    }

    /// Route a `callHierarchy/outgoingCalls` request to the origin server.
    pub(crate) async fn dispatch_outgoing_calls(
        &self,
        mut item: CallHierarchyItem,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<CallHierarchyOutgoingCall>> {
        let envelope = strip_call_hierarchy_envelope(&mut item)?;
        let config = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        )?;
        self.send_outgoing_calls_request(&config, item, envelope, upstream_id)
            .await
    }

    async fn send_incoming_calls_request(
        &self,
        server_config: &BridgeServerConfig,
        item: CallHierarchyItem,
        envelope: CallHierarchyEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<CallHierarchyIncomingCall>> {
        let response = self
            .send_call_hierarchy_resolve(
                "callHierarchy/incomingCalls",
                server_config,
                item,
                &envelope,
                upstream_id,
            )
            .await?;
        transform_incoming_response_to_host(response, &envelope)
    }

    async fn send_outgoing_calls_request(
        &self,
        server_config: &BridgeServerConfig,
        item: CallHierarchyItem,
        envelope: CallHierarchyEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<CallHierarchyOutgoingCall>> {
        let response = self
            .send_call_hierarchy_resolve(
                "callHierarchy/outgoingCalls",
                server_config,
                item,
                &envelope,
                upstream_id,
            )
            .await?;
        transform_outgoing_response_to_host(response, &envelope)
    }

    /// Shared transport for incoming/outgoing: connect, translate the item host
    /// → virtual, send `method` with the unwrapped item, return the raw JSON-RPC
    /// response (or `None` on any failure — fail-soft).
    async fn send_call_hierarchy_resolve(
        &self,
        method: &'static str,
        server_config: &BridgeServerConfig,
        mut item: CallHierarchyItem,
        envelope: &CallHierarchyEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> Option<serde_json::Value> {
        let server_name = &envelope.origin;
        let host_uri = Url::parse(&envelope.host_uri).ok();
        let handle = match self
            .get_or_create_connection(server_name, server_config, host_uri.as_ref())
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "{}: failed to connect to {}: {}", method, server_name, e
                );
                return None;
            }
        };

        // Both incoming/outgoing are gated by the prepare capability (LSP 3.18).
        if !handle.has_capability("textDocument/prepareCallHierarchy") {
            return None;
        }

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
                        "{}: failed to register request for {}: {}", method, server_name, e
                    );
                    if let Some(ref id) = upstream_id {
                        self.unregister_upstream_request(id, connection_key);
                    }
                    return None;
                }
            };

        // The item is in HOST coordinates; translate it to VIRTUAL and point its
        // uri at the virtual document before sending. `item.data` already holds
        // the downstream's original value (the caller stripped the envelope).
        translate_item_to_virtual(&mut item, envelope);
        let request = build_call_hierarchy_resolve_request(method, &item, request_id);

        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        if let Err(e) = handle.send_request(request, request_id) {
            warn!(
                target: "kakehashi::bridge",
                "{}: failed to send request for {}: {}", method, server_name, e
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

        match response {
            Ok(r) => Some(r),
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "{} failed for server {}: {}", method, server_name, e
                );
                None
            }
        }
    }
}

use std::str::FromStr;

/// Build a JSON-RPC `textDocument/prepareCallHierarchy` request.
fn build_prepare_call_hierarchy_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<tower_lsp_server::ls_types::CallHierarchyPrepareParams> {
    let params = tower_lsp_server::ls_types::CallHierarchyPrepareParams {
        text_document_position_params: build_text_document_position_params(
            virtual_uri,
            host_position,
            offset,
        ),
        work_done_progress_params: tower_lsp_server::ls_types::WorkDoneProgressParams {
            work_done_token: client_progress_token,
        },
    };
    JsonRpcRequest::new(
        request_id.as_i64(),
        "textDocument/prepareCallHierarchy",
        params,
    )
}

/// Build a JSON-RPC incoming/outgoing calls request. The params is a wrapper
/// object `{ "item": <CallHierarchyItem> }` (the spec params shape).
fn build_call_hierarchy_resolve_request(
    method: &'static str,
    item: &CallHierarchyItem,
    request_id: RequestId,
) -> JsonRpcRequest<serde_json::Value> {
    let params = serde_json::json!({ "item": item });
    JsonRpcRequest::new(request_id.as_i64(), method, params)
}

/// Transform a `prepareCallHierarchy` response from virtual → host coordinates,
/// dropping cross-region/cross-file items and enveloping the survivors.
fn transform_prepare_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    ctx: &CallHierarchyEnvelopeContext,
) -> Option<Vec<CallHierarchyItem>> {
    if response_has_jsonrpc_error(&response, "textDocument/prepareCallHierarchy") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    let items: Vec<CallHierarchyItem> = serde_json::from_value(result).ok()?;
    Some(
        items
            .into_iter()
            .filter_map(|item| transform_item_to_host(item, request_virtual_uri, ctx))
            .collect(),
    )
}

/// Transform an `incomingCalls` response virtual → host. Each `from` item is
/// translated (cross-region calls are dropped) and its `from_ranges` translated.
fn transform_incoming_response_to_host(
    mut response: serde_json::Value,
    envelope: &CallHierarchyEnvelope,
) -> Option<Vec<CallHierarchyIncomingCall>> {
    if response_has_jsonrpc_error(&response, "callHierarchy/incomingCalls") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    let calls: Vec<CallHierarchyIncomingCall> = serde_json::from_value(result).ok()?;
    let request_virtual_uri = envelope.virtual_uri_string()?;
    let offset = RegionOffset::from(&envelope.offset);
    let ctx = envelope_ctx_from(envelope, &offset);
    Some(
        calls
            .into_iter()
            .filter_map(|mut call| {
                let from = transform_item_to_host(call.from, &request_virtual_uri, &ctx)?;
                call.from = from;
                translate_from_ranges_to_host(&mut call.from_ranges, &offset);
                Some(call)
            })
            .collect(),
    )
}

/// Transform an `outgoingCalls` response virtual → host.
fn transform_outgoing_response_to_host(
    mut response: serde_json::Value,
    envelope: &CallHierarchyEnvelope,
) -> Option<Vec<CallHierarchyOutgoingCall>> {
    if response_has_jsonrpc_error(&response, "callHierarchy/outgoingCalls") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    let calls: Vec<CallHierarchyOutgoingCall> = serde_json::from_value(result).ok()?;
    let request_virtual_uri = envelope.virtual_uri_string()?;
    let offset = RegionOffset::from(&envelope.offset);
    let ctx = envelope_ctx_from(envelope, &offset);
    Some(
        calls
            .into_iter()
            .filter_map(|mut call| {
                let to = transform_item_to_host(call.to, &request_virtual_uri, &ctx)?;
                call.to = to;
                translate_from_ranges_to_host(&mut call.from_ranges, &offset);
                Some(call)
            })
            .collect(),
    )
}

/// Build an envelope context from an envelope + a borrowed offset (for response
/// transforms in incoming/outgoing, where the offset is reconstructed).
fn envelope_ctx_from<'a>(
    envelope: &'a CallHierarchyEnvelope,
    offset: &'a RegionOffset,
) -> CallHierarchyEnvelopeContext<'a> {
    CallHierarchyEnvelopeContext {
        server_name: &envelope.origin,
        host_uri: &envelope.host_uri,
        region_id: &envelope.region_id,
        injection_language: &envelope.injection_language,
        offset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower_lsp_server::ls_types::{Position, Range, SymbolKind, Uri};

    fn test_host_uri() -> Uri {
        Uri::from_str("file:///test.md").unwrap()
    }

    fn virtual_uri_string() -> String {
        VirtualDocumentUri::new(&test_host_uri(), "lua", "01ARZ3NDEKTSV4RRFFQ69G5FAV")
            .to_uri_string()
    }

    fn test_ctx<'a>(offset: &'a RegionOffset) -> CallHierarchyEnvelopeContext<'a> {
        CallHierarchyEnvelopeContext {
            server_name: "lua-ls",
            host_uri: "file:///test.md",
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            injection_language: "lua",
            offset,
        }
    }

    fn range(l0: u32, c0: u32, l1: u32, c1: u32) -> Range {
        Range {
            start: Position {
                line: l0,
                character: c0,
            },
            end: Position {
                line: l1,
                character: c1,
            },
        }
    }

    fn item_at(uri: &str, range_: Range) -> CallHierarchyItem {
        CallHierarchyItem {
            name: "greet".to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: Uri::from_str(uri).unwrap(),
            range: range_,
            selection_range: range_,
            data: Some(json!({"original": true})),
        }
    }

    // ===== Item transform: own-region uri→host + ranges→host =====

    #[test]
    fn transform_item_translates_own_region_to_host() {
        let offset = RegionOffset::new(5, 0);
        let vuri = virtual_uri_string();
        let item = item_at(&vuri, range(0, 0, 0, 5));

        let out = transform_item_to_host(item, &vuri, &test_ctx(&offset))
            .expect("own-region item is kept");

        assert_eq!(out.uri.as_str(), "file:///test.md");
        assert_eq!(out.range.start.line, 5);
        assert_eq!(out.selection_range.start.line, 5);
        // data is now an envelope, original preserved as inner
        let env = extract_call_hierarchy_envelope(&out).expect("enveloped");
        assert_eq!(env.origin, "lua-ls");
        assert_eq!(env.injection_language, "lua");
        assert_eq!(env.inner, Some(json!({"original": true})));
    }

    #[test]
    fn transform_item_drops_cross_region() {
        let offset = RegionOffset::new(5, 0);
        let vuri = virtual_uri_string();
        // A DIFFERENT virtual region URI
        let other = VirtualDocumentUri::new(&test_host_uri(), "lua", "01OTHERREGIONOTHERREGION12")
            .to_uri_string();
        let item = item_at(&other, range(0, 0, 0, 5));
        assert!(transform_item_to_host(item, &vuri, &test_ctx(&offset)).is_none());
    }

    #[test]
    fn transform_item_drops_real_file() {
        let offset = RegionOffset::new(5, 0);
        let vuri = virtual_uri_string();
        let item = item_at("file:///somewhere/real.lua", range(0, 0, 0, 5));
        assert!(
            transform_item_to_host(item, &vuri, &test_ctx(&offset)).is_none(),
            "cross-file items are dropped (VIRT-only scope)"
        );
    }

    // ===== Envelope round-trip: wrap → extract → strip =====

    #[test]
    fn envelope_round_trips() {
        let offset = RegionOffset::new(3, 0);
        let mut item = item_at(&virtual_uri_string(), range(0, 0, 0, 5));
        item.data = Some(json!({"k": "v"}));
        envelope_item_data(&mut item, &test_ctx(&offset));

        let extracted = extract_call_hierarchy_envelope(&item).expect("enveloped");
        assert_eq!(extracted.origin, "lua-ls");
        assert_eq!(extracted.inner, Some(json!({"k": "v"})));

        let stripped = strip_call_hierarchy_envelope(&mut item).expect("strip");
        assert_eq!(stripped.origin, "lua-ls");
        assert_eq!(
            item.data,
            Some(json!({"k": "v"})),
            "strip restores the downstream's original data"
        );
    }

    #[test]
    fn strip_returns_none_for_non_envelope() {
        let mut item = item_at(&virtual_uri_string(), range(0, 0, 0, 5));
        item.data = Some(json!({"custom": true}));
        assert!(strip_call_hierarchy_envelope(&mut item).is_none());
        assert_eq!(item.data, Some(json!({"custom": true})));
    }

    // ===== Response transforms =====

    #[test]
    fn prepare_response_translates_and_envelopes() {
        let offset = RegionOffset::new(5, 0);
        let vuri = virtual_uri_string();
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "name": "greet",
                "kind": 12,
                "uri": vuri,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
            }]
        });
        let items = transform_prepare_response_to_host(response, &vuri, &test_ctx(&offset))
            .expect("parsed");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].uri.as_str(), "file:///test.md");
        assert_eq!(items[0].range.start.line, 5);
        assert!(extract_call_hierarchy_envelope(&items[0]).is_some());
    }

    #[test]
    fn prepare_response_drops_cross_region_items() {
        let offset = RegionOffset::new(5, 0);
        let vuri = virtual_uri_string();
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "name": "ext",
                "kind": 12,
                "uri": "file:///real.lua",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
            }]
        });
        let items = transform_prepare_response_to_host(response, &vuri, &test_ctx(&offset))
            .expect("parsed");
        assert!(items.is_empty(), "cross-file item dropped");
    }

    #[test]
    fn incoming_response_translates_from_and_ranges() {
        let offset = RegionOffset::new(5, 0);
        let envelope = CallHierarchyEnvelope {
            origin: "lua-ls".to_string(),
            host_uri: "file:///test.md".to_string(),
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            injection_language: "lua".to_string(),
            offset: EnvelopeOffset::from(&offset),
            inner: None,
        };
        let vuri = envelope.virtual_uri_string().unwrap();
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "from": {
                    "name": "caller",
                    "kind": 12,
                    "uri": vuri,
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 6}},
                    "selectionRange": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 6}}
                },
                "fromRanges": [
                    {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}}
                ]
            }]
        });
        let calls = transform_incoming_response_to_host(response, &envelope).expect("parsed");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from.uri.as_str(), "file:///test.md");
        assert_eq!(calls[0].from.range.start.line, 6); // 1 + 5
        assert_eq!(calls[0].from_ranges[0].start.line, 7); // 2 + 5
        assert!(extract_call_hierarchy_envelope(&calls[0].from).is_some());
    }

    #[test]
    fn outgoing_response_translates_to_and_ranges() {
        let offset = RegionOffset::new(5, 0);
        let envelope = CallHierarchyEnvelope {
            origin: "lua-ls".to_string(),
            host_uri: "file:///test.md".to_string(),
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            injection_language: "lua".to_string(),
            offset: EnvelopeOffset::from(&offset),
            inner: None,
        };
        let vuri = envelope.virtual_uri_string().unwrap();
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "to": {
                    "name": "callee",
                    "kind": 12,
                    "uri": vuri,
                    "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 6}},
                    "selectionRange": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 6}}
                },
                "fromRanges": [
                    {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}}
                ]
            }]
        });
        let calls = transform_outgoing_response_to_host(response, &envelope).expect("parsed");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].to.uri.as_str(), "file:///test.md");
        assert_eq!(calls[0].to.range.start.line, 6);
        assert_eq!(calls[0].from_ranges[0].start.line, 7);
    }

    // ===== Request-side translation (host → virtual) =====

    #[test]
    fn translate_item_to_virtual_repoints_uri_and_subtracts_offset() {
        let offset = RegionOffset::new(5, 0);
        let envelope = CallHierarchyEnvelope {
            origin: "lua-ls".to_string(),
            host_uri: "file:///test.md".to_string(),
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            injection_language: "lua".to_string(),
            offset: EnvelopeOffset::from(&offset),
            inner: Some(json!({"original": true})),
        };
        // Host item at line 6 (its data already stripped to the downstream value).
        let mut item = item_at("file:///test.md", range(6, 0, 6, 5));
        item.data = Some(json!({"original": true}));

        translate_item_to_virtual(&mut item, &envelope);

        // uri now points at the region's virtual document
        assert_eq!(item.uri.as_str(), envelope.virtual_uri_string().unwrap());
        // line 6 - offset 5 = virtual line 1
        assert_eq!(item.range.start.line, 1);
        assert_eq!(item.selection_range.start.line, 1);
        // data is left untouched (the downstream's own value)
        assert_eq!(item.data, Some(json!({"original": true})));
    }

    // ===== Request builders =====

    #[test]
    fn prepare_request_uses_virtual_uri_and_translates_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        // Host position line 7; region starts at line 5 → virtual line 2.
        let request = build_prepare_call_hierarchy_request(
            &virtual_uri,
            Position {
                line: 7,
                character: 3,
            },
            &RegionOffset::new(5, 0),
            RequestId::new(42),
            None,
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "textDocument/prepareCallHierarchy");
        assert_eq!(json["params"]["position"]["line"], 2);
        assert!(
            json["params"]["textDocument"]["uri"]
                .as_str()
                .unwrap()
                .contains("kakehashi-virtual-uri-")
        );
    }

    #[test]
    fn prepare_request_carries_work_done_token_only_when_present() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let with = build_prepare_call_hierarchy_request(
            &virtual_uri,
            Position {
                line: 0,
                character: 0,
            },
            &RegionOffset::new(0, 0),
            RequestId::new(1),
            Some(NumberOrString::String("wd-1".to_string())),
        );
        assert_eq!(
            serde_json::to_value(&with).unwrap()["params"]["workDoneToken"],
            "wd-1"
        );
    }

    #[test]
    fn resolve_request_carries_item_in_params() {
        let item = item_at(&virtual_uri_string(), range(0, 0, 0, 5));
        let request = build_call_hierarchy_resolve_request(
            "callHierarchy/incomingCalls",
            &item,
            RequestId::new(7),
        );
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["method"], "callHierarchy/incomingCalls");
        assert_eq!(json["params"]["item"]["name"], "greet");
    }

    #[test]
    fn responses_return_none_on_error_or_null() {
        let envelope = CallHierarchyEnvelope {
            origin: "lua-ls".to_string(),
            host_uri: "file:///test.md".to_string(),
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            injection_language: "lua".to_string(),
            offset: EnvelopeOffset::from(&RegionOffset::new(5, 0)),
            inner: None,
        };
        let err = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32600, "message": "bad"}});
        assert!(transform_incoming_response_to_host(err.clone(), &envelope).is_none());
        assert!(transform_outgoing_response_to_host(err, &envelope).is_none());
        let null = json!({"jsonrpc": "2.0", "id": 1, "result": null});
        assert!(transform_incoming_response_to_host(null, &envelope).is_none());
    }
}
