//! Downstream-facing `kakehashi/bridge/peer*` request handlers.
//!
//! A downstream language server uses these methods to discover and request
//! another downstream connection through kakehashi. Unlike the editor-facing
//! `kakehashi/bridge/client*` family, these handlers are registered only on a
//! downstream connection's reader.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Weak};
use tower_lsp_server::ls_types::{TextDocumentIdentifier, WorkspaceFolder};

use super::pool::{
    ConnectionHandle, ConnectionKey, ConnectionState, DocumentTracker, HostDocuments,
};

pub(in crate::lsp::bridge) mod request;

/// A callable downstream connection exposed to another downstream server.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::lsp::bridge) struct Peer {
    name: String,
    id: String,
    workspace_folders: Vec<WorkspaceFolder>,
}

/// Weak directory of pool connections shared with downstream reader tasks.
///
/// Weak handles avoid a pool -> handle -> reader -> directory -> handle cycle.
/// Re-inserting the same key on respawn replaces the old generation.
pub(in crate::lsp::bridge) struct PeerDirectory {
    handles: DashMap<ConnectionKey, Weak<ConnectionHandle>>,
    document_tracker: Arc<DocumentTracker>,
    host_documents: Arc<HostDocuments>,
}

impl Default for PeerDirectory {
    fn default() -> Self {
        Self::new(
            Arc::new(DocumentTracker::new()),
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        )
    }
}

impl PeerDirectory {
    pub(in crate::lsp::bridge) fn new(
        document_tracker: Arc<DocumentTracker>,
        host_documents: Arc<HostDocuments>,
    ) -> Self {
        Self {
            handles: DashMap::new(),
            document_tracker,
            host_documents,
        }
    }

    pub(in crate::lsp::bridge) fn register(&self, handle: &Arc<ConnectionHandle>) {
        self.prune_dead();
        self.handles
            .insert(handle.key().clone(), Arc::downgrade(handle));
    }

    fn prune_dead(&self) {
        self.handles.retain(|_, handle| handle.strong_count() != 0);
    }

