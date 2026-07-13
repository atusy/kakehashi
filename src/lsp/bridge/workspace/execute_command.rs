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
use crate::lsp::bridge::pool::{ConnectionHandle, ConnectionState, LanguageServerPool, UpstreamId};
use crate::lsp::bridge::protocol::{JsonRpcRequest, response_has_jsonrpc_error};
use tower_lsp_server::ls_types::ExecuteCommandParams;

const METHOD: &str = "workspace/executeCommand";

#[derive(Debug, PartialEq, Eq)]
enum ReadyPaletteOrigin {
    None,
    Unique(crate::lsp::bridge::ConnectionKey),
    Ambiguous,
}

fn select_ready_palette_origin(
    origins: &[crate::lsp::bridge::ConnectionKey],
    mut is_ready: impl FnMut(&crate::lsp::bridge::ConnectionKey) -> bool,
) -> ReadyPaletteOrigin {
    let mut ready = origins.iter().filter(|key| is_ready(key));
    let Some(first) = ready.next() else {
        return ReadyPaletteOrigin::None;
    };
    if ready.next().is_some() {
        ReadyPaletteOrigin::Ambiguous
    } else {
        ReadyPaletteOrigin::Unique(first.clone())
    }
}

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
        // PALETTE FIRST. A downstream's advertised command ids are arbitrary
        // strings, so one could in principle be shaped exactly like a routed name
        // (`kakehashi|c|srv||reload`). Decoding first would strip it to `reload`
        // and send the wrong id. An encoded name can never be in this registry —
        // it holds RAW advertised names — so an exact hit here is unambiguous
        // evidence the client meant the palette command.
        if self.command_origins().is_registered(&params.command) {
            return self
                .dispatch_palette_command(params, settings, upstream_id)
                .await;
        }
        // Decode into an owned key/command so `params.arguments` can move into
        // the outgoing request without a partial-borrow conflict.
        let (key, command) = match decode_command(&params.command) {
            Some(route) => (route.key, route.command.to_string()),
            None => {
                // Neither a registered raw name nor an action-encoded one. The
                // palette path re-checks and emits the "foreign command" warn.
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
        let handle = match self
            .ready_connection_by_key_for_config(&key, Some(&config))
            .await
        {
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
        //
        // Drop the command if the wait did not settle. Sending anyway would
        // waive the exact guarantee this barrier exists for and surface as a
        // downstream error; a fail-soft null the user can re-fire is better.
        if !self.wait_for_pending_reopen(&key).await {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: {origin:?} is still re-opening its documents; \
                 dropping {command:?} rather than sending it out of order"
            );
            return None;
        }
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
    /// envelope) to the sole live connection advertising that exact name. If no
    /// advertiser is live, the handshake registry can reconnect one unambiguous
    /// client-root origin. Multiple live or historical origins fail soft because
    /// the raw request carries no workspace identity (#823). Forwards the command
    /// name and arguments verbatim; other failures remain fail-soft like the
    /// encoded-command path.
    async fn dispatch_palette_command(
        &self,
        params: ExecuteCommandParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Value> {
        let origins = self.command_origins().origins(&params.command);
        if origins.is_empty() {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: {:?} is neither a bridged nor a registered command; ignoring",
                params.command
            );
            return None;
        }

        // Resolve liveness from one connections-map snapshot, scanning the
        // handles' actual advertised command lists rather than only the origin
        // registry. A handshake publishes its capabilities + Ready state before
        // recording dynamic palette metadata; consulting only the registry in
        // that window would miss a live colliding advertiser (#823). The command
        // linearizes at this snapshot: exactly one Ready advertiser is safe;
        // several are inherently ambiguous because the raw name carries no
        // document/workspace identity.
        let ready_origins: Vec<_> = {
            let connections = self.connections().await;
            connections
                .iter()
                .filter(|(_, handle)| {
                    handle.state() == ConnectionState::Ready
                        && handle.advertises_execute_command(&params.command)
                })
                .map(|(key, _)| key.clone())
                .collect()
        };
        let key = match select_ready_palette_origin(&ready_origins, |_| true) {
            ReadyPaletteOrigin::Unique(key) => key,
            ReadyPaletteOrigin::Ambiguous => {
                warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: palette command {:?} has multiple live origins; ignoring",
                    params.command
                );
                return None;
            }
            ReadyPaletteOrigin::None if origins.len() == 1 => origins[0].clone(),
            ReadyPaletteOrigin::None => {
                warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: palette command {:?} has multiple registered origins and none is live; ignoring",
                    params.command
                );
                return None;
            }
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
        // Same ordering requirement as the encoded path: a palette command can
        // reference a document too (a downstream is free to take a URI argument),
        // and this connection may have just respawned with its re-open still in
        // flight. Bounded, and a no-op when nothing is pending.
        if !self.wait_for_pending_reopen(&key).await {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: origin {origin:?} is still re-opening its documents; \
                 dropping palette command {:?} rather than sending it out of order",
                params.command
            );
            return None;
        }
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
    use std::collections::HashSet;

    #[test]
    fn collision_routes_when_exactly_one_advertiser_is_live() {
        let ruff =
            crate::lsp::bridge::ConnectionKey::new("ruff", Some("file:///workspace/a".to_string()));
        let eslint = crate::lsp::bridge::ConnectionKey::new(
            "eslint",
            Some("file:///workspace/b".to_string()),
        );
        let ready = HashSet::from([eslint.clone()]);

        assert_eq!(
            select_ready_palette_origin(&[ruff, eslint.clone()], |key| ready.contains(key)),
            ReadyPaletteOrigin::Unique(eslint),
        );
    }

    #[test]
    fn collision_refuses_to_choose_between_live_advertisers() {
        let ruff =
            crate::lsp::bridge::ConnectionKey::new("ruff", Some("file:///workspace/a".to_string()));
        let eslint = crate::lsp::bridge::ConnectionKey::new(
            "eslint",
            Some("file:///workspace/b".to_string()),
        );

        assert_eq!(
            select_ready_palette_origin(&[ruff, eslint], |_| true),
            ReadyPaletteOrigin::Ambiguous,
        );
    }

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
