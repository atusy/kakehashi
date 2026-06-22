//! Cancellation registry for downstream-initiated requests forwarded to the
//! editor (`window/showMessageRequest`, `window/showDocument`).
//!
//! When a downstream server sends `$/cancelRequest` for such a request — or its
//! connection dies while one is in flight — the bridge must tell the editor to
//! cancel so a `showMessageRequest` dialog is dismissed. tower-lsp's `Client`
//! exposes no cancel API for an outgoing request, so the forwarding loop instead
//! sends the request with an id it minted (see `send_editor_request`) and, on
//! cancel, sends a correlated `$/cancelRequest` to the editor.
//!
//! This registry connects the two halves: the per-connection reader (which sees
//! the downstream `$/cancelRequest` and the connection lifecycle) registers each
//! in-flight forwarded request and fires its [`CancellationToken`]; the global
//! forwarding loop awaits that token. Keyed by `(connection, downstream request
//! id)`.
//!
//! Registration happens on the reader **before** the request is enqueued, so a
//! `$/cancelRequest` that races in immediately after can't miss it. The token is
//! also handed to the forwarding loop (via `UpstreamRequest`) so it awaits the
//! same one; the loop removes the entry once the request settles.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc;

use super::ProgressConnectionId;
use crate::error::LockResultExt;

/// Shared (cheaply cloneable) registry of in-flight forwarded requests and their
/// cancellation tokens. Nested `connection → (request id → token)` so per-id
/// lookups don't clone the id and a connection's requests can be cancelled and
/// dropped together in one O(its-entries) step.
#[derive(Clone, Default)]
pub(crate) struct InboundRequestRegistry {
    inner: Arc<Mutex<HashMap<ProgressConnectionId, HashMap<jsonrpc::Id, CancellationToken>>>>,
}

impl InboundRequestRegistry {
    /// Register an in-flight forwarded request and return the token the
    /// forwarding loop awaits. Called on the reader before the request is
    /// enqueued so a racing `$/cancelRequest` can't miss it.
    pub(crate) fn register(
        &self,
        connection_id: ProgressConnectionId,
        request_id: jsonrpc::Id,
    ) -> CancellationToken {
        let token = CancellationToken::new();
        self.inner
            .lock()
            .recover_poison("InboundRequestRegistry::register")
            .entry(connection_id)
            .or_default()
            .insert(request_id, token.clone());
        token
    }

    /// Cancel a specific in-flight request (a downstream `$/cancelRequest`). The
    /// entry is left for the forwarding loop to remove when the request settles.
    pub(crate) fn cancel(&self, connection_id: ProgressConnectionId, request_id: &jsonrpc::Id) {
        if let Some(token) = self
            .inner
            .lock()
            .recover_poison("InboundRequestRegistry::cancel")
            .get(&connection_id)
            .and_then(|requests| requests.get(request_id))
        {
            token.cancel();
        }
    }

    /// Drop a settled request's entry (called by the forwarding loop once the
    /// editor responded or the cancel was forwarded).
    pub(crate) fn unregister(&self, connection_id: ProgressConnectionId, request_id: &jsonrpc::Id) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) = self
            .inner
            .lock()
            .recover_poison("InboundRequestRegistry::unregister")
            .entry(connection_id)
        {
            entry.get_mut().remove(request_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// Cancel and drop every in-flight request for a connection — used when a
    /// downstream connection's reader exits, so its forwarded requests don't
    /// linger as open editor dialogs.
    pub(crate) fn cancel_connection(&self, connection_id: ProgressConnectionId) {
        if let Some(requests) = self
            .inner
            .lock()
            .recover_poison("InboundRequestRegistry::cancel_connection")
            .remove(&connection_id)
        {
            for token in requests.into_values() {
                token.cancel();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(n: u64) -> ProgressConnectionId {
        ProgressConnectionId::for_test(n)
    }

    #[test]
    fn cancel_fires_the_registered_token() {
        let registry = InboundRequestRegistry::default();
        let token = registry.register(conn(1), jsonrpc::Id::Number(7));
        assert!(!token.is_cancelled());

        registry.cancel(conn(1), &jsonrpc::Id::Number(7));
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_is_scoped_by_connection_and_id() {
        let registry = InboundRequestRegistry::default();
        let other_conn = registry.register(conn(2), jsonrpc::Id::Number(7));
        let other_id = registry.register(conn(1), jsonrpc::Id::Number(8));

        // Cancelling (conn 1, id 7) must not touch a different conn or id.
        registry.cancel(conn(1), &jsonrpc::Id::Number(7)); // not registered → no-op
        assert!(!other_conn.is_cancelled());
        assert!(!other_id.is_cancelled());
    }

    #[test]
    fn unregister_removes_the_entry() {
        let registry = InboundRequestRegistry::default();
        let token = registry.register(conn(1), jsonrpc::Id::Number(7));
        registry.unregister(conn(1), &jsonrpc::Id::Number(7));

        // After unregister, cancel can't reach the token.
        registry.cancel(conn(1), &jsonrpc::Id::Number(7));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_connection_fires_all_for_that_connection_only() {
        let registry = InboundRequestRegistry::default();
        let a = registry.register(conn(1), jsonrpc::Id::Number(1));
        let b = registry.register(conn(1), jsonrpc::Id::Number(2));
        let other = registry.register(conn(2), jsonrpc::Id::Number(1));

        registry.cancel_connection(conn(1));
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(!other.is_cancelled());
    }
}
