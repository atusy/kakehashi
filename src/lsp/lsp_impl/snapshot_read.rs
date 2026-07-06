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

/// The first-parse backstop shared by every snapshot wait (the token
/// handlers' `snapshot_for_tokens` and the explicit-action
/// `wait_for_current_snapshot`): generous on purpose, because it only runs
/// while the lifetime has NO snapshot and every open-parse resolution path
/// publishes one (tree, tree-less give-up, or the didClose sentinel) — the
/// wait is normally RELEASED by that publish long before this wall-clock
/// deadline; the deadline exists for the pathological case (a parse pipeline
/// that never resolves), and an unusually slow first parse that outruns it
/// degrades to the reader's empty fallback. One constant so the two reader
/// classes cannot drift apart.
pub(crate) const FIRST_PARSE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(15);

/// Settle backstop for the serve-current token readers (`semanticTokens`
/// full/delta): how long a token request may park waiting for the snapshot to
/// catch up with the live text before rejecting with `ContentModified`.
///
/// Like the first-parse backstop this is a wall-clock deadline that is
/// normally never reached: every edit's parse resolution publishes and
/// releases the park, so it expires only when the parse pipeline is slow
/// enough (saturation, a pathologically slow parse) that the snapshot cannot
/// catch up within it. Generous on purpose:
/// while the request parks, the client keeps drawing its previous tokens
/// (shifted by the editor across edits), which is strictly better than
/// receiving tokens computed for text it no longer has. On expiry the parse
/// loop's settle refresh re-drives the client once the snapshot lands.
///
/// This constant governs the *stale* park only (a snapshot exists but
/// trails). A document with NO snapshot for its lifetime parks on
/// [`FIRST_PARSE_BACKSTOP`] instead, regardless of the caller's `wait` — a
/// token reader's worst-case park is therefore the first-parse bound (15s),
/// not this.
pub(crate) const TOKEN_SETTLE_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(10);

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
        let first_parse_deadline = tokio::time::Instant::now() + FIRST_PARSE_BACKSTOP;
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
        // The incarnation clause guards the two-read window: per-lifetime
        // versions restart at 0, so a close+reopen landing between the reads
        // could pass the version equality with a dead lifetime's snapshot.
        (snapshot.incarnation == view.slot.current_incarnation
            && snapshot.parsed_version == view.content_version)
            .then_some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::LspService;

    fn server_with_doc(uri: &Url, text: &str) -> (LspService<Kakehashi>, u64) {
        let (service, _socket) = LspService::new(Kakehashi::new);
        service.inner().documents.insert(
            uri.clone(),
            text.to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = service
            .inner()
            .documents
            .latest_snapshot(uri)
            .expect("document just inserted")
            .slot
            .current_incarnation;
        (service, incarnation)
    }

    fn publish(service: &LspService<Kakehashi>, uri: &Url, text: &str, version: u64, inc: u64) {
        let landed = service
            .inner()
            .documents
            .get(uri)
            .map(|doc| {
                doc.publish_snapshot(Arc::new(ParseSnapshot {
                    text: Arc::from(text),
                    tree: None,
                    language: Some("rust".to_string()),
                    parsed_version: version,
                    incarnation: inc,
                    injection_regions: None,
                    bridge_regions: None,
                    resolved_regions: None,
                    layer_trees: std::sync::OnceLock::new(),
                }))
            })
            .unwrap_or(false);
        assert!(landed, "test publish must land");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_gone_for_unregistered_uri() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let uri = Url::parse("file:///nowhere.rs").unwrap();
        let outcome = service
            .inner()
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
            .await;
        assert!(matches!(outcome, SnapshotWait::Gone));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_current_immediately_when_snapshot_is_current() {
        let uri = Url::parse("file:///current.rs").unwrap();
        let (service, inc) = server_with_doc(&uri, "fn main() {}");
        publish(&service, &uri, "fn main() {}", 0, inc);
        let outcome = service
            .inner()
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
            .await;
        let SnapshotWait::Current(snapshot) = outcome else {
            panic!("expected Current for parsed_version == content_version");
        };
        assert_eq!(snapshot.parsed_version, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_stale_when_the_snapshot_keeps_trailing() {
        let uri = Url::parse("file:///stale.rs").unwrap();
        let (service, inc) = server_with_doc(&uri, "fn main() {}");
        publish(&service, &uri, "fn main() {}", 0, inc);
        // An edit bumps content_version past the published parse.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);
        let outcome = service
            .inner()
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
            .await;
        assert!(
            matches!(outcome, SnapshotWait::Stale),
            "a trailing snapshot past the settle wait is Stale"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_unparsed_when_no_parse_ever_publishes() {
        let uri = Url::parse("file:///unparsed.rs").unwrap();
        let (service, _inc) = server_with_doc(&uri, "fn main() {}");
        let outcome = service
            .inner()
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
            .await;
        assert!(
            matches!(outcome, SnapshotWait::Unparsed),
            "no snapshot for the lifetime by the first-parse backstop is Unparsed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_wakes_on_a_publish_landing_during_the_wait() {
        let uri = Url::parse("file:///late_publish.rs").unwrap();
        let (service, inc) = server_with_doc(&uri, "fn main() {}");
        let service = std::sync::Arc::new(service);
        let publisher = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                publish(&service, &uri, "fn main() {}", 0, inc);
            })
        };
        let outcome = service
            .inner()
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
            .await;
        publisher.await.unwrap();
        assert!(
            matches!(outcome, SnapshotWait::Current(_)),
            "a publish during the first-parse wait must wake the waiter"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_wakes_gone_on_didclose_during_the_wait() {
        let uri = Url::parse("file:///closed_mid_wait.rs").unwrap();
        let (service, _inc) = server_with_doc(&uri, "fn main() {}");
        let service = std::sync::Arc::new(service);
        let closer = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                service.inner().documents.remove(&uri);
            })
        };
        let outcome = service
            .inner()
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(200))
            .await;
        closer.await.unwrap();
        assert!(
            matches!(outcome, SnapshotWait::Gone),
            "the didClose sentinel must release a parked waiter as Gone"
        );
    }
}
