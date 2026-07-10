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
    // We ack the downstream immediately with Ok(null): this decouples the
    // downstream from editor latency. The editor is NOT asked to create the
    // token here — announcement is deferred until the token's first titled
    // `begin` (see `ProgressRegistry` lazy-announcement docs), because some
    // downstreams declare a token per analysis pass and the resulting
    // create-request storm queues ahead of real responses on the shared
    // stdout sink. The `$/progress` forwarder emits the create (an editor
    // round-trip awaited by the forwarding loop) immediately before the
    // begin when a token proves renderable.
    // Deserialize the token from the borrowed value (indexing yields
    // `Null` for a missing key, which fails to parse) — no clone.
    match NumberOrString::deserialize(&message["params"]["token"]) {
        Ok(downstream_token) => {
            // A downstream that re-`create`s a token it already declared
            // (without an intervening End) gets a fresh upstream token;
            // `register` evicts the prior one and returns it as `stale` so
            // we can tell the forwarding loop to forget its admission too —
            // otherwise it would leak in `created_tokens`. Tokens that were
            // never announced have no admission to forget.
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
            if let Some(stale) = stale_upstream_token
                && stale.announced
            {
                let _ = deps
                    .upstream_tx
                    .send(UpstreamNotification::ForgetWorkDoneProgress(vec![
                        stale.token,
                    ]));
            }
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
