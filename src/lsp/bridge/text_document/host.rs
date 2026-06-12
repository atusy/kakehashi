//! Host-document bridge requests (host-document-bridge).
//!
//! Unlike the virt path, the host path forwards the **real client URI**, the
//! **host text verbatim**, and applies **no coordinate translation** in either
//! direction — the response is the downstream server's answer about the very
//! document the editor sees. The pool's `(uri, server_name)` document state
//! and the request/cancel machinery are shared with the virt path; only the
//! document key and the (absent) translation differ.
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
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, Hover, LocationLink, Position,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier,
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

impl LanguageServerPool {
    /// Open or re-sync the host document on a downstream server.
    ///
    /// - First request for `(uri, server)`: send `didOpen` with the real URI,
    ///   the host language id, and the full host text.
    /// - Host text changed since the last sync: send a full-text `didChange`
    ///   with an incremented version.
    /// - Unchanged: no-op.
    ///
    /// The map lock is held across the queueing so concurrent requests cannot
    /// double-open; the single-writer loop (ls-bridge-message-ordering)
    /// guarantees the notification reaches the wire before the request that
    /// follows it.
    async fn ensure_host_document_synced(
        &self,
        handle: &Arc<ConnectionHandle>,
        doc: &HostDocument<'_>,
        server_name: &str,
    ) -> io::Result<()> {
        let uri_lsp = host_url_to_lsp_uri(doc.uri)?;
        let key = (doc.uri.to_string(), server_name.to_string());
        let fp = fingerprint(doc.text);

        let mut sender = ConnectionHandleSender(handle);
        let mut docs = self.host_documents().await;
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

    /// Send `didClose` for the host document to every server that has it
    /// open via the host bridge, and drop the sync state.
    ///
    /// Mirrors the virt path's `close_host_document`; called from the
    /// upstream `didClose` handler.
    pub(crate) async fn close_host_bridge_document(&self, uri: &Url) {
        let Ok(uri_lsp) = host_url_to_lsp_uri(uri) else {
            return;
        };
        let uri_string = uri.to_string();
        let server_names: Vec<String> = {
            let mut docs = self.host_documents().await;
            let names = docs
                .keys()
                .filter(|(doc_uri, _)| *doc_uri == uri_string)
                .map(|(_, server)| server.clone())
                .collect::<Vec<_>>();
            docs.retain(|(doc_uri, _), _| *doc_uri != uri_string);
            names
        };

        for server_name in server_names {
            let connections = self.connections().await;
            if let Some(handle) = connections.get(&server_name) {
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
        }
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

        if let Err(e) = self
            .ensure_host_document_synced(&handle, doc, server_name)
            .await
        {
            if let Some(ref id) = upstream_request_id {
                self.unregister_upstream_request(id, server_name);
            }
            return Err(e);
        }

        if let Err(e) = handle.send_request(request, request_id) {
            if let Some(ref id) = upstream_request_id {
                self.unregister_upstream_request(id, server_name);
            }
            return Err(e.into());
        }

        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        if let Some(ref id) = upstream_request_id {
            self.unregister_upstream_request(id, server_name);
        }

        Ok(transform_response(response?))
    }

    /// Send a definition request for the host document itself.
    ///
    /// The response is normalized to `Vec<LocationLink>` but otherwise
    /// verbatim: URIs and ranges already live in real-document space.
    pub(crate) async fn send_host_definition_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        doc: &HostDocument<'_>,
        position: Position,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<LocationLink>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/definition") {
            return Ok(None);
        }
        let uri_lsp = host_url_to_lsp_uri(doc.uri)?;
        self.execute_host_request(
            handle,
            server_name,
            doc,
            upstream_request_id,
            |request_id| {
                build_host_position_request(
                    &uri_lsp,
                    position,
                    request_id,
                    "textDocument/definition",
                )
            },
            parse_host_goto_response,
        )
        .await
    }

    /// Send a hover request for the host document itself. Response verbatim.
    pub(crate) async fn send_host_hover_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        doc: &HostDocument<'_>,
        position: Position,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Hover>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/hover") {
            return Ok(None);
        }
        let uri_lsp = host_url_to_lsp_uri(doc.uri)?;
        self.execute_host_request(
            handle,
            server_name,
            doc,
            upstream_request_id,
            |request_id| {
                build_host_position_request(&uri_lsp, position, request_id, "textDocument/hover")
            },
            parse_host_hover_response,
        )
        .await
    }
}

/// `textDocument/didClose` params (the bridge protocol layer has no shared
/// struct for notification-side document identifiers).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentIdentifierParams {
    text_document: TextDocumentIdentifier,
}

