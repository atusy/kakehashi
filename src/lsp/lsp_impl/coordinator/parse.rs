use crate::document::DocumentStore;
use crate::document::model::IncrementalSeed;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::{CacheCoordinator, PopulatedInjections};
use crate::lsp::client::ClientNotifier;
use tower_lsp_server::Client;
use url::Url;

use crate::lsp::lsp_impl::{Kakehashi, build_notifier};
use crate::lsp::settings_manager::SettingsManager;

/// The document revision whose parse may drive downstream work. Enriching
/// its region views keeps this identity; an edit or reopen does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParseLineage {
    pub(crate) incarnation: u64,
    pub(crate) content_version: u64,
}

impl ParseLineage {
    pub(crate) fn matches(self, document: &crate::document::Document) -> bool {
        document.incarnation() == self.incarnation
            && document.content_version() == self.content_version
    }
}

/// Inputs of the initial snapshot. Region completion reuses that published
/// snapshot directly and cannot submit these inputs again.
struct SnapshotInputs {
    text: std::sync::Arc<str>,
    tree: tree_sitter::Tree,
    language_name: String,
    parsed_version: u64,
    incarnation: u64,
}

impl SnapshotInputs {
    fn snapshot(
        &self,
        regions: PopulatedInjections,
    ) -> std::sync::Arc<crate::document::snapshot::ParseSnapshot> {
        std::sync::Arc::new(crate::document::snapshot::ParseSnapshot {
            text: std::sync::Arc::clone(&self.text),
            tree: Some(self.tree.clone()),
            language: Some(self.language_name.clone()),
            parsed_version: self.parsed_version,
            incarnation: self.incarnation,
            injection_regions: regions.discovery,
            regions: regions.regions,
            layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
        })
    }
}

/// An owned [`LanguageCheck`](crate::document::LanguageCheck): the first
/// install runs on the compute pool, so the expectation must travel.
enum InstallCheck {
    Record,
    Expect(Option<String>),
    RecordIfUnchanged(Option<String>),
}

impl InstallCheck {
    fn as_check(&self) -> crate::document::LanguageCheck<'_> {
        match self {
            Self::Record => crate::document::LanguageCheck::Record,
            Self::Expect(language) => crate::document::LanguageCheck::Expect(language.as_deref()),
            Self::RecordIfUnchanged(language) => {
                crate::document::LanguageCheck::RecordIfUnchanged(language.as_deref())
            }
        }
    }
}

/// Keep the historical publish outcome separate from the verdict after the
/// resolution work finishes. Only the latter authorizes current downstream work.
struct ParseCompletion {
    initial_install: crate::document::ParseInstall,
    current_at_completion: bool,
}

impl From<crate::document::ParseInstall> for ParseCompletion {
    fn from(initial_install: crate::document::ParseInstall) -> Self {
        Self {
            current_at_completion: initial_install.current,
            initial_install,
        }
    }
}

/// Timeout for compute-pool parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
/// THE SAME constant bounds the injected-layer re-parses (`parse_with_ranges`),
/// so host and injected parses cannot silently drift to different budgets.
const PARSE_TIMEOUT: std::time::Duration = crate::language::injection::NATIVE_PARSE_BUDGET;

/// The awaiter-side backstop for a pooled parse: pool-queue wait (a burst of
/// opens on a small pool can queue parses for a while) plus the in-parse
/// budget, with slack. Deliberately generous: the publish happens in the
/// CALLER after this await, so an awaiter that gives up while its work-unit
/// is still queued silently drops the parse result — the document then
/// serves stale (or empty) until the next edit with nothing to heal it. The
/// pool thread itself is protected by the in-parse abort, not by this.
const PARSE_AWAIT_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);

const RELOAD_WAIT_BACKSTOP: std::time::Duration = PARSE_TIMEOUT;

/// Host-parse with a wall-clock abort — the shared
/// [`parse_with_deadline`](crate::language::injection::parse_with_deadline)
/// primitive under the name the parse-loop call sites use.
///
/// `parse_with_pool`'s `tokio::time::timeout` only abandons the *awaiter*;
/// the in-parse abort is what actually reclaims the bounded-pool thread from
/// a pathological parse (see the primitive's doc).
fn parse_text_with_deadline(
    parser: &mut tree_sitter::Parser,
    text: &str,
    old_tree: Option<&tree_sitter::Tree>,
    deadline: std::time::Instant,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Option<tree_sitter::Tree> {
    crate::language::injection::parse_with_deadline_cancellable(
        parser, text, old_tree, deadline, cancel,
    )
}

/// The settled+stale gate for the parse loop's `semanticTokens/refresh`
/// emission (its full rationale lives at the call site): emit only when the
/// published parse is still the LIVE content version (settled — mid-burst
/// publishes skip; the newer text's own publish re-evaluates) AND some
/// client's last served tokens predate it (a served-version mark exists and
/// is older — no mark means nobody highlights this document).
fn should_emit_settle_refresh(
    documents: &DocumentStore,
    cache: &CacheCoordinator,
    uri: &Url,
    content_version: u64,
    tree_less_upgrade: bool,
) -> bool {
    let settled = documents
        .get(uri)
        .is_some_and(|doc| doc.content_version() == content_version);
    let client_is_stale = cache.served_semantic_version(uri).is_some_and(|served| {
        served < content_version || (tree_less_upgrade && served == content_version)
    });
    settled && client_is_stale
}

pub(super) struct ParseCoordinatorDeps {
    pub(super) client: Client,
    pub(super) language: std::sync::Arc<LanguageCoordinator>,
    pub(super) parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    pub(super) compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    pub(super) documents: std::sync::Arc<DocumentStore>,
    pub(super) cache: std::sync::Arc<CacheCoordinator>,
    pub(super) settings_manager: std::sync::Arc<SettingsManager>,
    pub(super) bridge: std::sync::Arc<BridgeCoordinator>,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
}

pub(crate) struct ParseCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<std::sync::Mutex<DocumentParserPool>>,
    compute_pool: std::sync::Arc<crate::compute_pool::ComputePool>,
    documents: std::sync::Arc<DocumentStore>,
    cache: std::sync::Arc<CacheCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    bridge: std::sync::Arc<BridgeCoordinator>,
    shutdown: tokio_util::sync::CancellationToken,
}

/// A label transition outlives the parse future that published it. Normal
/// callers join the work; cancellation only detaches that join, while server
/// shutdown still cancels the reconciliation through the shared token.
#[derive(Clone)]
struct HostLanguageReconciler {
    language: std::sync::Arc<LanguageCoordinator>,
    documents: std::sync::Arc<DocumentStore>,
    bridge: std::sync::Arc<BridgeCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    shutdown: tokio_util::sync::CancellationToken,
    runtime: tokio::runtime::Handle,
    settings_generation: u64,
}

