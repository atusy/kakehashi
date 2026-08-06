//! `completionItem/resolve`: route the request to the single downstream server
//! identified by the Kakehashi envelope stored in `CompletionItem.data` during
//! the original completion fan-out. Uses `send_request()` for FIFO ordering
//! through the single writer task (ls-bridge-message-ordering).
//!
//! No document sync is sent here — `completionItem/resolve` carries no
//! `textDocument`, and the completion that produced the item already opened
//! the virtual document (virt) or synced the host one (host). If the
//! downstream server has since restarted (or the connection was recreated),
//! the resolve fails and we return the unresolved item with envelope intact:
//! graceful degradation.
//!
//! Two paths share that degradation. The VIRT path translates coordinates and
//! additionally serves the unresolved item when the resolved primary edit is
//! unsafe for the injection region — escapes it, breaks per-line prefixes, or
//! merges content into the closing fence (see `resolve_guard_region_end`). The
//! HOST path (#958) forwards verbatim: its item is already in host
//! coordinates, so neither the translation nor that region guard applies.

use std::sync::Arc;

use log::warn;
use tower_lsp_server::ls_types::{CompletionItem, Position};
use url::Url;

use super::super::pool::{ConnectionHandle, LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, response_has_jsonrpc_error,
    translate_host_range_to_virtual,
};
use super::completion::{
    EnvelopeContext, KakehashiEnvelope, envelope_item_data, strip_envelope,
    transform_completion_item,
};
use crate::config::settings::WorkspaceSettings;
use crate::config::{
    merge_bridge_server_configs, resolve_with_wildcard, settings::BridgeServerConfig,
};
use crate::lsp::bridge::actor::RouterCleanupGuard;

impl LanguageServerPool {
    /// Route a `completionItem/resolve` request to the origin downstream server.
    ///
    /// Strips the Kakehashi envelope to identify the origin server, looks up
    /// the server config from `settings`, and delegates to whichever path the
    /// envelope's layer selects: `send_host_completion_resolve` (verbatim) for
    /// a host-layer item, `send_completion_resolve_request` (coordinate
    /// translation + region guard) otherwise. If any routing step fails (no
    /// envelope, server not configured), the item is returned as-is.
    pub(crate) async fn dispatch_completion_resolve(
        &self,
        mut item: CompletionItem,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> CompletionItem {
        // Extract envelope — if absent, this item wasn't produced by Kakehashi
        let Some(envelope) = strip_envelope(&mut item) else {
            return item;
        };

        // Look up the server config for the origin server. A server that is
        // no longer configured, or that the user has since disabled, must
        // not be respawned just to resolve a stale item. Check the
        // allocation-free predicate first to fail fast, before paying for
        // resolve_with_wildcard's full config clone/merge.
        if !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin) {
            re_envelope_item(&mut item, &envelope);
            return item;
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        ) else {
            // Structurally unreachable: is_server_spawnable already
            // confirmed the origin key exists (and isn't the wildcard), so
            // resolve_with_wildcard's only None case (both wildcard and
            // specific missing) can't happen here. Kept as a defensive
            // fallback rather than an unwrap.
            re_envelope_item(&mut item, &envelope);
            return item;
        };

        // Host-layer items are already in host coordinates — route their
        // resolve to the host server VERBATIM (no translation, #958). A genuine
        // host envelope has no region identity; requiring that blocks a client
        // flipping `host_layer` on a virt envelope to skip translation.
        if envelope.is_host_layer() {
            return self
                .send_host_completion_resolve(&config, item, envelope, upstream_id)
                .await;
        }

        self.send_completion_resolve_request(&config, item, envelope, upstream_id)
            .await
    }

