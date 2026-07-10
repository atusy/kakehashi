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
//! - **downstream → upstream** ([`ProgressRegistry::admit`]): decide, per
//!   `$/progress`, whether to announce/forward/drop, handing back the
//!   upstream token to rewrite onto forwarded notifications.
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
//!
//! # Lazy announcement (storm suppression)
//!
//! Registering a token does **not** immediately ask the editor to create it.
//! Some downstreams (e.g. basedpyright) declare a fresh progress token for
//! every analysis pass — thousands of `create` → `begin("")` → `end` triples
//! per minute of typing — and each forwarded `create` is a full editor
//! round-trip that queues ahead of real responses on the shared stdout sink.
//! Instead the registry tracks a per-token phase and [`ProgressRegistry::admit`]
//! decides, on the first `$/progress`, whether the token is worth announcing:
//! a `begin` with anything renderable (title, message, percentage, or a
//! cancel button) announces — create + begin forwarded, in order, on the same FIFO channel —
//! while a fully blank `begin` swallows the lifecycle's progress (a later
//! renderable `begin` reusing the token still upgrades it to announced).

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

/// Lifecycle phase of a registered token (lazy announcement, see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Registered via `create`, no `$/progress` seen yet; nothing sent upstream.
    Pending,
    /// A renderable `begin` arrived; the editor has been asked to create the
    /// token and subsequent progress is forwarded.
    Announced,
    /// A blank `begin` arrived; the lifecycle's progress is dropped (unless a
    /// later renderable `begin` reuses the token and upgrades it). Cleared by
    /// `End`, a re-`create`, or connection purge.
    Swallowed,
}

struct TokenEntry {
    upstream: NumberOrString,
    phase: Phase,
}

/// What the first `$/progress` for a token reveals about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressSignal {
    /// `begin` carrying something the editor can render (a non-empty title
    /// or message, a percentage, or `cancellable: true`) — worth announcing.
    RenderableBegin,
    /// `begin` with nothing renderable at all — the per-request storm shape.
    BlankBegin,
    /// `report`/`end` (or anything else). `admit` never distinguishes an
    /// `end` — the caller handles End's mapping cleanup via `complete`.
    Other,
}

/// Routing decision for one `$/progress` notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProgressAdmission {
    /// First renderable `begin`: send `window/workDoneProgress/create` for
    /// the token, then forward this notification with it.
    Announce(NumberOrString),
    /// Already announced: forward with the token.
    Forward(NumberOrString),
    /// Swallowed or meaningless without a `begin`: drop the notification.
    Drop,
}

/// A previously registered upstream token evicted by a re-`create`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvictedToken {
    pub(crate) token: NumberOrString,
    /// Whether the editor was ever asked to create it — only then does the
    /// forwarding loop hold an admission that needs forgetting.
    pub(crate) announced: bool,
}

#[derive(Default)]
struct Inner {
    /// upstream token → cancel routing target.
    up_to_down: HashMap<NumberOrString, CancelTarget>,
    /// connection → (downstream token → upstream entry). Nesting by connection
    /// keeps forward translation scoped (the same downstream token value may be
    /// used by different connections) and makes per-connection purge a single
    /// map removal. `NumberOrString` derives `Hash`/`Eq`, so the numeric token
    /// `1` and the string token `"1"` never collide as keys.
    down_to_up: HashMap<ProgressConnectionId, HashMap<NumberOrString, TokenEntry>>,
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

    /// Register a downstream-declared token and return `(upstream_token, stale)`:
    /// the unique upstream token the editor should see, plus the prior entry if
    /// this `(connection, downstream_token)` was already registered.
    /// `writer` is the owning connection's writer channel, used later to route a
    /// cancel back to that downstream.
    ///
    /// The token starts [`Phase::Pending`]: nothing is sent to the editor until
    /// [`ProgressRegistry::admit`] sees a renderable `begin` (lazy announcement,
    /// see module docs).
    ///
    /// Re-registering the same `(connection, downstream_token)` (a downstream that
    /// re-creates a token it already declared) replaces the prior mapping and
    /// mints a new upstream token; the evicted one is dropped from the registry
    /// and returned as `stale` so callers can also forget its loop admission —
    /// done in one locked pass (no separate `translate` lookup).
    pub(crate) fn register(
        &self,
        connection_id: ProgressConnectionId,
        downstream_token: NumberOrString,
        writer: tokio::sync::mpsc::Sender<OutboundMessage>,
    ) -> (NumberOrString, Option<EvictedToken>) {
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
            .insert(
                downstream_token.clone(),
                TokenEntry {
                    upstream: upstream_token.clone(),
                    phase: Phase::Pending,
                },
            )
            .map(|entry| EvictedToken {
                announced: entry.phase == Phase::Announced,
                token: entry.upstream,
            });
        if let Some(ref stale_upstream) = stale {
            inner.up_to_down.remove(&stale_upstream.token);
        }
        // Cancel routing is registered eagerly (before announcement): the
        // editor can only learn announced tokens, but keeping the entry from
        // birth means the pending→announced transition needs no second write
        // and a cancel can never race a half-registered token.
        inner.up_to_down.insert(
            upstream_token.clone(),
            CancelTarget {
                writer,
                downstream_token,
            },
        );
        (upstream_token, stale)
    }

