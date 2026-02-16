//! Shared bridge request lifecycle execution.
//!
//! This module provides two generic methods on [`LanguageServerPool`] that
//! encapsulate the common lifecycle boilerplate shared by all bridge request
//! handlers (hover, definition, document_link, etc.):
//!
//! - `execute_bridge_request`: Full lifecycle including connection lookup
//! - `execute_bridge_request_with_handle`: Lifecycle with pre-fetched connection handle
//!
//! The lifecycle steps are:
//! 1. Get or create a connection
//! 2. Convert host URI to `lsp_types::Uri`
//! 3. Build virtual document URI
//! 4. Register upstream request for cancel forwarding
//! 5. Register request with router to get oneshot receiver
//! 6. Build the JSON-RPC request (via caller-provided closure)
//! 7. Ensure document is opened (didOpen if needed)
//! 8. Send the request
//! 9. Wait for response
//! 10. Unregister upstream request
//! 11. Transform the response (via caller-provided closure)

use std::io;
use std::sync::Arc;

use tower_lsp_server::ls_types::Uri;
use url::Url;

use super::{ConnectionHandle, ConnectionHandleSender, LanguageServerPool, UpstreamId};
use crate::config::settings::BridgeServerConfig;
use crate::lsp::bridge::protocol::{RequestId, VirtualDocumentUri};

/// Context provided to response transformers during bridge request execution.
///
/// This struct holds the data that response transformers commonly need to
/// translate coordinates and URIs from virtual document space back to host
/// document space.
///
/// Fields are added incrementally as handlers are migrated.
pub(crate) struct BridgeResponseContext<'a> {
    /// The virtual document URI string (for matching against response URIs
    /// to determine whether locations point to the same virtual document).
    pub virtual_uri_string: String,
    /// The host document URI in `lsp_types::Uri` form (for rewriting virtual
    /// URIs back to the host URI in goto responses).
    pub host_uri_lsp: &'a Uri,
    /// The starting line of the injection region in the host document
    /// (for coordinate translation back to host space).
    pub region_start_line: u32,
}

impl LanguageServerPool {
    /// Execute a bridge request through the full lifecycle with a pre-fetched connection handle.
    ///
    /// This method is identical to [`execute_bridge_request`](Self::execute_bridge_request)
    /// but accepts a pre-fetched `ConnectionHandle` instead of fetching it internally.
    /// Use this when you've already obtained a handle (e.g., for capability checking)
    /// to avoid a redundant HashMap lookup.
    ///
    /// # Arguments
    ///
    /// * `handle` - The pre-fetched connection handle
    /// * `server_name` - The server name from config (still needed for document tracking)
    /// * `host_uri` - The host document URI
    /// * `injection_language` - The injection language (e.g., "lua")
    /// * `region_id` - The unique region ID for this injection
    /// * `region_start_line` - The starting line of the injection region in the host document
    /// * `virtual_content` - The content of the virtual document
    /// * `upstream_request_id` - The original request ID from the upstream client
    /// * `build_request` - Closure to build the JSON-RPC request from the
    ///   virtual document URI and allocated request ID
    /// * `transform_response` - Closure to transform the raw JSON-RPC response into the
    ///   typed result, given the response context
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_bridge_request_with_handle<T>(
        &self,
        handle: Arc<ConnectionHandle>,
        server_name: &str,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: UpstreamId,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> serde_json::Value,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        // Convert host_uri to lsp_types::Uri for bridge protocol functions
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(host_uri)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        // Build virtual document URI
        let virtual_uri = VirtualDocumentUri::new(&host_uri_lsp, injection_language, region_id);

        // Register in the upstream request registry FIRST for cancel lookup.
        // This order matters: if a cancel arrives between pool and router registration,
        // the cancel will fail at the router lookup (which is acceptable for best-effort
        // cancel semantics) rather than finding the server but no downstream ID.
        self.register_upstream_request(upstream_request_id.clone(), server_name);

        // Register request with upstream ID mapping for cancel forwarding
        let (request_id, response_rx) =
            match handle.register_request_with_upstream(Some(upstream_request_id.clone())) {
                Ok(result) => result,
                Err(e) => {
                    // Clean up the pool registration on failure
                    self.unregister_upstream_request(&upstream_request_id, server_name);
                    return Err(e);
                }
            };

        // Build the request via caller-provided closure
        let request = build_request(&virtual_uri, request_id);

        // Use a closure for cleanup on any failure path
        let cleanup = || {
            handle.router().remove(request_id);
            self.unregister_upstream_request(&upstream_request_id, server_name);
        };

        // Send didOpen notification only if document hasn't been opened yet
        if let Err(e) = self
            .ensure_document_opened(
                &mut ConnectionHandleSender(&handle),
                host_uri,
                &virtual_uri,
                virtual_content,
                server_name,
            )
            .await
        {
            cleanup();
            return Err(e);
        }

        // Queue the request via single-writer loop (ADR-0015)
        if let Err(e) = handle.send_request(request, request_id) {
            cleanup();
            return Err(e.into());
        }

        // Wait for response via oneshot channel (no Mutex held) with timeout
        let response = handle.wait_for_response(request_id, response_rx).await;

        // Unregister from the upstream request registry regardless of result
        self.unregister_upstream_request(&upstream_request_id, server_name);

        // Build context and transform response via caller-provided closure
        let context = BridgeResponseContext {
            virtual_uri_string: virtual_uri.to_uri_string(),
            host_uri_lsp: &host_uri_lsp,
            region_start_line,
        };

        Ok(transform_response(response?, &context))
    }

