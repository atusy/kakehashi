//! `workspace/executeCommand` routing (#568 PR 6).
//!
//! Outbound (editor → bridge → downstream). A `Command` the bridge surfaced in
//! a code action (bare or embedded) is executed by the client via
//! `workspace/executeCommand`, which carries only `command` + `arguments` — no
//! `data` envelope. The CONNECTION to run on is encoded in the command NAME
//! instead (see [`command_routing`](crate::lsp::bridge::protocol)), so this
//! handler decodes that `(server, root)` key, reaches the live connection under
//! it (or rebuilds one), and forwards the request with the downstream's
//! ORIGINAL command name and verbatim arguments (checklist §10).
//!
//! Routing by the decoded key — rather than by re-resolving a root from a host
//! document — is what makes this path document-free
//! (execute-command-routing-token).
//!
//! The server's result is relayed verbatim. Most command-style servers answer
//! executeCommand by sending a `workspace/applyEdit` back (handled inbound by
//! [`apply_edit`](super::apply_edit)) and returning a null result — the two
//! paths compose: execute → server-applyEdit → editor applies → execute-result.
//!
//! Fails soft at every step: a command that doesn't decode as a bridge command,
//! an unspawnable/unresolvable origin, a dead connection, or a server error all
//! yield `None` (a null result to the client) rather than an error dialog.

use std::sync::Arc;

use log::warn;
use serde_json::Value;

use crate::config::settings::WorkspaceSettings;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::decode_command;
use crate::lsp::bridge::pool::{ConnectionHandle, LanguageServerPool, UpstreamId};
use crate::lsp::bridge::protocol::{JsonRpcRequest, response_has_jsonrpc_error};
use tower_lsp_server::ls_types::ExecuteCommandParams;

const METHOD: &str = "workspace/executeCommand";

impl LanguageServerPool {
    /// Route a bridged `workspace/executeCommand` back to the origin downstream
    /// server encoded in the command name. Returns the server's result relayed
    /// verbatim, or `None` on any failure (fail soft).
    pub(crate) async fn dispatch_execute_command(
        &self,
        params: ExecuteCommandParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Value> {
        // Decode into an owned key/command first so `params.arguments` can move
        // into the outgoing request without a partial-borrow conflict.
        let (key, command) = match decode_command(&params.command) {
            Some(route) => (route.key, route.command.to_string()),
            None => {
                // Not an action-encoded command. It may still be a PALETTE
                // command — a name a downstream advertised via
                // `executeCommandProvider` that the client fired without an
                // action context (#628). Route it by the command→origin registry;
                // otherwise it's foreign and ignored.
                return self
                    .dispatch_palette_command(params, settings, upstream_id)
                    .await;
            }
        };
        let origin = key.server().to_string();

        // executeCommand is USER-invoked (they picked the action/palette
        // entry): failing soft is right, failing silently is not. The encoded
        // command outlives config edits, so a server removed/renamed since the
        // action was minted lands here.
        if !crate::config::is_server_spawnable(&settings.language_servers, &origin) {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: origin {origin:?} is not spawnable (removed or \
                 misconfigured since the action was produced); dropping {command:?}"
            );
            return None;
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &origin,
            merge_bridge_server_configs,
        ) else {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: origin {origin:?} has no resolvable config; dropping {command:?}"
            );
            return None;
        };

