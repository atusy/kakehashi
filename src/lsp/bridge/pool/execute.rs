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

use super::{
    ConnectionHandle, ConnectionHandleSender, ConnectionState, LanguageServerPool, UpstreamId,
};
use crate::lsp::bridge::actor::RouterCleanupGuard;
use crate::lsp::bridge::protocol::{
    JsonRpcRequest, PREPARE_TIMEOUT, RegionOffset, RequestId, VirtualDocumentUri,
    host_position_hidden_by_prepare, host_position_within_region_bounds,
};

/// How long a request waits for the lifecycle pass to send the text it was
/// built for (see [`LanguageServerPool::wait_for_request_text`]). Its answer
/// is in by then, and the send follows it within a lifecycle pass; a request
/// whose text a newer edit superseded never sees it sent, and fails here (the
/// editor usually cancels such a request first).
const REQUEST_TEXT_WAIT: std::time::Duration = PREPARE_TIMEOUT;

fn request_text_not_held(connection_key: &super::ConnectionKey) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("{connection_key} does not hold the text this request was built for"),
    )
}

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
    /// Whether the host text the request's map describes was still the
    /// region's current host text when the response arrived, as the
    /// region's latest recorded content tells (see
    /// `LanguageServerPool::host_text_still_current`). Always so as is.
    /// When not, the host moved, possibly under a prepared text that stayed
    /// the same (an edit to indentation the peer removes leaves it as it
    /// was), and edits mapped through the request's map would land
    /// elsewhere.
    pub request_map_current: bool,
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
        let uri_string = virtual_uri.to_uri_string();

        // A region its peer failed since this request's answer was taken
        // (under a new epoch, say): the request reads no coordinates through
        // it, as no push is placed through it. One answered after the
        // failure (for a newer text) is not refused.
        let built = offset.prepared();
        let failed = || {
            self.prepares() && self.prepare_failed(host_uri, injection_language, region_id, built)
        };
        if failed() {
            return Err(request_text_not_held(connection_key));
        }
        // Before the host lifecycle guard: the wait can last the whole
        // budget, and a didClose or reopen must not queue behind it.
        self.wait_for_request_text(&uri_string, connection_key, offset, &failed)
            .await?;

        let host_lifecycle = match expected_incarnation {
            Some(expected) => {
                self.request_host_lifecycle_for_incarnation(host_uri, expected)
                    .await?
            }
            None => self.request_host_lifecycle(host_uri).await?,
        };
        let routing_uri = url::Url::parse(&uri_string)
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
            if !connections.get(connection_key).is_some_and(|current| {
                Arc::ptr_eq(current, &handle) && current.state() == ConnectionState::Ready
            }) {
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
                    super::SentText {
                        text: virtual_content,
                        map: offset.prepared_map(),
                    },
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
            let queued = match self.queue_request_if_holding(
                &handle,
                &uri_string,
                connection_key,
                offset,
                failed(),
                request,
                request_id,
            ) {
                Some(queued) => queued.map_err(io::Error::from),
                None => Err(request_text_not_held(connection_key)),
            };
            if let Err(e) = queued {
                drop(connections);
                // router_guard drops here, cleaning up the router entry
                if let Some(ref id) = upstream_request_id {
                    self.unregister_upstream_request(id, connection_key);
                }
                return Err(e);
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

        // A failure of its answer recorded since it was checked (between
        // the check and the enqueue, or while it ran): its reply is read
        // through no failed region, as no push is placed through one.
        let response = response?;
        if failed() {
            return Err(request_text_not_held(connection_key));
        }

        // Build context and transform response via caller-provided closure
        let context = self.response_context(
            &uri_string,
            &host_uri_lsp,
            (host_uri, injection_language, region_id),
            offset,
        );

        Ok(transform_response(response, &context))
    }

    /// The context a response about `region_id` of `host_uri` is
    /// transformed in, read as it arrives.
    fn response_context<'a>(
        &self,
        uri_string: &str,
        host_uri_lsp: &'a Uri,
        region: (&Url, &str, &str),
        offset: &'a RegionOffset,
    ) -> BridgeResponseContext<'a> {
        let (host_uri, language, region_id) = region;
        BridgeResponseContext {
            virtual_uri_string: uri_string.to_string(),
            host_uri_lsp,
            offset,
            request_map_current: offset.prepared().is_none_or(|request_map| {
                self.host_text_still_current(host_uri, language, region_id, request_map)
            }),
        }
    }

    /// Whether `connection_key` holds exactly the text a request built on
    /// `offset` describes (see `document_tracker::holds_request_text`).
    fn holds_request_text(
        &self,
        uri_string: &str,
        connection_key: &super::ConnectionKey,
        offset: &RegionOffset,
    ) -> bool {
        let request_map = offset.prepared();
        let presentation = self.document_tracker.presentation(
            uri_string,
            connection_key,
            super::Observed::Now,
            super::RegionPrepare {
                prepared: request_map.is_some(),
                failed: false,
            },
            request_map.map_or("", |map| map.virtual_text()),
        );
        super::document_tracker::holds_request_text(presentation, request_map)
    }

    /// Queue `request` only while `connection_key` holds the text the
    /// request was built for on `offset`: the lazy open may have sent another
    /// text (a newer forwarded one), and the lifecycle pass may have sent one
    /// since the wait. The check and the enqueue happen under the lock every
    /// send records under, so no recorded text — the first prepared one
    /// included — slips between them (see
    /// `DocumentTracker::queue_if_holding`); the request is serialized
    /// before. `None`, with nothing queued, when the text is not held. A pool
    /// that prepares nothing queues at once.
    #[allow(clippy::too_many_arguments)]
    fn queue_request_if_holding<P: serde::Serialize>(
        &self,
        handle: &ConnectionHandle,
        uri_string: &str,
        connection_key: &super::ConnectionKey,
        offset: &RegionOffset,
        failed: bool,
        request: JsonRpcRequest<P>,
        request_id: RequestId,
    ) -> Option<Result<(), super::BridgeError>> {
        if !self.prepares() {
            return Some(handle.send_request(request, request_id));
        }
        let payload = match serde_json::to_value(&request) {
            Ok(payload) => payload,
            // Reported, and cleaned up after, as any unserializable request.
            Err(_) => return Some(handle.send_request(request, request_id)),
        };
        self.document_tracker.queue_if_holding(
            uri_string,
            connection_key,
            offset.prepared(),
            failed,
            || handle.send_serialized_request(payload, request_id),
        )
    }

    /// Whether [`Self::holds_request_text`] can be false for a request built
    /// on `offset`: the rule's shortcut (see `DocumentTracker::held_texts_matter`).
    fn request_text_matters(&self, offset: &RegionOffset) -> bool {
        self.document_tracker
            .held_texts_matter(super::RegionPrepare {
                prepared: offset.prepared().is_some(),
                failed: false,
            })
    }

    /// Wait (briefly) until a connection that has the document open holds
    /// the text a request built on `offset` describes.
    ///
    /// A request is prepared for the newest virtual text, while the lifecycle
    /// pass sends that text only once its prepare answer is in — so right
    /// after an edit, an open document can still hold the previous prepared
    /// text, whose answer the request's map would misread. Likewise right
    /// after a pair gains or loses its peer. The lifecycle pass is the only
    /// sender (a request sending here could overtake a newer text), so the
    /// request waits for it, and fails if it does not come. A document not
    /// open there is opened by the request with its own text.
    async fn wait_for_request_text(
        &self,
        uri_string: &str,
        connection_key: &super::ConnectionKey,
        offset: &RegionOffset,
        failed: impl Fn() -> bool,
    ) -> io::Result<()> {
        // Without any prepared text sent, nor a map on the request (always
        // so with experimental features off), every connection holds the
        // request's text: no document is looked up.
        if !self.request_text_matters(offset) {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + REQUEST_TEXT_WAIT;
        let _waiting = self.document_tracker.wait_for_held_changes();
        loop {
            // Enabled before the check: a record made after it wakes the
            // wait.
            let changed = self.document_tracker.held_changed();
            let mut changed = std::pin::pin!(changed);
            changed.as_mut().enable();
            // Failed since (its peer failed its input): the lifecycle pass
            // holds the region back and sends nothing, so the text never
            // comes, and the request is refused when queued anyway.
            if failed() {
                return Err(request_text_not_held(connection_key));
            }
            if !self
                .document_tracker
                .is_virtual_doc_open_on_connection(uri_string, connection_key)
                || self.holds_request_text(uri_string, connection_key, offset)
            {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(request_text_not_held(connection_key));
            }
        }
    }

    /// Wake the requests waiting for their text (see
    /// [`Self::wait_for_request_text`]) to check again: a peer failed an
    /// input, which no send records.
    pub(in crate::lsp::bridge) fn wake_request_text_waiters(&self) {
        self.document_tracker.wake_held_waiters();
    }

    /// [`Self::wait_for_request_text`], for the registry's tests, whose
    /// peers fail inputs as production's do.
    #[cfg(test)]
    pub(in crate::lsp::bridge) async fn wait_for_request_text_for_test(
        &self,
        uri_string: &str,
        connection_key: &super::ConnectionKey,
        offset: &RegionOffset,
        failed: impl Fn() -> bool,
    ) -> io::Result<()> {
        self.wait_for_request_text(uri_string, connection_key, offset, failed)
            .await
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
        let hidden = host_position_hidden_by_prepare(host_position, offset);
        if hidden || !host_position_within_region_bounds(host_position, offset, region_end) {
            if hidden {
                // Strictly inside a gap, or on a blank line the prepare peer
                // deleted: the prepared document has no such position, and
                // would answer for another.
                log::debug!(
                    target: "kakehashi::bridge",
                    "{method}: host position (line {}, char {}) has no place in the prepared \
                     document; aborting request",
                    host_position.line,
                    host_position.character,
                );
            } else if host_position.line < offset.line() {
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
            // Nothing was asked: the synthetic null carries no edit.
            let context = BridgeResponseContext {
                virtual_uri_string: virtual_uri.to_uri_string(),
                host_uri_lsp: &host_uri_lsp,
                offset,
                request_map_current: true,
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
    use crate::lsp::bridge::pool::SentText;
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

    fn dedent_answer() -> crate::lsp::bridge::protocol::PrepareResult {
        serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]}]}))
        .unwrap()
    }

    #[tokio::test]
    async fn a_request_reads_only_the_text_its_coordinates_describe() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let pool = LanguageServerPool::preparing();
        let host_uri = Url::parse("file:///test/held.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let uri = virtual_uri.to_uri_string();
        let key = ConnectionKey::for_server("lua");
        let prepared =
            apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                .unwrap();
        let prepared_offset = RegionOffset::new(0, 0).with_prepared(Arc::clone(&prepared.map));
        let as_is = RegionOffset::new(0, 0);
        // As is on record: an unprepared request reads it (any text as is,
        // as on every unprepared path); a prepared one does not.
        pool.record_sent_for_test(&virtual_uri, &key, "  a".into());
        assert!(pool.holds_request_text(&uri, &key, &as_is));
        assert!(!pool.holds_request_text(&uri, &key, &prepared_offset));
        // The prepared text under its map: the prepared request reads it,
        // with an equal map in another allocation too; an unprepared one
        // does not (its pair just lost its peer).
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &prepared.text,
                map: Some(&prepared.map),
            },
        );
        assert!(pool.holds_request_text(&uri, &key, &prepared_offset));
        let again =
            apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                .unwrap();
        assert!(pool.holds_request_text(
            &uri,
            &key,
            &RegionOffset::new(0, 0).with_prepared(again.map)
        ));
        assert!(!pool.holds_request_text(&uri, &key, &as_is));
        // Prepared from another text.
        let other =
            apply_prepare_result("  b", &VirtualLayout::single("  b"), Some(dedent_answer()))
                .unwrap();
        assert!(!pool.holds_request_text(
            &uri,
            &key,
            &RegionOffset::new(0, 0).with_prepared(other.map)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_waits_for_its_text_until_the_budget_runs_out() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let pool = Arc::new(LanguageServerPool::preparing());
        let host_uri = Url::parse("file:///test/wait.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let uri = virtual_uri.to_uri_string();
        let key = ConnectionKey::for_server("lua");
        pool.register_opened_document(&host_uri, &virtual_uri, &key)
            .await;
        pool.record_sent_for_test(&virtual_uri, &key, "  a".into());
        let prepared =
            apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                .unwrap();
        let offset = RegionOffset::new(0, 0).with_prepared(Arc::clone(&prepared.map));

        // Never sent: the request fails once the budget runs out.
        let started = tokio::time::Instant::now();
        assert!(
            pool.wait_for_request_text(&uri, &key, &offset, || false)
                .await
                .is_err()
        );
        assert!(started.elapsed() >= REQUEST_TEXT_WAIT);

        // Sent while it waits: the request goes on.
        let sender = {
            let pool = Arc::clone(&pool);
            let virtual_uri = virtual_uri.clone();
            let key = key.clone();
            let prepared = prepared.clone();
            tokio::spawn(async move {
                tokio::time::sleep(REQUEST_TEXT_WAIT / 2).await;
                pool.record_sent_for_test(
                    &virtual_uri,
                    &key,
                    SentText {
                        text: &prepared.text,
                        map: Some(&prepared.map),
                    },
                );
            })
        };
        let started = tokio::time::Instant::now();
        assert!(
            pool.wait_for_request_text(&uri, &key, &offset, || false)
                .await
                .is_ok()
        );
        assert!(started.elapsed() < REQUEST_TEXT_WAIT);
        sender.await.unwrap();

        // Not open on that connection: nothing to wait for (the request
        // opens it with its own text).
        let started = tokio::time::Instant::now();
        let other = ConnectionKey::for_server("other");
        assert!(
            pool.wait_for_request_text(&uri, &other, &offset, || false)
                .await
                .is_ok()
        );
        assert_eq!(started.elapsed(), std::time::Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_request_goes_on_as_soon_as_its_text_is_sent_or_closed() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let pool = Arc::new(LanguageServerPool::preparing());
        let host_uri = Url::parse("file:///test/wake.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let uri = virtual_uri.to_uri_string();
        let key = ConnectionKey::for_server("lua");
        pool.register_opened_document(&host_uri, &virtual_uri, &key)
            .await;
        pool.record_sent_for_test(&virtual_uri, &key, "  a".into());
        let prepared =
            apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                .unwrap();
        let offset = RegionOffset::new(0, 0).with_prepared(Arc::clone(&prepared.map));
        // Off any polling tick.
        let delay = std::time::Duration::from_millis(2_503);

        // Sent: the request goes on at that instant, not at a later tick.
        let sender = {
            let pool = Arc::clone(&pool);
            let virtual_uri = virtual_uri.clone();
            let key = key.clone();
            let prepared = prepared.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                pool.record_sent_for_test(
                    &virtual_uri,
                    &key,
                    SentText {
                        text: &prepared.text,
                        map: Some(&prepared.map),
                    },
                );
            })
        };
        let started = tokio::time::Instant::now();
        assert!(
            pool.wait_for_request_text(&uri, &key, &offset, || false)
                .await
                .is_ok()
        );
        assert_eq!(started.elapsed(), delay);
        sender.await.unwrap();

        // Closed on the connection: nothing is waited for any more (the
        // request opens it with its own text).
        let other =
            apply_prepare_result("  b", &VirtualLayout::single("  b"), Some(dedent_answer()))
                .unwrap();
        let other_offset = RegionOffset::new(0, 0).with_prepared(Arc::clone(&other.map));
        let closer = {
            let pool = Arc::clone(&pool);
            let virtual_uri = virtual_uri.clone();
            let key = key.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                pool.untrack_document(&virtual_uri, &key).await;
            })
        };
        let started = tokio::time::Instant::now();
        assert!(
            pool.wait_for_request_text(&uri, &key, &other_offset, || false)
                .await
                .is_ok()
        );
        assert_eq!(started.elapsed(), delay);
        closer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_request_goes_on_as_soon_as_its_text_is_held_under_its_map() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        // Two virtual texts the peer prepares into the same text (a gap
        // replaced by a fixed placeholder, say): only the map differs.
        let strip = |virtual_text: &str, columns: u32| {
            let answer = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": columns}}, "newText": ""}
            ]}]}))
            .unwrap();
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                Some(answer),
            )
            .unwrap()
        };
        let before = strip("  a", 2);
        let after = strip("   a", 3);
        assert_eq!(before.text, after.text);
        let pool = Arc::new(LanguageServerPool::preparing());
        let host_uri = Url::parse("file:///test/remap.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let uri = virtual_uri.to_uri_string();
        let key = ConnectionKey::for_server("lua");
        pool.register_opened_document(&host_uri, &virtual_uri, &key)
            .await;
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &before.text,
                map: Some(&before.map),
            },
        );
        let offset = RegionOffset::new(0, 0).with_prepared(Arc::clone(&after.map));
        // Off any polling tick.
        let delay = std::time::Duration::from_millis(2_503);
        let remap = {
            let pool = Arc::clone(&pool);
            let virtual_uri = virtual_uri.clone();
            let key = key.clone();
            let after = after.clone();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                // The lifecycle pass sends nothing for the same text: it
                // holds it under the new map.
                let sent = pool
                    .increment_version_if_content_changed(
                        &virtual_uri,
                        &key,
                        SentText {
                            text: &after.text,
                            map: Some(&after.map),
                        },
                    )
                    .await;
                assert!(sent.is_none());
            })
        };
        let started = tokio::time::Instant::now();
        assert!(
            pool.wait_for_request_text(&uri, &key, &offset, || false)
                .await
                .is_ok()
        );
        assert_eq!(started.elapsed(), delay);
        remap.await.unwrap();
    }

    /// A response is placed through its request's map only while that map
    /// describes the region's current host text, read from the latest
    /// recorded content: not while the host's newer text waits for its
    /// answer, not once the same prepared text is recorded under a new map
    /// (an edit to the host indentation the peer removes) — whatever the
    /// connection still holds, which lags behind — but still after typing
    /// recorded another prepared text, as any answer is stale then.
    #[tokio::test]
    async fn a_response_is_placed_only_while_its_map_describes_the_current_host_text() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let strip = |virtual_text: &str, columns: u32| {
            let answer = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
                {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": columns}}, "newText": ""}
            ]}]}))
            .unwrap();
            apply_prepare_result(
                virtual_text,
                &VirtualLayout::single(virtual_text),
                Some(answer),
            )
            .unwrap()
        };
        // `  red` re-indented to `    red`: the same prepared text; typed
        // on to `  redd`: another.
        let before = strip("  red\n", 2);
        let reindented = strip("    red\n", 4);
        let typed = strip("  redd\n", 2);
        assert_eq!(before.text, reindented.text);
        let pool = LanguageServerPool::preparing();
        // The prepare layer's table, which holds are kept in.
        pool.prepare_failures_for_test();
        let host_uri = Url::parse("file:///test/reindent.md").unwrap();
        let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap();
        pool.open_host_incarnation(&host_uri, 1).await;
        let record = |prepared: &crate::lsp::bridge::protocol::PreparedDocument| {
            pool.record_latest_virtual_content(
                &host_uri,
                1,
                "lua",
                TEST_ULID_LUA_0,
                SentText {
                    text: &prepared.text,
                    map: Some(&prepared.map),
                },
            );
        };
        record(&before);
        let request = RegionOffset::new(0, 0).with_prepared(Arc::clone(&before.map));
        let current = |offset: &RegionOffset| {
            pool.response_context(
                "uri",
                &host_uri_lsp,
                (&host_uri, "lua", TEST_ULID_LUA_0),
                offset,
            )
            .request_map_current
        };
        assert!(current(&request));
        // The host's newer text waits for its answer: held.
        pool.hold_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0);
        assert!(!current(&request));
        // Its answer recorded (the hold cleared in the same step): the same
        // prepared text under a new map, however late the connection is.
        record(&reindented);
        assert!(!current(&request));
        // Typing on recorded another prepared text: kept.
        record(&typed);
        assert!(current(&request));
        // The request's own map current again (an undo): kept.
        record(&before);
        assert!(current(&request));
        // The host edited, its pass not settled yet (the parse window,
        // before any hold is published): the recorded text may be the one
        // before the edit, so pending.
        pool.note_host_edited(&host_uri, 1, 7);
        assert!(!current(&request));
        // A pass settling an older version, or another incarnation's,
        // settles nothing.
        pool.note_host_settled(&host_uri, 1, 6);
        pool.note_host_settled(&host_uri, 2, 7);
        assert!(!current(&request));
        // Settled: the recorded text describes the host again.
        pool.note_host_settled(&host_uri, 1, 7);
        assert!(current(&request));
        // An older pass finishing late does not unsettle it.
        pool.note_host_settled(&host_uri, 1, 5);
        assert!(current(&request));
        // Edited and settled past the request's map: the existing rule.
        pool.note_host_edited(&host_uri, 1, 8);
        record(&reindented);
        pool.note_host_settled(&host_uri, 1, 8);
        assert!(!current(&request));
        record(&before);
        // A map its peer revoked since describes nothing it still gives.
        let revocation = Arc::new(crate::lsp::bridge::protocol::Revocation::default());
        let mut revoked = strip("  red\n", 2).map;
        Arc::get_mut(&mut revoked)
            .unwrap()
            .set_revocation(Arc::clone(&revocation));
        record(&before);
        let revoked_request = RegionOffset::new(0, 0).with_prepared(Arc::clone(&revoked));
        assert!(current(&revoked_request));
        revocation.revoke(crate::lsp::bridge::protocol::next_presentation());
        assert!(revoked.revoked());
        assert!(!current(&revoked_request));
        // A request as is reads nothing.
        pool.note_host_edited(&host_uri, 1, 9);
        assert!(current(&RegionOffset::new(0, 0)));
        pool.hold_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0);
        assert!(current(&RegionOffset::new(0, 0)));
    }

    /// A request is queued only while its connection holds the text it was
    /// built for, checked under the lock every send records and queues under
    /// — never under the document's transition, so a request waits on no
    /// didOpen or didChange in flight (nor holds the connections behind it).
    #[tokio::test]
    async fn a_request_is_queued_only_behind_the_text_it_was_built_for() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        let pool = Arc::new(LanguageServerPool::preparing());
        let host_uri = Url::parse("file:///test/first.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let uri = virtual_uri.to_uri_string();
        let key = ConnectionKey::for_server("lua");
        let queue = |request_map| {
            pool.document_tracker
                .queue_if_holding(&uri, &key, request_map, false, || ())
                .is_some()
        };
        pool.record_sent_for_test(&virtual_uri, &key, "  a".into());

        // A send in flight on the document holds its transition: nothing
        // waits for it.
        let _transition = pool
            .open_transition_lock(&virtual_uri, &key)
            .lock_owned()
            .await;
        assert!(queue(None));

        // The first prepared text sent: an as-is request is not queued
        // behind it; one built on its map is.
        let prepared =
            apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                .unwrap();
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &prepared.text,
                map: Some(&prepared.map),
            },
        );
        assert!(!queue(None));
        assert!(queue(Some(&prepared.map)));
        // The same virtual text prepared another way (the peer's answer
        // changed with its settings, say): not queued behind the text held.
        let other = apply_prepare_result(
            "  a",
            &VirtualLayout::single("  a"),
            Some(
                serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
                    {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "newText": ""}
                ]}]}))
                .unwrap(),
            ),
        )
        .unwrap();
        assert_ne!(other.text, prepared.text);
        assert!(!queue(Some(&other.map)));
    }

    /// A failure of a request's answer recorded after the request was
    /// checked — between the check and the enqueue here, or while it runs —
    /// drops its reply: nothing is read through a failed region. (The
    /// request itself may reach the server.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failure_recorded_after_a_request_was_checked_drops_its_reply() {
        use crate::lsp::bridge::protocol::{AnswerOrigin, VirtualLayout, apply_prepare_result};
        /// Records the failure as the request is serialized: after the
        /// request's check, before its enqueue.
        struct RecordFailure {
            pool: Arc<LanguageServerPool>,
            host: Url,
            failure: AnswerOrigin,
        }
        impl serde::Serialize for RecordFailure {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.pool.prepare_failures_for_test().hold_for_test(
                    &self.host,
                    "lua",
                    TEST_ULID_LUA_0,
                    self.pool.prepare_epoch(),
                    Some(self.failure),
                );
                serializer.serialize_unit()
            }
        }
        let pool = Arc::new(LanguageServerPool::preparing());
        let host = Url::parse("file:///test/failure-after-check.md").unwrap();
        pool.open_host_incarnation(&host, 1).await;
        let key = ConnectionKey::for_server("test-server");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        pool.insert_connection(Arc::clone(&handle)).await;
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let mut prepared =
            apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                .unwrap();
        {
            let map = Arc::get_mut(&mut prepared.map).unwrap();
            map.set_input(1);
            pool.prepare_failures_for_test()
                .share_for_test(&host, "lua", TEST_ULID_LUA_0, map);
        }
        pool.ensure_document_opened(
            &mut ConnectionHandleSender(&handle),
            &host,
            &virtual_uri,
            SentText {
                text: &prepared.text,
                map: Some(&prepared.map),
            },
            &key,
        )
        .await
        .unwrap();
        let failure = AnswerOrigin {
            taken: crate::lsp::bridge::protocol::next_presentation(),
            input: prepared.map.origin().input,
        };
        let probe = Arc::new(std::sync::OnceLock::new());
        let task = {
            let pool = Arc::clone(&pool);
            let handle = Arc::clone(&handle);
            let host = host.clone();
            let probe = Arc::clone(&probe);
            let map = Arc::clone(&prepared.map);
            let text = prepared.text.clone();
            tokio::spawn(async move {
                let params = RecordFailure {
                    pool: Arc::clone(&pool),
                    host: host.clone(),
                    failure,
                };
                pool.execute_bridge_request_observed(
                    handle,
                    &host,
                    "lua",
                    TEST_ULID_LUA_0,
                    &RegionOffset::new(0, 0).with_prepared(map),
                    &text,
                    None,
                    None,
                    |_, id| JsonRpcRequest::new(id.as_i64(), "test/failedAfterCheck", params),
                    |response, _| response,
                    Some(&probe),
                )
                .await
            })
        };
        let id = wait_for_downstream_id(&probe).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !handle.router().is_sent(id) && !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the request is sent or refused");
        let _ = handle.router().route(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.as_i64(),
            "result": "computed on the failed text"
        }));
        assert!(task.await.unwrap().is_err(), "its reply is dropped");
    }

    /// A request on an answer the registry holds its region failed on is
    /// refused at once, before the lifecycle pass re-syncs the host: the
    /// failure is read where it is recorded.
    #[tokio::test]
    async fn a_failure_the_registry_holds_refuses_a_request_before_any_resync() {
        use crate::lsp::bridge::prepare_registry::{Answer, PrepareInput, PrepareTarget};
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = crate::lsp::bridge::BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            crate::lsp::request_id::CancelForwarder::new(Arc::clone(&pool)),
        );
        let host = Url::parse("file:///test/failure-before-resync.md").unwrap();
        pool.open_host_incarnation(&host, 1).await;
        let key = ConnectionKey::for_server("test-server");
        let handle = create_handle_with_key(ConnectionState::Ready, key.clone()).await;
        pool.insert_connection(Arc::clone(&handle)).await;
        let input = PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id: TEST_ULID_LUA_0,
            virtual_text: "  a",
            gaps: &[],
        };
        // The request's answer for the input, as the registry gave it out
        // before the input was asked again (under a new epoch, say).
        let prepared = pool.prepare_failures_for_test().answered_for_test(input, 0);
        let target = PrepareTarget {
            candidates: Arc::from([]),
            epoch: pool.prepare_epoch(),
        };
        coordinator.answer_prepare_for_test(&target, input, Answer::Unusable, true);
        assert!(prepared.map.revoked());
        let probe = std::sync::OnceLock::new();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            pool.execute_bridge_request_observed(
                Arc::clone(&handle),
                &host,
                "lua",
                TEST_ULID_LUA_0,
                &RegionOffset::new(0, 0).with_prepared(Arc::clone(&prepared.map)),
                &prepared.text,
                None,
                None,
                |_, id| JsonRpcRequest::new(id.as_i64(), "test/failedInRegistry", ()),
                |response, _| response,
                Some(&probe),
            ),
        )
        .await
        .expect("refused at once, not sent and waited on");
        assert!(result.is_err());
        assert!(probe.get().is_none(), "nothing was sent");
    }

    /// A request built on an answer its region's peer no longer gives (it
    /// failed the same input since, under a new epoch, say) is not queued,
    /// though its connection still holds that answer's text: no coordinates
    /// are read through a revoked map, as no push is placed through one.
    /// The map says so itself, whatever the registry keeps of the failure.
    #[tokio::test]
    async fn a_request_on_a_region_its_peer_failed_is_not_queued() {
        use crate::lsp::bridge::protocol::{
            AnswerOrigin, PreparedDocument, VirtualLayout, apply_prepare_result,
        };
        let pool = LanguageServerPool::preparing();
        let host_uri = Url::parse("file:///test/failed.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host_uri).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let uri = virtual_uri.to_uri_string();
        let key = ConnectionKey::for_server("lua");
        let failures = pool.prepare_failures_for_test();
        // An answer for `input`, as the registry gives it out.
        let answered = |input| -> PreparedDocument {
            let mut prepared =
                apply_prepare_result("  a", &VirtualLayout::single("  a"), Some(dedent_answer()))
                    .unwrap();
            let map = Arc::get_mut(&mut prepared.map).unwrap();
            map.set_input(input);
            failures.share_for_test(&host_uri, "lua", TEST_ULID_LUA_0, map);
            prepared
        };
        let fail = |input| {
            failures.hold_for_test(
                &host_uri,
                "lua",
                TEST_ULID_LUA_0,
                pool.prepare_epoch(),
                Some(AnswerOrigin {
                    taken: crate::lsp::bridge::protocol::next_presentation(),
                    input,
                }),
            );
        };
        let failed = |prepared: &PreparedDocument| {
            pool.prepare_failed(&host_uri, "lua", TEST_ULID_LUA_0, Some(&prepared.map))
        };
        let as_is_failed = || pool.prepare_failed(&host_uri, "lua", TEST_ULID_LUA_0, None);
        let prepared = answered(1);
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &prepared.text,
                map: Some(&prepared.map),
            },
        );
        let queue = |failed| {
            pool.document_tracker
                .queue_if_holding(&uri, &key, Some(&prepared.map), failed, || ())
                .is_some()
        };
        assert!(queue(false));
        assert!(!queue(true));
        pool.open_host_incarnation(&host_uri, 1).await;
        pool.record_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0, "  a");
        assert!(!failed(&prepared));
        pool.hold_latest_virtual_content(&host_uri, 1, "lua", TEST_ULID_LUA_0);
        assert!(!failed(&prepared), "pending");
        // Another answer for the same input, and one for another text.
        let again = answered(1);
        let other = answered(2);
        // The same input asked again (under a new epoch, say), taken after
        // those answers, and failed: both are revoked, the held text with
        // them; the other text's answer is not.
        fail(1);
        assert!(failed(&prepared));
        assert!(failed(&again));
        assert!(!queue(false), "the held text's coordinates are not read");
        assert!(!failed(&other));
        assert!(as_is_failed(), "as is");
        // An answer taken after the failure (a newer text's, which the next
        // pass is about to send) is not refused by it...
        let after = answered(1);
        assert!(!failed(&after));
        // ...but is by a later failure of its input (another epoch's, say).
        fail(1);
        assert!(failed(&after));
        // The registry moved on (the region held on another input, the
        // failure's answer evicted): the maps it revoked stay revoked, and a
        // text as is is no longer refused by it.
        failures.hold_for_test(
            &host_uri,
            "lua",
            TEST_ULID_LUA_0,
            pool.prepare_epoch(),
            None,
        );
        assert!(failed(&prepared));
        assert!(!as_is_failed());
        // Once the epoch moves, before the lifecycle pass holds the region
        // on another input: the failure no longer says the region is not
        // bridged.
        fail(2);
        assert!(as_is_failed());
        pool.advance_prepare_epoch();
        assert!(!as_is_failed());
        assert!(failed(&other));
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

    /// A position the prepared document has no place for — here a blank line
    /// the peer deleted — aborts before any request is made: the server would
    /// answer for the next line. The sink server never answers, so a prompt
    /// `Ok(None)` proves nothing was sent.
    #[tokio::test]
    async fn position_request_aborts_where_the_prepared_document_has_no_place() {
        use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};
        use tower_lsp_server::ls_types::{HoverProviderCapability, Position, ServerCapabilities};

        let pool = Arc::new(LanguageServerPool::preparing());
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
        let virtual_text = "\nprint('hello')";
        let answer = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": ""}
        ]}]}))
        .unwrap();
        let map = apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(answer),
        )
        .unwrap()
        .map;
        let offset = RegionOffset::new(10, 0).with_prepared(map);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            pool.send_hover_request(
                "test-server",
                &config,
                &test_host_uri("doc"),
                Position {
                    line: 10,
                    character: 0,
                },
                region_host_end(virtual_text, &offset),
                "lua",
                TEST_ULID_LUA_0,
                offset.clone(),
                virtual_text,
                None,
            ),
        )
        .await
        .expect("the request must abort before it is sent");
        assert!(result.unwrap().is_none());
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
            request_map_current: true,
        };
        assert_eq!(ctx.virtual_uri_string, "file:///project/virtual.lua");
        assert_eq!(ctx.host_uri_lsp, &host_uri);
        assert_eq!(ctx.offset.line(), 5);
        assert_eq!(ctx.offset.column_for_line(0), 0);
    }
}
