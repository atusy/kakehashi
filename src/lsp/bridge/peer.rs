//! Downstream-facing `kakehashi/bridge/peer*` request handlers.
//!
//! A downstream language server uses these methods to discover and request
//! another downstream connection through kakehashi. Unlike the editor-facing
//! `kakehashi/bridge/client*` family, these handlers are registered only on a
//! downstream connection's reader.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Weak};
use tower_lsp_server::ls_types::WorkspaceFolder;

use super::pool::{ConnectionHandle, ConnectionKey, ConnectionState};

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
#[derive(Default)]
pub(in crate::lsp::bridge) struct PeerDirectory {
    handles: DashMap<ConnectionKey, Weak<ConnectionHandle>>,
}

impl PeerDirectory {
    pub(in crate::lsp::bridge) fn register(&self, handle: &Arc<ConnectionHandle>) {
        self.handles
            .insert(handle.key().clone(), Arc::downgrade(handle));
    }

    pub(in crate::lsp::bridge) fn list(
        &self,
        origin: &ConnectionKey,
        name: Option<&str>,
    ) -> Vec<Peer> {
        let mut peers = self
            .handles
            .iter()
            .filter(|entry| entry.key() != origin)
            .filter(|entry| name.is_none_or(|name| entry.key().server() == name))
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

    pub(in crate::lsp::bridge) fn resolve(
        &self,
        origin: &ConnectionKey,
        id: &str,
    ) -> Option<Arc<ConnectionHandle>> {
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
    name: Option<String>,
}

pub(in crate::lsp::bridge) fn list_result(
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
    serde_json::to_value(directory.list(origin, params.name.as_deref()))
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
    use crate::lsp::bridge::pool::test_helpers::create_handle_with_key;

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

        let peers = directory.list(&origin_key, None);
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
}