    /// Route a HOST-layer `completionItem/resolve` back to its host server
    /// VERBATIM: the item is already in host coordinates, so nothing is
    /// translated and the injection-region edit guard — which has no region to
    /// judge against here — does not run. Fails soft (returns the item
    /// unresolved, envelope restored) at every step.
    async fn send_host_completion_resolve(
        &self,
        server_config: &BridgeServerConfig,
        mut item: CompletionItem,
        envelope: KakehashiEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> CompletionItem {
        let server_name = &envelope.origin;
        // `host_uri` comes from client-supplied `data` (the resolve params echo
        // the item's envelope), so an unparseable value fails soft rather than
        // falling through to `get_or_create_connection(.., None)`, whose `None`
        // document hint routes to the rootless client-fallback key. The bridge
        // only ever mints a valid `Url::as_str()` here, so a parse failure means
        // a corrupt or foreign envelope. This rejects only UNPARSEABLE strings:
        // a well-formed non-file URL parses, then fails root resolution and
        // lands on that same fallback key anyway — the host path is fail-soft
        // throughout, so that costs a wasted round trip, not correctness.
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            warn!(
                target: "kakehashi::bridge",
                "completionItem/resolve (host): envelope host_uri '{}' is not a valid URL; ignoring",
                envelope.host_uri
            );
            re_envelope_item(&mut item, &envelope);
            return item;
        };
        let handle = match self
            .get_or_create_connection(server_name, server_config, Some(&host_url))
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve (host): failed to connect to {server_name}: {e}"
                );
                re_envelope_item(&mut item, &envelope);
                return item;
            }
        };
        if !handle.has_capability("completionItem/resolve") {
            // Anomalous, unlike on the virt path (which envelopes
            // unconditionally): a host envelope is minted ONLY for a server
            // that advertised resolve, so reaching here means a respawn
            // changed capabilities (or the handle is still initializing).
            warn!(
                target: "kakehashi::bridge",
                "completionItem/resolve: host server {server_name:?} no longer advertises \
                 resolveProvider; returning unresolved"
            );
            re_envelope_item(&mut item, &envelope);
            return item;
        }

        // Host coordinates throughout: the item goes out as served (minus the
        // envelope, already stripped) and the resolved reply needs no
        // translation on the way back.
        match self
            .send_completion_resolve_on_handle(&handle, item.clone(), upstream_id)
            .await
        {
            Some(mut resolved) => {
                re_envelope_item(&mut resolved, &envelope);
                resolved
            }
            None => {
                re_envelope_item(&mut item, &envelope);
                item
            }
        }
    }

    /// Send a `completionItem/resolve` request to the downstream server that
    /// produced the item, re-enveloping the resolved item for return to the client.
    ///
    /// Always returns the item. All failure modes (connection error, timeout, parse
    /// failure, missing capability) return the original item with its envelope
    /// restored so the client can still use the basic completion item.
    async fn send_completion_resolve_request(
        &self,
        server_config: &BridgeServerConfig,
        mut item: CompletionItem,
        envelope: KakehashiEnvelope,
        upstream_id: Option<UpstreamId>,
    ) -> CompletionItem {
        let server_name = &envelope.origin;
        // Route to the SAME `(server, root)` connection the completion request
        // ran on (#382): the envelope carries the originating host URI, which
        // resolves to the same connection key. Without it (legacy envelope with
        // an empty host_uri), this falls back to the server's client-root
        // connection — a different process in a multi-root monorepo. The origin
        // is normally already pooled by the completion request that produced the
        // item; only if it died in between does this respawn.
        let host_uri = Url::parse(&envelope.host_uri).ok();
        let handle = match self
            .get_or_create_connection(server_name, server_config, host_uri.as_ref())
            .await
        {
            Ok(h) => h,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve: failed to connect to {}: {}",
                    server_name, e
                );
                re_envelope_item(&mut item, &envelope);
                return item;
            }
        };

        if !handle.has_capability("completionItem/resolve") {
            re_envelope_item(&mut item, &envelope);
            return item;
        }

        // The original host-coordinate `item` is kept untouched for the
        // fail-soft returns below; the outgoing clone carries virtual ranges.
        let outgoing = prepare_completion_resolve_item(&item, &envelope);

        match self
            .send_completion_resolve_on_handle(&handle, outgoing, upstream_id)
            .await
        {
            Some(mut resolved) => {
                let offset = RegionOffset::from(&envelope.offset);
                let region_end = resolve_guard_region_end(&envelope, &offset);
                if transform_completion_item(&mut resolved, &offset, region_end, None) {
                    re_envelope_item(&mut resolved, &envelope);
                    resolved
                } else {
                    // The resolved primary edit is unsafe for the injection
                    // region — serve the original (already host-translated,
                    // guard-passed) item instead of a corrupting one.
                    warn!(
                        target: "kakehashi::bridge",
                        "completionItem/resolve: resolved item from {} carries an edit unsafe for the injection region; serving unresolved item",
                        server_name
                    );
                    re_envelope_item(&mut item, &envelope);
                    item
                }
            }
            None => {
                re_envelope_item(&mut item, &envelope);
                item
            }
        }
    }

    /// Transport half of `completionItem/resolve`, shared by the virt and host
    /// paths: register for cancel forwarding, send `outgoing` on `handle`, and
    /// parse the reply. `None` on every failure (registration, send, transport,
    /// error/malformed response) — the callers own the fail-soft policy of
    /// returning the unresolved item with its envelope restored. The upstream
    /// registry entry is removed on every `return`; a future DROPPED at the
    /// response await leaks it, since (unlike the layer-walk arms) this has no
    /// `UpstreamRegistrySweepGuard` above it — `completion_resolve_impl` calls
    /// the dispatch directly rather than through `run_layer_race`. Pre-existing
    /// and unchanged by the host path; noted so the claim is not read as RAII.
    async fn send_completion_resolve_on_handle(
        &self,
        handle: &Arc<ConnectionHandle>,
        outgoing: CompletionItem,
        upstream_id: Option<UpstreamId>,
    ) -> Option<CompletionItem> {
        // Route per-connection cancel state by this handle's pool key (#382) —
        // the same connection the completion ran on, recovered from the
        // envelope's host URI by the caller.
        let connection_key = handle.key();

        // Register in the upstream request registry FIRST for cancel lookup.
        if let Some(ref id) = upstream_id {
            self.register_upstream_request(id.clone(), connection_key);
        }

        let (request_id, response_rx) = match handle
            .register_request_with_upstream(upstream_id.clone())
        {
            Ok(pair) => pair,
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve: failed to register request on {connection_key:?}: {e}"
                );
                if let Some(ref id) = upstream_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return None;
            }
        };

        let request = build_completion_resolve_request(outgoing, request_id);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        if let Err(e) = handle.send_request(request, request_id) {
            warn!(
                target: "kakehashi::bridge",
                "completionItem/resolve: failed to send request on {connection_key:?}: {e}"
            );
            if let Some(ref id) = upstream_id {
                self.unregister_upstream_request(id, connection_key);
            }
            return None;
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        // Unregister from the upstream request registry regardless of result
        if let Some(ref id) = upstream_id {
            self.unregister_upstream_request(id, connection_key);
        }

        match response {
            Ok(response) => parse_completion_resolve_response(response),
            Err(e) => {
                warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve failed on {connection_key:?}: {e}"
                );
                None
            }
        }
    }
}

