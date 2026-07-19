use super::config_store::ConfigStore;
use super::events::{LanguageEvent, LanguageLoadResult, LanguageLoadSummary, LanguageLogLevel};
use super::loader::ParserLoader;
use super::parser_pool::{DocumentParserPool, ParserFactory};
use super::query_loader::{ParseFailure, QueryLoader, format_search_paths};
use super::query_store::QueryStore;
use super::registry::LanguageRegistry;
use crate::config::settings::{LanguageSettings, QueryKind, infer_query_kind};
use crate::config::{CaptureMappings, WorkspaceSettings};
use crate::error::LockResultExt;
use log::debug;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tree_sitter::Language;

/// Maximum length (in characters) for pattern previews in log messages.
const MAX_PREVIEW_LEN: usize = 60;

/// Backstop wait for a loser parked in [`LanguageCoordinator::ensure_language_loaded_async`]'s
/// single-flight: bounds worst-case latency if a notification is ever
/// somehow missed. A first load is expected to finish in the tens of
/// milliseconds (dlopen + query compile); this is purely defense-in-depth,
/// not a tuning knob for expected load time.
const LANGUAGE_LOAD_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Context for loading a query, including metadata for log messages.
struct QueryLoadContext<'a> {
    language_id: &'a str,
    query_kind: QueryKind,
}

/// RAII single-flight marker for [`LanguageCoordinator::ensure_language_loaded_async`]:
/// on drop, removes this language's in-flight entry (only if it still points
/// at this guard's own `Notify` — a reload's `failed_loads.clear()` never
/// touches this map, so nothing else contends the removal) and wakes every
/// parked loser, whether the load succeeded, failed, or panicked mid-flight.
struct LanguageLoadFlightGuard<'a> {
    map: &'a dashmap::DashMap<String, Arc<tokio::sync::Notify>>,
    key: String,
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for LanguageLoadFlightGuard<'_> {
    fn drop(&mut self) {
        self.map
            .remove_if(&self.key, |_, current| Arc::ptr_eq(current, &self.notify));
        self.notify.notify_waiters();
    }
}

