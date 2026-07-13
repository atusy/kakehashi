//! Host-document bridge requests (host-document-bridge).
//!
//! Unlike the virt path, the host path forwards the **real client URI**, the
//! **host text verbatim**, and applies **no coordinate translation** in either
//! direction — the response is the downstream server's answer about the very
//! document the editor sees. Because of that, host requests need no
//! per-method request builders or response transformers: the upstream
//! request's params are forwarded as raw JSON
//! ([`LanguageServerPool::send_host_raw_request`]) and the result comes back
//! verbatim. The pool's `(uri, connection key)` document state and the
//! request/cancel machinery are shared with the virt path; only the document
//! key and the (absent) translation differ.
//!
//! Document sync is lazy: the host document is opened on the first request
//! per `(uri, connection)` and re-synced with a full-text `didChange` whenever
//! the host text changed since the last request (fingerprint comparison) —
//! the same full-content sync the virt path uses for its `didChange`
//! forwarding.

use std::collections::hash_map::Entry;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::Arc;

use tower_lsp_server::ls_types::{
    Diagnostic, DidChangeTextDocumentParams, DidOpenTextDocumentParams, Location, LocationLink,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, TextEdit, Uri,
    VersionedTextDocumentIdentifier,
};
use url::Url;

use super::super::actor::RouterCleanupGuard;
use super::super::pool::{
    ConnectionHandle, ConnectionHandleSender, ConnectionKey, HostDocSyncState, LanguageServerPool,
    MessageSender, UpstreamId,
};
use super::super::protocol::{
    JsonRpcNotification, JsonRpcRequest, RequestId, jsonrpc_error_summary,
    response_has_jsonrpc_error,
};
use crate::config::settings::BridgeServerConfig;

/// The host document a request operates on: real URI, host language id, and
/// the current host text (verbatim).
pub(crate) struct HostDocument<'a> {
    pub(crate) uri: &'a Url,
    pub(crate) language_id: &'a str,
    pub(crate) text: &'a str,
}

fn fingerprint(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Owned reader of a host document's current text, shared across the eager
/// re-sync's per-server tasks. The eager on-edit re-sync builds one (capturing
/// the document store + URI) and hands a clone to each task; the task passes it
/// to [`sync_host_document`], which evaluates it under the `host_documents` lock
/// so a late task sends the latest text rather than a stale snapshot (#422).
pub(crate) type HostTextReader = Arc<dyn Fn() -> Option<Arc<str>> + Send + Sync>;

/// Open or re-sync the host document on a downstream server, mutating the
/// pool's sync-state map in place.
///
/// - First request for `(uri, connection)`: send `didOpen` with the real URI,
///   the host language id, and the full host text.
/// - Host text changed since the last sync: send a full-text `didChange`
///   with an incremented version.
/// - Unchanged: no-op.
///
/// The caller holds the `host_documents` lock across this call (and across
/// the request enqueue that follows it), so concurrent requests cannot
/// double-open and cannot interleave a different text between a sync and the
/// request that relies on it; the single-writer loop
/// (ls-bridge-message-ordering) guarantees wire order matches enqueue order.
/// Generic over [`MessageSender`] so tests can observe the notifications via
/// a channel.
///
/// `live_text_reader`, when `Some`, supplies the document's **current** text,
/// read here under the `host_documents` lock instead of trusting `doc.text`.
/// The eager on-edit re-sync passes it so a sync task that unparked late sends
/// the *latest* text rather than the schedule-time snapshot it was spawned with
/// — a snapshot a newer edit may have superseded (host-sync-stale-overwrite,
/// #422). Reading under the same lock that gates the fingerprint compare and the
/// send means the last task to take the lock observes the newest text, so
/// concurrent re-syncs can no longer roll a host server back to stale content; a
/// `None` return (document closed mid-sync) falls back to `doc.text`. Interactive
/// requests, the formatting pipeline's speculative scratch text, and the initial
/// `didOpen` pass `None` and are synced verbatim.
pub(super) async fn sync_host_document<S: MessageSender>(
    sender: &mut S,
    docs: &mut std::collections::HashMap<(String, ConnectionKey), HostDocSyncState>,
    doc: &HostDocument<'_>,
    live_text_reader: Option<&(dyn Fn() -> Option<Arc<str>> + Send + Sync)>,
    connection_key: &ConnectionKey,
) -> io::Result<()> {
    let uri_lsp = host_url_to_lsp_uri(doc.uri)?;
    let key = (doc.uri.to_string(), connection_key.clone());
    // With a live reader, read the document's CURRENT text under this lock so a
    // late-unparking eager re-sync sends the latest text, not the snapshot it was
    // spawned with (#422); a `None` read (closed mid-sync) falls back to the
    // snapshot. The effective text drives both the fingerprint dedup and the sent
    // content, so re-syncing the same current text stays a no-op.
    let live = live_text_reader.and_then(|read| read());
    let text: &str = live.as_deref().unwrap_or(doc.text);
    let fp = fingerprint(text);

    match docs.entry(key) {
        Entry::Vacant(entry) => {
            let notification = JsonRpcNotification::new(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem::new(
                        uri_lsp,
                        doc.language_id.to_string(),
                        1,
                        text.to_string(),
                    ),
                },
            );
            sender.send_notification(notification).await?;
            entry.insert(HostDocSyncState {
                version: 1,
                fingerprint: fp,
            });
        }
        Entry::Occupied(mut entry) => {
            if entry.get().fingerprint != fp {
                let version = entry.get().version + 1;
                let notification = JsonRpcNotification::new(
                    "textDocument/didChange",
                    DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier::new(uri_lsp, version),
                        content_changes: vec![TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: text.to_string(),
                        }],
                    },
                );
                sender.send_notification(notification).await?;
                *entry.get_mut() = HostDocSyncState {
                    version,
                    fingerprint: fp,
                };
            }
        }
    }
    Ok(())
}

