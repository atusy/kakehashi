//! Call-hierarchy preparation for host and virtual bridge layers.

use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyPrepareParams, NumberOrString, PartialResultParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use url::Url;

use crate::config::settings::BridgeServerConfig;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};

use super::super::pool::{ConnectionKey, LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, response_has_jsonrpc_error,
    translate_host_position_to_virtual, translate_host_range_to_virtual,
    translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use super::completion::EnvelopeOffset;
use crate::lsp::bridge::actor::RouterCleanupGuard;

const ENVELOPE_KEY: &str = "kakehashi";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct CallHierarchyEnvelope {
    pub(crate) origin: String,
    pub(crate) host_uri: String,
    pub(crate) region_id: String,
    pub(crate) injection_language: String,
    pub(crate) incarnation: Option<u64>,
    pub(crate) content_version: u64,
    pub(crate) connection_generation: u64,
    pub(crate) connection_key: ConnectionKey,
    pub(crate) offset: EnvelopeOffset,
    pub(crate) inner: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) host_layer: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) projected_from_virtual: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl CallHierarchyEnvelope {
    pub(crate) fn is_host_layer(&self) -> bool {
        self.host_layer && self.region_id.is_empty()
    }
}

pub(crate) fn extract_call_hierarchy_envelope(
    item: &CallHierarchyItem,
) -> Option<CallHierarchyEnvelope> {
    serde_json::from_value(item.data.as_ref()?.get(ENVELOPE_KEY)?.clone()).ok()
}

fn strip_call_hierarchy_envelope(item: &mut CallHierarchyItem) -> Option<CallHierarchyEnvelope> {
    let mut envelope = extract_call_hierarchy_envelope(item)?;
    item.data = envelope.inner.take();
    Some(envelope)
}

fn prepare_incoming_params(
    mut params: CallHierarchyIncomingCallsParams,
) -> Option<(CallHierarchyIncomingCallsParams, CallHierarchyEnvelope)> {
    let envelope = strip_call_hierarchy_envelope(&mut params.item)?;
    // The bridge does not relay typed partial-result chunks or map these
    // progress tokens for this exact-producer request. Asking downstream to
    // stream would lose calls, so require one final aggregate result.
    params.work_done_progress_params = WorkDoneProgressParams::default();
    params.partial_result_params = PartialResultParams::default();
    Some((params, envelope))
}

fn re_envelope_item(
    item: &mut CallHierarchyItem,
    envelope: &CallHierarchyEnvelope,
    projected_from_virtual: bool,
) {
    let offset = RegionOffset::from(&envelope.offset);
    envelope_item_data(
        item,
        &CallHierarchyEnvelopeContext {
            server_name: &envelope.origin,
            host_uri: &envelope.host_uri,
            region_id: &envelope.region_id,
            injection_language: &envelope.injection_language,
            revision: CallHierarchyDocumentRevision {
                incarnation: envelope.incarnation,
                content_version: envelope.content_version,
            },
            connection_generation: envelope.connection_generation,
            connection_key: &envelope.connection_key,
            offset: &offset,
            host_layer: envelope.host_layer,
            projected_from_virtual,
        },
    );
}

pub(crate) struct CallHierarchyDocumentRevision {
    pub(crate) incarnation: Option<u64>,
    pub(crate) content_version: u64,
}

struct CallHierarchyEnvelopeContext<'a> {
    server_name: &'a str,
    host_uri: &'a str,
    region_id: &'a str,
    injection_language: &'a str,
    revision: CallHierarchyDocumentRevision,
    connection_generation: u64,
    connection_key: &'a ConnectionKey,
    offset: &'a RegionOffset,
    host_layer: bool,
    projected_from_virtual: bool,
}

