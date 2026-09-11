//! Inline value request handling for virtual bridge connections.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    InlineValue, InlineValueContext, InlineValueParams, NumberOrString, Position, Range,
    TextDocumentIdentifier, WorkDoneProgressParams,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    host_position_within_region_bounds, response_has_jsonrpc_error,
    translate_host_range_to_virtual, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};
use crate::text::PositionMapper;

impl LanguageServerPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_inline_value_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_range: Range,
        context: InlineValueContext,
        region_end: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
        expected_incarnation: u64,
    ) -> io::Result<Option<Vec<InlineValue>>> {
        if context.stopped_location.start > context.stopped_location.end
            || !host_position_within_region_bounds(
                context.stopped_location.start,
                &offset,
                region_end,
            )
            || !host_position_within_region_bounds(
                context.stopped_location.end,
                &offset,
                region_end,
            )
        {
            return Ok(None);
        }
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/inlineValue") {
            return Ok(None);
        }
        self.execute_bridge_request_observed(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            Some(expected_incarnation),
            |virtual_uri, request_id| {
                build_inline_value_request(
                    virtual_uri,
                    host_range,
                    context,
                    &offset,
                    request_id,
                    client_progress_token,
                )
            },
            |response, ctx| {
                transform_inline_value_response_to_host(
                    response,
                    ctx.offset,
                    region_end,
                    virtual_content,
                )
            },
            None,
        )
        .await
    }
}

fn build_inline_value_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    mut context: InlineValueContext,
    offset: &RegionOffset,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<InlineValueParams> {
    let mut range = host_range;
    translate_host_range_to_virtual(&mut range, offset);
    translate_host_range_to_virtual(&mut context.stopped_location, offset);
    JsonRpcRequest::new(
        request_id.as_i64(),
        "textDocument/inlineValue",
        InlineValueParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: client_progress_token,
            },
            text_document: TextDocumentIdentifier {
                uri: virtual_uri_to_lsp_uri(virtual_uri),
            },
            range,
            context,
        },
    )
}

fn transform_inline_value_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
) -> Option<Vec<InlineValue>> {
    if response_has_jsonrpc_error(&response, "textDocument/inlineValue") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    let mut values: Vec<InlineValue> = serde_json::from_value(result).ok()?;
    let mapper = PositionMapper::new(virtual_content);
    values.retain_mut(|value| {
        let range = match value {
            InlineValue::Text(value) => &mut value.range,
            InlineValue::VariableLookup(value) => &mut value.range,
            InlineValue::EvaluatableExpression(value) => &mut value.range,
        };
        if range.start > range.end
            || mapper.position_to_byte_strict(range.start).is_none()
            || mapper.position_to_byte_strict(range.end).is_none()
        {
            return false;
        }
        translate_virtual_range_to_host(range, offset);
        host_position_within_region_bounds(range.start, offset, region_end)
            && host_position_within_region_bounds(range.end, offset, region_end)
    });
    (!values.is_empty()).then_some(values)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn request_translates_range_and_stopped_location() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_inline_value_request(
            &virtual_uri,
            Range::new(Position::new(3, 2), Position::new(3, 6)),
            InlineValueContext {
                frame_id: 7,
                stopped_location: Range::new(Position::new(3, 3), Position::new(3, 5)),
            },
            &RegionOffset::new(3, 2),
            test_request_id(),
            None,
        );
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value["params"]["range"],
            json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 4 }
            })
        );
        assert_eq!(
            value["params"]["context"]["stoppedLocation"],
            json!({
                "start": { "line": 0, "character": 1 },
                "end": { "line": 0, "character": 3 }
            })
        );
    }

    #[test]
    fn response_translates_all_variants_and_drops_escaping_ranges() {
        let response = json!({ "result": [
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } }, "text": "text" },
            { "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 3 } }, "variableName": "x", "caseSensitiveLookup": true },
            { "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 3 } }, "expression": "x" },
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 0 } }, "text": "escape" },
            { "range": { "start": { "line": 0, "character": 999 }, "end": { "line": 1, "character": 4 } }, "text": "overlong" }
        ] });
        let values = transform_inline_value_response_to_host(
            response,
            &RegionOffset::with_per_line_offsets(3, vec![2, 2]),
            Position::new(4, 6),
            "code\nnext",
        )
        .unwrap();
        assert_eq!(values.len(), 3);
        let value = serde_json::to_value(values).unwrap();
        assert_eq!(
            value[0]["range"]["start"],
            json!({ "line": 3, "character": 2 })
        );
        assert_eq!(
            value[1]["range"]["start"],
            json!({ "line": 3, "character": 3 })
        );
        assert_eq!(
            value[2]["range"]["end"],
            json!({ "line": 3, "character": 5 })
        );
    }

    #[tokio::test]
    async fn request_rejects_stopped_location_outside_the_region() {
        let pool = LanguageServerPool::new();
        let result = pool
            .send_inline_value_request(
                "unused",
                &BridgeServerConfig::default(),
                &Url::parse("file:///doc.md").unwrap(),
                Range::new(Position::new(3, 2), Position::new(3, 6)),
                InlineValueContext {
                    frame_id: 7,
                    stopped_location: Range::new(Position::new(2, 0), Position::new(2, 1)),
                },
                Position::new(3, 6),
                "lua",
                "region-0",
                RegionOffset::new(3, 2),
                "code",
                None,
                None,
                1,
            )
            .await
            .unwrap();
        assert!(result.is_none());
    }
}
