//! `execute_bridge_request_with_handle` on [`LanguageServerPool`]: shared
//! end-to-end lifecycle for every bridge request (hover, definition,
//! documentLink, …). Steps: convert host URI, build virtual URI, register
//! upstream for cancel forwarding, register with router, build request (via
//! callback), ensure `didOpen`, send, await, unregister upstream, transform
//! the response (via callback).

use std::io;
use std::sync::Arc;

use log::warn;
use tower_lsp_server::ls_types::{Position, Uri};
use url::Url;

use super::{ConnectionHandle, ConnectionHandleSender, LanguageServerPool, UpstreamId};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, host_position_within_region,
};

/// Context provided to response transformers during bridge request execution.
///
/// This struct holds the data that response transformers commonly need to
/// translate coordinates and URIs from virtual document space back to host
/// document space.
pub(crate) struct BridgeResponseContext<'a> {
    /// The virtual document URI string (for matching against response URIs
    /// to determine whether locations point to the same virtual document).
    pub virtual_uri_string: String,
    /// The host document URI in `lsp_types::Uri` form (for rewriting virtual
    /// URIs back to the host URI in goto responses).
    pub host_uri_lsp: &'a Uri,
    /// The injection region offset for coordinate translation back to host space.
    pub offset: &'a RegionOffset,
}

