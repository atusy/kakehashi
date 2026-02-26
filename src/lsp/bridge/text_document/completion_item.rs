//! completionItem/resolve request handling for bridge connections.
//!
//! Routes a resolve request to the single downstream server that produced
//! the completion item, identified by the Kakehashi envelope stored in
//! `CompletionItem.data` during the original completion fan-out.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue messages via the
//! channel-based writer task, ensuring FIFO ordering with other messages.
//!
//! # No didOpen
//!
//! Unlike other bridge handlers, this module does NOT call `ensure_document_opened`.
//! The virtual document was already opened during the completion request that
//! produced this item, so re-sending didOpen is unnecessary.
//!
//! **Edge case:** If the downstream server restarts (or the connection is
//! re-created via `get_or_create_connection`) between the original completion
//! and this resolve, the virtual document will not exist on the new server
//! instance. The resolve will fail and the fallback path returns the
//! unresolved item with its envelope intact — graceful degradation.

use std::sync::Arc;

use log::warn;
use tower_lsp_server::ls_types::CompletionItem;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{JsonRpcRequest, RegionOffset, RequestId};
use super::completion::{
    EnvelopeContext, KakehashiEnvelope, envelope_item_data, strip_envelope,
    transform_completion_item,
};
use crate::config::settings::WorkspaceSettings;
use crate::config::{resolve_language_server_with_wildcard, settings::BridgeServerConfig};
use crate::lsp::bridge::actor::RouterCleanupGuard;

impl LanguageServerPool {
    /// Route a `completionItem/resolve` request to the origin downstream server.
    ///
    /// Strips the Kakehashi envelope to identify the origin server, looks up
    /// the server config from `settings`, and delegates to
    /// `send_completion_resolve_request`. If any routing step fails (no envelope,
    /// server not configured), the item is returned as-is.
    pub(crate) async fn dispatch_completion_resolve(
        &self,
        mut item: CompletionItem,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> CompletionItem {
        // Extract envelope — if absent, this item wasn't produced by Kakehashi
        let Some(envelope) = strip_envelope(&mut item) else {
            return item;
        };

        // Look up the server config for the origin server
        let config = settings
            .language_servers
            .as_ref()
            .and_then(|servers| resolve_language_server_with_wildcard(servers, &envelope.origin));

        let Some(config) = config else {
            // Server no longer configured — re-envelope and return as-is
            re_envelope_item(&mut item, &envelope);
            return item;
        };

        self.send_completion_resolve_request(&config, item, envelope, upstream_id)
            .await
    }

    /// Send a `completionItem/resolve` request to the downstream server that
    /// produced the item, re-enveloping the resolved item for return to the client.
    ///
    /// Always returns the item. All failure modes (connection error, timeout, parse
    /// failure, missing capability) return the original item with its envelope
    /// restored so the client can still use the basic completion item.
    async fn send_completion_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut item: CompletionItem,
        envelope: KakehashiEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> CompletionItem {
        let server_name = &envelope.origin;
        let handle = match self
            .get_or_create_connection(server_name, server_config)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve: failed to connect to {}: {}",
                    server_name, e
                );
                re_envelope_item(&mut item, &envelope);
                return item;
            }
        };

        if !handle.has_capability("completionItem/resolve") {
            re_envelope_item(&mut item, &envelope);
            return item;
        }

        // Register in the upstream request registry FIRST for cancel lookup.
        if let Some(ref id) = upstream_id {
            self.register_upstream_request(id.clone(), server_name);
        }

        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(
                        target: "kakehashi::bridge",
                        "completionItem/resolve: failed to register request for {}: {}",
                        server_name, e
                    );
                    if let Some(ref id) = upstream_id {
                        self.unregister_upstream_request(id, server_name);
                    }
                    re_envelope_item(&mut item, &envelope);
                    return item;
                }
            };

        let request = build_completion_resolve_request(&item, request_id);

        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        if let Err(e) = handle.send_request(request, request_id) {
            warn!(
                target: "kakehashi::bridge",
                "completionItem/resolve: failed to send request for {}: {}",
                server_name, e
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, server_name);
            }
            re_envelope_item(&mut item, &envelope);
            return item;
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        // Unregister from the upstream request registry regardless of result
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, server_name);
        }

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve failed for server {}: {}",
                    server_name, e
                );
                re_envelope_item(&mut item, &envelope);
                return item;
            }
        };

        match parse_completion_resolve_response(response) {
            Some(mut resolved) => {
                let offset = RegionOffset::from(&envelope.offset);
                transform_completion_item(&mut resolved, &offset);
                re_envelope_item(&mut resolved, &envelope);
                resolved
            }
            None => {
                re_envelope_item(&mut item, &envelope);
                item
            }
        }
    }
}