/// Build a JSON-RPC `completionItem/resolve` request.
///
/// The `completionItem/resolve` method is unique: its params is the
/// `CompletionItem` itself (not a wrapper struct), per the LSP spec.
fn build_completion_resolve_request(
    item: CompletionItem,
    request_id: RequestId,
) -> JsonRpcRequest<CompletionItem> {
    JsonRpcRequest::new(request_id.as_i64(), "completionItem/resolve", item)
}

/// Parse a JSON-RPC resolve response into a `CompletionItem`.
///
/// Returns `None` for null results, missing results, and deserialization failures.
fn parse_completion_resolve_response(mut response: serde_json::Value) -> Option<CompletionItem> {
    if response_has_jsonrpc_error(&response, "completionItem/resolve") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    serde_json::from_value(result).ok()
}

/// Build the outgoing `completionItem/resolve` item from a SERVED one:
/// a clone with its edit ranges restored to VIRTUAL coordinates (the served
/// item is host-translated; a downstream that echoes the ranges verbatim
/// would otherwise get them re-translated on the way back — a double shift
/// the safety guard can't always catch). Mirrors the codeAction resolve path.
/// The HOST path has no such translation to undo and skips this entirely.
fn prepare_completion_resolve_item(
    item: &CompletionItem,
    envelope: &KakehashiEnvelope,
) -> CompletionItem {
    let mut outgoing = item.clone();
    translate_item_ranges_host_to_virtual(&mut outgoing, &RegionOffset::from(&envelope.offset));
    outgoing
}

