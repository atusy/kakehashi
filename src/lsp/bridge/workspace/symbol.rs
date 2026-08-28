//! Workspace-symbol fan-out and origin-preserving resolve routing.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tower_lsp_server::ls_types::{
    OneOf, SymbolInformation, SymbolTag, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};

use crate::config::settings::WorkspaceSettings;
use crate::config::{merge_bridge_server_configs, resolve_with_wildcard};
use crate::lsp::bridge::ConnectionKey;
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::pool::{
    ConnectionHandle, ConnectionState, INIT_TIMEOUT_SECS, LanguageServerPool, UpstreamId,
};
use crate::lsp::bridge::protocol::{JsonRpcRequest, response_has_jsonrpc_error};

const SYMBOL_METHOD: &str = "workspace/symbol";
const RESOLVE_METHOD: &str = "workspaceSymbol/resolve";
const ENVELOPE_KEY: &str = "kakehashi";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WorkspaceSymbolEnvelope {
    origin: String,
    connection_key: ConnectionKey,
    connection_generation: u64,
    inner: Option<Value>,
}

fn envelope_symbol(symbol: &mut WorkspaceSymbol, envelope: WorkspaceSymbolEnvelope) {
    symbol.data = Some(serde_json::json!({ ENVELOPE_KEY: { "workspaceSymbol": envelope } }));
}

fn strip_envelope(symbol: &mut WorkspaceSymbol) -> Option<WorkspaceSymbolEnvelope> {
    let envelope = symbol
        .data
        .as_ref()?
        .get(ENVELOPE_KEY)?
        .get("workspaceSymbol")?;
    let mut envelope: WorkspaceSymbolEnvelope = serde_json::from_value(envelope.clone()).ok()?;
    symbol.data = envelope.inner.take();
    Some(envelope)
}

fn re_envelope(symbol: &mut WorkspaceSymbol, envelope: &WorkspaceSymbolEnvelope) {
    let mut restored = envelope.clone();
    restored.inner = symbol.data.take();
    envelope_symbol(symbol, restored);
}

fn merge_resolved_range(original: &mut WorkspaceSymbol, resolved: WorkspaceSymbol) {
    let OneOf::Right(original_location) = &original.location else {
        return;
    };
    let OneOf::Left(resolved_location) = resolved.location else {
        return;
    };
    if original_location.uri == resolved_location.uri
        && resolved_location.range.start <= resolved_location.range.end
    {
        original.location = OneOf::Left(resolved_location);
    }
}

#[allow(deprecated)]
fn flatten_symbol(symbol: SymbolInformation) -> WorkspaceSymbol {
    let mut tags = symbol.tags;
    if symbol.deprecated == Some(true) {
        let tags = tags.get_or_insert_with(Vec::new);
        if !tags.contains(&SymbolTag::DEPRECATED) {
            tags.push(SymbolTag::DEPRECATED);
        }
    }
    WorkspaceSymbol {
        name: symbol.name,
        kind: symbol.kind,
        tags,
        container_name: symbol.container_name,
        location: OneOf::Left(symbol.location),
        data: None,
    }
}

fn normalize_response(
    response: WorkspaceSymbolResponse,
    supports_tags: bool,
) -> Vec<WorkspaceSymbol> {
    let mut symbols = match response {
        WorkspaceSymbolResponse::Flat(symbols) => symbols.into_iter().map(flatten_symbol).collect(),
        WorkspaceSymbolResponse::Nested(symbols) => symbols,
    };
    if !supports_tags {
        for symbol in &mut symbols {
            symbol.tags = None;
        }
    }
    symbols
}

fn decode_response(
    response: Value,
    supports_tags: bool,
) -> serde_json::Result<Vec<WorkspaceSymbol>> {
    let response = if response
        .as_array()
        .is_some_and(|symbols| symbols.iter().any(|symbol| symbol.get("data").is_some()))
    {
        WorkspaceSymbolResponse::Nested(serde_json::from_value(response)?)
    } else {
        serde_json::from_value(response)?
    };
    Ok(normalize_response(response, supports_tags))
}

#[derive(Clone, Copy)]
enum WorkspaceCapability {
    Search,
    Resolve,
}