impl LanguageServerPool {
    /// Drive a bridge request end-to-end on a pre-fetched `ConnectionHandle`
    /// (callers obtain it via `get_or_create_connection`, usually because they
    /// need capability checks first). `build_request` shapes the JSON-RPC body
    /// once the virtual URI and request ID are known; `transform_response`
    /// projects the raw response onto the typed result.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_bridge_request_with_handle<T, P: serde::Serialize>(
        &self,
        handle: Arc<ConnectionHandle>,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: &RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        self.execute_bridge_request_observed(
            handle,
            host_uri,
            injection_language,
            region_id,
            offset,
            virtual_content,
            upstream_request_id,
            build_request,
            transform_response,
            None,
        )
        .await
    }

    /// Like [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle),
    /// but additionally publishes the allocated downstream [`RequestId`] into
    /// `downstream_id_probe` as soon as it is known. A caller that may drop
    /// this future (e.g. the formatting pipeline's per-step timeout) can then
    /// still cancel the in-flight downstream request precisely by that id —
    /// the upstream-id cancel mapping is removed by the router cleanup guard
    /// the moment the future is dropped, so it cannot be looked up afterward.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_bridge_request_observed<T, P: serde::Serialize>(
        &self,
        handle: Arc<ConnectionHandle>,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: &RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
        downstream_id_probe: Option<&std::sync::OnceLock<RequestId>>,
    ) -> io::Result<T> {
        // Route all per-connection state by this handle's pool key
        // `(server_name, root)` rather than a separately-threaded server name,
        // so per-root pooling (#382) stays consistent.
        let connection_key = handle.key();

        // Convert host_uri to lsp_types::Uri for bridge protocol functions
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(host_uri)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        // Build virtual document URI
        let virtual_uri = VirtualDocumentUri::new(&host_uri_lsp, injection_language, region_id);

        // Register in the upstream request registry FIRST for cancel lookup.
        // This order matters: if a cancel arrives between pool and router registration,
        // the cancel will fail at the router lookup (which is acceptable for best-effort
        // cancel semantics) rather than finding the server but no downstream ID.
        if let Some(ref id) = upstream_request_id {
            self.register_upstream_request(id.clone(), connection_key);
        }

        // Register request with upstream ID mapping for cancel forwarding
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(upstream_request_id.clone()) {
                Ok(result) => result,
                Err(e) => {
                    // Clean up the pool registration on failure
                    if let Some(ref id) = upstream_request_id {
                        self.unregister_upstream_request(id, connection_key);
                    }
                    return Err(e);
                }
            };

        // Publish the downstream id immediately so a caller that drops this
        // future (per-step timeout) can still cancel the request precisely.
        if let Some(probe) = downstream_id_probe {
            let _ = probe.set(request_id);
        }

        // Build the request via caller-provided closure
        let request = build_request(&virtual_uri, request_id);

        // RAII guard: removes the ResponseRouter entry if the task is aborted
        // (e.g., by JoinSet::abort_all() in first_win). Disarmed after
        // wait_for_response completes normally (any exit path).
        let mut router_guard = RouterCleanupGuard::new(Arc::clone(handle.router()), request_id);

        // Send didOpen notification only if document hasn't been opened yet
        if let Err(e) = self
            .ensure_document_opened(
                &mut ConnectionHandleSender(&handle),
                host_uri,
                &virtual_uri,
                virtual_content,
                connection_key,
            )
            .await
        {
            // router_guard drops here, cleaning up the router entry
            if let Some(ref id) = upstream_request_id {
                self.unregister_upstream_request(id, connection_key);
            }
            return Err(e);
        }

        // Queue the request via single-writer loop (ls-bridge-message-ordering)
        if let Err(e) = handle.send_request(request, request_id) {
            // router_guard drops here, cleaning up the router entry
            if let Some(ref id) = upstream_request_id {
                self.unregister_upstream_request(id, connection_key);
            }
            return Err(e.into());
        }

        // Wait for response via oneshot channel (no Mutex held) with timeout.
        // After this returns (success, channel-closed, or timeout),
        // the router entry has been consumed or cleaned up internally.
        let response = handle.wait_for_response(request_id, response_rx).await;
        router_guard.disarm();

        // Unregister from the upstream request registry regardless of result
        if let Some(ref id) = upstream_request_id {
            self.unregister_upstream_request(id, connection_key);
        }

        // Build context and transform response via caller-provided closure
        let context = BridgeResponseContext {
            virtual_uri_string: virtual_uri.to_uri_string(),
            host_uri_lsp: &host_uri_lsp,
            offset,
        };

        Ok(transform_response(response?, &context))
    }

    /// Like [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle)
    /// but for position-based requests (hover, completion, definition, …): aborts
    /// before contacting the downstream server when `host_position` falls outside
    /// the injection region — either *above* its start line, or on the start line
    /// but *before* its start column (e.g. the cursor is on the markdown fence
    /// backticks or inside a blockquote `> ` prefix). See
    /// [`host_position_within_region`].
    ///
    /// Translating an out-of-region position would silently mistranslate it
    /// (clamping line and/or character via `saturating_sub`) and forward
    /// plausible-but-wrong coordinates. Per the LSP spec every position request
    /// may return an empty/null result, so on abort we feed a synthetic
    /// `{"result": null}` to `transform_response`, which produces the handler's
    /// natural "no result" value (`None` / empty `Vec`).
    ///
    /// A line *above* the region almost certainly means stale region data (a
    /// concurrent host edit) and is logged at `warn`; a position merely before
    /// the start column is a normal cursor location outside the content and is
    /// logged at `debug` to avoid flooding logs during ordinary editing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_position_bridge_request_with_handle<T, P: serde::Serialize>(
        &self,
        handle: Arc<ConnectionHandle>,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: &RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        host_position: Position,
        method: &'static str,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        if !host_position_within_region(host_position, offset) {
            if host_position.line < offset.line() {
                // Line above the region → almost certainly stale region data
                // (a concurrent host edit shifted the region). Unexpected.
                warn!(
                    target: "kakehashi::bridge",
                    "{method}: host position (line {}) is above injection region (start line {}); \
                     aborting request — stale region data",
                    host_position.line,
                    offset.line(),
                );
            } else {
                // On the start line but left of the start column (fence backticks,
                // blockquote prefix). A normal cursor location just outside the
                // injected content — debug, not warn.
                let virtual_line = host_position.line - offset.line();
                log::debug!(
                    target: "kakehashi::bridge",
                    "{method}: host position (line {}, char {}) is before injection start column {}; \
                     aborting request",
                    host_position.line,
                    host_position.character,
                    offset.column_for_line(virtual_line),
                );
            }

            let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(host_uri)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let virtual_uri = VirtualDocumentUri::new(&host_uri_lsp, injection_language, region_id);
            let context = BridgeResponseContext {
                virtual_uri_string: virtual_uri.to_uri_string(),
                host_uri_lsp: &host_uri_lsp,
                offset,
            };
            return Ok(transform_response(
                serde_json::json!({ "result": null }),
                &context,
            ));
        }

        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            offset,
            virtual_content,
            upstream_request_id,
            build_request,
            transform_response,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::pool::ConnectionKey;
    use crate::lsp::bridge::pool::ConnectionState;
    use crate::lsp::bridge::pool::test_helpers::*;
    use std::sync::Arc;

    /// Test that send_hover_request returns Ok(None) when server lacks hover capability.
    ///
    /// This validates the capability guard pattern: when a connection exists and is
    /// Ready but doesn't advertise hover support (server_capabilities not set),
    /// the request should short-circuit to Ok(None) without attempting to send.
    #[tokio::test]
    async fn send_hover_request_returns_none_when_no_hover_capability() {
        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a Ready connection with no capabilities set (all providers = None)
        {
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("test-server"),
            )
            .await;
            // Don't call set_server_capabilities — all providers will be None
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("test-server"), handle);
        }

        let host_uri = test_host_uri("doc");
        let result = pool
            .send_hover_request(
                "test-server",
                &config,
                &host_uri,
                tower_lsp_server::ls_types::Position {
                    line: 0,
                    character: 0,
                },
                "lua",
                TEST_ULID_LUA_0,
                RegionOffset::new(0, 0),
                "print('hello')",
                None,
            )
            .await;

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "Should return None when server lacks hover capability"
        );
    }

    /// Test that a position-based request aborts (returns Ok(None)) when the host
    /// position falls *above* the injection region — stale region data.
    ///
    /// The connection advertises hover support, so the capability guard passes and
    /// execution reaches `execute_position_bridge_request_with_handle`. The backing
    /// process is a sink that never replies, so if the request were actually sent
    /// the call would block until timeout and return `Err`. A fast `Ok(None)`
    /// therefore proves the request was aborted before contacting the server.
    #[tokio::test]
    async fn position_request_aborts_when_host_position_above_region() {
        use tower_lsp_server::ls_types::{HoverProviderCapability, Position, ServerCapabilities};

        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        {
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("test-server"),
            )
            .await;
            handle.set_server_capabilities(ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            });
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("test-server"), handle);
        }

        let host_uri = test_host_uri("doc");
        // Region starts at line 10, but the host position is on line 2 — above the
        // region. This is the stale-data condition the abort guards against.
        let result = pool
            .send_hover_request(
                "test-server",
                &config,
                &host_uri,
                Position {
                    line: 2,
                    character: 0,
                },
                "lua",
                TEST_ULID_LUA_0,
                RegionOffset::new(10, 0),
                "print('hello')",
                None,
            )
            .await;

        assert!(result.is_ok(), "abort should yield Ok, got {result:?}");
        assert!(
            result.unwrap().is_none(),
            "out-of-region host position must abort to None"
        );
    }

    /// Test that a position-based request also aborts when the host position is on
    /// the region's start line but *before* its start column (e.g. cursor on the
    /// markdown fence backticks). Same sink-server reasoning as the line-above
    /// case: a fast `Ok(None)` proves the abort fired before any request was sent.
    #[tokio::test]
    async fn position_request_aborts_when_host_position_before_start_column() {
        use tower_lsp_server::ls_types::{HoverProviderCapability, Position, ServerCapabilities};

        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        {
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("test-server"),
            )
            .await;
            handle.set_server_capabilities(ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            });
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("test-server"), handle);
        }

        let host_uri = test_host_uri("doc");
        // Region starts at line 10, column 4. Cursor is on line 10 but at column 1
        // — left of the start column, i.e. outside the injected content.
        let result = pool
            .send_hover_request(
                "test-server",
                &config,
                &host_uri,
                Position {
                    line: 10,
                    character: 1,
                },
                "lua",
                TEST_ULID_LUA_0,
                RegionOffset::new(10, 4),
                "print('hello')",
                None,
            )
            .await;

        assert!(result.is_ok(), "abort should yield Ok, got {result:?}");
        assert!(
            result.unwrap().is_none(),
            "position before start column must abort to None"
        );
    }

    /// Test that send_document_link_request returns Ok(None) when server lacks documentLink capability.
    ///
    /// Same pattern as send_hover_request_returns_none_when_no_hover_capability:
    /// a Ready connection with no capabilities set should short-circuit to Ok(None).
    #[tokio::test]
    async fn send_document_link_request_returns_none_when_no_capability() {
        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a Ready connection with no capabilities set (all providers = None)
        {
            let handle = create_handle_with_key(
                ConnectionState::Ready,
                ConnectionKey::for_server("test-server"),
            )
            .await;
            pool.connections
                .lock()
                .await
                .insert(ConnectionKey::for_server("test-server"), handle);
        }

        let host_uri = test_host_uri("doc");
        let result = pool
            .send_document_link_request(
                "test-server",
                &config,
                &host_uri,
                "lua",
                TEST_ULID_LUA_0,
                RegionOffset::new(0, 0),
                "print('hello')",
                None,
            )
            .await;

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "Should return None when server lacks documentLink capability"
        );
    }

    /// RouterCleanupGuard removes the router entry when dropped while armed.
    #[test]
    fn router_cleanup_guard_removes_entry_when_armed() {
        use crate::lsp::bridge::actor::ResponseRouter;

        let router = Arc::new(ResponseRouter::new());
        let rx = router.register(RequestId::new(1)).expect("should register");

        let guard = RouterCleanupGuard::new(Arc::clone(&router), RequestId::new(1));
        drop(guard);

        // The entry should have been removed — register again should succeed
        drop(rx); // drop the old receiver first
        assert!(
            router.register(RequestId::new(1)).is_some(),
            "entry should have been removed by the guard"
        );
    }

    /// RouterCleanupGuard does NOT remove the router entry when disarmed.
    #[test]
    fn router_cleanup_guard_skips_removal_when_disarmed() {
        use crate::lsp::bridge::actor::ResponseRouter;

        let router = Arc::new(ResponseRouter::new());
        let _rx = router.register(RequestId::new(1)).expect("should register");

        let mut guard = RouterCleanupGuard::new(Arc::clone(&router), RequestId::new(1));
        guard.disarm();
        drop(guard);

        // The entry should still be present — re-registering should fail (duplicate)
        assert!(
            router.register(RequestId::new(1)).is_none(),
            "entry should still be present since guard was disarmed"
        );
    }

    /// Test that BridgeResponseContext fields are accessible.
    #[test]
    fn bridge_response_context_exposes_fields() {
        let host_uri: Uri = "file:///project/doc.md".parse().unwrap();
        let offset = RegionOffset::new(5, 0);
        let ctx = BridgeResponseContext {
            virtual_uri_string: "file:///project/virtual.lua".to_string(),
            host_uri_lsp: &host_uri,
            offset: &offset,
        };
        assert_eq!(ctx.virtual_uri_string, "file:///project/virtual.lua");
        assert_eq!(ctx.host_uri_lsp, &host_uri);
        assert_eq!(ctx.offset.line(), 5);
        assert_eq!(ctx.offset.column_for_line(0), 0);
    }
}