/// Translate a served (host-coordinate) item's edit ranges back to virtual
/// coordinates before forwarding it in a `completionItem/resolve` request —
/// the inverse of `transform_completion_item`'s range translation.
fn translate_item_ranges_host_to_virtual(item: &mut CompletionItem, offset: &RegionOffset) {
    if let Some(text_edit) = &mut item.text_edit {
        match text_edit {
            tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit) => {
                translate_host_range_to_virtual(&mut edit.range, offset);
            }
            tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(edit) => {
                translate_host_range_to_virtual(&mut edit.insert, offset);
                translate_host_range_to_virtual(&mut edit.replace, offset);
            }
        }
    }
    if let Some(additional_edits) = &mut item.additional_text_edits {
        for edit in additional_edits.iter_mut() {
            translate_host_range_to_virtual(&mut edit.range, offset);
        }
    }
}

/// Restore the envelope into a resolved item's `data` field.
///
/// The resolved item may have its own `data` (from the downstream's resolve
/// response). We wrap it again so that future resolves can still be routed.
fn re_envelope_item(item: &mut CompletionItem, envelope: &KakehashiEnvelope) {
    let ctx = EnvelopeContext {
        server_name: &envelope.origin,
        host_uri: &envelope.host_uri,
        region_id: &envelope.region_id,
        offset: &RegionOffset::from(&envelope.offset),
        region_end: envelope
            .region_end
            .map(|(line, character)| Position { line, character }),
        // Preserve the layer: a host-layer item that has been resolved once
        // must still take the host path on the client's NEXT resolve of it.
        host_layer: envelope.host_layer,
    };
    envelope_item_data(item, &ctx);
}

