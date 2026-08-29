//! Semantic-token requests for virtual bridge connections.

use std::io;

use tower_lsp_server::ls_types::{
    NumberOrString, PartialResultParams, Position, Range, SemanticToken, SemanticTokens,
    SemanticTokensLegend, SemanticTokensParams, SemanticTokensRangeParams,
    SemanticTokensRangeResult, TextDocumentIdentifier, WorkDoneProgressParams,
};
use url::Url;

use crate::analysis::filter_semantic_tokens_by_range;
use crate::analysis::{LEGEND_MODIFIERS, LEGEND_TYPES};
use crate::config::settings::BridgeServerConfig;
use crate::text::PositionMapper;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    host_position_within_region_bounds, response_has_jsonrpc_error,
    translate_host_range_to_virtual, translate_virtual_position_to_host, virtual_uri_to_lsp_uri,
};
use super::super::{HostDocument, HostTextReader};

const RANGE_METHOD: &str = "textDocument/semanticTokens/range";
const FULL_METHOD: &str = "textDocument/semanticTokens/full";

impl LanguageServerPool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_host_semantic_tokens_range_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        document: &HostDocument<'_>,
        params: serde_json::Value,
        host_range: Range,
        upstream_request_id: Option<UpstreamId>,
        expected_incarnation: u64,
        revision_text_reader: HostTextReader,
    ) -> io::Result<Option<SemanticTokens>> {
        self.send_host_semantic_tokens_request(
            server_name,
            server_config,
            document,
            RANGE_METHOD,
            params,
            host_range,
            upstream_request_id,
            expected_incarnation,
            revision_text_reader,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_host_semantic_tokens_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        document: &HostDocument<'_>,
        method: &'static str,
        params: serde_json::Value,
        host_range: Range,
        upstream_request_id: Option<UpstreamId>,
        expected_incarnation: u64,
        revision_text_reader: HostTextReader,
    ) -> io::Result<Option<SemanticTokens>> {
        let Some(raw) = self
            .send_host_raw_request_for_revision(
                server_name,
                server_config,
                document,
                method,
                params,
                upstream_request_id,
                expected_incarnation,
                revision_text_reader,
            )
            .await?
        else {
            return Ok(None);
        };
        let Some(legend) = raw.handle.semantic_tokens_legend() else {
            return Ok(None);
        };
        let mapper = PositionMapper::new(document.text);
        let Some(document_end) = mapper.byte_to_position(document.text.len()) else {
            return Ok(None);
        };
        Ok(transform_semantic_tokens_result_to_host(
            raw.value,
            legend,
            &RegionOffset::new(0, 0),
            document_end,
            document.text,
            host_range,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_semantic_tokens_full_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        region_end: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
        expected_incarnation: Option<u64>,
        attempted: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        selected: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> io::Result<Option<SemanticTokens>> {
        let handle = match self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                if let Some(selected) = selected {
                    selected.store(true, std::sync::atomic::Ordering::Release);
                }
                return Err(error);
            }
        };
        if !handle.has_capability(FULL_METHOD) {
            return Ok(None);
        }
        let Some(legend) = handle.semantic_tokens_legend().cloned() else {
            return Ok(None);
        };
        let host_range = Range::new(
            Position::new(offset.line(), offset.column_for_line(0)),
            region_end,
        );
        if let Some(selected) = selected {
            selected.store(true, std::sync::atomic::Ordering::Release);
        }
        self.execute_bridge_request_observed(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            expected_incarnation,
            attempted,
            |virtual_uri, request_id| {
                build_semantic_tokens_full_request(virtual_uri, request_id, client_progress_token)
            },
            |response, ctx| {
                transform_semantic_tokens_full_response_to_host(
                    response,
                    &legend,
                    ctx.offset,
                    region_end,
                    virtual_content,
                    host_range,
                )
            },
            None,
        )
        .await?
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_semantic_tokens_range_request(
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
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
        expected_incarnation: u64,
    ) -> io::Result<Option<SemanticTokens>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability(RANGE_METHOD) {
            return Ok(None);
        }
        let Some(legend) = handle.semantic_tokens_legend().cloned() else {
            return Ok(None);
        };
        self.execute_bridge_request_observed(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            Some(expected_incarnation),
            None,
            |virtual_uri, request_id| {
                build_semantic_tokens_range_request(
                    virtual_uri,
                    host_range,
                    &offset,
                    request_id,
                    client_progress_token,
                )
            },
            |response, ctx| {
                transform_semantic_tokens_response_to_host(
                    response,
                    RANGE_METHOD,
                    &legend,
                    ctx.offset,
                    region_end,
                    virtual_content,
                    host_range,
                )
            },
            None,
        )
        .await
    }
}

fn build_semantic_tokens_full_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<SemanticTokensParams> {
    JsonRpcRequest::new(
        request_id.as_i64(),
        FULL_METHOD,
        SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: client_progress_token,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: virtual_uri_to_lsp_uri(virtual_uri),
            },
        },
    )
}

