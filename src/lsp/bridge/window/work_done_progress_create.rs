//! `window/workDoneProgress/create` server-request handler.
//!
//! Inbound (downstream → bridge → editor). A downstream declares a progress
//! token; the bridge mints a unique *upstream* token, records the mapping in the
//! shared [`ProgressRegistry`], and asks the editor to create the progress. The
//! data half of the feature — translating the downstream's `$/progress` to the
//! upstream token — lives in [`progress`](super::progress).
//!
//! [`ProgressRegistry`]: crate::lsp::bridge::ProgressRegistry

use log::{debug, warn};
use serde::Deserialize;
use tower_lsp_server::jsonrpc;
use tower_lsp_server::ls_types::NumberOrString;

use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

/// Handle a `window/workDoneProgress/create` request, returning the JSON-RPC
/// body the dispatcher wraps in a response.
pub(in crate::lsp::bridge) fn handle(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) -> jsonrpc::Result<serde_json::Value> {
    // A downstream is declaring a progress token. Downstreams pick tokens
    // independently, so two of them can collide; mint a unique upstream
    // token and remember the mapping so `$/progress` and cancel can be
    // translated in both directions (window-work-done-progress bridging).
    //
    // We ack the downstream immediately with Ok(null) rather than relaying
    // the editor's real create response: this decouples the downstream from
    // editor latency and lets progress buffer on the FIFO upstream channel.
    // The bridge only advertises `window.workDoneProgress` downstream when
    // the real editor supports it (see client_capabilities merge), so the
    // editor almost always accepts the create. If it nonetheless rejects
    // or times out, the forwarding loop drops that token's `$/progress`
    // (it admits a token only on a successful create), so the optimistic
    // ack never leaks progress for a token the editor didn't create.
    // Deserialize the token from the borrowed value (indexing yields
    // `Null` for a missing key, which fails to parse) — no clone.
    match NumberOrString::deserialize(&message["params"]["token"]) {
        Ok(downstream_token) => {
            // A downstream that re-`create`s a token it already declared
            // (without an intervening End) gets a fresh upstream token;
            // `register` evicts the prior one and returns it as `stale` so
            // we can tell the forwarding loop to forget its admission too —
            // otherwise it would leak in `created_tokens`.
            let (upstream_token, stale_upstream_token) = deps.progress_registry.register(
                deps.progress_connection_id,
                downstream_token,
                deps.response_tx.clone(),
            );
            debug!(
                target: "kakehashi::bridge::reader",
                "{}Bridging window/workDoneProgress/create as upstream token {:?}",
                server_prefix, upstream_token
            );
            if let Some(stale) = stale_upstream_token {
                let _ = deps
                    .upstream_tx
                    .send(UpstreamNotification::ForgetWorkDoneProgress(vec![stale]));
            }
            let _ = deps
                .upstream_tx
                .send(UpstreamNotification::CreateWorkDoneProgress {
                    token: upstream_token,
                });
            Ok(serde_json::Value::Null)
        }
        Err(_) => {
            warn!(
                target: "kakehashi::bridge::reader",
                "{}window/workDoneProgress/create missing/invalid token",
                server_prefix
            );
            Err(jsonrpc::Error::invalid_params(
                "window/workDoneProgress/create requires a token",
            ))
        }
    }
}
