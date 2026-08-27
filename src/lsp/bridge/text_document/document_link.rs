//! Document link request handling for bridge connections, with host/virtual
//! coordinate transformation.
//!
//! Unlike position-based requests (hover, definition, etc.), document link
//! requests operate on the entire document — they take no position parameter.
//!
//! Requests are queued via the channel-based writer task (`send_request()`) for
//! FIFO ordering with other messages (ls-bridge-message-ordering single-writer loop).

use std::io;
use std::sync::Arc;

use crate::config::settings::BridgeServerConfig;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::DocumentLink;
use url::Url;

use super::super::pool::{ConnectionKey, LanguageServerPool, UpstreamId};
use super::super::protocol::{
    DocumentParams, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_whole_document_request, response_has_jsonrpc_error,
};
use super::super::protocol::{translate_host_range_to_virtual, translate_virtual_range_to_host};
use super::completion::EnvelopeOffset;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;

const ENVELOPE_KEY: &str = "kakehashi";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct DocumentLinkEnvelope {
    pub(crate) origin: String,
    pub(crate) host_uri: String,
    pub(crate) region_id: String,
    pub(crate) injection_language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) incarnation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) connection_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) connection_key: Option<ConnectionKey>,
    pub(crate) offset: EnvelopeOffset,
    pub(crate) inner: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) host_layer: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl DocumentLinkEnvelope {
    pub(crate) fn is_host_layer(&self) -> bool {
        self.host_layer && self.region_id.is_empty()
    }
}

struct DocumentLinkEnvelopeContext<'a> {
    server_name: &'a str,
    host_uri: &'a str,
    region_id: &'a str,
    injection_language: &'a str,
    incarnation: Option<u64>,
    connection_generation: Option<u64>,
    connection_key: Option<&'a ConnectionKey>,
    offset: &'a RegionOffset,
    host_layer: bool,
}

fn envelope_link_data(link: &mut DocumentLink, ctx: &DocumentLinkEnvelopeContext<'_>) {
    let inner = link.data.take();
    let envelope = DocumentLinkEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        injection_language: ctx.injection_language.to_string(),
        incarnation: ctx.incarnation,
        connection_generation: ctx.connection_generation,
        connection_key: ctx.connection_key.cloned(),
        offset: EnvelopeOffset::from(ctx.offset),
        inner,
        host_layer: ctx.host_layer,
    };
    link.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