fn build_semantic_tokens_range_request(
    virtual_uri: &VirtualDocumentUri,
    mut host_range: Range,
    offset: &RegionOffset,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<SemanticTokensRangeParams> {
    translate_host_range_to_virtual(&mut host_range, offset);
    JsonRpcRequest::new(
        request_id.as_i64(),
        RANGE_METHOD,
        SemanticTokensRangeParams {
            work_done_progress_params: WorkDoneProgressParams {
                work_done_token: client_progress_token,
            },
            partial_result_params: PartialResultParams {
                partial_result_token: None,
            },
            text_document: TextDocumentIdentifier {
                uri: virtual_uri_to_lsp_uri(virtual_uri),
            },
            range: host_range,
        },
    )
}

fn transform_semantic_tokens_response_to_host(
    mut response: serde_json::Value,
    method: &'static str,
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
) -> Option<SemanticTokens> {
    if response_has_jsonrpc_error(&response, method) {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    transform_semantic_tokens_result_to_host(
        result,
        legend,
        offset,
        region_end,
        virtual_content,
        host_range,
    )
}

fn transform_semantic_tokens_full_response_to_host(
    mut response: serde_json::Value,
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
) -> io::Result<Option<SemanticTokens>> {
    if response_has_jsonrpc_error(&response, FULL_METHOD) {
        return Err(io::Error::other(
            "downstream semanticTokens/full returned a JSON-RPC error",
        ));
    }
    let result = response
        .get_mut("result")
        .map(serde_json::Value::take)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing semantic token result")
        })?;
    if result.is_null() {
        return Ok(None);
    }
    transform_semantic_tokens_result_to_host_strict(
        result,
        legend,
        offset,
        region_end,
        virtual_content,
        host_range,
    )
    .map(Some)
}

pub(crate) fn transform_semantic_tokens_result_to_host(
    result: serde_json::Value,
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
) -> Option<SemanticTokens> {
    transform_semantic_tokens_result_to_host_inner(
        result,
        legend,
        offset,
        region_end,
        virtual_content,
        host_range,
        false,
    )
    .ok()
}