        // Route to the EXACT connection the token names. The live connection is
        // the common case; a reconnect rebuilds the same `(server, root)` key
        // rather than re-resolving a root from a document, which is what lets
        // this path work without one (execute-command-routing-token).
        let handle = match self.ready_connection_by_key(&key).await {
            Some(handle) => handle,
            None => match self.reconnect_by_key(&key, &config).await {
                Some(handle) => handle,
                None => return None,
            },
        };
        // The command's `arguments` reference documents this connection must
        // already have open. If it was just respawned, its re-open is in flight
        // and asynchronous, so wait for it — the outbound queue is FIFO, and
        // enqueueing the command first would hand the downstream a command for a
        // document it has not opened yet. Bounded, and a no-op when nothing is
        // in flight (the common case).
        self.wait_for_pending_reopen(&key).await;
        if !handle.has_capability(METHOD) {
            // Nearly unreachable: the bridge only mints commands from servers it
            // bridged, and the wait-ready acquisition above rules out the
            // still-Initializing false negative. Reaching it means the server
            // genuinely dropped the capability across a respawn — a log entry
            // beats a silent drop (every other failure branch here warns).
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: {origin:?} does not advertise executeCommandProvider; ignoring {command:?}"
            );
            return None;
        }

        // Forward with the downstream's ORIGINAL command name and its own
        // arguments untouched (they reference the downstream's coordinate
        // system — its virtual or host document — checklist §10).
        let outgoing = ExecuteCommandParams {
            command,
            arguments: params.arguments,
            work_done_progress_params: params.work_done_progress_params,
        };
        self.send_execute_command_on_handle(&handle, outgoing, upstream_id)
            .await
    }

    /// Route a PALETTE-fired command (a raw downstream command name, no action
    /// envelope) to the exact connection that advertised it — recorded in the
    /// [`command_origins`](Self::command_origins) registry at handshake — so it
    /// runs in the same `(server, root)` workspace context (#628). Reuses the
    /// live advertising connection; only if it has since been shut down AND the
    /// key is a plain client-root fallback does it reconnect. Forwards the command
    /// name and arguments verbatim; fails soft (foreign command, unspawnable or
    /// unreachable origin) like every other branch.
    async fn dispatch_palette_command(
        &self,
        params: ExecuteCommandParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Value> {
        let Some(key) = self.command_origins().route(&params.command) else {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: {:?} is neither a bridged nor a registered command; ignoring",
                params.command
            );
            return None;
        };
        let origin = key.server();
        let handle = match self.ready_connection_by_key(&key).await {
            // The connection that advertised the command is still Ready — route
            // there, preserving its workspace root/context.
            Some(handle) => handle,
            // Not Ready or gone. Reconnect ONLY for a plain client-root fallback:
            // `get_or_create_connection(.., None)` resolves back to that exact
            // ClientFallback key, so the command runs in the same context. A
            // SHARED key (`preferSharedInstance`) does NOT round-trip through
            // `None` — `resolve_acquire` returns the client-fallback key for a
            // marker-less acquisition, so reconnecting with `None` would spawn a
            // client-root process instead of the shared instance and run the
            // command in the wrong workspace. A MARKER-rooted key has the same
            // problem here; the encoded-command path solves it with
            // `reconnect_by_key` (which rebuilds the workspace from the root the
            // token carries), but a palette command's registry entry is only a
            // key, so wiring this path to the same helper is a follow-up.
            // Shared keys cannot be re-rooted without a document either way.
            None if key.is_client_fallback() => {
                // The palette registry is session-persistent, so an origin
                // removed/disabled from config after registration lands here —
                // warn like the encoded-command path (user-invoked).
                if !crate::config::is_server_spawnable(&settings.language_servers, origin) {
                    warn!(
                        target: "kakehashi::bridge",
                        "executeCommand: palette origin {origin:?} is no longer spawnable; \
                         dropping {:?}",
                        params.command
                    );
                    return None;
                }
                let Some(config) = resolve_with_wildcard(
                    &settings.language_servers,
                    origin,
                    merge_bridge_server_configs,
                ) else {
                    warn!(
                        target: "kakehashi::bridge",
                        "executeCommand: palette origin {origin:?} has no resolvable config; \
                         dropping {:?}",
                        params.command
                    );
                    return None;
                };
                // Wait through initialization (bounded by the standard init
                // budget) rather than take a possibly-`Initializing` handle: the
                // pool returns an existing not-yet-Ready connection here, whose
                // `has_capability` check below would then spuriously fail-soft to
                // `null` even though it would be Ready moments later. Fails soft on
                // timeout/spawn error like every other branch.
                match self
                    .get_or_create_connection_wait_ready(
                        origin,
                        &config,
                        None,
                        std::time::Duration::from_secs(crate::lsp::bridge::pool::INIT_TIMEOUT_SECS),
                    )
                    .await
                {
                    Ok(handle) => handle,
                    Err(e) => {
                        warn!(
                            target: "kakehashi::bridge",
                            "executeCommand: failed to reconnect to {origin} for palette command: {e}"
                        );
                        return None;
                    }
                }
            }
            None => {
                warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: origin connection for palette command {:?} ({origin:?}) is not ready; ignoring",
                    params.command
                );
                return None;
            }
        };
        if !handle.has_capability(METHOD) {
            // The advertising connection was Ready (capabilities set) when it
            // registered the command, but the RECONNECT path can hand back a
            // still-`Initializing` handle whose capabilities aren't set yet, so
            // this is reachable. Warn rather than drop silently (every other
            // failure branch warns) so a fail-soft `null` is diagnosable.
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: origin {origin:?} for palette command {:?} does not (yet) advertise executeCommandProvider; ignoring",
                params.command
            );
            return None;
        }
        // Forward the command name and arguments verbatim.
        self.send_execute_command_on_handle(&handle, params, upstream_id)
            .await
    }

    /// Send a `workspace/executeCommand` on an already-connected handle and
    /// return the raw `result` (null normalized to `None`). Returns `None` on
    /// any failure (register/send/wait/error) so the caller fails soft.
    async fn send_execute_command_on_handle(
        &self,
        handle: &Arc<ConnectionHandle>,
        params: ExecuteCommandParams,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Value> {
        let connection_key = handle.key();
        if let Some(ref id) = upstream_id {
            self.register_upstream_request(id.clone(), connection_key);
        }
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(
                        target: "kakehashi::bridge",
                        "executeCommand: failed to register request on {connection_key:?} \
                         for {:?}: {e}",
                        params.command
                    );
                    if let Some(ref id) = upstream_id {
                        self.unregister_upstream_request(id, connection_key);
                    }
                    return None;
                }
            };

        let request = JsonRpcRequest::new(request_id.as_i64(), METHOD, &params);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        // Verify `handle` is still the pool's LIVE connection for its key before
        // sending, under the `connections` lock: `handle` was fetched earlier
        // (get_or_create), and a concurrent respawn could have replaced it. Both
        // the check and the enqueue must hold the lock so the swap can't
        // interleave — the same guard `execute_bridge_request_with_handle` uses.
        // Sending on a stale handle would route the request (and its cancel
        // bookkeeping) to a dead/outdated process. On failure `router_guard`
        // drops (cleaning the router entry).
        {
            let connections = self.connections().await;
            if !connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, handle))
            {
                drop(connections);
                warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: connection {connection_key} was replaced before send"
                );
                if let Some(ref id) = upstream_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return None;
            }
            if let Err(e) = handle.send_request(request, request_id) {
                drop(connections);
                warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: failed to send {:?} on {connection_key:?}: {e}",
                    params.command
                );
                if let Some(ref id) = upstream_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return None;
            }
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, connection_key);
        }

        // Fail soft, but not silently: surface timeouts / channel-closed like the
        // other branches so execute-time issues are debuggable (sibling sweep of
        // the codeAction/resolve logging fix).
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: wait for response failed on {connection_key:?}: {e}"
                );
                return None;
            }
        };
        parse_execute_command_response(response)
    }
}

