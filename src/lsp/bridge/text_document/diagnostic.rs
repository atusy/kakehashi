//! Diagnostic request handling for bridge connections.
//!
//! This module provides pull diagnostic request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! Like document symbol, diagnostic requests operate on the entire document -
//! they don't take a position parameter.
//!
//! Implements Pull-first diagnostic forwarding (pull-first-diagnostic-forwarding Phase 1).
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;
use std::time::Duration;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::Diagnostic;
use url::Url;

use super::super::pool::{INIT_TIMEOUT_SECS, LanguageServerPool, UpstreamId};
use tower_lsp_server::ls_types::{DocumentDiagnosticParams, TextDocumentIdentifier};

use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, jsonrpc_error_summary,
    response_has_jsonrpc_error, virtual_uri_to_lsp_uri,
};

impl LanguageServerPool {
    /// Send a diagnostic request and wait for the response.
    ///
    /// Unlike other request types that fail fast when a server is initializing,
    /// diagnostic requests wait for the server to become Ready so users see
    /// diagnostics appear rather than empty results. After the wait and capability
    /// check, delegates to
    /// [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_diagnostic_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        previous_result_id: Option<&str>,
    ) -> io::Result<Vec<Diagnostic>> {
        // Pre-work: wait for server to become Ready (unlike other handlers that fail fast).
        let handle = self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
                Some(host_uri),
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await?;

        // Skip if server doesn't advertise pull diagnostic support
        // (checked via both static capabilities and dynamic registrations).
        if !handle.has_capability("textDocument/diagnostic") {
            log::debug!(
                target: "kakehashi::bridge",
                "[{}] Server does not support textDocument/diagnostic, skipping",
                server_name
            );
            return Ok(Vec::new());
        }

        // Server is Ready and supports diagnostics — proceed with standard lifecycle.
        // Use execute_bridge_request_with_handle to reuse the pre-fetched handle,
        // avoiding a redundant HashMap lookup.
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_diagnostic_request(virtual_uri, request_id, previous_result_id)
            },
            |response, ctx| {
                // A downstream JSON-RPC error response is a request failure, not
                // "no diagnostics" — propagate it as `Err` so CLI mode's
                // request-error sink can surface it (mirrors the formatting
                // path; LSP mode just logs and the layer stays empty).
                if response_has_jsonrpc_error(&response, "textDocument/diagnostic") {
                    return Err(io::Error::other(format!(
                        "downstream server '{server_name}' answered textDocument/diagnostic \
                         with an error response: {}",
                        jsonrpc_error_summary(&response)
                    )));
                }
                // A present-but-malformed payload (absent `result`, unknown
                // report kind, missing/garbled `items`) is likewise a request
                // failure; only `null`/`unchanged` stays an empty layer.
                transform_diagnostic_response_to_host(response, ctx.offset, host_uri.as_str())
            },
        )
        .await?
    }
}

/// Build a JSON-RPC diagnostic request for a downstream language server.
///
/// Like DocumentSymbolParams, DocumentDiagnosticParams operates on the entire
/// document; an optional `previous_result_id` enables incremental updates.
fn build_diagnostic_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
    previous_result_id: Option<&str>,
) -> JsonRpcRequest<DocumentDiagnosticParams> {
    let params = DocumentDiagnosticParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        identifier: None,
        previous_result_id: previous_result_id.map(|s| s.to_string()),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/diagnostic", params)
}