impl HostLanguageReconciler {
    fn spawn(
        &self,
        uri: &Url,
        installed: crate::document::ParseInstall,
        snapshot: &crate::document::snapshot::ParseSnapshot,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !installed.host_language_changed {
            return None;
        }
        let this = self.clone();
        let uri = uri.clone();
        let language = snapshot.language.clone();
        let incarnation = snapshot.incarnation;
        Some(self.runtime.spawn(async move {
            tokio::select! {
                biased;
                _ = this.shutdown.cancelled() => {}
                _ = async {
                    let still_current = || this.documents.get(&uri).is_some_and(|document| {
                        document.incarnation() == incarnation
                            && document.language_id() == language.as_deref()
                    });
                    let edit_lock = this.documents.edit_lock(&uri);
                    let edit_guard = edit_lock.lock().await;
                    if !still_current() {
                        this.documents.remove_edit_lock_if_unshared(&uri, &edit_lock);
                        return;
                    }
                    // Initial opens and debounce register under this same lock.
                    // Cancel their old batch, then release ingress before waiting
                    // for outstanding host requests to release the bridge lock.
                    this.bridge.cancel_host_eager_open(&uri);
                    drop(edit_guard);
                    let settings_manager = this.settings_manager.clone();
                    let admission = crate::lsp::bridge::HostLanguageAdmission {
                        language: language.clone().expect("promotion carries a detected language"),
                        settings_generation: this.settings_generation,
                        current_settings_generation: std::sync::Arc::new(move || settings_manager.settings_generation()),
                    };
                    if !this.bridge.pool().reconcile_host_language(&uri, incarnation, admission, &still_current).await {
                        if this.documents.get(&uri).is_none() {
                            this.documents.remove_edit_lock_if_unshared(&uri, &edit_lock);
                        }
                        return;
                    }
                    // A later edit cannot erase an accepted language transition.
                    // Re-admit against its lifetime/label, not its old version,
                    // and read the latest text before registering replacement work.
                    let _edit_guard = edit_lock.lock().await;
                    let Some(document) = this.documents.get(&uri) else {
                        this.documents.remove_edit_lock_if_unshared(&uri, &edit_lock);
                        return;
                    };
                    if document.incarnation() != incarnation || document.language_id() != language.as_deref() {
                        return;
                    }
                    let text = document.text_arc();
                    let settings = this.settings_manager.load_settings_pair();
                    // A reload can make the preserved label an alias for another
                    // grammar while this task waits. Match current host contexts
                    // instead of reopening with the pre-reload parser's name.
                    let replacement = if settings.generation != this.settings_generation {
                        this.language.detect_language(uri.path(), &text, None, document.language_id())
                            .or_else(|| language.clone())
                    } else {
                        language.clone()
                    };
                    drop(document);
                    if let Some(language) = replacement.as_deref() {
                        this.bridge.eager_open_host_document_on_servers(
                            &settings.settings, language, &uri, &text,
                        );
                    }
                } => {}
            }
        }))
    }
}

async fn join_host_reconciliation(uri: &Url, task: Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = task
        && let Err(error) = task.await
    {
        log::error!("Host language reconciliation failed for {uri:?}: {error}");
    }
}

/// Run a populate work-unit cooperatively cancelled, but keep awaiting it.
///
/// `ComputePool::run(Some(cancel), ..)` releases its async awaiter as soon as
/// cancellation fires even when the synchronous closure has already started.
/// Populate mutates shared injection caches at its final epoch-gated commit, so
/// its caller must not proceed to a newer populate while the old closure can
/// still be running. The closure polls this token internally and returns
/// quickly when obsolete; `run(None, ..)` preserves the required join.
async fn run_awaited_populate<T, F>(
    pool: &crate::compute_pool::ComputePool,
    cancel: crate::cancel::CancelToken,
    work: F,
) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(crate::cancel::CancelToken) -> T + Send + 'static,
{
    pool.run(None, move || work(cancel)).await
}

impl ParseCoordinator {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self::from_parts(ParseCoordinatorDeps {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            parser_pool: std::sync::Arc::clone(&server.parser_pool),
            compute_pool: std::sync::Arc::clone(&server.compute_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            bridge: std::sync::Arc::clone(&server.bridge),
            shutdown: server.shutdown_token.clone(),
        })
    }

    pub(super) fn from_parts(deps: ParseCoordinatorDeps) -> Self {
        Self {
            client: deps.client,
            language: deps.language,
            parser_pool: deps.parser_pool,
            compute_pool: deps.compute_pool,
            documents: deps.documents,
            cache: deps.cache,
            settings_manager: deps.settings_manager,
            bridge: deps.bridge,
            shutdown: deps.shutdown,
        }
    }

    fn host_language_reconciler(&self) -> HostLanguageReconciler {
        HostLanguageReconciler {
            language: self.language.clone(),
            documents: self.documents.clone(),
            bridge: self.bridge.clone(),
            settings_manager: self.settings_manager.clone(),
            shutdown: self.shutdown.clone(),
            runtime: tokio::runtime::Handle::current(),
            settings_generation: self.settings_manager.settings_generation(),
        }
    }

    async fn install_and_reconcile(
        &self,
        uri: &Url,
        check: crate::document::LanguageCheck<'_>,
        snapshot: std::sync::Arc<crate::document::snapshot::ParseSnapshot>,
    ) -> crate::document::ParseInstall {
        let reconciler = self.host_language_reconciler();
        let installed = self.documents.install_parse(uri, check, snapshot.clone());
        join_host_reconciliation(uri, reconciler.spawn(uri, installed, &snapshot)).await;
        installed
    }