fn envelope_item_data(item: &mut CallHierarchyItem, ctx: &CallHierarchyEnvelopeContext<'_>) {
    let inner = item.data.take();
    item.data = Some(serde_json::json!({ ENVELOPE_KEY: CallHierarchyEnvelope {
        origin: ctx.server_name.to_string(),
        host_uri: ctx.host_uri.to_string(),
        region_id: ctx.region_id.to_string(),
        injection_language: ctx.injection_language.to_string(),
        incarnation: ctx.revision.incarnation,
        content_version: ctx.revision.content_version,
        connection_generation: ctx.connection_generation,
        connection_key: ctx.connection_key.clone(),
        offset: EnvelopeOffset::from(ctx.offset),
        inner,
        host_layer: ctx.host_layer,
        projected_from_virtual: ctx.projected_from_virtual,
    }}));
}

pub(crate) fn envelope_host_call_hierarchy_items(
    items: &mut [CallHierarchyItem],
    server_name: &str,
    host_uri: &str,
    revision: CallHierarchyDocumentRevision,
    connection_generation: u64,
    connection_key: &ConnectionKey,
) {
    let offset = RegionOffset::new(0, 0);
    let ctx = CallHierarchyEnvelopeContext {
        server_name,
        host_uri,
        region_id: "",
        injection_language: "",
        revision,
        connection_generation,
        connection_key,
        offset: &offset,
        host_layer: true,
        projected_from_virtual: false,
    };
    for item in items {
        envelope_item_data(item, &ctx);
    }
}

impl LanguageServerPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_call_hierarchy_prepare_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        region_end: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        incarnation: u64,
        content_version: u64,
        upstream_request_id: Option<UpstreamId>,
        work_done_token: Option<NumberOrString>,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/prepareCallHierarchy") {
            return Ok(None);
        }
        let connection_key = handle.key().clone();
        let connection_generation = self.document_connection_generation(&connection_key);
        self.execute_position_bridge_request_with_handle_for_incarnation(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            incarnation,
            host_position,
            region_end,
            "textDocument/prepareCallHierarchy",
            |virtual_uri, request_id| {
                build_call_hierarchy_prepare_request(
                    virtual_uri,
                    host_position,
                    &offset,
                    request_id,
                    work_done_token,
                )
            },
            |response, ctx| {
                transform_call_hierarchy_prepare_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.offset,
                    &CallHierarchyEnvelopeContext {
                        server_name,
                        host_uri: host_uri.as_str(),
                        region_id,
                        injection_language,
                        revision: CallHierarchyDocumentRevision {
                            incarnation: Some(incarnation),
                            content_version,
                        },
                        connection_generation,
                        connection_key: &connection_key,
                        offset: ctx.offset,
                        host_layer: false,
                        projected_from_virtual: false,
                    },
                )
            },
        )
        .await?
    }

    pub(crate) async fn dispatch_call_hierarchy_incoming(
        &self,
        params: CallHierarchyIncomingCallsParams,
        settings: &crate::config::settings::WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<CallHierarchyIncomingCall>> {
        let (params, envelope) = prepare_incoming_params(params)?;
        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            return None;
        }
        let config = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        )?;
        self.send_call_hierarchy_incoming_request(&config, params, envelope, upstream_id)
            .await
    }

    async fn send_call_hierarchy_incoming_request(
        &self,
        server_config: &BridgeServerConfig,
        mut params: CallHierarchyIncomingCallsParams,
        envelope: CallHierarchyEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<CallHierarchyIncomingCall>> {
        let server_name = &envelope.origin;
        let host_uri = Url::parse(&envelope.host_uri).ok()?;
        let expected_incarnation = envelope.incarnation?;
        if self.current_host_incarnation(&host_uri) != Some(expected_incarnation) {
            return None;
        }
        if envelope.connection_key.server() != server_name
            || self.document_connection_generation(&envelope.connection_key)
                != envelope.connection_generation
        {
            return None;
        }
        let handle = self
            .ready_connection_by_key_for_config(&envelope.connection_key, Some(server_config))
            .await?;
        let _host_lifecycle = self
            .request_host_lifecycle_for_incarnation(&host_uri, expected_incarnation)
            .await
            .ok()?;
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(&host_uri).ok()?;
        let virtual_uri = (!envelope.is_host_layer()).then(|| {
            VirtualDocumentUri::new(
                &host_uri_lsp,
                &envelope.injection_language,
                &envelope.region_id,
            )
        });

        let connection_key = handle.key();
        if let Some(ref id) = upstream_id {
            self.register_upstream_request_for_handle(id.clone(), &handle);
        }
        let (request_id, response_rx) = match handle
            .register_request_with_upstream(upstream_id.clone())
        {
            Ok(pair) => pair,
            Err(error) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "callHierarchy/incomingCalls: failed to register request for {server_name}: {error}"
                );
                if let Some(ref id) = upstream_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return None;
            }
        };

        params.item =
            call_hierarchy_item_to_downstream(params.item, &envelope, virtual_uri.as_ref());
        let request =
            JsonRpcRequest::new(request_id.as_i64(), "callHierarchy/incomingCalls", params);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);
        let send_result = {
            let connections = self.connections().await;
            let producer_is_live = connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle));
            let generation_matches = self.document_connection_generation(connection_key)
                == envelope.connection_generation;
            if !producer_is_live || !generation_matches {
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "producer connection was replaced before incomingCalls send",
                ))
            } else {
                handle.send_request(request, request_id).map_err(Into::into)
            }
        };
        if let Err(error) = send_result {
            log::warn!(
                target: "kakehashi::bridge",
                "callHierarchy/incomingCalls: failed to send request for {server_name}: {error}"
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, connection_key);
            }
            return None;
        }

        // Admission is complete once the request is queued. A slow language
        // server must not hold didClose/reopen behind its response.
        drop(_host_lifecycle);
        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, connection_key);
        }
        let response = response.ok()?;
        let producer_is_still_live = {
            let connections = self.connections().await;
            connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
                && self.document_connection_generation(connection_key)
                    == envelope.connection_generation
        };
        if !producer_is_still_live {
            return None;
        }
        transform_call_hierarchy_incoming_response_to_host(
            response,
            virtual_uri.as_ref().map(VirtualDocumentUri::to_uri_string),
            &host_uri_lsp,
            &envelope,
        )
        .ok()?
    }
}

