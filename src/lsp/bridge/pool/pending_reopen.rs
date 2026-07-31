//! Connections owing a virtual-document re-open after a respawn
//! (respawn-reopen-derives-its-targets).
//!
//! When a stale connection is purged, the replacement process has nothing open.
//! Nothing re-opens it on its own: `process_injections` eagerly opens virtual
//! documents after every parse, so an edit heals the gap — but a respawn with no
//! subsequent edit leaves the fresh process receiving requests for documents it
//! never opened.
//!
//! What this registry stores is a KEY, not a document set. A purge ARMS its key;
//! the replacement's handshake CLAIMS it and the re-open then derives what the
//! connection should hold from the documents that are open now. Remembering the
//! dead process's set instead made the record a snapshot of a past state that
//! kept diverging from the present — closed documents, re-rooted hosts, a second
//! purge reporting nothing — and each divergence needed its own repair.
//!
//! Held in an `Arc` on the pool (like the sibling `CommandOriginRegistry`)
//! because the claim runs inside the spawned handshake task, which cannot reach
//! `&self`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;

use super::ConnectionKey;
use crate::error::LockResultExt;

/// How long a request will wait for an in-flight re-open before proceeding
/// without it. Matches the bound the old inline pre-dispatch heal used: the
/// happy path (nothing pending, or already open) returns immediately, and a
/// stuck downstream must not hold a user-facing request open indefinitely.
pub(crate) const REOPEN_WAIT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct PendingReopenRegistry {
    /// Keys whose connection was purged and whose replacement still owes a
    /// re-open.
    armed: Mutex<HashSet<ConnectionKey>>,
    /// Re-opens handed off but not yet finished, so a request that must not
    /// overtake its own `didOpen` can wait for one.
    ///
    /// The re-open is serviced asynchronously (the pool signals *when*, the
    /// server side does the work), so without this a command enqueued right
    /// after `Ready` would reach the downstream BEFORE the didOpen it depends
    /// on — the FIFO ordering the previous inline heal guaranteed by awaiting.
    in_flight: Mutex<HashMap<ConnectionKey, watch::Receiver<bool>>>,
}

impl PendingReopenRegistry {
    /// Note that `key`'s connection was purged, so its replacement owes a
    /// re-open.
    ///
    /// UNCONDITIONAL, and that is load-bearing. The predecessor recorded a host
    /// list and skipped when it was empty — but a connection that dies young
    /// holds nothing, so the purge that follows it reported nothing, armed
    /// nothing, and its replacement was never repaired by anyone. Arming on the
    /// purge itself has no such hole: what the dead connection happened to hold
    /// is not what the replacement needs.
    ///
    /// Idempotent — a key purged twice before any replacement lands is armed
    /// once, and one re-open brings the replacement fully up to date.
    pub(crate) fn arm(&self, key: &ConnectionKey) {
        self.armed
            .lock()
            .recover_poison("PendingReopenRegistry::arm")
            .insert(key.clone());
    }

    /// Claim `key`'s outstanding re-open and mark it in flight, returning the
    /// completion sender. `None` when nothing was armed — a first-ever spawn,
    /// which has no predecessor's state to restore.
    ///
    /// MUST be called before the connection is published as `Ready`. A request
    /// unblocked by that transition checks [`wait_for_reopen`](Self::wait_for_reopen),
    /// so registering afterwards would leave a window where the re-open is
    /// pending but invisible — exactly the overtaking this exists to prevent.
    ///
    /// Dropping the returned sender completes the wait, so a handler that dies
    /// or is never serviced releases waiters instead of stranding them until the
    /// timeout.
    pub(crate) fn claim(&self, key: &ConnectionKey) -> Option<watch::Sender<bool>> {
        if !self
            .armed
            .lock()
            .recover_poison("PendingReopenRegistry::claim")
            .remove(key)
        {
            return None;
        }
        let (tx, rx) = watch::channel(false);
        self.in_flight
            .lock()
            .recover_poison("PendingReopenRegistry::claim")
            .insert(key.clone(), rx);
        Some(tx)
    }

