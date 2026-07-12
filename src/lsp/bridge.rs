//! Async Bridge Connection for LSP language server integration
//!
//! This module implements the async bridge architecture (ls-bridge-async-connection) for communicating
//! with downstream language servers via stdio.
//!
//! # Module Structure
//!
//! - `actor` - Actor components (ResponseRouter, Reader task) for async I/O (ls-bridge-message-ordering)
//! - `connection` - AsyncBridgeConnection for process spawning and I/O
//! - `coordinator` - BridgeCoordinator for unified pool + node tracking
//! - `protocol` - VirtualDocumentUri, request building, and response transformation
//! - `pool` - LanguageServerPool for server pool coordination (ls-bridge-server-pool-coordination)
//!
//! # Per-method namespace modules
//!
//! Message handling is organized by LSP namespace, one file per method, so the
//! directory listing maps to the supported surface. The dispatcher in
//! `actor::reader` owns the shared transport and routes each method to its file:
//!
//! - `text_document` - the `textDocument` namespace (mostly outbound request
//!   senders, plus the inbound scratch `publishDiagnostics` discard)
//! - `client` - inbound `client/*` (dynamic capability register/unregister)
//! - `window` - inbound `window/*` (log/show message, work-done progress) plus
//!   the bridged `$/progress` stream
//! - `workspace` - inbound `workspace/*` (diagnostic refresh, workspace folders)

mod actor;
mod client;
mod client_progress;
mod connection;
pub(crate) mod coordinator;
mod inbound_request_registry;
mod pool;
mod progress_registry;
mod protocol;
mod root_markers;
mod telemetry;
#[cfg(test)]
pub(crate) mod test_logging;
mod text_document;
mod window;
mod workspace;

// Re-export public types
#[cfg(test)]
pub(crate) use actor::ForwardedRequestCancel;
#[cfg(test)]
pub(crate) use actor::OutboundMessage;
pub(crate) use actor::UpstreamNotification;
pub(crate) use actor::UpstreamRequest;
pub(crate) use client_progress::{
    ClientProgressAggregator, ClientProgressDeregisterGuard, ClientProgressRegistry,
};
pub(crate) use coordinator::BridgeCoordinator;
pub(crate) use coordinator::ResolvedServerConfig;
pub(crate) use inbound_request_registry::InboundRequestRegistry;
pub(crate) use pool::ConnectionKey;
#[cfg(test)]
pub(crate) use pool::ConnectionState;
pub use pool::LanguageServerPool;
pub(crate) use pool::UpstreamId;
/// Re-exported for the capability-prefilter regression test in `lsp_impl`, which
/// seeds a `Ready` downstream to exercise `collect_push_diagnostics`' filter.
#[cfg(test)]
pub(crate) use pool::test_helpers;
pub(crate) use progress_registry::{ProgressConnectionId, ProgressRegistry};
pub(crate) use protocol::RegionOffset;
pub(crate) use protocol::RequestId;
pub(crate) use protocol::VirtualDocumentUri;
pub(crate) use protocol::decode_command;
pub(crate) use protocol::location_link_to_location;
pub(crate) use protocol::region_host_end;
pub(crate) use protocol::strip_bridge_local_versions;
pub(crate) use protocol::transform_workspace_edit_to_host;
pub(crate) use protocol::translate_virtual_range_to_host;
pub(crate) use protocol::workspace_edit_has_effect;
pub(crate) use protocol::workspace_edit_preserves_line_prefixes;
pub(crate) use protocol::workspace_edit_within_region;
pub(crate) use text_document::host::{HostDocument, HostTextReader, normalize_host_goto_result};
pub(crate) use text_document::{
    CodeActionEnvelope, CodeLensEnvelope, UpstreamCodeActionCaps, bridge_code_actions,
    extract_code_action_envelope, extract_code_lens_envelope, parse_code_actions_leniently,
};
pub(crate) use text_document::{KakehashiEnvelope, extract_envelope};
pub(crate) use workspace::WorkspaceFolderSet;

/// Integration tests for the bridge module.
///
/// These tests verify the end-to-end behavior of the bridge components working together.
/// Unit tests for individual components live in their respective modules:
/// - `connection.rs` - AsyncBridgeConnection tests
/// - `protocol.rs` - VirtualDocumentUri and request/response transformation tests
/// - `pool.rs` - LanguageServerPool lifecycle and state tests
#[cfg(test)]
mod tests {
    use super::pool::test_helpers::{is_lua_ls_available, lua_ls_config};
    use super::pool::{LanguageServerPool, UpstreamId};
    use super::protocol::RegionOffset;
    use tower_lsp_server::ls_types::Position;
    use url::Url;

    /// Integration test: LanguageServerPool sends hover request to lua-language-server
    #[tokio::test]
    async fn pool_hover_request_succeeds_with_lua_server() {
        if !is_lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();
        let server_config = lua_ls_config();

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        let host_position = Position {
            line: 3,
            character: 9,
        };
        let virtual_content = "function greet(name)\n    return \"Hello, \" .. name\nend";

        let response = pool
            .send_hover_request(
                "lua", // server_name
                &server_config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                RegionOffset::new(3, 0),
                virtual_content,
                Some(UpstreamId::Number(1)), // upstream_request_id
            )
            .await;

        let _hover_response = response.expect("Hover request should succeed");
    }

    /// Integration test: LanguageServerPool sends completion request to lua-language-server
    #[tokio::test]
    async fn pool_completion_request_succeeds_with_lua_server() {
        if !is_lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();
        let server_config = lua_ls_config();

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        let host_position = Position {
            line: 3,
            character: 3,
        };
        let virtual_content = "pri"; // Partial identifier for completion

        let response = pool
            .send_completion_request(
                "lua", // server_name
                &server_config,
                &host_uri,
                host_position,
                "lua",
                "region-0",
                RegionOffset::new(3, 0),
                virtual_content,
                Some(UpstreamId::Number(1)), // upstream_request_id
            )
            .await;

        let _completion_response = response.expect("Completion request should succeed");
    }