/// Coordinates language runtime components (registry, queries, configs).
pub(crate) struct LanguageCoordinator {
    query_store: QueryStore,
    config_store: ConfigStore,
    language_registry: LanguageRegistry,
    parser_loader: RwLock<ParserLoader>,
    /// Maps derived languageId → base language name.
    /// Built from `languages.<name>.base` in configuration.
    /// Example: "rmd" → "markdown" (when rmd has `base = "markdown"`)
    base_map: RwLock<HashMap<String, String>>,
    derived_languages: RwLock<HashSet<String>>,
    config_warnings: RwLock<Vec<String>>,
    /// Negative cache for parser-load attempts: a language whose load failed
    /// (no library found on the search paths) is recorded here — tagged with
    /// the [`load_generation`](Self::load_generation) the attempt captured
    /// BEFORE scanning — so repeated resolution attempts (per injection
    /// region, per compute, for a not-installed injected language like
    /// `latex` inside `markdown_inline`) return immediately instead of
    /// re-scanning the filesystem. Validity lives in the READ: an entry only
    /// suppresses when its generation equals the current one, so an insert
    /// racing a reload (any check-then-insert window) lands inert instead of
    /// re-poisoning — no clear-vs-store ordering to get right. The clear on
    /// `load_settings` is memory hygiene, not the correctness mechanism.
    failed_loads: dashmap::DashMap<String, u64>,
    /// Explicit configured parser failures override same-generation dynamic
    /// discovery. This closes the reload window where fallback publication can
    /// race ahead of validating `languages.<id>.parser`.
    configured_load_failures: dashmap::DashMap<String, u64>,
    /// Reload-scoped registry entries, tagged with the settings generation that
    /// resolved their library path. Manually pre-registered/built-in entries are
    /// untagged; configured and dynamically discovered parsers must match the
    /// current generation before they are cache hits.
    reload_scoped_registrations: dashmap::DashMap<String, u64>,
    /// Original query kinds displaced by configured overrides on untagged
    /// built-in registrations. Keeping each kind separately lets a partial
    /// override preserve and later restore all other built-in query kinds.
    builtin_queries: dashmap::DashMap<(String, QueryKind), Arc<tree_sitter::Query>>,
    /// Serializes settings-generation changes with registry publication so a
    /// load resolved under an old configuration cannot overwrite or retag a
    /// parser registered by a newer reload.
    registration_lock: Mutex<()>,
    /// Makes each config/query/registry reload one state transition. Without
    /// this, concurrent didChangeConfiguration and post-install reloads can
    /// combine one settings payload's languages with another's search paths.
    settings_reload_lock: Mutex<()>,
    /// Bumped by every `load_settings` (the only event that can make a
    /// failed load succeed) before the hygiene clear. Read-validated against
    /// `failed_loads` entries; see there.
    load_generation: std::sync::atomic::AtomicU64,
    /// Content identities for immutable parser artifacts. The generation in
    /// the key keeps didChange from re-reading a shared library while ensuring
    /// a configuration reload revalidates even a same-path replacement.
    worker_artifacts: dashmap::DashMap<(PathBuf, u64, u64), CachedWorkerArtifact>,
    worker_artifact_dir: Option<tempfile::TempDir>,
    worker_settings: RwLock<Arc<WorkspaceSettings>>,
    worker_settings_epoch: std::sync::atomic::AtomicU64,
    /// Single-flight marker for [`ensure_language_loaded_async`](Self::ensure_language_loaded_async):
    /// a language with an in-flight first load has an entry here, so a
    /// concurrent request for the same language parks on the winner's
    /// `Notify` instead of independently dlopen-ing the parser and
    /// recompiling its queries (#575). Scoped to the async entry point only
    /// — the synchronous `ensure_language_loaded` (called from non-tokio
    /// contexts: the ComputePool-run selection-range walk, tests) does not
    /// participate, so a sync caller racing an async one on the same
    /// language can still duplicate the load once; narrower than the
    /// pre-#575 free-for-all, and the common case (an LSP request burst)
    /// goes exclusively through the async path.
    load_inflight: dashmap::DashMap<String, Arc<tokio::sync::Notify>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkerGrammarDescriptor {
    pub(crate) source_path: PathBuf,
    pub(crate) parser_path: PathBuf,
    pub(crate) grammar_symbol: String,
    pub(crate) artifact_digest: String,
    pub(crate) configuration_generation: u64,
}

#[derive(Clone)]
struct CachedWorkerArtifact {
    path: PathBuf,
    digest: String,
}

impl Default for LanguageCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            query_store: QueryStore::new(),
            config_store: ConfigStore::new(),
            language_registry: LanguageRegistry::new(),
            parser_loader: RwLock::new(ParserLoader::new()),
            base_map: RwLock::new(HashMap::new()),
            derived_languages: RwLock::new(HashSet::new()),
            failed_loads: dashmap::DashMap::new(),
            configured_load_failures: dashmap::DashMap::new(),
            reload_scoped_registrations: dashmap::DashMap::new(),
            builtin_queries: dashmap::DashMap::new(),
            registration_lock: Mutex::new(()),
            settings_reload_lock: Mutex::new(()),
            load_generation: std::sync::atomic::AtomicU64::new(0),
            worker_artifacts: dashmap::DashMap::new(),
            worker_artifact_dir: tempfile::Builder::new()
                .prefix("kakehashi-worker-artifacts-")
                .tempdir()
                .ok(),
            worker_settings: RwLock::new(Arc::new(WorkspaceSettings::default())),
            worker_settings_epoch: std::sync::atomic::AtomicU64::new(0),
            config_warnings: RwLock::new(Vec::new()),
            load_inflight: dashmap::DashMap::new(),
        }
    }

    /// Ensure a language parser is loaded, attempting dynamic load if needed.
    ///
    /// Visibility: pub(crate) - called by LSP layer (semantic_tokens, selection_range)
    /// and analysis modules to ensure parsers are available before use.
    pub(crate) fn ensure_language_loaded(&self, language_id: &str) -> LanguageLoadResult {
        loop {
            let current_generation = self
                .load_generation
                .load(std::sync::atomic::Ordering::Acquire);
            if let Some(result) = self.cached_load_verdict(language_id, current_generation) {
                return result;
            }
            let result = self.try_load_language_by_id(language_id, current_generation);
            if self
                .load_generation
                .load(std::sync::atomic::Ordering::Acquire)
                != current_generation
            {
                continue;
            }
            if self.configured_load_failed(language_id, current_generation) {
                return LanguageLoadResult::default();
            }
            if !result.success {
                self.record_failed_load(language_id, current_generation);
            }
            return result;
        }
    }

    /// Already-decided verdict for a load, or `None` when a real attempt is
    /// needed. Single-sources the negative-cache/generation protocol shared by
    /// the sync [`ensure_language_loaded`](Self::ensure_language_loaded) and the
    /// async [`ensure_language_loaded_async`](Self::ensure_language_loaded_async)
    /// so the twins cannot drift.
    ///
    /// A known-failed load UNDER THE CURRENT GENERATION skips the filesystem
    /// scan (and the warning events — the first attempt already surfaced them).
    /// An old-generation entry is inert garbage, not a suppression (see
    /// [`failed_loads`](Self::failed_loads)).
    fn cached_load_verdict(
        &self,
        language_id: &str,
        current_generation: u64,
    ) -> Option<LanguageLoadResult> {
        if self.configured_load_failed(language_id, current_generation) {
            return Some(LanguageLoadResult::default());
        }
        if self.has_current_parser_registration(language_id, current_generation) {
            return Some(LanguageLoadResult::success_with(Vec::new()));
        }
        if self
            .failed_loads
            .get(language_id)
            .is_some_and(|generation| *generation == current_generation)
        {
            return Some(LanguageLoadResult::default());
        }
        None
    }

    fn configured_load_failed(&self, language_id: &str, current_generation: u64) -> bool {
        self.configured_load_failures
            .get(language_id)
            .is_some_and(|generation| *generation == current_generation)
    }

    fn has_current_parser_registration(&self, language_id: &str, current_generation: u64) -> bool {
        self.language_registry.contains(language_id)
            && self
                .reload_scoped_registrations
                .get(language_id)
                .is_none_or(|generation| *generation == current_generation)
    }

    /// Record a failed load, tagged with the generation captured BEFORE the
    /// scan: if a reload landed mid-scan, the tag is already stale and this
    /// entry suppresses nothing — the store itself cannot re-poison, closing
    /// the check-then-insert window an insert-time gate had.
    fn record_failed_load(&self, language_id: &str, current_generation: u64) {
        self.failed_loads
            .insert(language_id.to_string(), current_generation);
    }

    /// Async twin of [`ensure_language_loaded`](Self::ensure_language_loaded)
    /// for tokio-worker call sites (document-open/reparse, injection
    /// discovery): a language's first load runs the parser `.so` dlopen plus
    /// disk reads and `tree_sitter::Query` compilation for every query kind
    /// — tens of milliseconds of blocking work that must not run inline on
    /// an async task (#575). Runs the same [`try_load_language_by_id`]
    /// through `spawn_blocking`, and single-flights concurrent callers for
    /// the same language through [`load_inflight`](Self::load_inflight) so a
    /// request burst against one cold language (or a host document injecting
    /// it into many regions) collapses to a single load in the common case.
    /// Not a hard exactly-once guarantee: the synchronous
    /// `ensure_language_loaded` path does not participate (see `load_inflight`),
    /// and a cancelled winner can be briefly double-loaded — both are benign,
    /// idempotent re-registers.
    ///
    /// Only the winner's [`LanguageLoadResult`] carries the load's events
    /// (log messages, warnings) — a loser that parked and woke re-derives
    /// its own success/failure from `language_registry`/`failed_loads`
    /// (never the winner's result directly), so on a failed load every
    /// loser in the burst gets an empty `LanguageLoadResult::default()`
    /// with no events, even though the winner already logged the error
    /// once. Matches the synchronous [`ensure_language_loaded`](Self::ensure_language_loaded)'s
    /// existing cached-failure behavior — a caller must not assume every
    /// invocation surfaces load events for a language it didn't win the
    /// race to load.
    pub(crate) async fn ensure_language_loaded_async(
        self: &Arc<Self>,
        language_id: &str,
    ) -> LanguageLoadResult {
        loop {
            let current_generation = self
                .load_generation
                .load(std::sync::atomic::Ordering::Acquire);
            if let Some(result) = self.cached_load_verdict(language_id, current_generation) {
                return result;
            }

            // `get` before `entry`: the loser path (marker present) is the
            // common case under a burst, and `entry` would clone the owned
            // key just to find it occupied.
            let notify = if let Some(current) = self.load_inflight.get(language_id) {
                Arc::clone(&current)
            } else {
                match self.load_inflight.entry(language_id.to_string()) {
                    dashmap::mapref::entry::Entry::Occupied(occupied) => Arc::clone(occupied.get()),
                    dashmap::mapref::entry::Entry::Vacant(vacant) => {
                        // Re-check inside the claim: a winner may have
                        // registered the language (success) or recorded its
                        // failure and dropped its marker between the
                        // top-of-loop check and this claim — either verdict
                        // spares us a redundant dlopen scan and re-log.
                        if let Some(result) =
                            self.cached_load_verdict(language_id, current_generation)
                        {
                            return result;
                        }
                        let notify = Arc::new(tokio::sync::Notify::new());
                        vacant.insert(Arc::clone(&notify));
                        // RAII: removes the marker and wakes every parked
                        // loser on ANY exit from this arm — a normal return
                        // (load succeeded or failed), a panic INSIDE the
                        // closure (caught and surfaced as a `JoinError` value
                        // below, so the fn still returns normally), OR this
                        // future being dropped mid-`.await` by request
                        // cancellation. Cancellation is why RAII is required:
                        // the detached `spawn_blocking` closure runs on to
                        // completion and registers the language, but the
                        // marker must be cleared and losers woken NOW, or a
                        // loser re-parks every backstop interval indefinitely.
                        // A loser promoted by that cancellation may briefly
                        // double-load; benign — `immortal_library` caches the
                        // dlopen and registry/query inserts are idempotent
                        // overwrites.
                        let _guard = LanguageLoadFlightGuard {
                            map: &self.load_inflight,
                            key: language_id.to_string(),
                            notify: Arc::clone(&notify),
                        };

                        let coordinator = Arc::clone(self);
                        let owned_id = language_id.to_string();
                        let result = tokio::task::spawn_blocking(move || {
                            let result = coordinator
                                .try_load_language_by_id(&owned_id, current_generation);
                            // Record the failure INSIDE the blocking task so a
                            // winner cancelled mid-`.await` (its future dropped)
                            // still populates the negative cache: the detached
                            // task runs to completion regardless of the awaiting
                            // task's fate, so without this the scan+log would
                            // repeat for every later caller until one survives
                            // the await. Idempotent with the JoinError branch's
                            // record below.
                            if !result.success {
                                coordinator.record_failed_load(&owned_id, current_generation);
                            }
                            result
                        })
                        .await
                        .unwrap_or_else(|join_error| {
                            // Reached only when the closure PANICKED (the
                            // awaiting task is alive here, so this is not the
                            // cancellation path) — the in-closure record never
                            // ran, so record the synthetic failure here.
                            self.record_failed_load(language_id, current_generation);
                            LanguageLoadResult::failure_with(LanguageEvent::log(
                                LanguageLogLevel::Error,
                                format!(
                                    "language load for '{language_id}' failed on the blocking pool: {join_error}"
                                ),
                            ))
                        });

                        if self
                            .load_generation
                            .load(std::sync::atomic::Ordering::Acquire)
                            != current_generation
                        {
                            continue;
                        }
                        if self.configured_load_failed(language_id, current_generation) {
                            return LanguageLoadResult::default();
                        }
                        return result;
                    }
                }
            };
            // Register interest BEFORE re-validating the marker (mirrors
            // `captures.rs`'s single-flight): `enable()` makes a
            // `notify_waiters()` call that happens between here and the
            // `.await` below visible to this waiter — awaiting bare
            // `notify.notified()` without it has a lost-wakeup window
            // between reading the `Arc<Notify>` above and this future's
            // first poll, where a winner whose load finishes in that gap
            // (e.g. the `search_paths.is_empty()` fast-fail) removes the
            // marker and calls `notify_waiters()` before we're registered
            // to receive it. A bare `notify.notified().await` here would then
            // block on a notification that already fired, resolving only via
            // the 1s backstop below (and, without it, never — no future
            // winner for this language creates a fresh `Notify` to wake it).
            // The
            // re-check after `enable()` catches a winner that already
            // exited before we registered — its `notify_waiters()` preceded
            // our `enable()`, so we skip waiting and loop back immediately
            // instead of awaiting a notification that already happened.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let winner_live = self
                .load_inflight
                .get(language_id)
                .is_some_and(|current| Arc::ptr_eq(&current, &notify));
            if winner_live {
                tokio::select! {
                    _ = &mut notified => {}
                    _ = tokio::time::sleep(LANGUAGE_LOAD_WAIT_TIMEOUT) => {}
                }
            }
            // Loop back: re-check registry/failed_loads/in-flight from
            // scratch rather than trusting the winner's outcome directly —
            // the winner may have failed (loser re-derives the same
            // failed_loads verdict) or a reload may have landed concurrently.
        }
    }

    /// Initialize from workspace-level settings and return coordination events.
    ///
    /// Visibility: pub(crate) - called by LSP layer during initialization and
    /// settings updates to configure language support.
    pub(crate) fn load_settings(&self, settings: &WorkspaceSettings) -> LanguageLoadSummary {
        let _reload = self
            .settings_reload_lock
            .lock()
            .recover_poison("LanguageCoordinator::load_settings(reload)");
        self.worker_settings_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *self
            .worker_settings
            .write()
            .recover_poison("LanguageCoordinator::load_settings(worker_settings)") =
            Arc::new(settings.clone());
        self.config_store.update_from_settings(settings);
        self.clear_derived_languages();
        // A reload (new search paths, or the post-install reload) is the only
        // event that can turn a failed load into a success — bump the
        // generation so every existing negative entry stops suppressing
        // (validity is read-side; see `failed_loads`). The clear is memory
        // hygiene only: a stale store racing it lands with an old tag and is
        // inert, so no ordering between bump, clear, and in-flight scans can
        // produce a wrong suppression.
        {
            let _registration = self
                .registration_lock
                .lock()
                .recover_poison("LanguageCoordinator::load_settings(generation)");
            self.load_generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        self.failed_loads.clear();
        self.configured_load_failures.clear();

        // Build base map from language configs
        self.build_base_map(&settings.languages);

        // Partition languages into (no-base, has-base) groups.
        // HashMap iteration order is arbitrary, so explicit partitioning
        // ensures base languages are loaded before derived ones.
        let (eager_load_candidates, skipped_languages): (Vec<_>, Vec<_>) = settings
            .languages
            .iter()
            .partition(|(lang_name, config)| should_eager_load_language(lang_name, config));
        let (base_languages, derived_languages): (Vec<_>, Vec<_>) = eager_load_candidates
            .into_iter()
            .partition(|(_, config)| config.base.is_none());
        let mut summary = LanguageLoadSummary::default();
        summary.events.extend(self.config_warning_events());
        let search_paths = self.config_store.search_paths();

        for (lang_name, _) in skipped_languages {
            debug!(
                target: "kakehashi::config",
                "Skipping eager load for inheritance-only language root '{}'",
                lang_name
            );
        }

        // Pass 1: Load all languages WITHOUT base (normal path)
        for (lang_name, config) in &base_languages {
            let result = self.load_single_language(lang_name, config, &search_paths);
            summary.record(lang_name, result);
        }

        // Pass 2: For each language WITH base, register base's parser and queries
        // under the derived name.
        // A single visiting set is shared across all iterations so that cycle
        // detection spans the entire pass (each recursive load_derived_language
        // call already shares the set via &mut, but a shared top-level set
        // avoids redundant cycle re-discovery between iterations).
        let mut visiting = HashSet::new();
        for (derived_name, config) in &derived_languages {
            let result = self.load_derived_language(
                derived_name,
                config,
                &settings.languages,
                &search_paths,
                &mut visiting,
            );
            summary.record(derived_name, result);
        }

        self.worker_settings_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        summary
    }

    /// Load a derived language by copying parser and queries from its base.
    ///
    /// The base's parser is loaded first (if not already available), then
    /// registered under the derived name. Queries are similarly copied.
    ///
    /// Manages the `visiting` set to detect circular chains: inserts before
    /// the load attempt and removes after, regardless of outcome.
    fn load_derived_language(
        &self,
        derived_name: &str,
        config: &LanguageSettings,
        languages: &HashMap<String, LanguageSettings>,
        search_paths: &[PathBuf],
        visiting: &mut HashSet<String>,
    ) -> LanguageLoadResult {
        let inserted = visiting.insert(derived_name.to_string());
        let outcome = self.load_derived_language_inner(
            derived_name,
            config,
            languages,
            search_paths,
            visiting,
        );
        if inserted {
            visiting.remove(derived_name);
        }
        outcome
    }

    /// Core logic for loading a derived language, separated from visiting-set
    /// management. All early returns are plain returns — no IIFE needed.
    fn load_derived_language_inner(
        &self,
        derived_name: &str,
        config: &LanguageSettings,
        languages: &HashMap<String, LanguageSettings>,
        search_paths: &[PathBuf],
        visiting: &mut HashSet<String>,
    ) -> LanguageLoadResult {
        let base_name = match &config.base {
            Some(name) if name != derived_name => name.as_str(),
            Some(_) => {
                // Self-reference: load as a normal language using its own config
                return self.load_standalone_derived_language(derived_name, config, search_paths);
            }
            None => {
                return LanguageLoadResult::failure_with(LanguageEvent::log(
                    LanguageLogLevel::Error,
                    format!("load_derived_language called for '{derived_name}' without base"),
                ));
            }
        };

        let base_config = languages.get(base_name);
        let base_is_inheritance_only = base_config
            .is_some_and(|base_config| !should_eager_load_language(base_name, base_config));

        if base_is_inheritance_only {
            return self.load_standalone_derived_language(derived_name, config, search_paths);
        }

        if base_config.is_none() && config.parser.is_some() {
            return self.load_standalone_derived_language(derived_name, config, search_paths);
        }

        // Ensure base language is loaded (may need dynamic load from search paths)
        if let Err(result) = self.ensure_base_loaded(
            derived_name,
            base_name,
            config,
            languages,
            search_paths,
            visiting,
        ) {
            return result;
        }

        // Get the base's Language object and register under derived name
        let Some(language) = self.language_registry.get(base_name) else {
            return LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Error,
                format!("Base language '{base_name}' was loaded but not found in registry"),
            ));
        };

        if let Some(base_config) = base_config
            && config.parser != base_config.parser
        {
            return self.load_standalone_derived_language(derived_name, config, search_paths);
        }

        self.register_derived_from_base(
            derived_name,
            base_name,
            config,
            base_config,
            search_paths,
            &language,
        )
    }

    /// Register a derived language by reusing the base's parser and copying
    /// (or loading) queries. Marks the language as derived in the tracking set.
    fn register_derived_from_base(
        &self,
        derived_name: &str,
        base_name: &str,
        config: &LanguageSettings,
        base_config: Option<&LanguageSettings>,
        search_paths: &[PathBuf],
        language: &tree_sitter::Language,
    ) -> LanguageLoadResult {
        self.query_store.remove_queries(derived_name);
        self.register_configured_language(derived_name, language.clone());
        self.derived_languages
            .write()
            .recover_poison("LanguageCoordinator::register_derived_from_base(derived_languages)")
            .insert(derived_name.to_string());

        let mut events = Vec::new();
        let should_load_derived_queries = config.queries.is_some()
            && base_config.is_none_or(|base_config| config.queries != base_config.queries);
        if should_load_derived_queries {
            events.extend(self.load_queries_for_language(
                derived_name,
                config,
                search_paths,
                language,
            ));
        } else {
            for kind in QueryKind::ALL {
                if let Some(query) = self.query_store.get_query(kind, base_name) {
                    self.query_store
                        .insert_query(kind, derived_name.to_string(), query);
                }
            }
        }

        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!("Derived language '{derived_name}' loaded from base '{base_name}'"),
        ));
        if self.has_queries(derived_name) {
            events.push(LanguageEvent::semantic_tokens_refresh(
                derived_name.to_string(),
            ));
        }

        LanguageLoadResult::success_with(events)
    }

    /// Ensure the base language is loaded into the registry.
    ///
    /// Returns `Ok(())` if the base is already loaded or was successfully loaded.
    /// Returns `Err(LanguageLoadResult)` if loading failed — the caller should
    /// return that result directly (it may be a standalone fallback or an error).
    fn ensure_base_loaded(
        &self,
        derived_name: &str,
        base_name: &str,
        derived_config: &LanguageSettings,
        languages: &HashMap<String, LanguageSettings>,
        search_paths: &[PathBuf],
        visiting: &mut HashSet<String>,
    ) -> Result<(), LanguageLoadResult> {
        let current_generation = self
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        if self.has_current_parser_registration(base_name, current_generation) {
            return Ok(());
        }

        let base_config = languages.get(base_name);
        let base_result = match base_config {
            Some(base_config) if !visiting.contains(base_name) => {
                if base_config.base.is_some() {
                    self.load_derived_language(
                        base_name,
                        base_config,
                        languages,
                        search_paths,
                        visiting,
                    )
                } else {
                    self.load_single_language(base_name, base_config, search_paths)
                }
            }
            Some(_) => LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Error,
                format!(
                    "Cannot load derived language '{derived_name}': \
                     base language '{base_name}' is part of a circular chain"
                ),
            )),
            None => self.try_load_language_by_id(base_name, current_generation),
        };

        if base_result.success {
            Ok(())
        } else if derived_config.parser.is_some() {
            Err(self.load_standalone_derived_language(derived_name, derived_config, search_paths))
        } else {
            Err(LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Error,
                format!(
                    "Cannot load derived language '{derived_name}': \
                     base language '{base_name}' not found"
                ),
            )))
        }
    }

    fn load_standalone_derived_language(
        &self,
        derived_name: &str,
        config: &LanguageSettings,
        search_paths: &[PathBuf],
    ) -> LanguageLoadResult {
        let result = self.load_single_language(derived_name, config, search_paths);
        if result.success {
            self.derived_languages
                .write()
                .recover_poison(
                    "LanguageCoordinator::load_standalone_derived_language(derived_languages)",
                )
                .insert(derived_name.to_string());
        }
        result
    }

    fn clear_derived_languages(&self) {
        let mut derived_languages = self
            .derived_languages
            .write()
            .recover_poison("LanguageCoordinator::clear_derived_languages");

        for language_id in derived_languages.drain() {
            self.language_registry.unregister(&language_id);
            self.query_store.remove_queries(&language_id);
        }
    }

    /// Build the derived → base language map from configuration.
    ///
    /// For each language with `base = "markdown"`, maps derived_name → "markdown".
    /// This enables editors sending languageId "rmd" to use the "markdown" parser.
    ///
    /// Also warns if the deprecated `aliases` field is still in use.
    fn build_base_map(&self, languages: &HashMap<String, LanguageSettings>) {
        let mut base_map = self
            .base_map
            .write()
            .recover_poison("LanguageCoordinator::build_base_map");
        let mut config_warnings = self
            .config_warnings
            .write()
            .recover_poison("LanguageCoordinator::build_base_map(config_warnings)");

        base_map.clear();
        config_warnings.clear();

        for (lang_name, config) in languages {
            if let Some(base) = &config.base {
                if lang_name == base {
                    log::debug!(
                        target: "kakehashi::config",
                        "Language '{}' has base='{}' (self-reference, chain terminator)",
                        lang_name, base
                    );
                } else if config.parser.is_some()
                    && languages
                        .get(base)
                        .is_none_or(|base_config| config.parser != base_config.parser)
                {
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Skipping base fallback for '{}' because it defines its own parser",
                        lang_name
                    );
                } else {
                    base_map.insert(lang_name.clone(), base.clone());
                    log::debug!(
                        target: "kakehashi::language_detection",
                        "Registered base '{}' → '{}'",
                        lang_name,
                        base
                    );
                }
            }

            if let Some(aliases) = &config.aliases {
                let example_alias = aliases
                    .iter()
                    .next()
                    .map(|a| a.as_str())
                    .unwrap_or("<derived>");
                let message = format!(
                    "Language '{}' uses deprecated 'aliases' field. \
                     Use 'base' on derived languages instead. \
                     Example: [languages.{}] base = \"{}\"",
                    lang_name, example_alias, lang_name
                );
                log::warn!(target: "kakehashi::config", "{message}");
                config_warnings.push(message);
            }
        }
    }

    /// Resolve a derived languageId to its base language name.
    ///
    /// Returns the registered base mapping, otherwise `None`. A declaration is
    /// intentionally not registered when the derived language owns a distinct
    /// parser; see [`Self::build_base_map`].
    /// Example: "rmd" → Some("markdown") when its base mapping is eligible.
    fn resolve_base(&self, language_id: &str) -> Option<String> {
        let base_map = self
            .base_map
            .read()
            .recover_poison("LanguageCoordinator::resolve_base");

        base_map.get(language_id).cloned()
    }

    fn config_warning_events(&self) -> Vec<LanguageEvent> {
        let mut warnings = self
            .config_warnings
            .write()
            .recover_poison("LanguageCoordinator::config_warning_events");

        warnings
            .drain(..)
            .map(|message| LanguageEvent::show_message(LanguageLogLevel::Warning, message))
            .collect()
    }

    /// language-detection-fallback-chain: Try candidate directly, then with config-based base fallback.
    ///
    /// Returns the language name if a parser is available (either directly or via base).
    /// This is applied as a sub-step after each detection method.
    fn try_with_base_fallback(&self, candidate: &str) -> Option<String> {
        // Direct match
        if self.has_parser_available(candidate) {
            return Some(candidate.to_string());
        }
        // Config-based base resolution
        if let Some(base) = self.resolve_base(candidate)
            && self.has_parser_available(&base)
        {
            return Some(base);
        }
        None
    }

    /// Try to dynamically load a language by ID from configured search paths
    ///
    /// Visibility: Internal only - called by ensure_language_loaded.
    /// Not exposed as public API to keep interface minimal (YAGNI).
    fn try_load_language_by_id(
        &self,
        language_id: &str,
        current_generation: u64,
    ) -> LanguageLoadResult {
        let search_paths = self.config_store.search_paths();
        if search_paths.is_empty() {
            return LanguageLoadResult::failure_with(LanguageEvent::log(
                LanguageLogLevel::Warning,
                format!("No search paths configured, cannot load language '{language_id}'"),
            ));
        }

        // Warning: parser may not exist yet (dynamic discovery)
        let language =
            match self.load_parser(language_id, None, &search_paths, LanguageLogLevel::Warning) {
                Ok(lang) => lang,
                Err(result) => return result,
            };
        let mut events = Vec::new();
        if !self.publish_dynamic_language(language_id, language.clone(), current_generation, || {
            // Publish queries under the same generation gate as the parser.
            // A reload cannot advance the generation and then be overwritten
            // by queries resolved from the previous search paths.
            self.load_all_queries(
                &language,
                &search_paths,
                language_id,
                "Dynamically loaded",
                &mut events,
            );
        }) {
            return LanguageLoadResult::default();
        }

        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!("Dynamically loaded language {language_id} from search paths",),
        ));
        if self.has_queries(language_id) {
            events.push(LanguageEvent::semantic_tokens_refresh(
                language_id.to_string(),
            ));
        }

        LanguageLoadResult::success_with(events)
    }

    fn publish_dynamic_language<F>(
        &self,
        language_id: &str,
        language: Language,
        expected_generation: u64,
        publish_queries: F,
    ) -> bool
    where
        F: FnOnce(),
    {
        let _registration = self
            .registration_lock
            .lock()
            .recover_poison("LanguageCoordinator::register_dynamic_language");
        if self
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire)
            != expected_generation
            || self.configured_load_failed(language_id, expected_generation)
        {
            return false;
        }
        self.language_registry
            .register(language_id.to_string(), language);
        self.reload_scoped_registrations
            .insert(language_id.to_string(), expected_generation);
        self.query_store.remove_queries(language_id);
        publish_queries();
        true
    }

    /// Resolve and load a parser for the given language.
    ///
    /// Publication is deliberately separate so the caller can serialize it
    /// with settings-generation changes. Returns `Ok(Language)` on success,
    /// or `Err(LanguageLoadResult)` with an appropriate failure event.
    ///
    /// `missing_parser_level` controls how a missing parser is reported:
    /// - `Warning` for dynamic loading — the parser may not exist yet (normal)
    /// - `Error` for config-driven loading — the parser was explicitly configured
    ///   but cannot be found (configuration problem)
    fn load_parser(
        &self,
        lang_name: &str,
        parser_config: Option<&str>,
        search_paths: &[PathBuf],
        missing_parser_level: LanguageLogLevel,
    ) -> Result<Language, LanguageLoadResult> {
        let library_path =
            QueryLoader::resolve_library_path(parser_config, lang_name, search_paths);
        let Some(lib_path) = library_path else {
            return Err(LanguageLoadResult::failure_with(LanguageEvent::log(
                missing_parser_level,
                format!(
                    "No parser path found for language '{lang_name}' in search paths: {}",
                    format_search_paths(search_paths),
                ),
            )));
        };

        let language = {
            let result = self
                .parser_loader
                .write()
                .recover_poison("LanguageCoordinator::load_parser")
                .load_language(&lib_path, lang_name);
            match result {
                Ok(lang) => lang,
                Err(err) => {
                    return Err(LanguageLoadResult::failure_with(LanguageEvent::log(
                        LanguageLogLevel::Error,
                        format!(
                            "Failed to load language {lang_name} from {}: {err}",
                            lib_path.display()
                        ),
                    )));
                }
            }
        };

        Ok(language)
    }

    /// Load all three query types (highlights, bindings, injections) for a language.
    fn load_all_queries(
        &self,
        language: &Language,
        search_paths: &[PathBuf],
        lang_name: &str,
        context: &str,
        events: &mut Vec<LanguageEvent>,
    ) {
        for query_kind in QueryKind::ALL {
            self.load_query(
                language,
                search_paths,
                QueryLoadContext {
                    language_id: lang_name,
                    query_kind,
                },
                context,
                events,
                |store, query| store.insert_query(query_kind, lang_name.to_string(), query),
            );
        }
    }

    /// Load a query file with inheritance resolution.
    fn load_query(
        &self,
        language: &Language,
        paths: &[PathBuf],
        ctx: QueryLoadContext<'_>,
        context: &str,
        events: &mut Vec<LanguageEvent>,
        insert_fn: impl FnOnce(&QueryStore, Arc<tree_sitter::Query>),
    ) {
        let filename = ctx.query_kind.filename();
        let result = match QueryLoader::load_query_with_inheritance(
            language,
            paths,
            ctx.language_id,
            filename,
        ) {
            Ok(r) => r,
            Err(_) => {
                debug!(
                    "Query file {}/{} not found in search paths (this is normal if not provided)",
                    ctx.language_id, filename
                );
                return;
            }
        };

        let query_label = format!("{}/{}", ctx.language_id, filename);
        let success_prefix = format!(
            "{} {} for {}",
            context,
            ctx.query_kind.name(),
            ctx.language_id
        );
        self.process_query_result(result, &query_label, &success_prefix, events, insert_fn);
    }

    /// Load a query from explicit paths (unified queries configuration).
    fn load_query_from_paths<P: AsRef<Path>>(
        &self,
        language: &Language,
        paths: &[P],
        ctx: QueryLoadContext<'_>,
        events: &mut Vec<LanguageEvent>,
        insert_fn: impl FnOnce(&QueryStore, Arc<tree_sitter::Query>),
    ) {
        let result = match QueryLoader::load_query_from_paths(language, paths) {
            Ok(r) => r,
            Err(err) => {
                events.push(LanguageEvent::log(
                    LanguageLogLevel::Error,
                    format!(
                        "Failed to load {} query for {}: {err}",
                        ctx.query_kind.name(),
                        ctx.language_id
                    ),
                ));
                return;
            }
        };

        let query_label = format!("{} {} query", ctx.language_id, ctx.query_kind.name());
        let success_prefix = format!(
            "{} query loaded for {}",
            ctx.query_kind.name(),
            ctx.language_id
        );
        self.process_query_result(result, &query_label, &success_prefix, events, insert_fn);
    }

    /// Process a ParseResult: log skipped patterns, insert query, log outcome.
    fn process_query_result(
        &self,
        result: super::query_loader::ParseResult,
        query_label: &str,
        success_prefix: &str,
        events: &mut Vec<LanguageEvent>,
        insert_fn: impl FnOnce(&QueryStore, Arc<tree_sitter::Query>),
    ) {
        // Log warnings for skipped patterns
        for skipped in &result.skipped {
            let preview = truncate_preview(&skipped.text, MAX_PREVIEW_LEN);
            // When inheritance is used, line numbers refer to the combined query,
            // not the original source file
            let line_note = if result.used_inheritance {
                " (in combined query)"
            } else {
                ""
            };
            events.push(LanguageEvent::log(
                LanguageLogLevel::Warning,
                format!(
                    "Skipped invalid pattern in {query_label} (lines {}-{}{}): {} | pattern: {}",
                    skipped.start_line, skipped.end_line, line_note, skipped.error, preview
                ),
            ));
        }

        match result.query {
            Some(query) => {
                insert_fn(&self.query_store, Arc::new(query));
                let skipped_count = result.skipped.len();
                let msg = if skipped_count > 0 {
                    format!("{success_prefix} ({skipped_count} pattern(s) skipped)")
                } else {
                    success_prefix.to_string()
                };
                events.push(LanguageEvent::log(LanguageLogLevel::Info, msg));
            }
            None => {
                if let Some(reason) = &result.failure_reason {
                    let msg = match reason {
                        ParseFailure::PatternSplitFailed(err) => {
                            format!(
                                "Failed to parse {query_label}: could not split patterns ({err})"
                            )
                        }
                        ParseFailure::AllPatternsInvalid => {
                            format!(
                                "Failed to load {query_label}: all {} pattern(s) were invalid",
                                result.skipped.len()
                            )
                        }
                        ParseFailure::CombinationFailed(err) => {
                            format!("Failed to combine patterns in {query_label}: {err}")
                        }
                    };
                    events.push(LanguageEvent::log(LanguageLogLevel::Warning, msg));
                }
            }
        }
    }

    /// Path-only language detection for CLI directory walks
    /// (`kakehashi format <dir>`), loading the parser on first sight.
    ///
    /// [`Self::detect_language`] only accepts a candidate whose parser is
    /// already **loaded**, which works in LSP mode because `didOpen` loads
    /// parsers before anything queries the chain — but a directory walk
    /// filters paths before any document is opened, when the registry is
    /// still empty. This variant tries to load the candidate (and its
    /// configured base) from the search paths, so "installed but not yet
    /// loaded" counts as formattable. Auto-install is never triggered:
    /// a language whose parser is not installed is silently skipped.
    pub(crate) fn loadable_language_for_path(&self, path: &str) -> Option<String> {
        let token = super::heuristic::extract_token_from_path(path)?;
        // Normalize via syntect ("md" → "markdown"); fall back to the
        // raw token for extensions syntect doesn't know but a config
        // entry (possibly via `base`) might.
        let candidate =
            super::heuristic::detect_from_token(token).unwrap_or_else(|| token.to_string());

        self.load_candidate_or_base(candidate)
    }

    /// Like [`Self::loadable_language_for_path`], but falls back to
    /// first-line content detection (shebang, mode line) for extensionless
    /// files. `detect_language`'s own first-line stage only accepts
    /// already-loaded parsers, so an explicit CLI path like a
    /// `#!/usr/bin/env lua` script would otherwise silently fail detection
    /// before anything had a chance to load the lua parser.
    pub(crate) fn loadable_language_for_document(
        &self,
        path: &str,
        content: &str,
    ) -> Option<String> {
        if let Some(lang) = self.loadable_language_for_path(path) {
            return Some(lang);
        }
        let candidate = super::heuristic::detect_from_first_line(content)?;
        self.load_candidate_or_base(candidate)
    }

    /// Candidate-only document detection for unknown LSP language IDs.
    ///
    /// Unlike [`Self::loadable_language_for_document`], this does not load
    /// parsers. It only normalizes path/first-line heuristics and resolves a
    /// configured base, leaving didOpen's existing async load/auto-install path
    /// to do any parser work.
    pub(crate) fn candidate_language_for_document(
        &self,
        path: &str,
        content: &str,
    ) -> Option<String> {
        if let Some(token) = super::heuristic::extract_token_from_path(path) {
            if let Some(candidate) = super::heuristic::detect_from_token(token) {
                return Some(self.resolve_base(&candidate).unwrap_or(candidate));
            }
            if let Some(base) = self.resolve_base(token) {
                return Some(base);
            }
            if Path::new(path).extension().is_some() {
                return Some(token.to_string());
            }
        }

        if let Some(candidate) = super::heuristic::detect_from_first_line(content) {
            return Some(self.resolve_base(&candidate).unwrap_or(candidate));
        }

        None
    }

    /// Load `candidate` (or its configured base) from the search paths,
    /// returning the name that actually loaded.
    fn load_candidate_or_base(&self, candidate: String) -> Option<String> {
        if self.ensure_language_loaded(&candidate).success {
            return Some(candidate);
        }
        let base = self.resolve_base(&candidate)?;
        self.ensure_language_loaded(&base).success.then_some(base)
    }

    /// Check if a parser is available for a given language name.
    ///
    /// Used by the detection fallback chain (language-detection-fallback-chain) to determine whether
    /// to accept a detection result or continue to the next method.
    ///
    /// Visibility: pub(crate) - called by LSP layer (lsp_impl) to check parser
    /// availability before attempting language operations.
    pub(crate) fn has_parser_available(&self, language_name: &str) -> bool {
        let generation = self
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        !self.configured_load_failed(language_name, generation)
            && self.has_current_parser_registration(language_name, generation)
    }

    /// Parser-aware language-detection fallback chain for host documents;
    /// returns the first language with an available parser. Each stage is
    /// detect → base resolution → availability:
    /// (1) LSP `languageId` (if not `"plaintext"`); (2) heuristics — explicit
    /// token (`"py"`, `"js"`), path token via `extract_token_from_path`, then
    /// first-line content (shebang, mode line, Emacs markers).
    /// Host call: `detect_language(path, content, None, language_id)`.
    pub(crate) fn detect_language(
        &self,
        path: &str,
        content: &str,
        token: Option<&str>,
        language_id: Option<&str>,
    ) -> Option<String> {
        self.detect_language_logged(path, content, token, language_id, log::Level::Debug)
    }

    /// Host-language detection for high-frequency paths that log at TRACE.
    pub(crate) fn detect_language_trace(
        &self,
        path: &str,
        content: &str,
        token: Option<&str>,
        language_id: Option<&str>,
    ) -> Option<String> {
        self.detect_language_logged(path, content, token, language_id, log::Level::Trace)
    }

    /// Canonical injection language for bridge selection and virtual URIs.
    ///
    /// Candidate selection deliberately does not inspect parser state: eager
    /// bridge selection and virtual URIs must stay stable before and after a
    /// parser loads. An eligible configured base mapping for the explicit
    /// identifier takes precedence. An otherwise unconfigured explicit
    /// `plaintext` remains `plaintext`; other identifiers proceed through syntect
    /// token normalization and then first-line fallback. Consequently,
    /// registering a parser under a non-canonical key such as `py` or `js` does
    /// not change bridge keys/URIs: bridge configuration uses
    /// `python`/`javascript` unless the explicit identifier itself has an
    /// eligible base mapping (for example, `py.base`).
    pub(crate) fn canonical_injection_language(&self, identifier: &str, content: &str) -> String {
        if let Some(base) = self.resolve_base(identifier) {
            return base;
        }
        if identifier == "plaintext" {
            return identifier.to_string();
        }
        if let Some(candidate) = super::heuristic::detect_from_token(identifier) {
            return self.resolve_base(&candidate).unwrap_or(candidate);
        }
        if let Some(candidate) = super::heuristic::detect_from_first_line(content) {
            return self.resolve_base(&candidate).unwrap_or(candidate);
        }
        identifier.to_string()
    }

    fn detect_language_logged(
        &self,
        path: &str,
        content: &str,
        token: Option<&str>,
        language_id: Option<&str>,
        level: log::Level,
    ) -> Option<String> {
        let (result, method, candidate) =
            self.detect_language_with_method(path, content, token, language_id);

        match result {
            Some(ref lang) => {
                log::log!(
                    target: "kakehashi::language_detection",
                    level,
                    "Detected '{}' via {} for path='{}'",
                    lang,
                    method,
                    path
                );
            }
            None => {
                if let Some(detected) = candidate {
                    log::log!(
                        target: "kakehashi::language_detection",
                        level,
                        "Detected '{}' but no parser available for path='{}'",
                        detected,
                        path
                    );
                } else {
                    log::log!(
                        target: "kakehashi::language_detection",
                        level,
                        "No language detected for path='{}'",
                        path
                    );
                }
            }
        }

        result
    }

    /// Internal detection returning (result, method, last_candidate_without_parser).
    ///
    /// - `result`: The detected language with an available parser, if found
    /// - `method`: Description of the detection method that succeeded
    /// - `last_candidate`: The last detected candidate without a parser (for logging)
    fn detect_language_with_method(
        &self,
        path: &str,
        content: &str,
        token: Option<&str>,
        language_id: Option<&str>,
    ) -> (Option<String>, &'static str, Option<String>) {
        // Track last detected candidate without parser for logging
        let mut last_candidate: Option<String> = None;

        // 1. Try languageId (with base fallback)
        if let Some(lang_id) = language_id
            && lang_id != "plaintext"
        {
            if let Some(result) = self.try_with_base_fallback(lang_id) {
                return (Some(result), "languageId", None);
            }
            last_candidate = Some(lang_id.to_string());
        }

        // 2. Try heuristic detection: token, path-derived token, first line (shebang)
        //    Short-circuits on first successful match.
        //
        // Token priority (language-detection-fallback-chain):
        // - Explicit token (e.g., code fence identifier) takes precedence
        // - Path-derived token (extension or basename) is used as fallback
        // - First line (shebang/mode line) for extensionless files
        let effective_token = token.or_else(|| super::heuristic::extract_token_from_path(path));

        if let Some(tok) = effective_token {
            // First try syntect-based detection (normalizes "py" → "python", etc.)
            if let Some(candidate) = super::heuristic::detect_from_token(tok) {
                if let Some(result) = self.try_with_base_fallback(&candidate) {
                    let method = if token.is_some() {
                        "token"
                    } else {
                        "path-token"
                    };
                    return (Some(result), method, None);
                }
                // Syntect recognized but no parser available - record for logging
                last_candidate = Some(candidate);
            } else if let Some(result) = self.try_with_base_fallback(tok) {
                // Syntect doesn't recognize the token, try it directly as base
                // This handles extensions like "jsx", "tsx" that syntect doesn't know
                // but may be configured with base (e.g., jsx has base = "javascript")
                let method = if token.is_some() {
                    "token"
                } else {
                    "path-token"
                };
                return (Some(result), method, None);
            } else {
                // Neither syntect nor base resolved - record raw token for logging
                last_candidate = Some(tok.to_string());
            }
        }

        if let Some(candidate) = super::heuristic::detect_from_first_line(content) {
            if let Some(result) = self.try_with_base_fallback(&candidate) {
                return (Some(result), "first-line", None);
            }
            last_candidate = Some(candidate);
        }

        (None, "none", last_candidate)
    }

    /// language-detection-fallback-chain injection-language detection. Normally uses the same chain as `detect_language`
    /// (1: direct id, 2: syntect normalisation `py→python`, `js→javascript`,
    /// 3: first-line shebang/mode-line) but *loads* the parser instead of
    /// only checking availability — injection discovery runs before we know
    /// which parsers are needed. Each step runs through `try_load_with_base`
    /// for config-based base resolution (e.g. `rmd→markdown`). Explicit
    /// `plaintext` is the exception: only an eligible configured base is loaded,
    /// otherwise it remains featureless and returns `None` without heuristics.
    pub(crate) fn resolve_injection_language(
        &self,
        identifier: &str,
        content: &str,
    ) -> Option<(String, LanguageLoadResult)> {
        // trace!, not debug!: this runs per injection region per parse/walk
        // (thousands per keystroke on injection-heavy documents), and at
        // debug level the log volume itself becomes a measurable cost.
        log::trace!(
            target: "kakehashi::language_detection",
            "Resolving injection language for identifier='{}', content_len={}",
            identifier,
            content.len()
        );

        // 1. Try the identifier first. Explicit plaintext remains featureless
        // unless it has an eligible configured base mapping; loading plaintext
        // itself or applying content heuristics would violate that explicit key.
        if identifier == "plaintext" {
            let base = self.resolve_base(identifier)?;
            let found = self.try_load_with_base(&base)?;
            log::trace!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via configured plaintext base",
                identifier, found.0
            );
            return Some(found);
        }
        if let Some(found) = self.try_load_with_base(identifier) {
            log::trace!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via identifier (direct or base)",
                identifier, found.0
            );
            return Some(found);
        }

        // 2. Try syntect token normalization (handles py -> python, js -> javascript, etc.)
        if let Some(normalized) = super::heuristic::detect_from_token(identifier)
            && let Some(found) = self.try_load_with_base(&normalized)
        {
            log::trace!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via syntect token (direct or base)",
                identifier, found.0
            );
            return Some(found);
        }

        // 3. Try first-line detection (shebang, mode line)
        if let Some(first_line_lang) = super::heuristic::detect_from_first_line(content)
            && let Some(found) = self.try_load_with_base(&first_line_lang)
        {
            log::trace!(
                target: "kakehashi::language_detection",
                "Resolved injection '{}' -> '{}' via first-line detection (direct or base)",
                identifier, found.0
            );
            return Some(found);
        }

        log::trace!(
            target: "kakehashi::language_detection",
            "Failed to resolve injection language for identifier='{}'",
            identifier
        );
        None
    }

    /// Try to load a language directly, then via config base resolution.
    ///
    /// Returns `(resolved_name, load_result)` on success, or `None` if
    /// neither direct load nor base resolution succeeded.
    fn try_load_with_base(&self, candidate: &str) -> Option<(String, LanguageLoadResult)> {
        let result = self.ensure_language_loaded(candidate);
        if result.success {
            return Some((candidate.to_string(), result));
        }
        if let Some(base) = self.resolve_base(candidate) {
            let result = self.ensure_language_loaded(&base);
            if result.success {
                return Some((base, result));
            }
        }
        None
    }

    /// Create a document parser pool.
    ///
    /// Visibility: pub(crate) - called by LSP layer (lsp_impl) and analysis modules
    /// to obtain parser instances for document processing.
    pub(crate) fn create_document_parser_pool(&self) -> DocumentParserPool {
        let parser_factory = ParserFactory::new(self.language_registry.clone());
        DocumentParserPool::new(parser_factory)
    }

    /// Access the query store.
    ///
    /// Visibility: pub(crate) - used internally by coordinator methods and
    /// by test code that needs to register queries directly.
    pub(crate) fn query_store(&self) -> &QueryStore {
        &self.query_store
    }

    /// Check if queries exist for a language.
    ///
    /// Visibility: pub(crate) - called by LSP layer (lsp_impl) to determine if
    /// semantic tokens should be refreshed after language load.
    pub(crate) fn has_queries(&self, lang_name: &str) -> bool {
        self.query_store().has_highlight_query(lang_name)
    }

    /// Get highlight query for a language.
    ///
    /// Visibility: pub(crate) - called by LSP layer (semantic_tokens) and analysis
    /// layer (refactor, semantic) for syntax highlighting and token analysis.
    pub(crate) fn highlight_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store().highlight_query(lang_name)
    }

    /// Get injection query for a language.
    ///
    /// Visibility: pub(crate) - called by LSP layer (multiple handlers) and analysis
    /// layer (refactor, semantic, selection) for nested language support.
    pub(crate) fn injection_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store().injection_query(lang_name)
    }

    /// Get bindings query for a language (lexical name resolution).
    ///
    /// Visibility: pub(crate) - called by the LSP layer's native
    /// definition/references/documentHighlight/rename contributors.
    pub(crate) fn bindings_query(&self, lang_name: &str) -> Option<Arc<tree_sitter::Query>> {
        self.query_store().bindings_query(lang_name)
    }

    pub(crate) fn bindings_query_versioned(
        &self,
        lang_name: &str,
    ) -> (u64, Option<Arc<tree_sitter::Query>>) {
        use crate::error::LockResultExt;

        let _reload = self
            .settings_reload_lock
            .lock()
            .recover_poison("LanguageCoordinator::bindings_query_versioned");
        (
            self.load_generation
                .load(std::sync::atomic::Ordering::Acquire),
            self.query_store().bindings_query(lang_name),
        )
    }

    /// Get capture mappings.
    ///
    /// Visibility: pub(crate) - called by LSP layer (semantic_tokens) and analysis
    /// layer (refactor) for custom capture-to-token-type mapping.
    pub(crate) fn capture_mappings(&self) -> Arc<CaptureMappings> {
        self.config_store.capture_mappings()
    }

    fn load_single_language(
        &self,
        lang_name: &str,
        config: &LanguageSettings,
        search_paths: &[PathBuf],
    ) -> LanguageLoadResult {
        let generation = self
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let pre_registered =
            config.parser.is_none() && self.has_current_parser_registration(lang_name, generation);
        let pre_registered_is_builtin =
            pre_registered && self.reload_scoped_registrations.get(lang_name).is_none();
        let configured_query_kinds = config.queries.as_ref().map(|queries| {
            queries
                .iter()
                .filter_map(|query| query.kind.or_else(|| infer_query_kind(&query.path)))
                .collect::<HashSet<_>>()
        });
        if pre_registered_is_builtin {
            for kind in QueryKind::ALL {
                let key = (lang_name.to_string(), kind);
                if !self.builtin_queries.contains_key(&key)
                    && let Some(query) = self.query_store.get_query(kind, lang_name)
                {
                    self.builtin_queries.insert(key, query);
                }
            }
        }
        if !pre_registered_is_builtin {
            // Query kinds absent from the new configuration must disappear; the
            // insertion helpers only replace kinds that successfully load.
            self.query_store.remove_queries(lang_name);
        } else {
            self.query_store.remove_queries(lang_name);
            for kind in QueryKind::ALL {
                let restore_kind = match &configured_query_kinds {
                    None => true,
                    Some(kinds) if kinds.is_empty() => false,
                    Some(kinds) => !kinds.contains(&kind),
                };
                if !restore_kind {
                    continue;
                }
                if let Some(query) = self
                    .builtin_queries
                    .get(&(lang_name.to_string(), kind))
                    .map(|entry| Arc::clone(entry.value()))
                {
                    self.query_store
                        .insert_query(kind, lang_name.to_string(), query);
                }
            }
        }
        let language = if pre_registered {
            self.language_registry
                .get(lang_name)
                .expect("current parser registration disappeared")
        } else {
            match self.load_parser(
                lang_name,
                config.parser.as_deref(),
                search_paths,
                LanguageLogLevel::Error,
            ) {
                Ok(lang) => lang,
                Err(result) => {
                    self.record_configured_load_failure(lang_name, generation);
                    self.record_failed_load(lang_name, generation);
                    return result;
                }
            }
        };
        if !pre_registered_is_builtin {
            self.register_configured_language(lang_name, language.clone());
        }

        let mut events = self.load_queries_for_language(lang_name, config, search_paths, &language);
        events.push(LanguageEvent::log(
            LanguageLogLevel::Info,
            format!("Language {lang_name} loaded."),
        ));
        LanguageLoadResult::success_with(events)
    }

    fn register_configured_language(&self, language_id: &str, language: Language) {
        let _registration = self
            .registration_lock
            .lock()
            .recover_poison("LanguageCoordinator::register_configured_language");
        self.language_registry
            .register(language_id.to_string(), language);
        self.configured_load_failures.remove(language_id);
        let generation = self
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        self.reload_scoped_registrations
            .insert(language_id.to_string(), generation);
    }

    fn record_configured_load_failure(&self, language_id: &str, generation: u64) {
        let _registration = self
            .registration_lock
            .lock()
            .recover_poison("LanguageCoordinator::record_configured_load_failure");
        self.configured_load_failures
            .insert(language_id.to_string(), generation);
        self.language_registry.unregister(language_id);
        self.reload_scoped_registrations.remove(language_id);
        self.query_store.remove_queries(language_id);
    }

    fn load_queries_for_language(
        &self,
        lang_name: &str,
        config: &LanguageSettings,
        search_paths: &[PathBuf],
        language: &Language,
    ) -> Vec<LanguageEvent> {
        let mut events = Vec::new();

        // Process unified queries field if present
        if let Some(queries) = &config.queries {
            events.extend(self.load_unified_queries(lang_name, queries, language));
            return events;
        }

        // Fall back to search paths when queries field is not specified
        if !search_paths.is_empty() {
            self.load_all_queries(
                language,
                search_paths,
                lang_name,
                "Loaded from search paths",
                &mut events,
            );
        }

        events
    }

    /// Load queries from the unified queries field (new format).
    ///
    /// Processes each QueryItem, using explicit kind or inferring from filename.
    /// Unknown patterns (where kind is None and cannot be inferred) are skipped.
    fn load_unified_queries(
        &self,
        lang_name: &str,
        queries: &[crate::config::settings::QueryItem],
        language: &Language,
    ) -> Vec<LanguageEvent> {
        let mut events = Vec::new();

        // Group query paths by their effective kind
        let mut highlights: Vec<String> = Vec::new();
        let mut bindings: Vec<String> = Vec::new();
        let mut injections: Vec<String> = Vec::new();

        for query in queries {
            let effective_kind = query.kind.or_else(|| infer_query_kind(&query.path));
            match effective_kind {
                Some(QueryKind::Highlights) => highlights.push(query.path.clone()),
                Some(QueryKind::Bindings) => bindings.push(query.path.clone()),
                Some(QueryKind::Injections) => injections.push(query.path.clone()),
                None => {
                    // Skip unrecognized patterns silently
                }
            }
        }

        for (query_kind, paths) in [
            (QueryKind::Highlights, &highlights),
            (QueryKind::Bindings, &bindings),
            (QueryKind::Injections, &injections),
        ] {
            if !paths.is_empty() {
                self.load_query_from_paths(
                    language,
                    paths,
                    QueryLoadContext {
                        language_id: lang_name,
                        query_kind,
                    },
                    &mut events,
                    |store, q| store.insert_query(query_kind, lang_name.to_string(), q),
                );
            }
        }

        events
    }

    /// Get a clone of the language registry for parallel processing.
    ///
    /// This allows the parallel injection processor to create thread-local
    /// parser factories without going through the coordinator's document
    /// parser pool. The registry is safely clonable (uses Arc internally).
    pub(crate) fn language_registry_for_parallel(&self) -> LanguageRegistry {
        self.language_registry.clone()
    }

    /// Configured root search paths, for request-time resolution of dynamic
    /// capture query kinds — `queries/<lang>/<kind>.scm` lookups against the
    /// same bases config-time loading uses (captures-protocol §"Kind resolution").
    pub(crate) fn search_paths(&self) -> Vec<std::path::PathBuf> {
        self.config_store.search_paths()
    }

    /// Resolve the dynamic grammar identity the isolated tree worker must load.
    /// Derived languages that share a base parser use the base export symbol;
    /// standalone derived parsers retain their own symbol.
    pub(crate) fn worker_grammar_descriptor(
        &self,
        language_id: &str,
    ) -> Option<WorkerGrammarDescriptor> {
        let epoch = self
            .worker_settings_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if !epoch.is_multiple_of(2) {
            return None;
        }
        let settings = Arc::clone(
            &self
                .worker_settings
                .read()
                .recover_poison("LanguageCoordinator::worker_grammar_descriptor(settings)"),
        );
        let grammar_symbol = self
            .resolve_base(language_id)
            .unwrap_or_else(|| language_id.to_string());
        let parser_config = settings
            .languages
            .get(&grammar_symbol)
            .and_then(|language| language.parser.as_deref());
        let parser_path = QueryLoader::resolve_library_path(
            parser_config,
            &grammar_symbol,
            &self.config_store.search_paths(),
        )?;
        let parser_path = parser_path
            .canonicalize()
            .unwrap_or_else(|_| parser_path.clone());
        let configuration_generation = self
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        let cache_key = (parser_path.clone(), configuration_generation, epoch);
        let artifact = if let Some(artifact) = self.worker_artifacts.get(&cache_key) {
            artifact.clone()
        } else {
            let Some(artifact_dir) = &self.worker_artifact_dir else {
                log::warn!(
                    target: "kakehashi::tree_worker_shadow",
                    "cannot create private parser artifact directory",
                );
                return None;
            };
            let artifact = match import_worker_artifact(&parser_path, artifact_dir.path()) {
                Ok(artifact) => artifact,
                Err(error) => {
                    log::warn!(
                        target: "kakehashi::tree_worker_shadow",
                        "cannot import parser artifact {}: {error}",
                        parser_path.display(),
                    );
                    self.worker_artifacts
                        .iter()
                        .filter(|entry| {
                            entry.key().0 == parser_path && entry.key().1 < configuration_generation
                        })
                        .max_by_key(|entry| (entry.key().1, entry.key().2))
                        .map(|entry| entry.value().clone())?
                }
            };
            if self
                .worker_settings_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                != epoch
                || self
                    .load_generation
                    .load(std::sync::atomic::Ordering::Acquire)
                    != configuration_generation
            {
                return None;
            }
            self.worker_artifacts.insert(cache_key, artifact.clone());
            artifact
        };
        if self
            .worker_settings_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != epoch
        {
            return None;
        }
        Some(WorkerGrammarDescriptor {
            source_path: parser_path,
            parser_path: artifact.path,
            grammar_symbol,
            artifact_digest: artifact.digest,
            configuration_generation,
        })
    }

    pub(crate) fn configuration_generation(&self) -> u64 {
        self.load_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

fn import_worker_artifact(
    source: &Path,
    directory: &Path,
) -> std::io::Result<CachedWorkerArtifact> {
    const MAX_IMPORT_ATTEMPTS: usize = 3;
    for _ in 0..MAX_IMPORT_ATTEMPTS {
        let mut first = std::fs::File::open(source)?;
        let first_before = source_stamp(&first)?;
        let (first_digest, first_len) = hash_reader(&mut first, None)?;
        let first_after = source_stamp(&first)?;

        let mut second = std::fs::File::open(source)?;
        let second_before = source_stamp(&second)?;
        let mut candidate = tempfile::NamedTempFile::new_in(directory)?;
        let (second_digest, second_len) = hash_reader(&mut second, Some(&mut candidate))?;
        let second_after = source_stamp(&second)?;
        if first_before != first_after
            || second_before != second_after
            || first_before != second_before
            || first_digest != second_digest
            || first_len != second_len
        {
            continue;
        }
        candidate.as_file_mut().sync_all()?;
        let digest = format!("sha256:{first_digest}");
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or(std::env::consts::DLL_EXTENSION);
        let destination = directory.join(format!("{}.{}", &digest[7..], extension));
        match candidate.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error),
        }
        return Ok(CachedWorkerArtifact {
            path: destination,
            digest,
        });
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "parser artifact changed during all import attempts",
    ))
}

