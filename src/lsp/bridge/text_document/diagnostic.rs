//! Diagnostic request handling for bridge connections.
//!
//! This module provides pull diagnostic request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! Like document symbol, diagnostic requests operate on the entire document -
//! they don't take a position parameter.
//!
//! Implements ADR-0020 Phase 1: Pull-first diagnostic forwarding.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;
use std::time::Duration;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::Diagnostic;
use url::Url;

use super::super::pool::{INIT_TIMEOUT_SECS, LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri};

impl LanguageServerPool {
    /// Send a diagnostic request and wait for the response.
    ///
    /// # Wait-for-Ready Behavior
    ///
    /// Unlike other request types that fail fast when a server is initializing,
    /// diagnostic requests wait for the server to become Ready. This provides
    /// better UX — users see diagnostics appear once the server is ready rather
    /// than seeing empty results.
    ///
    /// After the wait-for-ready and capability check, delegates to
    /// [`execute_bridge_request`](Self::execute_bridge_request) for the standard
    /// lifecycle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_diagnostic_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        previous_result_id: Option<&str>,
    ) -> io::Result<Vec<Diagnostic>> {
        // Pre-work: wait for server to become Ready (unlike other handlers that fail fast).
        let handle = self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
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
            server_name,
            host_uri,
            injection_language,
            region_id,
            region_start_line,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_diagnostic_request(virtual_uri, request_id, previous_result_id)
            },
            |response, ctx| {
                transform_diagnostic_response_to_host(
                    response,
                    ctx.region_start_line,
                    host_uri.as_str(),
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC diagnostic request for a downstream language server.
///
/// Like DocumentSymbolParams, DocumentDiagnosticParams operates on the entire document.
/// The request may include an optional previousResultId for incremental updates.
///
/// # Arguments
/// * `virtual_uri` - The pre-built virtual document URI
/// * `request_id` - The JSON-RPC request ID
/// * `previous_result_id` - Optional previous result ID for incremental updates
fn build_diagnostic_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
    previous_result_id: Option<&str>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "textDocument": {
            "uri": virtual_uri.to_uri_string()
        }
    });

    if let Some(prev_id) = previous_result_id {
        params["previousResultId"] = serde_json::json!(prev_id);
    }

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": "textDocument/diagnostic",
        "params": params
    })
}

/// Parse a JSON-RPC diagnostic response and transform coordinates to host document space.
///
/// Instead of returning a modified JSON envelope, this deserializes the response
/// into `Vec<Diagnostic>` with coordinates already transformed.
///
/// Returns empty `Vec` for: null results, unchanged reports, missing items, and
/// deserialization failures.
///
/// # Arguments
/// * `response` - Raw JSON-RPC response envelope (`{"result": {"kind":"full","items":[...]}}`)
/// * `region_start_line` - Line offset to add to diagnostic ranges
/// * `host_uri` - The host document URI; only related info matching this URI gets transformed
fn transform_diagnostic_response_to_host(
    mut response: serde_json::Value,
    region_start_line: u32,
    host_uri: &str,
) -> Vec<Diagnostic> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/diagnostic: {}", error);
    }
    let Some(mut result) = response.get_mut("result").map(serde_json::Value::take) else {
        return Vec::new();
    };
    if result.is_null() {
        return Vec::new();
    }

    // Check report kind
    match result.get("kind").and_then(|k| k.as_str()) {
        Some("unchanged") => return Vec::new(),
        Some("full") | None => {}
        Some(other) => {
            log::warn!(
                target: "kakehashi::bridge",
                "Unknown diagnostic report kind: {}",
                other
            );
            return Vec::new();
        }
    }

    // Deserialize items, taking ownership to avoid clones
    let Some(items) = result.get_mut("items").map(serde_json::Value::take) else {
        return Vec::new();
    };
    let Ok(mut diagnostics) = serde_json::from_value::<Vec<Diagnostic>>(items) else {
        return Vec::new();
    };

    // Transform coordinates on typed structs
    for diag in &mut diagnostics {
        transform_diagnostic(diag, region_start_line, host_uri);
    }

    diagnostics
}

