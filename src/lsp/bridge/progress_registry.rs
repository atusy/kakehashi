//! Work-done progress token remapping for the bridge.
//!
//! Downstream language servers declare their own progress tokens via
//! `window/workDoneProgress/create`. Each downstream chooses tokens
//! independently, so two downstreams can pick the *same* token (e.g. the
//! integer `1`). Forwarding those verbatim to the upstream editor would make
//! distinct progress operations collide. This registry mints a globally unique
//! upstream token for every downstream-declared token and remembers the mapping
//! so the bridge can translate in both directions:
//!
//! - **downstream → upstream** ([`ProgressRegistry::translate`]): rewrite the
//!   token on `$/progress` notifications before forwarding to the editor.
//! - **upstream → downstream** ([`ProgressRegistry::resolve_cancel`]): when the
//!   editor sends `window/workDoneProgress/cancel` for an upstream token, find
//!   the owning downstream connection and its original token.
//!
//! # Scope
//!
//! This only covers **server-declared** progress (tokens created via
//! `window/workDoneProgress/create`). Progress reported against a
//! *client-provided* `workDoneToken`/`partialResultToken` (carried in a
//! client-initiated request) is **not** remapped here: such tokens are already
//! unique because the client mints them. The reader treats unknown tokens as
//! out of scope (see `reader.rs`).
//!
//! Forward translation is keyed by [`ProgressConnectionId`] because the same
//! downstream token value may be used by different connections; the connection
//! id disambiguates them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tower_lsp_server::ls_types::NumberOrString;

use crate::error::LockResultExt;

use super::actor::OutboundMessage;

/// Opaque identifier for a downstream connection, used to scope forward token
/// translation so that identical downstream tokens on different connections do
/// not collide. Allocated by [`ProgressRegistry::new_connection_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ProgressConnectionId(u64);

impl ProgressConnectionId {
    /// Construct a connection id directly. Test-only: production ids come from
    /// [`ProgressRegistry::new_connection_id`] so they stay unique.
    #[cfg(test)]
    pub(crate) fn for_test(id: u64) -> Self {
        Self(id)
    }
}

/// Where a `window/workDoneProgress/cancel` for an upstream token should be
/// routed: the owning connection's writer channel plus the token that
/// connection originally declared.
struct CancelTarget {
    writer: tokio::sync::mpsc::Sender<OutboundMessage>,
    downstream_token: NumberOrString,
}

#[derive(Default)]
struct Inner {
    /// upstream token → cancel routing target.
    up_to_down: HashMap<NumberOrString, CancelTarget>,
    /// connection → (downstream token → upstream token). Nesting by connection
    /// keeps forward translation scoped (the same downstream token value may be
    /// used by different connections) and makes per-connection purge a single
    /// map removal. `NumberOrString` derives `Hash`/`Eq`, so the numeric token
    /// `1` and the string token `"1"` never collide as keys.
    down_to_up: HashMap<ProgressConnectionId, HashMap<NumberOrString, NumberOrString>>,
}

/// Bidirectional registry mapping downstream-declared progress tokens to
/// globally unique upstream tokens. Shared (`Arc`) between the pool, every
/// reader task, and the cancel-forwarding path.
pub(crate) struct ProgressRegistry {
    inner: Mutex<Inner>,
    /// Monotonic source of unique upstream token suffixes.
    token_counter: AtomicU64,
    /// Monotonic source of unique connection ids.
    connection_counter: AtomicU64,
}

