//! Code lens request handling for bridge connections, with host/virtual
//! coordinate transformation and `codeLens/resolve` routing (#355).
//!
//! The `textDocument/codeLens` request operates on the entire document (no
//! position parameter, like document link); `codeLens/resolve` takes a single
//! lens as its complete params — see `dispatch_code_lens_resolve`.
//!
//! Every bridged lens gets a routing envelope in `lens.data` (the
//! `completionItem/resolve` pattern) so a later `codeLens/resolve` can be sent
//! to the origin downstream server. Unresolved lenses (no `command`, only
//! `data`) are therefore forwarded instead of dropped — servers like
//! rust-analyzer return mostly-unresolved lenses by design.
//!
//! Resolution fails soft: any failure (stale region, missing server,
//! connection error, parse failure) returns the lens unresolved with its
//! envelope intact; clients re-request lenses on change, so the window is
//! short (#355 maintainer decision).
//!
//! Requests are queued via the channel-based writer task (`send_request()`) for
//! FIFO ordering with other messages (ls-bridge-message-ordering single-writer loop).

use std::io;
use std::sync::Arc;

use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::CodeLens;
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    DocumentParams, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_whole_document_request, response_has_jsonrpc_error, translate_host_range_to_virtual,
    translate_virtual_range_to_host,
};
use super::completion::EnvelopeOffset;
use crate::config::settings::{BridgeServerConfig, WorkspaceSettings};
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;

/// Wrapper key inside `CodeLens.data` that identifies the origin server.
const ENVELOPE_KEY: &str = "kakehashi";

/// Envelope stored in `CodeLens.data` for routing `codeLens/resolve` (#355).
///
/// Carries everything resolve-time routing and fail-soft staleness checking
/// need: the origin server, the host document and region the lens came from,
/// the offset snapshot for coordinate translation, and the downstream's
/// original `data` (preserved verbatim).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CodeLensEnvelope {
    /// Server name identifying which downstream produced the lens.
    pub(crate) origin: String,
    /// Host document URI the lens belongs to (resolve params carry no
    /// textDocument, so the envelope must).
    pub(crate) host_uri: String,
    /// ULID of the injection region the lens came from; resolve-time staleness
    /// checks look it up in the node tracker (fail-soft when invalidated).
    pub(crate) region_id: String,
    /// Region offset snapshot for coordinate translation at resolve time.
    pub(crate) offset: EnvelopeOffset,
    /// The downstream server's original `data` value (preserved verbatim).
    pub(crate) inner: Option<Value>,
}

/// Context needed to create envelopes during code lens response processing.
struct CodeLensEnvelopeContext<'a> {
    server_name: &'a str,
    host_uri: &'a str,
    region_id: &'a str,
    offset: &'a RegionOffset,
}

/// Wrap `lens.data` in a Kakehashi envelope for origin tracking.
fn envelope_lens_data(lens: &mut CodeLens, ctx: &CodeLensEnvelopeContext) {
    let inner = lens.data.take();
    let envelope = CodeLensEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        offset: EnvelopeOffset::from(ctx.offset),
        inner,
    };
    lens.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

