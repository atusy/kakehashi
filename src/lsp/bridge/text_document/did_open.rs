//! Eager didOpen notification handling for bridge connections.
//!
//! This module provides eager opening of virtual documents on downstream
//! language servers when injection regions are detected during `did_open`
//! or `did_change` processing.

use std::sync::Arc;
use std::time::Duration;

use super::super::pool::{
    ConnectionHandleSender, ConnectionKey, INIT_TIMEOUT_SECS, LanguageServerPool,
};
use super::super::protocol::VirtualDocumentUri;

/// What the caller requires to STILL hold by the time an eager open actually
/// runs. Both fields are preconditions checked inside the open, not inputs to
/// it, which is why they travel together.
/// Whether an eager open did what the caller asked for.
///
/// Only a caller that named a `connection` can act on the difference, but the
/// value is returned unconditionally so the compiler forces every caller to
/// decide (`let _ =` is an explicit "I open wherever it lands").
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub(crate) enum OpenOutcome {
    /// The open ran on the connection the caller expected (or the caller named
    /// none). Documents it could open are enqueued.
    Opened,
    /// This host supplies nothing for the named connection — it routes to a
    /// different one, or none of its injections bridge to that server.
    ///
    /// NOT a failure. The respawn re-open asks this of every open document, and
    /// for most of them the answer is "not mine": a markdown file with no lua
    /// fence, or one under a different workspace root. Counting those as failed
    /// repairs would hold the barrier shut on every respawn.
    NotApplicable,
    /// The open was applicable — this host does belong to the named connection
    /// — but did not happen: the connection is gone, the document moved to a
    /// new lifetime, or the handle was replaced mid-loop. The caller repairing
    /// that connection must report failure; it is still missing documents.
    NotOpened,
}

pub(crate) struct OpenExpectation<'a> {
    /// The document lifetime the injections were resolved under; a close+reopen
    /// in between invalidates them.
    pub(crate) incarnation: u64,
    /// The connection this open is FOR, when the caller is repairing a specific
    /// one — the respawn re-open is claimed under a key and its barrier signals
    /// for that key.
    ///
    /// Naming it does two things the `None` case does not: the open is ACQUIRED
    /// by that key rather than by whatever the host routes to (a config change
    /// that re-roots the host would otherwise repair a different connection
    /// while the claimed one stays empty and its barrier reports success), and
    /// the host is first checked to actually belong there, so a caller may ask
    /// about a host without risking an open on the wrong connection.
    ///
    /// `None` opens wherever the host routes now, spawning if needed, which is
    /// what every other caller wants.
    pub(crate) connection: Option<&'a ConnectionKey>,
}

struct LifecycleCleanup<'a> {
    pool: &'a LanguageServerPool,
    host_uri: &'a url::Url,
    lifecycle: &'a Arc<tokio::sync::Mutex<()>>,
}

impl Drop for LifecycleCleanup<'_> {
    fn drop(&mut self) {
        if self.pool.current_host_incarnation(self.host_uri).is_none() {
            self.pool
                .remove_host_lifecycle_lock_if_unshared(self.host_uri, self.lifecycle);
        }
    }
}