fn call_hierarchy_item_to_downstream(
    mut item: CallHierarchyItem,
    envelope: &CallHierarchyEnvelope,
    virtual_uri: Option<&VirtualDocumentUri>,
) -> CallHierarchyItem {
    if envelope.projected_from_virtual
        && let Some(virtual_uri) = virtual_uri
        && item.uri.as_str() == envelope.host_uri
    {
        item.uri = virtual_uri_to_lsp_uri(virtual_uri);
        let offset = RegionOffset::from(&envelope.offset);
        translate_host_range_to_virtual(&mut item.range, &offset);
        translate_host_range_to_virtual(&mut item.selection_range, &offset);
    }
    item
}

fn transform_call_hierarchy_incoming_response_to_host(
    mut response: Value,
    request_virtual_uri: Option<String>,
    host_uri: &Uri,
    envelope: &CallHierarchyEnvelope,
) -> io::Result<Option<Vec<CallHierarchyIncomingCall>>> {
    const METHOD: &str = "callHierarchy/incomingCalls";
    if response_has_jsonrpc_error(&response, METHOD) {
        return Ok(None);
    }
    let Some(result) = response.get_mut("result").map(Value::take) else {
        return Err(io::Error::other(
            "callHierarchy/incomingCalls response carries neither result nor error (protocol violation)",
        ));
    };
    if result.is_null() {
        return Ok(None);
    }
    let calls: Vec<CallHierarchyIncomingCall> =
        serde_json::from_value(result).map_err(|error| {
            io::Error::other(format!(
                "malformed callHierarchy/incomingCalls result from downstream server: {error}"
            ))
        })?;
    let offset = RegionOffset::from(&envelope.offset);
    let calls = calls
        .into_iter()
        .filter_map(|mut call| {
            let projected_from_virtual =
                if VirtualDocumentUri::is_virtual_uri(call.from.uri.as_str()) {
                    if request_virtual_uri.as_deref() != Some(call.from.uri.as_str()) {
                        return None;
                    }
                    call.from.uri = host_uri.clone();
                    translate_virtual_range_to_host(&mut call.from.range, &offset);
                    translate_virtual_range_to_host(&mut call.from.selection_range, &offset);
                    for range in &mut call.from_ranges {
                        translate_virtual_range_to_host(range, &offset);
                    }
                    true
                } else {
                    false
                };
            re_envelope_item(&mut call.from, envelope, projected_from_virtual);
            Some(call)
        })
        .collect();
    Ok(Some(calls))
}