/// Extract the envelope from a lens's `data` without modifying the lens.
///
/// Returns `None` if `data` is absent or not an envelope.
pub(crate) fn extract_code_lens_envelope(lens: &CodeLens) -> Option<CodeLensEnvelope> {
    let data = lens.data.as_ref()?;
    let wrapper = data.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

/// Extract the envelope and restore the original `data` value.
///
/// On success, `lens.data` is set back to the downstream's original value
/// (`inner`). Returns `None` if not an envelope (lens unchanged).
pub(crate) fn strip_code_lens_envelope(lens: &mut CodeLens) -> Option<CodeLensEnvelope> {
    let mut envelope = extract_code_lens_envelope(lens)?;
    lens.data = envelope.inner.take();
    Some(envelope)
}

/// Restore the envelope into a lens's `data` field (fail-soft return path).
fn re_envelope_lens(lens: &mut CodeLens, envelope: &CodeLensEnvelope) {
    let ctx = CodeLensEnvelopeContext {
        server_name: &envelope.origin,
        host_uri: &envelope.host_uri,
        region_id: &envelope.region_id,
        offset: &RegionOffset::from(&envelope.offset),
    };
    envelope_lens_data(lens, &ctx);
}

impl LanguageServerPool {
    /// Send a code lens request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing code-lens-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_code_lens_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<CodeLens>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/codeLens") {
            return Ok(None);
        }
        let host_uri_string = host_uri.to_string();
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            build_code_lens_request,
            |response, ctx| {
                let envelope_ctx = CodeLensEnvelopeContext {
                    server_name,
                    host_uri: &host_uri_string,
                    region_id,
                    offset: ctx.offset,
                };
                transform_code_lens_response_to_host(response, ctx.offset, &envelope_ctx)
            },
        )
        .await
    }

    /// Route a `codeLens/resolve` request to the origin downstream server.
    ///
    /// Strips the Kakehashi envelope to identify the origin server (a lens
    /// without one wasn't produced by the injection bridge and passes through
    /// unchanged), looks up the server config from `settings`, and forwards
    /// the lens with its range translated back to virtual coordinates. The
    /// caller (lsp_impl) has already verified the region is not stale. Every
    /// failure path returns the lens unresolved with its envelope restored
    /// (fail-soft, #355).
    pub(crate) async fn dispatch_code_lens_resolve(
        &self,
        mut lens: CodeLens,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> CodeLens {
        let Some(envelope) = strip_code_lens_envelope(&mut lens) else {
            return lens;
        };

        let config = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        );

        let Some(config) = config else {
            re_envelope_lens(&mut lens, &envelope);
            return lens;
        };

        self.send_code_lens_resolve_request(&config, lens, envelope, upstream_id)
            .await
    }

    /// Send a `codeLens/resolve` request to the downstream server that
    /// produced the lens, re-enveloping the resolved lens for return.
    async fn send_code_lens_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut lens: CodeLens,
        envelope: CodeLensEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> CodeLens {
        let server_name = &envelope.origin;
        let handle = match self
            .get_or_create_connection(server_name, server_config)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "codeLens/resolve: failed to connect to {}: {}",
                    server_name, e
                );
                re_envelope_lens(&mut lens, &envelope);
                return lens;
            }
        };

        if !handle.has_capability("codeLens/resolve") {
            re_envelope_lens(&mut lens, &envelope);
            return lens;
        }

        // Register in the upstream request registry FIRST for cancel lookup.
        if let Some(ref id) = upstream_id {
            self.register_upstream_request(id.clone(), server_name);
        }

        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(
                        target: "kakehashi::bridge",
                        "codeLens/resolve: failed to register request for {}: {}",
                        server_name, e
                    );
                    if let Some(ref id) = upstream_id {
                        self.unregister_upstream_request(id, server_name);
                    }
                    re_envelope_lens(&mut lens, &envelope);
                    return lens;
                }
            };

        // The downstream produced this lens in VIRTUAL coordinates; translate
        // the (host) range back before sending. `lens.data` already carries the
        // downstream's original value (the caller stripped the envelope).
        let offset = RegionOffset::from(&envelope.offset);
        let mut outgoing = lens.clone();
        translate_host_range_to_virtual(&mut outgoing.range, &offset);
        let request = build_code_lens_resolve_request(&outgoing, request_id);

        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        if let Err(e) = handle.send_request(request, request_id) {
            warn!(
                target: "kakehashi::bridge",
                "codeLens/resolve: failed to send request for {}: {}",
                server_name, e
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, server_name);
            }
            re_envelope_lens(&mut lens, &envelope);
            return lens;
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, server_name);
        }

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "codeLens/resolve failed for server {}: {}",
                    server_name, e
                );
                re_envelope_lens(&mut lens, &envelope);
                return lens;
            }
        };

        match parse_code_lens_resolve_response(response) {
            Some(mut resolved) => {
                // Keep the ORIGINAL host range: resolve fills in lazy
                // properties (command), it does not relocate the lens, and the
                // original range was already validated/translated when the
                // lens was minted. Trusting a downstream-returned range here
                // would need fresh past-EOF bounds checks (cf. formatting's
                // guard) against a virtual document we no longer have.
                resolved.range = lens.range;
                re_envelope_lens(&mut resolved, &envelope);
                resolved
            }
            None => {
                re_envelope_lens(&mut lens, &envelope);
                lens
            }
        }
    }
}

/// Build a JSON-RPC code lens request for a downstream language server.
fn build_code_lens_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentParams> {
    build_whole_document_request(virtual_uri, request_id, "textDocument/codeLens")
}

/// Build a JSON-RPC `codeLens/resolve` request.
///
/// Like `completionItem/resolve`, the params is the `CodeLens` itself.
fn build_code_lens_resolve_request(
    lens: &CodeLens,
    request_id: RequestId,
) -> JsonRpcRequest<&CodeLens> {
    JsonRpcRequest::new(request_id.as_i64(), "codeLens/resolve", lens)
}