pub(crate) fn transform_semantic_tokens_result_to_host_strict(
    result: serde_json::Value,
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
) -> io::Result<SemanticTokens> {
    transform_semantic_tokens_result_to_host_inner(
        result,
        legend,
        offset,
        region_end,
        virtual_content,
        host_range,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn transform_semantic_tokens_result_to_host_inner(
    result: serde_json::Value,
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
    reject_invalid_coordinates: bool,
) -> io::Result<SemanticTokens> {
    let result: SemanticTokensRangeResult = serde_json::from_value(result).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid semantic token result: {error}"),
        )
    })?;
    let data = match result {
        SemanticTokensRangeResult::Tokens(tokens) => tokens.data,
        SemanticTokensRangeResult::Partial(partial) => partial.data,
    };
    let mapper = PositionMapper::new(virtual_content);
    let mut absolute_line = 0u32;
    let mut absolute_start = 0u32;
    let mut host_tokens = Vec::with_capacity(data.len());

    for token in data {
        absolute_line = absolute_line.checked_add(token.delta_line).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "semantic token line overflow")
        })?;
        absolute_start = if token.delta_line == 0 {
            absolute_start
                .checked_add(token.delta_start)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "semantic token column overflow")
                })?
        } else {
            token.delta_start
        };
        let start = Position::new(absolute_line, absolute_start);
        if token.length == 0 {
            continue;
        }
        let end = Position::new(
            absolute_line,
            absolute_start.checked_add(token.length).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "semantic token length overflow")
            })?,
        );
        if mapper.position_to_byte_strict(start).is_none()
            || mapper.position_to_byte_strict(end).is_none()
        {
            if reject_invalid_coordinates {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "semantic token position is outside the document",
                ));
            }
            continue;
        }
        let Some(token_type) = remap_token_type(token.token_type, legend) else {
            continue;
        };
        let modifiers = remap_token_modifiers(token.token_modifiers_bitset, legend);
        let mut host_start = start;
        translate_virtual_position_to_host(&mut host_start, offset);
        if !host_position_within_region_bounds(host_start, offset, region_end) {
            continue;
        }
        host_tokens.push((
            host_start.line,
            host_start.character,
            token.length,
            token_type,
            modifiers,
        ));
    }

    let tokens = SemanticTokens {
        result_id: None,
        data: encode_absolute_tokens(host_tokens),
    };
    Ok(filter_semantic_tokens_by_range(&tokens, &host_range))
}

fn remap_token_type(index: u32, legend: &SemanticTokensLegend) -> Option<u32> {
    let name = legend.token_types.get(index as usize)?.as_str();
    LEGEND_TYPES
        .iter()
        .position(|token_type| token_type.as_str() == name)
        .map(|index| index as u32)
}

fn remap_token_modifiers(bitset: u32, legend: &SemanticTokensLegend) -> u32 {
    legend
        .token_modifiers
        .iter()
        .take(u32::BITS as usize)
        .enumerate()
        .fold(0, |mapped, (source_index, modifier)| {
            if bitset & (1 << source_index) == 0 {
                return mapped;
            }
            LEGEND_MODIFIERS
                .iter()
                .position(|candidate| candidate.as_str() == modifier.as_str())
                .map_or(mapped, |target_index| mapped | (1 << target_index))
        })
}

/// Overlay a higher-priority semantic-token layer over a lower-priority one.
///
/// Clients were told that tokens do not overlap. Preserve every uncovered
/// lower-layer fragment, but let higher-layer classifications replace the
/// intersecting spans. Tokens are single-line because the bridge advertises
/// `multilineTokenSupport: false` downstream.
pub(crate) fn merge_semantic_token_layers(
    higher: Vec<SemanticToken>,
    lower: Vec<SemanticToken>,
) -> Vec<SemanticToken> {
    merge_semantic_token_layers_with_observer(higher, lower, || {})
}

