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
use crate::lsp::bridge::ConnectionKey;
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::decode_command;
use crate::lsp::bridge::pool::{ConnectionHandle, ConnectionState, LanguageServerPool, UpstreamId};
use crate::lsp::bridge::protocol::{JsonRpcRequest, response_has_jsonrpc_error};
use tower_lsp_server::ls_types::ExecuteCommandParams;

const METHOD: &str = "workspace/executeCommand";

/// What to do with a raw palette command name.
#[derive(Debug, PartialEq, Eq)]
enum PaletteRoute {
    /// Exactly one live connection advertises it.
    Route(ConnectionKey),
    /// Nothing live, but exactly one recorded origin can be revived.
    Reconnect(ConnectionKey),
    /// Several live connections advertise it; the request cannot say which.
    AmbiguousLive(Vec<ConnectionKey>),
    /// Nothing live, and zero or several revivable origins.
    Unreachable(Vec<ConnectionKey>),
}

/// The entire palette routing decision, as a pure function of the two sets the
/// dispatcher gathers.
///
/// Whole rather than in pieces on purpose: the property that matters is not that
/// "two live advertisers" classifies as ambiguous, it is that the dispatcher
/// then refuses. Splitting the classification from the action left the action
/// pinned by nothing, and mutations that dropped the refusal outright survived
/// the entire suite.
///
/// `ready` wins over `reconnectable` whenever it is non-empty: a live connection
/// is the one thing that can actually serve the command, and reviving a
/// recorded origin while another is live would spawn a second process for a
/// workspace that already has one.
fn select_palette_route(ready: &[ConnectionKey], reconnectable: &[ConnectionKey]) -> PaletteRoute {
    match ready {
        [only] => return PaletteRoute::Route(only.clone()),
        [_, _, ..] => return PaletteRoute::AmbiguousLive(ready.to_vec()),
        [] => {}
    }
    match reconnectable {
        [only] => PaletteRoute::Reconnect(only.clone()),
        _ => PaletteRoute::Unreachable(reconnectable.to_vec()),
    }
}

/// Why a palette command was refused. The two cases are genuinely different
/// sentences, not one sentence with a different noun: "several of these claim
/// it" and "none of these can be reached" share no clause, and gluing a common
/// tail onto both produced "[none] advertise it".
enum PaletteRefusal {
    /// Several live connections advertise the name.
    SeveralLive(Vec<ConnectionKey>),
    /// Nothing live advertises it, and the recorded origins do not resolve to a
    /// single revivable one. The list is often empty, which is the honest answer
    /// for a name whose advertisers have all gone away.
    NothingReachable(Vec<ConnectionKey>),
}

/// The whole reason, as one sentence the user can act on.
fn describe_refusal(refusal: &PaletteRefusal) -> String {
    match refusal {
        PaletteRefusal::SeveralLive(candidates) => format!(
            "several live connections [{}] advertise it, and the request carries no workspace \
             context, so kakehashi will not guess which to use",
            describe_candidates(candidates)
        ),
        PaletteRefusal::NothingReachable(candidates) => format!(
            "no live connection advertises it, and its recorded origins [{}] do not resolve to \
             a single one that can be reconnected",
            describe_candidates(candidates)
        ),
    }
}