    /// Integration test: LanguageServerPool sends document link request to lua-language-server
    #[tokio::test]
    async fn pool_document_link_request_succeeds_with_lua_server() {
        if !is_lua_ls_available() {
            return;
        }

        let pool = LanguageServerPool::new();
        let server_config = lua_ls_config();

        let host_uri = Url::parse("file:///test/doc.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        // Lua code with require statement - lua-ls may return document links for requires
        let virtual_content = "local mod = require(\"mymodule\")\nprint(mod)";

        let response = pool
            .send_document_link_request(
                "lua", // server_name
                &server_config,
                &host_uri,
                "lua",
                "region-0",
                RegionOffset::new(3, 0),
                virtual_content,
                Some(UpstreamId::Number(1)), // upstream_request_id
            )
            .await;

        // Result is now typed: Option<Vec<DocumentLink>>
        // lua-ls may or may not return links depending on configuration
        // The important thing is the request succeeded
        let result = response.expect("Document link request should succeed");
        // Result can be None (null) or Some(vec) - both are valid
        if let Some(links) = result {
            // If links were returned, they should be properly typed DocumentLink items
            for link in &links {
                // Each link must have a valid range
                assert!(
                    link.range.start.line >= 3,
                    "Links should be in host coordinates (region_start_line=3)"
                );
            }
        }
    }

    /// Unit test: Different languages using the same server_name share a single connection.
    ///
    /// This test verifies the core server-name-based pooling behavior by inserting
    /// a mock connection keyed by server_name, then checking that subsequent lookups
    /// for the same server_name return the same connection.
    ///
    /// Real-world example: ts and tsx both using server "tsgo" should share one process.
    #[tokio::test]
    async fn same_server_different_languages_share_connection() {
        use super::pool::ConnectionState;
        use super::pool::test_helpers::create_handle_with_key;
        use crate::lsp::bridge::ConnectionKey;

        let pool = std::sync::Arc::new(LanguageServerPool::new());

        // Create and insert a Ready connection for server_name "tsgo"
        let server_name = ConnectionKey::for_server("tsgo");
        let handle = create_handle_with_key(ConnectionState::Ready, server_name.clone()).await;
        let inserted_ptr = std::sync::Arc::as_ptr(&handle);

        pool.connections()
            .await
            .insert(server_name.clone(), std::sync::Arc::clone(&handle));

        // Verify only one connection exists
        let connections = pool.connections().await;
        assert_eq!(
            connections.len(),
            1,
            "Only one connection should exist for server_name"
        );
        assert!(
            connections.contains_key(&server_name),
            "Connection should be keyed by server_name"
        );

        // Verify the connection is the same one we inserted
        let retrieved_ptr = std::sync::Arc::as_ptr(connections.get(&server_name).unwrap());
        assert_eq!(
            inserted_ptr, retrieved_ptr,
            "Connection should be the same instance we inserted"
        );

        // Both ts and tsx lookups should return the same connection
        // (in the real system, coordinator resolves both languages to "tsgo")
        let ts_lookup = connections.get(&ConnectionKey::for_server("tsgo"));
        let tsx_lookup = connections.get(&ConnectionKey::for_server("tsgo")); // Same key, same connection
        assert!(ts_lookup.is_some(), "ts lookup should find connection");
        assert!(tsx_lookup.is_some(), "tsx lookup should find connection");
        assert!(
            std::sync::Arc::ptr_eq(ts_lookup.unwrap(), tsx_lookup.unwrap()),
            "Both lookups should return the same connection"
        );
    }

    /// Unit test: Different server_names create separate connections.
    ///
    /// This test verifies that different server_names have separate connections,
    /// even if they might handle similar languages.
    ///
    /// Real-world example: "tsgo" and "eslint" are separate servers even if both
    /// handle TypeScript files.
    #[tokio::test]
    async fn different_servers_create_separate_connections() {
        use super::pool::ConnectionState;
        use super::pool::test_helpers::create_handle_with_key;
        use crate::lsp::bridge::ConnectionKey;

        let pool = std::sync::Arc::new(LanguageServerPool::new());

        // Create and insert two different connections with different server_names
        let handle_tsgo =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("tsgo")).await;
        let handle_eslint =
            create_handle_with_key(ConnectionState::Ready, ConnectionKey::for_server("eslint"))
                .await;

        pool.connections().await.insert(
            ConnectionKey::for_server("tsgo"),
            std::sync::Arc::clone(&handle_tsgo),
        );
        pool.connections().await.insert(
            ConnectionKey::for_server("eslint"),
            std::sync::Arc::clone(&handle_eslint),
        );

        // Verify two separate connections exist
        let connections = pool.connections().await;
        assert_eq!(
            connections.len(),
            2,
            "Two separate connections should exist for different server_names"
        );
        assert!(
            connections.contains_key(&ConnectionKey::for_server("tsgo")),
            "Should have tsgo connection"
        );
        assert!(
            connections.contains_key(&ConnectionKey::for_server("eslint")),
            "Should have eslint connection"
        );

        // Verify handles point to different connections
        let tsgo_ptr =
            std::sync::Arc::as_ptr(connections.get(&ConnectionKey::for_server("tsgo")).unwrap());
        let eslint_ptr = std::sync::Arc::as_ptr(
            connections
                .get(&ConnectionKey::for_server("eslint"))
                .unwrap(),
        );
        assert_ne!(
            tsgo_ptr, eslint_ptr,
            "Different server_names should have different connections"
        );
    }
}
