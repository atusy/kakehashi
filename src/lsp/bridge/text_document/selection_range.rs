//! Selection-range requests for virtual bridge connections.

use std::io;

use tower_lsp_server::ls_types::{
    PartialResultParams, Position, SelectionRange, SelectionRangeParams, TextDocumentIdentifier,
    WorkDoneProgressParams,
};
use url::Url;

use crate::config::settings::BridgeServerConfig;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    host_position_within_region_bounds, response_has_jsonrpc_error,
    translate_host_position_to_virtual, translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};

const METHOD: &str = "textDocument/selectionRange";

impl LanguageServerPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_selection_range_request(
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
        upstream_request_id: Option<UpstreamId>,
        expected_incarnation: u64,
    ) -> io::Result<Option<SelectionRange>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability(METHOD) {
            return Ok(None);
        }
        self.execute_position_bridge_request_with_handle_for_incarnation(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            expected_incarnation,
            host_position,
            region_end,
            METHOD,
            |virtual_uri, request_id| {
                build_selection_range_request(virtual_uri, host_position, &offset, request_id)
            },
            |response, ctx| {
                transform_selection_range_response_to_host(
                    response,
                    ctx.offset,
                    virtual_content,
                    host_position,
                    region_end,
                )
            },
        )
        .await?
    }
}