/// Parse a successful JSON-RPC diagnostic response and transform coordinates to
/// host document space.
///
/// The caller ([`LanguageServerPool::send_diagnostic_request`]) handles a
/// JSON-RPC *error* response separately (it propagates as `Err`), so this only
/// inspects the `result`. Mirroring [`transform_formatting_response_to_host`],
/// a present-but-malformed payload is a request **failure** (`Err`): an absent
/// `result` member (protocol violation), a report with no `kind` or an unknown
/// report `kind`, a `full` report missing `items`, or `items` that fail to
/// deserialize as `Diagnostic[]`. A `null` result or an `unchanged` report is the
/// authoritative "no diagnostics" answer and stays an empty `Vec` (`Ok`).
/// Collapsing a malformed payload into the same empty value as "no
/// capability" would let a broken downstream server pass as "nothing wrong" —
/// the fan-in counts `Err`s, which CLI mode maps onto its `2` exit code and
/// the editor path logs at WARNING. Only related info matching `host_uri` is
/// transformed.
fn transform_diagnostic_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    host_uri: &str,
) -> io::Result<Vec<Diagnostic>> {
    // Absent `result` with no error (the caller already promoted an error
    // response to `Err`) is a protocol violation — a request failure.
    let Some(mut result) = response.get_mut("result").map(serde_json::Value::take) else {
        return Err(io::Error::other(
            "diagnostic response carries neither result nor error (protocol violation)",
        ));
    };
    if result.is_null() {
        // Authoritative "no diagnostics" — a handled response, not a failure.
        return Ok(Vec::new());
    }

    // Check report kind: `unchanged` is an authoritative empty; `full`
    // proceeds; an unknown kind, or an absent/non-string `kind` (the LSP report
    // tag is required), is a malformed payload (request failure). This matches
    // the host parser's tagged-enum deserialize so the two paths reject the
    // same shapes.
    match result.get("kind").and_then(|k| k.as_str()) {
        Some("unchanged") => return Ok(Vec::new()),
        Some("full") => {}
        Some(other) => {
            log::warn!(
                target: "kakehashi::bridge",
                "Unknown diagnostic report kind: {}",
                other
            );
            return Err(io::Error::other(format!(
                "malformed diagnostic result from downstream server: unknown report kind '{other}'"
            )));
        }
        None => {
            return Err(io::Error::other(
                "malformed diagnostic result from downstream server: report has no 'kind'",
            ));
        }
    }

    // A `full` report MUST carry `items`; a missing `items` member is a
    // malformed payload (request failure).
    let Some(items) = result.get_mut("items").map(serde_json::Value::take) else {
        return Err(io::Error::other(
            "malformed diagnostic result from downstream server: full report missing 'items'",
        ));
    };
    // `items` that do not deserialize as `Diagnostic[]` (wrong shape, missing
    // fields, etc.) is likewise a request failure; the log keeps the
    // misbehaving downstream diagnosable.
    let mut diagnostics: Vec<Diagnostic> = match serde_json::from_value(items) {
        Ok(diagnostics) => diagnostics,
        Err(err) => {
            log::warn!(
                target: "kakehashi::bridge",
                "Failed to deserialize diagnostic items as Diagnostic[]: {err}"
            );
            return Err(io::Error::other(format!(
                "malformed diagnostic result from downstream server: {err}"
            )));
        }
    };

    // Transform coordinates on typed structs
    for diag in &mut diagnostics {
        transform_diagnostic(diag, offset, host_uri);
    }

    Ok(diagnostics)
}