/// The `region_end` the resolve-path prefix guard runs with.
///
/// Known limitation (pre-existing class, shared with the envelope's `offset`
/// itself, which has translated resolve responses since #382): both are
/// completion-time snapshots round-tripped through the client, so an edit
/// arriving after the region moved translates against stale geometry. A live
/// re-resolution needs a region identity the envelope doesn't carry — the
/// codeAction path's freshness gate is the model if this ever bites. Normally the
/// envelope carries the completion-time snapshot verbatim. A LEGACY envelope
/// (minted before the field existed) has none, and the resolve path cannot
/// recompute it — fall back to `(region start line, character 0)`, which is
/// fully fail-closed: with `character == 0` the guard's boundary rule rejects
/// every edit at or past the region start in per-line-prefixed regions (the
/// unresolved item is served instead); unprefixed regions skip the prefix
/// rules but stay subject to containment and the fence-boundary EOL rule,
/// both fail-closed under this anchor. A permissive sentinel would disable the
/// boundary rule entirely and let fence-row edits through — never trade that
/// for fewer over-strips on envelopes that disappear after one session.
fn resolve_guard_region_end(envelope: &KakehashiEnvelope, offset: &RegionOffset) -> Position {
    envelope
        .region_end
        .map(|(line, character)| Position { line, character })
        .unwrap_or(Position {
            line: offset.line(),
            character: 0,
        })
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
            host_uri: "file:///test/doc.md".to_string(),
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            inner: Some(json!({"resolve_id": 99})),
            offset: EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: None,
            },
            region_end: Some((9, 0)),
            host_layer: false,
        }
    }

    // ==========================================================================
    // resolve_guard_region_end tests
    // ==========================================================================

    #[test]
    fn guard_region_end_uses_the_envelope_snapshot_when_carried() {
        let envelope = test_envelope();
        let offset = RegionOffset::from(&envelope.offset);
        assert_eq!(
            resolve_guard_region_end(&envelope, &offset),
            Position {
                line: 9,
                character: 0
            },
        );
    }

    #[test]
    fn guard_region_end_falls_back_fail_closed_for_legacy_envelopes() {
        let mut envelope = test_envelope();
        envelope.region_end = None;
        envelope.offset.line = 3;
        envelope.offset.line_column_offsets = Some(vec![2, 2, 0]);
        let offset = RegionOffset::from(&envelope.offset);

        let region_end = resolve_guard_region_end(&envelope, &offset);
        assert_eq!(
            region_end,
            Position {
                line: 3,
                character: 0
            },
            "legacy fallback anchors the boundary at the region start"
        );

        // Fail-closed: with character 0 the guard's boundary rule rejects even
        // a same-line, newline-free edit in this per-line-prefixed region —
        // the resolve serves the unresolved item rather than guessing where
        // the region really ends.
        let mut resolved = CompletionItem {
            label: "x".into(),
            text_edit: Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(
                tower_lsp_server::ls_types::TextEdit {
                    range: tower_lsp_server::ls_types::Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 3,
                        },
                    },
                    new_text: "single".into(),
                },
            )),
            ..Default::default()
        };
        assert!(
            !transform_completion_item(&mut resolved, &offset, region_end, None),
            "prefixed-region edits must be rejected under the legacy fallback"
        );
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
        let request = build_completion_resolve_request(item, RequestId::new(7));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7i64);
        assert_eq!(json["method"], "completionItem/resolve");
        assert_eq!(json["params"]["label"], "print");
        assert_eq!(json["params"]["data"]["resolve_id"], 99);
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
        // After round-trip (EnvelopeOffset → RegionOffset → EnvelopeOffset),
        // line_column_offsets is always populated.
        assert_eq!(
            extracted.offset,
            EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: Some(vec![0])
            }
        );
    }

    /// A host-layer item resolved ONCE must still route through the host path
    /// on the client's next resolve of it: `re_envelope_item` has to carry the
    /// layer marker across, or the second resolve silently takes the virt path
    /// and fails closed on a region that never existed. Two round trips —
    /// a single one passes even when the marker is dropped.
    #[test]
    fn re_envelope_preserves_the_host_layer_marker() {
        let mut item = CompletionItem {
            label: "./test".to_string(),
            ..Default::default()
        };
        crate::lsp::bridge::text_document::completion::envelope_host_item(
            &mut item,
            "tsudoi-ls",
            "file:///test/doc.txt",
        );

        for round in 1..=2 {
            let envelope = strip_envelope(&mut item).expect("should strip");
            assert!(
                envelope.is_host_layer(),
                "envelope must stay host-layer through resolve round {round}"
            );
            re_envelope_item(&mut item, &envelope);
        }
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
    // dispatch_completion_resolve integration tests
    // ==========================================================================

    /// Helper to create a completion item with a Kakehashi envelope.
    fn enveloped_item(server: &str) -> CompletionItem {
        let envelope = KakehashiEnvelope {
            origin: server.to_string(),
            host_uri: "file:///test/doc.md".to_string(),
            region_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
            inner: Some(json!({"resolve_id": 42})),
            offset: EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: None,
            },
            region_end: Some((9, 0)),
            host_layer: false,
        };
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"resolve_id": 42})),
            ..Default::default()
        };
        re_envelope_item(&mut item, &envelope);
        item
    }

    /// dispatch returns item unchanged when it has no envelope.
    #[tokio::test]
    async fn dispatch_returns_non_envelope_item_unchanged() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default();
        let item = CompletionItem {
            label: "plain".to_string(),
            data: Some(json!({"custom": true})),
            ..Default::default()
        };

        let result = pool
            .dispatch_completion_resolve(item.clone(), &settings, None)
            .await;
        assert_eq!(result.label, "plain");
        assert_eq!(result.data, Some(json!({"custom": true})));
    }

    /// dispatch re-envelopes and returns item when origin server is not in settings.
    #[tokio::test]
    async fn dispatch_re_envelopes_when_server_not_configured() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default(); // no language_servers configured

        let item = enveloped_item("nonexistent-ls");
        let result = pool
            .dispatch_completion_resolve(item, &settings, None)
            .await;

        // Should be re-enveloped (routing info preserved for future attempts)
        let envelope = extract_envelope(&result).expect("should have envelope");
        assert_eq!(envelope.origin, "nonexistent-ls");
    }

    /// dispatch must not respawn a server the user has since disabled just
    /// to resolve a stale completion item — it should degrade the same way
    /// as "server not configured" (re-envelope, return unresolved).
    #[tokio::test]
    async fn dispatch_re_envelopes_when_origin_server_disabled() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "lua-ls".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["lua-language-server".to_string()]),
                enabled: Some(false),
                ..Default::default()
            },
        );

        let item = enveloped_item("lua-ls");
        let result = pool
            .dispatch_completion_resolve(item, &settings, None)
            .await;

        let envelope = extract_envelope(&result).expect("should have envelope");
        assert_eq!(
            envelope.origin, "lua-ls",
            "a disabled server's item is returned unresolved, not respawned"
        );
    }

    /// A HOST-layer item whose origin cannot be reached comes back unresolved
    /// with the layer marker intact, so the client's next resolve still routes
    /// through the host path instead of falling into the virt geometry gate.
    #[tokio::test]
    async fn dispatch_re_envelopes_host_item_when_server_not_configured() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let settings = WorkspaceSettings::default();
        let mut item = CompletionItem {
            label: "./test".to_string(),
            ..Default::default()
        };
        crate::lsp::bridge::text_document::completion::envelope_host_item(
            &mut item,
            "tsudoi-ls",
            "file:///test/doc.txt",
        );

        let result = pool
            .dispatch_completion_resolve(item, &settings, None)
            .await;

        let envelope = extract_envelope(&result).expect("should have envelope");
        assert_eq!(envelope.origin, "tsudoi-ls");
        assert!(envelope.is_host_layer());
    }

    /// The disabled gate must also apply when the server inherits `enabled:
    /// false` from the `_` wildcard rather than setting it directly.
    #[tokio::test]
    async fn dispatch_re_envelopes_when_origin_server_disabled_via_wildcard() {
        let pool = std::sync::Arc::new(LanguageServerPool::new());
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "_".to_string(),
            BridgeServerConfig {
                enabled: Some(false),
                ..Default::default()
            },
        );
        settings.language_servers.insert(
            "lua-ls".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["lua-language-server".to_string()]),
                ..Default::default()
            },
        );

        let item = enveloped_item("lua-ls");
        let result = pool
            .dispatch_completion_resolve(item, &settings, None)
            .await;

        let envelope = extract_envelope(&result).expect("should have envelope");
        assert_eq!(
            envelope.origin, "lua-ls",
            "a server disabled via the wildcard default is not respawned either"
        );
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
        // After round-trip, line_column_offsets is always populated.
        assert_eq!(
            final_envelope.offset,
            EnvelopeOffset {
                line: 5,
                column: 0,
                line_column_offsets: Some(vec![0])
            }
        );
    }

    #[test]
    fn resolve_forwards_edit_ranges_in_virtual_coordinates() {
        // The served item is host-translated (region at host line 5); the
        // outgoing resolve must restore virtual coordinates or a downstream
        // echoing the ranges verbatim gets them double-shifted on return.
        let item = CompletionItem {
            label: "print".to_string(),
            text_edit: Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(
                tower_lsp_server::ls_types::TextEdit {
                    range: tower_lsp_server::ls_types::Range {
                        start: Position {
                            line: 5,
                            character: 2,
                        },
                        end: Position {
                            line: 5,
                            character: 7,
                        },
                    },
                    new_text: "print".to_string(),
                },
            )),
            ..Default::default()
        };
        let envelope = test_envelope();

        // Go through the SAME request-preparation helper production uses, and
        // assert on the serialized request.
        let request = build_completion_resolve_request(
            prepare_completion_resolve_item(&item, &envelope),
            RequestId::new(7),
        );
        let wire = serde_json::to_value(&request).unwrap();

        assert_eq!(
            wire["params"]["textEdit"]["range"]["start"]["line"], 0,
            "the resolve request must carry VIRTUAL coordinates (host 5 → virtual 0): {wire}"
        );
        assert_eq!(wire["params"]["textEdit"]["range"]["end"]["line"], 0);
        // The served item stays host-translated for the fail-soft returns.
        let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit)) = &item.text_edit
        else {
            panic!("edit variant preserved");
        };
        assert_eq!(edit.range.start.line, 5);
    }
}