impl LanguageServerPool {
    /// Fire `didOpen` for every resolved bridge virtual URI so the downstream
    /// server starts analyzing immediately instead of waiting for the first
    /// user request. Fire-and-forget: per-document failures are logged at
    /// debug level and never propagated; one open failing leaves the others
    /// alone.
    pub(crate) async fn eager_open_virtual_documents(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        host_uri: &url::Url,
        host_uri_lsp: &tower_lsp_server::ls_types::Uri,
        expect: OpenExpectation<'_>,
        injections: Vec<crate::lsp::bridge::coordinator::BridgeInjection>,
    ) -> OpenOutcome {
        let OpenExpectation {
            incarnation: expected_incarnation,
            connection: expected_key,
        } = expect;
        // A caller repairing a NAMED connection is acquired BY KEY. Resolving
        // from `host_uri` would find whatever that host routes to *now*, which
        // after a re-rooting config change is a different connection — and then
        // the named one, the one whose barrier is about to be signalled and whose
        // token a routed command carries, is never repaired at all. Ready-only
        // and never spawning: this repairs a connection that exists, and a named
        // connection that has since died has nothing to restore.
        let handle = match expected_key {
            Some(key) => {
                // Does this host belong to the connection being repaired?
                // Acquiring by key cannot answer that: the lookup succeeds for
                // the key no matter which host asked, so a caller sweeping
                // several hosts would open documents rooted elsewhere onto this
                // connection. Resolve where the host actually routes and
                // compare. Read-only — unlike the `None` arm below it never
                // spawns, so asking about a host that belongs to some other
                // root cannot bring that root's server up.
                let (_marker, routes_to) = self
                    .resolve_acquire(server_name, server_config, Some(host_uri))
                    .await;
                if &routes_to != key {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "Eager open: {host_uri} routes to {routes_to}, not the \
                         requested {key}; it supplies nothing for that connection"
                    );
                    return OpenOutcome::NotApplicable;
                }
                match self
                    .ready_connection_by_key_for_config(key, Some(server_config))
                    .await
                {
                    Some(handle) => handle,
                    None => {
                        log::debug!(
                            target: "kakehashi::bridge",
                            "Eager open: connection {key} is gone; nothing to repair for \
                             {} injections",
                            injections.len()
                        );
                        return OpenOutcome::NotOpened;
                    }
                }
            }
            // Open wherever this host routes now, spawning if needed.
            None => match self
                .get_or_create_connection_wait_ready(
                    server_name,
                    server_config,
                    Some(host_uri),
                    Duration::from_secs(INIT_TIMEOUT_SECS),
                )
                .await
            {
                Ok(h) => h,
                Err(e) => {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "Eager open: server {} not ready, skipping didOpen for {} injections: {}",
                        server_name,
                        injections.len(),
                        e
                    );
                    return OpenOutcome::NotOpened;
                }
            },
        };

        // The by-key lookup returns the connection stored under that key, so
        // comparing `handle.key()` to `expected_key` again could never fail —
        // the question that CAN fail ("does this host route here?") is asked
        // above, before the lookup.
        let connection_key = handle.key().clone();
        let mut sender = ConnectionHandleSender(&handle);
        // Every injection's didOpen must actually be enqueued before this counts
        // as a completed open.
        let mut enqueued_all = true;

        let Some(lifecycle) = self.existing_host_lifecycle_lock(host_uri) else {
            // The host closed underneath us; nothing to open.
            return OpenOutcome::NotOpened;
        };
        let _lifecycle_cleanup = LifecycleCleanup {
            pool: self,
            host_uri,
            lifecycle: &lifecycle,
        };
        for injection in injections {
            // Hold the host cache guard through didOpen. didClose/reopen replaces
            // this entry, so it either linearizes after this open (and closes the
            // tracked virtual document) or wins first and makes this stale batch
            // stop without opening old content.
            let lifecycle_guard = lifecycle.lock().await;
            let current_incarnation = self.current_host_incarnation(host_uri);
            if current_incarnation != Some(expected_incarnation) {
                drop(lifecycle_guard);
                // The document was reopened under a new lifetime; these
                // injections are stale. Nothing further is opened.
                return OpenOutcome::NotOpened;
            }
            let virtual_uri =
                VirtualDocumentUri::new(host_uri_lsp, &injection.language, &injection.region_id);

            // Verify `handle` is still the pool's LIVE connection for its key and
            // claim + didOpen this ONE injection under the `connections` lock,
            // then release before the next iteration — the same respawn guard the
            // request (execute.rs) and host (host.rs) paths use, scoped per
            // injection so the (best-effort, possibly many-region) eager open
            // never holds the pool lock across the whole loop and stalls
            // unrelated requests/spawns/cancels. Without the guard, a concurrent
            // respawn (get_or_create's SpawnNew branch) that purged this key's
            // tracker could let a claim through the dead handle, marking the doc
            // open while the fresh process never received the didOpen. Lock order
            // connections → document tracker matches the respawn purge. If a
            // respawn replaced the handle mid-loop, stop: the purge lets the next
            // real request re-open cleanly.
            let connections = self.connections().await;
            if !connections
                .get(&connection_key)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager open: connection {} replaced mid-loop; stopping",
                    connection_key
                );
                // A respawn replaced the handle; the purge lets the next real
                // request re-open cleanly, but THIS open did not finish.
                return OpenOutcome::NotOpened;
            }

            if let Err(e) = self
                .ensure_document_opened(
                    &mut sender,
                    host_uri,
                    &virtual_uri,
                    &injection.content,
                    &connection_key,
                )
                .await
            {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager open: failed to open {} on {}: {}",
                    virtual_uri.to_uri_string(),
                    server_name,
                    e
                );
                // Keep opening the rest — one region failing does not make the
                // others unopenable — but do NOT report success. A claim that
                // did not settle, a full outbound queue, or a claim invalidated
                // mid-enqueue each mean this connection is missing a document it
                // should have, and a caller repairing it must hear so: that is
                // exactly the state in which releasing a command produces the
                // out-of-order delivery the barrier exists to prevent.
                enqueued_all = false;
            }
        }
        if enqueued_all {
            OpenOutcome::Opened
        } else {
            OpenOutcome::NotOpened
        }
    }

    /// Fire `didOpen` for the real host document on a `_self` host-bridge server
    /// (host-document-bridge), so a push-only host server (no `textDocument/diagnostic`
    /// support) starts analyzing and pushing diagnostics immediately, instead of
    /// only after the first host-bridged request lazily opens it. Fire-and-forget:
    /// failures are logged at debug and never propagated.
    pub(crate) async fn eager_open_host_document(
        &self,
        server_name: &str,
        server_config: &crate::config::settings::BridgeServerConfig,
        host_uri: &url::Url,
        language_id: &str,
        text: &str,
        live_text_reader: Option<&(dyn Fn() -> Option<Arc<str>> + Send + Sync)>,
    ) {
        let handle = match self
            .get_or_create_connection_wait_ready(
                server_name,
                server_config,
                Some(host_uri),
                Duration::from_secs(INIT_TIMEOUT_SECS),
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Eager host open: server {} not ready for {}: {}",
                    server_name,
                    host_uri,
                    e
                );
                return;
            }
        };

        // Borrow the key (no clone) — both `connections.get` and `sync_host_document`
        // take it by reference, like `execute_host_request`.
        let connection_key = handle.key();
        // Sync (sends didOpen) under the `connections` + `host_documents` locks in
        // that order, with the live-handle `Arc::ptr_eq` check — identical to
        // `execute_host_request`, so a concurrent respawn purge cannot interleave
        // and leave sync state the replacement never saw.
        let connections = self.connections().await;
        if !connections
            .get(connection_key)
            .is_some_and(|current| Arc::ptr_eq(current, &handle))
        {
            // Replaced by a respawn between wait-ready and here; the new connection
            // will sync lazily on its first request.
            return;
        }
        let mut docs = self.host_documents().await;
        let mut sender = ConnectionHandleSender(&handle);
        let doc = super::host::HostDocument {
            uri: host_uri,
            language_id,
            text,
        };
        if let Err(e) = super::host::sync_host_document(
            &mut sender,
            &mut docs,
            &doc,
            live_text_reader,
            connection_key,
        )
        .await
        {
            log::debug!(
                target: "kakehashi::bridge",
                "Eager host open: didOpen failed for {} on {}: {}",
                host_uri,
                server_name,
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::pool::test_helpers::*;
    use super::super::super::pool::{ConnectionState, LanguageServerPool};
    use super::super::super::protocol::VirtualDocumentUri;
    use super::{OpenExpectation, OpenOutcome};

    /// Test that eager_open_virtual_documents marks virtual documents as opened.
    ///
    /// Given a ready server and injection data, calling eager_open_virtual_documents
    /// should result in each virtual document being marked as opened in DocumentTracker.
    #[tokio::test]
    async fn eager_open_marks_documents_as_opened() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let server_name = "test-server";

        // Pre-create a ready connection so eager_open_virtual_documents finds it
        let handle = create_handle_with_key(
            ConnectionState::Ready,
            crate::lsp::bridge::ConnectionKey::for_server(server_name),
        )
        .await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("eager_open");
        let host_uri_lsp = url_to_uri(&host_uri);
        pool.open_host_incarnation(&host_uri, 1).await;

        use super::super::super::coordinator::BridgeInjection;
        let injections = vec![
            BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_0.to_string(),
                content: "print('hello')".to_string(),
            },
            BridgeInjection {
                language: "lua".to_string(),
                region_id: TEST_ULID_LUA_1.to_string(),
                content: "print('world')".to_string(),
            },
        ];

        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: None,
                },
                injections,
            )
            .await;
        assert_eq!(outcome, OpenOutcome::Opened);

        // Verify both virtual documents are marked as opened
        let vuri_0 = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        let vuri_1 = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_1);

        assert!(
            pool.is_document_opened(&vuri_0),
            "First virtual document should be marked as opened"
        );
        assert!(
            pool.is_document_opened(&vuri_1),
            "Second virtual document should be marked as opened"
        );
    }

    /// A host that has been re-rooted away from the connection being repaired
    /// supplies nothing for it — and the connection is left EMPTY rather than
    /// propped up with a document that no longer belongs to it.
    ///
    /// This is a deliberate consequence of deriving. The remembered-list design
    /// re-opened such a host on the claimed connection anyway, because the list
    /// asserted the host had been its document. Derived, current settings are
    /// the authority: the host now routes elsewhere, so this connection's
    /// correct contents are nothing, and `done` reporting success for an empty
    /// connection is accurate rather than a lie. A command still in flight
    /// against the old root fails downstream instead — the same outcome the
    /// routing ADR already accepts for a token naming a root no open document
    /// sits under.
    #[tokio::test]
    async fn a_rerooted_host_is_not_reopened_on_the_connection_it_left() {
        let pool = LanguageServerPool::new();
        let server_name = "lua_ls";
        let config = crate::lsp::bridge::pool::test_helpers::devnull_config_for_language("lua");

        // The connection the purge claimed, rooted where the host no longer
        // resolves to. It is present and Ready, so only routing refuses it.
        let claimed = crate::lsp::bridge::ConnectionKey::new(
            server_name,
            Some("file:///claimed".to_string()),
        );
        let handle = create_handle_with_key(ConnectionState::Ready, claimed.clone()).await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("eager_open_named_connection");
        let host_uri_lsp = url_to_uri(&host_uri);
        pool.open_host_incarnation(&host_uri, 1).await;

        use super::super::super::coordinator::BridgeInjection;
        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: Some(&claimed),
                },
                vec![BridgeInjection {
                    language: "lua".to_string(),
                    region_id: TEST_ULID_LUA_0.to_string(),
                    content: "print('hello')".to_string(),
                }],
            )
            .await;

        assert_eq!(outcome, OpenOutcome::NotApplicable);
        let vuri = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        assert!(
            !pool.is_document_opened_on_connection(&vuri, &claimed),
            "a host that routes elsewhere must not be opened here"
        );
    }

    /// A named connection that has since died has nothing to restore, and the
    /// caller must learn that: releasing a command onto it would hit an empty
    /// process.
    #[tokio::test]
    async fn eager_open_reports_not_opened_when_the_named_connection_is_gone() {
        let pool = LanguageServerPool::new();
        let server_name = "lua_ls";
        let config = crate::lsp::bridge::pool::test_helpers::devnull_config_for_language("lua");
        let host_uri = test_host_uri("eager_open_named_gone");
        let host_uri_lsp = url_to_uri(&host_uri);
        pool.open_host_incarnation(&host_uri, 1).await;

        use super::super::super::coordinator::BridgeInjection;
        let injections = vec![BridgeInjection {
            language: "lua".to_string(),
            region_id: TEST_ULID_LUA_0.to_string(),
            content: "print('hello')".to_string(),
        }];

        // The key this host routes to (client-root fallback, no marker above
        // `/test`), with NO connection under it. Naming a key the host does not
        // route to would be refused by the routing filter first and never reach
        // the acquisition this test is about.
        let gone = crate::lsp::bridge::ConnectionKey::for_server(server_name);
        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: Some(&gone),
                },
                injections,
            )
            .await;

        assert_eq!(
            outcome,
            OpenOutcome::NotOpened,
            "a gone connection must report failure, not success"
        );
        let vuri = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        assert!(
            !pool.is_document_opened(&vuri),
            "and must not have opened anything anywhere"
        );
    }

    /// A named connection is not enough on its own: the host must actually
    /// route there.
    ///
    /// Acquiring by key always succeeds for a key the pool holds, whichever
    /// host asked — so without this check a caller sweeping candidate hosts
    /// would open documents belonging to one root onto a connection serving
    /// another. The connection here is present and `Ready`, so the by-key
    /// acquisition WOULD hand it over; only the routing question refuses.
    #[tokio::test]
    async fn an_open_is_refused_for_a_connection_the_host_does_not_route_to() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let server_name = "test-server";

        // The host sits under no marker root, so it routes to the client
        // fallback — never to this marker-rooted key.
        let elsewhere = crate::lsp::bridge::ConnectionKey::new(
            server_name,
            Some("file:///elsewhere".to_string()),
        );
        let handle = create_handle_with_key(ConnectionState::Ready, elsewhere.clone()).await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("routes_elsewhere");
        let host_uri_lsp = url_to_uri(&host_uri);
        pool.open_host_incarnation(&host_uri, 1).await;

        use super::super::super::coordinator::BridgeInjection;
        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: Some(&elsewhere),
                },
                vec![BridgeInjection {
                    language: "lua".to_string(),
                    region_id: TEST_ULID_LUA_0.to_string(),
                    content: "print('hello')".to_string(),
                }],
            )
            .await;

        assert_eq!(outcome, OpenOutcome::NotApplicable);
        assert!(
            !pool.is_document_opened(&VirtualDocumentUri::new(
                &host_uri_lsp,
                "lua",
                TEST_ULID_LUA_0
            )),
            "nothing may be opened on a connection this host does not route to"
        );
    }

    /// The mirror of the above: when the host DOES route to the named
    /// connection, naming it opens exactly as an unnamed open would.
    #[tokio::test]
    async fn an_open_for_the_connection_the_host_routes_to_proceeds() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let server_name = "test-server";

        let routed_key = crate::lsp::bridge::ConnectionKey::for_server(server_name);
        let handle = create_handle_with_key(ConnectionState::Ready, routed_key.clone()).await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("routes_here");
        let host_uri_lsp = url_to_uri(&host_uri);
        pool.open_host_incarnation(&host_uri, 1).await;

        use super::super::super::coordinator::BridgeInjection;
        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: Some(&routed_key),
                },
                vec![BridgeInjection {
                    language: "lua".to_string(),
                    region_id: TEST_ULID_LUA_0.to_string(),
                    content: "print('hello')".to_string(),
                }],
            )
            .await;

        assert_eq!(outcome, OpenOutcome::Opened);
        assert!(pool.is_document_opened(&VirtualDocumentUri::new(
            &host_uri_lsp,
            "lua",
            TEST_ULID_LUA_0
        )));
    }

    /// Test that eager_open_virtual_documents is idempotent.
    ///
    /// Calling it twice with the same injections should not cause errors or
    /// duplicate didOpen notifications. The second call should be a no-op
    /// for already-opened documents.
    #[tokio::test]
    async fn eager_open_is_idempotent() {
        let pool = LanguageServerPool::new();
        let config = devnull_config();
        let server_name = "test-server";

        let handle = create_handle_with_key(
            ConnectionState::Ready,
            crate::lsp::bridge::ConnectionKey::for_server(server_name),
        )
        .await;
        pool.insert_connection(handle).await;

        let host_uri = test_host_uri("idempotent");
        let host_uri_lsp = url_to_uri(&host_uri);
        pool.open_host_incarnation(&host_uri, 1).await;

        use super::super::super::coordinator::BridgeInjection;
        let injections = vec![BridgeInjection {
            language: "lua".to_string(),
            region_id: TEST_ULID_LUA_0.to_string(),
            content: "print('hello')".to_string(),
        }];

        // First call - should open the document
        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: None,
                },
                injections.clone(),
            )
            .await;
        assert_eq!(outcome, OpenOutcome::Opened);

        let vuri = VirtualDocumentUri::new(&host_uri_lsp, "lua", TEST_ULID_LUA_0);
        assert!(
            pool.is_document_opened(&vuri),
            "Should be opened after first call"
        );

        // Second call - should be a no-op (idempotent)
        let outcome = pool
            .eager_open_virtual_documents(
                server_name,
                &config,
                &host_uri,
                &host_uri_lsp,
                OpenExpectation {
                    incarnation: 1,
                    connection: None,
                },
                injections,
            )
            .await;
        assert_eq!(
            outcome,
            OpenOutcome::Opened,
            "an idempotent re-open still reports the connection as open"
        );

        assert!(
            pool.is_document_opened(&vuri),
            "Should still be opened after second call"
        );
    }
}
