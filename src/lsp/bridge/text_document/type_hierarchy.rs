//! Type-hierarchy requests for host and virtual bridge layers.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{
    NumberOrString, PartialResultParams, Position, Range, SymbolKind, SymbolTag,
    TextDocumentIdentifier, TextDocumentPositionParams, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySupertypesParams, Uri, WorkDoneProgressParams,
};

use super::super::pool::ConnectionKey;
use super::super::pool::{ConnectionState, LanguageServerPool, UpstreamId, VirtualUriObserver};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, response_has_jsonrpc_error,
    translate_host_position_to_virtual, translate_host_range_to_virtual,
    translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use super::completion::EnvelopeOffset;
use crate::config::settings::BridgeServerConfig;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;

impl LanguageServerPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_type_hierarchy_prepare_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &url::Url,
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
    ) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/prepareTypeHierarchy") {
            return Ok(None);
        }
        let connection_key = handle.key().clone();
        let connection_generation = self.document_connection_generation(&connection_key);
        let virtual_uri_observer =
            self.observe_virtual_uris_for_connection(&connection_key, connection_generation);
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
            "textDocument/prepareTypeHierarchy",
            |virtual_uri, request_id| {
                build_type_hierarchy_prepare_request(
                    virtual_uri,
                    host_position,
                    &offset,
                    request_id,
                    work_done_token,
                )
            },
            |response, ctx| {
                transform_type_hierarchy_prepare_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.offset,
                    &virtual_uri_observer,
                    &TypeHierarchyEnvelopeContext {
                        server_name,
                        host_uri: host_uri.as_str(),
                        region_id,
                        injection_language,
                        revision: TypeHierarchyDocumentRevision {
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

    pub(crate) async fn dispatch_type_hierarchy_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
        settings: &crate::config::settings::WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<TypeHierarchyItem>> {
        let (params, envelope) = prepare_supertypes_params(params)?;
        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            return None;
        }
        let config = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        )?;
        self.send_type_hierarchy_supertypes_request(&config, params, envelope, upstream_id)
            .await
    }

    async fn send_type_hierarchy_supertypes_request(
        &self,
        server_config: &BridgeServerConfig,
        mut params: TypeHierarchySupertypesParams,
        envelope: TypeHierarchyEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Vec<TypeHierarchyItem>> {
        const METHOD: &str = "typeHierarchy/supertypes";
        let server_name = &envelope.origin;
        let host_uri = url::Url::parse(&envelope.host_uri).ok()?;
        let expected_incarnation = envelope.incarnation?;
        if self.current_host_incarnation(&host_uri) != Some(expected_incarnation)
            || envelope.connection_key.server() != server_name
            || self.document_connection_generation(&envelope.connection_key)
                != envelope.connection_generation
        {
            return None;
        }
        let handle = self
            .ready_connection_by_key_for_config(&envelope.connection_key, Some(server_config))
            .await?;
        let host_lifecycle = self
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
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(pair) => pair,
                Err(error) => {
                    log::warn!(
                        target: "kakehashi::bridge",
                        "{METHOD}: failed to register request for {server_name}: {error}"
                    );
                    if let Some(ref id) = upstream_id {
                        self.unregister_upstream_request(id, connection_key);
                    }
                    return None;
                }
            };
        params.item =
            type_hierarchy_item_to_downstream(params.item, &envelope, virtual_uri.as_ref());
        let request = build_type_hierarchy_expansion_request(request_id, METHOD, params);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);
        let (send_result, virtual_uri_observer) = {
            let connections = self.connections().await;
            let producer_is_live = connections.get(connection_key).is_some_and(|current| {
                Arc::ptr_eq(current, &handle) && current.state() == ConnectionState::Ready
            });
            let generation_matches = self.document_connection_generation(connection_key)
                == envelope.connection_generation;
            if !producer_is_live || !generation_matches {
                (
                    Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "producer connection was replaced before supertypes send",
                    )),
                    None,
                )
            } else {
                let observer = self.observe_virtual_uris_for_connection(
                    connection_key,
                    envelope.connection_generation,
                );
                if let Some(uri) = virtual_uri.as_ref().map(VirtualDocumentUri::to_uri_string) {
                    observer.insert(uri);
                }
                (
                    handle.send_request(request, request_id).map_err(Into::into),
                    Some(observer),
                )
            }
        };
        if let Err(error) = send_result {
            log::warn!(
                target: "kakehashi::bridge",
                "{METHOD}: failed to send request for {server_name}: {error}"
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, connection_key);
            }
            return None;
        }
        let virtual_uri_observer = virtual_uri_observer?;
        drop(host_lifecycle);
        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, connection_key);
        }
        let response = response.ok()?;
        if !self
            .type_hierarchy_producer_is_live(
                connection_key,
                &handle,
                envelope.connection_generation,
            )
            .await
        {
            return None;
        }
        transform_type_hierarchy_expansion_response_to_host(
            response,
            METHOD,
            virtual_uri.as_ref().map(VirtualDocumentUri::to_uri_string),
            &host_uri_lsp,
            &envelope,
            &virtual_uri_observer,
        )
        .ok()?
    }

    async fn type_hierarchy_producer_is_live(
        &self,
        connection_key: &ConnectionKey,
        handle: &Arc<super::super::pool::ConnectionHandle>,
        expected_generation: u64,
    ) -> bool {
        let connections = self.connections().await;
        connections.get(connection_key).is_some_and(|current| {
            Arc::ptr_eq(current, handle) && current.state() == ConnectionState::Ready
        }) && self.document_connection_generation(connection_key) == expected_generation
    }
}

