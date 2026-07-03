//! Shared snapshot-read helpers for the request handlers (parse-snapshot ADR
//! §3): resolve the latest [`ParseSnapshot`] under each reader class's
//! staleness policy. Readers never parse inline; the only permitted waits are
//! the bounded first-parse wait and the explicit-action wait.

use std::sync::Arc;

use url::Url;

use crate::document::snapshot::ParseSnapshot;

use super::Kakehashi;

/// Outcome of a bounded wait for a **current** snapshot
/// (`parsed_version == content_version`).
pub(crate) enum SnapshotWait {
    /// A current snapshot landed within the wait.
    Current(Arc<ParseSnapshot>),
    /// Deadline passed with only a trailing snapshot — the reader's
    /// staleness-reject signal applies (`ContentModified` / `null`).
    Stale,
    /// Deadline passed with no snapshot for this lifetime (first parse still
    /// pending) — the reader's empty/`null` fallback applies.
    Unparsed,
    /// Unregistered or closed.
    Gone,
}

impl Kakehashi {
    /// Wait (bounded) until `uri`'s latest snapshot is current, re-resolving
    /// the cell per wakeup (per-request re-resolution + incarnation validation
    /// happen inside `latest_snapshot`). This is the ADR's explicit-action
    /// wait (`formatting` / `rename` / `selectionRange`) and doubles as the
    /// first-parse wait.
    pub(crate) async fn wait_for_current_snapshot(
        &self,
        uri: &Url,
        wait: std::time::Duration,
    ) -> SnapshotWait {
        // Two deadlines: the caller's `wait` bounds the SETTLE wait (a
        // snapshot exists but trails the input — degrading fast there is the
        // point), while the FIRST-parse wait is generous, because it is
        // bounded by parse completion rather than time: every open-parse
        // resolution path publishes (tree, tree-less, or the didClose
        // sentinel), so the receiver always wakes. A tight first-parse cap
        // made requests racing didOpen degrade to empty on loaded machines.
        let stale_deadline = tokio::time::Instant::now() + wait;
        let first_parse_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            // Subscribe BEFORE checking (lost-wakeup guard): `subscribe` marks
            // the current value as seen, so a publish landing between a check
            // and a later subscribe would never trigger `changed()`.
            let Some(mut receiver) = self.documents.subscribe_snapshots(uri) else {
                return SnapshotWait::Gone;
            };
            let Some(view) = self.documents.latest_snapshot(uri) else {
                return SnapshotWait::Gone;
            };
            let had_snapshot = match &view.slot.snapshot {
                Some(snapshot) if snapshot.parsed_version == view.content_version => {
                    return SnapshotWait::Current(Arc::clone(snapshot));
                }
                trailing => trailing.is_some(),
            };
            let deadline = if had_snapshot {
                stale_deadline
            } else {
                first_parse_deadline
            };
            match tokio::time::timeout_at(deadline, receiver.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_closed)) => return SnapshotWait::Gone,
                Err(_deadline) => {
                    return if had_snapshot {
                        SnapshotWait::Stale
                    } else {
                        SnapshotWait::Unparsed
                    };
                }
            }
        }
    }

    /// Resolve a **current** snapshot for the position/range readers
    /// (`kakehashi/node/*`, the bridge-context requests): a trailing snapshot
    /// rejects **immediately** — these are implicit/background requests, the
    /// client's next natural request heals — while a not-yet-parsed document
    /// gets only the bounded first-parse wait (`snapshot_for_tokens` waits
    /// only when no snapshot exists at all). `None` covers gone, unparsed,
    /// and stale alike: every caller's contract collapses those to its
    /// unresolvable signal.
    ///
    /// Currency is also what makes the callers' tracker mints safe: the
    /// shared `NodeTracker` is a live-position (`content_version`) index, so
    /// minting from a snapshot is only sound when the snapshot IS the live
    /// version (ADR §3 — a stale read never mints).
    pub(crate) async fn current_snapshot(&self, uri: &Url) -> Option<Arc<ParseSnapshot>> {
        let snapshot = self.snapshot_for_tokens(uri).await?;
        let view = self.documents.latest_snapshot(uri)?;
        (snapshot.parsed_version == view.content_version).then_some(snapshot)
    }
}