/// Parse a JSON-RPC `workspace/executeCommand` response into its `result`,
/// relayed verbatim. Returns `None` for errors and a null result.
fn parse_execute_command_response(mut response: Value) -> Option<Value> {
    if response_has_jsonrpc_error(&response, METHOD) {
        return None;
    }
    let result = response.get_mut("result").map(Value::take)?;
    if result.is_null() {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_relays_a_real_result_verbatim() {
        let response = json!({
            "jsonrpc": "2.0", "id": 7,
            "result": { "applied": true, "custom": [1, 2, 3] }
        });
        assert_eq!(
            parse_execute_command_response(response),
            Some(json!({ "applied": true, "custom": [1, 2, 3] }))
        );
    }

    #[test]
    fn parse_collapses_a_jsonrpc_error_to_none() {
        // Fail-soft contract: a downstream error becomes a null result
        // upstream (warn-logged elsewhere), never an upstream error.
        let response = json!({
            "jsonrpc": "2.0", "id": 7,
            "error": { "code": -32603, "message": "boom" }
        });
        assert_eq!(parse_execute_command_response(response), None);
    }

    #[test]
    fn parse_collapses_a_null_result_to_none() {
        // Many servers legitimately answer null (they applied their effect
        // via applyEdit); None keeps the upstream response null.
        let response = json!({ "jsonrpc": "2.0", "id": 7, "result": null });
        assert_eq!(parse_execute_command_response(response), None);
    }
}