const ENVELOPE_KEY: &str = "kakehashi";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct TypeHierarchyEnvelope {
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

impl TypeHierarchyEnvelope {
    pub(crate) fn is_host_layer(&self) -> bool {
        self.host_layer && self.region_id.is_empty()
    }
}

pub(crate) fn extract_type_hierarchy_envelope(
    item: &TypeHierarchyItem,
) -> Option<TypeHierarchyEnvelope> {
    serde_json::from_value(item.data.as_ref()?.get(ENVELOPE_KEY)?.clone()).ok()
}

fn strip_type_hierarchy_envelope(item: &mut TypeHierarchyItem) -> Option<TypeHierarchyEnvelope> {
    let mut envelope = extract_type_hierarchy_envelope(item)?;
    item.data = envelope.inner.take();
    Some(envelope)
}

fn prepare_supertypes_params(
    mut params: TypeHierarchySupertypesParams,
) -> Option<(TypeHierarchySupertypesParams, TypeHierarchyEnvelope)> {
    let envelope = strip_type_hierarchy_envelope(&mut params.item)?;
    params.work_done_progress_params = WorkDoneProgressParams::default();
    params.partial_result_params = PartialResultParams::default();
    Some((params, envelope))
}

fn type_hierarchy_item_to_downstream(
    mut item: TypeHierarchyItem,
    envelope: &TypeHierarchyEnvelope,
    virtual_uri: Option<&VirtualDocumentUri>,
) -> TypeHierarchyItem {
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

fn re_envelope_item(
    item: &mut TypeHierarchyItem,
    envelope: &TypeHierarchyEnvelope,
    projected_from_virtual: bool,
) {
    let offset = RegionOffset::from(&envelope.offset);
    envelope_item_data(
        item,
        &TypeHierarchyEnvelopeContext {
            server_name: &envelope.origin,
            host_uri: &envelope.host_uri,
            region_id: &envelope.region_id,
            injection_language: &envelope.injection_language,
            revision: TypeHierarchyDocumentRevision {
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

pub(crate) struct TypeHierarchyDocumentRevision {
    pub(crate) incarnation: Option<u64>,
    pub(crate) content_version: u64,
}

pub(crate) struct TypeHierarchyEnvelopeContext<'a> {
    pub(crate) server_name: &'a str,
    pub(crate) host_uri: &'a str,
    pub(crate) region_id: &'a str,
    pub(crate) injection_language: &'a str,
    pub(crate) revision: TypeHierarchyDocumentRevision,
    pub(crate) connection_generation: u64,
    pub(crate) connection_key: &'a ConnectionKey,
    pub(crate) offset: &'a RegionOffset,
    pub(crate) host_layer: bool,
    pub(crate) projected_from_virtual: bool,
}

fn envelope_item_data(item: &mut TypeHierarchyItem, ctx: &TypeHierarchyEnvelopeContext<'_>) {
    let inner = item.data.take();
    item.data = Some(serde_json::json!({ ENVELOPE_KEY: TypeHierarchyEnvelope {
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

pub(crate) fn envelope_host_type_hierarchy_items(
    items: &mut [TypeHierarchyItem],
    server_name: &str,
    host_uri: &str,
    revision: TypeHierarchyDocumentRevision,
    connection_generation: u64,
    connection_key: &ConnectionKey,
) {
    let offset = RegionOffset::new(0, 0);
    let ctx = TypeHierarchyEnvelopeContext {
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

fn build_type_hierarchy_prepare_request(
    virtual_uri: &VirtualDocumentUri,
    mut host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
    work_done_token: Option<NumberOrString>,
) -> JsonRpcRequest<TypeHierarchyPrepareParams> {
    translate_host_position_to_virtual(&mut host_position, offset);
    JsonRpcRequest::new(
        request_id.as_i64(),
        "textDocument/prepareTypeHierarchy",
        TypeHierarchyPrepareParams {
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

fn build_type_hierarchy_expansion_request(
    request_id: RequestId,
    method: &'static str,
    params: TypeHierarchySupertypesParams,
) -> JsonRpcRequest<Value> {
    let mut params = serde_json::to_value(params).unwrap_or(Value::Null);
    if let Some(tags) = params.pointer_mut("/item/tags")
        && !tags.is_array()
        && !tags.is_null()
    {
        *tags = Value::Array(vec![tags.take()]);
    }
    JsonRpcRequest::new(request_id.as_i64(), method, params)
}

fn transform_type_hierarchy_prepare_response_to_host(
    mut response: Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    known_virtual_uris: &impl KnownVirtualUris,
    envelope_ctx: &TypeHierarchyEnvelopeContext<'_>,
) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
    const METHOD: &str = "textDocument/prepareTypeHierarchy";
    if response_has_jsonrpc_error(&response, METHOD) {
        return Ok(None);
    }
    let Some(result) = response.get_mut("result").map(Value::take) else {
        return Err(io::Error::other(format!(
            "{METHOD} response carries neither result nor error (protocol violation)"
        )));
    };
    if result.is_null() {
        return Ok(None);
    }
    let items = parse_type_hierarchy_items(result).map_err(|error| {
        io::Error::other(format!(
            "malformed {METHOD} result from downstream server: {error}"
        ))
    })?;
    let items = items
        .into_iter()
        .filter_map(|mut item| {
            let projected_from_virtual = if known_virtual_uris.contains_uri(item.uri.as_str()) {
                if item.uri.as_str() != request_virtual_uri {
                    return None;
                }
                item.uri = host_uri.clone();
                translate_virtual_range_to_host(&mut item.range, offset);
                translate_virtual_range_to_host(&mut item.selection_range, offset);
                true
            } else {
                false
            };
            envelope_item_data(
                &mut item,
                &TypeHierarchyEnvelopeContext {
                    server_name: envelope_ctx.server_name,
                    host_uri: envelope_ctx.host_uri,
                    region_id: envelope_ctx.region_id,
                    injection_language: envelope_ctx.injection_language,
                    revision: TypeHierarchyDocumentRevision {
                        incarnation: envelope_ctx.revision.incarnation,
                        content_version: envelope_ctx.revision.content_version,
                    },
                    connection_generation: envelope_ctx.connection_generation,
                    connection_key: envelope_ctx.connection_key,
                    offset: envelope_ctx.offset,
                    host_layer: envelope_ctx.host_layer,
                    projected_from_virtual,
                },
            );
            Some(item)
        })
        .collect();
    Ok(Some(items))
}

fn transform_type_hierarchy_expansion_response_to_host(
    mut response: Value,
    method: &str,
    request_virtual_uri: Option<String>,
    host_uri: &Uri,
    envelope: &TypeHierarchyEnvelope,
    known_virtual_uris: &impl KnownVirtualUris,
) -> io::Result<Option<Vec<TypeHierarchyItem>>> {
    if response_has_jsonrpc_error(&response, method) {
        return Ok(None);
    }
    let Some(result) = response.get_mut("result").map(Value::take) else {
        return Err(io::Error::other(format!(
            "{method} response carries neither result nor error (protocol violation)"
        )));
    };
    if result.is_null() {
        return Ok(None);
    }
    let items = parse_type_hierarchy_items(result).map_err(|error| {
        io::Error::other(format!(
            "malformed {method} result from downstream server: {error}"
        ))
    })?;
    let offset = RegionOffset::from(&envelope.offset);
    let items = items
        .into_iter()
        .filter_map(|mut item| {
            let projected_from_virtual = if known_virtual_uris.contains_uri(item.uri.as_str()) {
                if request_virtual_uri.as_deref() != Some(item.uri.as_str()) {
                    return None;
                }
                item.uri = host_uri.clone();
                translate_virtual_range_to_host(&mut item.range, &offset);
                translate_virtual_range_to_host(&mut item.selection_range, &offset);
                true
            } else {
                false
            };
            re_envelope_item(&mut item, envelope, projected_from_virtual);
            Some(item)
        })
        .collect();
    Ok(Some(items))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTypeHierarchyItem {
    name: String,
    kind: SymbolKind,
    tags: Option<Vec<SymbolTag>>,
    detail: Option<String>,
    uri: Uri,
    range: Range,
    selection_range: Range,
    data: Option<Value>,
}

pub(crate) fn parse_type_hierarchy_items(
    value: Value,
) -> serde_json::Result<Vec<TypeHierarchyItem>> {
    serde_json::from_value::<Vec<WireTypeHierarchyItem>>(value).map(|items| {
        items
            .into_iter()
            .map(|item| TypeHierarchyItem {
                name: item.name,
                kind: item.kind,
                // LSP currently defines only `Deprecated`, so the incorrect
                // scalar field in ls-types can retain every known semantic bit.
                tags: item.tags.and_then(|tags| tags.into_iter().next()),
                detail: item.detail,
                uri: item.uri,
                range: item.range,
                selection_range: item.selection_range,
                data: item.data,
            })
            .collect()
    })
}

trait KnownVirtualUris {
    fn contains_uri(&self, uri: &str) -> bool;
}

impl KnownVirtualUris for HashSet<String> {
    fn contains_uri(&self, uri: &str) -> bool {
        self.contains(uri)
    }
}

impl KnownVirtualUris for VirtualUriObserver {
    fn contains_uri(&self, uri: &str) -> bool {
        self.contains(uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::{RegionOffset, RequestId, VirtualDocumentUri};
    use tower_lsp_server::ls_types::{NumberOrString, Position};

    fn test_envelope(host_uri: &Uri, key: &ConnectionKey) -> TypeHierarchyEnvelope {
        TypeHierarchyEnvelope {
            origin: "lua-ls".into(),
            host_uri: host_uri.to_string(),
            region_id: "region".into(),
            injection_language: "lua".into(),
            incarnation: Some(2),
            content_version: 3,
            connection_generation: 4,
            connection_key: key.clone(),
            offset: EnvelopeOffset::from(&RegionOffset::new(3, 2)),
            inner: Some(serde_json::json!({ "token": 9 })),
            host_layer: false,
            projected_from_virtual: true,
        }
    }

    fn type_item(uri: &str, data: Value) -> TypeHierarchyItem {
        serde_json::from_value(serde_json::json!({
            "name": "Child", "kind": 5, "uri": uri,
            "range": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 7 } },
            "selectionRange": { "start": { "line": 3, "character": 2 }, "end": { "line": 3, "character": 7 } },
            "data": data
        }))
        .unwrap()
    }

    #[test]
    fn supertypes_request_restores_virtual_item_and_strips_progress_tokens() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let params: TypeHierarchySupertypesParams = serde_json::from_value(serde_json::json!({
            "item": type_item(host_uri.as_str(), serde_json::json!({
                "kakehashi": test_envelope(&host_uri, &key)
            })),
            "workDoneToken": "work",
            "partialResultToken": "partial"
        }))
        .unwrap();

        let (mut params, envelope) = prepare_supertypes_params(params).unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        params.item = type_hierarchy_item_to_downstream(params.item, &envelope, Some(&virtual_uri));
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["item"]["uri"], virtual_uri.to_uri_string());
        assert_eq!(
            value["item"]["range"]["start"],
            serde_json::json!({ "line": 0, "character": 0 })
        );
        assert_eq!(value["item"]["data"], serde_json::json!({ "token": 9 }));
        assert!(value.get("workDoneToken").is_none());
        assert!(value.get("partialResultToken").is_none());
    }

    #[test]
    fn prepare_request_uses_virtual_position_and_progress_token() {
        let host_uri = "file:///test.md".parse().unwrap();
        let request = build_type_hierarchy_prepare_request(
            &VirtualDocumentUri::new(&host_uri, "lua", "region"),
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
            serde_json::json!({ "line": 0, "character": 2 })
        );
        assert_eq!(value["params"]["workDoneToken"], "progress");
    }

    #[test]
    fn prepare_response_translates_virtual_items_and_preserves_inner_data() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let offset = RegionOffset::new(3, 2);
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let response = serde_json::json!({ "result": [{
            "name": "Child",
            "kind": 5,
            "uri": virtual_uri.to_uri_string(),
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 1 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 5 }
            },
            "data": { "token": 9 }
        }]});
        let known_virtual_uris = HashSet::from([virtual_uri.to_uri_string()]);
        let items = transform_type_hierarchy_prepare_response_to_host(
            response,
            &virtual_uri.to_uri_string(),
            &host_uri,
            &offset,
            &known_virtual_uris,
            &TypeHierarchyEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region",
                injection_language: "lua",
                revision: TypeHierarchyDocumentRevision {
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
        assert_eq!(
            items[0].data.as_ref().unwrap()["kakehashi"]["inner"],
            serde_json::json!({ "token": 9 })
        );
        assert_eq!(
            items[0].data.as_ref().unwrap()["kakehashi"]["projected_from_virtual"],
            true
        );
    }

    #[test]
    fn prepare_response_preserves_unissued_virtual_shaped_real_uri() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let offset = RegionOffset::new(3, 2);
        let request_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let shaped_real_uri = "file:///external/kakehashi-virtual-uri-real.lua";
        let response = serde_json::json!({ "result": [{
            "name": "External", "kind": 5, "uri": shaped_real_uri,
            "range": { "start": { "line": 8, "character": 0 }, "end": { "line": 8, "character": 8 } },
            "selectionRange": { "start": { "line": 8, "character": 0 }, "end": { "line": 8, "character": 8 } }
        }]});
        let known_virtual_uris = HashSet::from([request_uri.to_uri_string()]);

        let items = transform_type_hierarchy_prepare_response_to_host(
            response,
            &request_uri.to_uri_string(),
            &host_uri,
            &offset,
            &known_virtual_uris,
            &TypeHierarchyEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region",
                injection_language: "lua",
                revision: TypeHierarchyDocumentRevision {
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

        assert_eq!(items[0].uri.as_str(), shaped_real_uri);
        assert_eq!(items[0].range.start, Position::new(8, 0));
        assert_eq!(
            items[0].data.as_ref().unwrap()["kakehashi"]["projected_from_virtual"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn prepare_response_filters_an_issued_sibling_virtual_uri() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let offset = RegionOffset::new(3, 2);
        let request_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-a");
        let sibling_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-b");
        let response = serde_json::json!({ "result": [{
            "name": "Sibling", "kind": 5, "uri": sibling_uri.to_uri_string(),
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } }
        }]});
        let known_virtual_uris =
            HashSet::from([request_uri.to_uri_string(), sibling_uri.to_uri_string()]);

        let items = transform_type_hierarchy_prepare_response_to_host(
            response,
            &request_uri.to_uri_string(),
            &host_uri,
            &offset,
            &known_virtual_uris,
            &TypeHierarchyEnvelopeContext {
                server_name: "lua-ls",
                host_uri: host_uri.as_str(),
                region_id: "region-a",
                injection_language: "lua",
                revision: TypeHierarchyDocumentRevision {
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

        assert!(items.is_empty());
    }

    #[test]
    fn expansion_response_filters_an_issued_sibling_virtual_uri() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let envelope = test_envelope(&host_uri, &key);
        let request_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let sibling_uri = VirtualDocumentUri::new(&host_uri, "lua", "sibling");
        let response = serde_json::json!({ "result": [{
            "name": "Sibling", "kind": 5, "uri": sibling_uri.to_uri_string(),
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 7 } }
        }]});
        let known_virtual_uris =
            HashSet::from([request_uri.to_uri_string(), sibling_uri.to_uri_string()]);

        let items = transform_type_hierarchy_expansion_response_to_host(
            response,
            "typeHierarchy/supertypes",
            Some(request_uri.to_uri_string()),
            &host_uri,
            &envelope,
            &known_virtual_uris,
        )
        .unwrap()
        .unwrap();

        assert!(items.is_empty());
    }

    #[test]
    fn expansion_response_preserves_unissued_virtual_shaped_real_uri() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let envelope = test_envelope(&host_uri, &key);
        let request_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let shaped_real_uri = "file:///external/kakehashi-virtual-uri-real.lua";
        let response = serde_json::json!({ "result": [{
            "name": "External", "kind": 5, "uri": shaped_real_uri,
            "range": { "start": { "line": 8, "character": 0 }, "end": { "line": 8, "character": 8 } },
            "selectionRange": { "start": { "line": 8, "character": 0 }, "end": { "line": 8, "character": 8 } }
        }]});
        let known_virtual_uris = HashSet::from([request_uri.to_uri_string()]);

        let items = transform_type_hierarchy_expansion_response_to_host(
            response,
            "typeHierarchy/supertypes",
            Some(request_uri.to_uri_string()),
            &host_uri,
            &envelope,
            &known_virtual_uris,
        )
        .unwrap()
        .unwrap();

        assert_eq!(items[0].uri.as_str(), shaped_real_uri);
        assert_eq!(items[0].range.start, Position::new(8, 0));
        assert_eq!(
            items[0].data.as_ref().unwrap()["kakehashi"]["projected_from_virtual"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn expansion_response_reenvelope_routes_the_next_request_recursively() {
        let host_uri: Uri = "file:///test.md".parse().unwrap();
        let key = ConnectionKey::shared("lua-ls");
        let envelope = test_envelope(&host_uri, &key);
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region");
        let response = serde_json::json!({ "result": [{
            "name": "Parent", "kind": 5, "uri": virtual_uri.to_uri_string(),
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 6 } },
            "data": { "token": "parent" }
        }]});
        let known_virtual_uris = HashSet::from([virtual_uri.to_uri_string()]);
        let item = transform_type_hierarchy_expansion_response_to_host(
            response,
            "typeHierarchy/supertypes",
            Some(virtual_uri.to_uri_string()),
            &host_uri,
            &envelope,
            &known_virtual_uris,
        )
        .unwrap()
        .unwrap()
        .remove(0);
        assert_eq!(item.uri, host_uri);
        assert_eq!(item.range.start, Position::new(3, 2));

        let params: TypeHierarchySupertypesParams =
            serde_json::from_value(serde_json::json!({ "item": item })).unwrap();
        let (params, next_envelope) = prepare_supertypes_params(params).unwrap();
        let downstream =
            type_hierarchy_item_to_downstream(params.item, &next_envelope, Some(&virtual_uri));
        let downstream = serde_json::to_value(downstream).unwrap();

        assert_eq!(downstream["uri"], virtual_uri.to_uri_string());
        assert_eq!(
            downstream["range"]["start"],
            serde_json::json!({ "line": 0, "character": 0 })
        );
        assert_eq!(downstream["data"], serde_json::json!({ "token": "parent" }));
    }

    #[test]
    fn prepare_response_accepts_protocol_array_tags() {
        let value = serde_json::json!([{
            "name": "Deprecated", "kind": 5, "tags": [1], "uri": "file:///type.lua",
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } },
            "selectionRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } }
        }]);

        let items = parse_type_hierarchy_items(value).unwrap();

        assert_eq!(items[0].tags, Some(SymbolTag::DEPRECATED));
    }
}