struct WorkspaceRequestFence<'a> {
    expected_generation: Option<u64>,
    admit: Option<&'a (dyn Fn() -> bool + Sync)>,
}

fn has_static_capability(handle: &ConnectionHandle, capability: WorkspaceCapability) -> bool {
    matches!(
        (handle.server_capabilities(), capability),
        (
            Some(tower_lsp_server::ls_types::ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true) | OneOf::Right(_)),
                ..
            }),
            WorkspaceCapability::Search,
        ) | (
            Some(tower_lsp_server::ls_types::ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Right(
                    tower_lsp_server::ls_types::WorkspaceSymbolOptions {
                        resolve_provider: Some(true),
                        ..
                    }
                )),
                ..
            }),
            WorkspaceCapability::Resolve,
        )
    )
}

impl LanguageServerPool {
    async fn workspace_symbol_producer_is_live(
        &self,
        handle: &Arc<ConnectionHandle>,
        expected_generation: u64,
    ) -> bool {
        let key = handle.key();
        let connections = self.connections().await;
        connections
            .get(key)
            .is_some_and(|live| Arc::ptr_eq(live, handle) && live.state() == ConnectionState::Ready)
            && self.document_connection_generation(key) == expected_generation
    }

    pub(crate) async fn dispatch_workspace_symbol(
        &self,
        mut params: WorkspaceSymbolParams,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
        supports_tags: bool,
        admit: &(dyn Fn() -> bool + Sync),
    ) -> Option<WorkspaceSymbolResponse> {
        // A partial-result token cannot be shared by several downstream producers.
        // Return one deterministic aggregate in the final response instead.
        params.partial_result_params.partial_result_token = None;
        params.work_done_progress_params.work_done_token = None;

        let mut servers: Vec<_> = settings
            .language_servers
            .keys()
            .filter(|name| name.as_str() != crate::config::WILDCARD_KEY)
            .filter(|name| crate::config::is_server_spawnable(&settings.language_servers, name))
            .filter_map(|name| {
                resolve_with_wildcard(
                    &settings.language_servers,
                    name,
                    merge_bridge_server_configs,
                )
                .map(|config| (name.clone(), config))
            })
            .collect();
        servers.sort_by(|a, b| a.0.cmp(&b.0));

        let requests = servers.into_iter().map(|(name, config)| {
            let params = params.clone();
            let upstream_id = upstream_id.clone();
            async move {
                let handle = self
                    .get_or_create_workspace_connection_wait_ready_admitted(
                        &name,
                        &config,
                        Duration::from_secs(INIT_TIMEOUT_SECS),
                        admit,
                    )
                    .await
                    .ok()?;
                if !handle.has_capability(SYMBOL_METHOD) {
                    return None;
                }
                let generation = self.document_connection_generation(handle.key());
                let (response, resolves) = self
                    .send_workspace_request(
                        &handle,
                        WorkspaceCapability::Search,
                        SYMBOL_METHOD,
                        params,
                        upstream_id,
                        WorkspaceRequestFence {
                            expected_generation: Some(generation),
                            admit: Some(admit),
                        },
                    )
                    .await
                    .ok()??;
                let mut symbols = decode_response(response, supports_tags).ok()?;
                if resolves {
                    for symbol in &mut symbols {
                        let inner = symbol.data.take();
                        envelope_symbol(
                            symbol,
                            WorkspaceSymbolEnvelope {
                                origin: name.clone(),
                                connection_key: handle.key().clone(),
                                connection_generation: generation,
                                inner,
                            },
                        );
                    }
                }
                Some(symbols)
            }
        });

        let symbols: Vec<_> = join_all(requests)
            .await
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        (!symbols.is_empty()).then_some(WorkspaceSymbolResponse::Nested(symbols))
    }

