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

use std::io;
use std::sync::Arc;

use log::warn;
use serde_json::json;
use tower_lsp_server::ls_types::CompletionItem;

use super::super::pool::LanguageServerPool;
use super::super::protocol::{RegionOffset, RequestId};
use super::completion::{
    EnvelopeContext, KakehashiEnvelope, envelope_item_data, strip_envelope,
    transform_completion_item,
};
use crate::config::settings::BridgeServerConfig;
use crate::lsp::bridge::actor::ResponseRouter;

impl LanguageServerPool {
    /// Send a `completionItem/resolve` request to the downstream server that
    /// produced the item, re-enveloping the resolved item for return to the client.
    ///
    /// If the downstream doesn't support resolve, returns the item with its
    /// envelope restored (graceful no-op).
    pub(crate) async fn send_completion_resolve_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        mut item: CompletionItem,
        envelope: KakehashiEnvelope,
    ) -> io::Result<CompletionItem> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;

        if !handle.has_capability("completionItem/resolve") {
            re_envelope_item(&mut item, &envelope);
            return Ok(item);
        }

        let (request_id, response_rx) = handle.register_request_with_upstream(None)?;

        let request = build_completion_resolve_request(&item, request_id);

        let mut router_guard = RouterCleanupGuard {
            router: Arc::clone(handle.router()),
            request_id: Some(request_id),
        };

        if let Err(e) = handle.send_request(request, request_id) {
            re_envelope_item(&mut item, &envelope);
            return Err(e.into());
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.request_id.take();

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve failed for server {}: {}",
                    server_name, e
                );
                re_envelope_item(&mut item, &envelope);
                return Ok(item);
            }
        };

        match parse_completion_resolve_response(response) {
            Some(mut resolved) => {
                let offset = RegionOffset::from(&envelope.offset);
                transform_completion_item(&mut resolved, offset);
                re_envelope_item(&mut resolved, &envelope);
                Ok(resolved)
            }
            None => {
                re_envelope_item(&mut item, &envelope);
                Ok(item)
            }
        }
    }
}

/// Build a JSON-RPC `completionItem/resolve` request.
fn build_completion_resolve_request(
    item: &CompletionItem,
    request_id: RequestId,
) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": "completionItem/resolve",
        "params": item,
    })
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
        offset: RegionOffset::from(&envelope.offset),
    };
    envelope_item_data(item, &ctx);
}

/// RAII guard that removes a pending ResponseRouter entry on drop.
///
/// Mirrors the guard in `execute.rs` for the same abort-safety guarantee:
/// if the resolve task is dropped before `wait_for_response` completes,
/// the router entry is cleaned up to prevent a memory leak.
struct RouterCleanupGuard {
    router: Arc<ResponseRouter>,
    request_id: Option<RequestId>,
}

impl Drop for RouterCleanupGuard {
    fn drop(&mut self) {
        if let Some(id) = self.request_id {
            self.router.remove(id);
        }
    }
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
            offset: EnvelopeOffset { line: 5, column: 0 },
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

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 7i64);
        assert_eq!(request["method"], "completionItem/resolve");
        assert_eq!(request["params"]["label"], "print");
        assert_eq!(request["params"]["data"]["resolve_id"], 99);
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
        assert_eq!(extracted.offset, EnvelopeOffset { line: 5, column: 0 });
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
        assert_eq!(final_envelope.offset, EnvelopeOffset { line: 5, column: 0 });
    }
}
