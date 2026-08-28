//! Semantic-token requests for virtual bridge connections.

use std::io;

use tower_lsp_server::ls_types::{
    NumberOrString, PartialResultParams, Position, Range, SemanticToken, SemanticTokens,
    SemanticTokensLegend, SemanticTokensRangeParams, SemanticTokensRangeResult,
    TextDocumentIdentifier, WorkDoneProgressParams,
};
use url::Url;

use crate::analysis::filter_semantic_tokens_by_range;
use crate::analysis::{LEGEND_MODIFIERS, LEGEND_TYPES};
use crate::config::settings::BridgeServerConfig;
use crate::text::PositionMapper;

use super::super::HostDocument;
use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    host_position_within_region_bounds, response_has_jsonrpc_error,
    translate_host_range_to_virtual, translate_virtual_position_to_host, virtual_uri_to_lsp_uri,
};

const RANGE_METHOD: &str = "textDocument/semanticTokens/range";

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
    ) -> io::Result<Option<SemanticTokens>> {
        let Some(raw) = self
            .send_host_raw_request_for_incarnation(
                server_name,
                server_config,
                document,
                RANGE_METHOD,
                params,
                upstream_request_id,
                expected_incarnation,
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
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
) -> Option<SemanticTokens> {
    if response_has_jsonrpc_error(&response, RANGE_METHOD) {
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

pub(crate) fn transform_semantic_tokens_result_to_host(
    result: serde_json::Value,
    legend: &SemanticTokensLegend,
    offset: &RegionOffset,
    region_end: Position,
    virtual_content: &str,
    host_range: Range,
) -> Option<SemanticTokens> {
    let result: SemanticTokensRangeResult = serde_json::from_value(result).ok()?;
    let data = match result {
        SemanticTokensRangeResult::Tokens(tokens) => tokens.data,
        SemanticTokensRangeResult::Partial(partial) => partial.data,
    };
    let mapper = PositionMapper::new(virtual_content);
    let mut absolute_line = 0u32;
    let mut absolute_start = 0u32;
    let mut host_tokens = Vec::with_capacity(data.len());

    for token in data {
        absolute_line = absolute_line.checked_add(token.delta_line)?;
        absolute_start = if token.delta_line == 0 {
            absolute_start.checked_add(token.delta_start)?
        } else {
            token.delta_start
        };
        let start = Position::new(absolute_line, absolute_start);
        let end = Position::new(absolute_line, absolute_start.checked_add(token.length)?);
        if mapper.position_to_byte_strict(start).is_none()
            || mapper.position_to_byte_strict(end).is_none()
        {
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
    Some(filter_semantic_tokens_by_range(&tokens, &host_range))
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
            0, 0, 4, 2, 5,
            1, 0, 4, 1, 1,
            0, 999, 1, 1, 0,
            1, 0, 1, 0, 0
        ] } });
        let tokens = transform_semantic_tokens_response_to_host(
            response,
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
}
