//! `window/showDocument` server-request handler.
//!
//! Inbound (downstream → bridge → editor → downstream). A downstream asks the
//! client to show a resource (by URI) in the UI; the bridge forwards it to the
//! editor and relays the editor's `success` flag back as a [`ShowDocumentResult`].
//!
//! **URI pass-through:** the `uri` is forwarded verbatim, including
//! concatenated-formatting *virtual-document* URIs — the bridge performs no
//! virtual→host translation here. If the editor cannot open such a URI it simply
//! answers `success: false`, which is relayed downstream unchanged. This is a
//! deliberate v1 simplification; mapping virtual URIs back to host documents is
//! out of scope for now.
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

use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamRequest, send_server_response};

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

    let (reply_tx, reply_rx) = oneshot::channel();

    // Spawn the responder first; it owns `reply_rx`. If the request below fails
    // to enqueue (forwarding loop gone), `reply_tx` drops and `reply_rx` resolves
    // to `Err`, which maps to `success: false` — so the downstream always gets a
    // response without a special-cased "loop gone" reply path.
    tokio::spawn(async move {
        let success = reply_rx.await.unwrap_or(false);
        // Build the `ShowDocumentResult` shape (`{ "success": bool }`) directly
        // and infallibly — `serde_json::to_value` of the typed struct would force
        // a fallback whose only safe value is this same object anyway.
        let result = serde_json::json!({ "success": success });
        let response = jsonrpc::Response::from_ok(id, result);
        send_server_response(&response_tx, response, &server_prefix_owned, METHOD).await;
    });

    if deps
        .upstream_request_tx
        .send(UpstreamRequest::ShowDocument {
            params,
            reply: reply_tx,
        })
        .is_err()
    {
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Forwarding loop gone; answering {} with success:false",
            server_prefix, METHOD
        );
    }
}
