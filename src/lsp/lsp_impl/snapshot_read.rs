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
    /// Deadline passed with only a trailing snapshot, or (explicit actions
    /// only) the text moved on from the one current at entry — the reader's
    /// staleness-reject signal applies (`ContentModified` / `null`).
    Stale,
    /// Deadline passed with no parse of the current text: no snapshot for
    /// this lifetime (first parse still pending), or — for a wait that does
    /// not accept it — a reload placeholder whose reparse is still pending.
    /// The reader's empty/`null` fallback applies.
    Unparsed,
    /// Unregistered or closed.
    Gone,
}

/// The first-parse backstop shared by every snapshot wait (the token
/// handlers' `snapshot_for_tokens` and every `wait_for_snapshot_in` caller,
/// including the explicit-action `wait_for_explicit_action_snapshot`):
/// generous on purpose, because it only runs while the lifetime has NO
/// snapshot and every open-parse resolution path publishes one (tree,
/// tree-less give-up, or the didClose sentinel) — the wait is normally RELEASED by that publish long before this wall-clock
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

/// How long an explicit action (formatting / range formatting /
/// selectionRange) waits for a trailing snapshot to catch up before
/// rejecting with `ContentModified`, or for a reload placeholder's reparse
/// before falling back to the unparsed answer.
pub(crate) const EXPLICIT_ACTION_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

impl Kakehashi {
    /// Resolve one snapshot's whole-document regions. `None` means the
    /// parser/query pair could not be read consistently; `Some(empty)` means
    /// the settled language has no injection query or no matching regions.
    pub(super) fn whole_document_regions(
        &self,
        uri: &Url,
        snapshot: &ParseSnapshot,
    ) -> Option<Arc<Vec<crate::language::injection::ResolvedInjection>>> {
        use crate::error::LockResultExt;
        let reloading = || {
            self.parser_pool
                .lock()
                .recover_poison("Kakehashi::whole_document_regions")
                .reload_in_progress()
        };
        if reloading() {
            return None;
        }
        let generation = self.cache.semantic_token_generation();
        let language = snapshot.language.as_deref()?;
        // Request entry already requires a published parser through language
        // detection. Recheck after the snapshot wait: a reload can invalidate
        // that registration between entry and this read. Loading it here would
        // not repair requests rejected by the earlier detection gate.
        if !self.language.has_parser_available(language) {
            return None;
        }
        let tree = snapshot.tree.as_ref()?;
        let regions = if let Some(regions) = snapshot.regions_for_generation(generation) {
            Arc::clone(&regions.whole_document)
        } else if let Some(query) = self.language.injection_query(language) {
            Arc::new(crate::language::InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                uri,
                tree,
                &snapshot.text,
                &query,
                snapshot.incarnation,
            ))
        } else {
            Arc::new(Vec::new())
        };
        // A reload may have started or completed during any of the reads,
        // including a snapshot-cache hit. Neither result is then evidence.
        (!reloading() && self.cache.semantic_token_generation() == generation).then_some(regions)
    }

    /// Wait (bounded) until `uri`'s latest snapshot is current, re-resolving
    /// the cell per wakeup (per-request re-resolution + incarnation validation
    /// happen inside `latest_snapshot`). It doubles as the first-parse wait,
    /// and accepts a settings reload's placeholder as current; the explicit
    /// actions wait past it through
    /// [`wait_for_explicit_action_snapshot`](Self::wait_for_explicit_action_snapshot).
    pub(crate) async fn wait_for_current_snapshot(
        &self,
        uri: &Url,
        wait: std::time::Duration,
    ) -> SnapshotWait {
        wait_for_current_snapshot_in(&self.documents, uri, wait).await
    }

    /// The explicit-action bounded wait (parse-snapshot ADR §3) for the
    /// user-triggered formatting requests: infrequent and consciously
    /// triggered, so they may briefly wait for the in-flight parse rather
    /// than silently no-op.
    ///
    /// Unlike [`wait_for_current_snapshot`](Self::wait_for_current_snapshot),
    /// a settings reload's placeholder does not end the wait: it is not a
    /// parse, so the action settles for the reparse it awaits and reads the
    /// placeholder still standing at the deadline as [`SnapshotWait::Unparsed`].
    pub(crate) async fn wait_for_explicit_action_snapshot(&self, uri: &Url) -> SnapshotWait {
        wait_for_snapshot_in(
            &self.documents,
            uri,
            EXPLICIT_ACTION_WAIT,
            ReloadPlaceholder::AwaitReparse,
        )
        .await
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

/// The body of [`Kakehashi::wait_for_current_snapshot`], over a bare
/// [`DocumentStore`].
///
/// Free-standing because the coordinators reached from the bridge's upward
/// request channel hold a `DocumentStore` but not a `Kakehashi`, and the
/// respawn re-open must wait for a current tree the same way the request paths
/// do (execute-command-routing-token) — resolving injections against a tree
/// `didChange` just cleared would find none and open nothing.
pub(crate) async fn wait_for_current_snapshot_in(
    documents: &crate::document::DocumentStore,
    uri: &Url,
    wait: std::time::Duration,
) -> SnapshotWait {
    wait_for_snapshot_in(documents, uri, wait, ReloadPlaceholder::Accept).await
}

/// How a wait treats a settings reload's placeholder
/// ([`ParseSnapshot::awaiting_reparse`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReloadPlaceholder {
    /// It is current: the reader serves its missing tree like any tree-less
    /// parse and relies on its own heal path.
    Accept,
    /// It is not a parse: keep waiting (bounded by the settle wait) for the
    /// reparse, and report one still standing at the deadline as `Unparsed`.
    /// The wait answers only for the text current at entry: an edit landing
    /// meanwhile reads as `Stale`, even once its own reparse is current.
    AwaitReparse,
}