/// Transform a single typed Diagnostic by applying the region offset to its range.
///
/// Also transforms relatedInformation locations if present, filtering out entries
/// that reference virtual URIs (which clients cannot resolve).
fn transform_diagnostic(diag: &mut Diagnostic, offset: &RegionOffset, host_uri: &str) {
    // Transform main range
    translate_virtual_range_to_host(&mut diag.range, offset);

    // Transform related information
    if let Some(related) = &mut diag.related_information {
        related.retain_mut(|info| {
            let uri_str = info.location.uri.as_str();
            if VirtualDocumentUri::is_virtual_uri(uri_str) {
                // Virtual URI - filter out this entry
                return false;
            }

            // Only transform ranges for entries that reference the same host document.
            // Related info pointing to other files (e.g., imported modules) should
            // keep their original coordinates since they're not in the injection region.
            if uri_str == host_uri {
                translate_virtual_range_to_host(&mut info.location.range, offset);
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_response_transforms_range_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 5 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "syntax error",
                        "severity": 1
                    },
                    {
                        "range": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 3, "character": 5 }
                        },
                        "message": "undefined variable",
                        "severity": 2
                    }
                ]
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .unwrap();

        assert_eq!(diagnostics.len(), 2);

        // First diagnostic: line 0 + 5 = 5
        assert_eq!(diagnostics[0].range.start.line, 5);
        assert_eq!(diagnostics[0].range.end.line, 5);
        assert_eq!(diagnostics[0].range.start.character, 5); // character unchanged
        assert_eq!(diagnostics[0].message, "syntax error");

        // Second diagnostic: lines 2,3 + 5 = 7,8
        assert_eq!(diagnostics[1].range.start.line, 7);
        assert_eq!(diagnostics[1].range.end.line, 8);
    }

    #[test]
    fn diagnostic_response_transforms_related_information_for_same_host() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "unused variable 'x'",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///test.md",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "'x' is declared here"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics = transform_diagnostic_response_to_host(
            response,
            &RegionOffset::new(3, 0),
            "file:///test.md",
        )
        .unwrap();

        assert_eq!(diagnostics.len(), 1);

        // Main diagnostic range transformed
        assert_eq!(diagnostics[0].range.start.line, 3);

        // Related information location range transformed (same host file)
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.range.start.line, 8); // 5 + 3
        assert_eq!(related[0].location.range.end.line, 8);
    }

    #[test]
    fn diagnostic_response_preserves_related_info_for_different_file() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "type mismatch",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///other_module.lua",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "expected type defined here"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics = transform_diagnostic_response_to_host(
            response,
            &RegionOffset::new(3, 0),
            "file:///test.md",
        )
        .unwrap();

        assert_eq!(diagnostics.len(), 1);

        // Main diagnostic range transformed
        assert_eq!(diagnostics[0].range.start.line, 3);

        // Related info pointing to different file should NOT be transformed
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.uri.as_str(), "file:///other_module.lua");
        assert_eq!(related[0].location.range.start.line, 5); // unchanged!
        assert_eq!(related[0].location.range.end.line, 5);
    }

    #[test]
    fn diagnostic_response_unchanged_kind_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "unchanged",
                "resultId": "prev-123"
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .expect("unchanged is an authoritative empty result, not a failure");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_null_result_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .expect("null is an authoritative empty result, not a failure");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_missing_result_is_request_failure() {
        // Neither `result` nor `error` member — a protocol violation. (An
        // *error* response is promoted to `Err` by the caller before transform
        // runs, so transform only ever sees the no-result-no-error shape here.)
        let response = json!({ "jsonrpc": "2.0", "id": 42 });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(
            transformed.is_err(),
            "a response with neither result nor error must be a request failure"
        );
    }

    #[test]
    fn diagnostic_response_malformed_items_is_request_failure() {
        // A non-`Diagnostic[]` `items` is a malformed payload — must be `Err`
        // (request failure), not the lenient empty, so CLI mode can exit 2 for
        // a broken downstream server.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": "not_an_array"
            }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_unknown_kind_is_request_failure() {
        // An unrecognized report `kind` is a malformed payload — `Err`, not the
        // lenient empty.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "kind": "partial", "items": [] }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_full_report_missing_items_is_request_failure() {
        // A `full` report MUST carry `items`; its absence is malformed → `Err`.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "kind": "full" }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_report_without_kind_is_request_failure() {
        // The LSP report `kind` tag is required; a present report that omits it
        // (e.g. `{ "items": [] }`) is malformed → `Err`, matching the host
        // parser's tagged-enum deserialize. Without this the region path would
        // wrongly read it as an empty `full` report and miss the exit-2 failure.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "items": [] }
        });

        let transformed =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused");
        assert!(transformed.is_err());
    }

    #[test]
    fn diagnostic_response_empty_items_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": []
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(5, 0), "unused")
                .expect("an empty items array is an authoritative empty result, not a failure");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_filters_related_info_with_virtual_uris() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "unused variable",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///lua/kakehashi-virtual-uri-region-0.lua",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "this is a virtual URI - should be filtered"
                            },
                            {
                                "location": {
                                    "uri": "file:///real/file.lua",
                                    "range": {
                                        "start": { "line": 10, "character": 0 },
                                        "end": { "line": 10, "character": 5 }
                                    }
                                },
                                "message": "this is a real file URI - should be kept"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(3, 0), "unused")
                .unwrap();

        assert_eq!(diagnostics.len(), 1);
        let related = diagnostics[0].related_information.as_ref().unwrap();

        // Only the real file URI entry should remain
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].location.uri.as_str(), "file:///real/file.lua");
        // The range for real file entry is NOT transformed (different from host URI)
        assert_eq!(related[0].location.range.start.line, 10); // unchanged
    }

    #[test]
    fn diagnostic_response_filters_all_related_info_with_virtual_uris() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "message": "unused variable",
                        "relatedInformation": [
                            {
                                "location": {
                                    "uri": "file:///lua/kakehashi-virtual-uri-region-0.lua",
                                    "range": {
                                        "start": { "line": 5, "character": 0 },
                                        "end": { "line": 5, "character": 5 }
                                    }
                                },
                                "message": "first virtual URI"
                            },
                            {
                                "location": {
                                    "uri": "file:///python/kakehashi-virtual-uri-region-1.py",
                                    "range": {
                                        "start": { "line": 10, "character": 0 },
                                        "end": { "line": 10, "character": 5 }
                                    }
                                },
                                "message": "second virtual URI"
                            }
                        ]
                    }
                ]
            }
        });

        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(3, 0), "unused")
                .unwrap();

        assert_eq!(diagnostics.len(), 1);
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert!(related.is_empty());
    }

    #[test]
    fn diagnostic_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_diagnostic_request(&virtual_uri, RequestId::new(42), None);

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn diagnostic_request_has_correct_method_and_structure() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_diagnostic_request(&virtual_uri, RequestId::new(123), None);

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/diagnostic");
        // Diagnostic request has no position parameter (whole-document operation)
        assert!(
            json["params"].get("position").is_none(),
            "Diagnostic request should not have position parameter"
        );
        // Without previous_result_id, the field should be null (typed params serialize None as null)
        assert!(
            json["params"]["previousResultId"].is_null(),
            "Diagnostic request without previous_result_id should have null previousResultId"
        );
    }

    #[test]
    fn diagnostic_request_includes_previous_result_id_when_provided() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_diagnostic_request(&virtual_uri, RequestId::new(123), Some("prev-result-123"));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["previousResultId"], "prev-result-123");
    }

    #[test]
    fn diagnostic_response_near_max_line_saturates() {
        // u32::MAX because lsp_types::Position.line is u32
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": [
                    {
                        "range": {
                            "start": { "line": u32::MAX - 10, "character": 0 },
                            "end": { "line": u32::MAX - 5, "character": 10 }
                        },
                        "message": "diagnostic at very large line number"
                    }
                ]
            }
        });

        // This should not panic due to overflow
        let diagnostics =
            transform_diagnostic_response_to_host(response, &RegionOffset::new(100, 0), "unused")
                .unwrap();

        // Values should saturate at u32::MAX
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, u32::MAX);
        assert_eq!(diagnostics[0].range.end.line, u32::MAX);
    }
}