pub(crate) fn extract_document_link_envelope(link: &DocumentLink) -> Option<DocumentLinkEnvelope> {
    let wrapper = link.data.as_ref()?.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

fn strip_document_link_envelope(link: &mut DocumentLink) -> Option<DocumentLinkEnvelope> {
    let mut envelope = extract_document_link_envelope(link)?;
    link.data = envelope.inner.take();
    Some(envelope)
}

fn re_envelope_link(link: &mut DocumentLink, envelope: &DocumentLinkEnvelope) {
    let offset = RegionOffset::from(&envelope.offset);
    envelope_link_data(
        link,
        &DocumentLinkEnvelopeContext {
            server_name: &envelope.origin,
            host_uri: &envelope.host_uri,
            region_id: &envelope.region_id,
            injection_language: &envelope.injection_language,
            incarnation: envelope.incarnation,
            connection_generation: envelope.connection_generation,
            connection_key: envelope.connection_key.as_ref(),
            offset: &offset,
            host_layer: envelope.host_layer,
        },
    );
}

pub(crate) fn envelope_host_document_links(
    links: &mut [DocumentLink],
    server_name: &str,
    host_uri: &str,
    incarnation: Option<u64>,
    connection_generation: u64,
    connection_key: &ConnectionKey,
    server_resolves: bool,
) {
    let offset = RegionOffset::new(0, 0);
    let ctx = DocumentLinkEnvelopeContext {
        server_name,
        host_uri,
        region_id: "",
        injection_language: "",
        incarnation,
        connection_generation: Some(connection_generation),
        connection_key: Some(connection_key),
        offset: &offset,
        host_layer: true,
    };
    for link in links {
        if server_resolves
            || link
                .data
                .as_ref()
                .is_some_and(|data| data.get(ENVELOPE_KEY).is_some())
        {
            envelope_link_data(link, &ctx);
        }
    }
}

impl LanguageServerPool {
    /// Send a document link request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing document-link-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_document_link_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
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
        if !handle.has_capability("textDocument/documentLink") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            build_document_link_request,
            |response, ctx| {
                transform_document_link_response_to_host(
                    response,
                    ctx.offset,
                    &DocumentLinkEnvelopeContext {
                        server_name,
                        host_uri: host_uri.as_str(),
                        region_id,
                        injection_language,
                        incarnation: host_incarnation,
                        connection_generation: None,
                        connection_key: None,
                        offset: ctx.offset,
                        host_layer: false,
                    },
                )
            },
        )
        .await
    }

    pub(crate) async fn dispatch_document_link_resolve(
        &self,
        mut link: DocumentLink,
        settings: &crate::config::settings::WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> DocumentLink {
        let Some(envelope) = strip_document_link_envelope(&mut link) else {
            return link;
        };

        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            re_envelope_link(&mut link, &envelope);
            return link;
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        ) else {
            re_envelope_link(&mut link, &envelope);
            return link;
        };

        self.send_document_link_resolve_request(&config, link, envelope, upstream_id)
            .await
    }

    async fn send_document_link_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut link: DocumentLink,
        envelope: DocumentLinkEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> DocumentLink {
        let server_name = &envelope.origin;
        let Ok(host_uri) = Url::parse(&envelope.host_uri) else {
            re_envelope_link(&mut link, &envelope);
            return link;
        };
        if envelope
            .incarnation
            .is_some_and(|expected| self.current_host_incarnation(&host_uri) != Some(expected))
        {
            re_envelope_link(&mut link, &envelope);
            return link;
        }

        let handle_result = if envelope.is_host_layer() {
            let Some(expected_generation) = envelope.connection_generation else {
                re_envelope_link(&mut link, &envelope);
                return link;
            };
            let Some(connection_key) = envelope
                .connection_key
                .as_ref()
                .filter(|key| key.server() == server_name)
            else {
                re_envelope_link(&mut link, &envelope);
                return link;
            };
            if self.document_connection_generation(connection_key) != expected_generation {
                re_envelope_link(&mut link, &envelope);
                return link;
            }
            self.ready_connection_by_key_for_config(connection_key, Some(server_config))
                .await
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "producer connection is no longer live",
                    )
                })
        } else {
            self.get_or_create_virtual_connection(
                server_name,
                server_config,
                &host_uri,
                &envelope.injection_language,
                &envelope.region_id,
            )
            .await
        };
        let handle = match handle_result {
            Ok(handle) => handle,
            Err(error) => {
                warn!(
                    target: "kakehashi::bridge",
                    "documentLink/resolve: failed to connect to {server_name}: {error}"
                );
                re_envelope_link(&mut link, &envelope);
                return link;
            }
        };

        if !handle.has_capability("documentLink/resolve") {
            re_envelope_link(&mut link, &envelope);
            return link;
        }

        let _host_lifecycle = if envelope.is_host_layer() {
            let Some(expected_incarnation) = envelope.incarnation else {
                re_envelope_link(&mut link, &envelope);
                return link;
            };
            match self
                .request_host_lifecycle_for_incarnation(&host_uri, expected_incarnation)
                .await
            {
                Ok(lifecycle) => Some(lifecycle),
                Err(_) => {
                    re_envelope_link(&mut link, &envelope);
                    return link;
                }
            }
        } else {
            None
        };

        let connection_key = handle.key();
        if let Some(ref id) = upstream_id {
            self.register_upstream_request(id.clone(), connection_key);
        }
        let (request_id, response_rx) = match handle
            .register_request_with_upstream(upstream_id.clone())
        {
            Ok(pair) => pair,
            Err(error) => {
                warn!(
                    target: "kakehashi::bridge",
                    "documentLink/resolve: failed to register request for {server_name}: {error}"
                );
                if let Some(ref id) = upstream_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                re_envelope_link(&mut link, &envelope);
                return link;
            }
        };

        let mut outgoing = link.clone();
        if !envelope.is_host_layer() {
            translate_host_range_to_virtual(
                &mut outgoing.range,
                &RegionOffset::from(&envelope.offset),
            );
        }
        let request = build_document_link_resolve_request(&outgoing, request_id);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        let send_result = {
            let connections = self.connections().await;
            let producer_is_live = connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle));
            let generation_matches = !envelope.is_host_layer()
                || envelope.connection_generation.is_some_and(|expected| {
                    self.document_connection_generation(connection_key) == expected
                });
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
                "documentLink/resolve: failed to send request for {server_name}: {error}"
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, connection_key);
            }
            re_envelope_link(&mut link, &envelope);
            return link;
        }

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
                    "documentLink/resolve failed for server {server_name}: {error}"
                );
                re_envelope_link(&mut link, &envelope);
                return link;
            }
        };

        match parse_document_link_resolve_response(response) {
            Some(mut resolved) => {
                // Resolve materializes target/tooltip; the range belongs to the
                // original document-link result and was already translated and
                // freshness-checked.
                resolved.range = link.range;
                if resolved.data.is_none() {
                    resolved.data = link.data.take();
                }
                re_envelope_link(&mut resolved, &envelope);
                resolved
            }
            None => {
                re_envelope_link(&mut link, &envelope);
                link
            }
        }
    }
}