    /// Re-arm after a claimed hand-off failed, and retire the barrier THAT
    /// claim registered.
    ///
    /// [`claim`](Self::claim) disarms, so a handshake that claims and then dies
    /// before the re-open is queued would leave the replacement owing a repair
    /// nobody remembers to make.
    ///
    /// Takes the claim's own `done` so the retire can be identity-guarded, for
    /// the same reason [`wait_for_reopen`](Self::wait_for_reopen) guards its
    /// own: claim → publish-Ready → rearm is not atomic against the registry,
    /// so a LATER respawn can claim this key in between and install a live
    /// barrier. Removing by key alone would evict that one, and a command
    /// arriving next finds nothing outstanding, reads the requirement as
    /// vacuously met, and is enqueued ahead of the didOpens the live re-open
    /// has not sent yet — the one failure mode this whole mechanism exists to
    /// prevent, and the one case that does NOT degrade to the lazy heal.
    pub(crate) fn rearm(&self, key: &ConnectionKey, done: &watch::Sender<bool>) {
        let probe = done.subscribe();
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .recover_poison("PendingReopenRegistry::rearm");
            if in_flight
                .get(key)
                .is_some_and(|current| current.same_channel(&probe))
            {
                in_flight.remove(key);
            }
        }
        self.arm(key);
    }

    /// Wait (bounded by [`REOPEN_WAIT`]) for an in-flight re-open on `key`.
    ///
    /// Returns `true` when the ordering requirement is met — either nothing was
    /// outstanding (the common case) or the re-open reported that it repaired
    /// this connection.
    ///
    /// Returns `false` in three cases, all meaning "this connection may still be
    /// missing documents": the wait timed out and the re-open is still running;
    /// the re-open reported it could not repair this connection; or its sender
    /// was dropped before reporting success (a handler that died).
    ///
    /// The caller must NOT proceed on `false`. A bounded wait means the guarantee
    /// can be unmet, and sending anyway is the failure this barrier exists to
    /// prevent — the command would reach the downstream ahead of the `didOpen` it
    /// depends on and fail there instead. Failing soft costs the user one
    /// no-op action they can re-fire; sending unordered costs them a confusing
    /// downstream error.
    pub(crate) async fn wait_for_reopen(&self, key: &ConnectionKey) -> bool {
        let Some(mut rx) = self
            .in_flight
            .lock()
            .recover_poison("PendingReopenRegistry::wait_for_reopen")
            .get(key)
            .cloned()
        else {
            // Nothing outstanding: the ordering requirement is vacuously met.
            return true;
        };
        // Three outcomes, and they are not the same thing:
        // - observed `true`  → the re-open repaired this connection; go ahead.
        // - sender dropped   → no further news will come. Trust the last value:
        //   a re-open that reported failure (or died before reporting) leaves
        //   the connection empty, so the caller must NOT send. Retire the entry
        //   regardless — nothing will ever settle it, and keeping it would fail
        //   every later command on this key forever.
        // - timed out        → still running. Keep the entry so the next
        //   command is bounded by the same budget rather than sailing past.
        // Collapse to a plain discriminant first: `wait_for`'s `Ok` holds a
        // `Ref` borrowing `rx`, and the sender-gone arm needs to read `rx` again.
        enum Waited {
            Repaired,
            SenderGone,
            TimedOut,
        }
        let waited = match tokio::time::timeout(REOPEN_WAIT, rx.wait_for(|done| *done)).await {
            Ok(Ok(_)) => Waited::Repaired,
            Ok(Err(_)) => Waited::SenderGone,
            Err(_) => Waited::TimedOut,
        };
        let (settled, retire) = match waited {
            Waited::Repaired => (true, true),
            Waited::SenderGone => (*rx.borrow(), true),
            Waited::TimedOut => (false, false),
        };
        // Two awaits separate the clone above from this retire, so a LATER
        // respawn may have claimed the key in between and registered a genuinely
        // outstanding re-open. Removing by key alone would evict it.
        if retire {
            let mut in_flight = self
                .in_flight
                .lock()
                .recover_poison("PendingReopenRegistry::wait_for_reopen");
            if in_flight
                .get(key)
                .is_some_and(|current| current.same_channel(&rx))
            {
                in_flight.remove(key);
            }
        }
        settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_disarms_what_arm_set() {
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);

        assert!(registry.claim(&key).is_some());
        // Disarmed, not retained: one respawn owes one re-open. A retained key
        // would make every later respawn of it re-derive for no reason.
        assert!(registry.claim(&key).is_none());
    }

    #[test]
    fn a_connection_that_died_holding_nothing_still_arms() {
        // The hole the remembered-list design could not close. A connection
        // purged before it opened anything reported an EMPTY set, which armed
        // nothing — so its replacement was never repaired by anyone, and no
        // amount of restoring helped because the set had never existed. Arming
        // on the purge itself does not consult what was held.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);

        assert!(
            registry.claim(&key).is_some(),
            "a purge must arm its key regardless of what the dead connection held"
        );
    }

    #[test]
    fn arming_twice_before_a_claim_owes_one_reopen() {
        // A respawn that dies during its own handshake is purged again. One
        // derivation brings the eventual replacement fully up to date, so the
        // second purge must not queue a second one.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        registry.arm(&key);

        assert!(registry.claim(&key).is_some());
        assert!(registry.claim(&key).is_none());
    }

    #[test]
    fn rearm_puts_back_a_claim_that_could_not_be_handed_off() {
        // A handshake that claims and then fails to publish Ready (or to queue
        // the re-open) leaves the connection still owing one.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        let done = registry.claim(&key).expect("armed");
        registry.rearm(&key, &done);
        drop(done);

        assert!(
            registry.claim(&key).is_some(),
            "the next replacement must still learn it owes a re-open"
        );
    }

    #[tokio::test]
    async fn rearm_retires_the_barrier_the_failed_claim_registered() {
        // The claim registered an in-flight entry; nobody will ever signal it.
        // Leaving it would block every command on the key for the full budget.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        let done = registry.claim(&key).expect("armed");
        registry.rearm(&key, &done);
        drop(done);

        assert!(
            tokio::time::timeout(Duration::from_secs(1), registry.wait_for_reopen(&key))
                .await
                .expect("a retired barrier must not make the caller wait"),
            "a re-armed key has no re-open in flight to wait for"
        );
    }

    /// A late `rearm` must not retire a barrier a LATER respawn registered.
    ///
    /// claim -> publish-Ready -> rearm is not atomic against this registry, so a
    /// replacement can claim the same key in between and install a live barrier.
    /// Retiring by key alone would evict it, and the next command would find
    /// nothing outstanding, read the ordering requirement as vacuously met, and
    /// be enqueued ahead of didOpens that have not been sent — the one failure
    /// this mechanism exists to prevent, and the one that does NOT degrade to
    /// the lazy heal.
    #[tokio::test]
    async fn a_stale_rearm_does_not_retire_a_later_respawns_barrier() {
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");

        // The displaced handshake claims...
        registry.arm(&key);
        let stale = registry.claim(&key).expect("armed");
        // ...is displaced, so its key is armed again and a replacement claims.
        registry.arm(&key);
        let live = registry.claim(&key).expect("re-armed");

        // Only NOW does the displaced handshake notice it lost the Ready flip.
        registry.rearm(&key, &stale);
        drop(stale);

        // The live re-open is still outstanding, so a command must wait for it.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), registry.wait_for_reopen(&key))
                .await
                .is_err(),
            "the live respawn's barrier must survive a stale rearm"
        );
        live.send(true).expect("the registry holds the receiver");
        assert!(registry.wait_for_reopen(&key).await);
    }

    #[test]
    fn keys_are_armed_independently() {
        // Sibling connections (same server, different root) respawn
        // independently; one reaching Ready must not consume the other's claim.
        let registry = PendingReopenRegistry::default();
        let a = ConnectionKey::new("ruff", Some("file:///w/a".to_string()));
        let b = ConnectionKey::new("ruff", Some("file:///w/b".to_string()));
        registry.arm(&a);

        assert!(registry.claim(&a).is_some());
        assert!(
            registry.claim(&b).is_none(),
            "arming one root must not arm its sibling"
        );
    }

    #[tokio::test]
    async fn waiting_on_a_key_with_no_reopen_returns_at_once() {
        // The overwhelmingly common case: nothing was purged, so a request must
        // not pay the timeout (or any wait at all).
        let registry = PendingReopenRegistry::default();
        assert!(
            registry
                .wait_for_reopen(&ConnectionKey::for_server("ruff"))
                .await,
            "nothing outstanding means the ordering requirement is vacuously met"
        );
    }

    #[tokio::test]
    async fn an_armed_but_unclaimed_key_registers_no_wait() {
        // Arming records a debt; only the claim puts a re-open in flight. A
        // request arriving between the two must not wait for a barrier that
        // nobody is holding.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);

        assert!(
            tokio::time::timeout(Duration::from_secs(1), registry.wait_for_reopen(&key))
                .await
                .expect("nothing in flight, so no wait"),
        );
    }

    #[tokio::test]
    async fn a_request_waits_until_the_reopen_signals_completion() {
        // The ordering guarantee: a command enqueued after Ready must not
        // overtake the didOpen its arguments depend on.
        let registry = std::sync::Arc::new(PendingReopenRegistry::default());
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        let tx = registry.claim(&key).expect("a re-open is pending");

        let waiter = {
            let registry = std::sync::Arc::clone(&registry);
            let key = key.clone();
            tokio::spawn(async move { registry.wait_for_reopen(&key).await })
        };
        // Still in flight: the waiter must not have finished.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "must block while the re-open runs");

        tx.send(true).expect("waiter holds the receiver");
        waiter.await.expect("waiter completes once signalled");
    }

    #[tokio::test]
    async fn dropping_the_sender_releases_waiters() {
        // A handler that dies (or a message never serviced) must not strand the
        // request for the full timeout.
        let registry = std::sync::Arc::new(PendingReopenRegistry::default());
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        let tx = registry.claim(&key).expect("a re-open is pending");
        drop(tx);

        // Would hang for REOPEN_WAIT if a dropped sender did not count as done.
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), registry.wait_for_reopen(&key))
                .await
                .expect("a dropped sender releases the waiter immediately"),
            "a re-open that died without reporting success leaves the connection \
             empty, so the caller must NOT send"
        );
        // ...but the entry is retired, so it cannot block every later command.
        assert!(
            tokio::time::timeout(Duration::from_secs(1), registry.wait_for_reopen(&key))
                .await
                .expect("no wait"),
            "a settled-by-drop entry must be retired, not left blocking forever"
        );
    }

    #[tokio::test]
    async fn a_reopen_that_reports_failure_does_not_release_the_caller() {
        // The re-open ran but could not bring THIS connection up to date (a
        // document that belongs to it would not open). Releasing the caller
        // would send a command to a connection still missing documents — the
        // exact outcome the barrier exists to prevent.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        let done = registry.claim(&key).expect("a re-open is pending");

        done.send(false).expect("the registry holds the receiver");
        drop(done);

        assert!(
            !tokio::time::timeout(Duration::from_secs(1), registry.wait_for_reopen(&key))
                .await
                .expect("a reported failure must not make the caller wait out the budget"),
            "a re-open that reported failure must not release the caller"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_wait_leaves_the_reopen_registered() {
        // A timeout does not mean the re-open finished. Retiring the entry there
        // would let the NEXT request skip the wait entirely — with no bound at
        // all — exactly when the re-open is known to be slow.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.arm(&key);
        let done = registry.claim(&key).expect("a re-open is pending");

        // Auto-advances past REOPEN_WAIT without a real sleep. The wait did NOT
        // settle, so the caller must be told to fail soft rather than send
        // without the ordering guarantee.
        assert!(
            !registry.wait_for_reopen(&key).await,
            "an unfinished re-open must report NOT settled"
        );

        // Still outstanding, so a second request is bounded the same way rather
        // than sailing straight through.
        let second = tokio::time::timeout(REOPEN_WAIT / 2, registry.wait_for_reopen(&key)).await;
        assert!(
            second.is_err(),
            "a still-running re-open must keep blocking, not be forgotten"
        );
        done.send(true).expect("the registry holds the receiver");
        assert!(
            registry.wait_for_reopen(&key).await,
            "a completed re-open reports settled"
        );
    }
}