    pub(crate) async fn dispatch_workspace_symbol_resolve(
        &self,
        mut symbol: WorkspaceSymbol,
        settings: &WorkspaceSettings,
        upstream_id: Option<UpstreamId>,
    ) -> WorkspaceSymbol {
        let Some(envelope) = strip_envelope(&mut symbol) else {
            return symbol;
        };
        let fail_soft = |mut symbol: WorkspaceSymbol| {
            re_envelope(&mut symbol, &envelope);
            symbol
        };
        if envelope.connection_key.server() != envelope.origin
            || !crate::config::is_server_spawnable(&settings.language_servers, &envelope.origin)
        {
            return fail_soft(symbol);
        }
        let Some(config) = resolve_with_wildcard(
            &settings.language_servers,
            &envelope.origin,
            merge_bridge_server_configs,
        ) else {
            return fail_soft(symbol);
        };
        if self.document_connection_generation(&envelope.connection_key)
            != envelope.connection_generation
        {
            return fail_soft(symbol);
        }
        let Some(handle) = self
            .ready_connection_by_key_for_config(&envelope.connection_key, Some(&config))
            .await
        else {
            return fail_soft(symbol);
        };
        if !handle.has_capability(RESOLVE_METHOD) {
            return fail_soft(symbol);
        }
        match self
            .send_workspace_request::<_, WorkspaceSymbol>(
                &handle,
                WorkspaceCapability::Resolve,
                RESOLVE_METHOD,
                symbol.clone(),
                upstream_id,
                WorkspaceRequestFence {
                    expected_generation: Some(envelope.connection_generation),
                    admit: None,
                },
            )
            .await
        {
            Ok(Some((resolved, _))) => {
                merge_resolved_range(&mut symbol, resolved);
                re_envelope(&mut symbol, &envelope);
                symbol
            }
            Ok(None) | Err(_) => fail_soft(symbol),
        }
    }