fn build_selection_range_request(
    virtual_uri: &VirtualDocumentUri,
    mut host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<SelectionRangeParams> {
    translate_host_position_to_virtual(&mut host_position, offset);
    JsonRpcRequest::new(
        request_id.as_i64(),
        METHOD,
        SelectionRangeParams {
            text_document: TextDocumentIdentifier {
                uri: virtual_uri_to_lsp_uri(virtual_uri),
            },
            positions: vec![host_position],
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
}

fn transform_selection_range_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    virtual_content: &str,
    host_position: Position,
    region_end: Position,
) -> io::Result<Option<SelectionRange>> {
    if response_has_jsonrpc_error(&response, METHOD) {
        return Ok(None);
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        return Err(io::Error::other(
            "textDocument/selectionRange response carries neither result nor error (protocol violation)",
        ));
    };
    if result.is_null() {
        return Ok(None);
    }
    let mut ranges: Vec<SelectionRange> = serde_json::from_value(result).map_err(|error| {
        io::Error::other(format!(
            "malformed textDocument/selectionRange result from downstream server: {error}"
        ))
    })?;
    if ranges.len() != 1 {
        return Err(io::Error::other(format!(
            "textDocument/selectionRange returned {} ranges for one position",
            ranges.len()
        )));
    }

    let mut selection = ranges.pop().expect("length checked");
    translate_and_validate_chain(
        &mut selection,
        offset,
        virtual_content,
        host_position,
        region_end,
    )?;
    Ok(Some(selection))
}

fn translate_and_validate_chain(
    selection: &mut SelectionRange,
    offset: &RegionOffset,
    virtual_content: &str,
    host_position: Position,
    region_end: Position,
) -> io::Result<()> {
    let mapper = crate::text::PositionMapper::new(virtual_content);
    let document_end = mapper.byte_to_position(virtual_content.len());
    let mut virtual_position = host_position;
    translate_host_position_to_virtual(&mut virtual_position, offset);
    let mut child_range = None;
    let mut current = selection;
    loop {
        let range = current.range;
        let valid_bounds = range.start <= range.end
            && mapper.position_to_byte_strict(range.start).is_some()
            && mapper.position_to_byte_strict(range.end).is_some();
        let contains_child = child_range.is_none_or(|child: tower_lsp_server::ls_types::Range| {
            range.start <= child.start && range.end >= child.end
        });
        if !valid_bounds || !contains_child {
            return Err(io::Error::other(
                "invalid textDocument/selectionRange hierarchy from downstream server",
            ));
        }
        if child_range.is_none()
            && !((range.start == virtual_position && range.end == virtual_position)
                || (range.start <= virtual_position
                    && (virtual_position < range.end
                        || (Some(virtual_position) == document_end
                            && virtual_position == range.end))))
        {
            return Err(io::Error::other(
                "textDocument/selectionRange does not contain the requested position",
            ));
        }
        child_range = Some(range);
        translate_virtual_range_to_host(&mut current.range, offset);
        if !host_position_within_region_bounds(current.range.start, offset, region_end)
            || !host_position_within_region_bounds(current.range.end, offset, region_end)
        {
            return Err(io::Error::other(
                "textDocument/selectionRange hierarchy escapes its virtual region",
            ));
        }
        let Some(parent) = current.parent.as_deref_mut() else {
            break;
        };
        current = parent;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn request_uses_virtual_uri_and_translates_one_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_selection_range_request(
            &virtual_uri,
            Position::new(5, 7),
            &RegionOffset::new(3, 2),
            test_request_id(),
        );
        let value = serde_json::to_value(request).expect("serialize request");
        assert_eq!(value["method"], METHOD);
        assert!(
            value["params"]["textDocument"]["uri"]
                .as_str()
                .expect("virtual URI")
                .contains("lua")
        );
        assert_eq!(
            value["params"]["positions"],
            json!([{ "line": 2, "character": 7 }])
        );
    }

    #[test]
    fn response_translates_the_entire_parent_chain() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 4 } },
                "parent": {
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 1, "character": 5 } }
                }
            }]
        });
        let result = transform_selection_range_response_to_host(
            response,
            &RegionOffset::with_per_line_offsets(3, vec![2, 2]),
            "code\nvalue",
            Position::new(3, 4),
            Position::new(4, 7),
        )
        .expect("valid response")
        .expect("selection range");
        assert_eq!(result.range.start, Position::new(3, 3));
        assert_eq!(result.range.end, Position::new(3, 6));
        let parent = result.parent.expect("parent");
        assert_eq!(parent.range.start, Position::new(3, 2));
        assert_eq!(parent.range.end, Position::new(4, 7));
    }

    #[test]
    fn response_rejects_wrong_count_and_non_containing_parent() {
        let wrong_count = json!({ "jsonrpc": "2.0", "id": 1, "result": [] });
        assert!(
            transform_selection_range_response_to_host(
                wrong_count,
                &RegionOffset::new(3, 0),
                "code",
                Position::new(3, 1),
                Position::new(3, 9),
            )
            .is_err()
        );

        let bad_parent = json!({
            "jsonrpc": "2.0", "id": 1, "result": [{
                "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 3 } },
                "parent": { "range": { "start": { "line": 0, "character": 2 }, "end": { "line": 0, "character": 4 } } }
            }]
        });
        assert!(
            transform_selection_range_response_to_host(
                bad_parent,
                &RegionOffset::new(3, 0),
                "code",
                Position::new(3, 2),
                Position::new(3, 9),
            )
            .is_err()
        );

        let overlong_column = json!({
            "jsonrpc": "2.0", "id": 1, "result": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 999 } }
            }]
        });
        assert!(
            transform_selection_range_response_to_host(
                overlong_column,
                &RegionOffset::new(3, 0),
                "a\nb",
                Position::new(3, 0),
                Position::new(4, 1),
            )
            .is_err(),
            "an overlong intermediate-line column must not spill into a later line"
        );
    }

    #[test]
    fn response_uses_end_exclusive_containment_but_allows_empty_at_position() {
        let ending_at_position = json!({
            "jsonrpc": "2.0", "id": 1, "result": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 2 } }
            }]
        });
        assert!(
            transform_selection_range_response_to_host(
                ending_at_position,
                &RegionOffset::new(3, 0),
                "code",
                Position::new(3, 2),
                Position::new(3, 4),
            )
            .is_err()
        );

        let empty_at_position = json!({
            "jsonrpc": "2.0", "id": 1, "result": [{
                "range": { "start": { "line": 0, "character": 2 }, "end": { "line": 0, "character": 2 } }
            }]
        });
        assert!(
            transform_selection_range_response_to_host(
                empty_at_position,
                &RegionOffset::new(3, 0),
                "code",
                Position::new(3, 2),
                Position::new(3, 4),
            )
            .expect("valid empty range")
            .is_some()
        );
    }

    #[test]
    fn response_allows_a_nonempty_range_ending_at_requested_virtual_eof() {
        let ending_at_eof = json!({
            "jsonrpc": "2.0", "id": 1, "result": [{
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 4 } }
            }]
        });

        assert!(
            transform_selection_range_response_to_host(
                ending_at_eof,
                &RegionOffset::new(3, 0),
                "code",
                Position::new(3, 4),
                Position::new(3, 4),
            )
            .expect("the requested EOF belongs to a range ending at EOF")
            .is_some()
        );
    }
}
