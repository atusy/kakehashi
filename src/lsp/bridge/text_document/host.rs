//! Host-document bridge requests (host-document-bridge).
//!
//! Unlike the virt path, the host path forwards the **real client URI**, the
//! **host text verbatim**, and applies **no coordinate translation** in either
//! direction — the response is the downstream server's answer about the very
//! document the editor sees. Because of that, host requests need no
//! per-method request builders or response transformers: the upstream
//! request's params are forwarded as raw JSON
//! ([`LanguageServerPool::send_host_raw_request`]) and the result comes back
//! verbatim. The pool's `(uri, server_name)` document state and the
//! request/cancel machinery are shared with the virt path; only the document
//! key and the (absent) translation differ.
//!
//! Document sync is lazy: the host document is opened on the first request
//! per `(uri, server)` and re-synced with a full-text `didChange` whenever
//! the host text changed since the last request (fingerprint comparison) —
//! the same full-content sync the virt path uses for its `didChange`
//! forwarding.

use std::collections::hash_map::Entry;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::Arc;

use tower_lsp_server::ls_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, Location, LocationLink,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem, TextEdit, Uri,
    VersionedTextDocumentIdentifier,
};
use url::Url;

use super::super::actor::RouterCleanupGuard;
use super::super::pool::{
    ConnectionHandle, ConnectionHandleSender, HostDocSyncState, LanguageServerPool, MessageSender,
    UpstreamId,
};
use super::super::protocol::{
    JsonRpcNotification, JsonRpcRequest, RequestId, response_has_jsonrpc_error,
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

/// Open or re-sync the host document on a downstream server, mutating the
/// pool's sync-state map in place.
///
/// - First request for `(uri, server)`: send `didOpen` with the real URI,
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
async fn sync_host_document<S: MessageSender>(
    sender: &mut S,
    docs: &mut std::collections::HashMap<(String, String), HostDocSyncState>,
    doc: &HostDocument<'_>,
    server_name: &str,
) -> io::Result<()> {
    let uri_lsp = host_url_to_lsp_uri(doc.uri)?;
    let key = (doc.uri.to_string(), server_name.to_string());
    let fp = fingerprint(doc.text);

    match docs.entry(key) {
        Entry::Vacant(entry) => {
            let notification = JsonRpcNotification::new(
                "textDocument/didOpen",
                DidOpenTextDocumentParams {
                    text_document: TextDocumentItem::new(
                        uri_lsp,
                        doc.language_id.to_string(),
                        1,
                        doc.text.to_string(),
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
                            text: doc.text.to_string(),
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
        let Ok(uri_lsp) = host_url_to_lsp_uri(uri) else {
            return;
        };
        let uri_string = uri.to_string();

        let handles: Vec<(String, Arc<ConnectionHandle>)> = {
            let connections = self.connections().await;
            connections
                .iter()
                .map(|(name, handle)| (name.clone(), Arc::clone(handle)))
                .collect()
        };

        let mut docs = self.host_documents().await;
        for (server_name, handle) in &handles {
            if !docs.contains_key(&(uri_string.clone(), server_name.clone())) {
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
        docs.retain(|(doc_uri, _), _| *doc_uri != uri_string);
    }

    /// Send a host bridge request with the upstream params forwarded
    /// **verbatim** (host-document-bridge): the params already reference the
    /// real URI and real coordinates, so no per-method request shaping is
    /// needed. Returns the raw `result` value, or `None` for a `null`
    /// result, a JSON-RPC error, or a missing capability.
    ///
    /// One exception to verbatim: the progress tokens are stripped. The
    /// bridge discards downstream notifications, so a server honoring
    /// `partialResultToken` would stream its results into the void and could
    /// legally return an empty final result; `workDoneToken` would likewise
    /// report progress nowhere.
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
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability(method) {
            return Ok(None);
        }
        self.execute_host_request(
            handle,
            server_name,
            doc,
            upstream_request_id,
            |request_id| JsonRpcRequest::new(request_id.as_i64(), method, params),
            move |response| parse_host_raw_response(response, method),
        )
        .await
    }

    /// Send a host formatting request. Unlike [`Self::send_host_raw_request`],
    /// the response keeps formatting's LSP semantics: a `null` result from a
    /// capable server is the authoritative "no edits needed" signal
    /// (concatenated-formatting-pipeline) and comes back as `Some(vec![])`,
    /// distinct from `None` = no capability / error / malformed — only the
    /// latter should trigger fallback to a lower-priority server.
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
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability(method) {
            return Ok(None);
        }
        self.execute_host_request(
            handle,
            server_name,
            doc,
            upstream_request_id,
            |request_id| JsonRpcRequest::new(request_id.as_i64(), method, params),
            move |response| parse_host_formatting_response(response, method),
        )
        .await
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
        server_name: &str,
        doc: &HostDocument<'_>,
        upstream_request_id: Option<UpstreamId>,
        build_request: impl FnOnce(RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value) -> T,
    ) -> io::Result<T> {
        if let Some(ref id) = upstream_request_id {
            self.register_upstream_request(id.clone(), server_name);
        }

        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_request_id.clone()) {
                Ok(result) => result,
                Err(e) => {
                    if let Some(ref id) = upstream_request_id {
                        self.unregister_upstream_request(id, server_name);
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
        // wedging the `(uri, server)` pair until the next purge. Holding
        // `connections` here also excludes a purge from interleaving with
        // this sync, closing the race entirely.
        {
            let connections = self.connections().await;
            if !connections
                .get(server_name)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                drop(connections);
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, server_name);
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("connection to {server_name} was replaced during host sync"),
                ));
            }

            let mut docs = self.host_documents().await;
            let mut sender = ConnectionHandleSender(&handle);
            if let Err(e) = sync_host_document(&mut sender, &mut docs, doc, server_name).await {
                drop(docs);
                drop(connections);
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, server_name);
                }
                return Err(e);
            }

            if let Err(e) = handle.send_request(request, request_id) {
                drop(docs);
                drop(connections);
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, server_name);
                }
                return Err(e.into());
            }
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        if let Some(ref id) = upstream_request_id {
            self.unregister_upstream_request(id, server_name);
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

/// Strip the JSON-RPC envelope: `None` for an error response or a `null`
/// result, the bare `result` value otherwise.
fn parse_host_raw_response(
    mut response: serde_json::Value,
    method: &'static str,
) -> Option<serde_json::Value> {
    if response_has_jsonrpc_error(&response, method) {
        return None;
    }
    let result = response.get_mut("result")?.take();
    if result.is_null() { None } else { Some(result) }
}

/// Parse a host formatting response with formatting's null semantics:
/// `null` from a capable server = authoritative "no edits needed" →
/// `Some(vec![])`; only an error / malformed payload yields `None`
/// (fallback).
fn parse_host_formatting_response(
    mut response: serde_json::Value,
    method: &'static str,
) -> Option<Vec<TextEdit>> {
    if response_has_jsonrpc_error(&response, method) {
        return None;
    }
    let result = response.get_mut("result")?.take();
    if result.is_null() {
        return Some(Vec::new());
    }
    match serde_json::from_value::<Vec<TextEdit>>(result) {
        Ok(edits) => Some(edits),
        Err(e) => {
            log::warn!(
                target: "kakehashi::bridge",
                "host formatting response failed to deserialize: {e}"
            );
            None
        }
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
        let result =
            parse_host_raw_response(response, "textDocument/definition").expect("result expected");
        assert_eq!(result[0]["uri"], "file:///doc.md");
        assert_eq!(
            result[0]["range"]["start"]["line"], 34,
            "host results must NOT be offset-translated"
        );
    }

    #[test]
    fn raw_response_null_is_none() {
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        assert!(parse_host_raw_response(response, "textDocument/hover").is_none());
    }

    #[test]
    fn raw_response_error_is_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32603, "message": "boom" }
        });
        assert!(parse_host_raw_response(response, "textDocument/hover").is_none());
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
    fn formatting_response_error_falls_through() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32603, "message": "boom" }
        });
        assert!(
            parse_host_formatting_response(response, "textDocument/formatting").is_none(),
            "errors must trigger fallback, unlike null"
        );
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
        sync_host_document(&mut sender, &mut docs, &host_doc(&uri, "v1"), "srv")
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
        sync_host_document(&mut sender, &mut docs, &host_doc(&uri, "v1"), "srv")
            .await
            .unwrap();
        assert!(rx.try_recv().is_err(), "unchanged text must be a no-op");

        // Drifted text: full-text didChange with version 2.
        sync_host_document(&mut sender, &mut docs, &host_doc(&uri, "v2"), "srv")
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
        sync_host_document(&mut sender, &mut docs, &host_doc(&uri, "v3"), "srv")
            .await
            .unwrap();
        let OutboundMessage::Untracked(payload) = rx.try_recv().expect("didChange must be queued")
        else {
            panic!("expected a notification");
        };
        assert_eq!(payload["params"]["textDocument"]["version"], 3);
    }

    #[tokio::test]
    async fn close_host_bridge_document_drops_only_that_uri() {
        let pool = LanguageServerPool::new();
        let uri_a = Url::parse("file:///test/a.md").unwrap();
        let uri_b = Url::parse("file:///test/b.md").unwrap();
        {
            let mut docs = pool.host_documents().await;
            docs.insert(
                (uri_a.to_string(), "srv".to_string()),
                HostDocSyncState {
                    version: 1,
                    fingerprint: 1,
                },
            );
            docs.insert(
                (uri_b.to_string(), "srv".to_string()),
                HostDocSyncState {
                    version: 1,
                    fingerprint: 2,
                },
            );
        }

        pool.close_host_bridge_document(&uri_a).await;

        let docs = pool.host_documents().await;
        assert!(
            !docs.contains_key(&(uri_a.to_string(), "srv".to_string())),
            "closed uri's state must be dropped"
        );
        assert!(
            docs.contains_key(&(uri_b.to_string(), "srv".to_string())),
            "other documents must be untouched"
        );
    }
}