impl LanguageServerPool {
    /// Send `didClose` for the host document to every server that has it
    /// open via the host bridge, and drop the sync state.
    ///
    /// Connection handles are pre-fetched before taking the `host_documents`
    /// lock (consistent `connections` → `host_documents` ordering with the
    /// respawn purge in `pool.rs`), and the `didClose`s are queued while the
    /// lock is held: a concurrent request either re-opens before this runs —
    /// its `didOpen` precedes our `didClose` on the wire and its state entry
    /// is removed here — or after, sending a fresh `didOpen`. Either
    /// interleaving leaves the map and the server consistent; sending after
    /// releasing the lock could close a document another request had just
    /// re-opened while its state survives, wedging the pair.
    ///
    /// Mirrors the virt path's `close_host_document`; called from the
    /// upstream `didClose` handler.
    pub(crate) async fn close_host_bridge_document(&self, uri: &Url) {
        self.invalidate_diagnostic_host(uri);
        let Ok(uri_lsp) = host_url_to_lsp_uri(uri) else {
            return;
        };
        let uri_string = uri.to_string();

        let handles: Vec<(ConnectionKey, Arc<ConnectionHandle>)> = {
            let connections = self.connections().await;
            connections
                .iter()
                .map(|(key, handle)| (key.clone(), Arc::clone(handle)))
                .collect()
        };

        let mut docs = self.host_documents().await;
        for (connection_key, handle) in &handles {
            if !docs.contains_key(&(uri_string.clone(), connection_key.clone())) {
                continue;
            }
            let notification = JsonRpcNotification::new(
                "textDocument/didClose",
                DocumentIdentifierParams {
                    text_document: TextDocumentIdentifier {
                        uri: uri_lsp.clone(),
                    },
                },
            );
            handle.send_notification(notification);
        }
        let closed_keys = docs
            .keys()
            .filter(|(doc_uri, _)| doc_uri == &uri_string)
            .cloned()
            .map(|(doc_uri, connection_key)| (connection_key, doc_uri))
            .collect::<Vec<_>>();
        docs.retain(|(doc_uri, _), _| *doc_uri != uri_string);
        for key in closed_keys {
            self.invalidate_diagnostic_document(&key);
        }
    }

    /// Forward a `textDocument/willSave` notification (#357) to every host
    /// bridge server that **already has this host document open** and that
    /// advertises `willSave`.
    ///
    /// willSave concerns the host document the editor is about to save, so it
    /// is a verbatim passthrough (real URI, real `reason`) — no per-virtual-doc
    /// fan-out and no coordinate translation. Restricting to servers with the
    /// document already open mirrors the lazy host-document lifecycle: a server
    /// that never received a request never opened the document, so a willSave
    /// referencing an unknown document would be a protocol nuisance — and as a
    /// fire-and-forget notification it must not lazily spawn a connection just
    /// to announce a save (issue Q3, latency).
    ///
    /// Lock discipline follows the host **request** path
    /// ([`Self::execute_host_request`]), stricter than the pre-snapshot in
    /// [`Self::close_host_bridge_document`]: `connections` is held across the
    /// `host_documents` iteration (consistent `connections` → `host_documents`
    /// ordering with the respawn purge in `pool.rs`), and willSave is queued
    /// only onto the **live** handle for each key. A pre-snapshot would leave a
    /// window where a respawn between the snapshot and the send could queue
    /// willSave onto a replaced/dead connection; holding `connections`
    /// excludes the purge from interleaving, closing it (#357 review). The
    /// sends are non-yielding queue writes, so holding both locks is cheap.
    pub(crate) async fn notify_host_will_save(
        &self,
        uri: &Url,
        params: &tower_lsp_server::ls_types::WillSaveTextDocumentParams,
    ) {
        // `as_str()` is the already-serialized form; `.to_owned()` clones it in
        // one allocation, skipping the `Display`/`to_string` formatting path.
        let uri_string = uri.as_str().to_owned();

        let connections = self.connections().await;
        let docs = self.host_documents().await;
        for (connection_key, handle) in connections.iter() {
            if !docs.contains_key(&(uri_string.clone(), connection_key.clone())) {
                continue;
            }
            if !handle.has_capability("textDocument/willSave") {
                continue;
            }
            let notification = JsonRpcNotification::new("textDocument/willSave", params);
            handle.send_notification(notification);
        }
    }

