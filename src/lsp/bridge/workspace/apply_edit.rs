//! `workspace/applyEdit` server-request handler (#568).
//!
//! Inbound (downstream → bridge → editor → downstream). A downstream asks the
//! client to apply a [`WorkspaceEdit`]; the bridge forwards it to the editor
//! and relays the editor's full [`ApplyWorkspaceEditResponse`] (`applied`,
//! `failureReason`, `failedChange`) back — it is a **request**, not a
//! notification: the downstream acts on the outcome.
//!
//! **Edit translation happens downstream of this handler:** the forwarding
//! loop rewrites virtual-document URIs + ranges back to the host document
//! (see `ApplyEditTranslator` in `lsp_impl::apply_edit_translation`) before
//! sending the edit to the editor, and answers untranslatable edits
//! `applied: false` locally. This handler stays transport-only, exactly like
//! [`show_document`](crate::lsp::bridge::window::show_document).
//!
//! Like the other deferred handlers this **defers** its response on a spawned
//! task so neither the read loop nor the global forwarding loop blocks. If the
//! editor errors, or the downstream dies before an answer (the reply channel
//! drops), the downstream receives `applied: false` with a `failureReason`.
//!
//! [`WorkspaceEdit`]: tower_lsp_server::ls_types::WorkspaceEdit
//! [`ApplyWorkspaceEditResponse`]: tower_lsp_server::ls_types::ApplyWorkspaceEditResponse

use log::{debug, warn};
use serde::Deserialize;
use tokio::sync::oneshot;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::{ApplyWorkspaceEditParams, ApplyWorkspaceEditResponse};

use crate::lsp::bridge::actor::{
    ForwardedRequestCancel, ServerRequestDeps, UpstreamRequest, send_server_response,
};

const METHOD: &str = "workspace/applyEdit";

/// Handle a `workspace/applyEdit` request, relaying the editor's
/// `ApplyWorkspaceEditResponse` back to the downstream asynchronously.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    id: jsonrpc::Id,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let response_tx = deps.response_tx.clone();
    let server_prefix_owned = server_prefix.to_string();

    let params = match ApplyWorkspaceEditParams::deserialize(&message["params"]) {
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
    // to `Err`, which maps to `applied: false` — so the downstream always gets a
    // response without a special-cased "loop gone" reply path.
    let response_id = id.clone();
    tokio::spawn(async move {
        let result: ApplyWorkspaceEditResponse = reply_rx.await.unwrap_or_else(|_| {
            ApplyWorkspaceEditResponse {
                applied: false,
                failure_reason: Some(
                    "kakehashi: the editor did not answer workspace/applyEdit".to_string(),
                ),
                failed_change: None,
            }
        });
        // The typed struct serializes infallibly in practice; the fallback keeps
        // the no-panic guarantee with the same protocol default.
        let result = serde_json::to_value(result)
            .unwrap_or_else(|_| serde_json::json!({ "applied": false }));
        let response = jsonrpc::Response::from_ok(response_id, result);
        send_server_response(&response_tx, response, &server_prefix_owned, METHOD).await;
    });

    if deps
        .upstream_request_tx
        .send(UpstreamRequest::ApplyEdit {
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
            "{}Forwarding loop gone; answering {} with applied:false",
            server_prefix, METHOD
        );
    }
}