/// Parse a JSON-RPC `codeLens/resolve` response into a `CodeLens`.
///
/// Returns `None` for null results, missing results, and deserialization
/// failures (the caller falls back to the unresolved lens).
fn parse_code_lens_resolve_response(mut response: serde_json::Value) -> Option<CodeLens> {
    if response_has_jsonrpc_error(&response, "codeLens/resolve") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    serde_json::from_value(result).ok()
}

/// Transform a code lens response from virtual to host document coordinates.
///
/// Each lens's `range` is translated by `offset` and its `data` is wrapped in
/// a routing envelope so `codeLens/resolve` can find the origin server later.
/// Unresolved lenses (no `command`) are kept now that resolve is supported
/// (#355) — dropping them wholesale destroyed most of rust-analyzer's lenses.
fn transform_code_lens_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    envelope_ctx: &CodeLensEnvelopeContext<'_>,
) -> Option<Vec<CodeLens>> {
    if response_has_jsonrpc_error(&response, "textDocument/codeLens") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    let mut lenses: Vec<CodeLens> = serde_json::from_value(result).ok()?;

    for lens in &mut lenses {
        translate_virtual_range_to_host(&mut lens.range, offset);
        envelope_lens_data(lens, envelope_ctx);
    }

    Some(lenses)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    fn ctx_with<'a>(offset: &'a RegionOffset) -> CodeLensEnvelopeContext<'a> {
        CodeLensEnvelopeContext {
            server_name: "lua-ls",
            host_uri: "file:///test.md",
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            offset,
        }
    }

    // ==========================================================================
    // Request builder tests
    // ==========================================================================

    #[test]
    fn code_lens_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_code_lens_request(&virtual_uri, RequestId::new(42));

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn code_lens_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_code_lens_request(&virtual_uri, RequestId::new(123));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/codeLens");
        assert!(
            json["params"].get("position").is_none(),
            "CodeLens request should not have position parameter"
        );
    }

    #[test]
    fn code_lens_resolve_request_carries_lens_as_params() {
        let lens = CodeLens {
            range: tower_lsp_server::ls_types::Range::default(),
            command: None,
            data: Some(json!({"kind": "references"})),
        };
        let request = build_code_lens_resolve_request(&lens, RequestId::new(7));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7);
        assert_eq!(json["method"], "codeLens/resolve");
        assert_eq!(json["params"]["data"]["kind"], "references");
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[test]
    fn code_lens_response_transforms_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 10 }
                    },
                    "command": {
                        "title": "3 references",
                        "command": "editor.action.showReferences"
                    }
                }
            ]
        });

        let offset = RegionOffset::new(5, 0);
        let transformed =
            transform_code_lens_response_to_host(response, &offset, &ctx_with(&offset));

        let lenses = transformed.expect("Should parse code lenses");
        assert_eq!(lenses.len(), 1);
        assert_eq!(lenses[0].range.start.line, 5);
        assert_eq!(lenses[0].range.end.line, 5);
        assert_eq!(
            lenses[0].command.as_ref().map(|c| c.title.as_str()),
            Some("3 references")
        );
    }

    #[test]
    fn code_lens_response_keeps_unresolved_lenses_and_envelopes_them() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    },
                    "data": { "kind": "references" }
                },
                {
                    "range": {
                        "start": { "line": 1, "character": 0 },
                        "end": { "line": 1, "character": 5 }
                    },
                    "command": { "title": "Run test", "command": "test.run" }
                }
            ]
        });

        let offset = RegionOffset::new(3, 0);
        let transformed =
            transform_code_lens_response_to_host(response, &offset, &ctx_with(&offset));

        let lenses = transformed.expect("Should parse code lenses");
        assert_eq!(
            lenses.len(),
            2,
            "unresolved lenses are kept now that codeLens/resolve is supported (#355)"
        );

        // Unresolved lens: enveloped, downstream data preserved in inner.
        assert!(lenses[0].command.is_none());
        assert_eq!(lenses[0].range.start.line, 3);
        let envelope =
            extract_code_lens_envelope(&lenses[0]).expect("unresolved lens carries envelope");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(envelope.host_uri, "file:///test.md");
        assert_eq!(envelope.region_id, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(envelope.inner, Some(json!({"kind": "references"})));
        assert_eq!(envelope.offset.line, 3);

        // Resolved lens: also enveloped (resolve may still be issued on it).
        assert_eq!(lenses[1].range.start.line, 4);
        assert!(extract_code_lens_envelope(&lenses[1]).is_some());
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn code_lens_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        let offset = RegionOffset::new(5, 0);
        let transformed =
            transform_code_lens_response_to_host(response, &offset, &ctx_with(&offset));
        assert!(transformed.is_none());
    }

    #[test]
    fn code_lens_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let offset = RegionOffset::new(5, 0);
        let transformed =
            transform_code_lens_response_to_host(response, &offset, &ctx_with(&offset));
        assert!(transformed.expect("Should parse empty array").is_empty());
    }

    #[test]
    fn code_lens_response_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                },
                "command": { "title": "x", "command": "y" }
            }]
        });

        let offset = RegionOffset::new(10, 0);
        let transformed =
            transform_code_lens_response_to_host(response, &offset, &ctx_with(&offset));

        let lenses = transformed.expect("Should parse code lenses");
        assert_eq!(
            lenses[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }

    // ==========================================================================
    // Envelope round-trip tests
    // ==========================================================================

    #[test]
    fn strip_then_re_envelope_round_trips() {
        let offset = RegionOffset::new(3, 0);
        let mut lens = CodeLens {
            range: tower_lsp_server::ls_types::Range::default(),
            command: None,
            data: Some(json!({"kind": "references"})),
        };
        envelope_lens_data(&mut lens, &ctx_with(&offset));

        let envelope = strip_code_lens_envelope(&mut lens).expect("envelope present");
        assert_eq!(
            lens.data,
            Some(json!({"kind": "references"})),
            "strip restores the downstream's original data"
        );
        assert_eq!(envelope.origin, "lua-ls");

        re_envelope_lens(&mut lens, &envelope);
        let extracted = extract_code_lens_envelope(&lens).expect("re-enveloped");
        assert_eq!(extracted.origin, "lua-ls");
        assert_eq!(extracted.inner, Some(json!({"kind": "references"})));
    }

    #[test]
    fn strip_returns_none_for_non_envelope_data() {
        let mut lens = CodeLens {
            range: tower_lsp_server::ls_types::Range::default(),
            command: None,
            data: Some(json!({"custom": true})),
        };
        assert!(strip_code_lens_envelope(&mut lens).is_none());
        assert_eq!(
            lens.data,
            Some(json!({"custom": true})),
            "non-envelope lens is untouched"
        );
    }

    // ==========================================================================
    // parse_code_lens_resolve_response tests
    // ==========================================================================

    #[test]
    fn parse_resolve_response_happy_path() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 5 }
                },
                "command": { "title": "3 references", "command": "show" }
            }
        });
        let lens = parse_code_lens_resolve_response(response).expect("should parse");
        assert_eq!(
            lens.command.as_ref().map(|c| c.title.as_str()),
            Some("3 references")
        );
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 7, "result": null}))]
    #[case::missing_result(json!({"jsonrpc": "2.0", "id": 7}))]
    #[case::error(json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32600, "message": "bad"}}))]
    fn parse_resolve_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        assert!(parse_code_lens_resolve_response(response).is_none());
    }

    // ==========================================================================
    // dispatch_code_lens_resolve fail-soft tests
    // ==========================================================================

    /// dispatch re-envelopes and returns the lens when the origin server is
    /// not configured (fail-soft).
    #[tokio::test]
    async fn dispatch_re_envelopes_when_server_not_configured() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default(); // no language_servers configured

        let offset = RegionOffset::new(3, 0);
        let mut lens = CodeLens {
            range: tower_lsp_server::ls_types::Range::default(),
            command: None,
            data: Some(json!({"kind": "references"})),
        };
        envelope_lens_data(&mut lens, &ctx_with(&offset));

        let result = pool.dispatch_code_lens_resolve(lens, &settings, None).await;

        let envelope = extract_code_lens_envelope(&result).expect("envelope restored");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(envelope.inner, Some(json!({"kind": "references"})));
        assert!(result.command.is_none(), "lens stays unresolved");
    }

    /// dispatch returns a non-envelope lens unchanged (host-layer or foreign).
    #[tokio::test]
    async fn dispatch_returns_non_envelope_lens_unchanged() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default();
        let lens = CodeLens {
            range: tower_lsp_server::ls_types::Range::default(),
            command: None,
            data: Some(json!({"custom": true})),
        };

        let result = pool
            .dispatch_code_lens_resolve(lens.clone(), &settings, None)
            .await;
        assert_eq!(result.data, Some(json!({"custom": true})));
    }
}
