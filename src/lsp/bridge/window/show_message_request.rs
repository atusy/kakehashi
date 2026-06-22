//! `window/showMessageRequest` server-request handler.
//!
//! Inbound (downstream → bridge → editor → downstream). A downstream asks the
//! client to show a message with a set of action items and wait for the user's
//! choice; the bridge forwards it to the editor and relays the selected
//! [`MessageActionItem`] (or `null`) back to the downstream.
//!
//! Unlike the ack-immediately handlers, this **defers** its response: the editor
//! may pend on user interaction for an unbounded time. So the handler returns at
//! once after handing an [`UpstreamRequest`] to the forwarding loop, and a
//! spawned task relays the editor's answer back via `response_tx` — the read
//! loop and the global forwarding loop are never blocked, and no bridge-imposed
//! timeout truncates a legitimately slow user. If the editor errors, or the
//! downstream connection dies before an answer arrives (the reply channel
//! drops), the downstream receives `null` (no selection) per the protocol.
//!
//! [`MessageActionItem`]: tower_lsp_server::ls_types::MessageActionItem
//! [`UpstreamRequest`]: crate::lsp::bridge::actor::UpstreamRequest

use log::{debug, warn};
use serde::Deserialize;
use tokio::sync::oneshot;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::ShowMessageRequestParams;

use crate::lsp::bridge::actor::{
    ForwardedRequestCancel, ServerRequestDeps, UpstreamRequest, send_server_response,
};

const METHOD: &str = "window/showMessageRequest";

/// Handle a `window/showMessageRequest` request, relaying the editor's selected
/// action back to the downstream asynchronously.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    id: jsonrpc::Id,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let response_tx = deps.response_tx.clone();
    let server_prefix_owned = server_prefix.to_string();

    let params = match ShowMessageRequestParams::deserialize(&message["params"]) {
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

    // Register the in-flight request BEFORE enqueueing it, so a downstream
    // `$/cancelRequest` racing in right after can't miss it (#404). The reader
    // fires this token on cancel / connection death; the forwarding loop awaits
    // it and, if fired, sends a correlated `$/cancelRequest` to the editor.
    let connection_id = deps.progress_connection_id;
    let token = deps
        .inbound_request_registry
        .register(connection_id, id.clone());
    let cancel = ForwardedRequestCancel {
        connection_id,
        request_id: id.clone(),
        token,
    };

    let (reply_tx, reply_rx) = oneshot::channel();

    // Spawn the responder first; it owns `reply_rx`. If the request below fails
    // to enqueue (forwarding loop gone), `reply_tx` drops and `reply_rx` resolves
    // to `Err`, which maps to `None` (no selection) — so the downstream always
    // gets a response without a special-cased "loop gone" reply path.
    let response_id = id.clone();
    tokio::spawn(async move {
        let action = reply_rx.await.unwrap_or(None);
        let result = serde_json::to_value(action).unwrap_or(serde_json::Value::Null);
        let response = jsonrpc::Response::from_ok(response_id, result);
        send_server_response(&response_tx, response, &server_prefix_owned, METHOD).await;
    });

    if deps
        .upstream_request_tx
        .send(UpstreamRequest::ShowMessageRequest {
            typ: params.typ,
            message: params.message,
            actions: params.actions,
            reply: reply_tx,
            cancel,
        })
        .is_err()
    {
        // Forwarding loop gone (shutdown): drop the registry entry we just made.
        // The responder still answers `null` via the dropped `reply_tx`.
        deps.inbound_request_registry.unregister(connection_id, &id);
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Forwarding loop gone; answering {} with null",
            server_prefix, METHOD
        );
    }
}