#[derive(Eq, PartialEq)]
struct SourceStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn source_stamp(file: &std::fs::File) -> std::io::Result<SourceStamp> {
    let metadata = file.metadata()?;
    Ok(SourceStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn hash_reader(
    source: &mut std::fs::File,
    mut destination: Option<&mut tempfile::NamedTempFile>,
) -> std::io::Result<(String, u64)> {
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        total = total.saturating_add(read as u64);
        if let Some(destination) = destination.as_mut() {
            destination.write_all(&buffer[..read])?;
        }
    }
    Ok((format!("{:x}", digest.finalize()), total))
}

/// Truncate a pattern string for display in log messages.
///
/// Collapses whitespace and truncates to max_len characters, adding "..." if truncated.
fn truncate_preview(pattern: &str, max_len: usize) -> String {
    // Collapse all whitespace (including newlines) to single spaces
    let collapsed: String = pattern.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = collapsed.chars().count();
    if char_count <= max_len {
        collapsed
    } else {
        // Truncate to max_len characters (including the "...")
        let truncate_at = max_len.saturating_sub(3);
        let truncated: String = collapsed.chars().take(truncate_at).collect();
        format!("{truncated}...")
    }
}

/// Self-referential entries without an explicit parser are configuration roots.
/// They participate in base-chain resolution but should not be eager-loaded.
fn should_eager_load_language(language_id: &str, config: &LanguageSettings) -> bool {
    !(config.base.as_deref() == Some(language_id) && config.parser.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::expand::make_env;
    use crate::config::settings::RawWorkspaceSettings;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn worker_grammar_descriptor_uses_shared_base_parser_identity() {
        let coordinator = LanguageCoordinator::new();
        let directory = tempdir().unwrap();
        let parser_path = directory.path().join("tree-sitter-markdown.so");
        fs::write(&parser_path, b"parser-v1").unwrap();
        let settings = WorkspaceSettings {
            languages: HashMap::from([
                (
                    "markdown".to_string(),
                    LanguageSettings {
                        parser: Some(parser_path.to_string_lossy().into_owned()),
                        ..Default::default()
                    },
                ),
                (
                    "rmd".to_string(),
                    LanguageSettings {
                        base: Some("markdown".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let _ = coordinator.load_settings(&settings);

        let descriptor = coordinator.worker_grammar_descriptor("rmd").unwrap();

        assert_eq!(descriptor.grammar_symbol, "markdown");
        assert_ne!(descriptor.parser_path, parser_path.canonicalize().unwrap());
        assert_eq!(fs::read(&descriptor.parser_path).unwrap(), b"parser-v1");

        let alias = coordinator.worker_grammar_descriptor("markdown").unwrap();
        assert_eq!(alias.artifact_digest, descriptor.artifact_digest);

        fs::write(&parser_path, b"parser-v2").unwrap();
        let cached = coordinator.worker_grammar_descriptor("rmd").unwrap();
        assert_eq!(cached.artifact_digest, descriptor.artifact_digest);
        assert_eq!(fs::read(&cached.parser_path).unwrap(), b"parser-v1");

        coordinator
            .load_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        let replaced = coordinator.worker_grammar_descriptor("rmd").unwrap();
        assert_ne!(replaced.artifact_digest, descriptor.artifact_digest);
        assert_ne!(replaced.parser_path, descriptor.parser_path);
        assert_eq!(fs::read(&replaced.parser_path).unwrap(), b"parser-v2");
    }

    /// Pins the #575 single-flight fix: a caller that arrives while another
    /// caller already claimed the in-flight marker for a language must PARK
    /// on that marker (not independently attempt its own load), and must
    /// wake once the marker is cleared and notified. Simulates a winner
    /// mid-load by claiming the marker directly (mirroring
    /// `captures_full_single_flight_loser_serves_winner_memo`'s technique) —
    /// a real dlopen fixture isn't needed to pin the coordination mechanic.
    #[tokio::test(start_paused = true)]
    async fn concurrent_load_for_the_same_language_parks_on_the_in_flight_winner() {
        let coordinator = Arc::new(LanguageCoordinator::new());
        let notify = Arc::new(tokio::sync::Notify::new());
        coordinator
            .load_inflight
            .insert("nosuchlang".to_string(), Arc::clone(&notify));

        let loser = Arc::clone(&coordinator);
        let request =
            tokio::spawn(async move { loser.ensure_language_loaded_async("nosuchlang").await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "a concurrent caller must park on the in-flight winner, not race it"
        );

        coordinator.load_inflight.remove("nosuchlang");
        notify.notify_waiters();

        // Woken, the loser loops back: the registry still doesn't contain
        // the language and no `failed_loads` entry exists (the simulated
        // winner never really ran), so it becomes its own winner and
        // attempts a fresh load — which fails fast (no search paths
        // configured) rather than hanging or panicking.
        let result = request
            .await
            .expect("the parked task must not panic on wake");
        assert!(
            !result.success,
            "a language with no search paths configured cannot load"
        );
    }

    /// Two callers racing the SAME cold language must run the load exactly
    /// once: only the winner's `try_load_language_by_id` runs and returns the
    /// "no search paths" event; the other caller (parked-then-cached, or
    /// arrived after the negative-cache entry landed) re-derives its verdict
    /// and returns an empty `LanguageLoadResult::default()`. Deleting the
    /// single-flight would let BOTH run the load -> two event-bearing results.
    /// Also pins the loser-gets-empty-events-on-failure contract.
    #[tokio::test]
    async fn two_concurrent_callers_run_the_load_once() {
        let coordinator = Arc::new(LanguageCoordinator::new());
        let a = Arc::clone(&coordinator);
        let b = Arc::clone(&coordinator);
        let (ra, rb) = tokio::join!(
            async move { a.ensure_language_loaded_async("nosuchlang").await },
            async move { b.ensure_language_loaded_async("nosuchlang").await },
        );

        let with_events = [&ra, &rb].iter().filter(|r| !r.events.is_empty()).count();
        assert_eq!(
            with_events, 1,
            "the load must run once, not once per caller"
        );
        assert!(!ra.success && !rb.success, "no search paths -> both fail");
        assert!(
            coordinator.load_inflight.is_empty(),
            "the RAII guard must clear the in-flight marker on exit"
        );
    }

    /// The 1s [`LANGUAGE_LOAD_WAIT_TIMEOUT`] backstop must free a loser whose
    /// wakeup was lost. Simulate the lost wakeup by removing the winner's
    /// marker WITHOUT notifying: only the backstop can now unpark the loser,
    /// which loops back and (finding no marker) becomes its own winner.
    /// Deleting the `sleep` arm makes this hang -> test timeout.
    ///
    /// NOTE: this pins the backstop's *recovery value*, not the `enable()` +
    /// `winner_live` recheck of the lost-wakeup fix (b428a1f9c) directly. That
    /// recheck branch is only reachable under true cross-thread timing — the
    /// loser prologue from `load_inflight.get()` through the recheck runs in a
    /// single uninterrupted poll with no `.await` seam, so a current-thread
    /// runtime cannot place a winner's `remove_if`+`notify_waiters()` at the
    /// one instant that would diverge the two branches. It is therefore not
    /// deterministically testable; a timing-based stress test was
    /// deliberately omitted to keep the suite flake-free.
    #[tokio::test(start_paused = true)]
    async fn parked_loser_recovers_via_backstop_when_wake_is_lost() {
        let coordinator = Arc::new(LanguageCoordinator::new());
        let notify = Arc::new(tokio::sync::Notify::new());
        coordinator
            .load_inflight
            .insert("nosuchlang".to_string(), Arc::clone(&notify));

        let loser = Arc::clone(&coordinator);
        let request =
            tokio::spawn(async move { loser.ensure_language_loaded_async("nosuchlang").await });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the loser must park while the marker is live"
        );

        // Remove the marker but never notify: a lost wakeup. Only the backstop
        // can free the loser now.
        coordinator.load_inflight.remove("nosuchlang");
        tokio::time::sleep(LANGUAGE_LOAD_WAIT_TIMEOUT + std::time::Duration::from_millis(1)).await;

        let result = request.await.expect("the loser must not panic");
        assert!(
            !result.success,
            "loops back, becomes winner, fast-fails on no search paths"
        );
    }

    #[test]
    fn test_injection_direct_identifier_first() {
        let coordinator = LanguageCoordinator::new();
        // Register "python" parser
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());

        // Direct identifier "python" should work
        let result = coordinator.resolve_injection_language("python", "print('hello')");
        assert!(result.is_some());
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_uses_syntect_token() {
        let coordinator = LanguageCoordinator::new();
        // Register "python" parser (not "py")
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());

        // Token "py" should resolve to "python" via syntect's detect_from_token
        let result = coordinator.resolve_injection_language("py", "print('hello')");
        assert!(result.is_some());
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_unknown_base_returns_none() {
        let coordinator = LanguageCoordinator::new();
        // No parsers registered

        // Unknown language with no parser should return None
        let result = coordinator.resolve_injection_language("unknown_lang", "");
        assert!(result.is_none());

        // Known token but no parser should also return None
        let result = coordinator.resolve_injection_language("py", "");
        assert!(result.is_none());
    }

    #[test]
    fn test_injection_uses_config_base() {
        // When config has rmd.base = "markdown", injection resolution should use it
        let coordinator = LanguageCoordinator::new();

        // Register "markdown" parser (not "rmd")
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Injection "rmd" should resolve to "markdown" via base
        let result = coordinator.resolve_injection_language("rmd", "");
        assert!(result.is_some(), "rmd should resolve via config base");
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "markdown");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_plaintext_uses_eligible_config_base() {
        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());
        coordinator.build_base_map(&HashMap::from([(
            "plaintext".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("python".to_string()),
                ..Default::default()
            },
        )]));

        let (resolved, load_result) = coordinator
            .resolve_injection_language("plaintext", "#!/usr/bin/env ruby")
            .expect("eligible plaintext base must drive parser resolution");

        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_plaintext_without_base_stays_featureless() {
        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());

        assert!(
            coordinator
                .resolve_injection_language("plaintext", "#!/usr/bin/env python")
                .is_none()
        );
    }

    #[test]
    fn test_injection_prefers_direct_over_base() {
        let coordinator = LanguageCoordinator::new();
        // Register both "js" and "javascript" as separate parsers
        let registry = coordinator.language_registry_for_parallel();
        registry.register("js".to_string(), tree_sitter_rust::LANGUAGE.into());
        registry.register("javascript".to_string(), tree_sitter_rust::LANGUAGE.into());

        // "js" should resolve to "js" (direct), not "javascript" (base)
        let result = coordinator.resolve_injection_language("js", "");
        assert!(result.is_some());
        let (resolved, _) = result.unwrap();
        assert_eq!(
            resolved, "js",
            "Direct identifier should be preferred over base"
        );
    }

    #[test]
    fn test_injection_uses_first_line_detection() {
        // When identifier doesn't match and token normalization fails,
        // fall back to first-line (shebang/mode line) detection
        let coordinator = LanguageCoordinator::new();

        // Register "python" parser
        coordinator
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());

        // Injection with unknown identifier but Python shebang in content
        let content = "#!/usr/bin/env python\nprint('hello')";
        let result = coordinator.resolve_injection_language("script", content);

        assert!(
            result.is_some(),
            "Should detect python via shebang when identifier is unknown"
        );
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "python");
        assert!(load_result.success);
    }

    #[test]
    fn test_injection_first_line_with_base() {
        // First-line detection should also try config base resolution
        let coordinator = LanguageCoordinator::new();

        // Register "bash" parser
        coordinator
            .language_registry_for_parallel()
            .register("bash".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Content with bash shebang - syntect detects "bash" for #!/bin/bash
        let content = "#!/bin/bash\necho hello";
        let result = coordinator.resolve_injection_language("unknown", content);

        assert!(result.is_some(), "Should detect bash via shebang");
        let (resolved, load_result) = result.unwrap();
        assert_eq!(resolved, "bash");
        assert!(load_result.success);
    }

    #[test]
    fn test_load_settings_does_not_make_parser_available() {
        // Documents that load_settings alone does NOT make parsers available.
        // ensure_language_loaded must be called to actually load the parser.
        // This is important for reload_language_after_install to work correctly.
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();

        // Initially, parser is not available
        assert!(
            !coordinator.has_parser_available("rust"),
            "Parser should not be available before load_settings"
        );

        // Load settings (simulating apply_settings behavior)
        let settings = WorkspaceSettings::default();
        let _summary = coordinator.load_settings(&settings);

        // After load_settings, parser is STILL not available
        assert!(
            !coordinator.has_parser_available("rust"),
            "Parser should not be available after load_settings alone - ensure_language_loaded must be called"
        );
    }

    // Smoke tests for coordinator API (moved from integration tests)
    // These verify the API surface exists and basic functionality works

    #[test]
    fn test_coordinator_has_parser_available() {
        let coordinator = LanguageCoordinator::new();

        // No languages loaded initially - should return false
        assert!(!coordinator.has_parser_available("rust"));

        // Full behavior (true when loaded) is tested in other unit tests.
    }

    #[test]
    fn test_heuristic_used_when_language_id_plaintext() {
        let coordinator = LanguageCoordinator::new();

        // When languageId is "plaintext", skip it and fall through to heuristic.
        // Python shebang detected but no parser loaded.
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/script", content, None, Some("plaintext"));

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_falls_back_to_heuristic_when_language_id_missing_parser() {
        let coordinator = LanguageCoordinator::new();

        // languageId "rust" has no parser, falls through to heuristic.
        // Python shebang overrides "rust" as last_candidate.
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/script", content, None, Some("rust"));

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_extension_fallback_after_heuristic() {
        let coordinator = LanguageCoordinator::new();

        // No languageId. Path extension "rs" → syntect detects "rust" (no parser).
        // Then first-line shebang detects "python" (no parser) → last candidate.
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/path/to/file.rs", content, None, None);

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn test_full_detection_chain() {
        let coordinator = LanguageCoordinator::new();

        // Full chain: "plaintext" skipped, "rs" extension → "rust" (no parser),
        // first-line shebang → "python" (no parser).
        let content = "#!/usr/bin/env python\nprint('hello')";
        let (result, method, last_candidate) = coordinator.detect_language_with_method(
            "/path/to/file.rs",
            content,
            None,
            Some("plaintext"),
        );

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("python".to_string()));
    }

    #[test]
    fn candidate_language_for_document_does_not_require_loaded_parser() {
        let coordinator = LanguageCoordinator::new();

        assert_eq!(
            coordinator.candidate_language_for_document("/path/to/file.rs", ""),
            Some("rust".to_string())
        );
        assert_eq!(
            coordinator.candidate_language_for_document("/script", "#!/usr/bin/env python\n"),
            Some("python".to_string())
        );
        assert_eq!(
            coordinator.candidate_language_for_document("/path/to/file.my_lang", ""),
            Some("my_lang".to_string())
        );
    }

    #[test]
    fn test_detection_chain_returns_none_when_all_fail() {
        let coordinator = LanguageCoordinator::new();

        // No languageId, syntect doesn't recognize "random_file", no shebang.
        let (result, method, last_candidate) = coordinator.detect_language_with_method(
            "/random_file",
            "random content without shebang",
            None,
            None,
        );

        assert_eq!(result, None);
        assert_eq!(method, "none");
        // "random_file" basename is tried as token but syntect doesn't recognize it
        assert_eq!(last_candidate, Some("random_file".to_string()));
    }

    #[test]
    fn test_heuristic_detects_makefile_by_filename() {
        let coordinator = LanguageCoordinator::new();

        // Basename "Makefile" → syntect maps to "make" (no parser loaded).
        let (result, method, last_candidate) =
            coordinator.detect_language_with_method("/path/to/Makefile", "all: build", None, None);

        assert_eq!(result, None);
        assert_eq!(method, "none");
        assert_eq!(last_candidate, Some("make".to_string()));
    }

    // Tests for load_unified_queries

    #[test]
    fn test_load_unified_queries_with_explicit_kind() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Create a temporary query file with valid highlights content
        let temp_dir = TempDir::new().unwrap();
        let query_path = temp_dir.path().join("my_highlights.scm");
        let mut file = fs::File::create(&query_path).unwrap();
        writeln!(file, "(identifier) @variable").unwrap();

        // Create QueryItem with explicit kind
        let queries = vec![QueryItem {
            path: query_path.to_str().unwrap().to_string(),
            kind: Some(QueryKind::Highlights),
        }];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have one info event for successful load
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            LanguageEvent::Log {
                level: LanguageLogLevel::Info,
                ..
            }
        ));

        // Verify the query was actually loaded
        assert!(
            coordinator.highlight_query("rust").is_some(),
            "Highlight query should be loaded"
        );
    }

    #[test]
    fn test_load_unified_queries_with_filename_inference() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Create a temporary query file with the exact filename "highlights.scm"
        let temp_dir = TempDir::new().unwrap();
        let query_path = temp_dir.path().join("highlights.scm");
        let mut file = fs::File::create(&query_path).unwrap();
        writeln!(file, "(identifier) @variable").unwrap();

        // Create QueryItem WITHOUT explicit kind - should infer from filename
        let queries = vec![QueryItem {
            path: query_path.to_str().unwrap().to_string(),
            kind: None, // No explicit kind - will be inferred
        }];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have one info event for successful load
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            LanguageEvent::Log {
                level: LanguageLogLevel::Info,
                ..
            }
        ));

        // Verify the query was actually loaded via inference
        assert!(
            coordinator.highlight_query("rust").is_some(),
            "Highlight query should be loaded via filename inference"
        );
    }

    #[test]
    fn test_load_unified_queries_unknown_patterns_skipped() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Create a temporary query file with a non-standard filename
        let temp_dir = TempDir::new().unwrap();
        let query_path = temp_dir.path().join("custom.scm");
        let mut file = fs::File::create(&query_path).unwrap();
        writeln!(file, "(identifier) @variable").unwrap();

        // Create QueryItem with unknown pattern (no explicit kind, non-standard filename)
        let queries = vec![QueryItem {
            path: query_path.to_str().unwrap().to_string(),
            kind: None, // No explicit kind and filename won't match inference patterns
        }];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries - should silently skip the unknown pattern
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have NO events because the unknown pattern was silently skipped
        assert_eq!(
            events.len(),
            0,
            "Unknown patterns should be silently skipped with no events"
        );

        // Verify no queries were loaded
        assert!(
            coordinator.highlight_query("rust").is_none(),
            "No highlight query should be loaded for unknown pattern"
        );
    }

