//! Cancellation registry for downstream-initiated requests forwarded to the
//! editor (`window/showMessageRequest`, `window/showDocument`,
//! `workspace/applyEdit` — which is registered but answered locally, never
//! editor-bound, when the editor lacks the capability).
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc;

use super::ProgressConnectionId;
use crate::error::LockResultExt;

/// One in-flight forwarded request: its cancellation token plus a unique
/// `generation`. The generation lets [`InboundRequestRegistry::unregister`]
/// remove only the registration it owns, so a late unregister from a request
/// whose id was reused (and thus replaced) can't drop the newer entry.
struct Entry {
    generation: u64,
    token: CancellationToken,
}

/// Shared (cheaply cloneable) registry of in-flight forwarded requests. Nested
/// `connection → (request id → entry)` so per-id lookups don't clone the id and
/// a connection's requests can be cancelled and dropped together in one
/// O(its-entries) step.
#[derive(Clone, Default)]
pub(crate) struct InboundRequestRegistry {
    inner: Arc<Mutex<HashMap<ProgressConnectionId, HashMap<jsonrpc::Id, Entry>>>>,
    next_generation: Arc<AtomicU64>,
}

impl InboundRequestRegistry {
    /// Register an in-flight forwarded request, returning the token the
    /// forwarding loop awaits and the `generation` it must pass to
    /// [`Self::unregister`]. Called on the reader before the request is enqueued
    /// so a racing `$/cancelRequest` can't miss it.
    pub(crate) fn register(
        &self,
        connection_id: ProgressConnectionId,
        request_id: jsonrpc::Id,
    ) -> (CancellationToken, u64) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        let replaced = self
            .inner
            .lock()
            .recover_poison("InboundRequestRegistry::register")
            .entry(connection_id)
            .or_default()
            .insert(
                request_id,
                Entry {
                    generation,
                    token: token.clone(),
                },
            );
        // A well-behaved downstream never reuses a request id while one is in
        // flight, but if it does, cancel the orphaned request so its forwarded
        // editor request (and dialog) doesn't dangle unreachable. The orphan's
        // later unregister is a no-op — its generation no longer matches.
        if let Some(old) = replaced {
            old.token.cancel();
        }
        (token, generation)
    }

    /// Cancel a specific in-flight request (a downstream `$/cancelRequest`). The
    /// entry is left for the forwarding loop to remove when the request settles.
    pub(crate) fn cancel(&self, connection_id: ProgressConnectionId, request_id: &jsonrpc::Id) {
        if let Some(entry) = self
            .inner
            .lock()
            .recover_poison("InboundRequestRegistry::cancel")
            .get(&connection_id)
            .and_then(|requests| requests.get(request_id))
        {
            entry.token.cancel();
        }
    }

    /// Drop a settled request's entry, but only if it's still the registration
    /// identified by `generation`. A request whose id was reused has been
    /// replaced, so its late unregister must leave the newer entry in place.
    pub(crate) fn unregister(
        &self,
        connection_id: ProgressConnectionId,
        request_id: &jsonrpc::Id,
        generation: u64,
    ) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) = self
            .inner
            .lock()
            .recover_poison("InboundRequestRegistry::unregister")
            .entry(connection_id)
        {
            if entry
                .get()
                .get(request_id)
                .is_some_and(|e| e.generation == generation)
            {
                entry.get_mut().remove(request_id);
            }
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
            for entry in requests.into_values() {
                entry.token.cancel();
            }
        }
    }

    /// Test-only probe: whether an entry is currently registered. Production
    /// code must not branch on this (register/unregister own the lifecycle).
    #[cfg(test)]
    pub(crate) fn is_registered(
        &self,
        connection_id: ProgressConnectionId,
        request_id: &jsonrpc::Id,
    ) -> bool {
        self.inner
            .lock()
            .recover_poison("InboundRequestRegistry::is_registered")
            .get(&connection_id)
            .is_some_and(|requests| requests.contains_key(request_id))
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
        let (token, _gen) = registry.register(conn(1), jsonrpc::Id::Number(7));
        assert!(!token.is_cancelled());

        registry.cancel(conn(1), &jsonrpc::Id::Number(7));
        assert!(token.is_cancelled());
    }

    #[test]
    fn reusing_an_in_flight_id_cancels_the_orphan_and_keeps_the_new_entry() {
        let registry = InboundRequestRegistry::default();
        let (first, first_gen) = registry.register(conn(1), jsonrpc::Id::Number(7));
        // A misbehaving downstream reuses the id while the first is in flight.
        let (second, _second_gen) = registry.register(conn(1), jsonrpc::Id::Number(7));
        assert!(first.is_cancelled(), "the orphaned request is cancelled");
        assert!(!second.is_cancelled());

        // The orphan's late unregister (its own generation) must NOT drop the
        // live entry — the second request must stay cancellable.
        registry.unregister(conn(1), &jsonrpc::Id::Number(7), first_gen);
        registry.cancel(conn(1), &jsonrpc::Id::Number(7));
        assert!(
            second.is_cancelled(),
            "the live request is still cancellable"
        );
    }

    #[test]
    fn cancel_is_scoped_by_connection_and_id() {
        let registry = InboundRequestRegistry::default();
        let (other_conn, _) = registry.register(conn(2), jsonrpc::Id::Number(7));
        let (other_id, _) = registry.register(conn(1), jsonrpc::Id::Number(8));

        // Cancelling (conn 1, id 7) must not touch a different conn or id.
        registry.cancel(conn(1), &jsonrpc::Id::Number(7)); // not registered → no-op
        assert!(!other_conn.is_cancelled());
        assert!(!other_id.is_cancelled());
    }

    #[test]
    fn unregister_removes_the_entry() {
        let registry = InboundRequestRegistry::default();
        let (token, generation) = registry.register(conn(1), jsonrpc::Id::Number(7));
        registry.unregister(conn(1), &jsonrpc::Id::Number(7), generation);

        // After unregister, cancel can't reach the token.
        registry.cancel(conn(1), &jsonrpc::Id::Number(7));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_connection_fires_all_for_that_connection_only() {
        let registry = InboundRequestRegistry::default();
        let (a, _) = registry.register(conn(1), jsonrpc::Id::Number(1));
        let (b, _) = registry.register(conn(1), jsonrpc::Id::Number(2));
        let (other, _) = registry.register(conn(2), jsonrpc::Id::Number(1));

        registry.cancel_connection(conn(1));
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(!other.is_cancelled());
    }
}