    /// Send a host bridge request with the upstream params forwarded
    /// **verbatim** (host-document-bridge): the params already reference the
    /// real URI and real coordinates, so no per-method request shaping is
    /// needed. Returns the raw `result` value, `Ok(None)` for a `null`/absent
    /// result or a missing capability, and `Err` for a transport failure or a
    /// downstream JSON-RPC error response (a request failure, so a sink can
    /// count it).
    ///
    /// One exception to verbatim: the progress tokens are stripped. The
    /// bridge discards downstream notifications, so a server honoring
    /// `partialResultToken` would stream its results into the void and could
    /// legally return an empty final result; `workDoneToken` would likewise
    /// report progress nowhere.
    ///
    /// Fails fast on an initializing server (an empty layer for this request;
    /// the next request gets it) — the policy every interactive host-bridged
    /// method wants. The diagnostic path, which must instead wait through
    /// initialization, uses the dedicated [`Self::send_host_diagnostic_request`].
    pub(crate) async fn send_host_raw_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        doc: &HostDocument<'_>,
        method: &'static str,
        mut params: serde_json::Value,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<serde_json::Value>> {
        strip_progress_tokens(&mut params);
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(doc.uri))
            .await?;
        if !handle.has_capability(method) {
            return Ok(None);
        }
        self.execute_host_request(
            handle,
            doc,
            upstream_request_id,
            |request_id| JsonRpcRequest::new(request_id.as_i64(), method, params),
            move |response| parse_host_raw_response(response, method),
        )
        // Outer `?`: transport/protocol failure from `execute_host_request`.
        // Inner `Result` from the parser: a downstream JSON-RPC error response.
        // Both are request failures, so flatten them into one `Err`.
        .await?
    }

    /// Whether the host server for `(server_name, uri)` advertises `method`
    /// (e.g. `codeAction/resolve`). Reuses the existing connection when the
    /// request that ran just before already opened it, but still goes through
    /// `get_or_create_connection`, which re-resolves the marker/root and takes
    /// the pool lock even on a cache hit — cheap, not free, so callers gate the
    /// call when the answer can't change the outcome. Fail-closed (`false`) when
    /// the server can't be reached. Used to decide whether a host lazy action can
    /// be resolve-routed rather than disabled (#627).
    pub(crate) async fn host_server_advertises(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        uri: &url::Url,
        method: &str,
    ) -> bool {
        self.get_or_create_connection(server_name, server_config, Some(uri))
            .await
            .map(|handle| handle.has_capability(method))
            .unwrap_or(false)
    }

    /// Send a host formatting request. Unlike [`Self::send_host_raw_request`],
    /// the response keeps formatting's LSP semantics, mirroring
    /// [`Self::send_formatting_request`]: a `null` result from a capable
    /// server is the authoritative "no edits needed" signal
    /// (concatenated-formatting-pipeline) and comes back as `Ok(Some(vec![]))`;
    /// `Ok(None)` means the server does not advertise the capability (the
    /// only fallback-without-failure case); an error response, a success
    /// without a `result` member, or a malformed payload is a counted
    /// request failure (`Err`).
    pub(crate) async fn send_host_formatting_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        doc: &HostDocument<'_>,
        method: &'static str,
        params: serde_json::Value,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<TextEdit>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(doc.uri))
            .await?;
        if !handle.has_capability(method) {
            return Ok(None);
        }
        self.execute_host_request(
            handle,
            doc,
            upstream_request_id,
            |request_id| JsonRpcRequest::new(request_id.as_i64(), method, params),
            // The parser promotes error responses, missing results, and
            // malformed payloads to `Err` (request failure) — mirrors
            // `send_formatting_request`: the fan-in counts `Err`s, which CLI
            // mode maps onto its error exit code. Only the no-capability
            // early return above yields `Ok(None)`.
            move |response| parse_host_formatting_response(response, method).map(Some),
        )
        .await?
    }

    /// Send a host `textDocument/diagnostic` request with diagnostics' strict
    /// semantics. Unlike [`Self::send_host_raw_request`] (which collapses an
    /// absent `result`, a `null` result, and a missing capability into the same
    /// `Ok(None)`), this distinguishes them: a present-but-malformed report
    /// (error response, absent `result` member, unknown `kind`, a `full` report
    /// missing `items`, or items that fail to deserialize) is a counted request
    /// failure (`Err`), mirroring [`Self::send_host_formatting_request`]; a
    /// `null` is authoritative empty, while `unchanged` reuses the baseline
    /// established by the preceding full report. A server that does not
    /// advertise the capability stays the lenient empty `Ok(vec![])`. The
    /// dedicated method keeps that strictness out of the
    /// shared raw path that other host methods rely on.
    ///
    /// Waits through server initialization (the caller's request timeout bounds
    /// the wait) — returning empty while a server initializes would silently
    /// lose the host layer on the first pull, like the virt diagnostic path.
    ///
    /// The method is fixed to `textDocument/diagnostic`: this path is dedicated
    /// to the diagnostic-only parser and capability check, so it takes no
    /// `method` argument — a caller cannot accidentally route another method
    /// through it (the generic [`Self::send_host_raw_request`] is for that).
    pub(crate) async fn send_host_diagnostic_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        doc: &HostDocument<'_>,
        mut params: serde_json::Value,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Vec<Diagnostic>> {
        let method = "textDocument/diagnostic";
        let (host_generation, request_sequence) = self.begin_diagnostic_pull(doc.uri);
        let handle = self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
                Some(doc.uri),
                std::time::Duration::from_secs(super::super::pool::INIT_TIMEOUT_SECS),
            )
            .await?;
        if !handle.has_capability(method) {
            // No capability — the lenient empty layer (kept, unlike a malformed
            // payload), so a host server that simply does not pull contributes
            // nothing without looking like a failure.
            return Ok(Vec::new());
        }
        let cache_key = (handle.key().clone(), doc.uri.as_str().to_string());
        let snapshot = self.diagnostic_pull_snapshot(
            &cache_key,
            doc.uri.as_str(),
            host_generation,
            request_sequence,
        );
        let previous_result_id = snapshot
            .baseline
            .as_ref()
            .map(|entry| entry.result_id.clone());
        if let Some(result_id) = &previous_result_id {
            params["previousResultId"] = serde_json::Value::String(result_id.clone());
        }
        let report = self
            .execute_host_request(
                handle,
                doc,
                upstream_request_id,
                |request_id| JsonRpcRequest::new(request_id.as_i64(), method, params),
                move |response| {
                    if response_has_jsonrpc_error(&response, method) {
                        return Err(io::Error::other(format!(
                            "downstream server answered {method} with an error response: {}",
                            jsonrpc_error_summary(&response)
                        )));
                    }
                    super::diagnostic::parse_downstream_diagnostic_report(response)
                },
            )
            .await??;
        let document_is_open = self
            .host_documents()
            .await
            .contains_key(&(cache_key.1.clone(), cache_key.0.clone()));
        Ok(self.resolve_diagnostic_pull_report(&cache_key, snapshot, report, document_is_open))
    }

    /// Drive a host bridge request end-to-end: register for cancel
    /// forwarding, sync the host document, send, await, transform.
    ///
    /// The skeleton mirrors `execute_bridge_request_with_handle` minus the
    /// virtual URI and the coordinate translation — host responses are the
    /// downstream server's verbatim answer.
    async fn execute_host_request<T, P: serde::Serialize>(
        &self,
        handle: Arc<ConnectionHandle>,
        doc: &HostDocument<'_>,
        upstream_request_id: Option<UpstreamId>,
        build_request: impl FnOnce(RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value) -> T,
    ) -> io::Result<T> {
        // Route per-connection state by this handle's pool key (#382).
        let connection_key = handle.key();
        if let Some(ref id) = upstream_request_id {
            self.register_upstream_request(id.clone(), connection_key);
        }

        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_request_id.clone()) {
                Ok(result) => result,
                Err(e) => {
                    if let Some(ref id) = upstream_request_id {
                        self.unregister_upstream_request(id, connection_key);
                    }
                    return Err(e);
                }
            };

        let request = build_request(request_id);
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        // Sync the document and enqueue the request under ONE lock scope:
        // with the lock released in between, a concurrent host request could
        // slide its own full-text didChange between this request's sync and
        // the request itself, making the server answer about different text
        // than the caller synced (fatal for the formatting pipeline's
        // speculative intermediate text). All sends are non-yielding queue
        // writes, so holding the locks across them is cheap.
        //
        // The `connections` lock is held around the scope (consistent
        // connections → host_documents order, matching the respawn purge in
        // `pool.rs`) to verify `handle` is still the pool's LIVE connection:
        // `handle` was fetched earlier, and a concurrent respawn could have
        // replaced it and purged the sync state — syncing onto the dying
        // process's queue would record state the replacement never saw,
        // wedging the `(uri, connection)` pair until the next purge. Holding
        // `connections` here also excludes a purge from interleaving with
        // this sync, closing the race entirely.
        {
            let connections = self.connections().await;
            if !connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                drop(connections);
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("connection to {connection_key} was replaced during host sync"),
                ));
            }

            let mut docs = self.host_documents().await;
            let mut sender = ConnectionHandleSender(&handle);
            if let Err(e) =
                sync_host_document(&mut sender, &mut docs, doc, None, connection_key).await
            {
                drop(docs);
                drop(connections);
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(e);
            }

            if let Err(e) = handle.send_request(request, request_id) {
                drop(docs);
                drop(connections);
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(e.into());
            }
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        if let Some(ref id) = upstream_request_id {
            self.unregister_upstream_request(id, connection_key);
        }

        Ok(transform_response(response?))
    }
}

