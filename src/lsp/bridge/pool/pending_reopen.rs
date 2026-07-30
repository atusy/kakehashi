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

use url::Url;

use super::ConnectionKey;
use crate::error::LockResultExt;

#[derive(Default)]
pub(crate) struct PendingReopenRegistry {
    hosts: Mutex<HashMap<ConnectionKey, Vec<Url>>>,
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

    /// Take the host documents awaiting re-open on `key`, leaving nothing behind.
    ///
    /// Draining means one re-open attempt per respawn. That is deliberate: a
    /// retained set would re-open the same documents on every later respawn of
    /// the key, including documents the editor has since closed.
    pub(crate) fn take(&self, key: &ConnectionKey) -> Vec<Url> {
        self.hosts
            .lock()
            .recover_poison("PendingReopenRegistry::take")
            .remove(key)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(path: &str) -> Url {
        Url::parse(path).expect("valid test URL")
    }

    #[test]
    fn take_drains_what_record_stored() {
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);

        assert_eq!(registry.take(&key), vec![url("file:///w/a.md")]);
        // Drained, not retained: a later respawn must not re-open documents the
        // editor may have closed in the meantime.
        assert!(registry.take(&key).is_empty());
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

        assert_eq!(registry.take(&key), vec![url("file:///w/a.md")]);
    }

    #[test]
    fn record_unions_without_duplicating() {
        let registry = PendingReopenRegistry::default();
        let key = ConnectionKey::for_server("ruff");
        registry.record(&key, vec![url("file:///w/a.md")]);
        registry.record(&key, vec![url("file:///w/a.md"), url("file:///w/b.md")]);

        assert_eq!(
            registry.take(&key),
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

        assert_eq!(registry.take(&a), vec![url("file:///w/a/doc.md")]);
        assert_eq!(registry.take(&b), vec![url("file:///w/b/doc.md")]);
    }
}