    /// Execute a bridge request through the full lifecycle.
    ///
    /// This method encapsulates the common lifecycle boilerplate shared by all
    /// bridge request handlers. It handles connection management, request
    /// registration, document opening, request sending, response waiting, and
    /// cleanup — leaving only request building and response transformation to
    /// the caller.
    ///
    /// For handlers that need to pre-fetch the connection (e.g., for capability
    /// checking), use [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle)
    /// instead to avoid a redundant connection lookup.
    ///
    /// # Arguments
    ///
    /// * `server_name` - The server name from config (e.g., "tsgo", "lua-ls")
    /// * `server_config` - The server configuration containing command and options
    /// * `host_uri` - The host document URI
    /// * `injection_language` - The injection language (e.g., "lua")
    /// * `region_id` - The unique region ID for this injection
    /// * `region_start_line` - The starting line of the injection region in the host document
    /// * `virtual_content` - The content of the virtual document
    /// * `upstream_request_id` - The original request ID from the upstream client
    /// * `build_request` - Closure to build the JSON-RPC request from the
    ///   virtual document URI and allocated request ID
    /// * `transform_response` - Closure to transform the raw JSON-RPC response into the
    ///   typed result, given the response context
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_bridge_request<T>(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: UpstreamId,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> serde_json::Value,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        // Get or create connection - state check is atomic with lookup (ADR-0015)
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;

        // Delegate to the handle-based variant
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            region_start_line,
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
    use crate::lsp::bridge::pool::ConnectionState;
    use crate::lsp::bridge::pool::test_helpers::*;
    use std::sync::Arc;

    /// Test that execute_bridge_request returns error immediately when server is initializing.
    ///
    /// This validates the early-return path in the lifecycle: get_or_create_connection
    /// returns an error for initializing servers, so the closures are never called.
    #[tokio::test]
    async fn execute_bridge_request_returns_error_during_init() {
        let pool = Arc::new(LanguageServerPool::new());
        let config = devnull_config();

        // Insert a ConnectionHandle with Initializing state
        {
            let handle = create_handle_with_state(ConnectionState::Initializing).await;
            pool.connections
                .lock()
                .await
                .insert("lua".to_string(), handle);
        }

        let host_uri = test_host_uri("doc");

        let result = pool
            .execute_bridge_request(
                "lua",
                &config,
                &host_uri,
                "lua",
                TEST_ULID_LUA_0,
                3,
                "print('hello')",
                UpstreamId::Number(1),
                |_virtual_uri, _request_id| {
                    panic!("build_request should not be called during init");
                },
                |_response, _ctx| -> Option<()> {
                    panic!("transform_response should not be called during init");
                },
            )
            .await;

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "bridge: downstream server initializing"
        );
    }

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
            let handle = create_handle_with_state(ConnectionState::Ready).await;
            // Don't call set_server_capabilities — all providers will be None
            pool.connections
                .lock()
                .await
                .insert("test-server".to_string(), handle);
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
                0,
                "print('hello')",
                UpstreamId::Null,
            )
            .await;

        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "Should return None when server lacks hover capability"
        );
    }

    /// Test that BridgeResponseContext fields are accessible.
    #[test]
    fn bridge_response_context_exposes_fields() {
        let host_uri: Uri = "file:///project/doc.md".parse().unwrap();
        let ctx = BridgeResponseContext {
            virtual_uri_string: "file:///project/virtual.lua".to_string(),
            host_uri_lsp: &host_uri,
            region_start_line: 5,
        };
        assert_eq!(ctx.virtual_uri_string, "file:///project/virtual.lua");
        assert_eq!(ctx.host_uri_lsp, &host_uri);
        assert_eq!(ctx.region_start_line, 5);
    }
}