/// Build a JSON-RPC document link request for a downstream language server.
fn build_document_link_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentParams> {
    build_whole_document_request(virtual_uri, request_id, "textDocument/documentLink")
}

fn build_document_link_resolve_request(
    link: &DocumentLink,
    request_id: RequestId,
) -> JsonRpcRequest<&DocumentLink> {
    JsonRpcRequest::new(request_id.as_i64(), "documentLink/resolve", link)
}

fn parse_document_link_resolve_response(mut response: serde_json::Value) -> Option<DocumentLink> {
    if response_has_jsonrpc_error(&response, "documentLink/resolve") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    serde_json::from_value(result).ok()
}

/// Transform a document link response from virtual to host document coordinates.
///
/// Only each link's `range` is translated by `offset`; target, tooltip, and
/// data are preserved unchanged.
fn transform_document_link_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    envelope_ctx: &DocumentLinkEnvelopeContext<'_>,
) -> Option<Vec<DocumentLink>> {
    if response_has_jsonrpc_error(&response, "textDocument/documentLink") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse into typed Vec<DocumentLink>
    let mut links: Vec<DocumentLink> = serde_json::from_value(result).ok()?;

    // Transform ranges to host coordinates
    for link in &mut links {
        translate_virtual_range_to_host(&mut link.range, offset);
        envelope_link_data(link, envelope_ctx);
    }

    Some(links)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    fn transform_for_test(
        response: serde_json::Value,
        offset: &RegionOffset,
    ) -> Option<Vec<DocumentLink>> {
        transform_document_link_response_to_host(
            response,
            offset,
            &DocumentLinkEnvelopeContext {
                server_name: "lua-ls",
                host_uri: "file:///test.md",
                region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                injection_language: "lua",
                incarnation: Some(1),
                connection_generation: None,
                connection_key: None,
                offset,
                host_layer: false,
            },
        )
    }

    fn unresolved_link(data: Option<serde_json::Value>) -> DocumentLink {
        DocumentLink {
            range: tower_lsp_server::ls_types::Range::default(),
            target: None,
            tooltip: None,
            data,
        }
    }

    // ==========================================================================
    // Document link request tests
    // ==========================================================================

    #[test]
    fn document_link_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_document_link_request(&virtual_uri, RequestId::new(42));

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn document_link_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_document_link_request(&virtual_uri, RequestId::new(123));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/documentLink");
        assert!(
            json["params"].get("position").is_none(),
            "DocumentLink request should not have position parameter"
        );
    }

    #[test]
    fn document_link_resolve_request_carries_the_link_as_params() {
        let link = unresolved_link(Some(json!({"token": 7})));
        let request = build_document_link_resolve_request(&link, RequestId::new(9));
        let value = serde_json::to_value(request).expect("request should serialize");

        assert_eq!(value["method"], "documentLink/resolve");
        assert_eq!(value["params"]["data"]["token"], 7);
    }

    #[test]
    fn document_link_response_wraps_downstream_data_for_resolve_routing() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 0, "character": 1 },
                    "end": { "line": 0, "character": 4 }
                },
                "data": { "token": "link-1" }
            }]
        });

        let links = transform_for_test(response, &RegionOffset::new(3, 2)).unwrap();
        let envelope = extract_document_link_envelope(&links[0]).expect("routing envelope");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(envelope.host_uri, "file:///test.md");
        assert_eq!(envelope.inner, Some(json!({"token": "link-1"})));
        assert_eq!(links[0].range.start.line, 3);
        assert_eq!(links[0].range.start.character, 3);
    }

    #[test]
    fn host_links_are_enveloped_only_when_the_server_resolves() {
        let key = ConnectionKey::shared("lua-ls");
        let mut resolvable = vec![unresolved_link(Some(json!({"token": 1})))];
        envelope_host_document_links(
            &mut resolvable,
            "lua-ls",
            "file:///test.lua",
            Some(2),
            5,
            &key,
            true,
        );
        let envelope = extract_document_link_envelope(&resolvable[0]).unwrap();
        assert!(envelope.is_host_layer());
        assert_eq!(envelope.connection_generation, Some(5));
        assert_eq!(envelope.connection_key, Some(key));

        let mut bare = vec![unresolved_link(Some(json!({"token": 2})))];
        envelope_host_document_links(
            &mut bare,
            "lua-ls",
            "file:///test.lua",
            Some(2),
            5,
            &ConnectionKey::shared("lua-ls"),
            false,
        );
        assert_eq!(bare[0].data, Some(json!({"token": 2})));
    }

    // ==========================================================================
    // Document link response transformation tests
    // ==========================================================================

    #[test]
    fn document_link_response_transforms_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 10 },
                        "end": { "line": 0, "character": 25 }
                    },
                    "target": "file:///some/module.lua"
                },
                {
                    "range": {
                        "start": { "line": 2, "character": 5 },
                        "end": { "line": 2, "character": 15 }
                    }
                }
            ]
        });
        let region_start_line = 5;

        let transformed = transform_for_test(response, &RegionOffset::new(region_start_line, 0));

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].range.start.line, 5);
        assert_eq!(links[0].range.end.line, 5);
        assert_eq!(links[0].range.start.character, 10);
        assert_eq!(
            links[0].target.as_ref().map(|u| u.as_str()),
            Some("file:///some/module.lua")
        );
        assert_eq!(links[1].range.start.line, 7);
        assert_eq!(links[1].range.end.line, 7);
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn document_link_response_returns_none_for_invalid_response(
        #[case] response: serde_json::Value,
    ) {
        let transformed = transform_for_test(response, &RegionOffset::new(5, 0));
        assert!(transformed.is_none());
    }

    #[test]
    fn document_link_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed = transform_for_test(response, &RegionOffset::new(5, 0));
        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert!(links.is_empty());
    }

    #[test]
    fn document_link_response_preserves_target_and_tooltip() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 10 }
                },
                "target": "file:///target.lua",
                "tooltip": "Go to definition"
            }]
        });
        let region_start_line = 3;

        let transformed = transform_for_test(response, &RegionOffset::new(region_start_line, 0));

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links[0].range.start.line, 3);
        assert_eq!(
            links[0].target.as_ref().map(|u| u.as_str()),
            Some("file:///target.lua")
        );
        assert_eq!(links[0].tooltip.as_deref(), Some("Go to definition"));
    }

    #[test]
    fn document_link_response_without_target_transforms_range() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 1, "character": 5 },
                    "end": { "line": 1, "character": 20 }
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_for_test(response, &RegionOffset::new(region_start_line, 0));

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links[0].range.start.line, 11);
        assert_eq!(links[0].range.end.line, 11);
        assert!(links[0].target.is_none());
    }

    #[test]
    fn document_link_response_transformation_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_for_test(response, &RegionOffset::new(region_start_line, 0));

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(
            links[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }

    #[test]
    fn parse_document_link_resolve_response_materializes_target_and_tooltip() {
        let resolved = parse_document_link_resolve_response(json!({
            "jsonrpc": "2.0",
            "id": 9,
            "result": {
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 3 }
                },
                "target": "file:///resolved.lua",
                "tooltip": "resolved"
            }
        }))
        .expect("valid resolve response");

        assert_eq!(
            resolved.target.as_ref().map(|uri| uri.as_str()),
            Some("file:///resolved.lua")
        );
        assert_eq!(resolved.tooltip.as_deref(), Some("resolved"));
    }

    #[tokio::test]
    async fn dispatch_re_envelopes_when_origin_server_is_not_configured() {
        let pool = LanguageServerPool::new();
        let settings = crate::config::settings::WorkspaceSettings::default();
        let offset = RegionOffset::new(3, 2);
        let mut link = unresolved_link(Some(json!({"token": "link-1"})));
        envelope_link_data(
            &mut link,
            &DocumentLinkEnvelopeContext {
                server_name: "lua-ls",
                host_uri: "file:///test.md",
                region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                injection_language: "lua",
                incarnation: Some(1),
                connection_generation: None,
                connection_key: None,
                offset: &offset,
                host_layer: false,
            },
        );

        let result = pool
            .dispatch_document_link_resolve(link, &settings, None)
            .await;
        let envelope = extract_document_link_envelope(&result).expect("envelope restored");
        assert_eq!(envelope.inner, Some(json!({"token": "link-1"})));
        assert!(result.target.is_none());
    }
}