    /// Shared parsing orchestration: run parser acquisition + parse logic as one
    /// work-unit on the bounded compute pool, with timeout.
    ///
    /// The caller provides the actual parse logic via `parse_fn`, which receives a
    /// `tree_sitter::Parser`, the work-unit's wall-clock deadline (feed it to
    /// [`parse_text_with_deadline`]), whether this attempt follows a parser
    /// generation change, and the optional cancellation token. Incremental callers
    /// must drop old-tree seeds on that retry because the replacement parser may
    /// use a different grammar.
    /// The `parser_pool` sync mutex is acquired **only on the pool thread** (the
    /// parse-snapshot ADR's Stage-1 obligation: a tokio worker must never block on
    /// a mutex a compute thread holds), briefly around acquire and release; the
    /// parse itself runs unlocked. On normal completion the parser returns to the
    /// pool — including after this caller timed out, since the release runs inside
    /// the work-unit. A panicking work-unit loses its parser.
    ///
    /// Returns `None` if:
    /// - No parser is available for the language
    /// - The parse work-unit panicked
    /// - The document input version became obsolete before or during parsing
    /// - The native parse aborted itself at its `PARSE_TIMEOUT` in-parse
    ///   deadline (anchored at dequeue, via tree-sitter's progress callback),
    ///   so a pathological parse cannot pin a bounded-pool thread past its
    ///   budget (the pool is sized as low as ONE thread on small hosts, where
    ///   a pinned thread would stall every document's tree-CPU)
    /// - The `PARSE_AWAIT_BACKSTOP` awaiter timeout fired (extreme queue
    ///   pressure; the work-unit still self-bounds)
    /// - Settings changed during both the initial parse and its one retry, so
    ///   neither result belongs to the current parser generation
    /// - The closure returned `None`
    pub(crate) async fn parse_with_pool<T, F>(
        &self,
        language_name: &str,
        uri: &Url,
        text_len: usize,
        cancel: Option<crate::cancel::CancelToken>,
        parse_fn: F,
    ) -> Option<T>
    where
        F: FnMut(
                tree_sitter::Parser,
                std::time::Instant,
                bool,
                Option<&crate::cancel::CancelToken>,
            ) -> (tree_sitter::Parser, Option<T>)
            + Send
            + 'static,
        T: Send + 'static,
    {
        use crate::error::LockResultExt;

        // This is queue + parse + resume time, not isolated parser CPU time.
        let profile_start = log::log_enabled!(target: "kakehashi::profile", log::Level::Debug)
            .then(std::time::Instant::now);
        let parser_pool = std::sync::Arc::clone(&self.parser_pool);
        let language_name_owned = language_name.to_string();
        let profile_uri = profile_start.map(|_| uri.clone());
        let cancel_for_work = cancel.clone();
        let result = tokio::time::timeout(
            PARSE_AWAIT_BACKSTOP,
            self.compute_pool.run(cancel, move || {
                let mut parse_fn = parse_fn;
                let mut language_name_owned = language_name_owned;
                for attempt in 0..2 {
                    if crate::cancel::is_cancelled(cancel_for_work.as_ref()) {
                        return None;
                    }
                    let reload_wait_deadline = std::time::Instant::now() + RELOAD_WAIT_BACKSTOP;
                    let (parser, parser_generation) = loop {
                        if crate::cancel::is_cancelled(cancel_for_work.as_ref()) {
                            return None;
                        }
                        match parser_pool
                            .lock()
                            .recover_poison("ParseCoordinator::parse_with_pool(acquire)")
                            .acquire_versioned(&language_name_owned)
                        {
                            crate::language::parser_pool::ParserCheckout::Acquired(
                                parser,
                                generation,
                            ) => break (parser, generation),
                            crate::language::parser_pool::ParserCheckout::Reloading => {
                                // A reload is synchronous and normally brief. Keep this
                                // transient state inside the bounded work unit instead
                                // of publishing a terminal tree-less result.
                                if std::time::Instant::now() >= reload_wait_deadline {
                                    return None;
                                }
                                // Leave enough scheduling room for the reload owner
                                // to reacquire the mutex and close the window.
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            crate::language::parser_pool::ParserCheckout::Unavailable => {
                                return None;
                            }
                        }
                    };
                    // The in-parse abort deadline is anchored at DEQUEUE, not
                    // submission: a parse that sat in the pool queue behind other
                    // documents' work still gets its full budget of actual parse
                    // CPU (a submission-anchored deadline let a burst of opens
                    // expire healthy parses in the queue, leaving those documents
                    // tree-less until the next edit). The awaiter above covers
                    // queue + parse with slack, so the result is not dropped.
                    let deadline = std::time::Instant::now() + PARSE_TIMEOUT;
                    let parse_start = profile_uri.as_ref().map(|_| std::time::Instant::now());
                    let (parser, value) =
                        parse_fn(parser, deadline, attempt != 0, cancel_for_work.as_ref());
                    if let (Some(start), Some(uri)) = (parse_start, profile_uri.as_ref()) {
                        log::debug!(
                            target: "kakehashi::profile",
                            "phase=parse elapsed_us={} completed={} attempt={} bytes={} language={} uri={}",
                            start.elapsed().as_micros(),
                            value.is_some(),
                            attempt,
                            text_len,
                            language_name_owned,
                            uri,
                        );
                    }
                    match parser_pool
                        .lock()
                        .recover_poison("ParseCoordinator::parse_with_pool(release)")
                        .release_versioned(language_name_owned, parser, parser_generation)
                    {
                        Ok(()) => return value,
                        Err(stale_language_name) => {
                            language_name_owned = stale_language_name;
                        }
                    }
                }
                None
            }),
        )
        .await;

        if let Some(start) = profile_start {
            log::debug!(
                target: "kakehashi::profile",
                "phase=parse_await elapsed_us={} completed={} bytes={} language={} uri={}",
                start.elapsed().as_micros(),
                matches!(&result, Ok(Some(Some(_)))),
                text_len,
                language_name,
                uri,
            );
        }
        match result {
            // Outer Option: None = the work-unit panicked (logged with its
            // payload by the pool). Inner Option: None = no parser available
            // for this language, or the closure itself yielded nothing.
            Ok(Some(value)) => value,
            Ok(None) => None,
            Err(_timeout) => {
                log::warn!(
                    "Parse await backstop hit after {:?} for language '{}' on document {} ({} bytes)",
                    PARSE_AWAIT_BACKSTOP,
                    language_name,
                    uri,
                    text_len
                );
                None
            }
        }
    }

    /// Publish tree/discovery from inside the compute work unit, then complete
    /// that exact snapshot with the resolution. The store owns enrichment and
    /// rechecks currency after the awaited work; downstream never acts on the
    /// earlier install-time verdict. A skipped/cancelled/panicking resolution
    /// leaves the first snapshot usable and readers fall back inline.
    /// Awaiting the work unit preserves populate → mark-finished ordering.
    async fn populate_and_install(
        &self,
        uri: &Url,
        check: InstallCheck,
        inputs: SnapshotInputs,
        version_cancel: crate::cancel::CancelToken,
    ) -> ParseCompletion {
        let cache = std::sync::Arc::clone(&self.cache);
        let language = std::sync::Arc::clone(&self.language);
        let tracker = self.bridge.node_tracker_arc();
        let documents = std::sync::Arc::clone(&self.documents);
        // Shared with the work-unit rather than moved into it: a panic
        // inside the pass is contained by the pool and must still leave
        // this parse installed (once, without regions) — as it always did.
        let inputs = std::sync::Arc::new(inputs);
        let check = std::sync::Arc::new(check);
        let entry_mint_epoch = {
            let edit_lock = self.documents.edit_lock(uri);
            let _edit_guard = edit_lock.lock().await;
            let latch = tracker.mint_epoch(uri);
            let latest = self.documents.latest_snapshot(uri);
            let valid = latest.as_ref().is_some_and(|view| {
                view.slot.current_incarnation == inputs.incarnation
                    && view.content_version == inputs.parsed_version
            });
            if !valid {
                // The edit_lock() accessor above materializes a lock entry
                // even for a document a didClose already removed — drop it
                // so raced closes can't grow the map (the did_change stray
                // rule). Identity- AND share-checked: a reopen racing this
                // probe may already be reusing this very entry (or have
                // installed a new one) for the new lifetime's edits, and
                // removing it from under a queued edit would let the next
                // edit mint a fresh mutex and run concurrently.
                if latest.is_none() {
                    self.documents.remove_edit_lock_if_unshared(uri, &edit_lock);
                }
                None
            } else {
                Some(latch)
            }
        };
        // Refused at the latch (the inputs moved on): one install, without
        // regions — after the edit lock is released, so a queued edit never
        // waits behind a stale publish and the evicted tree's teardown.
        let Some(entry_mint_epoch) = entry_mint_epoch else {
            return self
                .install_and_reconcile(
                    uri,
                    check.as_check(),
                    inputs.snapshot(PopulatedInjections::default()),
                )
                .await
                .into();
        };
        let build_bridge_regions = self
            .settings_manager
            .load_settings()
            .any_bridge_server_runnable();
        let pool_uri = uri.clone();
        // The first install's verdict lives outside the work-unit too, so a
        // panic after the hand-off cannot lose a publish that already landed.
        type First = (
            crate::document::ParseInstall,
            std::sync::Arc<crate::document::snapshot::ParseSnapshot>,
            Option<tokio::task::JoinHandle<()>>,
        );
        let first = std::sync::Arc::new(std::sync::OnceLock::<First>::new());
        let host_reconciler = self.host_language_reconciler();
        let regions = run_awaited_populate(&self.compute_pool, version_cancel, {
            let inputs = std::sync::Arc::clone(&inputs);
            let check = std::sync::Arc::clone(&check);
            let first = std::sync::Arc::clone(&first);
            move |cancel_for_work| {
                let populated = cache.populate_injections_cancellable(
                    &pool_uri,
                    &inputs.text,
                    &inputs.tree,
                    &inputs.language_name,
                    &language,
                    &tracker,
                    entry_mint_epoch,
                    inputs.incarnation,
                    build_bridge_regions,
                    &mut |discovered| {
                        log::trace!(
                            target: "kakehashi::parse",
                            "first install of {pool_uri} v{} at settings generation {}",
                            inputs.parsed_version,
                            discovered.generation
                        );
                        let snapshot = inputs.snapshot(PopulatedInjections {
                            discovery: discovered.discovery,
                            ..PopulatedInjections::default()
                        });
                        let installed = documents.install_parse(
                            &pool_uri,
                            check.as_check(),
                            std::sync::Arc::clone(&snapshot),
                        );
                        let reconciliation = host_reconciler.spawn(&pool_uri, installed, &snapshot);
                        let _ = first.set((installed, snapshot, reconciliation));
                    },
                    Some(&cancel_for_work),
                );
                // A refused pass (`None`) maps to all-`None` region fields —
                // the version then keeps what the first install published
                // (the tree with the discovery already handed out, or
                // nothing yet) and readers resolve inline. Mapping it to the
                // ran-and-empty shape instead would publish "no injections"
                // for a pass that never derived anything, blanking the
                // document's injections until the next parse.
                populated.unwrap_or_default()
            }
        })
        .await
        // A work-unit the pool contained (it panicked) derived nothing: the
        // snapshot rides without regions, exactly as a refused pass.
        .unwrap_or_default();
        // The awaited work unit has dropped its cloned owner, even on panic.
        let first = std::sync::Arc::into_inner(first)
            .expect("populate work unit has completed")
            .into_inner();
        match first {
            // The pass handed its discovery out and the tree is published:
            // land the regions it resolved on the same version. Nothing to
            // land (skipped resolution, refused commit) publishes nothing —
            // the cell would refuse the same shape anyway. A first install
            // the cell refused (a sibling parse of this version landed its
            // tree first) has nothing to upgrade: the regions would ride the
            // sibling's tree with a tree the readers never derived from.
            Some((initial_install, first_snapshot, reconciliation)) => {
                join_host_reconciliation(uri, reconciliation).await;
                let current_at_completion = initial_install.published
                    && self.documents.complete_parse(
                        uri,
                        if initial_install.current
                            && matches!(&*check, InstallCheck::RecordIfUnchanged(_))
                        {
                            // The first publish refined the provisional label.
                            // Regions must validate the label that publish stored.
                            crate::document::LanguageCheck::Expect(
                                first_snapshot.language.as_deref(),
                            )
                        } else {
                            check.as_check()
                        },
                        &first_snapshot,
                        regions.regions,
                    );
                ParseCompletion {
                    initial_install,
                    current_at_completion,
                }
            }
            // No hand-off: one install, whose currency is judged right now.
            None => self
                .install_and_reconcile(uri, check.as_check(), inputs.snapshot(regions))
                .await
                .into(),
        }
    }

    fn reparse_language_check(&self, language_id: Option<String>, detected: &str) -> InstallCheck {
        let preserve = language_id.as_deref().is_some_and(|label| {
            label == detected
                || self
                    .language
                    .is_declared_host_language(label, &self.settings_manager.load_settings())
        });
        if preserve {
            InstallCheck::Expect(language_id)
        } else {
            InstallCheck::RecordIfUnchanged(language_id)
        }
    }

    /// Parse the (already-registered) document at `uri` and publish the result.
    ///
    /// The registering `didOpen` inserts the document — **with its text** — before
    /// calling this, so the parse re-reads that stored text (a cheap `Arc<str>`
    /// refcount bump, [`text_arc`](crate::document::Document::text_arc)) rather than
    /// carrying a second owned `String`, and records the detected language + tree
    /// **in place** through the non-inserting, cell-admitted
    /// [`install_parse`](crate::document::DocumentStore::install_parse) instead of
    /// re-inserting a fresh copy of the text. Net: zero full-document text copies
    /// in the open parse.
    /// That store write is **non-inserting** and lifetime-guarded, so it is
    /// resurrection-safe and stale-safe once the open parse moves off the ingress
    /// ticket: a `didClose` racing it stays closed, and a `didChange` / reopen landing
    /// mid-parse drops the now-stale tree rather than clobbering the newer state.
    ///
    /// `ticket` is the ingress writer ticket of the mutation that scheduled this
    /// parse, or `None` for a caller outside the ingress sequence. On every resolution
    /// path that still observes this lifetime — a tree, a parsed-to-nothing, or no
    /// detectable language — the parse advances the
    /// store's per-document **watermark** to `ticket` (guarded by the open
    /// incarnation), releasing a reader waiting on it. The one path that does **not**
    /// advance is a document already gone (a `didClose` removed it): its watermark
    /// channel is gone too, so its readers have already fallen back.
    ///
    /// Return the revision published by this parse only when it is current at
    /// completion. Callers must validate it again at downstream admission: an
    /// edit or reopen may win while this task is suspended after publication.
    pub(crate) async fn parse_document(
        &self,
        uri: Url,
        language_id: Option<&str>,
        ticket: Option<u64>,
        expected_incarnation: Option<u64>,
    ) -> Option<ParseLineage> {
        let mut events = Vec::new();

        // Read the text the registering didOpen already stored (a refcount bump, not
        // a copy), together with the open lifetime's **incarnation** — BEFORE marking
        // the parse started, so a document a `didClose` already removed leaves neither
        // a resurrected document nor an orphan parse-state entry for the now-closed
        // URI. A missing document stops **without** touching the watermark: the
        // watermark is per-lifetime, so a plain advance with this prior-lifetime
        // ticket could inflate a reopen's freshly-seeded channel and prematurely
        // release a new-lifetime reader; a genuine close instead drops the channel and
        // wakes its readers (they fall back). Unreachable while this parse is inline on
        // the writer ticket (a `didClose` is gated behind the open); the guard is for
        // the off-ingress open flip (#6), where a `didClose`/reopen can race it.
        let (text, incarnation, content_version, version_cancel) =
            self.documents.get(&uri).and_then(|doc| {
                if expected_incarnation.is_some_and(|expected| doc.incarnation() != expected) {
                    return None;
                }
                Some((
                    doc.text_arc(),
                    doc.incarnation(),
                    doc.content_version(),
                    doc.version_cancel_token(),
                ))
            })?;

        // Publish the watermark on whichever path resolves the parse below, but
        // **only if this lifetime is still current**: a close + reopen re-seeds the
        // watermark at 0, and this (prior-lifetime) ticket must not inflate it. Same
        // lifetime → advances (releasing a gated reader even on the no-language /
        // no-tree paths, to the empty fallback). Mirrors `reparse_latest`.
        let advance_watermark = || {
            if let Some(ticket) = ticket {
                self.documents
                    .advance_watermark_for_incarnation(&uri, ticket, incarnation);
            }
        };

        let parse_generation = self.documents.mark_parse_started(&uri);

        let language_name = self
            .language
            .detect_language(uri.path(), &text, None, language_id);

        if let Some(language_name) = language_name {
            let load_result = self
                .language
                .ensure_language_loaded_async(&language_name)
                .await;
            events.extend(load_result.events);

            // This is the document-open parse: there is no prior tree to seed an
            // incremental parse from, so it is always a full parse. (The off-ingress
            // edit reparse — `reparse_latest` — is the incremental path, seeded from
            // `Document::incremental_seed`.) A full parse is also the only safe option
            // without an edited old tree: reusing an unedited tree against different
            // text violates tree-sitter's incremental contract and corrupts external
            // scanners (#348).
            let text_for_parse = text.clone();

            let parsed_tree = if load_result.success {
                self.parse_with_pool(
                    &language_name,
                    &uri,
                    text.len(),
                    Some(version_cancel.clone()),
                    move |mut parser, deadline, _generation_retry, cancel| {
                        let parse_result = parse_text_with_deadline(
                            &mut parser,
                            &text_for_parse,
                            None,
                            deadline,
                            cancel,
                        );
                        (parser, parse_result)
                    },
                )
                .await
            } else {
                None
            };

            if let Some(tree) = parsed_tree {
                // Populate BEFORE the install so the derived discovery rides
                // the snapshot (ADR §3 don't-discover-twice); populate guards
                // itself against a pass whose text or lifetime moved on (the
                // tracker's epoch and incarnation), so it needs no confirmation
                // from the store first, and the install below then publishes
                // the snapshot iff the cell admits it and reports it current
                // iff it parsed the document's content version (a language
                // mismatch rejects it outright).
                let installed = self
                    .populate_and_install(
                        &uri,
                        InstallCheck::Record,
                        SnapshotInputs {
                            text: text.clone(),
                            tree,
                            language_name: language_name.clone(),
                            parsed_version: content_version,
                            incarnation,
                        },
                        version_cancel.clone(),
                    )
                    .await;
                if installed.current_at_completion {
                    // AFTER the install: a downstream task woken by this mark
                    // on another runtime thread must find the snapshot (and
                    // its fast-path regions) already in the cell.
                    self.documents
                        .mark_parse_finished(&uri, parse_generation, true);
                }
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                // Carry the producing revision across this await. Admission
                // must still reject an edit/reopen that won after completion.
                return installed.current_at_completion.then_some(ParseLineage {
                    incarnation,
                    content_version,
                });
            }

            // Parse produced no tree (timeout / parser unavailable / join error) but
            // the language WAS detected — record it with no tree, rather than falling
            // through to the no-language path below which would null it out. Host
            // bridging needs only text + language (never a tree), so preserving the
            // language keeps a host-bridged document working after a parse failure.
            let installed = self
                .install_and_reconcile(
                    &uri,
                    crate::document::LanguageCheck::Record,
                    std::sync::Arc::new(crate::document::snapshot::ParseSnapshot {
                        text: text.clone(),
                        tree: None,
                        language: Some(language_name.clone()),
                        parsed_version: content_version,
                        incarnation,
                        injection_regions: None,
                        regions: None,
                        layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
                    }),
                )
                .await;
            if installed.current {
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
            }
            advance_watermark();
            self.notifier().log_language_events(&events).await;
            return None;
        }

        // No language detected at all → store no language, no tree.
        let installed = self
            .install_and_reconcile(
                &uri,
                crate::document::LanguageCheck::Record,
                std::sync::Arc::new(crate::document::snapshot::ParseSnapshot {
                    text: text.clone(),
                    tree: None,
                    language: None,
                    parsed_version: content_version,
                    incarnation,
                    injection_regions: None,
                    regions: None,
                    layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
                }),
            )
            .await;
        if installed.current {
            self.documents
                .mark_parse_finished(&uri, parse_generation, false);
        }
        advance_watermark();
        self.notifier().log_language_events(&events).await;
        None
    }

    /// Re-parse a document after its parser finished installing, **off the
    /// ingress path** and **resurrection-safely**.
    ///
    /// Called from the spawned auto-install task (see `did_open`), so by the time
    /// it runs the originating `didOpen` writer ticket has already completed.
    /// Unlike [`parse_document`](Self::parse_document) it:
    ///
    /// - re-reads the **latest** store text rather than the open-time text (a
    ///   `didChange` may have landed while the install ran), and
    /// - persists through the **non-inserting** `install_parse`, so a `didClose`
    ///   during the install leaves the document gone instead of resurrecting it
    ///   (the install/parse resurrection vector the actor ADR calls out), and a
    ///   `didChange` between the read and the write drops the now-stale tree.
    ///
    /// No watermark advance: the originating `didOpen`'s skip-parse branch already
    /// resolved that ticket's watermark, and this reparse carries no ticket.
    ///
    /// Because the install is now off-ingress, a `didChange` can run *concurrently*
    /// with this reparse (it is no longer gated behind the install). A `didChange`
    /// that lands while the parser is still loading stores its new text with **no
    /// tree** (the parser wasn't available), and would then make this reparse's
    /// tree not current (it publishes as stale; `tree()` stays `None`) — leaving
    /// the document tree-less. To converge, this
    /// re-reads the latest text and retries a bounded number of times until the
    /// tree lands (or another parse wins). Sustained editing falls back to the
    /// reader's on-demand parse; the parse actor replaces this with a proper
    /// coalescing loop.
    /// Return the revision this call published, if it completed current.
    /// A tree already supplied by another parse grants no downstream work.
    pub(crate) async fn reparse_installed_document(
        &self,
        uri: Url,
        installed_language: &str,
        required_incarnation: Option<u64>,
    ) -> Option<ParseLineage> {
        /// Bound on the convergence retries (a burst of edits landing exactly as
        /// the install completes); past this the reader on-demand parse covers it.
        const MAX_REPARSE_ATTEMPTS: usize = 8;

        // Resolve the language under one read guard, short-circuiting if the
        // document is gone (a `didClose` ran during the install — do not
        // resurrect it) or already parsed (a concurrent parse won — nothing to do,
        // and skip the `ensure_language_loaded` work). Detection borrows the stored
        // text (synchronous, no `.await` and no document write under the `Ref`).
        // Capture the grammar **and** the (language_id, incarnation) it is resolved
        // for, together under one read guard. `language_name` is fixed for the whole
        // loop, so the install must check against the language_id/incarnation
        // captured *here* — not re-read per attempt. Otherwise a relabelling reopen
        // mid-loop would have its new language_id captured per attempt, satisfy the
        // install's language check, and let a tree parsed by the *old* grammar reach
        // the relabelled document. The incarnation is likewise lifetime-stable; only the
        // text legitimately changes within a lifetime (a `didChange`), so only the
        // text is re-read per attempt.
        let (language_name, expected_language_id, expected_incarnation) = {
            let doc = self.documents.get(&uri)?;
            if doc.has_current_tree() {
                return None;
            }
            if required_incarnation.is_some_and(|required| doc.incarnation() != required) {
                return None;
            }
            let language_name =
                self.language
                    .detect_language(uri.path(), doc.text(), None, doc.language_id());
            if language_name.is_some() && language_name.as_deref() != Some(installed_language) {
                return None;
            }
            (
                language_name,
                doc.language_id().map(|s| s.to_string()),
                doc.incarnation(),
            )
        };
        let Some(language_name) = language_name else {
            // Give-up: release a parked first-parse waiter (bootstrap-gated).
            self.documents
                .publish_giveup_snapshot(&uri, expected_incarnation);
            return None;
        };
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
        let mut events = load_result.events;
        if !load_result.success {
            self.documents
                .publish_giveup_snapshot(&uri, expected_incarnation);
            self.notifier().log_language_events(&events).await;
            return None;
        }

        let mut completed = None;
        for _ in 0..MAX_REPARSE_ATTEMPTS {
            // Re-read the latest text each attempt. Gone => closed (no resurrect);
            // already has a tree => a concurrent parse won; a changed incarnation =>
            // a close+reopen, whose new lifetime drives its own parse — stop rather
            // than parse its text with this lifetime's (possibly relabelled-away)
            // grammar (the cell would reject it anyway; this just avoids the wasted
            // parses).
            let (text, content_version, version_cancel) = {
                let Some(doc) = self.documents.get(&uri) else {
                    break;
                };
                if doc.has_current_tree() {
                    break;
                }
                if doc.incarnation() != expected_incarnation {
                    break;
                }
                // `text_arc()` is a refcount bump, not a full copy (#498) — the
                // original stays here for the install while a cheap clone goes to the
                // blocking parse closure.
                (
                    doc.text_arc(),
                    doc.content_version(),
                    doc.version_cancel_token(),
                )
            };

            let text_len = text.len();
            // Hand a cheap `Arc<str>` clone (refcount bump) to the blocking closure;
            // the original stays here for the install below, so the (potentially large)
            // document text is never copied.
            let text_for_parse = text.clone();
            let parsed = self
                .parse_with_pool(
                    &language_name,
                    &uri,
                    text_len,
                    Some(version_cancel.clone()),
                    move |mut parser, deadline, _generation_retry, cancel| {
                        let result = parse_text_with_deadline(
                            &mut parser,
                            &text_for_parse,
                            None,
                            deadline,
                            cancel,
                        );
                        (parser, result)
                    },
                )
                .await;

            let Some(tree) = parsed else { break };

            // Populate BEFORE the install so the discovery rides the snapshot
            // (ADR §3); populate guards itself against a pass whose text or
            // lifetime moved on, and the install then reports the tree
            // current only when its snapshot was admitted at the document's
            // content version: a closed (Vacant) document, one whose text
            // moved (a concurrent `didChange` — its snapshot may still
            // publish as an older version, not current), and one a
            // concurrent parse already gave a tree at this version (the cell
            // refuses the equal-version swap) all leave this tree out.
            // (`Tree` clone is a cheap refcount bump.)
            let installed = self
                .populate_and_install(
                    &uri,
                    self.reparse_language_check(expected_language_id.clone(), &language_name),
                    SnapshotInputs {
                        text: text.clone(),
                        tree,
                        language_name: language_name.clone(),
                        parsed_version: content_version,
                        incarnation: expected_incarnation,
                    },
                    version_cancel.clone(),
                )
                .await;
            // Serve-stale's heal signal, mirroring reparse_latest: a token
            // request answered empty (or 15s-capped) while the install was
            // still compiling has no lineage to re-drive it — without the
            // refresh, a slow install leaves the document unhighlighted
            // until an incidental edit.
            if installed.initial_install.published {
                events.push(crate::language::LanguageEvent::semantic_tokens_refresh(
                    language_name.clone(),
                ));
            }
            if installed.current_at_completion {
                completed = Some(ParseLineage {
                    incarnation: expected_incarnation,
                    content_version,
                });
                break;
            }
            // Not current: the text moved under us (a concurrent `didChange`
            // — the snapshot may still have published as stale-but-
            // consistent), or a sibling parse already installed this version.
            // Loop to re-read the latest text and try again.
        }

        // Covers the give-up exits of the retry loop (parser still
        // unavailable after the install, exhausted attempts): a no-op after
        // a successful publish (bootstrap gate), otherwise it releases a
        // parked first-parse waiter.
        self.documents
            .publish_giveup_snapshot(&uri, expected_incarnation);
        self.notifier().log_language_events(&events).await;
        completed
    }

    /// Re-parse `uri`'s **latest** store text off the ingress path, for the
    /// per-document parse scheduler (`Kakehashi::schedule_reparse`).
    ///
    /// `did_change` bumps the content version synchronously (which makes the
    /// published tree stale for readers) and schedules this; it runs in a spawned
    /// loop, *not* on the writer ticket. When the document can derive an
    /// `incremental_seed` (the published tree with the edits since replayed) the
    /// parse is **incremental**, seeded from it; after a full-text sync there is no
    /// seed and it parses from scratch (which keeps #348 closed). The tree write is the
    /// non-inserting, text **and language** guarded
    /// [`install_parse`](crate::document::DocumentStore::install_parse): a closed (Vacant) document is
    /// left gone (resurrection-safe), a text that moved on (a `didChange` landed
    /// while parsing) is dropped — the scheduler's `dirty` loop then reparses the
    /// newer text — and a reopen that changed the language is rejected (no
    /// wrong-grammar tree). On **every** resolution path the parse
    /// advances the store watermark to `ticket`, so a virt/native reader gated
    /// behind the originating edit is released once its parse resolved.
    ///
    /// The semantic-token `full/delta` path is unaffected by the off-ingress move:
    /// it diffs cached token arrays by `result_id` (never `changed_ranges`), so as
    /// long as the seed keeps this reparse cheap the delta stays cheap too.
    pub(crate) async fn reparse_latest(&self, uri: &Url, ticket: Option<u64>) {
        // Re-read the latest text + detect the language under one read guard. A
        // missing document means a `didClose` ran — stop without touching the
        // watermark (no resurrection). Advancing it here would be unsafe: the
        // watermark is per-lifetime, so if a reopen has *already* re-seeded a fresh
        // channel, a plain advance with this prior-lifetime ticket would inflate it
        // and prematurely release a new-lifetime reader — and it is also
        // unnecessary, since a genuine close drops the channel and wakes its readers
        // (they fall back). The incarnation isn't known on this path, but it isn't
        // needed: only the post-read paths below (which captured it) advance, and
        // they gate on it. `language_id` is captured so the tree write can reject a
        // reopen that changed the language while this parse was in flight. The
        // `incremental_seed` is the published tree (a cheap `Tree` clone) with the
        // edits logged since replayed; `None` after a full-text sync or for a
        // never-parsed document, in which case we parse from scratch.
        let (language_name, language_id, text, seed, incarnation, content_version, version_cancel) = {
            let Some(doc) = self.documents.get(uri) else {
                return;
            };
            // `text_arc()` is a refcount bump, not a full copy of the document text
            // (#498) — cheap on this reparse hot path.
            let text = doc.text_arc();
            let language_id = doc.language_id().map(|s| s.to_string());
            // The lifetime this parse is for: a close+reopen before the install
            // changes it, and the cell rejects on the mismatch (so a tree from
            // this lifetime never reaches a reopened document).
            let incarnation = doc.incarnation();
            let language_name =
                self.language
                    .detect_language(uri.path(), &text, None, language_id.as_deref());
            // The seed is bound to the grammar that produced its tree: a
            // detection that moved with the edit (a changed shebang) parses
            // from scratch rather than reusing another grammar's tree.
            let seed = language_name
                .as_deref()
                .and_then(|language| doc.incremental_seed(language));
            (
                language_name,
                language_id,
                text,
                seed,
                incarnation,
                doc.content_version(),
                doc.version_cancel_token(),
            )
        };

        // Post-read resolutions advance the watermark **only if this lifetime is
        // still current** — a close+reopen re-seeds the watermark at 0, and this
        // (prior-lifetime) ticket must not inflate it and prematurely release a
        // new-lifetime reader. Same lifetime → advances (releasing readers even on
        // the no-language / no-tree paths, to the empty fallback).
        let advance_watermark = || {
            if let Some(ticket) = ticket {
                self.documents
                    .advance_watermark_for_incarnation(uri, ticket, incarnation);
            }
        };

        let Some(language_name) = language_name else {
            // Give-up: release a parked first-parse waiter with a tree-less
            // snapshot (bootstrap-gated inside) rather than letting every
            // request burn the full first-parse backstop.
            self.documents.publish_giveup_snapshot(uri, incarnation);
            advance_watermark();
            return;
        };
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
        if !load_result.success {
            self.documents.publish_giveup_snapshot(uri, incarnation);
            advance_watermark();
            self.notifier()
                .log_language_events(&load_result.events)
                .await;
            return;
        }

        let text_len = text.len();
        // Hand a cheap `Arc<str>` clone (refcount bump) to the blocking closure; the
        // original stays here for the install + injection populate below. The seed
        // makes this an **incremental** parse when the document could derive one:
        // its edit replay (`IncrementalSeed::replay`, the per-edit path copies)
        // runs here on the pool with the parse — not under the store guard the
        // seed was read under — and tree-sitter then reuses the unchanged
        // subtrees and reparses only the edited region. `None` (full-text sync /
        // never parsed) parses from scratch.
        let text_for_parse = text.clone();
        let mut seed = seed;
        let mut replayed: Option<tree_sitter::Tree> = None;
        let parsed = self
            .parse_with_pool(
                &language_name,
                uri,
                text_len,
                Some(version_cancel.clone()),
                move |mut parser, deadline, generation_retry, cancel| {
                    let seed_tree = if generation_retry {
                        None
                    } else {
                        if replayed.is_none() {
                            replayed = seed.take().map(IncrementalSeed::replay);
                        }
                        replayed.as_ref()
                    };
                    let result = parse_text_with_deadline(
                        &mut parser,
                        &text_for_parse,
                        seed_tree,
                        deadline,
                        cancel,
                    );
                    (parser, result)
                },
            )
            .await;

        let mut events = load_result.events;
        if let Some(tree) = parsed {
            // The publish is the one `install_parse` under the entry guard;
            // readers derive the tree from it. Language + the snapshot's own
            // stamps are checked under that guard: language rejects a reopen
            // that relabelled the URI; the cell rejects a same-language,
            // identical-text reopen by incarnation — the tree belongs to the
            // prior lifetime and must not reach the reopened document (nor
            // let the watermark advance below run on the old lifetime's
            // ticket); a within-lifetime stale parse (a `didChange` landed
            // mid-parse) publishes as stale-but-consistent and is reported
            // not current.
            // Populate BEFORE the install so the derived discovery rides the
            // snapshot (ADR §3, don't-discover-twice); readers keep serving the
            // previous snapshot for populate's duration. Populate guards itself
            // against a pass whose text moved on mid-parse (the scheduler's
            // dirty loop is already reparsing the newer text), and the install
            // then publishes it.
            let installed = self
                .populate_and_install(
                    uri,
                    self.reparse_language_check(language_id.clone(), &language_name),
                    SnapshotInputs {
                        text: text.clone(),
                        tree,
                        language_name: language_name.clone(),
                        parsed_version: content_version,
                        incarnation,
                    },
                    version_cancel.clone(),
                )
                .await;
            let published = installed.initial_install.published;
            // Serve-stale's heal signal (ADR §3), narrowed to the cases the
            // workspace-scoped request is actually FOR. `refresh` is expensive
            // for the client (Neovim's handler cancels its in-flight token
            // request and re-tokenizes every attached buffer), and clients
            // already re-request per didChange — so a publish emits it only
            // when ALL of:
            // - the publish landed (a rejected publish emits nothing);
            // - the document has SETTLED (this parse's version is still the
            //   live content_version — during a typing burst the scheduler is
            //   already reparsing newer text, whose own publish re-evaluates);
            // - some client actually consumes this document's semantic tokens
            //   (a served-version mark exists) AND its last served tokens
            //   predate this snapshot, OR it served the reload's current
            //   tree-less placeholder that this publish upgrades at the same
            //   version (otherwise its own didChange-driven request caught up).
            // Net: at most one refresh per settle, none mid-burst, none for
            // documents nobody highlights. Emitted from the parse loop, never
            // didChange (synchronous clients can't answer a server request
            // mid-notification).
            if published
                && should_emit_settle_refresh(
                    &self.documents,
                    &self.cache,
                    uri,
                    content_version,
                    installed.initial_install.tree_upgrade,
                )
            {
                events.push(crate::language::LanguageEvent::semantic_tokens_refresh(
                    language_name.clone(),
                ));
            }
        }

        // Covers the parse-produced-no-tree path (timeout / parser
        // unavailable): a no-op after a successful publish (bootstrap gate),
        // otherwise it releases a parked first-parse waiter.
        self.documents.publish_giveup_snapshot(uri, incarnation);
        advance_watermark();
        self.notifier().log_language_events(&events).await;
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::LspService;

    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    #[tokio::test]
    async fn language_reconciliation_survives_a_newer_edit(#[case] reload_before_admission: bool) {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///test/sticky-promotion.rs").unwrap();
        let incarnation =
            server
                .documents
                .insert(uri.clone(), "old text".into(), Some("text".into()), None);
        server.bridge.open_host_incarnation(&uri, incarnation).await;
        server
            .bridge
            .pool()
            .set_host_routing_by_server(&uri, "old-server", false);
        let snapshot = std::sync::Arc::new(crate::document::snapshot::ParseSnapshot {
            text: std::sync::Arc::from("old text"),
            tree: None,
            language: Some("rust".into()),
            parsed_version: 0,
            incarnation,
            injection_regions: None,
            regions: None,
            layer_trees: std::sync::Arc::new(std::sync::OnceLock::new()),
        });
        let reconciler = server.parse_coordinator().host_language_reconciler();
        let installed = server.documents.install_parse(
            &uri,
            crate::document::LanguageCheck::RecordIfUnchanged(Some("text")),
            snapshot.clone(),
        );
        assert!(installed.host_language_changed);
        let lock = server.documents.edit_lock(&uri);
        let guard = lock.lock().await;
        let task = reconciler.spawn(&uri, installed, &snapshot).unwrap();
        server
            .documents
            .update_document(uri.clone(), "newer text".into(), None);
        if reload_before_admission {
            server
                .settings_manager
                .apply_settings((*server.settings_manager.load_settings()).clone());
        }
        let virtual_uri = Url::parse("file:///test/current-injection.lua").unwrap();
        let virtual_routing = server
            .bridge
            .pool()
            .begin_virtual_routing(&uri, &virtual_uri);
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            server
                .bridge
                .pool()
                .host_routing_by_server(&uri, "old-server"),
            None,
            "a newer edit must not strand the old routing decision"
        );
        assert_eq!(
            server.bridge.pool().accepts_host_language(&uri, "text"),
            reload_before_admission,
            "a late old-generation reconciliation must not install a new fence"
        );
        assert_eq!(server.documents.get(&uri).unwrap().text(), "newer text");
        assert!(
            server
                .bridge
                .pool()
                .is_virtual_routing_current(&uri, &virtual_uri, &virtual_routing),
            "host reconciliation must preserve a newer edit's virtual routing pass"
        );
    }

    #[tokio::test]
    async fn reparse_without_detectable_language_publishes_giveup_snapshot() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/no-language.unknown").unwrap();
        let incarnation =
            server
                .documents
                .insert(uri.clone(), "plain text".to_string(), None, None);

        server
            .parse_coordinator()
            .reparse_installed_document(uri.clone(), "rust", Some(incarnation))
            .await;

        let snapshot = server
            .documents
            .latest_snapshot(&uri)
            .and_then(|view| view.slot.snapshot)
            .expect("undetectable language must release first-parse waiters");
        assert!(snapshot.tree.is_none());
        assert_eq!(snapshot.incarnation, incarnation);
    }