    #[test]
    fn test_load_unified_queries_grouped_by_type() {
        use crate::config::settings::QueryItem;
        use std::fs;
        use std::io::Write;
        use tempfile::TempDir;

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Create temporary query files for different types
        let temp_dir = TempDir::new().unwrap();

        // Highlights query
        let highlights_path = temp_dir.path().join("highlights.scm");
        let mut highlights_file = fs::File::create(&highlights_path).unwrap();
        writeln!(highlights_file, "(identifier) @variable").unwrap();

        // Bindings query
        let bindings_path = temp_dir.path().join("bindings.scm");
        let mut bindings_file = fs::File::create(&bindings_path).unwrap();
        writeln!(bindings_file, "(identifier) @reference").unwrap();

        // Injections query
        let injections_path = temp_dir.path().join("injections.scm");
        let mut injections_file = fs::File::create(&injections_path).unwrap();
        writeln!(injections_file, "; empty injection query").unwrap();

        // Create mixed QueryItems - some with explicit kind, some with inference
        let queries = vec![
            QueryItem {
                path: highlights_path.to_str().unwrap().to_string(),
                kind: None, // Will be inferred as Highlights
            },
            QueryItem {
                path: bindings_path.to_str().unwrap().to_string(),
                kind: Some(QueryKind::Bindings), // Explicit kind
            },
            QueryItem {
                path: injections_path.to_str().unwrap().to_string(),
                kind: None, // Will be inferred as Injections
            },
        ];

        // Get the language
        let language = coordinator
            .language_registry
            .get("rust")
            .expect("Language should be registered");

        // Load queries
        let events = coordinator.load_unified_queries("rust", &queries, &language);

        // Should have 3 info events (one for each query type)
        assert_eq!(events.len(), 3, "Should have 3 events for 3 query types");
        for event in &events {
            assert!(
                matches!(
                    event,
                    LanguageEvent::Log {
                        level: LanguageLogLevel::Info,
                        ..
                    }
                ),
                "All events should be Info level"
            );
        }

        // Verify highlight query was loaded (bindings and injections confirmed by events)
        assert!(
            coordinator.highlight_query("rust").is_some(),
            "Highlight query should be loaded"
        );
    }

