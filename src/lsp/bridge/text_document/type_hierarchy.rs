//! Type-hierarchy requests for host and virtual bridge layers.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{
    NumberOrString, Position, TextDocumentIdentifier, TextDocumentPositionParams,
    TypeHierarchyItem, TypeHierarchyPrepareParams, Uri, WorkDoneProgressParams,
};

use super::super::pool::ConnectionKey;
use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, response_has_jsonrpc_error,
    translate_host_position_to_virtual, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use super::completion::EnvelopeOffset;
use crate::config::settings::BridgeServerConfig;

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

fn transform_type_hierarchy_prepare_response_to_host(
    mut response: Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
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
    let items: Vec<TypeHierarchyItem> = serde_json::from_value(result).map_err(|error| {
        io::Error::other(format!(
            "malformed {METHOD} result from downstream server: {error}"
        ))
    })?;
    let items = items
        .into_iter()
        .filter_map(|mut item| {
            let projected_from_virtual = if VirtualDocumentUri::is_virtual_uri(item.uri.as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::{RegionOffset, RequestId, VirtualDocumentUri};
    use tower_lsp_server::ls_types::{NumberOrString, Position};

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
        let items = transform_type_hierarchy_prepare_response_to_host(
            response,
            &virtual_uri.to_uri_string(),
            &host_uri,
            &offset,
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
}
