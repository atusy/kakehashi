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
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, host_position_within_region_bounds,
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

pub(crate) struct RequestHostLifecycle<'a> {
    pool: &'a LanguageServerPool,
    host_uri: &'a Url,
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    incarnation: u64,
}

impl RequestHostLifecycle<'_> {
    pub(crate) fn incarnation(&self) -> u64 {
        self.incarnation
    }
}

impl Drop for RequestHostLifecycle<'_> {
    fn drop(&mut self) {
        self.guard.take();
        self.pool
            .remove_host_lifecycle_lock_if_unshared(self.host_uri, &self.lifecycle);
    }
}

impl LanguageServerPool {
    pub(crate) async fn request_host_lifecycle<'a>(
        &'a self,
        host_uri: &'a Url,
    ) -> io::Result<RequestHostLifecycle<'a>> {
        let lifecycle = self.existing_host_lifecycle_lock(host_uri).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("bridge: host document is closed: {host_uri}"),
            )
        })?;
        // SHARED: concurrent requests on one document must overlap. Only
        // lifecycle transitions take this exclusively.
        let guard = Arc::clone(&lifecycle).read_owned().await;
        let Some(incarnation) = self.current_host_incarnation(host_uri) else {
            drop(guard);
            self.remove_host_lifecycle_lock_if_unshared(host_uri, &lifecycle);
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("bridge: host document is closed: {host_uri}"),
            ));
        };
        Ok(RequestHostLifecycle {
            pool: self,
            host_uri,
            lifecycle,
            guard: Some(guard),
            incarnation,
        })
    }

    pub(crate) async fn request_host_lifecycle_for_incarnation<'a>(
        &'a self,
        host_uri: &'a Url,
        expected_incarnation: u64,
    ) -> io::Result<RequestHostLifecycle<'a>> {
        let lifecycle = self.request_host_lifecycle(host_uri).await?;
        if lifecycle.incarnation() != expected_incarnation {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("bridge: host document incarnation changed before request: {host_uri}"),
            ));
        }
        Ok(lifecycle)
    }

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
            None,
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
        expected_incarnation: Option<u64>,
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

        let host_lifecycle = match expected_incarnation {
            Some(expected) => {
                self.request_host_lifecycle_for_incarnation(host_uri, expected)
                    .await?
            }
            None => self.request_host_lifecycle(host_uri).await?,
        };
        let routing_uri = url::Url::parse(&virtual_uri.to_uri_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        if self
            .host_routing_by_server(&routing_uri, connection_key.server())
            .is_some_and(|enabled| !enabled)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("virtual document routing disabled on {connection_key}"),
            ));
        }
        self.apply_host_routing_workspace_folders(&routing_uri, connection_key.server(), &handle)
            .await?;

        // Register in the upstream request registry before downstream router
        // registration for cancel lookup. This relative order matters: if a
        // cancel arrives between pool and router registration,
        // the cancel will fail at the router lookup (which is acceptable for best-effort
        // cancel semantics) rather than finding the server but no downstream ID.
        if let Some(ref id) = upstream_request_id {
            self.register_upstream_request_for_handle(id.clone(), &handle);
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

        // Claim/didOpen and enqueue the request under the `connections` lock,
        // after verifying `handle` is still the pool's LIVE connection for its
        // key. `handle` was fetched earlier; a concurrent respawn
        // (get_or_create's SpawnNew branch) could have removed it and PURGED the
        // document tracker for this key. Registering didOpen state through the
        // dead handle after that purge would re-seed `document_versions` /
        // `opened_documents` for a process that never received the didOpen,
        // wedging the doc until the host closes it. Holding `connections` across
        // the claim + send excludes the purge from interleaving — the same
        // guard the host path (`execute_host_request`) uses. Lock order
        // connections → document tracker matches the respawn purge.
        {
            let connections = self.connections().await;
            if !connections
                .get(connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                drop(connections);
                // router_guard drops here, cleaning up the router entry
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("connection {connection_key} was replaced before request send"),
                ));
            }

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
                drop(connections);
                // router_guard drops here, cleaning up the router entry
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(e);
            }

            // Queue the request via single-writer loop (ls-bridge-message-ordering)
            if let Err(e) = handle.send_request(request, request_id) {
                drop(connections);
                // router_guard drops here, cleaning up the router entry
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(e.into());
            }
        }
        drop(host_lifecycle);

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
    /// the *snapshotted* injection region — *above* its start line, on the
    /// start line but *before* its start column (e.g. the cursor is on the
    /// markdown fence backticks or inside a blockquote `> ` prefix), or *past*
    /// the region's end-of-content (a caret inside a trailing child the query
    /// excluded). The bound is snapshot-scoped — see the comment at the guard
    /// below for the in-flight-edit window it does not cover — and checked by
    /// [`host_position_within_region_bounds`].
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
    /// a region line's content start column, or past the region's content end,
    /// is a normal cursor location outside the content and is logged at
    /// `debug` to avoid flooding logs during ordinary editing.
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
        region_end: Position,
        method: &'static str,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        self.execute_position_bridge_request_with_handle_inner(
            handle,
            host_uri,
            injection_language,
            region_id,
            offset,
            virtual_content,
            upstream_request_id,
            None,
            host_position,
            region_end,
            method,
            build_request,
            transform_response,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_position_bridge_request_with_handle_for_incarnation<
        T,
        P: serde::Serialize,
    >(
        &self,
        handle: Arc<ConnectionHandle>,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: &RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        expected_incarnation: u64,
        host_position: Position,
        region_end: Position,
        method: &'static str,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        self.execute_position_bridge_request_with_handle_inner(
            handle,
            host_uri,
            injection_language,
            region_id,
            offset,
            virtual_content,
            upstream_request_id,
            Some(expected_incarnation),
            host_position,
            region_end,
            method,
            build_request,
            transform_response,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_position_bridge_request_with_handle_inner<T, P: serde::Serialize>(
        &self,
        handle: Arc<ConnectionHandle>,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: &RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        expected_incarnation: Option<u64>,
        host_position: Position,
        region_end: Position,
        method: &'static str,
        build_request: impl FnOnce(&VirtualDocumentUri, RequestId) -> JsonRpcRequest<P>,
        transform_response: impl FnOnce(serde_json::Value, &BridgeResponseContext<'_>) -> T,
    ) -> io::Result<T> {
        // `region_end` is `region_host_end(virtual_content, offset)`, derived
        // once per request by the fan-out (deriving it is O(virtual_content))
        // and shared by every arm. The bound is SNAPSHOT-scoped, not
        // wire-scoped: `virtual_content` and `offset` come from the
        // preamble's document snapshot, while the open path
        // (`ensure_document_opened`) deliberately substitutes the latest
        // published content at enqueue time so later didChanges serialize
        // behind a current didOpen. An edit landing between the snapshot and
        // the enqueue can therefore still put this (validated) position past
        // the content actually opened — the same in-flight staleness every
        // LSP position request has, which downstream servers clamp. Closing
        // it needs generation-bound opens (issue #996).
        if !host_position_within_region_bounds(host_position, offset, region_end) {
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
            } else if host_position > region_end {
                // Past the region's end-of-content: no virtual coordinate
                // exists for it (end-of-content itself is accepted, inclusive).
                log::debug!(
                    target: "kakehashi::bridge",
                    "{method}: host position (line {}, char {}) is past the region's content \
                     end (line {}, char {}); aborting request",
                    host_position.line,
                    host_position.character,
                    region_end.line,
                    region_end.character,
                );
            } else {
                // On a region line but left of that line's content start
                // column (fence backticks, blockquote prefix). A normal cursor
                // location just outside the injected content — debug, not warn.
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

        self.execute_bridge_request_observed(
            handle,
            host_uri,
            injection_language,
            region_id,
            offset,
            virtual_content,
            upstream_request_id,
            expected_incarnation,
            build_request,
            transform_response,
            None,
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
    use crate::lsp::bridge::protocol::region_host_end;
    use std::sync::Arc;

    fn start_observed_request(
        pool: Arc<LanguageServerPool>,
        handle: Arc<ConnectionHandle>,
        host_uri: Url,
        upstream_id: UpstreamId,
    ) -> (
        tokio::task::JoinHandle<io::Result<()>>,
        Arc<std::sync::OnceLock<RequestId>>,
    ) {
        let probe = Arc::new(std::sync::OnceLock::new());
        let request_probe = Arc::clone(&probe);
        let request = tokio::spawn(async move {
            pool.execute_bridge_request_observed(
                handle,
                &host_uri,
                "lua",
                TEST_ULID_LUA_0,
                &RegionOffset::new(0, 0),
                "print('hello')",
                Some(upstream_id),
                None,
                |_, request_id| {
                    JsonRpcRequest::new(
                        request_id.as_i64(),
                        "test/request",
                        serde_json::Value::Null,
                    )
                },
                |_, _| (),
                Some(&request_probe),
            )
            .await
        });

        (request, probe)
    }

    async fn wait_for_downstream_id(probe: &std::sync::OnceLock<RequestId>) -> RequestId {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while probe.get().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("request should publish its downstream id");
        *probe.get().expect("probe should be populated")
    }

    async fn assert_concurrent_downstream_ids_are_unique(upstream_id: UpstreamId, name: &str) {
        let numeric_upstream_id = match &upstream_id {
            UpstreamId::Number(id) => Some(*id),
            UpstreamId::String(_) => None,
        };
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = test_host_uri(name);
        pool.open_host_incarnation(&host_uri, 1).await;
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            ConnectionKey::for_server("test-server"),
        )
        .await;
        pool.insert_connection(Arc::clone(&handle)).await;
        let (first_request, first_probe) = start_observed_request(
            Arc::clone(&pool),
            Arc::clone(&handle),
            host_uri.clone(),
            upstream_id.clone(),
        );
        let (second_request, second_probe) =
            start_observed_request(pool, handle, host_uri, upstream_id);

        let (first, second) = tokio::join!(
            wait_for_downstream_id(&first_probe),
            wait_for_downstream_id(&second_probe)
        );
        first_request.abort();
        second_request.abort();
        let (first_join, second_join) = tokio::join!(first_request, second_request);
        for join in [first_join, second_join] {
            let error = join.expect_err("an aborted request task must not complete normally");
            assert!(
                error.is_cancelled(),
                "request task failed for a reason other than cancellation: {error}"
            );
        }

        assert_ne!(first, second, "concurrent requests need fresh numeric ids");
        if let Some(upstream_id) = numeric_upstream_id {
            assert_ne!(
                first.as_i64(),
                upstream_id,
                "downstream id mirrored upstream"
            );
            assert_ne!(
                second.as_i64(),
                upstream_id,
                "downstream id mirrored upstream"
            );
        }
    }

    #[tokio::test]
    async fn downstream_ids_are_unique_and_independent_of_upstream_id() {
        assert_concurrent_downstream_ids_are_unique(
            UpstreamId::String("client-request".into()),
            "string-upstream-id",
        )
        .await;
        assert_concurrent_downstream_ids_are_unique(UpstreamId::Number(42), "numeric-upstream-id")
            .await;
    }

    #[tokio::test]
    async fn host_close_waits_for_request_enqueue_guard() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/guarded-close.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        let guard = pool.request_host_lifecycle(&host_uri).await.unwrap();
        assert_eq!(guard.incarnation(), 1);

        let close = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            tokio::spawn(async move { pool.close_host_incarnation(&host_uri, 1).await })
        };
        tokio::task::yield_now().await;
        assert!(!close.is_finished());

        drop(guard);
        close.await.unwrap();
        let Err(error) = pool.request_host_lifecycle(&host_uri).await else {
            panic!("closed host must reject a late request");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    /// The host and virt layers of one document race under `try_join!`, and
    /// each takes this lock for its whole downstream round trip. If it were
    /// exclusive, the second layer could not even send its request until the
    /// first answered — a client `$/cancelRequest` would then have nothing to
    /// forward to the layer that had not started, which is exactly what
    /// `e2e_whole_document_link_cancel_forwards_to_concatenated_layers` saw.
    #[tokio::test]
    async fn concurrent_requests_on_one_document_hold_the_lifecycle_together() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/concurrent-layers.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;

        let first = pool.request_host_lifecycle(&host_uri).await.unwrap();
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            pool.request_host_lifecycle(&host_uri),
        )
        .await
        .expect("a second in-flight request must not wait out the first")
        .expect("the document is still open");

        assert_eq!(first.incarnation(), 1);
        assert_eq!(second.incarnation(), 1);

        // A close still waits for BOTH to finish.
        let close = {
            let pool = Arc::clone(&pool);
            let host_uri = host_uri.clone();
            tokio::spawn(async move { pool.close_host_incarnation(&host_uri, 1).await })
        };
        tokio::task::yield_now().await;
        assert!(
            !close.is_finished(),
            "close must not pass an in-flight request"
        );
        drop(first);
        tokio::task::yield_now().await;
        assert!(
            !close.is_finished(),
            "close must wait for the last in-flight request, not just the first"
        );
        drop(second);
        close.await.unwrap();
    }

    #[tokio::test]
    async fn host_request_rejects_a_reopened_incarnation() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/reopened.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.close_host_incarnation(&host_uri, 1).await;
        pool.open_host_incarnation(&host_uri, 2).await;

        let Err(error) = pool
            .request_host_lifecycle_for_incarnation(&host_uri, 1)
            .await
        else {
            panic!("an old resolve must not enter the reopened lifetime");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn closed_host_request_reclaims_a_lifecycle_lock_retained_by_the_race() {
        let pool = Arc::new(LanguageServerPool::new());
        let host_uri = Url::parse("file:///test/closed-race.md").unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;

        let raced_clone = pool.host_lifecycle_lock(&host_uri);
        pool.close_host_incarnation(&host_uri, 1).await;
        assert!(pool.existing_host_lifecycle_lock(&host_uri).is_some());
        drop(raced_clone);

        assert!(pool.request_host_lifecycle(&host_uri).await.is_err());
        assert!(
            pool.existing_host_lifecycle_lock(&host_uri).is_none(),
            "the losing request must reclaim the stale lifecycle-map entry"
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
                region_host_end("print('hello')", &RegionOffset::new(0, 0)),
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
                region_host_end("print('hello')", &RegionOffset::new(10, 0)),
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
                region_host_end("print('hello')", &RegionOffset::new(10, 4)),
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