/// `textDocument/didClose` params (the bridge protocol layer has no shared
/// struct for notification-side document identifiers).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentIdentifierParams {
    text_document: TextDocumentIdentifier,
}

/// Remove the LSP progress tokens from forwarded params: the bridge discards
/// downstream notifications, so honoring `partialResultToken` downstream
/// would stream results into the void (and possibly return an empty final
/// result), and `workDoneToken` would report progress nowhere.
fn strip_progress_tokens(params: &mut serde_json::Value) {
    if let Some(object) = params.as_object_mut() {
        object.remove("workDoneToken");
        object.remove("partialResultToken");
    }
}

fn host_url_to_lsp_uri(uri: &Url) -> io::Result<Uri> {
    crate::lsp::lsp_impl::url_to_uri(uri)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Strip the JSON-RPC envelope: `Err` for an error response (a request
/// failure), `Ok(None)` for a `null` or absent result, and `Ok(Some(result))`
/// with the bare `result` value otherwise.
fn parse_host_raw_response(
    mut response: serde_json::Value,
    method: &'static str,
) -> io::Result<Option<serde_json::Value>> {
    // A downstream JSON-RPC error response is a request failure, not "no
    // result" — propagate it as `Err` so a request-error sink can surface it
    // (mirrors the formatting path). The generic host bridging caller
    // (`host_layer_value_with_ctx`) collapses `Err` and `Ok(None)` to the same
    // empty layer, so this only adds a WARNING log there; the host diagnostic
    // path uses the `Err` to count the failure for CLI mode's exit code.
    if response_has_jsonrpc_error(&response, method) {
        return Err(io::Error::other(format!(
            "downstream server answered {method} with an error response: {}",
            jsonrpc_error_summary(&response)
        )));
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        // No `result` and no `error` is a protocol violation, but kept as an
        // empty layer here (not `Err`) to preserve the long-standing raw-host
        // behavior for non-diagnostic methods.
        return Ok(None);
    };
    Ok(if result.is_null() { None } else { Some(result) })
}

/// Parse a host formatting response with formatting's null semantics:
/// `null` from a capable server = authoritative "no edits needed" →
/// `Ok(vec![])`. An error response, a success response without a `result`
/// member (protocol violation), or a malformed payload is a request failure
/// (`Err`), mirroring `transform_formatting_response_to_host` — collapsing
/// it into the no-capability value would let a broken host formatter pass
/// as "nothing to format".
fn parse_host_formatting_response(
    mut response: serde_json::Value,
    method: &'static str,
) -> io::Result<Vec<TextEdit>> {
    if response_has_jsonrpc_error(&response, method) {
        return Err(io::Error::other(format!(
            "downstream server answered {method} with an error response: {}",
            jsonrpc_error_summary(&response)
        )));
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        return Err(io::Error::other(format!(
            "{method} response carries neither result nor error (protocol violation)"
        )));
    };
    if result.is_null() {
        return Ok(Vec::new());
    }
    match serde_json::from_value::<Vec<TextEdit>>(result) {
        Ok(edits) => Ok(edits),
        Err(e) => {
            log::warn!(
                target: "kakehashi::bridge",
                "host formatting response failed to deserialize: {e}"
            );
            Err(io::Error::other(format!(
                "malformed {method} result from downstream server: {e}"
            )))
        }
    }
}

/// Parse a host `textDocument/diagnostic` response with diagnostics' strict
/// semantics, mirroring [`parse_host_formatting_response`]. An error response,
/// a success without a `result` member (protocol violation), or a present-but-
/// malformed report (an unknown `kind`, a `full` report missing `items`, or
/// `items` that fail to deserialize) is a request failure (`Err`) — collapsing
/// it into the empty layer would let a broken host server pass as "nothing
/// wrong". A `null` result or a spurious `unchanged` report without a cached
/// baseline is the authoritative empty `Ok(vec![])`. `relatedDocuments`
/// entries are dropped: diagnostics for OTHER documents have no place here.
#[cfg(test)]
fn parse_host_diagnostic_response(
    response: serde_json::Value,
    method: &'static str,
) -> io::Result<Vec<Diagnostic>> {
    if response_has_jsonrpc_error(&response, method) {
        return Err(io::Error::other(format!(
            "downstream server answered {method} with an error response: {}",
            jsonrpc_error_summary(&response)
        )));
    }
    match super::diagnostic::parse_downstream_diagnostic_report(response)? {
        super::diagnostic::DownstreamDiagnosticReport::Full { diagnostics, .. } => Ok(diagnostics),
        super::diagnostic::DownstreamDiagnosticReport::Unchanged { .. }
        | super::diagnostic::DownstreamDiagnosticReport::Null => Ok(Vec::new()),
    }
}

/// Normalize a goto result (`Location | Location[] | LocationLink[]`) to
/// `Vec<LocationLink>` **without** any URI or range rewriting — host
/// responses already speak real-document coordinates.
pub(crate) fn normalize_host_goto_result(result: serde_json::Value) -> Option<Vec<LocationLink>> {
    /// The three goto result shapes the LSP spec allows, deserialized in a
    /// single pass. `Links` must come first: a `LocationLink` array would
    /// also *partially* match the laxer shapes below.
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum GotoResult {
        Links(Vec<LocationLink>),
        Locations(Vec<Location>),
        Single(Location),
    }

    fn location_to_link(location: Location) -> LocationLink {
        LocationLink {
            origin_selection_range: None,
            target_uri: location.uri,
            target_range: location.range,
            target_selection_range: location.range,
        }
    }

    match serde_json::from_value::<GotoResult>(result) {
        Ok(GotoResult::Links(links)) => Some(links),
        Ok(GotoResult::Locations(locations)) => {
            Some(locations.into_iter().map(location_to_link).collect())
        }
        Ok(GotoResult::Single(location)) => Some(vec![location_to_link(location)]),
        Err(_) => {
            log::warn!(
                target: "kakehashi::bridge",
                "host goto response did not match Location | Location[] | LocationLink[]"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_response_strips_envelope_and_passes_result_verbatim() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{ "uri": "file:///doc.md", "range": {
                "start": { "line": 34, "character": 0 },
                "end": { "line": 34, "character": 11 } } }]
        });
        let result = parse_host_raw_response(response, "textDocument/definition")
            .expect("no request failure")
            .expect("result expected");
        assert_eq!(result[0]["uri"], "file:///doc.md");
        assert_eq!(
            result[0]["range"]["start"]["line"], 34,
            "host results must NOT be offset-translated"
        );
    }

    #[test]
    fn raw_response_null_is_none() {
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        assert!(
            parse_host_raw_response(response, "textDocument/hover")
                .expect("null result is not a failure")
                .is_none()
        );
    }

    #[test]
    fn raw_response_error_is_err() {
        // A JSON-RPC error response is a request failure, surfaced as `Err` so
        // a request-error sink (CLI diagnose) can count it.
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32603, "message": "boom" }
        });
        assert!(parse_host_raw_response(response, "textDocument/hover").is_err());
    }

    #[test]
    fn goto_normalization_passes_location_array_through_verbatim() {
        let result = serde_json::json!([{
            "uri": "file:///project/doc.md",
            "range": {
                "start": { "line": 34, "character": 0 },
                "end": { "line": 34, "character": 11 }
            }
        }]);
        let links = normalize_host_goto_result(result).expect("locations must parse");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri.as_str(), "file:///project/doc.md");
        assert_eq!(links[0].target_range.start.line, 34);
    }

    #[test]
    fn goto_normalization_handles_single_location() {
        let result = serde_json::json!({
            "uri": "file:///other.md",
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 5 }
            }
        });
        let links = normalize_host_goto_result(result).expect("single location must parse");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_selection_range.start.line, 1);
    }

    #[test]
    fn goto_normalization_handles_location_links() {
        let result = serde_json::json!([{
            "targetUri": "file:///doc.md",
            "targetRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 4 }
            },
            "targetSelectionRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 4 }
            }
        }]);
        let links = normalize_host_goto_result(result).expect("links must parse");
        assert_eq!(links[0].target_uri.as_str(), "file:///doc.md");
    }

    #[test]
    fn fingerprint_distinguishes_changed_text() {
        assert_eq!(fingerprint("a"), fingerprint("a"));
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }

    #[test]
    fn formatting_response_null_is_authoritative_no_edits() {
        // LSP formatting: a capable server's `null` means "no edits needed"
        // (concatenated-formatting-pipeline) — it must NOT read as "no usable
        // response", which would fall through to a lower-priority formatter.
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        let edits = parse_host_formatting_response(response, "textDocument/formatting")
            .expect("null must be an authoritative empty edit list");
        assert!(edits.is_empty());
    }

    #[test]
    fn formatting_response_edits_pass_through_verbatim() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": [{
                "range": { "start": { "line": 3, "character": 0 },
                           "end": { "line": 3, "character": 5 } },
                "newText": "fixed"
            }]
        });
        let edits =
            parse_host_formatting_response(response, "textDocument/formatting").expect("edits");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "fixed");
        assert_eq!(edits[0].range.start.line, 3, "no translation");
    }

    #[test]
    fn formatting_response_error_is_a_request_failure() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32603, "message": "boom" }
        });
        assert!(
            parse_host_formatting_response(response, "textDocument/formatting").is_err(),
            "errors must be a counted request failure, unlike null"
        );
    }

    #[test]
    fn host_diagnostic_full_report_yields_items() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "kind": "full",
                "items": [{
                    "range": { "start": { "line": 2, "character": 0 },
                               "end": { "line": 2, "character": 5 } },
                    "message": "host says hi"
                }]
            }
        });
        let items = parse_host_diagnostic_response(response, "textDocument/diagnostic")
            .expect("a full report is a valid response");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "host says hi");
        assert_eq!(
            items[0].range.start.line, 2,
            "no translation on the host path"
        );
    }

    #[test]
    fn host_diagnostic_null_and_spurious_unchanged_are_empty() {
        // This parser seam has no baseline. `null` and a volunteered
        // `unchanged` therefore stay empty; the live send path resolves a
        // legitimate unchanged response from its per-document cache.
        let null = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        assert!(
            parse_host_diagnostic_response(null, "textDocument/diagnostic")
                .expect("null is authoritative empty")
                .is_empty()
        );

        let unchanged = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "kind": "unchanged", "resultId": "x" }
        });
        assert!(
            parse_host_diagnostic_response(unchanged, "textDocument/diagnostic")
                .expect("spurious unchanged without a baseline stays empty")
                .is_empty()
        );
    }

    #[test]
    fn host_diagnostic_error_response_is_a_request_failure() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32603, "message": "boom" }
        });
        assert!(
            parse_host_diagnostic_response(response, "textDocument/diagnostic").is_err(),
            "an error response must be a counted request failure, unlike null"
        );
    }

    #[test]
    fn host_diagnostic_missing_result_is_a_request_failure() {
        // Neither `result` nor `error` — a protocol violation, not the lenient
        // empty that the shared raw path keeps for non-diagnostic methods.
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": 1 });
        assert!(parse_host_diagnostic_response(response, "textDocument/diagnostic").is_err());
    }

    #[test]
    fn host_diagnostic_malformed_payload_is_a_request_failure() {
        // A present, non-null `result` that is not a diagnostic report at all.
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": "nonsense" });
        assert!(parse_host_diagnostic_response(response, "textDocument/diagnostic").is_err());
    }

    #[test]
    fn host_diagnostic_unknown_kind_is_a_request_failure() {
        // The exit-2 behavior hinges on the tagged-enum deserialize REJECTING
        // an unrecognized report `kind` — assert it rather than infer it.
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "kind": "partial", "items": [] }
        });
        assert!(parse_host_diagnostic_response(response, "textDocument/diagnostic").is_err());
    }

    #[test]
    fn host_diagnostic_full_report_missing_items_is_a_request_failure() {
        // Likewise, a `full` report MUST carry `items`; the deserialize must
        // reject its absence.
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "kind": "full" }
        });
        assert!(parse_host_diagnostic_response(response, "textDocument/diagnostic").is_err());
    }

    #[test]
    fn strip_progress_tokens_removes_only_the_tokens() {
        let mut params = serde_json::json!({
            "textDocument": { "uri": "file:///doc.md" },
            "position": { "line": 1, "character": 2 },
            "workDoneToken": "wd-1",
            "partialResultToken": "pr-1",
        });
        strip_progress_tokens(&mut params);
        assert!(params.get("workDoneToken").is_none());
        assert!(params.get("partialResultToken").is_none());
        assert_eq!(params["textDocument"]["uri"], "file:///doc.md");
        assert_eq!(params["position"]["line"], 1);
    }

    fn host_doc<'a>(uri: &'a Url, text: &'a str) -> HostDocument<'a> {
        HostDocument {
            uri,
            language_id: "markdown",
            text,
        }
    }

    #[tokio::test]
    async fn sync_sends_didopen_once_then_versioned_didchange_on_drift() {
        use crate::lsp::bridge::actor::OutboundMessage;

        let mut docs = std::collections::HashMap::new();
        let (mut sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(16);
        let uri = Url::parse("file:///test/host.md").unwrap();

        // First sync: didOpen with the full text, version 1.
        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "v1"),
            None,
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();
        let msg = rx.try_recv().expect("didOpen must be queued");
        let OutboundMessage::Untracked(payload) = msg else {
            panic!("expected a notification");
        };
        assert_eq!(payload["method"], "textDocument/didOpen");
        assert_eq!(payload["params"]["textDocument"]["text"], "v1");
        assert_eq!(payload["params"]["textDocument"]["version"], 1);

        // Unchanged text: no notification at all.
        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "v1"),
            None,
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "unchanged text must be a no-op");

        // Drifted text: full-text didChange with version 2.
        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "v2"),
            None,
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();
        let OutboundMessage::Untracked(payload) = rx.try_recv().expect("didChange must be queued")
        else {
            panic!("expected a notification");
        };
        assert_eq!(payload["method"], "textDocument/didChange");
        assert_eq!(payload["params"]["textDocument"]["version"], 2);
        assert_eq!(payload["params"]["contentChanges"][0]["text"], "v2");

        // Drift again: version keeps increasing monotonically.
        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "v3"),
            None,
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();
        let OutboundMessage::Untracked(payload) = rx.try_recv().expect("didChange must be queued")
        else {
            panic!("expected a notification");
        };
        assert_eq!(payload["params"]["textDocument"]["version"], 3);
    }

    #[tokio::test]
    async fn sync_with_live_reader_sends_current_text_not_stale_snapshot() {
        use crate::lsp::bridge::actor::OutboundMessage;

        // The eager on-edit re-sync passes a live reader. sync_host_document must
        // send the reader's CURRENT text, not the (possibly superseded) snapshot
        // the task was spawned with — otherwise a late-unparking task rolls the
        // host server back to stale content (host-sync-stale-overwrite, #422).
        let mut docs = std::collections::HashMap::new();
        let (mut sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(16);
        let uri = Url::parse("file:///test/host.md").unwrap();

        let reader = || Some(Arc::<str>::from("fresh"));
        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "stale-snapshot"),
            Some(&reader),
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();

        let OutboundMessage::Untracked(payload) = rx.try_recv().expect("didOpen must be queued")
        else {
            panic!("expected a notification");
        };
        assert_eq!(payload["method"], "textDocument/didOpen");
        assert_eq!(
            payload["params"]["textDocument"]["text"], "fresh",
            "the live reader's current text must win over the stale snapshot"
        );
    }

    #[tokio::test]
    async fn sync_with_live_reader_dedups_unchanged_current_text() {
        use crate::lsp::bridge::actor::OutboundMessage;

        // Two re-syncs whose live reader returns the SAME current text: only the
        // first sends; the second is a fingerprint no-op. This is the user's
        // concern — multiple debounce fires must not each re-send the latest text.
        let mut docs = std::collections::HashMap::new();
        let (mut sender, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(16);
        let uri = Url::parse("file:///test/host.md").unwrap();
        let reader = || Some(Arc::<str>::from("current"));

        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "ignored"),
            Some(&reader),
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();
        let OutboundMessage::Untracked(payload) = rx.try_recv().expect("first sync must didOpen")
        else {
            panic!("expected a notification");
        };
        assert_eq!(payload["params"]["textDocument"]["text"], "current");

        // Second sync, same current text from the reader: fingerprint no-op.
        sync_host_document(
            &mut sender,
            &mut docs,
            &host_doc(&uri, "ignored"),
            Some(&reader),
            &ConnectionKey::for_server("srv"),
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "re-sync of the same current text must be a no-op (no redundant send)"
        );
    }

    #[tokio::test]
    async fn close_host_bridge_document_drops_only_that_uri() {
        let pool = LanguageServerPool::new();
        let uri_a = Url::parse("file:///test/a.md").unwrap();
        let uri_b = Url::parse("file:///test/b.md").unwrap();
        {
            let mut docs = pool.host_documents().await;
            docs.insert(
                (uri_a.to_string(), ConnectionKey::for_server("srv")),
                HostDocSyncState {
                    version: 1,
                    fingerprint: 1,
                },
            );
            docs.insert(
                (uri_b.to_string(), ConnectionKey::for_server("srv")),
                HostDocSyncState {
                    version: 1,
                    fingerprint: 2,
                },
            );
        }

        pool.close_host_bridge_document(&uri_a).await;

        let docs = pool.host_documents().await;
        assert!(
            !docs.contains_key(&(uri_a.to_string(), ConnectionKey::for_server("srv"))),
            "closed uri's state must be dropped"
        );
        assert!(
            docs.contains_key(&(uri_b.to_string(), ConnectionKey::for_server("srv"))),
            "other documents must be untouched"
        );
    }
}
