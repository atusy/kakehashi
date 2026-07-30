//! Host documents awaiting re-open on a respawned connection
//! (execute-command-routing-token).
//!
//! When a stale connection is purged, the replacement process has nothing open.
//! Nothing re-opens it on its own: `process_injections` eagerly opens virtual
//! documents after every parse, so an edit heals the gap — but a respawn with no
//! subsequent edit leaves the fresh process receiving requests for documents it
//! never opened.
//!
//! The purge is the last moment the affected set is knowable, since it is the
//! purge itself that forgets what the dead process held. So `purge_connection`
//! returns the host documents it dropped and they are recorded here, then drained
//! when the replacement reaches `Ready`.
//!
//! Held in an `Arc` on the pool (like the sibling `CommandOriginRegistry`)
//! because the drain runs inside the spawned handshake task, which cannot reach
//! `&self`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::watch;
use url::Url;

use super::ConnectionKey;
use crate::error::LockResultExt;

/// How long a request will wait for an in-flight re-open before proceeding
/// without it. Matches the bound the old inline pre-dispatch heal used: the
/// happy path (nothing pending, or already open) returns immediately, and a
/// stuck downstream must not hold a user-facing request open indefinitely.
pub(crate) const REOPEN_WAIT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct PendingReopenRegistry {
    hosts: Mutex<HashMap<ConnectionKey, Vec<Url>>>,
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
    /// Record the host documents a just-purged connection held.
    ///
    /// Unions rather than replaces: a respawn that dies before reaching `Ready`
    /// is purged again, and the second purge reports nothing (the first already
    /// emptied the tracker), so replacing would forget the set entirely.
    pub(crate) fn record(&self, key: &ConnectionKey, hosts: Vec<Url>) {
        if hosts.is_empty() {
            return;
        }
        let mut pending = self
            .hosts
            .lock()
            .recover_poison("PendingReopenRegistry::record");
        let entry = pending.entry(key.clone()).or_default();
        for host in hosts {
            if !entry.contains(&host) {
                entry.push(host);
            }
        }
    }

    /// Take the host documents awaiting re-open on `key` and mark the re-open
    /// in flight, returning the hosts and the completion sender.
    ///
    /// Draining means one re-open attempt per respawn. That is deliberate: a
    /// retained set would re-open the same documents on every later respawn of
    /// the key, including documents the editor has since closed.
    ///
    /// MUST be called before the connection is published as `Ready`. A request
    /// unblocked by that transition checks [`wait_for_reopen`](Self::wait_for_reopen),
    /// so registering afterwards would leave a window where the re-open is
    /// pending but invisible — exactly the overtaking this exists to prevent.
    ///
    /// Dropping the returned sender completes the wait, so a handler that dies
    /// or is never serviced releases waiters instead of stranding them until the
    /// timeout.
    pub(crate) fn take(&self, key: &ConnectionKey) -> Option<(Vec<Url>, watch::Sender<bool>)> {
        let hosts = self
            .hosts
            .lock()
            .recover_poison("PendingReopenRegistry::take")
            .remove(key)?;
        if hosts.is_empty() {
            return None;
        }
        let (tx, rx) = watch::channel(false);
        self.in_flight
            .lock()
            .recover_poison("PendingReopenRegistry::take")
            .insert(key.clone(), rx);
        Some((hosts, tx))
    }

    /// Put a claimed host set back after a hand-off failed, and retire the
    /// barrier it registered.
    ///
    /// [`take`](Self::take) DRAINS, so a handshake that claims the set and then
    /// dies before the re-open is queued would lose it permanently: the purge
    /// that recorded it already emptied the document tracker, so the next purge
    /// of the same key reports nothing to re-record. Restoring leaves it for the
    /// next replacement to claim.
    pub(crate) fn restore(&self, key: &ConnectionKey, hosts: Vec<Url>) {
        self.in_flight
            .lock()
            .recover_poison("PendingReopenRegistry::restore")
            .remove(key);
        self.record(key, hosts);
    }

    /// Wait (bounded by [`REOPEN_WAIT`]) for an in-flight re-open on `key`.
    ///
    /// Returns `true` when the ordering requirement is met — either nothing was
    /// outstanding (the common case) or the re-open finished. Returns `false` on
    /// timeout, where the re-open is still running.
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

    fn url(path: &str) -> Url {
        Url::parse(path).expect("valid test URL")
    }

    fn hosts_of(taken: Option<(Vec<Url>, watch::Sender<bool>)>) -> Vec<Url> {
        taken.map(|(hosts, _tx)| hosts).unwrap_or_default()
    }

    #[test]
    fn take_drains_what_record_stored() {
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);

        assert_eq!(hosts_of(registry.take(&key)), vec![url("file:///w/a.md")]);
        // Drained, not retained: a later respawn must not re-open documents the
        // editor may have closed in the meantime.
        assert!(registry.take(&key).is_none());
    }

    #[test]
    fn a_second_purge_before_ready_keeps_the_first_set() {
        // A respawn that dies during the handshake is purged again, and that
        // second purge reports nothing — the first one already emptied the
        // tracker. Replacing instead of unioning would lose the documents.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);
        registry.record(&key, vec![]);

        assert_eq!(hosts_of(registry.take(&key)), vec![url("file:///w/a.md")]);
    }

    #[test]
    fn record_unions_without_duplicating() {
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);
        registry.record(&key, vec![url("file:///w/a.md"), url("file:///w/b.md")]);

        assert_eq!(
            hosts_of(registry.take(&key)),
            vec![url("file:///w/a.md"), url("file:///w/b.md")]
        );
    }

    #[test]
    fn keys_do_not_share_a_pending_set() {
        // Sibling connections (same server, different root) respawn
        // independently; one reaching Ready must not consume the other's set.
        let registry = PendingReopenRegistry::default();
        let a = ConnectionKey::new("ruff", Some("file:///w/a".to_string()));
        let b = ConnectionKey::new("ruff", Some("file:///w/b".to_string()));
        registry.record(&a, vec![url("file:///w/a/doc.md")]);
        registry.record(&b, vec![url("file:///w/b/doc.md")]);

        assert_eq!(hosts_of(registry.take(&a)), vec![url("file:///w/a/doc.md")]);
        assert_eq!(hosts_of(registry.take(&b)), vec![url("file:///w/b/doc.md")]);
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
    async fn a_request_waits_until_the_reopen_signals_completion() {
        // The ordering guarantee: a command enqueued after Ready must not
        // overtake the didOpen its arguments depend on.
        let registry = std::sync::Arc::new(PendingReopenRegistry::default());
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);
        let (_hosts, tx) = registry.take(&key).expect("a re-open is pending");

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
        registry.record(&key, vec![url("file:///w/a.md")]);
        let (_hosts, tx) = registry.take(&key).expect("a re-open is pending");
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
        // The re-open ran but could not repair THIS connection (its host
        // re-routed, so the opens were skipped). Releasing the caller would send
        // a command to a connection that is still empty — the exact outcome the
        // barrier exists to prevent.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);
        let (_hosts, done) = registry.take(&key).expect("a re-open is pending");

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
        registry.record(&key, vec![url("file:///w/a.md")]);
        let (_hosts, done) = registry.take(&key).expect("a re-open is pending");

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

    #[tokio::test]
    async fn an_empty_pending_set_registers_no_wait() {
        // `record` ignores an empty set, so `take` must report nothing in flight
        // rather than register a barrier nobody will ever signal.
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![]);

        assert!(registry.take(&key).is_none());
        tokio::time::timeout(Duration::from_secs(1), registry.wait_for_reopen(&key))
            .await
            .expect("nothing in flight, so no wait");
    }
}