    /// The first install's verdict says the tree it published was current
    /// at that instant, but the caller acts on it only after the resolution
    /// — an edit landing in between has moved the document on, and the
    /// downstream (did_open's eager open, the finished mark) must not run
    /// for a version that is no longer current. With the resolution held,
    /// an edit lands between the two installs; the verdict must be stale.
    #[tokio::test]
    async fn an_edit_during_the_resolution_stales_the_installs_verdict() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        server
            .language
            .language_registry_for_parallel()
            .register("markdown".to_string(), language.clone());
        let query = tree_sitter::Query::new(
            &language,
            "(fenced_code_block (info_string (language) @injection.language) \
             (code_fence_content) @injection.content)",
        )
        .expect("valid markdown injection query");
        server
            .language
            .query_store()
            .insert_injection_query("markdown".to_string(), std::sync::Arc::new(query));
        let uri = Url::parse("file:///workspace/stale-verdict.md").unwrap();
        let text = "```lua\nprint(1)\n```\n";
        let incarnation = server.documents.insert(
            uri.clone(),
            text.to_string(),
            Some("markdown".to_string()),
            None,
        );
        let (text_arc, content_version, version_cancel) = {
            let doc = server.documents.get(&uri).unwrap();
            (
                doc.text_arc(),
                doc.content_version(),
                doc.version_cancel_token(),
            )
        };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(text, None).unwrap();