    #[test]
    fn bindings_query_stays_absent_without_disk_sources() {
        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let language = coordinator.language_registry.get("rust").unwrap();

        // No search paths, no unified queries: nothing is bundled into the
        // binary, so the native layer stays silent for this language.
        let config = crate::config::settings::LanguageSettings::default();
        let _events = coordinator.load_queries_for_language("rust", &config, &[], &language);
        assert!(
            coordinator.bindings_query("rust").is_none(),
            "no bindings.scm may appear out of thin air"
        );
    }

    #[test]
    fn search_path_bindings_load() {
        use std::io::Write;

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let language = coordinator.language_registry.get("rust").unwrap();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let lang_dir = temp_dir.path().join("queries").join("rust");
        std::fs::create_dir_all(&lang_dir).unwrap();
        let mut file = std::fs::File::create(lang_dir.join("bindings.scm")).unwrap();
        writeln!(file, "(identifier) @reference").unwrap();

        let config = crate::config::settings::LanguageSettings::default();
        let _events = coordinator.load_queries_for_language(
            "rust",
            &config,
            &[temp_dir.path().to_path_buf()],
            &language,
        );
        let query = coordinator.bindings_query("rust").expect("loaded");
        assert_eq!(query.pattern_count(), 1);
    }

    // Tests for base resolution