fn build_call_hierarchy_prepare_request(
    virtual_uri: &VirtualDocumentUri,
    mut host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
    work_done_token: Option<NumberOrString>,
) -> JsonRpcRequest<CallHierarchyPrepareParams> {
    translate_host_position_to_virtual(&mut host_position, offset);
    JsonRpcRequest::new(
        request_id.as_i64(),
        "textDocument/prepareCallHierarchy",
        CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: virtual_uri_to_lsp_uri(virtual_uri),
                },
                position: host_position,
            },
            work_done_progress_params: WorkDoneProgressParams { work_done_token },
        },
    )
}

fn transform_call_hierarchy_prepare_response_to_host(
    mut response: Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    envelope_ctx: &CallHierarchyEnvelopeContext<'_>,
) -> io::Result<Option<Vec<CallHierarchyItem>>> {
    if response_has_jsonrpc_error(&response, "textDocument/prepareCallHierarchy") {
        return Ok(None);
    }
    let Some(result) = response.get_mut("result").map(Value::take) else {
        return Err(io::Error::other(
            "textDocument/prepareCallHierarchy response carries neither result nor error (protocol violation)",
        ));
    };
    if result.is_null() {
        return Ok(None);
    }
    let items: Vec<CallHierarchyItem> = serde_json::from_value(result).map_err(|error| {
        io::Error::other(format!(
            "malformed textDocument/prepareCallHierarchy result from downstream server: {error}"
        ))
    })?;
    let items = items
        .into_iter()
        .filter_map(|mut item| {
            let uri = item.uri.as_str();
            let projected_from_virtual = if VirtualDocumentUri::is_virtual_uri(uri) {
                if uri != request_virtual_uri {
                    return None;
                }
                item.uri = host_uri.clone();
                translate_virtual_range_to_host(&mut item.range, offset);
                translate_virtual_range_to_host(&mut item.selection_range, offset);
                true
            } else {
                false
            };
            let envelope_ctx = CallHierarchyEnvelopeContext {
                server_name: envelope_ctx.server_name,
                host_uri: envelope_ctx.host_uri,
                region_id: envelope_ctx.region_id,
                injection_language: envelope_ctx.injection_language,
                revision: CallHierarchyDocumentRevision {
                    incarnation: envelope_ctx.revision.incarnation,
                    content_version: envelope_ctx.revision.content_version,
                },
                connection_generation: envelope_ctx.connection_generation,
                connection_key: envelope_ctx.connection_key,
                offset: envelope_ctx.offset,
                host_layer: envelope_ctx.host_layer,
                projected_from_virtual,
            };
            envelope_item_data(&mut item, &envelope_ctx);
            Some(item)
        })
        .collect();
    Ok(Some(items))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;

    fn incoming_test_envelope(host_uri: &Uri, key: &ConnectionKey) -> CallHierarchyEnvelope {
        CallHierarchyEnvelope {
            origin: "lua-ls".into(),
            host_uri: host_uri.to_string(),
            region_id: "region".into(),
            injection_language: "lua".into(),
            incarnation: Some(2),
            content_version: 3,
            connection_generation: 4,
            connection_key: key.clone(),
            offset: EnvelopeOffset::from(&RegionOffset::new(3, 2)),
            inner: Some(json!({ "token": 9 })),
            host_layer: false,
            projected_from_virtual: true,
        }
    }

    fn call_item(uri: &str, data: Value) -> CallHierarchyItem {
        serde_json::from_value(json!({
            "name": "f",
            "kind": 12,
            "uri": uri,
            "range": {
                "start": { "line": 3, "character": 2 },
                "end": { "line": 3, "character": 5 }
            },
            "selectionRange": {
                "start": { "line": 3, "character": 2 },
                "end": { "line": 3, "character": 3 }
            },
            "data": data
        }))
        .unwrap()
    }

    #[test]
    fn prepare_request_uses_virtual_position_and_progress_token() {
        let request = build_call_hierarchy_prepare_request(
            &VirtualDocumentUri::new(&test_host_uri(), "lua", "region"),
            Position::new(3, 4),
            &RegionOffset::new(3, 2),
            RequestId::new(7),
            Some(NumberOrString::String("progress".into())),
        );
        let value = serde_json::to_value(request).unwrap();
        assert!(
            value["params"]["textDocument"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.contains("kakehashi-virtual-uri-region.lua"))
        );
        assert_eq!(
            value["params"]["position"],
            json!({ "line": 0, "character": 2 })
        );
        assert_eq!(value["params"]["workDoneToken"], "progress");
    }

    #[test]
    fn prepare_response_translates_and_envelopes_same_virtual_item() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let offset = RegionOffset::new(3, 2);
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region");
        let response = json!({ "jsonrpc": "2.0", "id": 7, "result": [{
            "name": "f",
            "kind": 12,
            "uri": virtual_uri.to_uri_string(),
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 1 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 1 }
            },
            "data": { "token": 9 }
        }]});
        let items = transform_call_hierarchy_prepare_response_to_host(
            response,
            &virtual_uri.to_uri_string(),
            &host_uri,
            &offset,
            &CallHierarchyEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region",
                injection_language: "lua",
                revision: CallHierarchyDocumentRevision {
                    incarnation: Some(2),
                    content_version: 3,
                },
                connection_generation: 4,
                connection_key: &key,
                offset: &offset,
                host_layer: false,
                projected_from_virtual: false,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(items[0].uri, host_uri);
        assert_eq!(items[0].range.start, Position::new(3, 2));
        assert_eq!(items[0].range.end, Position::new(4, 1));
        assert_eq!(items[0].selection_range.start, Position::new(3, 2));
        assert_eq!(
            items[0].data.as_ref().unwrap()["kakehashi"]["inner"],
            json!({ "token": 9 })
        );
        assert_eq!(
            items[0].data.as_ref().unwrap()["kakehashi"]["content_version"],
            3
        );
    }

    #[test]
    fn prepare_response_filters_cross_region_virtual_item() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let offset = RegionOffset::new(3, 0);
        let request_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-a");
        let other_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-b");
        let response = json!({ "jsonrpc": "2.0", "id": 7, "result": [{
            "name": "f", "kind": 12, "uri": other_uri.to_uri_string(),
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }
        }]});
        let items = transform_call_hierarchy_prepare_response_to_host(
            response,
            &request_uri.to_uri_string(),
            &host_uri,
            &offset,
            &CallHierarchyEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region-a",
                injection_language: "lua",
                revision: CallHierarchyDocumentRevision {
                    incarnation: Some(1),
                    content_version: 1,
                },
                connection_generation: 1,
                connection_key: &key,
                offset: &offset,
                host_layer: false,
                projected_from_virtual: false,
            },
        )
        .unwrap()
        .unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn incoming_request_restores_virtual_item_and_inner_data() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let envelope = incoming_test_envelope(&host_uri, &key);
        let mut item = call_item(host_uri.as_str(), json!({ "kakehashi": envelope.clone() }));
        let stripped = strip_call_hierarchy_envelope(&mut item).unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let outgoing = call_hierarchy_item_to_downstream(item, &stripped, Some(&virtual_uri));

        assert_eq!(outgoing.uri.as_str(), virtual_uri.to_uri_string());
        assert_eq!(outgoing.range.start, Position::new(0, 0));
        assert_eq!(outgoing.selection_range.end, Position::new(0, 1));
        assert_eq!(outgoing.data, Some(json!({ "token": 9 })));
    }

    #[test]
    fn incoming_request_strips_unroutable_progress_tokens() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let envelope = incoming_test_envelope(&host_uri, &key);
        let params: CallHierarchyIncomingCallsParams = serde_json::from_value(json!({
            "item": call_item(host_uri.as_str(), json!({ "kakehashi": envelope })),
            "workDoneToken": "work",
            "partialResultToken": "partial"
        }))
        .unwrap();

        let (params, _) = prepare_incoming_params(params).unwrap();
        let value = serde_json::to_value(params).unwrap();
        assert!(value.get("workDoneToken").is_none());
        assert!(value.get("partialResultToken").is_none());
    }

    #[test]
    fn incoming_request_preserves_real_item_matching_the_host_uri() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let offset = RegionOffset::new(3, 2);
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let response = json!({ "result": [{
            "name": "real", "kind": 12, "uri": host_uri,
            "range": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 5 } },
            "selectionRange": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 3 } }
        }]});
        let mut item = transform_call_hierarchy_prepare_response_to_host(
            response,
            &virtual_uri.to_uri_string(),
            &host_uri,
            &offset,
            &CallHierarchyEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region",
                injection_language: "lua",
                revision: CallHierarchyDocumentRevision {
                    incarnation: Some(2),
                    content_version: 3,
                },
                connection_generation: 4,
                connection_key: &key,
                offset: &offset,
                host_layer: false,
                projected_from_virtual: false,
            },
        )
        .unwrap()
        .unwrap()
        .remove(0);
        let envelope = strip_call_hierarchy_envelope(&mut item).unwrap();

        assert!(!envelope.projected_from_virtual);
        let outgoing = call_hierarchy_item_to_downstream(item, &envelope, Some(&virtual_uri));
        assert_eq!(outgoing.uri, host_uri);
        assert_eq!(outgoing.range.start, Position::new(3, 2));
    }

    #[test]
    fn incoming_response_translates_same_virtual_and_preserves_real_callers() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let envelope = incoming_test_envelope(&host_uri, &key);
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let other_virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "other");
        let virtual_item = json!({
            "name": "virtual", "kind": 12, "uri": virtual_uri.to_uri_string(),
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "data": { "caller": "virtual" }
        });
        let external_item = json!({
            "name": "external", "kind": 12, "uri": "file:///external.lua",
            "range": { "start": { "line": 8, "character": 0 }, "end": { "line": 8, "character": 3 } },
            "selectionRange": { "start": { "line": 8, "character": 0 }, "end": { "line": 8, "character": 1 } },
            "data": { "caller": "external" }
        });
        let mut cross_item = virtual_item.clone();
        cross_item["uri"] = json!(other_virtual_uri.to_uri_string());
        let response = json!({ "result": [
            { "from": virtual_item, "fromRanges": [{ "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 2 } }] },
            { "from": external_item, "fromRanges": [{ "start": { "line": 8, "character": 1 }, "end": { "line": 8, "character": 2 } }] },
            { "from": cross_item, "fromRanges": [] }
        ]});

        let calls = transform_call_hierarchy_incoming_response_to_host(
            response,
            Some(virtual_uri.to_uri_string()),
            &host_uri,
            &envelope,
        )
        .unwrap()
        .unwrap();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].from.uri, host_uri);
        assert_eq!(calls[0].from.range.start, Position::new(3, 2));
        assert_eq!(calls[0].from_ranges[0].start, Position::new(3, 3));
        assert_eq!(calls[1].from.uri.as_str(), "file:///external.lua");
        assert_eq!(calls[1].from.range.start, Position::new(8, 0));
        assert_eq!(calls[1].from_ranges[0].start, Position::new(8, 1));
        assert_eq!(
            extract_call_hierarchy_envelope(&calls[1].from)
                .unwrap()
                .inner,
            Some(json!({ "caller": "external" }))
        );
    }
}