        let hold = server.cache.hold_resolution();
        let coordinator = server.parse_coordinator();
        let task_uri = uri.clone();
        let install = tokio::spawn(async move {
            coordinator
                .populate_and_install(
                    &task_uri,
                    InstallCheck::Expect(Some("markdown".to_string())),
                    SnapshotInputs {
                        text: text_arc,
                        tree,
                        language_name: "markdown".to_string(),
                        parsed_version: content_version,
                        incarnation,
                    },
                    version_cancel,
                )
                .await
        });
        // The first install lands while the resolution is held.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline
            && !server
                .documents
                .get(&uri)
                .is_some_and(|doc| doc.has_current_tree())
        {
            tokio::task::yield_now().await;
        }
        assert!(
            server
                .documents
                .get(&uri)
                .is_some_and(|doc| doc.has_current_tree()),
            "the tree publishes before the resolution"
        );
        // An edit lands between the two installs.
        server
            .documents
            .apply_edit(&uri, "```lua\nprint(2)\n```\n".to_string(), &[]);
        drop(hold);
        let installed = install.await.expect("the install task completes");
        assert!(
            installed.initial_install.published,
            "the first install did publish the tree of the version it parsed"
        );
        assert!(
            !installed.current_at_completion,
            "the verdict acted on after the resolution must reflect the edit"
        );
    }

    #[tokio::test]
    async fn cancelled_populate_keeps_awaiter_joined_until_work_returns() {
        let pool = crate::compute_pool::test_pool();
        let cancel = crate::cancel::CancelToken::default();
        let cancel_for_work = cancel.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let task = tokio::spawn(async move {
            run_awaited_populate(&pool, cancel_for_work, move |work_cancel| {
                started_tx.send(()).expect("test receiver should wait");
                release_rx.recv().expect("test should release work");
                work_cancel.is_cancelled()
            })
            .await
        });
        started_rx.await.expect("populate work should start");

        cancel.cancel();
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "version cancellation must not detach an in-flight populate"
        );

        release_tx
            .send(())
            .expect("populate work should still wait");
        assert_eq!(
            task.await.expect("populate task should not panic"),
            Some(true)
        );
    }

    /// The four documented invariants of the settle-refresh gate.
    #[test]
    fn settle_refresh_gate_emits_only_for_settled_and_stale() {
        let documents = DocumentStore::new();
        let cache = CacheCoordinator::new();
        let uri = url::Url::parse("file:///settle_gate.rs").unwrap();
        documents.insert(uri.clone(), "a".into(), Some("rust".into()), None);
        // content_version == 0 now.

        // No served mark: nobody highlights this document -> no refresh.
        assert!(!should_emit_settle_refresh(
            &documents, &cache, &uri, 0, false
        ));

        // Client already served THIS version -> its didChange-driven request
        // caught up -> no refresh.
        cache.record_served_semantic_version(&uri, 0);
        assert!(!should_emit_settle_refresh(
            &documents, &cache, &uri, 0, false
        ));

        // An edit bumps the live version; the publish for v1 finds the client
        // stale (served 0 < 1) and the document settled (live == 1) -> emit.
        documents.update_document(uri.clone(), "ab".into(), None);
        assert!(should_emit_settle_refresh(
            &documents, &cache, &uri, 1, false
        ));

        // Mid-burst: another edit already moved the live version past this
        // publish (live 2, publish v1) -> not settled -> no refresh (the v2
        // publish re-evaluates).
        documents.update_document(uri.clone(), "abc".into(), None);
        assert!(!should_emit_settle_refresh(
            &documents, &cache, &uri, 1, false
        ));

        // The mark is monotonic: a stale serve cannot regress it.
        cache.record_served_semantic_version(&uri, 2);
        cache.record_served_semantic_version(&uri, 1);
        assert!(!should_emit_settle_refresh(
            &documents, &cache, &uri, 2, false
        ));
        assert!(
            should_emit_settle_refresh(&documents, &cache, &uri, 2, true),
            "a same-version tree-less to tree upgrade must heal an empty serve"
        );
    }

    /// The deadline must actually abort the native parse (an expired one
    /// yields `None` fast) and must not poison the parser for reuse.
    #[test]
    fn parse_text_with_deadline_aborts_when_expired_and_parses_within_it() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        // Large enough that tree-sitter fires the progress callback at least
        // once before completing.
        let text = "fn f() { let x = 1 + 2 * 3; }\n".repeat(4000);

        let expired = std::time::Instant::now();
        let started = std::time::Instant::now();
        let aborted = parse_text_with_deadline(&mut parser, &text, None, expired, None);
        assert!(aborted.is_none(), "an expired deadline aborts the parse");
        assert!(
            started.elapsed() < PARSE_TIMEOUT,
            "the abort happens in-parse, not after the full parse"
        );

        // The reset parser is immediately reusable on the same input.
        let future = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let parsed = parse_text_with_deadline(&mut parser, &text, None, future, None);
        assert!(parsed.is_some(), "a live deadline parses normally");
    }
}