    /// Decide how to route one `$/progress` for a registered token, advancing
    /// its phase (lazy announcement, see module docs). Returns `None` for
    /// tokens this registry never minted.
    ///
    /// - `Pending` + renderable `begin` → `Announce` (create + forward).
    /// - `Pending` + blank `begin` → swallow the lifecycle.
    /// - `Pending` + `report`/`end` (no `begin` seen) → drop: without a `begin`
    ///   the editor has no progress UI to update.
    /// - `Swallowed` + renderable `begin` (token reuse without an `end`) →
    ///   upgrade to `Announce` rather than lose renderable progress.
    pub(crate) fn admit(
        &self,
        connection_id: ProgressConnectionId,
        downstream_token: &NumberOrString,
        signal: ProgressSignal,
    ) -> Option<ProgressAdmission> {
        let mut guard = self.inner.lock().recover_poison("ProgressRegistry::admit");
        let entry = guard
            .down_to_up
            .get_mut(&connection_id)
            .and_then(|tokens| tokens.get_mut(downstream_token))?;
        Some(match (entry.phase, signal) {
            (Phase::Announced, _) => ProgressAdmission::Forward(entry.upstream.clone()),
            (Phase::Pending | Phase::Swallowed, ProgressSignal::RenderableBegin) => {
                entry.phase = Phase::Announced;
                ProgressAdmission::Announce(entry.upstream.clone())
            }
            (Phase::Pending, ProgressSignal::BlankBegin) => {
                entry.phase = Phase::Swallowed;
                ProgressAdmission::Drop
            }
            (Phase::Pending | Phase::Swallowed, _) => ProgressAdmission::Drop,
        })
    }

    /// Translate a downstream token to its upstream token. Returns `None` for
    /// tokens this registry never minted. Production forwarding goes through
    /// [`ProgressRegistry::admit`] (which also advances the phase); this
    /// phase-blind lookup remains for test assertions on the mapping itself.
    #[cfg(test)]
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
            .and_then(|tokens| tokens.get(downstream_token))
            .map(|entry| entry.upstream.clone())
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
        if let Some(entry) = removed {
            inner.up_to_down.remove(&entry.upstream);
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
        let upstream_tokens: Vec<NumberOrString> =
            tokens.into_values().map(|entry| entry.upstream).collect();
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
        let (up_a, _) = reg.register(conn_a, NumberOrString::Number(1), dummy_writer());
        let (up_b, _) = reg.register(conn_b, NumberOrString::Number(1), dummy_writer());

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

        let (up_a, _) = reg.register(conn_a, token.clone(), dummy_writer());
        let (up_b, _) = reg.register(conn_b, token.clone(), dummy_writer());

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

        let (up_num, _) = reg.register(conn, num.clone(), dummy_writer());
        let (up_string, _) = reg.register(conn, string.clone(), dummy_writer());

        assert_ne!(up_num, up_string);
        assert_eq!(reg.translate(conn, &num), Some(up_num));
        assert_eq!(reg.translate(conn, &string), Some(up_string));
    }

    #[test]
    fn resolve_cancel_returns_original_downstream_token() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(7);
        let (upstream, _) = reg.register(conn, downstream.clone(), dummy_writer());

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
        let (upstream, _) = reg.register(conn, downstream.clone(), dummy_writer());

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
        let (up_a, _) = reg.register(conn_a, token.clone(), dummy_writer());
        let (up_b, _) = reg.register(conn_b, token.clone(), dummy_writer());

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

        let (up_first, stale_first) = reg.register(conn, downstream.clone(), dummy_writer());
        let (up_second, stale_second) = reg.register(conn, downstream.clone(), dummy_writer());