fn host_url_to_lsp_uri(uri: &Url) -> io::Result<Uri> {
    crate::lsp::lsp_impl::url_to_uri(uri)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Build a position-based request against the real host URI (no translation).
fn build_host_position_request(
    uri: &Uri,
    position: Position,
    request_id: RequestId,
    method: &'static str,
) -> JsonRpcRequest<TextDocumentPositionParams> {
    JsonRpcRequest::new(
        request_id.as_i64(),
        method,
        TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            position,
        },
    )
}

/// Normalize a goto response (`Location | Location[] | LocationLink[]`) to
/// `Vec<LocationLink>` **without** any URI or range rewriting — host
/// responses already speak real-document coordinates.
fn parse_host_goto_response(mut response: serde_json::Value) -> Option<Vec<LocationLink>> {
    if response_has_jsonrpc_error(&response, "textDocument/definition (host)") {
        return None;
    }
    let result = response.get_mut("result")?.take();
    if result.is_null() {
        return None;
    }

    use tower_lsp_server::ls_types::Location;
    fn location_to_link(location: Location) -> LocationLink {
        LocationLink {
            origin_selection_range: None,
            target_uri: location.uri,
            target_range: location.range,
            target_selection_range: location.range,
        }
    }

    if let Ok(links) = serde_json::from_value::<Vec<LocationLink>>(result.clone()) {
        return Some(links);
    }
    if let Ok(locations) = serde_json::from_value::<Vec<Location>>(result.clone()) {
        return Some(locations.into_iter().map(location_to_link).collect());
    }
    if let Ok(location) = serde_json::from_value::<Location>(result) {
        return Some(vec![location_to_link(location)]);
    }
    log::warn!(
        target: "kakehashi::bridge",
        "host definition response did not match Location | Location[] | LocationLink[]"
    );
    None
}

/// Parse a hover response verbatim (no range translation).
fn parse_host_hover_response(mut response: serde_json::Value) -> Option<Hover> {
    if response_has_jsonrpc_error(&response, "textDocument/hover (host)") {
        return None;
    }
    let result = response.get_mut("result")?.take();
    if result.is_null() {
        return None;
    }
    match serde_json::from_value::<Hover>(result) {
        Ok(hover) => Some(hover),
        Err(e) => {
            log::warn!(
                target: "kakehashi::bridge",
                "host hover response failed to deserialize: {e}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position() -> Position {
        Position {
            line: 32,
            character: 12,
        }
    }

    #[test]
    fn host_position_request_uses_real_uri_and_untranslated_position() {
        let uri: Uri = "file:///project/doc.md".parse().unwrap();
        let request = build_host_position_request(
            &uri,
            position(),
            RequestId::new(7),
            "textDocument/definition",
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value["params"]["textDocument"]["uri"], "file:///project/doc.md",
            "host requests must carry the real client URI"
        );
        assert_eq!(
            value["params"]["position"]["line"], 32,
            "host positions must be forwarded untranslated"
        );
        assert_eq!(value["method"], "textDocument/definition");
    }

    #[test]
    fn host_goto_response_passes_locations_through_verbatim() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "uri": "file:///project/doc.md",
                "range": {
                    "start": { "line": 34, "character": 0 },
                    "end": { "line": 34, "character": 11 }
                }
            }]
        });
        let links = parse_host_goto_response(response).expect("locations must parse");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri.as_str(), "file:///project/doc.md");
        assert_eq!(
            links[0].target_range.start.line, 34,
            "host ranges must NOT be offset-translated"
        );
    }

    #[test]
    fn host_goto_response_handles_single_location() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "uri": "file:///other.md",
                "range": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 5 }
                }
            }
        });
        let links = parse_host_goto_response(response).expect("single location must parse");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri.as_str(), "file:///other.md");
        assert_eq!(links[0].target_selection_range.start.line, 1);
    }

    #[test]
    fn host_goto_response_null_is_none() {
        let response = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": null });
        assert!(parse_host_goto_response(response).is_none());
    }

    #[test]
    fn host_goto_response_error_is_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32603, "message": "boom" }
        });
        assert!(parse_host_goto_response(response).is_none());
    }

    #[test]
    fn host_hover_response_keeps_range_verbatim() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "contents": "docs",
                "range": {
                    "start": { "line": 9, "character": 0 },
                    "end": { "line": 9, "character": 4 }
                }
            }
        });
        let hover = parse_host_hover_response(response).expect("hover must parse");
        assert_eq!(hover.range.unwrap().start.line, 9);
    }

    #[test]
    fn fingerprint_distinguishes_changed_text() {
        assert_eq!(fingerprint("a"), fingerprint("a"));
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }
}
