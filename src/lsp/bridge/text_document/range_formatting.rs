//! `textDocument/rangeFormatting` bridge handler.
//!
//! Unlike full formatting, range formatting carries a host-coordinate `Range`
//! in the request. The bridge translates that range into virtual coordinates
//! before forwarding, and the response transformer ([`transform_formatting_response_to_host`])
//! is shared with full formatting since the payload shape is identical.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.
//!
//! # Multi-line edit limitation
//!
//! Same caveat as full formatting: positions translate correctly via
//! [`RegionOffset::column_for_line`], but the `new_text` payload of a
//! multi-line edit inside a prefixed injection (indented or blockquoted code
//! block) is not re-indented. Single-line edits — which dominate the
//! "format the selected range" use case — are unaffected.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    DocumentRangeFormattingParams, FormattingOptions, Range, TextDocumentIdentifier, TextEdit,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, translate_host_range_to_virtual,
};
use super::formatting::{count_lines, transform_formatting_response_to_host};

impl LanguageServerPool {
    /// Send a `textDocument/rangeFormatting` request and wait for the response.
    ///
    /// Returns `Ok(None)` when the downstream server does not advertise
    /// `documentRangeFormattingProvider`.
    ///
    /// `host_range` is the **already-clipped** host range — the caller is
    /// responsible for intersecting the editor-supplied range with the
    /// injection region's line span (see
    /// `lsp_impl::text_document::range_formatting` for the clipping logic).
    /// This handler simply translates that range from host to virtual
    /// coordinates and forwards.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_range_formatting_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        host_range: Range,
        options: FormattingOptions,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/rangeFormatting") {
            return Ok(None);
        }
        let virtual_line_count = count_lines(virtual_content);
        let offset_for_request = offset.clone();
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            move |virtual_uri, request_id| {
                build_range_formatting_request(
                    virtual_uri,
                    host_range,
                    &offset_for_request,
                    options,
                    request_id,
                )
            },
            |response, ctx| {
                transform_formatting_response_to_host(response, ctx.offset, virtual_line_count)
            },
        )
        .await
    }
}

/// Build a JSON-RPC `textDocument/rangeFormatting` request.
///
/// Translates the host range into virtual coordinates so the downstream
/// language server formats only the slice of the virtual document that
/// corresponds to the editor's selection. Translation uses saturating
/// arithmetic — if a race condition leaves a stale offset, the request
/// will degrade rather than panic (the downstream server sees a virtual
/// range clipped at the document edges).
fn build_range_formatting_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    offset: &RegionOffset,
    options: FormattingOptions,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentRangeFormattingParams> {
    let mut virtual_range = host_range;
    translate_host_range_to_virtual(&mut virtual_range, offset);

    let params = DocumentRangeFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri.to_lsp_uri(),
        },
        range: virtual_range,
        options,
        work_done_progress_params: Default::default(),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/rangeFormatting", params)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use tower_lsp_server::ls_types::Position;

    fn default_options() -> FormattingOptions {
        FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..Default::default()
        }
    }

    fn range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Range {
        Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        }
    }

    #[test]
    fn range_formatting_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_range_formatting_request(
            &virtual_uri,
            range(5, 0, 8, 0),
            &RegionOffset::new(5, 0),
            default_options(),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn range_formatting_request_translates_range_to_virtual_coordinates() {
        // Region starts at host line 8; request covers host lines 10..20.
        // Virtual range: 2..12 (subtract 8).
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_range_formatting_request(
            &virtual_uri,
            range(10, 5, 20, 30),
            &RegionOffset::new(8, 0),
            default_options(),
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
        assert_eq!(json["method"], "textDocument/rangeFormatting");
        assert_eq!(json["params"]["range"]["start"]["line"], 2);
        assert_eq!(json["params"]["range"]["start"]["character"], 5);
        assert_eq!(json["params"]["range"]["end"]["line"], 12);
        assert_eq!(json["params"]["range"]["end"]["character"], 30);
    }

    #[test]
    fn range_formatting_request_first_line_applies_column_offset() {
        // Host range starts on the first line of the region (line 5),
        // region starts at column 4 → virtual start column = 10 - 4 = 6.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_range_formatting_request(
            &virtual_uri,
            range(5, 10, 8, 15),
            &RegionOffset::new(5, 4),
            default_options(),
            RequestId::new(1),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["range"]["start"]["line"], 0);
        assert_eq!(json["params"]["range"]["start"]["character"], 6);
        assert_eq!(json["params"]["range"]["end"]["line"], 3);
        // End is on virtual line 3 (host line 8 - region line 5), not the
        // first line, so column offset is not applied.
        assert_eq!(json["params"]["range"]["end"]["character"], 15);
    }

    #[test]
    fn range_formatting_request_forwards_options() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let options = FormattingOptions {
            tab_size: 2,
            insert_spaces: false,
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(false),
            trim_final_newlines: Some(false),
            ..Default::default()
        };

        let request = build_range_formatting_request(
            &virtual_uri,
            range(0, 0, 2, 0),
            &RegionOffset::new(0, 0),
            options,
            RequestId::new(1),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["options"]["tabSize"], 2);
        assert_eq!(json["params"]["options"]["insertSpaces"], false);
        assert_eq!(json["params"]["options"]["trimTrailingWhitespace"], true);
        assert_eq!(json["params"]["options"]["insertFinalNewline"], false);
        assert_eq!(json["params"]["options"]["trimFinalNewlines"], false);
    }

    #[test]
    fn range_formatting_request_range_saturates_at_zero() {
        // Stale region: host range starts before the region.
        // saturating_sub: virtual start = 0.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_range_formatting_request(
            &virtual_uri,
            range(2, 0, 5, 0),
            &RegionOffset::new(10, 0),
            default_options(),
            RequestId::new(1),
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["range"]["start"]["line"], 0);
        assert_eq!(json["params"]["range"]["end"]["line"], 0);
    }
}