    #[test]
    fn test_base_resolution_detects_canonical_language() {
        // When languageId "rmd" has base = "markdown" and markdown parser exists,
        // detect_language should return "markdown"
        let coordinator = LanguageCoordinator::new();

        // Register "markdown" parser (using rust as a stand-in)
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "rmd" → "markdown", "qmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        languages.insert(
            "qmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Detection with languageId "rmd" should resolve to "markdown"
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result,
            Some("markdown".to_string()),
            "rmd should resolve to markdown via base"
        );

        // Also test qmd
        let result = coordinator.detect_language("/path/to/file.qmd", "", None, Some("qmd"));
        assert_eq!(
            result,
            Some("markdown".to_string()),
            "qmd should also resolve to markdown via base"
        );
    }

    #[test]
    fn test_load_settings_surfaces_deprecated_aliases_to_client() {
        let coordinator = LanguageCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                aliases: Some(vec!["rmd".to_string(), "qmd".to_string()]),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            summary.events.iter().any(|event| matches!(
                event,
                LanguageEvent::ShowMessage { level, message }
                    if *level == LanguageLogLevel::Warning
                        && message.contains("deprecated 'aliases' field")
                        && message.contains("Use 'base' on derived languages instead")
            )),
            "load_settings should emit a client-visible migration warning for deprecated aliases"
        );
    }

    #[test]
    fn test_load_settings_self_ref_base_no_warning() {
        let coordinator = LanguageCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("rmd".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        // Self-referential base is a normal chain terminator, not a warning
        assert!(
            !summary.events.iter().any(|event| matches!(
                event,
                LanguageEvent::ShowMessage { level, .. }
                    if *level == LanguageLogLevel::Warning
            )),
            "load_settings should not emit warnings for self-referencing base. Events: {:?}",
            summary.events
        );
    }

    #[test]
    fn test_load_settings_skips_wildcard_inheritance_root() {
        let coordinator = LanguageCoordinator::new();

        let settings = WorkspaceSettings {
            languages: HashMap::from([(
                "_".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("_".to_string()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            !summary.loaded.contains(&"_".to_string()),
            "wildcard inheritance root should not be recorded as loaded"
        );
        assert!(
            !summary.events.iter().any(|event| matches!(
                event,
                LanguageEvent::Log { level, message }
                    if *level == LanguageLogLevel::Error && message.contains("'_'")
            )),
            "wildcard inheritance root should not emit parser load errors: {:?}",
            summary.events
        );
    }

    #[test]
    fn test_load_settings_skips_named_inheritance_root() {
        let coordinator = LanguageCoordinator::new();

        let settings = WorkspaceSettings {
            languages: HashMap::from([(
                "custom_default".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("custom_default".to_string()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            !summary.loaded.contains(&"custom_default".to_string()),
            "named inheritance root should not be recorded as loaded"
        );
        assert!(
            !summary.events.iter().any(|event| matches!(
                event,
                LanguageEvent::Log { level, message }
                    if *level == LanguageLogLevel::Error && message.contains("custom_default")
            )),
            "named inheritance root should not emit parser load errors: {:?}",
            summary.events
        );
    }

    #[test]
    fn test_load_settings_loads_language_through_named_inheritance_root() {
        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let parser_path = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("markdown.{}", std::env::consts::DLL_EXTENSION));
        if !parser_path.exists() {
            eprintln!(
                "skipping test_load_settings_loads_language_through_named_inheritance_root: parser '{}' does not exist",
                parser_path.display()
            );
            return;
        }

        let raw_settings = RawWorkspaceSettings {
            search_paths: Some(vec![grammar_dir]),
            languages: HashMap::from([
                (
                    "markdown".to_string(),
                    crate::config::settings::LanguageSettings {
                        base: Some("custom_default".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    "custom_default".to_string(),
                    crate::config::settings::LanguageSettings {
                        base: Some("custom_default".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let settings = WorkspaceSettings::try_from_settings(&raw_settings, None, make_env(&[]))
            .expect("settings should resolve");

        let summary = coordinator.load_settings(&settings);

        assert!(
            coordinator.has_parser_available("markdown"),
            "markdown should load via its own language id through the inheritance-only root, summary={summary:?}"
        );
        assert!(
            summary.loaded.contains(&"markdown".to_string()),
            "markdown should be recorded as loaded, summary={summary:?}"
        );
        assert!(
            !summary.loaded.contains(&"custom_default".to_string()),
            "named inheritance root should not be recorded as loaded, summary={summary:?}"
        );
    }

    #[test]
    fn test_base_resolution_prefers_direct_language() {
        // When languageId directly has a parser, use it (don't check base)
        let coordinator = LanguageCoordinator::new();

        // Register both "rmd" and "markdown" as separate parsers
        let registry = coordinator.language_registry_for_parallel();
        registry.register("rmd".to_string(), tree_sitter_rust::LANGUAGE.into());
        registry.register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Detection with languageId "rmd" should use "rmd" directly (not base)
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result,
            Some("rmd".to_string()),
            "Direct languageId should be preferred over base"
        );
    }

    #[test]
    fn test_base_resolution_skips_base_fallback_for_custom_parser_languages() {
        let coordinator = LanguageCoordinator::new();

        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                parser: Some("/missing/rmd-parser.dylib".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result, None,
            "language with its own parser should not fall back to its base language"
        );
    }

    #[test]
    fn test_injection_resolution_skips_base_fallback_for_custom_parser_languages() {
        let coordinator = LanguageCoordinator::new();

        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                parser: Some("/missing/rmd-parser.dylib".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        let result = coordinator.resolve_injection_language("rmd", "");
        assert!(
            result.is_none(),
            "injection language with its own parser should not fall back to its base language"
        );
    }

    #[test]
    fn test_inherited_parser_preserves_base_fallback_after_try_from_settings() {
        let coordinator = LanguageCoordinator::new();

        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), tree_sitter_rust::LANGUAGE.into());

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                parser: Some("/opt/parsers/markdown.so".to_string()),
                ..Default::default()
            },
        );
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );

        let settings = RawWorkspaceSettings {
            languages,
            ..Default::default()
        };
        let settings =
            crate::config::WorkspaceSettings::try_from_settings(&settings, None, make_env(&[]))
                .expect("settings should resolve inherited parser");

        coordinator.build_base_map(&settings.languages);

        let result = coordinator.resolve_injection_language("rmd", "");
        assert!(
            result.is_some(),
            "derived language that only inherits parser should still fall back to base"
        );

        let (resolved, _) = result.unwrap();
        assert_eq!(resolved, "markdown");
    }

    #[test]
    fn test_base_resolution_skipped_when_no_parser_for_base() {
        // When base points to a language without a parser, continue fallback
        let coordinator = LanguageCoordinator::new();

        // Don't register any parser - only the base mapping

        // Build base map: "rmd" → "markdown"
        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // Detection should return None (base found but no parser for "markdown")
        let result = coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd"));
        assert_eq!(
            result, None,
            "Should return None when base target has no parser"
        );
    }

    #[test]
    fn test_base_map_cleared_on_reload() {
        // Verify that base map is cleared and rebuilt when settings change
        let coordinator = LanguageCoordinator::new();

        // First config: "rmd" → "markdown"
        let mut languages1 = HashMap::new();
        languages1.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages1);

        // Verify first mapping
        assert_eq!(
            coordinator.resolve_base("rmd"),
            Some("markdown".to_string())
        );

        // Second config: no base for rmd, "jsx" → "javascript"
        let mut languages2 = HashMap::new();
        languages2.insert(
            "jsx".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("javascript".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages2);

        // Old base should be gone
        assert_eq!(
            coordinator.resolve_base("rmd"),
            None,
            "Old base should be cleared after rebuild"
        );
        // New base should work
        assert_eq!(
            coordinator.resolve_base("jsx"),
            Some("javascript".to_string())
        );
    }

    // language-detection-fallback-chain: Base resolution as sub-step for extension detection

    #[test]
    fn test_extension_detection_with_base_fallback() {
        // When extension is "jsx" and base maps "jsx" → "javascript",
        // detect_language should return "javascript"
        let coordinator = LanguageCoordinator::new();

        // Register "javascript" parser (using rust as a stand-in)
        coordinator
            .language_registry_for_parallel()
            .register("javascript".to_string(), tree_sitter_rust::LANGUAGE.into());

        // Build base map: "jsx" → "javascript"
        let mut languages = HashMap::new();
        languages.insert(
            "jsx".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("javascript".to_string()),
                ..Default::default()
            },
        );
        coordinator.build_base_map(&languages);

        // No languageId, no shebang, extension = jsx
        let result = coordinator.detect_language("/path/to/component.jsx", "", None, None);

        assert_eq!(
            result,
            Some("javascript".to_string()),
            "jsx extension should resolve to javascript via base"
        );
    }

    // Tests for two-pass loading of base languages

    #[test]
    fn test_load_settings_registers_derived_language_from_base() {
        // When rmd has base = "markdown" and markdown parser is loadable,
        // load_settings should register "rmd" with markdown's parser
        let coordinator = LanguageCoordinator::new();

        // Manually register "markdown" parser (simulating it being loaded)
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query =
            std::sync::Arc::new(tree_sitter::Query::new(&lang, "(identifier) @variable").unwrap());
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), lang);
        coordinator
            .query_store()
            .insert_highlight_query("markdown".to_string(), query.clone());

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        // rmd should now have a parser available
        assert!(
            coordinator.has_parser_available("rmd"),
            "rmd should have parser from base markdown"
        );

        // rmd should also have the highlight query
        assert!(
            coordinator.has_queries("rmd"),
            "rmd should have queries from base markdown"
        );

        // rmd should be recorded as loaded
        assert!(
            summary.loaded.contains(&"rmd".to_string()),
            "rmd should be recorded as loaded"
        );
    }

    #[test]
    fn test_load_settings_derived_language_uses_effective_queries() {
        let coordinator = LanguageCoordinator::new();

        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let base_query = std::sync::Arc::new(
            tree_sitter::Query::new(&language, "(identifier) @variable").unwrap(),
        );
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), language);
        coordinator
            .query_store()
            .insert_highlight_query("markdown".to_string(), base_query);

        let dir = tempdir().unwrap();
        let custom_query = dir.path().join("rmd-highlights.scm");
        fs::write(&custom_query, "(identifier) @function").unwrap();

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                queries: Some(vec![crate::config::settings::QueryItem {
                    path: custom_query.display().to_string(),
                    kind: Some(crate::config::settings::QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            summary.loaded.contains(&"rmd".to_string()),
            "rmd should be recorded as loaded"
        );

        let rmd_query = coordinator
            .highlight_query("rmd")
            .expect("rmd should have a highlight query");
        assert_eq!(
            rmd_query.capture_names(),
            &["function".to_string()],
            "derived language should use its effective queries instead of copying base queries"
        );
    }

    #[test]
    fn test_load_settings_derived_language_uses_effective_queries_with_dynamic_base() {
        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let parser_path = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("markdown.{}", std::env::consts::DLL_EXTENSION));
        if !parser_path.exists() {
            eprintln!(
                "skipping test_load_settings_derived_language_uses_effective_queries_with_dynamic_base: parser '{}' does not exist",
                parser_path.display()
            );
            return;
        }

        let search_dir = tempdir().unwrap();
        let markdown_query_dir = search_dir.path().join("queries").join("markdown");
        fs::create_dir_all(&markdown_query_dir).unwrap();
        fs::write(
            markdown_query_dir.join("highlights.scm"),
            "(paragraph) @markup.heading",
        )
        .unwrap();

        let custom_query = search_dir.path().join("rmd-highlights.scm");
        fs::write(&custom_query, "(paragraph) @function").unwrap();

        let settings = WorkspaceSettings {
            search_paths: vec![
                search_dir.path().to_string_lossy().into_owned(),
                grammar_dir.clone(),
            ],
            languages: HashMap::from([(
                "rmd".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("markdown".to_string()),
                    queries: Some(vec![crate::config::settings::QueryItem {
                        path: custom_query.display().to_string(),
                        kind: Some(crate::config::settings::QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            summary.loaded.contains(&"rmd".to_string()),
            "rmd should be recorded as loaded, summary={summary:?}"
        );

        let rmd_query = coordinator
            .highlight_query("rmd")
            .expect("rmd should have a highlight query");
        assert_eq!(
            rmd_query.capture_names(),
            &["function".to_string()],
            "derived language should use its explicit queries even when base loads from search paths"
        );
    }

    #[test]
    fn test_load_settings_loads_circular_base_chain_from_resolved_config() {
        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let parser_path = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("rust.{}", std::env::consts::DLL_EXTENSION));
        if !parser_path.exists() {
            eprintln!(
                "skipping test_load_settings_loads_circular_base_chain_from_resolved_config: parser '{}' does not exist",
                parser_path.display()
            );
            return;
        }

        let query_dir = tempdir().unwrap();
        let highlight_path = query_dir.path().join("rust-highlights.scm");
        fs::write(&highlight_path, "(identifier) @variable").unwrap();

        let unresolved_languages = HashMap::from([
            (
                "rust".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("derived-rust".to_string()),
                    parser: Some(parser_path.to_string_lossy().into_owned()),
                    queries: Some(vec![crate::config::settings::QueryItem {
                        path: highlight_path.to_string_lossy().into_owned(),
                        kind: Some(crate::config::settings::QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            ),
            (
                "derived-rust".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("rust".to_string()),
                    ..Default::default()
                },
            ),
        ]);
        let settings = WorkspaceSettings {
            languages: crate::config::merge::resolve_base_configs(&unresolved_languages),
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            coordinator.has_parser_available("rust"),
            "rust should load from its resolved parser config, summary={summary:?}"
        );
        assert!(
            coordinator.has_parser_available("derived-rust"),
            "derived-rust should load from the resolved circular chain, summary={summary:?}"
        );
        assert!(
            coordinator.has_queries("derived-rust"),
            "derived-rust should inherit effective queries from the resolved circular chain, summary={summary:?}"
        );
        assert!(
            summary.loaded.contains(&"rust".to_string()),
            "rust should be recorded as loaded, summary={summary:?}"
        );
        assert!(
            summary.loaded.contains(&"derived-rust".to_string()),
            "derived-rust should be recorded as loaded, summary={summary:?}"
        );
    }

    #[test]
    fn test_load_settings_detects_mutual_cycle_between_derived_languages() {
        // Two derived languages that reference each other: A base=B, B base=A
        // Neither has a parser, so neither can fall back to standalone loading.
        // Both should fail with cycle/not-found errors (not hang or succeed silently).
        let coordinator = LanguageCoordinator::new();

        let unresolved_languages = HashMap::from([
            (
                "lang_a".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("lang_b".to_string()),
                    ..Default::default()
                },
            ),
            (
                "lang_b".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("lang_a".to_string()),
                    ..Default::default()
                },
            ),
        ]);
        let settings = WorkspaceSettings {
            languages: crate::config::merge::resolve_base_configs(&unresolved_languages),
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            !coordinator.has_parser_available("lang_a"),
            "lang_a should not load (mutual cycle), summary={summary:?}"
        );
        assert!(
            !coordinator.has_parser_available("lang_b"),
            "lang_b should not load (mutual cycle), summary={summary:?}"
        );
        assert!(
            !summary.loaded.contains(&"lang_a".to_string()),
            "lang_a should not be in loaded list, summary={summary:?}"
        );
        assert!(
            !summary.loaded.contains(&"lang_b".to_string()),
            "lang_b should not be in loaded list, summary={summary:?}"
        );
    }

    #[test]
    fn test_load_settings_clears_removed_derived_language_registrations() {
        let coordinator = LanguageCoordinator::new();

        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query =
            std::sync::Arc::new(tree_sitter::Query::new(&lang, "(identifier) @variable").unwrap());
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), lang);
        coordinator
            .query_store()
            .insert_highlight_query("markdown".to_string(), query);

        let mut initial_languages = HashMap::new();
        initial_languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        initial_languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        coordinator.load_settings(&WorkspaceSettings {
            languages: initial_languages,
            ..Default::default()
        });
        assert!(coordinator.has_parser_available("rmd"), "precondition");
        assert!(coordinator.has_queries("rmd"), "precondition");

        let mut reloaded_languages = HashMap::new();
        reloaded_languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        coordinator.load_settings(&WorkspaceSettings {
            languages: reloaded_languages,
            ..Default::default()
        });
        assert!(
            coordinator.has_parser_available("markdown"),
            "an untagged pre-registered base must survive repeated reloads"
        );

        assert!(
            !coordinator.has_parser_available("rmd"),
            "removed derived language should be unregistered on reload"
        );
        assert!(
            !coordinator.has_queries("rmd"),
            "removed derived language queries should be cleared on reload"
        );
        assert_eq!(
            coordinator.detect_language("/path/to/file.Rmd", "", None, Some("rmd")),
            None,
            "removed derived language should no longer resolve"
        );
    }

    #[test]
    fn test_load_settings_clears_removed_standalone_loaded_derived_language() {
        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let parser_path = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("markdown.{}", std::env::consts::DLL_EXTENSION));
        if !parser_path.exists() {
            eprintln!(
                "skipping test_load_settings_clears_removed_standalone_loaded_derived_language: parser '{}' does not exist",
                parser_path.display()
            );
            return;
        }

        let query_dir = tempdir().unwrap();
        let highlight_path = query_dir.path().join("markdown-highlights.scm");
        fs::write(&highlight_path, "(paragraph) @function").unwrap();

        let mut initial_languages = HashMap::new();
        initial_languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("missing-markdown".to_string()),
                parser: Some(parser_path.to_string_lossy().into_owned()),
                queries: Some(vec![crate::config::settings::QueryItem {
                    path: highlight_path.to_string_lossy().into_owned(),
                    kind: Some(crate::config::settings::QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );
        let summary = coordinator.load_settings(&WorkspaceSettings {
            languages: initial_languages,
            ..Default::default()
        });
        assert!(
            coordinator.has_parser_available("markdown"),
            "precondition: expected custom parser to load, summary={summary:?}"
        );
        assert!(
            coordinator.has_queries("markdown"),
            "precondition: expected custom queries to load, summary={summary:?}"
        );

        coordinator.load_settings(&WorkspaceSettings {
            languages: HashMap::new(),
            ..Default::default()
        });

        assert!(
            !coordinator.has_parser_available("markdown"),
            "removed standalone-loaded derived language should be unregistered on reload"
        );
        assert!(
            !coordinator.has_queries("markdown"),
            "removed standalone-loaded derived language should have queries cleared on reload"
        );
    }

    #[test]
    fn settings_reload_invalidates_dynamic_registry_hits() {
        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry
            .register("dynamic".to_string(), tree_sitter_rust::LANGUAGE.into());
        coordinator
            .reload_scoped_registrations
            .insert("dynamic".to_string(), 0);
        assert!(
            coordinator.ensure_language_loaded("dynamic").success,
            "the dynamic parser is current before reload"
        );

        coordinator.load_settings(&WorkspaceSettings::default());

        assert!(
            coordinator.language_registry.contains("dynamic"),
            "the regression requires a stale registry entry to remain present"
        );
        assert!(
            !coordinator.has_parser_available("dynamic"),
            "detection must not accept a stale registration"
        );
        assert!(
            !coordinator.ensure_language_loaded("dynamic").success,
            "a prior-generation dynamic entry must not bypass current search-path resolution"
        );
    }

    #[test]
    fn stale_dynamic_load_cannot_retag_configured_registration() {
        let coordinator = LanguageCoordinator::new();
        coordinator.load_settings(&WorkspaceSettings::default());
        coordinator.register_configured_language("dynamic", tree_sitter_rust::LANGUAGE.into());

        assert!(
            !coordinator.publish_dynamic_language(
                "dynamic",
                tree_sitter_rust::LANGUAGE.into(),
                0,
                || {},
            ),
            "a load resolved before the reload must lose publication"
        );
        assert!(coordinator.language_registry.contains("dynamic"));
        assert!(
            coordinator
                .reload_scoped_registrations
                .get("dynamic")
                .is_some_and(|generation| *generation == 1),
            "the current configured parser must keep its newer generation"
        );
    }

    #[test]
    fn failed_config_reload_does_not_revalidate_old_parser() {
        let coordinator = LanguageCoordinator::new();
        coordinator.register_configured_language("configured", tree_sitter_rust::LANGUAGE.into());
        assert!(coordinator.ensure_language_loaded("configured").success);

        coordinator.load_settings(&WorkspaceSettings::default());

        assert!(
            !coordinator.ensure_language_loaded("configured").success,
            "an old configured registration must not survive a generation where it was not loaded"
        );
    }

    #[test]
    fn configured_parser_failure_blocks_dynamic_fallback() {
        let coordinator = LanguageCoordinator::new();
        let rust_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        coordinator.query_store.insert_highlight_query(
            "markdown".to_string(),
            std::sync::Arc::new(
                tree_sitter::Query::new(&rust_language, "(identifier) @variable").unwrap(),
            ),
        );
        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let dynamic_parser = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("markdown.{}", std::env::consts::DLL_EXTENSION));
        if !dynamic_parser.exists() {
            eprintln!(
                "skipping configured_parser_failure_blocks_dynamic_fallback: '{}' does not exist",
                dynamic_parser.display()
            );
            return;
        }
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings {
                parser: Some("/definitely/missing/parser.so".to_string()),
                ..Default::default()
            },
        );

        coordinator.load_settings(&WorkspaceSettings {
            languages,
            search_paths: vec![grammar_dir],
            ..Default::default()
        });
        let generation = coordinator
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        assert!(
            !coordinator.publish_dynamic_language(
                "markdown",
                tree_sitter_rust::LANGUAGE.into(),
                generation,
                || {},
            ),
            "configured failure must reject same-generation dynamic publication"
        );
        assert!(!coordinator.language_registry.contains("markdown"));
        assert!(
            coordinator.highlight_query("markdown").is_none(),
            "queries from the previous configured grammar must be removed"
        );

        assert!(
            !coordinator.ensure_language_loaded("markdown").success,
            "an explicit configured parser failure must not fall back to dynamic discovery"
        );
    }

    #[test]
    fn configured_failure_removes_racing_dynamic_publication() {
        let coordinator = LanguageCoordinator::new();
        coordinator.load_settings(&WorkspaceSettings::default());
        let generation = coordinator
            .load_generation
            .load(std::sync::atomic::Ordering::Acquire);
        assert!(coordinator.publish_dynamic_language(
            "racing",
            tree_sitter_rust::LANGUAGE.into(),
            generation,
            || {},
        ));

        coordinator.record_configured_load_failure("racing", generation);

        assert!(!coordinator.language_registry.contains("racing"));
        assert!(
            coordinator
                .reload_scoped_registrations
                .get("racing")
                .is_none()
        );
        assert!(coordinator.configured_load_failed("racing", generation));
    }

    #[test]
    fn reload_without_override_preserves_builtin_queries() {
        let coordinator = LanguageCoordinator::new();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        coordinator
            .language_registry
            .register("builtin".to_string(), language.clone());
        coordinator.query_store.insert_highlight_query(
            "builtin".to_string(),
            std::sync::Arc::new(
                tree_sitter::Query::new(&language, "(identifier) @variable").unwrap(),
            ),
        );
        let mut languages = HashMap::new();
        languages.insert("builtin".to_string(), LanguageSettings::default());

        coordinator.load_settings(&WorkspaceSettings {
            languages,
            ..Default::default()
        });

        assert!(coordinator.highlight_query("builtin").is_some());
        assert!(coordinator.has_parser_available("builtin"));
    }

    #[test]
    fn partial_builtin_query_override_preserves_and_restores_other_kinds() {
        let coordinator = LanguageCoordinator::new();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        coordinator
            .language_registry
            .register("builtin".to_string(), language.clone());
        let builtin_highlight =
            Arc::new(tree_sitter::Query::new(&language, "(identifier) @variable").unwrap());
        let builtin_injection = Arc::new(
            tree_sitter::Query::new(&language, "(string_literal) @injection.content").unwrap(),
        );
        coordinator
            .query_store
            .insert_highlight_query("builtin".to_string(), Arc::clone(&builtin_highlight));
        coordinator
            .query_store
            .insert_injection_query("builtin".to_string(), Arc::clone(&builtin_injection));

        let dir = tempdir().unwrap();
        let configured_highlight_path = dir.path().join("highlights.scm");
        fs::write(&configured_highlight_path, "(identifier) @function").unwrap();
        let configured = LanguageSettings {
            queries: Some(vec![crate::config::settings::QueryItem {
                path: configured_highlight_path.to_string_lossy().into_owned(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };
        coordinator.load_settings(&WorkspaceSettings {
            languages: HashMap::from([("builtin".to_string(), configured)]),
            ..Default::default()
        });

        assert!(Arc::ptr_eq(
            &coordinator.injection_query("builtin").unwrap(),
            &builtin_injection
        ));
        assert!(!Arc::ptr_eq(
            &coordinator.highlight_query("builtin").unwrap(),
            &builtin_highlight
        ));

        coordinator.load_settings(&WorkspaceSettings {
            languages: HashMap::from([("builtin".to_string(), LanguageSettings::default())]),
            ..Default::default()
        });
        assert!(Arc::ptr_eq(
            &coordinator.highlight_query("builtin").unwrap(),
            &builtin_highlight
        ));
        assert!(Arc::ptr_eq(
            &coordinator.injection_query("builtin").unwrap(),
            &builtin_injection
        ));

        coordinator.load_settings(&WorkspaceSettings {
            languages: HashMap::from([(
                "builtin".to_string(),
                LanguageSettings {
                    queries: Some(Vec::new()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        assert!(coordinator.highlight_query("builtin").is_none());
        assert!(coordinator.injection_query("builtin").is_none());
    }

    #[test]
    fn test_load_settings_multi_level_derived_chain() {
        // Verifies that a 3-level chain (rmd → markdown_custom → markdown)
        // loads correctly via on-demand recursive resolution, regardless of
        // HashMap iteration order.
        let coordinator = LanguageCoordinator::new();

        // Pre-register a parser under "markdown" so derived languages can find it
        // (any grammar works here — we only test load mechanics, not parsing)
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        coordinator
            .language_registry_for_parallel()
            .register("markdown".to_string(), language);

        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            crate::config::settings::LanguageSettings::default(),
        );
        languages.insert(
            "markdown_custom".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown".to_string()),
                ..Default::default()
            },
        );
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("markdown_custom".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            summary.loaded.contains(&"markdown_custom".to_string()),
            "intermediate derived 'markdown_custom' should load"
        );
        assert!(
            summary.loaded.contains(&"rmd".to_string()),
            "leaf derived 'rmd' should load"
        );
    }

    #[test]
    fn derived_registration_replaces_standalone_queries() {
        let coordinator = LanguageCoordinator::new();
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        coordinator.query_store.insert_highlight_query(
            "derived".to_string(),
            std::sync::Arc::new(
                tree_sitter::Query::new(&language, "(identifier) @variable").unwrap(),
            ),
        );

        let result = coordinator.register_derived_from_base(
            "derived",
            "base",
            &crate::config::settings::LanguageSettings {
                base: Some("base".to_string()),
                ..Default::default()
            },
            None,
            &[],
            &language,
        );

        assert!(result.success);
        assert!(
            coordinator.highlight_query("derived").is_none(),
            "query kinds absent from the new base must not survive standalone-to-derived reload"
        );
    }

    #[test]
    fn test_load_settings_derived_with_missing_base_fails() {
        // When rmd has base = "nonexistent" and no parser for it exists,
        // it should fail gracefully
        let coordinator = LanguageCoordinator::new();

        let mut languages = HashMap::new();
        languages.insert(
            "rmd".to_string(),
            crate::config::settings::LanguageSettings {
                base: Some("nonexistent".to_string()),
                ..Default::default()
            },
        );
        let settings = WorkspaceSettings {
            languages,
            ..Default::default()
        };

        let summary = coordinator.load_settings(&settings);

        assert!(
            !coordinator.has_parser_available("rmd"),
            "rmd should not have parser when base is missing"
        );
        assert!(
            !summary.loaded.contains(&"rmd".to_string()),
            "rmd should not be recorded as loaded"
        );
    }

    // Tests for truncate_preview

    #[test]
    fn test_truncate_preview_short_string() {
        let result = truncate_preview("(identifier) @variable", 60);
        assert_eq!(result, "(identifier) @variable");
    }

    #[test]
    fn test_truncate_preview_long_string() {
        let long_pattern = "a".repeat(100);
        let result = truncate_preview(&long_pattern, 20);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 20); // 17 'a's + "..."
    }

    #[test]
    fn test_truncate_preview_collapses_whitespace() {
        let result = truncate_preview("(identifier)\n    @variable", 60);
        assert_eq!(result, "(identifier) @variable");
    }

    #[test]
    fn test_truncate_preview_multibyte_characters() {
        // Pattern with multi-byte UTF-8 characters (Japanese)
        // "こんにちは世界" = 7 characters
        let pattern = "こんにちは世界";
        // Truncate to 5 characters - should include 2 chars + "..."
        let result = truncate_preview(pattern, 5);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 5); // 2 Japanese chars + "..."
        assert_eq!(result, "こん...");
    }

    #[test]
    fn test_truncate_preview_mixed_ascii_multibyte() {
        // Mix of ASCII and multi-byte characters
        // "abc日本語def" = 10 characters
        let pattern = "abc日本語def";
        let result = truncate_preview(pattern, 8);
        assert!(result.ends_with("..."));
        assert_eq!(result.chars().count(), 8); // 5 chars + "..."
        assert_eq!(result, "abc日本...");
    }

    #[test]
    fn ensure_language_loaded_fails_with_empty_search_paths() {
        let coordinator = LanguageCoordinator::new();
        // No settings loaded → search_paths is empty Vec
        let result = coordinator.ensure_language_loaded("lua");
        assert!(!result.success, "Should fail when search paths are empty");
        match &result.events[0] {
            LanguageEvent::Log { level, message } => {
                assert!(matches!(level, LanguageLogLevel::Warning));
                assert!(
                    message.contains("No search paths configured"),
                    "Expected 'No search paths configured' warning, got: {message}"
                );
            }
            other => panic!("Expected Log event, got: {other:?}"),
        }
    }

    #[test]
    fn load_single_language_with_empty_search_paths_skips_queries() {
        use crate::config::settings::LanguageSettings;

        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let grammar_dir = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        let parser_path = std::path::PathBuf::from(&grammar_dir)
            .join("parser")
            .join(format!("lua.{}", std::env::consts::DLL_EXTENSION));
        if !parser_path.exists() {
            eprintln!(
                "skipping load_single_language_with_empty_search_paths_skips_queries: parser '{}' does not exist",
                parser_path.display()
            );
            return;
        }

        let config = LanguageSettings {
            parser: Some(parser_path.to_string_lossy().into_owned()),
            queries: None,
            ..Default::default()
        };
        const NO_SEARCH_PATHS: &[PathBuf] = &[];
        let result = coordinator.load_single_language("lua", &config, NO_SEARCH_PATHS);
        assert!(result.success, "Language should load with explicit parser");
        assert!(
            coordinator.has_parser_available("lua"),
            "Parser should be registered"
        );
        assert!(
            !coordinator.has_queries("lua"),
            "No queries should be loaded when search_paths is empty and no queries field"
        );
    }

    #[test]
    fn dynamic_lua_load_from_search_paths() {
        let coordinator = LanguageCoordinator::new();

        let cwd = std::env::current_dir().expect("cwd");
        let search_path = std::env::var("TREE_SITTER_GRAMMARS")
            .unwrap_or_else(|_| cwd.join("deps/tree-sitter").to_string_lossy().to_string());
        if !std::path::Path::new(&search_path).exists() {
            eprintln!(
                "skipping dynamic_lua_load_from_search_paths: grammar path '{}' does not exist",
                search_path
            );
            return;
        }

        let settings = WorkspaceSettings {
            search_paths: vec![search_path.clone()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        assert!(
            !coordinator.config_store.search_paths().is_empty(),
            "Search paths should be set after load_settings"
        );

        let result = coordinator.ensure_language_loaded("lua");

        assert!(
            result.success,
            "Lua should load successfully from {}",
            search_path
        );

        assert!(
            coordinator.has_parser_available("lua"),
            "Lua should be registered in language registry"
        );
        assert!(
            coordinator.has_queries("lua"),
            "Lua should have highlight queries"
        );
    }
}