/// Render the connections a refused command could have meant, e.g.
/// `ruff@file:///w/a, ruff@file:///w/b`. Empty renders as `none`, which is the
/// honest answer for a name whose advertisers have all gone away.
fn describe_candidates(candidates: &[ConnectionKey]) -> String {
    if candidates.is_empty() {
        return "none".to_string();
    }
    candidates
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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
        // and send the wrong id, so a raw-registry hit wins.
        //
        // That rule was safe while encoded names were minted per code-action
        // response and never registered: the client could not be holding one as
        // a palette entry, so a hit meant the palette command and nothing else.
        // Registering encoded names breaks the premise. A downstream can now
        // advertise a raw id byte-identical to an entry kakehashi minted for a
        // DIFFERENT connection, and the two readings point at different
        // processes with no way to tell which the user picked. Refuse — the same
        // answer #823 gives every other unresolvable name.
        // The test is whether a routed entry of this exact text EXISTS, not
        // whether the string happens to decode. A downstream is free to
        // advertise a raw id shaped like a routed name with nothing on the other
        // side of it; that has one reading and routes fine, as it did before
        // these entries were registered.
        let is_raw_name = self.command_origins().is_registered(&params.command);
        let collides_with_routed_entry = self.command_origins().holds_encoded(&params.command);
        if collides_with_routed_entry && is_raw_name {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: {:?} is both a routed name kakehashi minted and a raw id a \
                 downstream advertised; refusing rather than guessing which was picked",
                params.command
            );
            self.warn_to_editor(format!(
                "command {:?} was not run: it is both a routed entry kakehashi created and a \
                 raw command a downstream server advertised, so which one you picked cannot \
                 be told apart",
                params.command
            ));
            return None;
        }
        if is_raw_name {
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
            //
            // Deliberately the LOOSE check, unlike the palette path's
            // `advertises_execute_command`: an action-minted command is a name
            // the bridge chose from a `Command` the server surfaced, and servers
            // routinely surface code-action commands whose ids are absent from
            // `executeCommandProvider.commands`. Requiring the exact name here
            // would fail soft on working setups.
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

    /// Log AND tell the editor that a palette command was refused, naming the
    /// connections it could have meant.
    ///
    /// Naming them is the point: the user knows which command they picked, not
    /// which servers claim it, and `ConnectionKey`'s `Display` is exactly the
    /// server-and-root pair they would have to change to disambiguate.
    fn refuse_palette_command(&self, command: &str, refusal: PaletteRefusal) {
        let reason = describe_refusal(&refusal);
        warn!(
            target: "kakehashi::bridge",
            "executeCommand: refusing palette command {command:?}: {reason}"
        );
        self.warn_to_editor(format!("command {command:?} was not run: {reason}"));
    }

    /// Every live connection whose EXACT advertised command list contains
    /// `command`, read from one connections-map snapshot.
    ///
    /// Scanning the handles rather than the origin registry is what closes the
    /// REGISTRY-LAG window: a connection publishes its capabilities and flips to
    /// `Ready` before its palette metadata reaches the registry, so a
    /// registry-only check can miss a live colliding advertiser and conclude
    /// "unique" about a set it cannot yet see (#823).
    ///
    /// A candidate must also be one the CURRENT settings would still produce.
    /// The settings snapshot is published before `propagate_settings` finishes
    /// invalidating connections, so this window contains processes spawned from
    /// a config that no longer exists — a deleted server, or one whose `cmd`
    /// moved. They have to be excluded HERE rather than at acquisition: the
    /// acquisition would reject them anyway, but by then the candidate count has
    /// already been used, so a stale process standing beside the current one
    /// turns a routable command into a refused "ambiguous" one.
    ///
    /// In production this filter is never vacuous: `record_launch_config` runs
    /// on the one spawn path before the `Ready` transition, so every connection
    /// the state filter admits has a snapshot to compare.
    ///
    /// It does not make the decision globally atomic, and nothing downstream
    /// re-checks it: an advertiser that reaches `Ready` after this snapshot is
    /// not counted, so the command goes to what was the sole live advertiser
    /// when it was decided. Re-checking later would only move that window, and
    /// the target was never the *wrong* one — just not the only one.
    async fn ready_palette_origins(
        &self,
        command: &str,
        settings: &WorkspaceSettings,
    ) -> Vec<ConnectionKey> {
        // Take the cheap, lock-scoped half first. Resolving a config allocates
        // an owned `BridgeServerConfig` — cloning `cmd`, `languages`, and
        // arbitrary `initialization_options` JSON — and merging it is real work;
        // none of that belongs under the pool-wide connections guard.
        let advertisers: Vec<_> = {
            let connections = self.connections().await;
            connections
                .iter()
                .filter(|(_, handle)| {
                    handle.state() == ConnectionState::Ready
                        && handle.advertises_execute_command(command)
                })
                .map(|(key, handle)| (key.clone(), Arc::clone(handle)))
                .collect()
        };
        // Resolve once per distinct server: a collision is usually the SAME
        // server under several roots, which is the case that would otherwise
        // merge the same config repeatedly.
        let mut resolved: std::collections::HashMap<String, Option<_>> =
            std::collections::HashMap::new();
        advertisers
            .into_iter()
            .filter(|(key, handle)| {
                resolved
                    .entry(key.server().to_string())
                    .or_insert_with(|| {
                        resolve_with_wildcard(
                            &settings.language_servers,
                            key.server(),
                            merge_bridge_server_configs,
                        )
                    })
                    .as_ref()
                    .is_some_and(|config| handle.matches_launch_config(config))
            })
            .map(|(key, _)| key)
            .collect()
    }

    /// Route a PALETTE-fired command (a raw downstream command name, no action
    /// envelope) to the sole live connection advertising that exact name. If no
    /// advertiser is live, one unambiguous client-root origin can be reconnected.
    /// Several live advertisers fail soft because the raw request carries no
    /// workspace identity (#823). Forwards the command name and arguments
    /// verbatim; other failures remain fail-soft like the encoded-command path.
    ///
    /// Refusals are reported to the EDITOR as well as the log: this is the one
    /// branch where the user positively picked the action and there is no
    /// correct target, so a silent null would be indistinguishable from "nothing
    /// to do".
    async fn dispatch_palette_command(
        &self,
        params: ExecuteCommandParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> Option<Value> {
        if !self.command_origins().is_registered(&params.command) {
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: {:?} is neither a bridged nor a registered command; ignoring",
                params.command
            );
            return None;
        }
        // Both candidate sets are filtered by the CURRENT config before anything
        // counts them, because a candidate the acquisition would reject must not
        // get a vote on whether the command is ambiguous.
        //
        // A recorded origin whose server was removed from config cannot be
        // revived, so counting it would let a deleted server permanently veto
        // the origin that is still spawnable. `ready_palette_origins` applies
        // the launch-config half of the same rule; this adds the half it cannot
        // express, since a server can be present in config yet disabled or
        // command-less.
        let spawnable = |key: &ConnectionKey| {
            crate::config::is_server_spawnable(&settings.language_servers, key.server())
        };
        let reconnectable: Vec<_> = self
            .command_origins()
            .reconnectable_origins(&params.command)
            .into_iter()
            .filter(&spawnable)
            .collect();
        let ready: Vec<_> = self
            .ready_palette_origins(&params.command, settings)
            .await
            .into_iter()
            .filter(&spawnable)
            .collect();

        let key = match select_palette_route(&ready, &reconnectable) {
            PaletteRoute::Route(key) | PaletteRoute::Reconnect(key) => key,
            PaletteRoute::AmbiguousLive(candidates) => {
                self.refuse_palette_command(
                    &params.command,
                    PaletteRefusal::SeveralLive(candidates),
                );
                return None;
            }
            PaletteRoute::Unreachable(candidates) => {
                self.refuse_palette_command(
                    &params.command,
                    PaletteRefusal::NothingReachable(candidates),
                );
                return None;
            }
        };
        let origin = key.server();
        // Resolved once, and BEFORE the live lookup rather than only inside the
        // reconnect branch, so the by-key fast path performs the same
        // launch-config check every other acquisition in the pool does.
        //
        // Bound fail-closed rather than passed as an `Option`. A chosen key has
        // already passed `is_server_spawnable` on this same immutable snapshot,
        // which requires a concrete entry — so `None` here is unreachable today.
        // Encoding it as a refusal costs nothing and means a future change that
        // makes it reachable lands on a drop rather than on a lookup that
        // silently skips the config check.
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
        let handle = match self
            .ready_connection_by_key_for_config(&key, Some(&config))
            .await
        {
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
                // No spawnability re-check here: the chosen key passed the
                // identical predicate against this same borrowed snapshot a few
                // lines up, and nothing between can change it. A second one
                // would read as a live guard against a race that does not exist.
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
        if !handle.advertises_execute_command(&params.command) {
            // Revalidate the EXACT name on the handle we are about to send on.
            // The scan proved something advertised it; this handle is whatever
            // acquisition returned, which on the reconnect path can be a freshly
            // spawned — possibly upgraded — server that dropped the name. Mere
            // `executeCommandProvider` presence must not forward a command the
            // process no longer has (#823).
            //
            // This is NOT what protects against a replacement during the re-open
            // wait: `server_capabilities` is a `OnceLock` written before Ready,
            // so THIS `Arc`'s answer cannot change once read, and a replacement
            // is a different `Arc` entirely. That case is covered by the
            // `Arc::ptr_eq` re-check taken with the enqueue in
            // `send_execute_command_on_handle` — do not weaken it on the
            // strength of this check.
            warn!(
                target: "kakehashi::bridge",
                "executeCommand: origin {origin:?} no longer advertises palette command {:?}; ignoring",
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
    use crate::lsp::bridge::pool::test_helpers::create_handle_advertising_commands;
    use serde_json::json;

    fn key(server: &str, root: &str) -> ConnectionKey {
        ConnectionKey::new(server, Some(root.to_string()))
    }

    /// Seed a connection in `state` advertising exactly `commands`, with no
    /// recorded launch config (so config checks pass).
    async fn seed(
        pool: &LanguageServerPool,
        key: &ConnectionKey,
        state: ConnectionState,
        commands: &[&str],
    ) {
        pool.insert_connection(
            create_handle_advertising_commands(state, key.clone(), commands, None).await,
        )
        .await;
    }

    /// As [`seed`], but the connection remembers the config it was spawned from.
    async fn seed_spawned_from(
        pool: &LanguageServerPool,
        key: &ConnectionKey,
        commands: &[&str],
        cmd: &str,
    ) {
        let config = crate::config::settings::BridgeServerConfig {
            cmd: vec![cmd.to_string()],
            languages: vec!["*".to_string()],
            ..Default::default()
        };
        pool.insert_connection(
            create_handle_advertising_commands(
                ConnectionState::Ready,
                key.clone(),
                commands,
                Some(&config),
            )
            .await,
        )
        .await;
    }

    /// The whole routing decision, as a table. Every row is a mutation that
    /// would otherwise pass: dropping the ambiguity refusal, letting the no-live
    /// path pick an arbitrary origin, or preferring a revivable origin over a
    /// live one all change a cell here.
    #[test]
    fn the_palette_route_table() {
        let a = key("ruff", "file:///w/a");
        let b = key("eslint", "file:///w/b");
        let fallback = ConnectionKey::new("ruff", None);

        // Live wins, and one live advertiser is the only routable shape.
        assert_eq!(
            select_palette_route(std::slice::from_ref(&a), &[]),
            PaletteRoute::Route(a.clone()),
        );
        assert_eq!(
            select_palette_route(std::slice::from_ref(&a), std::slice::from_ref(&fallback)),
            PaletteRoute::Route(a.clone()),
            "a live connection must not be passed over to spawn a second one",
        );

        // Several live advertisers: the request cannot say which, so neither can we.
        assert_eq!(
            select_palette_route(&[a.clone(), b.clone()], &[]),
            PaletteRoute::AmbiguousLive(vec![a.clone(), b.clone()]),
        );
        assert_eq!(
            select_palette_route(&[a.clone(), b.clone()], std::slice::from_ref(&fallback)),
            PaletteRoute::AmbiguousLive(vec![a.clone(), b.clone()]),
            "a revivable origin must not break a live tie",
        );

        // Nothing live: exactly one revivable origin may be reconnected.
        assert_eq!(
            select_palette_route(&[], std::slice::from_ref(&fallback)),
            PaletteRoute::Reconnect(fallback.clone()),
        );
        assert_eq!(
            select_palette_route(&[], &[]),
            PaletteRoute::Unreachable(vec![]),
        );
        assert_eq!(
            select_palette_route(&[], &[fallback.clone(), b.clone()]),
            PaletteRoute::Unreachable(vec![fallback, b]),
            "two revivable origins are as unresolvable as two live ones",
        );
    }

    #[test]
    fn refusals_name_the_connections_the_user_would_have_to_change() {
        assert_eq!(describe_candidates(&[]), "none");
        assert_eq!(
            describe_candidates(&[key("ruff", "file:///w/a"), key("ruff", "file:///w/b")]),
            "ruff@file:///w/a, ruff@file:///w/b",
            "server AND root: the root is the axis a same-server collision turns on"
        );
    }

    #[test]
    fn each_refusal_reads_as_a_sentence_about_its_own_case() {
        let several = describe_refusal(&PaletteRefusal::SeveralLive(vec![
            key("ruff", "file:///w/a"),
            key("eslint", "file:///w/b"),
        ]));
        assert!(several.contains("ruff@file:///w/a, eslint@file:///w/b"));
        assert!(several.contains("will not guess which to use"));

        // The case a shared sentence tail broke: no candidates at all, where
        // "[none] advertise it" was both ungrammatical and untrue.
        let nothing = describe_refusal(&PaletteRefusal::NothingReachable(vec![]));
        assert!(
            !nothing.contains("advertise it"),
            "an empty candidate list must not be described as advertising anything: {nothing}"
        );
        assert!(nothing.contains("no live connection advertises it"));
        assert!(nothing.contains("[none]"));
    }

    #[tokio::test]
    async fn the_scan_sees_every_live_advertiser_of_the_exact_name() {
        let pool = LanguageServerPool::new();
        let ruff = key("ruff", "file:///workspace/a");
        let eslint = key("eslint", "file:///workspace/b");
        seed(&pool, &ruff, ConnectionState::Ready, &["source.fixAll"]).await;
        seed(&pool, &eslint, ConnectionState::Ready, &["source.fixAll"]).await;

        let mut found = pool
            .ready_palette_origins(
                "source.fixAll",
                &settings_with_servers(&["ruff", "eslint", "biome"]),
            )
            .await;
        found.sort_by_key(|k| k.server().to_string());

        assert_eq!(
            found,
            vec![eslint, ruff],
            "missing either advertiser would make an ambiguous command look unique"
        );
    }

    #[tokio::test]
    async fn the_scan_ignores_a_connection_that_cannot_serve_the_name() {
        let pool = LanguageServerPool::new();
        let ready = key("ruff", "file:///workspace/a");
        seed(&pool, &ready, ConnectionState::Ready, &["source.fixAll"]).await;
        // Advertises the name but is still handshaking: not routable yet.
        seed(
            &pool,
            &key("eslint", "file:///workspace/b"),
            ConnectionState::Initializing,
            &["source.fixAll"],
        )
        .await;
        // Ready, but this exact name is not in its list — `has_capability` would
        // have accepted it on mere provider presence.
        seed(
            &pool,
            &key("biome", "file:///workspace/c"),
            ConnectionState::Ready,
            &["source.organizeImports"],
        )
        .await;

        assert_eq!(
            pool.ready_palette_origins(
                "source.fixAll",
                &settings_with_servers(&["ruff", "eslint", "biome"])
            )
            .await,
            vec![ready],
        );
    }

    #[tokio::test]
    async fn the_scan_matches_the_name_exactly_not_by_prefix() {
        let pool = LanguageServerPool::new();
        seed(
            &pool,
            &key("eslint", "file:///w/a"),
            ConnectionState::Ready,
            &["source.fixAll.eslint"],
        )
        .await;

        assert!(
            pool.ready_palette_origins(
                "source.fixAll",
                &settings_with_servers(&["ruff", "eslint", "biome"])
            )
            .await
            .is_empty(),
            "a longer name that merely starts with the query is a different command"
        );
    }

    fn params(command: &str) -> ExecuteCommandParams {
        ExecuteCommandParams {
            command: command.to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }
    }

    /// Drive the dispatcher itself. `select_palette_route` returning the right
    /// verdict is not the property that matters — acting on it is, and that
    /// wiring is what a classification-only test leaves unpinned.
    ///
    /// `settings` is never `default()` when a connection is meant to be usable:
    /// both candidate sets are filtered against the current config, so a server
    /// absent from `languageServers` is correctly treated as deleted — and a
    /// test that forgot to configure it would pass by refusing for the wrong
    /// reason.
    async fn dispatch(
        pool: &LanguageServerPool,
        settings: &WorkspaceSettings,
        command: &str,
    ) -> Option<Value> {
        pool.dispatch_palette_command(params(command), settings, None)
            .await
    }

    /// Assert a dispatch reached a downstream — it parks waiting for a reply the
    /// sink process never writes, so not settling IS the send.
    ///
    /// The distinction these tests need, and the one a bare `is_none()` cannot
    /// make: every branch in this file fails soft to `None`, so "returned null"
    /// says nothing about whether the command was sent.
    ///
    /// The two directions get different budgets on purpose. Proving a send only
    /// needs long enough for a refusal to have finished (microseconds of map and
    /// mutex work), so it is short. Proving a REFUSAL waits for the future to
    /// settle, and a budget that is merely generous would turn a loaded machine
    /// into a red suite — so that side gets a budget no refusal can plausibly
    /// exceed while still being far below "parks forever".
    async fn assert_reached_downstream(fut: impl std::future::Future<Output = Option<Value>>) {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), fut)
                .await
                .is_err(),
            "expected the command to be forwarded, but the dispatch settled — \
             which only the refusal branches do"
        );
    }

    async fn assert_refused(fut: impl std::future::Future<Output = Option<Value>>) {
        let settled = tokio::time::timeout(std::time::Duration::from_secs(10), fut).await;
        match settled {
            Ok(result) => assert!(result.is_none(), "a refusal must answer null"),
            Err(_) => panic!("expected a refusal, but the dispatch forwarded and parked"),
        }
    }

    #[tokio::test]
    async fn dispatch_refuses_a_command_two_live_connections_advertise() {
        let pool = LanguageServerPool::new();
        let ruff = key("ruff", "file:///w/a");
        let eslint = key("eslint", "file:///w/b");
        pool.command_origins()
            .register(&ruff, vec!["source.fixAll".to_string()]);
        pool.command_origins()
            .register(&eslint, vec!["source.fixAll".to_string()]);
        seed(&pool, &ruff, ConnectionState::Ready, &["source.fixAll"]).await;
        seed(&pool, &eslint, ConnectionState::Ready, &["source.fixAll"]).await;

        // Picking either of two live advertisers is the #823 defect itself.
        let settings = settings_with_servers(&["ruff", "eslint"]);
        assert_refused(dispatch(&pool, &settings, "source.fixAll")).await;
    }

    #[tokio::test]
    async fn dispatch_routes_when_only_one_of_the_advertisers_is_live() {
        let pool = LanguageServerPool::new();
        let ruff = key("ruff", "file:///w/a");
        let eslint = key("eslint", "file:///w/b");
        pool.command_origins()
            .register(&ruff, vec!["source.fixAll".to_string()]);
        pool.command_origins()
            .register(&eslint, vec!["source.fixAll".to_string()]);
        // Only ruff is live; eslint is a registered-but-dead collision.
        seed(&pool, &ruff, ConnectionState::Ready, &["source.fixAll"]).await;

        // A dead collision must not cost the one advertiser that can serve it.
        let settings = settings_with_servers(&["ruff", "eslint"]);
        assert_reached_downstream(dispatch(&pool, &settings, "source.fixAll")).await;
    }

    /// Settings in which exactly the named servers can be spawned.
    fn settings_with_servers(names: &[&str]) -> WorkspaceSettings {
        let mut settings = WorkspaceSettings::default();
        for name in names {
            settings.language_servers.insert(
                (*name).to_string(),
                crate::config::settings::BridgeServerConfig {
                    cmd: vec!["true".to_string()],
                    languages: vec!["*".to_string()],
                    ..Default::default()
                },
            );
        }
        settings
    }

    #[tokio::test]
    async fn a_superseded_process_beside_the_current_one_does_not_make_it_ambiguous() {
        // The window's real shape: during settings propagation the OLD process
        // is still Ready while the new one is already up. Both advertise the
        // name and both name a configured server, so a spawnability-only filter
        // counts two and refuses — losing a command that has exactly one valid
        // target. The launch-config check has to run before anything counts.
        let pool = LanguageServerPool::new();
        let current = key("ruff", "file:///w/a");
        let superseded = key("ruff", "file:///w/b");
        pool.command_origins()
            .register(&current, vec!["source.fixAll".to_string()]);
        seed_spawned_from(&pool, &current, &["source.fixAll"], "true").await;
        seed_spawned_from(&pool, &superseded, &["source.fixAll"], "old-ruff").await;

        // `settings_with_servers` spawns `true`, so only `current` matches.
        let settings = settings_with_servers(&["ruff"]);
        assert_eq!(
            pool.ready_palette_origins("source.fixAll", &settings).await,
            vec![current],
            "a process the acquisition would reject must not get a vote"
        );
        assert_reached_downstream(dispatch(&pool, &settings, "source.fixAll")).await;
    }

    #[tokio::test]
    async fn a_live_connection_spawned_from_a_superseded_cmd_is_not_routed_to() {
        // Same publication window as the deleted-server case, but the server is
        // still configured — only its command line moved. The name-level
        // spawnability filter cannot see that, so the by-key acquisition must do
        // what every other acquisition in the pool does and compare the launch
        // config, or the command runs on the process the user just replaced.
        let pool = LanguageServerPool::new();
        let ruff = key("ruff", "file:///w/a");
        pool.command_origins()
            .register(&ruff, vec!["source.fixAll".to_string()]);
        seed_spawned_from(&pool, &ruff, &["source.fixAll"], "old-ruff").await;

        // `settings_with_servers` spawns `true`, which is not `old-ruff`.
        let settings = settings_with_servers(&["ruff"]);
        assert_refused(dispatch(&pool, &settings, "source.fixAll")).await;
    }

    #[tokio::test]
    async fn a_live_connection_for_a_deleted_server_does_not_create_ambiguity() {
        // The settings snapshot is published before `propagate_settings` finishes
        // invalidating connections, so a request can find a still-Ready process
        // spawned from a config that no longer exists. Counting it would either
        // route a workspace-mutating command to a superseded server or refuse a
        // command that has exactly one valid target.
        let pool = LanguageServerPool::new();
        let ruff = key("ruff", "file:///w/a");
        let deleted = key("deleted", "file:///w/b");
        pool.command_origins()
            .register(&ruff, vec!["source.fixAll".to_string()]);
        pool.command_origins()
            .register(&deleted, vec!["source.fixAll".to_string()]);
        seed(&pool, &ruff, ConnectionState::Ready, &["source.fixAll"]).await;
        seed(&pool, &deleted, ConnectionState::Ready, &["source.fixAll"]).await;

        let settings = settings_with_servers(&["ruff"]);
        assert_reached_downstream(dispatch(&pool, &settings, "source.fixAll")).await;
    }

    #[tokio::test]
    async fn a_raw_name_that_collides_with_a_real_routed_entry_is_refused() {
        // The genuine collision: `srv` advertises `reload`, so kakehashi minted
        // and registered `kakehashi|c|srv||reload` for it — and `ruff`
        // separately advertises that exact text as a raw id of its own. The two
        // readings name different processes with nothing to separate them.
        let pool = LanguageServerPool::new();
        let srv = ConnectionKey::new("srv", None);
        let ruff = ConnectionKey::new("ruff", None);
        let shaped = "kakehashi|c|srv||reload";
        assert!(
            pool.command_origins()
                .register(&srv, vec!["reload".to_string()])
                .newly_encoded
                .contains(&shaped.to_string()),
            "the routed entry this collides with must really have been minted"
        );
        pool.command_origins()
            .register(&ruff, vec![shaped.to_string()]);
        seed(&pool, &ruff, ConnectionState::Ready, &[shaped]).await;

        assert_refused(pool.dispatch_execute_command(
            params(shaped),
            &settings_with_servers(&["ruff", "srv"]),
            None,
        ))
        .await;
    }

    #[tokio::test]
    async fn a_routed_shaped_raw_name_with_no_routed_twin_still_routes() {
        // Decodability alone is not a collision. Nothing ever minted
        // `kakehashi|c|srv||reload`, so there is exactly one reading of it and
        // refusing would break a command that works.
        let pool = LanguageServerPool::new();
        let ruff = ConnectionKey::new("ruff", None);
        let shaped = "kakehashi|c|srv||reload";
        pool.command_origins()
            .register(&ruff, vec![shaped.to_string()]);
        seed(&pool, &ruff, ConnectionState::Ready, &[shaped]).await;

        assert_reached_downstream(pool.dispatch_execute_command(
            params(shaped),
            &settings_with_servers(&["ruff", "srv"]),
            None,
        ))
        .await;
    }

    #[tokio::test]
    async fn an_ordinary_raw_name_still_takes_the_palette_path() {
        // The guard must fire only on the genuine collision. A raw name that
        // does not decode is not ambiguous with anything.
        let pool = LanguageServerPool::new();
        let ruff = ConnectionKey::new("ruff", None);
        pool.command_origins()
            .register(&ruff, vec!["ruff.fix".to_string()]);
        seed(&pool, &ruff, ConnectionState::Ready, &["ruff.fix"]).await;

        assert_reached_downstream(pool.dispatch_execute_command(
            params("ruff.fix"),
            &settings_with_servers(&["ruff"]),
            None,
        ))
        .await;
    }

    #[tokio::test]
    async fn a_deleted_server_does_not_veto_the_origin_that_survives_it() {
        // Both origins are revivable in shape, but only one names a server the
        // current config can still spawn — so the set is not really ambiguous.
        // Counting the deleted one would refuse the command permanently, since
        // nothing ever removes it from the registry.
        let pool = LanguageServerPool::new();
        let ruff = ConnectionKey::new("ruff", None);
        pool.command_origins()
            .register(&ruff, vec!["source.fixAll".to_string()]);
        pool.command_origins().register(
            &ConnectionKey::new("deleted", None),
            vec!["source.fixAll".to_string()],
        );

        let settings = settings_with_servers(&["ruff"]);
        // `true` exits immediately, so the revive cannot succeed either way;
        // what this pins is that the dispatcher got as far as TRYING to revive
        // `ruff` rather than refusing on a phantom collision. A refusal never
        // reaches the pool at all.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            pool.dispatch_palette_command(params("source.fixAll"), &settings, None),
        )
        .await;
        assert!(
            !pool.connections().await.is_empty(),
            "a server removed from config must not out-vote the one still configured"
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_an_unknown_name_without_scanning() {
        let pool = LanguageServerPool::new();
        seed(
            &pool,
            &key("ruff", "file:///w/a"),
            ConnectionState::Ready,
            &["source.fixAll"],
        )
        .await;

        let settings = settings_with_servers(&["ruff"]);
        assert!(
            dispatch(&pool, &settings, "source.fixAll").await.is_none(),
            "a live advertiser the editor was never told about is not a palette command"
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_when_nothing_is_live_and_several_origins_are_recorded() {
        let pool = LanguageServerPool::new();
        // Two client-fallback origins: both revivable, so neither is the answer.
        pool.command_origins().register(
            &ConnectionKey::new("ruff", None),
            vec!["source.fixAll".to_string()],
        );
        pool.command_origins().register(
            &ConnectionKey::new("eslint", None),
            vec!["source.fixAll".to_string()],
        );

        let settings = settings_with_servers(&["ruff", "eslint"]);
        assert!(dispatch(&pool, &settings, "source.fixAll").await.is_none());
        assert!(
            pool.connections().await.is_empty(),
            "refusing must not spawn one of the candidates"
        );
    }

    #[tokio::test]
    async fn a_marker_rooted_origin_does_not_veto_the_revivable_one() {
        // The #628 regression: a key the reconnect branch can never revive used
        // to count toward the cardinality test and refuse the key that works.
        let pool = LanguageServerPool::new();
        let registry = pool.command_origins();
        registry.register(
            &key("ruff", "file:///w/a"),
            vec!["source.fixAll".to_string()],
        );
        registry.register(
            &ConnectionKey::new("ruff", None),
            vec!["source.fixAll".to_string()],
        );

        assert_eq!(
            registry.reconnectable_origins("source.fixAll"),
            vec![ConnectionKey::new("ruff", None)],
        );
        assert_eq!(
            select_palette_route(&[], &registry.reconnectable_origins("source.fixAll")),
            PaletteRoute::Reconnect(ConnectionKey::new("ruff", None)),
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
