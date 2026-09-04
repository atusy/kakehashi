//! One settle-retry waiter per (kind, host): a burst of deferrals during a
//! reload or a language publication must not spawn one polling task each,
//! all of which would then serialize through the host's republish lock (or
//! re-run the injection pass) once the language settles.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use url::Url;

use crate::error::LockResultExt;

/// The set of hosts that already have a settle-retry waiter of a given kind.
#[derive(Clone, Default)]
pub(crate) struct SettleRetryWaiters {
    active: Arc<Mutex<HashSet<(&'static str, Url)>>>,
}

/// Held by the one active waiter of its (kind, host); releases the slot on
/// drop, so every exit of the waiter — settled, expired, panicked — frees it.
pub(crate) struct SettleRetryClaim {
    waiters: SettleRetryWaiters,
    key: (&'static str, Url),
}

impl SettleRetryWaiters {
    /// Claim the (kind, host) slot; `None` when a waiter is already active,
    /// in which case the caller has nothing to do — the active waiter
    /// re-runs the same work once the language settles.
    pub(crate) fn claim(&self, kind: &'static str, host: &Url) -> Option<SettleRetryClaim> {
        let key = (kind, host.clone());
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
    pub(crate) fn is_active(&self, kind: &'static str, host: &Url) -> bool {
        self.active
            .lock()
            .recover_poison("SettleRetryWaiters::is_active")
            .contains(&(kind, host.clone()))
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
        let first = waiters.claim("republish", &host).expect("first claim");
        assert!(
            waiters.claim("republish", &host).is_none(),
            "second waiter coalesces"
        );
        assert!(
            waiters.claim("injection", &host).is_some(),
            "another kind is independent"
        );
        assert!(
            waiters.claim("republish", &other).is_some(),
            "another host is independent"
        );
        assert!(waiters.is_active("republish", &host));
        drop(first);
        assert!(!waiters.is_active("republish", &host));
        assert!(
            waiters.claim("republish", &host).is_some(),
            "the slot is free again"
        );
    }
}