fn merge_semantic_token_layers_with_observer(
    higher: Vec<SemanticToken>,
    lower: Vec<SemanticToken>,
    mut inspect_higher: impl FnMut(),
) -> Vec<SemanticToken> {
    let higher = decode_absolute_tokens(higher);
    let lower = decode_absolute_tokens(lower);
    let mut lower_fragments = Vec::with_capacity(lower.len());
    let mut higher_index = 0;

    for (line, start, length, token_type, modifiers) in lower {
        let end = start.saturating_add(length);
        while higher_index < higher.len() {
            inspect_higher();
            let (higher_line, higher_start, higher_length, ..) = higher[higher_index];
            if higher_line < line
                || (higher_line == line && higher_start.saturating_add(higher_length) <= start)
            {
                higher_index += 1;
            } else {
                break;
            }
        }

        let mut cursor = start;
        let mut scan = higher_index;
        while let Some(&(higher_line, higher_start, higher_length, ..)) = higher.get(scan) {
            inspect_higher();
            if higher_line > line || (higher_line == line && higher_start >= end) {
                break;
            }
            if higher_line < line {
                scan += 1;
                continue;
            }
            let from = higher_start.max(start);
            let to = higher_start.saturating_add(higher_length).min(end);
            if cursor < from {
                lower_fragments.push((line, cursor, from - cursor, token_type, modifiers));
            }
            cursor = cursor.max(to);
            scan += 1;
        }
        if cursor < end {
            lower_fragments.push((line, cursor, end - cursor, token_type, modifiers));
        }
    }

    let mut higher = higher.into_iter().peekable();
    let mut lower_fragments = lower_fragments.into_iter().peekable();
    let mut merged = Vec::with_capacity(higher.len() + lower_fragments.len());
    while higher.peek().is_some() || lower_fragments.peek().is_some() {
        let take_higher = match (higher.peek(), lower_fragments.peek()) {
            (Some(&(higher_line, higher_start, ..)), Some(&(lower_line, lower_start, ..))) => {
                (higher_line, higher_start) <= (lower_line, lower_start)
            }
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        merged.push(if take_higher {
            higher.next().expect("peeked higher token")
        } else {
            lower_fragments.next().expect("peeked lower token")
        });
    }
    encode_absolute_tokens(merged)
}

fn decode_absolute_tokens(tokens: Vec<SemanticToken>) -> Vec<(u32, u32, u32, u32, u32)> {
    let mut line = 0u32;
    let mut start = 0u32;
    tokens
        .into_iter()
        .filter_map(|token| {
            line = line.checked_add(token.delta_line)?;
            start = if token.delta_line == 0 {
                start.checked_add(token.delta_start)?
            } else {
                token.delta_start
            };
            Some((
                line,
                start,
                token.length,
                token.token_type,
                token.token_modifiers_bitset,
            ))
        })
        .collect()
}

fn encode_absolute_tokens(tokens: Vec<(u32, u32, u32, u32, u32)>) -> Vec<SemanticToken> {
    let mut previous_line = 0;
    let mut previous_start = 0;
    tokens
        .into_iter()
        .map(
            |(line, start, length, token_type, token_modifiers_bitset)| {
                let delta_line = line - previous_line;
                let delta_start = if delta_line == 0 {
                    start - previous_start
                } else {
                    start
                };
                previous_line = line;
                previous_start = start;
                SemanticToken {
                    delta_line,
                    delta_start,
                    length,
                    token_type,
                    token_modifiers_bitset,
                }
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;
    use tower_lsp_server::ls_types::{SemanticTokenModifier, SemanticTokenType};

    #[test]
    fn full_request_targets_the_virtual_document_without_partial_progress() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_semantic_tokens_full_request(
            &virtual_uri,
            test_request_id(),
            Some(NumberOrString::String("work".to_string())),
        );
        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["method"], json!(FULL_METHOD));
        assert_eq!(value["params"]["workDoneToken"], json!("work"));
        assert!(value["params"].get("partialResultToken").is_none());
        assert_eq!(
            value["params"]["textDocument"]["uri"],
            json!(virtual_uri.to_uri_string())
        );
    }

    #[test]
    fn higher_semantic_layer_splits_and_replaces_lower_spans() {
        let higher = vec![SemanticToken {
            delta_line: 0,
            delta_start: 2,
            length: 3,
            token_type: 1,
            token_modifiers_bitset: 8,
        }];
        let lower = vec![SemanticToken {
            delta_line: 0,
            delta_start: 0,
            length: 6,
            token_type: 2,
            token_modifiers_bitset: 0,
        }];

        assert_eq!(
            merge_semantic_token_layers(higher, lower),
            vec![
                SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 2,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 2,
                    length: 3,
                    token_type: 1,
                    token_modifiers_bitset: 8,
                },
                SemanticToken {
                    delta_line: 0,
                    delta_start: 3,
                    length: 1,
                    token_type: 2,
                    token_modifiers_bitset: 0,
                },
            ]
        );
    }

    #[test]
    fn semantic_layer_overlay_scales_across_large_ordered_streams() {
        let stream = |token_type| {
            (0..10_000)
                .map(|_| SemanticToken {
                    delta_line: 1,
                    delta_start: 0,
                    length: 1,
                    token_type,
                    token_modifiers_bitset: 0,
                })
                .collect::<Vec<_>>()
        };

        let mut inspections = 0usize;
        let merged = merge_semantic_token_layers_with_observer(stream(1), stream(2), || {
            inspections += 1;
        });

        assert_eq!(merged.len(), 10_000);
        assert!(merged.iter().all(|token| token.token_type == 1));
        assert!(
            inspections <= 40_000,
            "ordered overlay must inspect higher tokens linearly, got {inspections} visits"
        );
    }

    #[test]
    fn range_request_translates_coordinates_and_withholds_partial_progress() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_semantic_tokens_range_request(
            &virtual_uri,
            Range::new(Position::new(3, 2), Position::new(4, 6)),
            &RegionOffset::with_per_line_offsets(3, vec![2, 2]),
            test_request_id(),
            Some(NumberOrString::String("work".to_string())),
        );
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value["params"]["range"],
            json!({
                "start": { "line": 0, "character": 0 },
                "end": { "line": 1, "character": 4 }
            })
        );
        assert_eq!(value["params"]["workDoneToken"], json!("work"));
        assert!(value["params"].get("partialResultToken").is_none());
    }

    #[test]
    fn response_remaps_legend_translates_positions_and_drops_invalid_tokens() {
        let legend = SemanticTokensLegend {
            token_types: vec![
                SemanticTokenType::new("custom"),
                SemanticTokenType::VARIABLE,
                SemanticTokenType::KEYWORD,
            ],
            token_modifiers: vec![
                SemanticTokenModifier::STATIC,
                SemanticTokenModifier::new("custom"),
                SemanticTokenModifier::READONLY,
            ],
        };
        let response = json!({ "result": { "data": [
            0, 0, 4, 0, 5,
            0, 0, 0, 2, 0,
            0, 0, 4, 2, 5,
            1, 0, 4, 1, 1,
            0, 999, 1, 1, 0
        ] } });
        let tokens = transform_semantic_tokens_response_to_host(
            response,
            RANGE_METHOD,
            &legend,
            &RegionOffset::with_per_line_offsets(3, vec![2, 2]),
            Position::new(4, 6),
            "code\nnext",
            Range::new(Position::new(3, 2), Position::new(4, 6)),
        )
        .unwrap();

        assert_eq!(tokens.data.len(), 2);
        assert_eq!(tokens.data[0].delta_line, 3);
        assert_eq!(tokens.data[0].delta_start, 2);
        assert_eq!(tokens.data[0].token_type, 1);
        assert_eq!(tokens.data[0].token_modifiers_bitset, (1 << 2) | (1 << 3));
        assert_eq!(tokens.data[1].delta_line, 1);
        assert_eq!(tokens.data[1].delta_start, 2);
        assert_eq!(tokens.data[1].token_type, 17);
        assert_eq!(tokens.data[1].token_modifiers_bitset, 1 << 3);
    }

    #[test]
    fn full_response_distinguishes_valid_empty_from_invalid_payloads() {
        let legend = SemanticTokensLegend {
            token_types: vec![SemanticTokenType::VARIABLE],
            token_modifiers: Vec::new(),
        };
        let transform = |response| {
            transform_semantic_tokens_full_response_to_host(
                response,
                &legend,
                &RegionOffset::new(0, 0),
                Position::new(0, 4),
                "code",
                Range::new(Position::new(0, 0), Position::new(0, 4)),
            )
        };

        assert!(transform(json!({ "result": null })).unwrap().is_none());
        assert!(
            transform(json!({ "result": { "data": [] } }))
                .unwrap()
                .is_some()
        );
        assert!(transform(json!({ "result": { "data": "bad" } })).is_err());
        assert!(
            transform(json!({
                "result": { "data": [0, 5, 1, 0, 0] }
            }))
            .is_err(),
            "a non-empty response outside the virtual document is not a valid empty result"
        );
        assert!(
            transform(json!({
                "error": { "code": -32603, "message": "failed" }
            }))
            .is_err()
        );
    }
}
