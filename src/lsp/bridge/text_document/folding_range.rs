//! Folding range request handling for bridge connections, with host/virtual
//! coordinate transformation.
//!
//! Like document link requests, folding range requests operate on the entire
//! document — they take no position parameter.
//!
//! Requests are queued via the channel-based writer task (`send_request()`) for
//! FIFO ordering with other messages (ls-bridge-message-ordering single-writer loop).

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{FoldingRange, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    Bias, DocumentParams, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_whole_document_request, response_has_jsonrpc_error,
};

impl LanguageServerPool {
    /// Send a folding range request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing folding-range-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_folding_range_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/foldingRange") {
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
            build_folding_range_request,
            |response, ctx| transform_folding_range_response_to_host(response, ctx.offset),
        )
        .await
    }
}

/// Build a JSON-RPC folding range request for a downstream language server.
fn build_folding_range_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentParams> {
    build_whole_document_request(virtual_uri, request_id, "textDocument/foldingRange")
}

/// Transform a folding range response from virtual to host document coordinates.
///
/// `FoldingRange` uses bare line numbers plus optional character offsets rather
/// than a `Range`, so translation mirrors `translate_virtual_position_to_host`:
/// lines shift by the region's start line, and the optional characters shift by
/// the per-line column offset of the virtual line they belong to.
fn transform_folding_range_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
) -> Option<Vec<FoldingRange>> {
    if response_has_jsonrpc_error(&response, "textDocument/foldingRange") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    let mut ranges: Vec<FoldingRange> = serde_json::from_value(result).ok()?;

    if let Some(prepared) = offset.prepared() {
        // A prepared document's lines need not match the virtual document's
        // (a gap may have grown or shrunk, edge lines gone), so each
        // endpoint maps as a position first; the arithmetic below then works
        // in the virtual document. An omitted character is the line's
        // length (LSP), for the start as for the end: a prepared line can
        // join several virtual ones, and the fold starts on the last.
        for folding in &mut ranges {
            let start = prepared.to_virtual(
                Position::new(
                    folding.start_line,
                    folding.start_character.unwrap_or(u32::MAX),
                ),
                Bias::Start,
            );
            let end = prepared.to_virtual(
                Position::new(folding.end_line, folding.end_character.unwrap_or(u32::MAX)),
                Bias::End,
            );
            // A character given stays given, at its mapped column. One
            // omitted (the line's end) stays omitted only while the mapped
            // endpoint is still its virtual line's end: a prepared line may
            // be part of a virtual one (a gap turned into a line break), and
            // an omitted character there would read as that line's end.
            let character = |given: Option<u32>, at: Position| {
                (given.is_some() || at.character != prepared.virtual_line_length(at.line))
                    .then_some(at.character)
            };
            folding.start_character = character(folding.start_character, start);
            folding.end_character = character(folding.end_character, end);
            folding.start_line = start.line;
            folding.end_line = end.line;
        }
    }

    for folding in &mut ranges {
        let virtual_start_line = folding.start_line;
        let virtual_end_line = folding.end_line;
        folding.start_line = folding.start_line.saturating_add(offset.line());
        folding.end_line = folding.end_line.saturating_add(offset.line());
        folding.start_character = folding
            .start_character
            .map(|c| c.saturating_add(offset.column_for_line(virtual_start_line)));
        folding.end_character = folding
            .end_character
            .map(|c| c.saturating_add(offset.column_for_line(virtual_end_line)));
    }

    Some(ranges)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    // ==========================================================================
    // Request builder tests
    // ==========================================================================

    /// A prepared document's lines need not be the virtual document's: each
    /// endpoint maps through the prepared map before the region's offset,
    /// and an absent character stays absent.
    #[test]
    fn a_prepared_folding_range_maps_its_lines_through_the_map() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        // The peer deletes the two leading blank lines: prepared line 0 is
        // virtual line 2.
        let virtual_text = "\n\nfoo\n  bar\nend";
        let answer = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 0}}, "newText": ""}
        ]}]}))
        .unwrap();
        let map = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(answer),
        )
        .unwrap()
        .map;
        let response = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [
            {"startLine": 0, "endLine": 2},
            {"startLine": 1, "startCharacter": 2, "endLine": 2, "endCharacter": 3}
        ]});
        let ranges = transform_folding_range_response_to_host(
            response,
            &RegionOffset::new(10, 0).with_prepared(map),
        )
        .unwrap();
        assert_eq!((ranges[0].start_line, ranges[0].end_line), (12, 14));
        assert_eq!(ranges[0].start_character, None);
        assert_eq!(ranges[0].end_character, None);
        assert_eq!(
            (
                ranges[1].start_line,
                ranges[1].start_character,
                ranges[1].end_line,
                ranges[1].end_character
            ),
            (13, Some(2), 14, Some(3))
        );
    }

    /// A gap emptied by its placeholder joins two virtual lines into one
    /// prepared line: a fold given without characters starts and ends at
    /// that line's end (LSP: an omitted character is the line's length),
    /// on the virtual line the joined line ends on.
    #[test]
    fn a_fold_without_an_end_character_ends_where_its_prepared_line_does() {
        use crate::language::injection::VirtualGap;
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let virtual_text = "ab\n--\ncd";
        let gaps = [VirtualGap {
            virtual_range: 2..6,
            host_text: "\n--\n".to_string(),
        }];
        let answer = serde_json::from_value(serde_json::json!({"segments": [
            {"type": "content"},
            {"type": "gap", "content": ""},
            {"type": "content"}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::from_gaps(virtual_text, &gaps),
            Some(answer),
        )
        .unwrap();
        assert_eq!(prepared.text, "abcd");
        let response = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [
            {"startLine": 0, "endLine": 0}
        ]});
        let ranges = transform_folding_range_response_to_host(
            response,
            &RegionOffset::new(10, 0).with_prepared(prepared.map),
        )
        .unwrap();
        assert_eq!((ranges[0].start_line, ranges[0].end_line), (12, 12));
    }

    /// A fold over a prepared line joining several virtual ones starts where
    /// that line ends — on the last virtual line it joins — not on its
    /// first: starting there would fold host lines the fold leaves out.
    #[test]
    fn a_fold_without_a_start_character_starts_where_its_prepared_line_ends() {
        use crate::language::injection::VirtualGap;
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let virtual_text = "ab\n  \ncd\nend";
        let gaps = [VirtualGap {
            virtual_range: 2..6,
            host_text: "\n  \n".to_string(),
        }];
        let answer = serde_json::from_value(serde_json::json!({"segments": [
            {"type": "content"},
            {"type": "gap", "content": ""},
            {"type": "content"}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::from_gaps(virtual_text, &gaps),
            Some(answer),
        )
        .unwrap();
        assert_eq!(prepared.text, "abcd\nend");
        let response = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [
            {"startLine": 0, "endLine": 1},
            {"startLine": 0, "startCharacter": 1, "endLine": 1}
        ]});
        let ranges = transform_folding_range_response_to_host(
            response,
            &RegionOffset::new(0, 0).with_prepared(prepared.map),
        )
        .unwrap();
        assert_eq!((ranges[0].start_line, ranges[0].end_line), (2, 3));
        assert_eq!(ranges[0].start_character, None);
        // A start character given is mapped as is: "b" is on line 0.
        assert_eq!(
            (ranges[1].start_line, ranges[1].start_character),
            (0, Some(1))
        );
    }

    /// A gap turned into a line break splits one virtual line into two
    /// prepared ones: a fold over them given without characters starts
    /// mid-line in the virtual document, so its start character is given
    /// there (omitted would mean the line's end); its end, at the virtual
    /// line's end, stays omitted.
    #[test]
    fn a_fold_over_a_split_line_gives_its_start_character() {
        use crate::language::injection::VirtualGap;
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let virtual_text = "abc  def";
        let gaps = [VirtualGap {
            virtual_range: 3..5,
            host_text: "--".to_string(),
        }];
        let answer = serde_json::from_value(serde_json::json!({"segments": [
            {"type": "content"},
            {"type": "gap", "content": "\n"},
            {"type": "content"}
        ]}))
        .unwrap();
        let prepared = apply_prepare_result(
            virtual_text,
            &VirtualLayout::from_gaps(virtual_text, &gaps),
            Some(answer),
        )
        .unwrap();
        assert_eq!(prepared.text, "abc\ndef");
        let response = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": [
            {"startLine": 0, "endLine": 1},
            {"startLine": 0, "startCharacter": 1, "endLine": 1, "endCharacter": 2}
        ]});
        let ranges = transform_folding_range_response_to_host(
            response,
            &RegionOffset::new(10, 4).with_prepared(prepared.map),
        )
        .unwrap();
        let fold = |range: &FoldingRange| {
            (
                range.start_line,
                range.start_character,
                range.end_line,
                range.end_character,
            )
        };
        // Host column = virtual column + the region's 4.
        assert_eq!(fold(&ranges[0]), (10, Some(7), 10, None));
        // Characters given are mapped: P (1, 2) is before "f", V column 7.
        assert_eq!(fold(&ranges[1]), (10, Some(5), 10, Some(11)));
    }

    #[test]
    fn folding_range_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_folding_range_request(&virtual_uri, RequestId::new(42));

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn folding_range_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_folding_range_request(&virtual_uri, RequestId::new(123));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/foldingRange");
        assert!(
            json["params"].get("position").is_none(),
            "FoldingRange request should not have position parameter"
        );
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[test]
    fn folding_range_response_shifts_lines_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": 0, "endLine": 3, "kind": "region" },
                { "startLine": 5, "endLine": 8 }
            ]
        });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(10, 0));

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start_line, 10);
        assert_eq!(ranges[0].end_line, 13);
        assert_eq!(
            ranges[0].kind,
            Some(tower_lsp_server::ls_types::FoldingRangeKind::Region)
        );
        assert_eq!(ranges[1].start_line, 15);
        assert_eq!(ranges[1].end_line, 18);
    }

    #[test]
    fn folding_range_response_shifts_characters_with_per_line_columns() {
        // Blockquote-style region: every line is prefixed by "> " (2 columns)
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": 0, "startCharacter": 4, "endLine": 1, "endCharacter": 7 }
            ]
        });

        let transformed = transform_folding_range_response_to_host(
            response,
            &RegionOffset::with_per_line_offsets(3, vec![2, 2]),
        );

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(ranges[0].start_line, 3);
        assert_eq!(ranges[0].start_character, Some(6)); // 4 + 2
        assert_eq!(ranges[0].end_line, 4);
        assert_eq!(ranges[0].end_character, Some(9)); // 7 + 2
    }

    #[test]
    fn folding_range_response_preserves_missing_characters() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": 1, "endLine": 2, "collapsedText": "..." }
            ]
        });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(5, 0));

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(ranges[0].start_line, 6);
        assert_eq!(ranges[0].end_line, 7);
        assert!(ranges[0].start_character.is_none());
        assert!(ranges[0].end_character.is_none());
        assert_eq!(ranges[0].collapsed_text.as_deref(), Some("..."));
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn folding_range_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(transformed.is_none());
    }

    #[test]
    fn folding_range_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(transformed.expect("Should parse empty array").is_empty());
    }

    #[test]
    fn folding_range_response_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": u32::MAX, "endLine": u32::MAX }
            ]
        });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(10, 0));

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(
            ranges[0].start_line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }
}