/// Transform a single typed Diagnostic by adding region_start_line to its range.
///
/// Also transforms relatedInformation locations if present, filtering out entries
/// that reference virtual URIs (which clients cannot resolve).
fn transform_diagnostic(diag: &mut Diagnostic, region_start_line: u32, host_uri: &str) {
    // Transform main range
    diag.range.start.line = diag.range.start.line.saturating_add(region_start_line);
    diag.range.end.line = diag.range.end.line.saturating_add(region_start_line);

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
                info.location.range.start.line = info
                    .location
                    .range
                    .start
                    .line
                    .saturating_add(region_start_line);
                info.location.range.end.line = info
                    .location
                    .range
                    .end
                    .line
                    .saturating_add(region_start_line);
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
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

        let diagnostics = transform_diagnostic_response_to_host(response, 5, "unused");

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

        let diagnostics = transform_diagnostic_response_to_host(response, 3, "file:///test.md");

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

        let diagnostics = transform_diagnostic_response_to_host(response, 3, "file:///test.md");

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

        let diagnostics = transform_diagnostic_response_to_host(response, 5, "unused");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_null_result_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let diagnostics = transform_diagnostic_response_to_host(response, 5, "unused");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_missing_result_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "error": { "code": -32603, "message": "internal error" } });

        let diagnostics = transform_diagnostic_response_to_host(response, 5, "unused");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn diagnostic_response_malformed_items_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "kind": "full",
                "items": "not_an_array"
            }
        });

        let diagnostics = transform_diagnostic_response_to_host(response, 5, "unused");
        assert!(diagnostics.is_empty());
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

        let diagnostics = transform_diagnostic_response_to_host(response, 5, "unused");
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

        let diagnostics = transform_diagnostic_response_to_host(response, 3, "unused");

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

        let diagnostics = transform_diagnostic_response_to_host(response, 3, "unused");

        assert_eq!(diagnostics.len(), 1);
        let related = diagnostics[0].related_information.as_ref().unwrap();
        assert!(related.is_empty());
    }

    fn test_host_uri() -> tower_lsp_server::ls_types::Uri {
        let url = url::Url::parse("file:///project/doc.md").unwrap();
        crate::lsp::lsp_impl::url_to_uri(&url).expect("test URL should convert to URI")
    }

    #[test]
    fn diagnostic_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_diagnostic_request(&virtual_uri, RequestId::new(42), None);

        let uri_str = request["params"]["textDocument"]["uri"].as_str().unwrap();
        let url = url::Url::parse(uri_str).expect("URI should be parseable");
        let filename = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("");
        assert!(
            filename.starts_with("kakehashi-virtual-uri-") && filename.ends_with(".lua"),
            "Request should use virtual URI with .lua extension: {}",
            uri_str
        );
    }

    #[test]
    fn diagnostic_request_has_correct_method_and_structure() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_diagnostic_request(&virtual_uri, RequestId::new(123), None);

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 123);
        assert_eq!(request["method"], "textDocument/diagnostic");
        // Diagnostic request has no position parameter (whole-document operation)
        assert!(
            request["params"].get("position").is_none(),
            "Diagnostic request should not have position parameter"
        );
        // Without previous_result_id, there should be no previousResultId field
        assert!(
            request["params"].get("previousResultId").is_none(),
            "Diagnostic request without previous_result_id should not have previousResultId"
        );
    }

    #[test]
    fn diagnostic_request_includes_previous_result_id_when_provided() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_diagnostic_request(&virtual_uri, RequestId::new(123), Some("prev-result-123"));

        assert_eq!(request["params"]["previousResultId"], "prev-result-123");
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
        let diagnostics = transform_diagnostic_response_to_host(response, 100, "unused");

        // Values should saturate at u32::MAX
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].range.start.line, u32::MAX);
        assert_eq!(diagnostics[0].range.end.line, u32::MAX);
    }
}