    async fn send_workspace_request<P, R>(
        &self,
        handle: &Arc<ConnectionHandle>,
        capability: WorkspaceCapability,
        method: &'static str,
        params: P,
        upstream_id: Option<UpstreamId>,
        fence: WorkspaceRequestFence<'_>,
    ) -> io::Result<Option<(R, bool)>>
    where
        P: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let WorkspaceRequestFence {
            expected_generation,
            admit,
        } = fence;
        let key = handle.key();
        if let Some(id) = &upstream_id {
            self.register_upstream_request_for_handle(id.clone(), handle);
        }
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_id.clone()) {
                Ok(request) => request,
                Err(error) => {
                    if let Some(id) = &upstream_id {
                        self.unregister_upstream_request(id, key);
                    }
                    return Err(error);
                }
            };
        let mut guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);
        let request = JsonRpcRequest::new(request_id.into(), method, params);
        let resolves_at_admission = {
            let connections = self.connections().await;
            if !connections.get(key).is_some_and(|live| {
                Arc::ptr_eq(live, handle) && live.state() == ConnectionState::Ready
            }) || expected_generation
                .is_some_and(|expected| self.document_connection_generation(key) != expected)
            {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "connection replaced",
                ));
            }
            if admit.is_some_and(|admit| !admit()) {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "workspace symbol settings changed before request send",
                ));
            }
            let static_admitted = has_static_capability(handle, capability);
            let static_resolves = has_static_capability(handle, WorkspaceCapability::Resolve);
            let admitted = handle.dynamic_capabilities().with_registration_snapshot(
                SYMBOL_METHOD,
                "resolveProvider",
                |dynamic_search, dynamic_resolves| {
                    let dynamic_admitted = match capability {
                        WorkspaceCapability::Search => dynamic_search,
                        WorkspaceCapability::Resolve => dynamic_resolves,
                    };
                    (static_admitted || dynamic_admitted).then(|| {
                        (
                            handle.send_request(request, request_id),
                            static_resolves || dynamic_resolves,
                        )
                    })
                },
            );
            let Some((send_result, resolves_at_admission)) = admitted else {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "workspace symbol capability was unregistered before request send",
                ));
            };
            if let Err(error) = send_result {
                if let Some(id) = &upstream_id {
                    self.unregister_upstream_request(id, key);
                }
                return Err(error.into());
            }
            resolves_at_admission
        };
        let response = handle.wait_for_response(request_id, response_rx).await;
        guard.disarm();
        if let Some(id) = &upstream_id {
            self.unregister_upstream_request(id, key);
        }
        let response = response?;
        if let Some(expected) = expected_generation
            && !self
                .workspace_symbol_producer_is_live(handle, expected)
                .await
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "workspace symbol producer was replaced before its response was accepted",
            ));
        }
        if admit.is_some_and(|admit| !admit()) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "workspace symbol settings changed before the response was accepted",
            ));
        }
        if response_has_jsonrpc_error(&response, method) {
            return Ok(None);
        }
        serde_json::from_value(response.get("result").cloned().unwrap_or(Value::Null))
            .map(|result| Some((result, resolves_at_admission)))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::pool::test_helpers::{
        create_handle_advertising_workspace_symbols,
        create_handle_advertising_workspace_symbols_with_state, create_handle_with_key,
        record_test_spawn_root, seed_test_client_root, transition_handle_to_ready,
    };
    use crate::lsp::bridge::protocol::RequestId;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower_lsp_server::ls_types::{
        Location, Position, Range, SymbolKind, Uri, WorkspaceLocation,
    };

    fn location() -> Location {
        Location {
            uri: Uri::from_str("file:///workspace/main.rs").unwrap(),
            range: Range::new(Position::new(1, 2), Position::new(1, 5)),
        }
    }

    #[test]
    #[allow(deprecated)]
    fn flat_symbols_are_normalized_and_preserve_deprecation() {
        let symbols = normalize_response(
            WorkspaceSymbolResponse::Flat(vec![SymbolInformation {
                name: "main".into(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: Some(true),
                location: location(),
                container_name: Some("crate".into()),
            }]),
            true,
        );
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].location, OneOf::Left(location()));
        assert_eq!(symbols[0].tags, Some(vec![SymbolTag::DEPRECATED]));
    }

    #[test]
    #[allow(deprecated)]
    fn tags_are_suppressed_for_clients_without_tag_support() {
        let symbols = normalize_response(
            WorkspaceSymbolResponse::Flat(vec![SymbolInformation {
                name: "old".into(),
                kind: SymbolKind::FUNCTION,
                tags: Some(vec![SymbolTag::DEPRECATED]),
                deprecated: Some(true),
                location: location(),
                container_name: None,
            }]),
            false,
        );
        assert_eq!(symbols[0].tags, None);
    }

    #[test]
    #[allow(deprecated)]
    fn empty_supported_tag_set_does_not_imply_deprecated_support() {
        let supports_deprecated = tower_lsp_server::ls_types::TagSupport::<SymbolTag> {
            value_set: Vec::new(),
        }
        .value_set
        .contains(&SymbolTag::DEPRECATED);
        let symbols = normalize_response(
            WorkspaceSymbolResponse::Flat(vec![SymbolInformation {
                name: "old".into(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: Some(true),
                location: location(),
                container_name: None,
            }]),
            supports_deprecated,
        );
        assert_eq!(symbols[0].tags, None);
    }

    #[test]
    fn envelope_round_trip_preserves_origin_and_inner_data() {
        let mut symbol = WorkspaceSymbol {
            name: "main".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(location()),
            data: Some(serde_json::json!({"server": 7})),
        };
        let inner = symbol.data.take();
        envelope_symbol(
            &mut symbol,
            WorkspaceSymbolEnvelope {
                origin: "rust-analyzer".into(),
                connection_key: ConnectionKey::for_server("rust-analyzer"),
                connection_generation: 3,
                inner,
            },
        );
        let envelope = strip_envelope(&mut symbol).unwrap();
        assert_eq!(envelope.origin, "rust-analyzer");
        assert_eq!(envelope.connection_generation, 3);
        assert_eq!(symbol.data, Some(serde_json::json!({"server": 7})));
    }

    #[test]
    fn full_workspace_symbol_response_preserves_resolve_data() {
        let response = serde_json::json!([{
            "name": "main",
            "kind": 12,
            "location": {
                "uri": "file:///workspace/main.rs",
                "range": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 5 }
                }
            },
            "data": { "server": 7 }
        }]);

        let symbols = decode_response(response, true).unwrap();

        assert_eq!(symbols[0].data, Some(serde_json::json!({"server": 7})));
    }

    #[test]
    fn resolved_symbol_only_supplies_the_lazy_range() {
        let mut original = WorkspaceSymbol {
            name: "original".into(),
            kind: SymbolKind::FUNCTION,
            tags: Some(vec![SymbolTag::DEPRECATED]),
            container_name: Some("module".into()),
            location: OneOf::Right(WorkspaceLocation {
                uri: location().uri,
            }),
            data: Some(serde_json::json!({"owner": "original"})),
        };
        let resolved = WorkspaceSymbol {
            name: "changed".into(),
            kind: SymbolKind::CLASS,
            tags: None,
            container_name: None,
            location: OneOf::Left(Location {
                uri: location().uri,
                range: Range::new(Position::new(9, 0), Position::new(9, 8)),
            }),
            data: None,
        };

        merge_resolved_range(&mut original, resolved);

        assert_eq!(original.name, "original");
        assert_eq!(original.kind, SymbolKind::FUNCTION);
        assert_eq!(original.tags, Some(vec![SymbolTag::DEPRECATED]));
        assert_eq!(original.container_name.as_deref(), Some("module"));
        assert_eq!(
            original.data,
            Some(serde_json::json!({"owner": "original"}))
        );
        assert_eq!(
            original.location,
            OneOf::Left(Location {
                uri: location().uri,
                range: Range::new(Position::new(9, 0), Position::new(9, 8)),
            })
        );
    }

    #[test]
    fn resolved_symbol_cannot_change_the_original_uri() {
        let mut original = WorkspaceSymbol {
            name: "original".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Right(WorkspaceLocation {
                uri: location().uri,
            }),
            data: Some(serde_json::json!({"owner": "original"})),
        };
        let original_location = original.location.clone();
        let resolved = WorkspaceSymbol {
            name: "changed".into(),
            kind: SymbolKind::CLASS,
            tags: None,
            container_name: None,
            location: OneOf::Left(Location {
                uri: Uri::from_str("file:///workspace/other.rs").unwrap(),
                range: Range::new(Position::new(9, 0), Position::new(9, 8)),
            }),
            data: None,
        };

        merge_resolved_range(&mut original, resolved);

        assert_eq!(original.location, original_location);
        assert_eq!(original.name, "original");
        assert_eq!(original.kind, SymbolKind::FUNCTION);
        assert_eq!(
            original.data,
            Some(serde_json::json!({"owner": "original"}))
        );
    }

    #[test]
    fn resolved_symbol_cannot_replace_an_existing_range() {
        let mut original = WorkspaceSymbol {
            name: "original".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(location()),
            data: None,
        };
        let original_location = original.location.clone();
        let resolved = WorkspaceSymbol {
            name: "original".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(Location {
                uri: location().uri,
                range: Range::new(Position::new(9, 0), Position::new(9, 8)),
            }),
            data: None,
        };

        merge_resolved_range(&mut original, resolved);

        assert_eq!(original.location, original_location);
    }

    #[test]
    fn resolved_symbol_rejects_an_inverted_range() {
        let mut original = WorkspaceSymbol {
            name: "original".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Right(WorkspaceLocation {
                uri: location().uri,
            }),
            data: None,
        };
        let original_location = original.location.clone();
        let resolved = WorkspaceSymbol {
            name: "original".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(Location {
                uri: location().uri,
                range: Range::new(Position::new(9, 8), Position::new(9, 0)),
            }),
            data: None,
        };

        merge_resolved_range(&mut original, resolved);

        assert_eq!(original.location, original_location);
    }

    #[tokio::test]
    async fn response_fence_rejects_a_replacement_under_the_same_key() {
        let pool = LanguageServerPool::new();
        let key = ConnectionKey::for_server("symbols");
        let producer = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&producer));
        let generation = pool.document_connection_generation(&key);
        assert!(
            pool.workspace_symbol_producer_is_live(&producer, generation)
                .await
        );

        let replacement = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        pool.connections().await.insert(key, replacement);
        assert!(
            !pool
                .workspace_symbol_producer_is_live(&producer, generation)
                .await,
            "a queued response from the old producer must be rejected"
        );
    }

    #[tokio::test]
    async fn resolve_sender_rejects_an_old_response_after_replacement() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("symbols");
        let producer = create_handle_advertising_workspace_symbols(key.clone()).await;
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&producer));

        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "symbols".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-symbols".into()]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let mut symbol = WorkspaceSymbol {
            name: "lazy".into(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(location()),
            data: None,
        };
        envelope_symbol(
            &mut symbol,
            WorkspaceSymbolEnvelope {
                origin: "symbols".into(),
                connection_key: key.clone(),
                connection_generation: pool.document_connection_generation(&key),
                inner: Some(serde_json::json!({ "owner": "old" })),
            },
        );
        let unresolved = symbol.clone();
        let pool_for_request = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_symbol_resolve(symbol, &settings, None)
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("resolve request reaches Sent state");

        let replacement = create_handle_advertising_workspace_symbols(key.clone()).await;
        pool.connections().await.insert(key, replacement);
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "name": "resolved",
                "kind": 12,
                "location": {
                    "uri": "file:///workspace/main.rs",
                    "range": {
                        "start": { "line": 9, "character": 0 },
                        "end": { "line": 9, "character": 8 }
                    }
                },
                "data": { "owner": "old" }
            }
        }));

        assert_eq!(request.await.unwrap(), unresolved);
    }

    #[tokio::test]
    async fn search_sender_rejects_an_old_response_after_replacement() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("symbols");
        let producer = create_handle_advertising_workspace_symbols(key.clone()).await;
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&producer));
        let generation = pool.document_connection_generation(&key);
        let params: WorkspaceSymbolParams = serde_json::from_value(serde_json::json!({
            "query": "stale"
        }))
        .unwrap();
        let pool_for_request = Arc::clone(&pool);
        let producer_for_request = Arc::clone(&producer);
        let request = tokio::spawn(async move {
            pool_for_request
                .send_workspace_request::<_, Value>(
                    &producer_for_request,
                    WorkspaceCapability::Search,
                    SYMBOL_METHOD,
                    params,
                    None,
                    WorkspaceRequestFence {
                        expected_generation: Some(generation),
                        admit: None,
                    },
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("search request reaches Sent state");

        let replacement = create_handle_advertising_workspace_symbols(key.clone()).await;
        pool.connections().await.insert(key, replacement);
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": []
        }));

        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn search_sender_rejects_a_response_after_settings_change() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("symbols");
        let producer = create_handle_advertising_workspace_symbols(key.clone()).await;
        pool.connections()
            .await
            .insert(key.clone(), Arc::clone(&producer));
        let generation = pool.document_connection_generation(&key);
        let admitted = Arc::new(AtomicBool::new(true));
        let params: WorkspaceSymbolParams = serde_json::from_value(serde_json::json!({
            "query": "stale settings"
        }))
        .unwrap();
        let pool_for_request = Arc::clone(&pool);
        let producer_for_request = Arc::clone(&producer);
        let admitted_for_request = Arc::clone(&admitted);
        let request = tokio::spawn(async move {
            let admit = || admitted_for_request.load(Ordering::Acquire);
            pool_for_request
                .send_workspace_request::<_, Value>(
                    &producer_for_request,
                    WorkspaceCapability::Search,
                    SYMBOL_METHOD,
                    params,
                    None,
                    WorkspaceRequestFence {
                        expected_generation: Some(generation),
                        admit: Some(&admit),
                    },
                )
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("search request reaches Sent state");

        admitted.store(false, Ordering::Release);
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": []
        }));

        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[tokio::test]
    async fn search_waits_for_an_existing_initializing_producer() {
        let pool = Arc::new(LanguageServerPool::new());
        let key = ConnectionKey::for_server("symbols");
        let producer = create_handle_advertising_workspace_symbols_with_state(
            ConnectionState::Initializing,
            key.clone(),
        )
        .await;
        pool.connections().await.insert(key, Arc::clone(&producer));
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "symbols".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-symbols".into()]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let params: WorkspaceSymbolParams = serde_json::from_value(serde_json::json!({
            "query": "initializing"
        }))
        .unwrap();
        let pool_for_request = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_symbol(params, &settings, None, true, &|| true)
                .await
        });

        tokio::task::yield_now().await;
        assert!(
            !request.is_finished(),
            "search must wait through initialization"
        );
        assert!(transition_handle_to_ready(&producer));

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !producer.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("search is sent after initialization");
        let _ = producer.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": [{
                "name": "ready",
                "kind": 12,
                "location": {
                    "uri": "file:///workspace/main.rs",
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    }
                }
            }]
        }));

        assert!(matches!(
            request.await.unwrap(),
            Some(WorkspaceSymbolResponse::Nested(symbols)) if symbols.len() == 1
        ));
    }

    #[tokio::test]
    async fn search_uses_client_fallback_when_shared_producer_cannot_follow_workspace() {
        let pool = Arc::new(LanguageServerPool::new());
        seed_test_client_root(&pool, "file:///workspace");
        let shared =
            create_handle_advertising_workspace_symbols(ConnectionKey::shared("symbols")).await;
        record_test_spawn_root(&shared, "file:///workspace/project-a");
        let fallback =
            create_handle_advertising_workspace_symbols(ConnectionKey::new("symbols", None)).await;
        pool.connections().await.extend([
            (shared.key().clone(), Arc::clone(&shared)),
            (fallback.key().clone(), Arc::clone(&fallback)),
        ]);
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "symbols".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["mock-symbols".into()]),
                languages: Some(Vec::new()),
                prefer_shared_instance: Some(true),
                ..Default::default()
            },
        );
        let params: WorkspaceSymbolParams = serde_json::from_value(serde_json::json!({
            "query": "workspace"
        }))
        .unwrap();
        let pool_for_request = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_symbol(params, &settings, None, true, &|| true)
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !fallback.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("workspace search reaches the client-fallback producer");
        assert!(
            !shared.router().is_sent(request_id),
            "a marker-rooted incapable shared producer must not own workspace search"
        );
        let _ = fallback.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": []
        }));
        assert!(request.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn search_reuses_an_incapable_shared_producer_seeded_from_client_root() {
        let pool = Arc::new(LanguageServerPool::new());
        seed_test_client_root(&pool, "file:///workspace");
        let shared =
            create_handle_advertising_workspace_symbols(ConnectionKey::shared("symbols")).await;
        record_test_spawn_root(&shared, "file:///workspace");
        pool.connections()
            .await
            .insert(shared.key().clone(), Arc::clone(&shared));
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "symbols".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["must-not-spawn-fallback".into()]),
                languages: Some(Vec::new()),
                prefer_shared_instance: Some(true),
                ..Default::default()
            },
        );
        let params: WorkspaceSymbolParams = serde_json::from_value(serde_json::json!({
            "query": "workspace"
        }))
        .unwrap();
        let pool_for_request = Arc::clone(&pool);
        let request = tokio::spawn(async move {
            pool_for_request
                .dispatch_workspace_symbol(params, &settings, None, true, &|| true)
                .await
        });

        let request_id = RequestId::new(2);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !shared.router().is_sent(request_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client-seeded shared producer owns workspace search");
        assert_eq!(pool.connection_count().await, 1);
        let _ = shared.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": []
        }));
        assert!(request.await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_settings_refuse_workspace_symbol_producer_admission() {
        let pool = LanguageServerPool::new();
        let temp = tempfile::tempdir().unwrap();
        let sentinel = temp.path().join("stale-producer-started");
        let mut settings = WorkspaceSettings::default();
        settings.language_servers.insert(
            "stale-symbols".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "touch \"$1\"".into(),
                    "workspace-symbol-admission".into(),
                    sentinel.to_string_lossy().into_owned(),
                ]),
                languages: Some(Vec::new()),
                ..Default::default()
            },
        );
        let params: WorkspaceSymbolParams = serde_json::from_value(serde_json::json!({
            "query": "stale"
        }))
        .unwrap();

        let response = pool
            .dispatch_workspace_symbol(params, &settings, None, true, &|| false)
            .await;

        assert_eq!(response, None);
        assert!(
            !sentinel.exists(),
            "a superseded settings snapshot must not spawn its producer"
        );
    }
}