impl ProgressRegistry {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            token_counter: AtomicU64::new(0),
            connection_counter: AtomicU64::new(0),
        }
    }

    /// Allocate a fresh connection id for a newly spawned downstream reader.
    pub(crate) fn new_connection_id(&self) -> ProgressConnectionId {
        ProgressConnectionId(self.connection_counter.fetch_add(1, Ordering::Relaxed))
    }

    /// Register a downstream-declared token and return the unique upstream token
    /// the editor should see. `writer` is the owning connection's writer channel,
    /// used later to route a cancel back to that downstream.
    ///
    /// Re-registering the same `(connection, downstream_token)` (a downstream
    /// that re-creates a token it already declared) replaces the prior mapping
    /// and mints a new upstream token; the stale upstream entry is dropped.
    pub(crate) fn register(
        &self,
        connection_id: ProgressConnectionId,
        downstream_token: NumberOrString,
        writer: tokio::sync::mpsc::Sender<OutboundMessage>,
    ) -> NumberOrString {
        let suffix = self.token_counter.fetch_add(1, Ordering::Relaxed);
        let upstream_token = NumberOrString::String(format!("kakehashi/bridge/progress/{suffix}"));

        let mut guard = self
            .inner
            .lock()
            .recover_poison("ProgressRegistry::register");
        let inner = &mut *guard;
        let stale = inner
            .down_to_up
            .entry(connection_id)
            .or_default()
            .insert(downstream_token.clone(), upstream_token.clone());
        if let Some(stale_upstream) = stale {
            inner.up_to_down.remove(&stale_upstream);
        }
        inner.up_to_down.insert(
            upstream_token.clone(),
            CancelTarget {
                writer,
                downstream_token,
            },
        );
        upstream_token
    }

    /// Translate a downstream token to its upstream token for `$/progress`
    /// forwarding. Returns `None` for tokens this registry never minted (e.g.
    /// client-provided `workDoneToken`s — out of scope, see module docs).
    pub(crate) fn translate(
        &self,
        connection_id: ProgressConnectionId,
        downstream_token: &NumberOrString,
    ) -> Option<NumberOrString> {
        let inner = self
            .inner
            .lock()
            .recover_poison("ProgressRegistry::translate");
        inner
            .down_to_up
            .get(&connection_id)
            .and_then(|tokens| tokens.get(downstream_token).cloned())
    }

    /// Drop the mapping for a finished progress operation (a `$/progress` with
    /// `WorkDoneProgress::End`). Idempotent.
    pub(crate) fn complete(
        &self,
        connection_id: ProgressConnectionId,
        downstream_token: &NumberOrString,
    ) {
        let mut guard = self
            .inner
            .lock()
            .recover_poison("ProgressRegistry::complete");
        let inner = &mut *guard;
        let removed = inner
            .down_to_up
            .get_mut(&connection_id)
            .and_then(|tokens| tokens.remove(downstream_token));
        if let Some(upstream_token) = removed {
            inner.up_to_down.remove(&upstream_token);
        }
    }

    /// Remove every mapping owned by a connection and return the upstream tokens
    /// that were live. Called when the connection's reader task exits (crash,
    /// shutdown, respawn) so dead entries don't leak and a later cancel can't
    /// route to a dead writer. The returned tokens let the forwarding loop drop
    /// its matching created-token admissions (window-work-done-progress).
    pub(crate) fn purge_connection(
        &self,
        connection_id: ProgressConnectionId,
    ) -> Vec<NumberOrString> {
        let mut guard = self
            .inner
            .lock()
            .recover_poison("ProgressRegistry::purge_connection");
        let inner = &mut *guard;
        let Some(tokens) = inner.down_to_up.remove(&connection_id) else {
            return Vec::new();
        };
        let upstream_tokens: Vec<NumberOrString> = tokens.into_values().collect();
        for upstream_token in &upstream_tokens {
            inner.up_to_down.remove(upstream_token);
        }
        upstream_tokens
    }

    /// Resolve an upstream token (from a client `window/workDoneProgress/cancel`)
    /// to the owning downstream's writer channel and original token. Returns
    /// `None` if the token is unknown (already completed, or never ours).
    ///
    /// The mapping is left in place: the cancelled downstream is still expected
    /// to emit a terminating `$/progress` End, which clears the entry via
    /// [`ProgressRegistry::complete`].
    pub(crate) fn resolve_cancel(
        &self,
        upstream_token: &NumberOrString,
    ) -> Option<(tokio::sync::mpsc::Sender<OutboundMessage>, NumberOrString)> {
        let inner = self
            .inner
            .lock()
            .recover_poison("ProgressRegistry::resolve_cancel");
        inner
            .up_to_down
            .get(upstream_token)
            .map(|target| (target.writer.clone(), target.downstream_token.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_writer() -> tokio::sync::mpsc::Sender<OutboundMessage> {
        // Capacity 1 is fine; these tests only register/translate, they never
        // send on the writer.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx
    }

    #[test]
    fn distinct_downstreams_get_distinct_upstream_tokens() {
        let reg = ProgressRegistry::new();
        let conn_a = reg.new_connection_id();
        let conn_b = reg.new_connection_id();

        // Both downstreams declare the SAME token value.
        let up_a = reg.register(conn_a, NumberOrString::Number(1), dummy_writer());
        let up_b = reg.register(conn_b, NumberOrString::Number(1), dummy_writer());

        assert_ne!(
            up_a, up_b,
            "colliding downstream tokens must not collide upstream"
        );
    }

    #[test]
    fn translate_is_connection_scoped() {
        let reg = ProgressRegistry::new();
        let conn_a = reg.new_connection_id();
        let conn_b = reg.new_connection_id();
        let token = NumberOrString::String("work".to_string());

        let up_a = reg.register(conn_a, token.clone(), dummy_writer());
        let up_b = reg.register(conn_b, token.clone(), dummy_writer());

        assert_eq!(reg.translate(conn_a, &token), Some(up_a));
        assert_eq!(reg.translate(conn_b, &token), Some(up_b));
    }

    #[test]
    fn translate_unknown_token_returns_none() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        assert_eq!(reg.translate(conn, &NumberOrString::Number(42)), None);
    }

    #[test]
    fn numeric_and_string_tokens_do_not_collide() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let num = NumberOrString::Number(1);
        let string = NumberOrString::String("1".to_string());

        let up_num = reg.register(conn, num.clone(), dummy_writer());
        let up_string = reg.register(conn, string.clone(), dummy_writer());

        assert_ne!(up_num, up_string);
        assert_eq!(reg.translate(conn, &num), Some(up_num));
        assert_eq!(reg.translate(conn, &string), Some(up_string));
    }

    #[test]
    fn resolve_cancel_returns_original_downstream_token() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(7);
        let upstream = reg.register(conn, downstream.clone(), dummy_writer());

        let (_writer, resolved) = reg
            .resolve_cancel(&upstream)
            .expect("registered token resolves");
        assert_eq!(resolved, downstream);
    }

    #[test]
    fn resolve_cancel_unknown_returns_none() {
        let reg = ProgressRegistry::new();
        assert!(
            reg.resolve_cancel(&NumberOrString::String("nope".to_string()))
                .is_none()
        );
    }

    #[test]
    fn complete_removes_both_directions() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(3);
        let upstream = reg.register(conn, downstream.clone(), dummy_writer());

        reg.complete(conn, &downstream);

        assert_eq!(reg.translate(conn, &downstream), None);
        assert!(reg.resolve_cancel(&upstream).is_none());
    }

    #[test]
    fn purge_connection_removes_only_that_connection() {
        let reg = ProgressRegistry::new();
        let conn_a = reg.new_connection_id();
        let conn_b = reg.new_connection_id();
        let token = NumberOrString::Number(1);
        let up_a = reg.register(conn_a, token.clone(), dummy_writer());
        let up_b = reg.register(conn_b, token.clone(), dummy_writer());

        reg.purge_connection(conn_a);

        assert_eq!(reg.translate(conn_a, &token), None);
        assert!(reg.resolve_cancel(&up_a).is_none());
        assert_eq!(reg.translate(conn_b, &token), Some(up_b.clone()));
        assert!(reg.resolve_cancel(&up_b).is_some());
    }

    #[test]
    fn re_register_replaces_stale_upstream_entry() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);

        let up_first = reg.register(conn, downstream.clone(), dummy_writer());
        let up_second = reg.register(conn, downstream.clone(), dummy_writer());

        assert_ne!(up_first, up_second);
        // Stale upstream token no longer routable.
        assert!(reg.resolve_cancel(&up_first).is_none());
        // New one is.
        assert!(reg.resolve_cancel(&up_second).is_some());
        assert_eq!(reg.translate(conn, &downstream), Some(up_second));
    }
}