async fn wait_for_snapshot_in(
    documents: &crate::document::DocumentStore,
    uri: &Url,
    wait: std::time::Duration,
    placeholder: ReloadPlaceholder,
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
    let mut expired = false;
    // An explicit action answers for the text it was sent against, so only
    // it pins the entry lineage; a newer edit during the wait reads stale.
    let mut request_lineage = None;
    loop {
        // Subscribe BEFORE checking (lost-wakeup guard): `subscribe` marks
        // the current value as seen, so a publish landing between a check
        // and a later subscribe would never trigger `changed()`.
        let Some(mut receiver) = documents.subscribe_snapshots(uri) else {
            return SnapshotWait::Gone;
        };
        let Some(view) = documents.latest_snapshot(uri) else {
            return SnapshotWait::Gone;
        };
        if placeholder == ReloadPlaceholder::AwaitReparse {
            let lineage = (view.slot.current_incarnation, view.content_version);
            if *request_lineage.get_or_insert(lineage) != lineage {
                return SnapshotWait::Stale;
            }
        }
        let (had_snapshot, awaiting_reparse) = match &view.slot.snapshot {
            Some(snapshot) if snapshot.parsed_version == view.content_version => {
                if !(snapshot.awaiting_reparse && placeholder == ReloadPlaceholder::AwaitReparse) {
                    return SnapshotWait::Current(Arc::clone(snapshot));
                }
                (true, true)
            }
            trailing => (trailing.is_some(), false),
        };
        if expired {
            // Judged on a fresh read: an edit publishes no snapshot, so one
            // landing mid-wait (turning a waited-on placeholder into a
            // trailing snapshot) never woke the wait.
            return if had_snapshot && !awaiting_reparse {
                SnapshotWait::Stale
            } else {
                SnapshotWait::Unparsed
            };
        }
        let deadline = if had_snapshot {
            stale_deadline
        } else {
            first_parse_deadline
        };
        match tokio::time::timeout_at(deadline, receiver.changed()).await {
            Ok(Ok(())) => continue,
            Ok(Err(_closed)) => return SnapshotWait::Gone,
            // Re-resolve once more, then answer from that read.
            Err(_deadline) => expired = true,
        }
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
                doc.publish_snapshot(&Arc::new(ParseSnapshot {
                    text: Arc::from(text),
                    tree: None,
                    language: Some("rust".to_string()),
                    parsed_version: version,
                    incarnation: inc,
                    injection_regions: None,
                    regions: None,
                    layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
                    awaiting_reparse: false,
                }))
            })
            .unwrap_or(false);
        assert!(landed, "test publish must land");
    }

    #[tokio::test]
    async fn whole_document_regions_distinguishes_unavailable_empty_and_cached() {
        let uri = Url::parse("file:///regions.rs").unwrap();
        let text = r#"fn main() { let html = "<div>"; }"#;
        let (service, inc) = server_with_doc(&uri, text);
        let server = service.inner();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let mut snapshot = ParseSnapshot {
            text: Arc::from(text),
            tree: Some(parser.parse(text, None).unwrap()),
            language: Some("rust".into()),
            parsed_version: 0,
            incarnation: inc,
            injection_regions: None,
            regions: None,
            layer_trees: Arc::new(std::sync::OnceLock::new()),
            awaiting_reparse: false,
        };
        assert!(
            server.whole_document_regions(&uri, &snapshot).is_none(),
            "unpublished parser is not evidence"
        );
        server
            .language
            .language_registry_for_parallel()
            .register("rust".into(), language.clone());
        assert!(
            server
                .whole_document_regions(&uri, &snapshot)
                .unwrap()
                .is_empty(),
            "published parser without a query is definitive"
        );
        let query = tree_sitter::Query::new(&language, r#"((string_literal (string_content) @injection.content) (#set! injection.language "html"))"#).unwrap();
        server
            .language
            .query_store()
            .insert_injection_query("rust".into(), Arc::new(query));
        let regions = server.whole_document_regions(&uri, &snapshot).unwrap();
        assert_eq!(regions.len(), 1);
        snapshot.regions = Some(crate::document::snapshot::ResolvedRegions {
            generation: server.cache.semantic_token_generation(),
            bridge: Arc::new(Vec::new()),
            whole_document: Arc::clone(&regions),
        });
        assert!(
            Arc::ptr_eq(
                &regions,
                &server.whole_document_regions(&uri, &snapshot).unwrap()
            ),
            "settled cache is reused"
        );
        let reload = super::super::ParserReloadGuard::begin(&server.parser_pool);
        assert!(
            server.whole_document_regions(&uri, &snapshot).is_none(),
            "even matching-generation cache is unavailable during reload"
        );
        server.cache.bump_semantic_token_generation();
        drop(reload);
        let refreshed = server.whole_document_regions(&uri, &snapshot).unwrap();
        assert_eq!(refreshed.len(), 1);
        assert!(
            !Arc::ptr_eq(&regions, &refreshed),
            "generation mismatch resolves inline rather than using old regions"
        );
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

    fn rust_tree(text: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
    }

    /// A settings reload's placeholder is version-current but is not a parse:
    /// an explicit action settles for the reparse it awaits, not for the
    /// placeholder's missing tree.
    #[tokio::test(start_paused = true)]
    async fn explicit_action_wait_settles_for_the_reparse_behind_a_reload_placeholder() {
        let uri = Url::parse("file:///reload_placeholder.rs").unwrap();
        let text = "fn main() {}";
        let (service, inc) = server_with_doc(&uri, text);
        let server = service.inner();
        publish(&service, &uri, text, 0, inc);
        server.documents.invalidate_all_parses();
        let reload_version = server
            .documents
            .latest_snapshot(&uri)
            .unwrap()
            .content_version;

        let reparse = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let landed = server.documents.get(&uri).is_some_and(|doc| {
                doc.publish_snapshot(&Arc::new(ParseSnapshot {
                    text: Arc::from(text),
                    tree: Some(rust_tree(text)),
                    language: Some("rust".to_string()),
                    parsed_version: reload_version,
                    incarnation: inc,
                    injection_regions: None,
                    regions: None,
                    layer_trees: Arc::new(std::sync::OnceLock::new()),
                    awaiting_reparse: false,
                }))
            });
            assert!(landed, "the reparse must land over the placeholder");
        };
        let (outcome, ()) = tokio::join!(server.wait_for_explicit_action_snapshot(&uri), reparse);

        let SnapshotWait::Current(snapshot) = outcome else {
            panic!("the reparse is current");
        };
        assert!(
            snapshot.tree.is_some(),
            "settled on the placeholder instead"
        );
    }

    /// A reparse that outlasts the wait leaves the live text unparsed, not
    /// the action's coordinates stale; the readers that accept the
    /// placeholder still get it at once.
    #[tokio::test(start_paused = true)]
    async fn a_reload_placeholder_outlasting_the_wait_reads_unparsed_for_explicit_actions_only() {
        let uri = Url::parse("file:///slow_reparse.rs").unwrap();
        let text = "fn main() {}";
        let (service, inc) = server_with_doc(&uri, text);
        let server = service.inner();
        publish(&service, &uri, text, 0, inc);
        server.documents.invalidate_all_parses();

        assert!(matches!(
            server.wait_for_explicit_action_snapshot(&uri).await,
            SnapshotWait::Unparsed
        ));
        let SnapshotWait::Current(snapshot) = server
            .wait_for_current_snapshot(&uri, std::time::Duration::ZERO)
            .await
        else {
            panic!("the default wait accepts the placeholder as current");
        };
        assert!(snapshot.awaiting_reparse);
    }

    /// Edits publish no snapshot, so one landing mid-wait does not wake the
    /// waiter; the deadline must still see that the placeholder now trails.
    #[tokio::test(start_paused = true)]
    async fn an_edit_during_the_placeholder_wait_reads_stale_at_the_deadline() {
        let uri = Url::parse("file:///edit_during_reload.rs").unwrap();
        let text = "fn main() {}";
        let (service, inc) = server_with_doc(&uri, text);
        let server = service.inner();
        publish(&service, &uri, text, 0, inc);
        server.documents.invalidate_all_parses();

        let edit = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            server
                .documents
                .update_document(uri.clone(), "fn main() { }".to_string(), None);
        };
        let (outcome, ()) = tokio::join!(server.wait_for_explicit_action_snapshot(&uri), edit);

        assert!(matches!(outcome, SnapshotWait::Stale));
    }

    /// An explicit action answers for the text it was sent against: a newer
    /// edit reparsed during the wait is not that text.
    #[tokio::test(start_paused = true)]
    async fn an_edit_reparsed_during_the_placeholder_wait_reads_stale() {
        let uri = Url::parse("file:///edit_reparsed_during_reload.rs").unwrap();
        let text = "fn main() {}";
        let edited = "fn main() { }";
        let (service, inc) = server_with_doc(&uri, text);
        let server = service.inner();
        publish(&service, &uri, text, 0, inc);
        server.documents.invalidate_all_parses();

        let edit = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            server
                .documents
                .update_document(uri.clone(), edited.to_string(), None);
            let version = server
                .documents
                .latest_snapshot(&uri)
                .unwrap()
                .content_version;
            publish(&service, &uri, edited, version, inc);
        };
        let (outcome, ()) = tokio::join!(server.wait_for_explicit_action_snapshot(&uri), edit);

        assert!(matches!(outcome, SnapshotWait::Stale));
    }
}