/// Build a JSON-RPC `completionItem/resolve` request.
///
/// The `completionItem/resolve` method is unique: its params is the
/// `CompletionItem` itself (not a wrapper struct), per the LSP spec.
fn build_completion_resolve_request(
    item: &CompletionItem,
    request_id: RequestId,
) -> JsonRpcRequest<&CompletionItem> {
    JsonRpcRequest::new(request_id.as_i64(), "completionItem/resolve", item)
}

/// Parse a JSON-RPC resolve response into a `CompletionItem`.
///
/// Returns `None` for null results, missing results, and deserialization failures.
fn parse_completion_resolve_response(mut response: serde_json::Value) -> Option<CompletionItem> {
    if let Some(error) = response.get("error") {
        warn!(
            target: "kakehashi::bridge",
            "Downstream server returned error for completionItem/resolve: {}",
            error
        );
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    serde_json::from_value(result).ok()
}

/// Restore the envelope into a resolved item's `data` field.
///
/// The resolved item may have its own `data` (from the downstream's resolve
/// response). We wrap it again so that future resolves can still be routed.
fn re_envelope_item(item: &mut CompletionItem, envelope: &KakehashiEnvelope) {
    let ctx = EnvelopeContext {
        server_name: &envelope.origin,
        offset: &RegionOffset::from(&envelope.offset),
    };
    envelope_item_data(item, &ctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower_lsp_server::ls_types::CompletionItem;

    use crate::lsp::bridge::text_document::completion::{EnvelopeOffset, extract_envelope};

    fn test_envelope() -> KakehashiEnvelope {
        KakehashiEnvelope {
            origin: "lua-ls".to_string(),
            inner: Some(json!({"resolve_id": 99})),
            offset: EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: None,
            },
        }
    }

    // ==========================================================================
    // build_completion_resolve_request tests
    // ==========================================================================

    #[test]
    fn resolve_request_has_correct_structure() {
        let item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"resolve_id": 99})),
            ..Default::default()
        };
        let request = build_completion_resolve_request(&item, RequestId::new(7));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7i64);
        assert_eq!(json["method"], "completionItem/resolve");
        assert_eq!(json["params"]["label"], "print");
        assert_eq!(json["params"]["data"]["resolve_id"], 99);
    }

    // ==========================================================================
    // parse_completion_resolve_response tests
    // ==========================================================================

    #[test]
    fn parse_response_happy_path() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {
                "label": "print",
                "documentation": "Prints to stdout"
            }
        });
        let item = parse_completion_resolve_response(response).expect("should parse");
        assert_eq!(item.label, "print");
    }

    #[test]
    fn parse_response_null_result_returns_none() {
        let response = json!({"jsonrpc": "2.0", "id": 7, "result": null});
        assert!(parse_completion_resolve_response(response).is_none());
    }

    #[test]
    fn parse_response_missing_result_returns_none() {
        let response = json!({"jsonrpc": "2.0", "id": 7});
        assert!(parse_completion_resolve_response(response).is_none());
    }

    #[test]
    fn parse_response_error_field_returns_none() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "error": {"code": -32600, "message": "Invalid Request"}
        });
        assert!(parse_completion_resolve_response(response).is_none());
    }

    // ==========================================================================
    // re_envelope_item tests
    // ==========================================================================

    #[test]
    fn re_envelope_restores_routing_envelope() {
        let envelope = test_envelope();
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"new_data": true})),
            ..Default::default()
        };
        re_envelope_item(&mut item, &envelope);

        let extracted = extract_envelope(&item).expect("should have envelope");
        assert_eq!(extracted.origin, "lua-ls");
        // The resolved item's own data is preserved in inner
        assert_eq!(extracted.inner, Some(json!({"new_data": true})));
        // After round-trip (EnvelopeOffset → RegionOffset → EnvelopeOffset),
        // line_column_offsets is always populated.
        assert_eq!(
            extracted.offset,
            EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: Some(vec![0])
            }
        );
    }

    #[test]
    fn re_envelope_preserves_none_data() {
        let envelope = test_envelope();
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: None,
            ..Default::default()
        };
        re_envelope_item(&mut item, &envelope);

        let extracted = extract_envelope(&item).expect("should have envelope");
        assert_eq!(extracted.inner, None);
    }

    // ==========================================================================
    // dispatch_completion_resolve integration tests
    // ==========================================================================

    /// Helper to create a completion item with a Kakehashi envelope.
    fn enveloped_item(server: &str) -> CompletionItem {
        let envelope = KakehashiEnvelope {
            origin: server.to_string(),
            inner: Some(json!({"resolve_id": 42})),
            offset: EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: None,
            },
        };
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"resolve_id": 42})),
            ..Default::default()
        };
        re_envelope_item(&mut item, &envelope);
        item
    }

    /// dispatch returns item unchanged when it has no envelope.
    #[tokio::test]
    async fn dispatch_returns_non_envelope_item_unchanged() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default();
        let item = CompletionItem {
            label: "plain".to_string(),
            data: Some(json!({"custom": true})),
            ..Default::default()
        };

        let result = pool
            .dispatch_completion_resolve(item.clone(), &settings, None)
            .await;
        assert_eq!(result.label, "plain");
        assert_eq!(result.data, Some(json!({"custom": true})));
    }

    /// dispatch re-envelopes and returns item when origin server is not in settings.
    #[tokio::test]
    async fn dispatch_re_envelopes_when_server_not_configured() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default(); // no language_servers configured

        let item = enveloped_item("nonexistent-ls");
        let result = pool
            .dispatch_completion_resolve(item, &settings, None)
            .await;

        // Should be re-enveloped (routing info preserved for future attempts)
        let envelope = extract_envelope(&result).expect("should have envelope");
        assert_eq!(envelope.origin, "nonexistent-ls");
    }

    // ==========================================================================
    // strip_envelope round-trip (used in lsp_impl layer)
    // ==========================================================================

    #[test]
    fn strip_then_re_envelope_round_trips() {
        // Simulate the lsp_impl → bridge flow:
        // 1. lsp_impl receives an item with a Kakehashi envelope (from completion fan-out)
        // 2. lsp_impl strips the envelope to get the downstream's original data
        // 3. bridge resolves the item and re-envelopes the result

        // Start with an item that already carries the Kakehashi envelope
        // (as produced by the completion fan-out)
        let envelope = test_envelope(); // inner: Some({"resolve_id": 99})
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: None, // will be set by re_envelope_item below
            ..Default::default()
        };
        // Simulate what completion fan-out does: the item had data=None, now enveloped
        re_envelope_item(&mut item, &envelope);
        // item.data is now the envelope with inner=None (item originally had no data)

        // lsp_impl strips before forwarding to bridge (reveals downstream data = None here)
        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.origin, "lua-ls");
        assert_eq!(item.data, None); // original downstream data restored

        // Simulate bridge receiving a resolved item with new documentation data
        item.data = Some(json!({"resolved": true}));

        // bridge re-envelopes after resolve
        re_envelope_item(&mut item, &stripped);
        let final_envelope = extract_envelope(&item).expect("should have envelope");
        assert_eq!(final_envelope.origin, "lua-ls");
        assert_eq!(final_envelope.inner, Some(json!({"resolved": true})));
        // After round-trip, line_column_offsets is always populated.
        assert_eq!(
            final_envelope.offset,
            EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: Some(vec![0])
            }
        );
    }
}