    pub(in crate::lsp::bridge) async fn list(
        &self,
        origin: &ConnectionKey,
        name: Option<&str>,
        text_document: Option<&TextDocumentIdentifier>,
    ) -> Vec<Peer> {
        self.prune_dead();
        let serving_connections = match text_document {
            Some(text_document) => Some(self.serving_connections(text_document.uri.as_str()).await),
            None => None,
        };
        let mut peers = self
            .handles
            .iter()
            .filter(|entry| entry.key() != origin)
            .filter(|entry| name.is_none_or(|name| entry.key().server() == name))
            .filter(|entry| {
                serving_connections
                    .as_ref()
                    .is_none_or(|connections| connections.contains(entry.key()))
            })
            .filter_map(|entry| entry.value().upgrade())
            .filter(|handle| handle.state() == ConnectionState::Ready)
            .map(|handle| Peer {
                name: handle.key().server().to_string(),
                id: handle.key().peer_id(),
                workspace_folders: handle.workspace_folders().snapshot().unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        peers.sort_unstable_by(|left, right| left.id.cmp(&right.id));
        peers
    }

    async fn serving_connections(&self, uri: &str) -> HashSet<ConnectionKey> {
        let mut connections = self
            .document_tracker
            .connections_serving_uri(uri)
            .await
            .into_iter()
            .collect::<HashSet<_>>();
        connections.extend(
            self.host_documents
                .lock()
                .await
                .keys()
                .filter(|(document_uri, _)| document_uri == uri)
                .map(|(_, connection_key)| connection_key.clone()),
        );
        connections
    }

    pub(in crate::lsp::bridge) fn resolve(
        &self,
        origin: &ConnectionKey,
        id: &str,
    ) -> Option<Arc<ConnectionHandle>> {
        self.prune_dead();
        self.handles
            .iter()
            .find(|entry| entry.key() != origin && entry.key().peer_id() == id)
            .and_then(|entry| entry.value().upgrade())
            .filter(|handle| handle.state() == ConnectionState::Ready)
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerParams {
    #[serde(default)]
    text_document: Option<TextDocumentIdentifier>,
    #[serde(default)]
    name: Option<String>,
}

pub(in crate::lsp::bridge) async fn list_result(
    directory: &PeerDirectory,
    origin: &ConnectionKey,
    message: &serde_json::Value,
) -> tower_lsp_server::jsonrpc::Result<serde_json::Value> {
    let params = message
        .get("params")
        .map_or_else(|| Ok(PeerParams::default()), PeerParams::deserialize)
        .map_err(|error| {
            tower_lsp_server::jsonrpc::Error::invalid_params(format!("Invalid params: {error}"))
        })?;
    serde_json::to_value(
        directory
            .list(
                origin,
                params.name.as_deref(),
                params.text_document.as_ref(),
            )
            .await,
    )
    .map_err(|_| tower_lsp_server::jsonrpc::Error::internal_error())
}

#[cfg(test)]
fn peer_keys<'a>(
    keys: impl IntoIterator<Item = &'a ConnectionKey>,
    origin: &ConnectionKey,
) -> Vec<ConnectionKey> {
    keys.into_iter()
        .filter(|key| *key != origin)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::pool::{HostDocSyncState, test_helpers::create_handle_with_key};

    #[test]
    fn peer_list_excludes_only_the_origin_connection() {
        let origin = ConnectionKey::new("tsudoi", Some("file:///repo/a".to_string()));
        let same_server_other_root =
            ConnectionKey::new("tsudoi", Some("file:///repo/b".to_string()));
        let formatter = ConnectionKey::new("denols", Some("file:///repo/a".to_string()));
        let keys = [
            origin.clone(),
            same_server_other_root.clone(),
            formatter.clone(),
        ];

        assert_eq!(
            peer_keys(keys.iter(), &origin),
            vec![same_server_other_root, formatter]
        );
    }

    #[tokio::test]
    async fn peer_directory_lists_only_other_running_connections_in_id_order() {
        let directory = PeerDirectory::default();
        let origin_key = ConnectionKey::for_server("tsudoi");
        let failed_key = ConnectionKey::for_server("broken");
        let z_key = ConnectionKey::for_server("zfmt");
        let a_key = ConnectionKey::for_server("afmt");

        let origin = create_handle_with_key(ConnectionState::Ready, origin_key.clone()).await;
        let failed = create_handle_with_key(ConnectionState::Failed, failed_key).await;
        let zfmt = create_handle_with_key(ConnectionState::Ready, z_key).await;
        let afmt = create_handle_with_key(ConnectionState::Ready, a_key).await;
        for handle in [&origin, &failed, &zfmt, &afmt] {
            directory.register(handle);
        }

        let peers = directory.list(&origin_key, None, None).await;
        assert_eq!(
            peers
                .iter()
                .map(|peer| peer.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "kakehashi-peer:4:afmt:fallback",
                "kakehashi-peer:4:zfmt:fallback"
            ]
        );
    }

    #[tokio::test]
    async fn peer_request_filters_by_name_and_serializes_workspace_folders() {
        let directory = PeerDirectory::default();
        let origin_key = ConnectionKey::for_server("tsudoi");
        let denols_key = ConnectionKey::for_server("denols");
        let oxfmt_key = ConnectionKey::for_server("oxfmt");
        let origin = create_handle_with_key(ConnectionState::Ready, origin_key.clone()).await;
        let denols = create_handle_with_key(ConnectionState::Ready, denols_key).await;
        let oxfmt = create_handle_with_key(ConnectionState::Ready, oxfmt_key).await;
        for handle in [&origin, &denols, &oxfmt] {
            directory.register(handle);
        }

        let result = list_result(
            &directory,
            &origin_key,
            &serde_json::json!({ "params": { "name": "oxfmt" } }),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            serde_json::json!([{
                "name": "oxfmt",
                "id": "kakehashi-peer:5:oxfmt:fallback",
                "workspaceFolders": []
            }])
        );
    }

    #[tokio::test]
    async fn peer_request_filters_out_peers_not_serving_the_text_document() {
        let directory = PeerDirectory::default();
        let origin_key = ConnectionKey::for_server("tsudoi");
        let peer_key = ConnectionKey::for_server("oxfmt");
        let origin = create_handle_with_key(ConnectionState::Ready, origin_key.clone()).await;
        let peer = create_handle_with_key(ConnectionState::Ready, peer_key).await;
        directory.register(&origin);
        directory.register(&peer);

        let result = list_result(
            &directory,
            &origin_key,
            &serde_json::json!({
                "params": {
                    "textDocument": { "uri": "file:///repo/main.ts" }
                }
            }),
        )
        .await
        .unwrap();

        assert_eq!(result, serde_json::json!([]));
    }

    #[tokio::test]
    async fn peer_request_composes_text_document_and_name_filters() {
        let directory = PeerDirectory::default();
        let origin_key = ConnectionKey::for_server("tsudoi");
        let denols_key = ConnectionKey::for_server("denols");
        let other_root_key = ConnectionKey::new("denols", Some("file:///other".to_string()));
        let origin = create_handle_with_key(ConnectionState::Ready, origin_key.clone()).await;
        let denols = create_handle_with_key(ConnectionState::Ready, denols_key.clone()).await;
        let other_root =
            create_handle_with_key(ConnectionState::Ready, other_root_key.clone()).await;
        for handle in [&origin, &denols, &other_root] {
            directory.register(handle);
        }
        directory.host_documents.lock().await.insert(
            ("file:///repo/main.ts".to_string(), denols_key),
            HostDocSyncState {
                version: 1,
                fingerprint: 0,
            },
        );
        directory.host_documents.lock().await.insert(
            ("file:///other/main.ts".to_string(), other_root_key),
            HostDocSyncState {
                version: 1,
                fingerprint: 0,
            },
        );

        let result = list_result(
            &directory,
            &origin_key,
            &serde_json::json!({
                "params": {
                    "textDocument": { "uri": "file:///repo/main.ts" },
                    "name": "denols"
                }
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            result,
            serde_json::json!([{
                "name": "denols",
                "id": "kakehashi-peer:6:denols:fallback",
                "workspaceFolders": []
            }])
        );
    }

    #[tokio::test]
    async fn peer_resolution_accepts_a_running_peer_but_never_the_origin() {
        let directory = PeerDirectory::default();
        let origin_key = ConnectionKey::for_server("tsudoi");
        let peer_key = ConnectionKey::for_server("oxfmt");
        let origin = create_handle_with_key(ConnectionState::Ready, origin_key.clone()).await;
        let peer = create_handle_with_key(ConnectionState::Ready, peer_key).await;
        directory.register(&origin);
        directory.register(&peer);

        assert!(
            directory
                .resolve(&origin_key, "kakehashi-peer:5:oxfmt:fallback")
                .is_some()
        );
        assert!(
            directory
                .resolve(&origin_key, "kakehashi-peer:6:tsudoi:fallback")
                .is_none()
        );
    }

    #[tokio::test]
    async fn peer_lookup_prunes_connections_after_their_handles_drop() {
        let directory = PeerDirectory::default();
        let origin_key = ConnectionKey::for_server("tsudoi");
        let peer = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::new("denols", Some("file:///old-root".to_string())),
        )
        .await;
        directory.register(&peer);
        drop(peer);

        let replacement = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::new("denols", Some("file:///new-root".to_string())),
        )
        .await;
        directory.register(&replacement);
        assert_eq!(directory.handles.len(), 1);

        assert_eq!(directory.list(&origin_key, None, None).await.len(), 1);
    }
}
