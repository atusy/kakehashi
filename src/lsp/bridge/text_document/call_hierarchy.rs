//! Call-hierarchy preparation for host and virtual bridge layers.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{
    CallHierarchyItem, CallHierarchyPrepareParams, NumberOrString, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, Uri, WorkDoneProgressParams,
};
use url::Url;

use crate::config::settings::BridgeServerConfig;

use super::super::pool::{ConnectionKey, LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, response_has_jsonrpc_error,
    translate_host_position_to_virtual, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use super::completion::EnvelopeOffset;

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
}

fn is_false(value: &bool) -> bool {
    !*value
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
        content_version: u64,
        upstream_request_id: Option<UpstreamId>,
        work_done_token: Option<NumberOrString>,
    ) -> io::Result<Option<Vec<CallHierarchyItem>>> {
        let incarnation = self.current_host_incarnation(host_uri);
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
        self.execute_position_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
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
                            incarnation,
                            content_version,
                        },
                        connection_generation,
                        connection_key: &connection_key,
                        offset: ctx.offset,
                        host_layer: false,
                    },
                )
            },
        )
        .await?
    }
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
            if VirtualDocumentUri::is_virtual_uri(uri) {
                if uri != request_virtual_uri {
                    return None;
                }
                item.uri = host_uri.clone();
                translate_virtual_range_to_host(&mut item.range, offset);
                translate_virtual_range_to_host(&mut item.selection_range, offset);
            }
            envelope_item_data(&mut item, envelope_ctx);
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

    #[test]
    fn prepare_request_uses_virtual_position_and_progress_token() {
        let request = build_call_hierarchy_prepare_request(
            &VirtualDocumentUri::new(&test_host_uri(), "lua", "region"),
            Position::new(5, 4),
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
            json!({ "line": 2, "character": 4 })
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
            },
        )
        .unwrap()
        .unwrap();
        assert!(items.is_empty());
    }
}
