//! One settle-retry waiter per (kind, host): a burst of deferrals during a
//! reload or a language publication must not spawn one polling task each,
//! all of which would then serialize through the host's republish lock (or
//! re-run the injection pass) once the language settles.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use url::Url;

use crate::error::LockResultExt;

/// The set of hosts that already have a settle-retry waiter of a given kind.
///
/// A waiter bound to a document lifetime carries that lifetime in its key:
/// a close and reopen at the same URI while it is active must get its own
/// waiter, since the old one exits at its lifetime check once the language
/// settles and would otherwise take the reopened document's retry with it.
/// (kind, host, lifetime the waiter is bound to — `None` for host-level work).
type WaiterKey = (&'static str, Url, Option<u64>);

#[derive(Clone, Default)]
pub(crate) struct SettleRetryWaiters {
    active: Arc<Mutex<HashSet<WaiterKey>>>,
}

/// Held by the one active waiter of its (kind, host, lifetime); releases the
/// slot on drop, so every exit of the waiter — settled, expired, panicked —
/// frees it.
pub(crate) struct SettleRetryClaim {
    waiters: SettleRetryWaiters,
    key: WaiterKey,
}

impl SettleRetryWaiters {
    /// Claim the (kind, host, lifetime) slot; `None` when a waiter is already
    /// active, in which case the caller has nothing to do — the active waiter
    /// re-runs the same work once the language settles.
    pub(crate) fn claim(
        &self,
        kind: &'static str,
        host: &Url,
        lifetime: Option<u64>,
    ) -> Option<SettleRetryClaim> {
        let key = (kind, host.clone(), lifetime);
        let inserted = self
            .active
            .lock()
            .recover_poison("SettleRetryWaiters::claim")
            .insert(key.clone());
        inserted.then(|| SettleRetryClaim {
            waiters: self.clone(),
            key,
        })
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self, kind: &'static str, host: &Url, lifetime: Option<u64>) -> bool {
        self.active
            .lock()
            .recover_poison("SettleRetryWaiters::is_active")
            .contains(&(kind, host.clone(), lifetime))
    }
}

impl Drop for SettleRetryClaim {
    fn drop(&mut self) {
        self.waiters
            .active
            .lock()
            .recover_poison("SettleRetryClaim::drop")
            .remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::SettleRetryWaiters;
    use url::Url;

    #[test]
    fn one_waiter_per_kind_and_host_until_it_drops() {
        let waiters = SettleRetryWaiters::default();
        let host = Url::parse("file:///a.md").unwrap();
        let other = Url::parse("file:///b.md").unwrap();
        let first = waiters
            .claim("republish", &host, None)
            .expect("first claim");
        assert!(
            waiters.claim("republish", &host, None).is_none(),
            "second waiter coalesces"
        );
        assert!(
            waiters.claim("injection", &host, Some(1)).is_some(),
            "another kind is independent"
        );
        assert!(
            waiters.claim("republish", &other, None).is_some(),
            "another host is independent"
        );
        assert!(waiters.is_active("republish", &host, None));
        drop(first);
        assert!(!waiters.is_active("republish", &host, None));
        assert!(
            waiters.claim("republish", &host, None).is_some(),
            "the slot is free again"
        );
    }

    /// A reopened lifetime's retry must not be dropped because the closed
    /// lifetime's waiter still holds the URI.
    #[test]
    fn a_reopened_lifetime_gets_its_own_waiter() {
        let waiters = SettleRetryWaiters::default();
        let host = Url::parse("file:///a.md").unwrap();
        let _closed = waiters
            .claim("injection", &host, Some(1))
            .expect("lifetime 1");
        assert!(
            waiters.claim("injection", &host, Some(1)).is_none(),
            "same lifetime coalesces"
        );
        assert!(
            waiters.claim("injection", &host, Some(2)).is_some(),
            "lifetime 2 is its own retry"
        );
    }
}
