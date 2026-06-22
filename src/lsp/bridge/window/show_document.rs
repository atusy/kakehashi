//! `window/showDocument` server-request handler.
//!
//! Inbound (downstream → bridge → editor → downstream). A downstream asks the
//! client to show a resource (by URI) in the UI; the bridge forwards it to the
//! editor and relays the editor's `success` flag back as a [`ShowDocumentResult`].
//!
//! **URI translation happens downstream of this handler:** the `uri` is
//! forwarded verbatim to the global forwarding loop, which rewrites a
//! *virtual-document* URI back to its host document (and translates the
//! selection) before sending it to the editor (#403). This handler stays
//! transport-only.
//!
//! Like [`show_message_request`](super::show_message_request) this **defers** its
//! response on a spawned task so neither the read loop nor the global forwarding
//! loop blocks. If the editor errors, or the downstream dies before an answer
//! (the reply channel drops), the downstream receives `success: false`.
//!
//! [`ShowDocumentResult`]: tower_lsp_server::ls_types::ShowDocumentResult

use log::{debug, warn};
use serde::Deserialize;
use tokio::sync::oneshot;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::ShowDocumentParams;

use crate::lsp::bridge::actor::{
    ForwardedRequestCancel, ServerRequestDeps, UpstreamRequest, send_server_response,
};

const METHOD: &str = "window/showDocument";

/// Handle a `window/showDocument` request, relaying the editor's success flag
/// back to the downstream asynchronously.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    id: jsonrpc::Id,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let response_tx = deps.response_tx.clone();
    let server_prefix_owned = server_prefix.to_string();

    let params = match ShowDocumentParams::deserialize(&message["params"]) {
        Ok(params) => params,
        Err(e) => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}Rejecting {} with InvalidParams (unparseable params): {}",
                server_prefix, METHOD, e
            );
            tokio::spawn(async move {
                let response = jsonrpc::Response::from_error(
                    id,
                    jsonrpc::Error::invalid_params(format!("Invalid params: {e}")),
                );
                send_server_response(&response_tx, response, &server_prefix_owned, METHOD).await;
            });
            return;
        }
    };

    // Register the in-flight request BEFORE enqueueing it so a racing downstream
    // `$/cancelRequest` can't miss it (#404). See `show_message_request`.
    let connection_id = deps.progress_connection_id;
    let (token, generation) = deps
        .inbound_request_registry
        .register(connection_id, id.clone());
    let cancel = ForwardedRequestCancel {
        connection_id,
        request_id: id.clone(),
        token,
        generation,
    };

    let (reply_tx, reply_rx) = oneshot::channel();

    // Spawn the responder first; it owns `reply_rx`. If the request below fails
    // to enqueue (forwarding loop gone), `reply_tx` drops and `reply_rx` resolves
    // to `Err`, which maps to `success: false` — so the downstream always gets a
    // response without a special-cased "loop gone" reply path.
    let response_id = id.clone();
    tokio::spawn(async move {
        let success = reply_rx.await.unwrap_or(false);
        // Build the `ShowDocumentResult` shape (`{ "success": bool }`) directly
        // and infallibly — `serde_json::to_value` of the typed struct would force
        // a fallback whose only safe value is this same object anyway.
        let result = serde_json::json!({ "success": success });
        let response = jsonrpc::Response::from_ok(response_id, result);
        send_server_response(&response_tx, response, &server_prefix_owned, METHOD).await;
    });

    if deps
        .upstream_request_tx
        .send(UpstreamRequest::ShowDocument {
            params,
            reply: reply_tx,
            cancel,
        })
        .is_err()
    {
        // Forwarding loop gone (shutdown): drop the registry entry we just made.
        deps.inbound_request_registry
            .unregister(connection_id, &id, generation);
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Forwarding loop gone; answering {} with success:false",
            server_prefix, METHOD
        );
    }
}