        assert_ne!(up_first, up_second);
        // First registration evicts nothing; the second evicts and returns the first.
        assert_eq!(stale_first, None);
        assert_eq!(
            stale_second,
            Some(EvictedToken {
                token: up_first.clone(),
                announced: false,
            })
        );
        // Stale upstream token no longer routable.
        assert!(reg.resolve_cancel(&up_first).is_none());
        // New one is.
        assert!(reg.resolve_cancel(&up_second).is_some());
        assert_eq!(reg.translate(conn, &downstream), Some(up_second));
    }

    #[test]
    fn re_register_of_swallowed_token_reports_unannounced() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);

        let (up_first, _) = reg.register(conn, downstream.clone(), dummy_writer());
        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::BlankBegin),
            Some(ProgressAdmission::Drop)
        );

        let (_, stale) = reg.register(conn, downstream.clone(), dummy_writer());
        assert_eq!(
            stale,
            Some(EvictedToken {
                token: up_first,
                announced: false,
            })
        );
    }

    #[test]
    fn admit_is_connection_scoped() {
        let reg = ProgressRegistry::new();
        let conn_a = reg.new_connection_id();
        let conn_b = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);
        let (_up, _) = reg.register(conn_a, downstream.clone(), dummy_writer());

        assert_eq!(
            reg.admit(conn_b, &downstream, ProgressSignal::RenderableBegin),
            None,
            "a foreign connection's token must not admit"
        );
    }

    /// Purge is deliberately phase-blind: it returns every phase's upstream
    /// token. The Forget the purge guard sends for never-announced tokens is
    /// a no-op in the forwarding loop (they were never admitted), so
    /// filtering here would buy nothing on the rare crash path.
    #[test]
    fn purge_connection_returns_tokens_of_every_phase() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let pending = NumberOrString::Number(1);
        let announced = NumberOrString::Number(2);
        let swallowed = NumberOrString::Number(3);
        let (up_pending, _) = reg.register(conn, pending.clone(), dummy_writer());
        let (up_announced, _) = reg.register(conn, announced.clone(), dummy_writer());
        let (up_swallowed, _) = reg.register(conn, swallowed.clone(), dummy_writer());
        reg.admit(conn, &announced, ProgressSignal::RenderableBegin);
        reg.admit(conn, &swallowed, ProgressSignal::BlankBegin);

        let mut purged = reg.purge_connection(conn);
        purged.sort_by_key(|t| format!("{t:?}"));
        let mut expected = vec![
            up_pending.clone(),
            up_announced.clone(),
            up_swallowed.clone(),
        ];
        expected.sort_by_key(|t| format!("{t:?}"));
        assert_eq!(purged, expected);
        for up in [up_pending, up_announced, up_swallowed] {
            assert!(reg.resolve_cancel(&up).is_none(), "purged token routable");
        }
    }

    #[test]
    fn re_register_reports_whether_evicted_token_was_announced() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);

        let (up_first, _) = reg.register(conn, downstream.clone(), dummy_writer());
        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::RenderableBegin),
            Some(ProgressAdmission::Announce(up_first.clone()))
        );

        let (_, stale) = reg.register(conn, downstream.clone(), dummy_writer());
        assert_eq!(
            stale,
            Some(EvictedToken {
                token: up_first,
                announced: true,
            })
        );
    }

    #[test]
    fn admit_renderable_begin_announces_then_forwards() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);
        let (upstream, _) = reg.register(conn, downstream.clone(), dummy_writer());

        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::RenderableBegin),
            Some(ProgressAdmission::Announce(upstream.clone()))
        );
        // Subsequent report/end forward without re-announcing.
        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::Other),
            Some(ProgressAdmission::Forward(upstream))
        );
    }

    #[test]
    fn admit_blank_begin_swallows_lifecycle() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);
        let (_upstream, _) = reg.register(conn, downstream.clone(), dummy_writer());

        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::BlankBegin),
            Some(ProgressAdmission::Drop)
        );
        // report/end for the swallowed token stay dropped.
        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::Other),
            Some(ProgressAdmission::Drop)
        );
    }

    #[test]
    fn admit_report_or_end_without_begin_is_dropped() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);
        let (_upstream, _) = reg.register(conn, downstream.clone(), dummy_writer());

        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::Other),
            Some(ProgressAdmission::Drop)
        );
    }

    #[test]
    fn admit_renderable_begin_after_swallow_upgrades_to_announce() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        let downstream = NumberOrString::Number(1);
        let (upstream, _) = reg.register(conn, downstream.clone(), dummy_writer());

        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::BlankBegin),
            Some(ProgressAdmission::Drop)
        );
        assert_eq!(
            reg.admit(conn, &downstream, ProgressSignal::RenderableBegin),
            Some(ProgressAdmission::Announce(upstream))
        );
    }

    #[test]
    fn admit_unknown_token_returns_none() {
        let reg = ProgressRegistry::new();
        let conn = reg.new_connection_id();
        assert_eq!(
            reg.admit(
                conn,
                &NumberOrString::Number(9),
                ProgressSignal::RenderableBegin
            ),
            None
        );
    }
}
