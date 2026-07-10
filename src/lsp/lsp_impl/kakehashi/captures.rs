//! `kakehashi/captures/{full, full/delta, range}` — run a server-owned query
//! kind over a document (captures-protocol).
//!
//! The triple mirrors `textDocument/semanticTokens`: `full` computes every
//! match and hands out a `resultId`; `full/delta` answers a repeat request
//! with a positional single-edit diff over the matches array (or a full
//! result when the `previousResultId` is stale); `range` scopes the query
//! walk to a viewport.
//!
//! The query itself is not sent by the client: `kind` names a per-language
//! asset resolved as `queries/<lang>/<kind>.scm` across the configured
//! `searchPaths`, through the same loader (`; inherits:`, tolerant
//! per-pattern compilation) used for highlights. Kinds are therefore
//! open-ended — defined by what files exist, not by an enum.
//!
//! `#set!` directives in the kind file ride along as `metadata` objects —
//! match-level for `(#set! key value)`, on the capture entry for
//! `(#set! @cap key value)` — following Neovim's treesitter-directive-set!
//! scoping (captures-protocol §"Result shapes").
//!
//! With `injection: true` the kind query runs across **every** layer — the
//! host, then each injection region in document-order DFS — each layer
//! resolving its own language's kind file, with result nodes minted in their
//! layer's depth so they compose with `kakehashi/node/*` under the per-layer
//! Scope rule. Every match carries the producing layer's `language`. Deltas
//! carry no `injection` parameter: the mode is **lineage state**, inherited
//! from the most recent `full` for that `(uri, kind)`.
//!
//! Null vs. error (captures-protocol §"Null vs. error semantics"):
//! - JSON `null` for an unresolvable document, a kind with no query file for
//!   any visited language, a query asset that compiles to nothing (warned —
//!   that is a configuration problem, mirroring broken highlight queries),
//!   or a delta whose `(uri, kind)` has no lineage (the inherited injection
//!   mode is unknowable, so a full fallback could silently serve the wrong
//!   layer set).
//! - JSON-RPC `InvalidParams` for a malformed `kind` (fails the character
//!   whitelist guarding the filesystem lookup).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::{Error as JsonRpcError, Result};
use tower_lsp_server::ls_types::{Range, TextDocumentIdentifier, Uri};
use url::Url;

use crate::analysis::next_result_id;
use crate::language::query_exec::execute_query;
use crate::language::query_loader::QueryLoader;
use crate::language::registry::LanguageRegistry;
use crate::lsp::lsp_impl::kakehashi::node::injection_stack::walk_document_layers;
use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};
use crate::text::PositionMapper;

/// Request parameters for `kakehashi/captures/full`.
///
/// There is deliberately no result cap: a truncated `full` would silently
/// poison the delta lineage (edits computed over a clipped array), and the
/// scoping tool for "too much data" is `kakehashi/captures/range` —
/// matching semanticTokens, which has no limit parameter either
/// (captures-protocol §"Considered Options").
///
/// `pub` because the handlers are registered as custom LSP methods in the
/// `kakehashi` binary (see `src/bin/main.rs`), outside the library crate.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturesFullParams {
    pub text_document: TextDocumentIdentifier,
    /// Query kind, resolved as `queries/<lang>/<kind>.scm` on the search paths.
    pub kind: String,
    /// `true` runs the kind query across every injection layer; absent/`false`
    /// stays on the host layer. A plain boolean — unlike `kakehashi/node`,
    /// captures have no cursor position to anchor a layer stack, so there is
    /// nothing for an integer index to select.
    #[serde(default)]
    pub injection: bool,
}

/// Request parameters for `kakehashi/captures/full/delta`.
///
/// No `injection` field: the mode is inherited from the `(uri, kind)` lineage
/// established by `full` (captures-protocol §"Delta semantics").
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturesDeltaParams {
    pub text_document: TextDocumentIdentifier,
    pub kind: String,
    /// The `resultId` from a prior `full` (or delta) response for the same
    /// `(textDocument, kind)`. A stale id gets a full result back; a
    /// `(uri, kind)` with no lineage at all gets `null`.
    pub previous_result_id: String,
}

/// Request parameters for `kakehashi/captures/range`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturesRangeParams {
    pub text_document: TextDocumentIdentifier,
    pub kind: String,
    /// LSP range (UTF-16 positions) scoping the query walk; matches whose
    /// nodes intersect the range are returned.
    pub range: Range,
    /// As on `full`; with `injection: true`, layers not intersecting the
    /// range are pruned without being parsed.
    #[serde(default)]
    pub injection: bool,
}

/// Whitelist for `kind`: it names a file under `queries/<lang>/`, so anything
/// beyond `[A-Za-z0-9_-]` (path separators, dots) is rejected before the
/// filesystem is touched.
fn is_valid_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Positional single-edit diff over two match arrays, mirroring the semantic
/// tokens delta algorithm (`calculate_semantic_tokens_delta`): drop the common
/// prefix and suffix, replace the middle. Returns `None` when the arrays are
/// identical (→ empty `edits`).
///
/// Unlike token deltas there is no line-count safety guard: matches carry
/// absolute ranges, so equal JSON means an identical match regardless of what
/// changed elsewhere in the document.
fn matches_delta_edit(previous: &[Value], current: &[Value]) -> Option<(usize, usize, Vec<Value>)> {
    let common_prefix = previous
        .iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count();
    if common_prefix == previous.len() && common_prefix == current.len() {
        return None;
    }

    let prev_rest = &previous[common_prefix..];
    let curr_rest = &current[common_prefix..];
    let common_suffix = prev_rest
        .iter()
        .rev()
        .zip(curr_rest.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let delete_count = prev_rest.len() - common_suffix;
    let data = curr_rest[..curr_rest.len() - common_suffix].to_vec();
    Some((common_prefix, delete_count, data))
}

/// Shape `#set!` metadata pairs as a JSON object, or `None` when the pattern
/// set none (so plain patterns carry no `metadata` field at all).
///
/// A valued key maps to its string; the bare flag form `(#set! key)` maps to
/// `true` (a flag a client can test for, unlike Neovim's nil no-op). Duplicate
/// keys are last-write-wins, matching Neovim's in-order directive application.
fn metadata_object(pairs: &[(String, Option<String>)]) -> Option<Value> {
    if pairs.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (key, value) in pairs {
        map.insert(
            key.clone(),
            value
                .as_ref()
                .map_or(Value::Bool(true), |v| Value::String(v.clone())),
        );
    }
    Some(Value::Object(map))
}

/// Shape an LSP `Position` as its wire object by direct construction —
/// `{"line": .., "character": ..}` — avoiding the serde round-trip a `json!`
/// expression embed would pay per capture endpoint on the walk hot path.
fn position_value(position: tower_lsp_server::ls_types::Position) -> Value {
    let mut map = serde_json::Map::with_capacity(2);
    map.insert("line".to_owned(), Value::from(position.line));
    map.insert("character".to_owned(), Value::from(position.character));
    Value::Object(map)
}

/// A memoized full-mode captures walk result, tagged with the exact inputs
/// it was computed from: the snapshot identity `(parsed_version,
/// incarnation)` and the settings `generation`. Named fields (three adjacent
/// `u64`s as a tuple invited a silent swap).
pub(in crate::lsp::lsp_impl) struct CachedCapturesWalk {
    pub(in crate::lsp::lsp_impl) parsed_version: u64,
    pub(in crate::lsp::lsp_impl) incarnation: u64,
    pub(in crate::lsp::lsp_impl) generation: u64,
    /// `Some((matches, skipped))` — `Arc`'d: a memo hit and the lineage store
    /// share the array by refcount. A deep clone of a large document's
    /// matches (19.6k `Value`s on the profiling corpus) taxed every repeat
    /// request twice — once out of this memo, once into the lineage slot.
    ///
    /// `None` is a NEGATIVE memo: the walk completed and no visited language
    /// had a kind file (protocol `null`). That outcome is deterministic for
    /// the tag (same snapshot, same generation — a kind file can only appear
    /// via a settings reload or post-install, both of which bump the
    /// generation), so without it every parked single-flight loser re-elected
    /// itself winner and re-ran the identical doomed walk SEQUENTIALLY, and
    /// each client `null`-retry re-walked from scratch.
    pub(in crate::lsp::lsp_impl) result: Option<WalkArrays>,
}

/// The shared `(matches, skipped)` arrays of one completed walk.
pub(in crate::lsp::lsp_impl) type WalkArrays =
    (std::sync::Arc<Vec<Value>>, std::sync::Arc<Vec<Value>>);

/// Identity and freshness of one full-mode captures walk. Field order is the
/// supersession order: a reopened document beats every prior lifetime, a
/// settings generation beats prior query assets, then a newer parse snapshot
/// beats an older one within that generation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(in crate::lsp::lsp_impl) struct CapturesWalkTag {
    incarnation: u64,
    generation: u64,
    parsed_version: u64,
}

/// Shared state for the winner and same-snapshot losers of one captures walk.
pub(in crate::lsp::lsp_impl) struct CapturesWalkFlight {
    tag: CapturesWalkTag,
    notify: std::sync::Arc<tokio::sync::Notify>,
    cancel: crate::cancel::CancelToken,
}

impl CapturesWalkFlight {
    fn new(tag: CapturesWalkTag, cancel: crate::cancel::CancelToken) -> Self {
        Self {
            tag,
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            cancel,
        }
    }

    pub(in crate::lsp::lsp_impl) fn cancel_and_notify(&self) {
        self.cancel.cancel();
        self.notify.notify_waiters();
    }
}

enum WalkFlightClaim {
    Winner(std::sync::Arc<CapturesWalkFlight>),
    Join(std::sync::Arc<CapturesWalkFlight>),
    Obsolete,
}

fn claim_walk_flight(
    map: &dashmap::DashMap<(Url, String, bool), std::sync::Arc<CapturesWalkFlight>>,
    key: &(Url, String, bool),
    tag: CapturesWalkTag,
    cancel: crate::cancel::CancelToken,
) -> WalkFlightClaim {
    // Avoid cloning the owned key on the same-snapshot loser path.
    if let Some(current) = map.get(key) {
        match tag.cmp(&current.tag) {
            std::cmp::Ordering::Equal => {
                return WalkFlightClaim::Join(std::sync::Arc::clone(&current));
            }
            std::cmp::Ordering::Less => return WalkFlightClaim::Obsolete,
            std::cmp::Ordering::Greater => {}
        }
    }

    match map.entry(key.clone()) {
        dashmap::mapref::entry::Entry::Vacant(vacant) => {
            let flight = std::sync::Arc::new(CapturesWalkFlight::new(tag, cancel));
            vacant.insert(std::sync::Arc::clone(&flight));
            WalkFlightClaim::Winner(flight)
        }
        dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
            match tag.cmp(&occupied.get().tag) {
                std::cmp::Ordering::Equal => {
                    WalkFlightClaim::Join(std::sync::Arc::clone(occupied.get()))
                }
                std::cmp::Ordering::Less => WalkFlightClaim::Obsolete,
                std::cmp::Ordering::Greater => {
                    let previous = std::sync::Arc::clone(occupied.get());
                    let flight = std::sync::Arc::new(CapturesWalkFlight::new(tag, cancel));
                    occupied.insert(std::sync::Arc::clone(&flight));
                    previous.cancel_and_notify();
                    WalkFlightClaim::Winner(flight)
                }
            }
        }
    }
}

/// Winner-side handle of the single-flight captures walk: holds the
/// in-flight marker for one `(uri, kind, injection)` and, on drop (every
/// exit path — success, cancel, panic-unwind), removes it and wakes the
/// parked losers so they re-check the walk memo or elect a new winner.
/// `remove_if` with pointer equality so a slow winner can never evict a
/// successor's marker.
struct WalkFlightGuard<'a> {
    map: &'a dashmap::DashMap<(Url, String, bool), std::sync::Arc<CapturesWalkFlight>>,
    key: (Url, String, bool),
    flight: std::sync::Arc<CapturesWalkFlight>,
}

impl Drop for WalkFlightGuard<'_> {
    fn drop(&mut self) {
        // Also covers wholesale handler-future drops: a detached compute-pool
        // unit observes the same token and stops at its next checkpoint.
        self.flight.cancel.cancel();
        self.map.remove_if(&self.key, |_, current| {
            std::sync::Arc::ptr_eq(current, &self.flight)
        });
        self.flight.notify.notify_waiters();
    }
}

/// A kind query compiled for one language, with the patterns tolerant
/// compilation dropped.
struct KindQuery {
    query: tree_sitter::Query,
    skipped: Vec<crate::language::query_loader::SkippedPattern>,
}

/// Compiled-kind-query cache, `language → kind file → (query, generation)`.
/// `load_kind_query` re-read and re-COMPILED every visited language's kind
/// file on every request — for a per-keystroke highlighter over an
/// injection-heavy document that meant re-compiling several large query
/// files per keystroke, dwarfing the walk itself. Nested maps so the hit
/// path looks up with two borrowed `&str`s (a tuple key would allocate two
/// `String`s per lookup). A hit requires the stored generation to equal the
/// caller's (the same hot-reload discipline as the highlight-query store); a
/// miss simply OVERWRITES the entry, which is also the eviction: never more
/// than one generation per (language, kind), no sweep, no
/// clear-vs-concurrent-store race, and reload churn cannot accumulate dead
/// compiled queries.
type KindQueryCache =
    dashmap::DashMap<String, dashmap::DashMap<String, (std::sync::Arc<KindQueryLoad>, u64)>>;

fn kind_query_cache() -> &'static KindQueryCache {
    static CACHE: std::sync::OnceLock<KindQueryCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(KindQueryCache::default)
}

/// Generation-cached wrapper around [`load_kind_query`].
fn load_kind_query_cached(
    registry: &LanguageRegistry,
    search_paths: &[PathBuf],
    language_id: &str,
    file_name: &str,
    generation: u64,
) -> std::sync::Arc<KindQueryLoad> {
    if let Some(by_kind) = kind_query_cache().get(language_id)
        && let Some(hit) = by_kind.get(file_name)
        && hit.1 == generation
    {
        return std::sync::Arc::clone(&hit.0);
    }
    let loaded = std::sync::Arc::new(load_kind_query(
        registry,
        search_paths,
        language_id,
        file_name,
    ));
    // Overwrite-on-miss doubles as eviction (one generation per entry). A
    // racing same-generation compute overwrites with identical data. `get`
    // first so a present language avoids the key clone `entry()` needs.
    let stored = (std::sync::Arc::clone(&loaded), generation);
    if let Some(by_kind) = kind_query_cache().get(language_id) {
        by_kind.insert(file_name.to_string(), stored);
    } else {
        kind_query_cache()
            .entry(language_id.to_string())
            .or_default()
            .insert(file_name.to_string(), stored);
    }
    loaded
}

/// Per-language outcome of resolving `queries/<lang>/<kind>.scm`.
///
/// `Broken` is distinct from `Unavailable` so a kind file that exists but
/// compiled to nothing still surfaces its `skipped` diagnostics whenever
/// another language yields a result — without counting as "available" for
/// the overall null decision (captures-protocol §"Null vs. error semantics").
enum KindQueryLoad {
    /// Compiled; this layer executes the query.
    Loaded(KindQuery),
    /// The file exists but produced no usable patterns; carries the dropped
    /// patterns for `skipped` reporting. Contributes no matches.
    Broken(Vec<crate::language::query_loader::SkippedPattern>),
    /// No grammar, no kind file, or the file could not be read.
    Unavailable,
}

/// Resolve and compile `queries/<language_id>/<file_name>` for one language.
///
/// Logging separates the expected from the troubling: a missing file is the
/// common "no such kind for this language" case (debug); a file that exists
/// but fails to load or compiles to nothing is asset trouble (warn) —
/// captures-protocol §"Null vs. error semantics".
fn load_kind_query(
    registry: &LanguageRegistry,
    search_paths: &[PathBuf],
    language_id: &str,
    file_name: &str,
) -> KindQueryLoad {
    let Some(language) = registry.get(language_id) else {
        return KindQueryLoad::Unavailable;
    };
    let parsed = match QueryLoader::load_query_with_inheritance(
        &language,
        search_paths,
        language_id,
        file_name,
    ) {
        Ok(parsed) => parsed,
        Err(err) => {
            if QueryLoader::find_query_file(search_paths, language_id, file_name).is_none() {
                log::debug!(
                    target: "kakehashi::captures",
                    "no {file_name} for {language_id}: {err}"
                );
            } else {
                log::warn!(
                    target: "kakehashi::captures",
                    "failed to load {file_name} for {language_id}: {err}"
                );
            }
            return KindQueryLoad::Unavailable;
        }
    };
    let Some(query) = parsed.query else {
        log::warn!(
            target: "kakehashi::captures",
            "{file_name} for {language_id} compiled to nothing: {:?}",
            parsed.failure_reason
        );
        return KindQueryLoad::Broken(parsed.skipped);
    };
    KindQueryLoad::Loaded(KindQuery {
        query,
        skipped: parsed.skipped,
    })
}

/// Outcome of the shared resolution + execution pipeline: the resolved `Url`
/// (cache key half), the document's open incarnation at snapshot time (so the
/// lineage store can detect a close-then-reopen racing the request), and the
/// wire-shaped `matches` / `skipped` arrays.
struct ComputedCaptures {
    uri: Url,
    incarnation: u64,
    /// The live `content_version` at snapshot resolution. The resolved
    /// snapshot is current at that instant (serve-current park), but an edit
    /// landing DURING the walk still voids the lineage store: the response
    /// was already outdated at delivery, and lineage must not record matches
    /// for a text the client no longer has (ADR §3).
    entry_content_version: u64,
    matches: std::sync::Arc<Vec<Value>>,
    skipped: std::sync::Arc<Vec<Value>>,
}

/// Serialize an `Arc<Vec<Value>>` field as its borrowed slice, without
/// requiring serde's `rc` feature (which would add `Serialize`/`Deserialize`
/// for every `Arc<T>`/`Rc<T>` crate-wide just for this one field shape).
fn serialize_arc_vec<S: Serializer>(
    v: &Arc<Vec<Value>>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    v.as_slice().serialize(s)
}

/// Wire shape for `kakehashi/captures/full` and the full-fallback branch of
/// `kakehashi/captures/full/delta`.
///
/// Holds `Arc` clones (a refcount bump, not a deep copy) of the memoized
/// match/skip arrays so the handler can move the originals into the lineage
/// cache after building this response. `tower-lsp-server` forces every
/// handler return type through `serde_json::to_value` before the wire write
/// (`IntoResponse for Result<R, Error>`) — that one pass is unavoidable, but
/// returning this struct as `R` instead of a `json!`-built `Value` means
/// that pass serializes straight off these borrowed slices instead of
/// re-walking an already-built `Value` tree a second time.
#[derive(Debug, Serialize)]
pub struct CapturesFullResponse {
    #[serde(rename = "resultId")]
    result_id: String,
    #[serde(serialize_with = "serialize_arc_vec")]
    matches: Arc<Vec<Value>>,
    #[serde(serialize_with = "serialize_arc_vec")]
    skipped: Arc<Vec<Value>>,
}

/// Wire shape for `kakehashi/captures/range` (no `resultId`: range results
/// have no stable lineage to diff against).
#[derive(Debug, Serialize)]
pub struct CapturesRangeResponse {
    #[serde(serialize_with = "serialize_arc_vec")]
    matches: Arc<Vec<Value>>,
    #[serde(serialize_with = "serialize_arc_vec")]
    skipped: Arc<Vec<Value>>,
}

/// A single positional edit in a `kakehashi/captures/full/delta` response:
/// replace `deleteCount` matches starting at `start` with `data`. Typed
/// (rather than `json!`-built) for the same reason as `CapturesFullResponse` —
/// `data` is already a `Vec<Value>` from `matches_delta_edit`, so this avoids
/// re-walking it into a fresh `Value` tree before the one inherent
/// `serde_json::to_value` pass tower-lsp-server performs anyway.
#[derive(Debug, Serialize)]
pub struct CapturesDeltaEdit {
    start: usize,
    #[serde(rename = "deleteCount")]
    delete_count: usize,
    data: Vec<Value>,
}

/// Wire shape for `kakehashi/captures/full/delta`: either a positional edit
/// over the previous matches, or a full result (stale `previousResultId`,
/// or lineage advanced during the await — LSP convention lets a server
/// always answer a delta request with a full one).
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum CapturesDeltaResponse {
    Edits {
        #[serde(rename = "resultId")]
        result_id: String,
        edits: Vec<CapturesDeltaEdit>,
    },
    Full(CapturesFullResponse),
}

impl Kakehashi {
    pub(in crate::lsp::lsp_impl) fn cancel_captures_walks_for_document(&self, uri: &Url) {
        self.captures_walk_inflight.retain(|key, flight| {
            if key.0 == *uri {
                flight.cancel_and_notify();
                false
            } else {
                true
            }
        });
    }

    /// Handler for `kakehashi/captures/full`.
    pub async fn kakehashi_captures_full(
        &self,
        params: CapturesFullParams,
    ) -> Result<Option<CapturesFullResponse>> {
        let computed = self
            .compute_captures(
                &params.text_document.uri,
                &params.kind,
                None,
                params.injection,
            )
            .await?;
        let Some(c) = computed else {
            return Ok(None);
        };
        let result_id = next_result_id();
        // `Arc::clone` is a refcount bump, not a deep copy — the originals
        // stay in `c` and move into the cache insert below, while this
        // response holds its own cheap handle for the eventual (single,
        // tower-lsp-server-forced) serialization pass.
        let response = CapturesFullResponse {
            result_id: result_id.clone(),
            matches: Arc::clone(&c.matches),
            skipped: Arc::clone(&c.skipped),
        };
        // A full that cannot install lineage (the text moved past the computed
        // snapshot, or a close/reopen raced) answers `null` instead: handing
        // out a `resultId` with no stored lineage would make the client's next
        // delta lie about its baseline. `null` is the protocol's re-sync
        // signal; the client re-requests and converges once the reparse lands
        // (ADR §3).
        if !self
            .store_lineage(c, params.kind, params.injection, result_id)
            .await
        {
            return Ok(None);
        }
        Ok(Some(response))
    }

    /// Store a fresh lineage in this mode's slot, unless the document closed
    /// (or closed-and-reopened) or its text moved past the computed snapshot
    /// while the request was in flight. Returns whether the lineage landed.
    ///
    /// Three guards, all under the same per-URI edit lock `didClose` and
    /// `didChange` hold around their mutations:
    /// - liveness: a close that won the lock first leaves the document gone,
    ///   so the check skips the insert; an insert that won first is cleared by
    ///   the close's cache cleanup, which runs after the removal;
    /// - incarnation: a close **followed by a fast reopen** makes the URI look
    ///   alive again, but the reopened document carries a fresh open incarnation,
    ///   so a stale request's snapshot-time incarnation no longer matches and
    ///   the insert is skipped — a reopened document starts its lineage fresh
    ///   (captures-protocol §"Delta semantics");
    /// - version: an edit that landed DURING the request (the live
    ///   `content_version` moved past its request-entry value) makes the
    ///   response outdated at delivery; storing it would record lineage for a
    ///   text the client no longer has (parse-snapshot ADR §3), so nothing is
    ///   stored and the caller answers `null`. Under serve-current the
    ///   snapshot was current at resolution, so this guard covers exactly the
    ///   resolve→delivery window.
    async fn store_lineage(
        &self,
        computed: ComputedCaptures,
        kind: String,
        injection: bool,
        result_id: String,
    ) -> bool {
        let uri = computed.uri;
        let edit_lock = self.documents.edit_lock(&uri);
        let _guard = edit_lock.lock().await;
        // A single `documents` lookup reads liveness, the current incarnation,
        // and the current content_version atomically under one shard read lock.
        let still_current = self.documents.get(&uri).is_some_and(|doc| {
            doc.incarnation() == computed.incarnation
                && doc.content_version() == computed.entry_content_version
        });
        if still_current {
            let mut entry = self.captures_cache.entry((uri, kind)).or_default();
            entry[usize::from(injection)] = Some((result_id, computed.matches));
        }
        still_current
    }

    /// Handler for `kakehashi/captures/full/delta`.
    ///
    /// `previousResultId` selects its lineage **and so its injection mode** by
    /// matching one of the per-mode slots; the current matches are then
    /// recomputed under that mode (delta saves wire bytes, not compute — same
    /// trade-off as semantic tokens) and diffed against the slot. A stale id
    /// falls back to a full result only when a single mode slot is live —
    /// with both modes live the intended mode is ambiguous, and guessing
    /// could silently serve the wrong layer set, so the answer is `null`
    /// ("re-acquire via full"), as is a delta with no lineage at all
    /// (captures-protocol §"Delta semantics").
    pub async fn kakehashi_captures_full_delta(
        &self,
        params: CapturesDeltaParams,
    ) -> Result<Option<CapturesDeltaResponse>> {
        if !is_valid_kind(&params.kind) {
            return Err(JsonRpcError::invalid_params(format!(
                "invalid capture kind {:?}: expected [A-Za-z0-9_-]+",
                params.kind
            )));
        }
        let Ok(uri) = uri_to_url(&params.text_document.uri) else {
            log::warn!(
                target: "kakehashi::captures",
                "invalid URI: {}", params.text_document.uri.as_str()
            );
            return Ok(None);
        };
        let key = (uri, params.kind.clone());

        // Resolve the mode from the id; for a stale id, fall back to the sole
        // live slot's mode when unambiguous. `matched` records whether the id
        // selected its slot (diff possible) or we are already on the
        // full-fallback path. The guard drops at the end of the statement —
        // before the await below.
        let probe = self.captures_cache.get(&key).and_then(|entry| {
            let slots = entry.value();
            if let Some(mode) = slots.iter().position(|slot| {
                slot.as_ref()
                    .is_some_and(|(id, _)| *id == params.previous_result_id)
            }) {
                return Some((mode == 1, true));
            }
            let live: Vec<usize> = slots
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| slot.is_some().then_some(i))
                .collect();
            match live.as_slice() {
                [only] => Some((*only == 1, false)),
                _ => None,
            }
        });
        let Some((injection, matched)) = probe else {
            return Ok(None);
        };

        let computed = self
            .compute_captures(&params.text_document.uri, &params.kind, None, injection)
            .await?;
        let Some(c) = computed else {
            return Ok(None);
        };

        // The stale-id fallback was justified by exactly one live mode slot —
        // but that was observed before the await, and another client may have
        // opened the other mode's lineage since. Revalidate: if the mode is
        // no longer unambiguously the one we computed under, answer `null`
        // (and store nothing) rather than serve a guessed layer set.
        if !matched {
            let still_single = self.captures_cache.get(&key).is_some_and(|entry| {
                let live: Vec<usize> = entry
                    .value()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, slot)| slot.is_some().then_some(i))
                    .collect();
                matches!(live.as_slice(), [only] if *only == usize::from(injection))
            });
            if !still_single {
                return Ok(None);
            }
        }

        let result_id = next_result_id();
        // Diff against the cached slot while borrowing it in place — cloning
        // the previous matches would tax every delta request, the hot repeat
        // path. The read guard (the match scrutinee temporary) drops at the
        // end of this statement. The slot is re-verified against
        // `previousResultId` because another request for the same mode may
        // have advanced the lineage during the await above; if it did, fall
        // back to a full result. The full-fallback branch `Arc::clone`s (not
        // deep-copies) `c.matches`/`c.skipped`, mirroring `kakehashi_captures_full`.
        let slot_index = usize::from(injection);
        let response = match self.captures_cache.get(&key) {
            Some(entry)
                if matched
                    && entry.value()[slot_index]
                        .as_ref()
                        .is_some_and(|(id, _)| *id == params.previous_result_id) =>
            {
                let (_, prev_matches) = entry.value()[slot_index]
                    .as_ref()
                    .expect("slot verified Some by the guard above");
                let edits = match matches_delta_edit(prev_matches, &c.matches[..]) {
                    None => Vec::new(),
                    Some((start, delete_count, data)) => vec![CapturesDeltaEdit {
                        start,
                        delete_count,
                        data,
                    }],
                };
                CapturesDeltaResponse::Edits {
                    result_id: result_id.clone(),
                    edits,
                }
            }
            // Stale id (or lineage advanced during the await): full result
            // under the resolved mode (LSP convention — a server may always
            // answer a delta with a full).
            _ => CapturesDeltaResponse::Full(CapturesFullResponse {
                result_id: result_id.clone(),
                matches: Arc::clone(&c.matches),
                skipped: Arc::clone(&c.skipped),
            }),
        };
        // Same gate as `full`: a delta whose fresh lineage cannot install
        // (text moved past the computed snapshot, or close/reopen) answers
        // `null` — the client re-acquires via `full` (its baseline slot may
        // have been superseded by the failed rotation's circumstances anyway).
        if !self.store_lineage(c, key.1, injection, result_id).await {
            return Ok(None);
        }
        Ok(Some(response))
    }

    /// Handler for `kakehashi/captures/range`.
    ///
    /// No `resultId` — range results depend on the viewport, so there is no
    /// stable lineage to diff against (matching semanticTokens/range, which
    /// likewise has no delta variant).
    pub async fn kakehashi_captures_range(
        &self,
        params: CapturesRangeParams,
    ) -> Result<Option<CapturesRangeResponse>> {
        let computed = self
            .compute_captures(
                &params.text_document.uri,
                &params.kind,
                Some(params.range),
                params.injection,
            )
            .await?;
        Ok(computed.map(|c| CapturesRangeResponse {
            matches: Arc::clone(&c.matches),
            skipped: Arc::clone(&c.skipped),
        }))
    }

    /// Shared pipeline: validate `kind`, resolve the document, load + compile
    /// `queries/<lang>/<kind>.scm` per visited layer language, execute, and
    /// shape the wire JSON (matches tagged with their layer's `language` and
    /// minted in their layer's depth).
    ///
    /// `Err` only for a malformed `kind` (client bug); every "not currently
    /// resolvable" case — unknown document, no visited language with a kind
    /// file, assets compiling to nothing — is `Ok(None)` → JSON `null`.
    async fn compute_captures(
        &self,
        lsp_uri: &Uri,
        kind: &str,
        lsp_range: Option<Range>,
        injection: bool,
    ) -> Result<Option<ComputedCaptures>> {
        if !is_valid_kind(kind) {
            return Err(JsonRpcError::invalid_params(format!(
                "invalid capture kind {kind:?}: expected [A-Za-z0-9_-]+"
            )));
        }

        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!(target: "kakehashi::captures", "invalid URI: {}", lsp_uri.as_str());
            return Ok(None);
        };

        // A typing client cancels superseded captures aggressively (one full +
        // several deltas per keystroke); without a cancel signal each walk ran
        // to completion on a compute-pool thread. The token doubles as the
        // pool's dequeue hook (a cancelled work-unit is skipped before it ever
        // takes a thread) and as the walk's per-layer checkpoint. Two accepted
        // bounds on that granularity: (a) the layer-tree BUILD phase
        // (`layer_trees.get_or_init`) deliberately ignores cancel — its result
        // is shared by every subsequent walk on the snapshot via OnceLock, and
        // a cancelled partial build must never be cached, so it runs at most
        // once per snapshot to completion. The token is flipped by an explicit
        // client cancel, a fresher snapshot replacing this flight, didClose,
        // or the winner guard dropping with its handler future.
        let upstream_id = crate::lsp::current_upstream_id();
        let (mut cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let cancel_token = crate::cancel::CancelToken::default();

        // Serve-current (parse-snapshot ADR §3, revised like semanticTokens):
        // park — racing the client's cancel — until the latest snapshot
        // matches the live text, then walk THAT. The previous serve-stale
        // policy walked whatever snapshot was latest; during a typing burst
        // every one of those walks completed its full compute and was then
        // voided by the lineage still-current gate → `null` → the client
        // re-requested → another doomed walk. A live capture (01KWSECM…)
        // shows 41 walks in ~15s, most for snapshots several edits behind,
        // all nulled — saturating the compute pool and queueing the semantic
        // delta 10s behind them. Parking costs a watch subscription instead;
        // walks run once, on the settled snapshot, and install lineage.
        // `null` stays the protocol's re-sync signal for no-snapshot
        // (pre-first-parse), the settle backstop, and the lineage gate.
        // Deliberate asymmetry with semanticTokens: tokens get the parse
        // loop's settle refresh (server re-drive) and so may reject silently;
        // captures rely entirely on the client's null-retry — past the
        // backstop that cycles at one park per retry (~0.1 req/s per kind),
        // churn but never dark, and no server-side re-drive is needed.
        // The injected-grammar install is not triggered inline here: the
        // parse loop's downstream (`process_injections`) detects missing
        // injected grammars and spawns the install as a detached task; a
        // layer whose grammar is still installing is skipped by the walk and
        // heals on a later request.
        let snapshot = {
            use crate::lsp::lsp_impl::snapshot_read::{SnapshotWait, TOKEN_SETTLE_BACKSTOP};
            // The parked settle wait is the full/delta policy. `range` is the
            // position/range class (ADR §3): its byte range is authored
            // against the LIVE text and it fires per redraw, so it
            // staleness-rejects to `null` IMMEDIATELY (zero stale wait —
            // only the bounded first-parse wait inside applies) instead of
            // holding an ingress slot for up to the backstop.
            let stale_wait = if lsp_range.is_some() {
                std::time::Duration::ZERO
            } else {
                TOKEN_SETTLE_BACKSTOP
            };
            let wait = self.wait_for_current_snapshot(&uri, stale_wait);
            let outcome = match cancel_rx.as_mut() {
                Some(rx) => {
                    tokio::select! {
                        biased;
                        _ = rx => {
                            return Err(JsonRpcError::request_cancelled());
                        }
                        outcome = wait => outcome,
                    }
                }
                None => wait.await,
            };
            match outcome {
                SnapshotWait::Current(snapshot) => snapshot,
                SnapshotWait::Stale | SnapshotWait::Unparsed | SnapshotWait::Gone => {
                    log::debug!(
                        target: "kakehashi::captures",
                        "no current snapshot for {uri}: re-sync via null"
                    );
                    return Ok(None);
                }
            }
        };
        // The live input state at request entry: `range` staleness-rejects
        // against it, and the lineage store re-checks it (see ComputedCaptures).
        let Some(view) = self.documents.latest_snapshot(&uri) else {
            return Ok(None);
        };
        let entry_content_version = view.content_version;
        // A close+reopen between the snapshot resolve above and this read
        // leaves `snapshot` on a dead lifetime; per-lifetime versions restart
        // at 0, so the version equality below could pass by coincidence. A
        // cross-incarnation snapshot can never answer (or mint) anything —
        // `null` re-syncs the client against the new lifetime.
        if snapshot.incarnation != view.slot.current_incarnation {
            return Ok(None);
        }
        // `range` re-check against the LIVE version read above: the resolve
        // returned a then-current snapshot, but an edit can land between it
        // and this read. `null` — the captures protocol's re-sync signal —
        // not ContentModified (a JSON-RPC error would violate the captures
        // contract). Defense in depth behind the zero-stale-wait resolve.
        if lsp_range.is_some() && entry_content_version != snapshot.parsed_version {
            log::debug!(
                target: "kakehashi::captures",
                "stale snapshot for range request on {uri}: re-sync via null"
            );
            return Ok(None);
        }
        let Some(language_id) = snapshot.language.clone() else {
            log::debug!(target: "kakehashi::captures", "no host language detected for {uri}");
            return Ok(None);
        };
        let incarnation = snapshot.incarnation;
        let text = std::sync::Arc::clone(&snapshot.text);
        let Some(tree) = snapshot.tree.clone() else {
            // Resolved-but-tree-less (no parser installed / crashed grammar).
            return Ok(None);
        };

        // The query-execution walk over every layer — including the injected
        // layers' one-off re-parses — is synchronous tree-CPU; run it as one
        // compute-pool work-unit instead of inline on this tokio worker
        // (parse-snapshot ADR §4). Everything moved in is an owned snapshot
        // piece or a cheap Arc clone; `None` from the pool (work-unit panic)
        // degrades to the protocol's `null`.
        // Settings generation for the kind-query compile cache — the same
        // reload discipline the token caches use.
        let generation = self.cache.semantic_token_generation();

        // Walk-result memo (full mode only — range results depend on the
        // viewport): a typing client sends captures/full AND full/delta per
        // keystroke, both walking the SAME snapshot; the second serves this
        // memo instead of re-executing the kind query over every layer. The
        // tag must match the exact inputs: snapshot identity
        // (parsed_version, incarnation) and the settings generation.
        // Key built only on the full-mode path (range bypasses the memo), and
        // carried to the store below so the Url/String allocations happen at
        // most once per request.
        //
        // Single-flight: a typing client sends `full` AND several `delta`s
        // for the same kind concurrently, and the memo only helps AFTER the
        // first walk completes — concurrent misses all re-executed the same
        // walk (the 01KWSECM… capture shows the same (snapshot, kind) walked
        // up to 4× at once). Losers park on the winner's Notify and re-check
        // the memo; a winner that stores nothing (cancelled, pool panic)
        // wakes them to elect a new winner. A COMPLETED walk always stores —
        // a `null` outcome as a negative entry — so the no-kind-file case
        // can't convoy the losers into sequential identical walks. The 1s
        // re-loop timeout is a lost-wakeup backstop only — correctness never
        // depends on it. A request for a fresher snapshot atomically replaces
        // the marker and cancels the old winner; same-snapshot full/delta
        // requests still join it, while an older request that reaches the map
        // after the replacement answers RequestCancelled instead of reviving
        // obsolete compute.
        let mut flight_guard = None;
        let walk_key = if lsp_range.is_none() {
            let key = (uri.clone(), kind.to_string(), injection);
            let walk_tag = CapturesWalkTag {
                incarnation,
                generation,
                parsed_version: snapshot.parsed_version,
            };
            loop {
                // Memo lookup and flight election share didChange/didClose's
                // per-document edit lock. Without this gate, a delayed loser
                // could wake after its fresher successor removed the marker,
                // reclaim the vacant flight with an old tag, and resurrect an
                // obsolete memo after close or edit.
                let edit_lock = self.documents.edit_lock(&uri);
                let edit_guard = edit_lock.lock().await;
                let current = self.cache.semantic_token_generation() == generation
                    && self.documents.latest_snapshot(&uri).is_some_and(|view| {
                        view.slot.current_incarnation == incarnation
                            && view.content_version == snapshot.parsed_version
                    });
                if !current {
                    drop(edit_guard);
                    self.documents
                        .remove_edit_lock_if_unshared(&uri, &edit_lock);
                    return Ok(None);
                }
                if let Some(hit) = self.captures_walk_cache.get(&key)
                    && hit.parsed_version == snapshot.parsed_version
                    && hit.incarnation == incarnation
                    && hit.generation == generation
                {
                    // A negative entry (`None`) serves the protocol `null`
                    // without a walk — no kind file for this snapshot +
                    // generation, see `CachedCapturesWalk::result`.
                    return Ok(hit
                        .result
                        .clone()
                        .map(|(matches, skipped)| ComputedCaptures {
                            uri,
                            incarnation,
                            entry_content_version,
                            matches,
                            skipped,
                        }));
                }
                let claim = claim_walk_flight(
                    &self.captures_walk_inflight,
                    &key,
                    walk_tag,
                    cancel_token.clone(),
                );
                let flight = match claim {
                    WalkFlightClaim::Winner(flight) => {
                        flight_guard = Some(WalkFlightGuard {
                            map: &self.captures_walk_inflight,
                            key: key.clone(),
                            flight,
                        });
                        // A prior winner can store its memo and remove its
                        // marker between the loop's memo read and our claim.
                        // Re-check after claiming so this successor does not
                        // repeat an already-completed identical walk.
                        if let Some(hit) = self.captures_walk_cache.get(&key)
                            && hit.parsed_version == snapshot.parsed_version
                            && hit.incarnation == incarnation
                            && hit.generation == generation
                        {
                            let result = hit.result.clone();
                            drop(hit);
                            return Ok(result.map(|(matches, skipped)| ComputedCaptures {
                                uri,
                                incarnation,
                                entry_content_version,
                                matches,
                                skipped,
                            }));
                        }
                        drop(edit_guard);
                        break;
                    }
                    WalkFlightClaim::Join(flight) => {
                        drop(edit_guard);
                        flight
                    }
                    WalkFlightClaim::Obsolete => {
                        drop(edit_guard);
                        return Err(JsonRpcError::request_cancelled());
                    }
                };
                // Register interest BEFORE re-validating the marker: enable()
                // makes a notify_waiters() between here and the await visible,
                // and the marker re-check catches a winner that exited before
                // we registered (its notify_waiters preceded our enable).
                let notified = flight.notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let winner_live = self
                    .captures_walk_inflight
                    .get(&key)
                    .is_some_and(|current| std::sync::Arc::ptr_eq(&*current, &flight));
                if winner_live {
                    match cancel_rx.as_mut() {
                        Some(rx) => {
                            tokio::select! {
                                biased;
                                _ = rx => return Err(JsonRpcError::request_cancelled()),
                                _ = &mut notified => {}
                                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                            }
                        }
                        None => {
                            tokio::select! {
                                _ = &mut notified => {}
                                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                            }
                        }
                    }
                }
                if flight.cancel.is_cancelled() {
                    return Ok(None);
                }
            }
            Some(key)
        } else {
            None
        };
        // Holds the in-flight marker for the winner until this function
        // returns (its Drop removes the marker and wakes the losers, AFTER
        // the memo store below on the success path).
        let _flight_guard = flight_guard;

        let uri_for_walk = uri.clone();
        let kind = kind.to_string();
        let language = std::sync::Arc::clone(&self.language);
        let tracker = self.bridge.node_tracker_arc();
        let documents = std::sync::Arc::clone(&self.documents);
        let match_cache = std::sync::Arc::clone(&self.captures_match_cache);
        let parsed_version = snapshot.parsed_version;
        let snapshot_for_layers = std::sync::Arc::clone(&snapshot);
        let walk_cancel = cancel_token.clone();
        let inner_cancel = cancel_token.clone();
        // Mint latch + currency gate, taken atomically UNDER the document's
        // edit lock (populate's discipline, see
        // `populate_injections_on_pool`): `did_change` shifts the tracker
        // BEFORE it bumps `content_version` under this same lock, so a
        // lock-free latch + version check could observe the intermediate
        // state — latch matching the already-shifted tracker, version not
        // yet bumped — and let the walk's per-layer batches mint
        // old-snapshot coordinates as correct-at-birth. Under the lock the
        // pair is atomic; an edit (or close) landing after the lock drops
        // fails the per-layer batch latch instead, degrading that layer to
        // the read-only fallback. Taken before the pool dispatch: an edit
        // during the queue wait now refuses the batches rather than
        // flipping the gate, the same read-only outcome; the sweep it
        // permits stays bounded by the single-flight winner guard (see the
        // sweep note in `execute_captures_walk`).
        let (entry_mint_epoch, mint_into_tracker) = {
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let latch = tracker.mint_epoch(&uri);
            let latest = self.documents.latest_snapshot(&uri);
            let current = latest.as_ref().is_some_and(|view| {
                view.slot.current_incarnation == incarnation
                    && view.content_version == parsed_version
            });
            if latest.is_none() {
                // Reclaim the lock entry edit_lock() materialized for a
                // closed document (identity+share-checked — see
                // `remove_edit_lock_if_unshared`).
                self.documents
                    .remove_edit_lock_if_unshared(&uri, &edit_lock);
            }
            (latch, current)
        };
        let walk_future = self
            .compute_pool
            .run(Some(cancel_token.clone()), move || {
                // Lazily build the snapshot's layer trees on first use (this
                // is the same walk the inline path would run) and share them
                // across every subsequent walk on this snapshot — captures
                // full + delta both walk per keystroke. Concurrent walkers on
                // one snapshot block on the OnceLock and reuse the winner's.
                let build_start = std::time::Instant::now();
                let mut layers_were_cached = true;
                let fresh_layers;
                let layers = if injection {
                    let cached = snapshot_for_layers.layer_trees.get_or_init(|| {
                        layers_were_cached = false;
                        (
                            generation,
                            std::sync::Arc::new(
                                crate::lsp::lsp_impl::kakehashi::node::injection_stack::collect_document_layer_trees(
                                    &language,
                                    &language_id,
                                    &text,
                                    &tree,
                                ),
                            ),
                        )
                    });
                    // Settings-generation gate (the DiscoveredInjections
                    // pattern): an injected-grammar auto-install bumps the
                    // generation without a new snapshot, so cached trees
                    // would keep an embedded layer empty long after its
                    // parser landed. On mismatch walk fresh, uncached — the
                    // pre-cache per-request cost — until the next snapshot.
                    if cached.0 == generation {
                        Some(cached.1.as_slice())
                    } else {
                        fresh_layers =
                            crate::lsp::lsp_impl::kakehashi::node::injection_stack::collect_document_layer_trees(
                                &language,
                                &language_id,
                                &text,
                                &tree,
                            );
                        Some(fresh_layers.as_slice())
                    }
                } else {
                    None
                };
                let build_elapsed = build_start.elapsed();
                let walk_start = std::time::Instant::now();
                let walked = execute_captures_walk(
                    &uri_for_walk,
                    &kind,
                    lsp_range,
                    injection,
                    &language_id,
                    &text,
                    &tree,
                    layers,
                    &language,
                    &tracker,
                    &documents,
                    entry_mint_epoch,
                    mint_into_tracker,
                    generation,
                    &match_cache,
                    Some(&inner_cancel),
                );
                log::debug!(
                    target: "kakehashi::captures",
                    "walk {uri_for_walk} kind={kind} v{parsed_version}: layers {}={}ms walk={}ms matches={}",
                    if layers_were_cached { "cached" } else { "built" },
                    build_elapsed.as_millis(),
                    walk_start.elapsed().as_millis(),
                    walked.as_ref().map_or(0, |(m, _)| m.len()),
                );
                walked
            });
        let walked = if let Some(cancel_rx) = cancel_rx {
            tokio::pin!(cancel_rx);
            let superseded = walk_cancel.cancelled();
            tokio::pin!(superseded);
            tokio::select! {
                biased;
                // $/cancelRequest while queued/walking: flip the token so a
                // queued unit is skipped at dequeue and a running walk bails
                // at its next per-layer checkpoint, then answer the protocol
                // cancellation instead of `null` (which would make the client
                // immediately re-request).
                _ = &mut cancel_rx => {
                    walk_cancel.cancel();
                    return Err(JsonRpcError::request_cancelled());
                }
                _ = &mut superseded => {
                    return Err(JsonRpcError::request_cancelled());
                }
                walked = walk_future => walked,
            }
        } else {
            tokio::select! {
                _ = walk_cancel.cancelled() => {
                    return Err(JsonRpcError::request_cancelled());
                }
                walked = walk_future => walked,
            }
        };
        let Some(walked) = walked else {
            // A pool-skip of an already-cancelled unit (or a work-unit panic).
            if walk_cancel.is_cancelled() {
                return Err(JsonRpcError::request_cancelled());
            }
            return Ok(None);
        };
        // A completed walk can race a fresher flight or didClose after its last
        // internal checkpoint. Never cache or serve that obsolete result.
        if walk_cancel.is_cancelled() {
            return Err(JsonRpcError::request_cancelled());
        }
        // Arc once; the memo and the returned ComputedCaptures share the
        // arrays by refcount instead of deep-cloning ~20k `Value`s. A `None`
        // walk (no kind file) is memoized too — the negative entry, see
        // `CachedCapturesWalk::result`.
        let result = walked
            .map(|(matches, skipped)| (std::sync::Arc::new(matches), std::sync::Arc::new(skipped)));
        if let Some(key) = walk_key {
            // Serialize the final currency check and memo installation with
            // didChange/didClose. This prevents a close from clearing the memo
            // between our check and insert, then having this old winner recreate
            // state for the dead document lifetime.
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let current = self.cache.semantic_token_generation() == generation
                && self.documents.latest_snapshot(&uri).is_some_and(|view| {
                    view.slot.current_incarnation == incarnation
                        && view.content_version == snapshot.parsed_version
                });
            if !current {
                return Ok(None);
            }
            self.captures_walk_cache.insert(
                key,
                CachedCapturesWalk {
                    parsed_version: snapshot.parsed_version,
                    incarnation,
                    generation,
                    result: result.clone(),
                },
            );
        }
        Ok(result.map(|(matches, skipped)| ComputedCaptures {
            uri,
            incarnation,
            entry_content_version,
            matches,
            skipped,
        }))
    }
}

/// Synchronous half of the captures pipeline: load + compile the kind query
/// per visited layer language, execute it over each layer, and shape the wire
/// JSON. Runs as a compute-pool work-unit (never on a tokio worker).
///
/// Returns `None` when no visited language compiled a kind file (→ `null` on
/// the wire), `Some((matches, skipped))` otherwise.
#[allow(clippy::too_many_arguments)]
fn execute_captures_walk(
    uri: &Url,
    kind: &str,
    lsp_range: Option<Range>,
    injection: bool,
    language_id: &str,
    text: &str,
    tree: &tree_sitter::Tree,
    layer_trees: Option<&[crate::document::SnapshotLayerTree]>,
    language: &crate::language::LanguageCoordinator,
    tracker: &crate::language::NodeTracker,
    documents: &crate::document::DocumentStore,
    entry_mint_epoch: (u64, u64),
    mint_into_tracker: bool,
    generation: u64,
    match_cache: &super::captures_match_cache::CapturesMatchCache,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Option<(Vec<Value>, Vec<Value>)> {
    // Tracker minting is currency-gated: the NodeTracker is a LIVE-coordinate
    // index (didChange shifts every entry before the reparse), so a trailing
    // snapshot's byte offsets must never be written into it — they would be
    // wrong-space entries that later edits keep shifting as if they were live
    // (the "a stale read never mints" invariant, see snapshot_read.rs).
    // `entry_mint_epoch` + `mint_into_tracker` are captured by the CALLER,
    // atomically under the document's edit lock (see `compute_captures`) —
    // `did_change` shifts the tracker BEFORE it bumps `content_version`
    // under that lock, so computing the pair lock-free here could observe
    // the intermediate state and mint old-snapshot coordinates as
    // correct-at-birth. A stale-at-entry serve (`mint_into_tracker` false)
    // goes READ-ONLY on the tracker: a position the intervening edits did
    // not shift reuses its live id (so the delta lineage stays a minimal
    // diff), and an unknown position gets an unregistered id — resolving
    // one via `kakehashi/node/*` yields `null`, the protocol's re-sync
    // signal, and the client's next current-snapshot request mints real
    // ones. An edit landing AFTER the caller's gate — during the pool-queue
    // wait or the walk itself — fails the per-layer batch latch instead
    // (`mint_batch_if_unshifted` re-checks it under the tracker entry's
    // exclusive lock; the epoch half covers close/reopen, whose per-URI
    // generation restarts at 0), degrading those layers to the same
    // read-only resolution. Either way no wrong-space mint EXISTS, ever.
    let mapper = PositionMapper::new(text);

    // Range scoping: clamped conversion (like other viewport-shaped
    // requests) — an out-of-bounds position means "to the document edge",
    // not an error. An inverted range is normalized to [min, max], the
    // same forgiveness range_formatting applies, rather than collapsing
    // to null (which the protocol reserves for unresolvable states).
    let byte_range = lsp_range.map(|r| {
        let a = mapper.position_to_byte_clamped(r.start);
        let b = mapper.position_to_byte_clamped(r.end);
        a.min(b)..a.max(b)
    });

    let registry = language.language_registry_for_parallel();
    let search_paths = language.search_paths();
    let file_name = format!("{kind}.scm");

    // One kind-query load per language per request — served from the
    // process-wide generation-keyed compile cache, so across requests each
    // (language, kind) compiles once per settings generation instead of once
    // per keystroke. The per-request map still yields each language's
    // `skipped` exactly once.
    let mut kind_queries: HashMap<String, std::sync::Arc<KindQueryLoad>> = HashMap::new();
    let mut matches: Vec<Value> = Vec::new();

    // Cross-snapshot match reuse is a full-walk concern only: a range walk
    // produces results clipped to the viewport, which must neither be
    // served as a layer's full matches nor stored as them.
    let cache_full_walk = byte_range.is_none();
    // Layer-entry hashes this walk read or wrote — the live set for the
    // post-walk sweep (host slots self-replace and need no sweep).
    let mut touched_layer_hashes: HashSet<u64> = HashSet::new();
    // Hit/miss counters for the walk log line (cache-behavior diagnostics).
    let (mut layers_reused, mut layers_executed) = (0usize, 0usize);

    let mut visit = |layer_language: &str, layer_tree: &tree_sitter::Tree, depth: usize| {
        // Per-layer cancellation checkpoint: a cancelled walk stops doing
        // query/mint work; the caller discards the (partial) result.
        if crate::cancel::is_cancelled(cancel) {
            return;
        }
        let entry = kind_queries
            .entry(layer_language.to_string())
            .or_insert_with(|| {
                load_kind_query_cached(
                    &registry,
                    &search_paths,
                    layer_language,
                    &file_name,
                    generation,
                )
            });
        let KindQueryLoad::Loaded(kind_query) = entry.as_ref() else {
            return;
        };
        // Content-addressed cross-snapshot reuse: a layer whose included
        // ranges carry the same bytes in the same relative geometry (merely
        // translated by edits elsewhere) serves its cached MatchData and
        // skips execute_query. Only the ID-free byte-offset stage is cached;
        // minting/positions/shaping below run identically on hit and miss.
        //
        // The host layer (depth 0) is DELIBERATELY included even though any
        // keystroke changes host bytes and misses: its hit case is exact
        // content recurrence (undo/redo, revert), which replays the most
        // expensive single query of the walk for free. The price is one
        // FNV pass over the layer bytes per visit — about twice the
        // document bytes per full walk, sub-ms against the walk times the
        // per-walk debug line reports (see the reused/executed counters).
        let cache_key = cache_full_walk.then(|| {
            super::captures_match_cache::tree_cache_key(kind, layer_language, layer_tree, text)
        });
        let reused = cache_key.and_then(|(_, hash)| {
            if depth == 0 {
                match_cache.get_host(uri, kind, hash, generation)
            } else {
                match_cache.get_layer(uri, hash, generation)
            }
        });
        let (layer_matches, anchor): (
            std::sync::Arc<Vec<crate::language::query_exec::MatchData>>,
            usize,
        ) = if let (Some(cached), Some((anchor, hash))) = (reused, cache_key) {
            if depth > 0 {
                touched_layer_hashes.insert(hash);
            }
            layers_reused += 1;
            (cached, anchor)
        } else {
            layers_executed += 1;
            let mut fresh = execute_query(&kind_query.query, layer_tree, text, byte_range.clone());
            // Rebase to anchor-relative before storing; a refusal (a capture
            // below the layer anchor, e.g. a root node reaching outside its
            // included ranges) leaves the matches absolute and uncached.
            let rebased_key = cache_key.filter(|&(anchor, _)| {
                super::captures_match_cache::rebase_matches(&mut fresh, anchor)
            });
            let arc = std::sync::Arc::new(fresh);
            if let Some((anchor, hash)) = rebased_key {
                // The doc-open probe only runs when the store must CREATE the
                // per-document slot (first store after open, or a walk racing
                // its own didClose) — see the resurrection-leak note on
                // `store_host`.
                let doc_open = || documents.latest_snapshot(uri).is_some();
                if depth == 0 {
                    match_cache.store_host(
                        uri,
                        kind,
                        hash,
                        generation,
                        std::sync::Arc::clone(&arc),
                        doc_open,
                    );
                } else {
                    match_cache.store_layer(
                        uri,
                        kind,
                        hash,
                        generation,
                        std::sync::Arc::clone(&arc),
                        doc_open,
                    );
                    touched_layer_hashes.insert(hash);
                }
                (arc, anchor)
            } else {
                (arc, 0)
            }
        };
        // Per-layer id reconciliation (the §3 walk lever): resolve every
        // capture's ULID in ONE tracker entry-lock acquisition instead of
        // one per capture (~20k on an injection-heavy document). Minted in
        // the layer's depth, so the id resolves in its minting layer via
        // kakehashi/node/* (per-layer Scope rule). The batch is keyed on the
        // walk's entry latch: a mid-walk edit refuses it wholesale (nothing
        // minted — no wrong-space entries, no purge), and the layer degrades
        // to the stale serve's read-only resolution below. Ids are minted
        // for every capture, including one whose span later fails position
        // mapping and is dropped from the response — under a current
        // snapshot every span maps by construction, so the difference is
        // theoretical; the alignment (one id per capture, match order) is
        // what the shaping loop indexes by.
        let capture_keys = || {
            layer_matches.iter().flat_map(|m| {
                m.captures
                    .iter()
                    .map(|c| (c.start_byte + anchor, c.end_byte + anchor, c.kind, depth))
            })
        };
        let layer_ulids: Vec<ulid::Ulid> = if mint_into_tracker {
            tracker.mint_batch_if_unshifted(uri, entry_mint_epoch, capture_keys())
        } else {
            None
        }
        .unwrap_or_else(|| {
            // Read-only resolution (stale-at-entry serve, or a mid-walk edit
            // refusing the batch): a position the intervening edits did not
            // shift reuses its live id; an unknown position gets a fresh
            // UNREGISTERED id — NOT Ulid::default() (the nil id), since
            // unregistered ids must still be unique per capture.
            capture_keys()
                .map(|(start, end, kind, layer)| {
                    match tracker.lookup_in_layer(uri, start, end, kind, layer) {
                        Some(live) => live,
                        None => ulid::Ulid::new(),
                    }
                })
                .collect()
        });
        let mut capture_idx = 0usize;
        for m in layer_matches.iter() {
            let captures: Vec<Value> = m
                .captures
                .iter()
                .filter_map(|c| {
                    // Consume this capture's reconciled id FIRST (the batch
                    // is aligned one-id-per-capture in match order), so a
                    // dropped capture cannot shift later captures' ids.
                    let ulid = layer_ulids[capture_idx];
                    capture_idx += 1;
                    // A capture whose bytes don't map to positions (corrupt
                    // span) is dropped rather than failing the whole request.
                    let start_byte = c.start_byte + anchor;
                    let end_byte = c.end_byte + anchor;
                    let start = mapper.byte_to_position(start_byte)?;
                    let end = mapper.byte_to_position(end_byte)?;
                    // Direct Map construction, MOVING every sub-value: a
                    // `json!` literal with non-literal expressions funnels
                    // them through `to_value(&expr)` — a full re-serialization
                    // (deep copy) of each nested Value plus the drop of the
                    // original, which profiling showed as ~37% of the walk on
                    // an injection-heavy document. The field order below IS
                    // the wire shape (`envelope_wire_shape_is_stable`).
                    let mut node = serde_json::Map::with_capacity(2);
                    node.insert("id".to_owned(), Value::String(ulid.to_string()));
                    node.insert("kind".to_owned(), Value::String(c.kind.to_owned()));
                    let mut range = serde_json::Map::with_capacity(2);
                    range.insert("start".to_owned(), position_value(start));
                    range.insert("end".to_owned(), position_value(end));
                    let has_meta = !c.metadata.is_empty();
                    let mut capture = serde_json::Map::with_capacity(3 + usize::from(has_meta));
                    capture.insert("name".to_owned(), Value::String(c.name.clone()));
                    capture.insert("node".to_owned(), Value::Object(node));
                    capture.insert("range".to_owned(), Value::Object(range));
                    // Capture-scoped `#set! @cap key value` metadata,
                    // only when the capture was annotated.
                    if let Some(meta) = metadata_object(&c.metadata) {
                        capture.insert("metadata".to_owned(), meta);
                    }
                    Some(Value::Object(capture))
                })
                .collect();
            // Mirror execute_query's invariant: clients never see an empty
            // match envelope, even when every capture span fails to map.
            if captures.is_empty() {
                continue;
            }
            // Shaped after the empty-captures bail so a skipped envelope
            // allocates nothing.
            let match_metadata = metadata_object(&m.metadata);
            let mut envelope =
                serde_json::Map::with_capacity(3 + usize::from(match_metadata.is_some()));
            envelope.insert("patternIndex".to_owned(), Value::from(m.pattern_index));
            envelope.insert(
                "language".to_owned(),
                Value::String(layer_language.to_owned()),
            );
            envelope.insert("captures".to_owned(), Value::Array(captures));
            // Match-level `#set!` metadata, only when the pattern set any
            // (treesitter-directive-set!) — absent otherwise, so patterns
            // without directives keep their pre-metadata wire shape.
            if let Some(meta) = match_metadata {
                envelope.insert("metadata".to_owned(), meta);
            }
            matches.push(Value::Object(envelope));
        }
        // The alignment contract the unchecked indexing above relies on:
        // `capture_keys()` and the shaping loop enumerate the same captures
        // in the same order, so the loop consumes EXACTLY the batch. A
        // future divergence (a filter on one side but not the other) trips
        // this in tests instead of panicking on an index in production.
        debug_assert_eq!(
            capture_idx,
            layer_ulids.len(),
            "capture_keys() and the shaping loop must enumerate the same captures"
        );
    };

    if injection {
        if let Some(layers) = layer_trees {
            // Pre-parsed layers from the snapshot's populate pass (the
            // layer-tree half of never-discover-twice): identical to the
            // inline walk below by construction — populate built them with
            // that walk over this same (text, tree). The span check mirrors
            // the walk's byte_filter pruning: a false-positive visit only
            // makes the layer's query yield nothing for the clipped range.
            visit(language_id, tree, 0);
            for layer in layers {
                if crate::cancel::is_cancelled(cancel) {
                    break;
                }
                if let Some(filter) = &byte_range
                    && (layer.span.end <= filter.start || layer.span.start >= filter.end)
                {
                    continue;
                }
                visit(&layer.language, &layer.tree, layer.depth);
            }
        } else {
            walk_document_layers(
                language,
                language_id,
                text,
                tree,
                byte_range.as_ref(),
                cancel,
                &mut visit,
            );
        }
    } else {
        visit(language_id, tree, 0);
    }

    // A cancelled walk must not shape a partial result: the caller answers
    // the protocol cancellation, and nothing may reach the walk memo. The
    // per-layer batch mints that ran before the bail were latch-gated
    // (correct-at-birth), so no post-walk reconciliation is needed — only
    // the response is discarded.
    if crate::cancel::is_cancelled(cancel) {
        return None;
    }

    // Match-cache sweep: after a COMPLETED full-coverage walk (all layers
    // visited: injection mode, no viewport clip, no cancel bail), the
    // touched set IS the document's live layer set for this kind — retain
    // exactly it. Two things bound eviction of CURRENT entries:
    // - the `mint_into_tracker` entry-currency gate drops the sweep of a
    //   walk that was already trailing when it started (a stale-at-entry
    //   walk never sweeps; its stores are harmless — content-addressed,
    //   they only hit if that content returns, e.g. undo);
    // - for a walk outrun DURING the walk (current at entry, edit lands
    //   mid-flight), it is the single-flight winner guard — held across
    //   walk AND sweep for this same (uri, kind, injection) key — that
    //   keeps the successor's stores out until this sweep lands, so the
    //   stale touched set can only miss entries, never evict a newer
    //   walk's. Relaxing single-flight would re-open that window (worst
    //   case: extra misses until the next current walk restores, never
    //   wrong output).
    if cache_full_walk && injection && mint_into_tracker {
        match_cache.sweep_layers(uri, kind, &touched_layer_hashes);
    }
    log::debug!(
        target: "kakehashi::captures",
        "walk {uri} kind={kind}: match-cache reused={layers_reused} executed={layers_executed}",
    );

    // The kind is "available" when at least one visited language COMPILED
    // the file — a host without a context.scm still surfaces the embedded
    // layers' contexts. A `Broken` file does not make the kind available
    // (a sole broken asset degrades to null, per the ADR), but its
    // diagnostics are surfaced below whenever another language yields a
    // result.
    if !kind_queries
        .values()
        .any(|load| matches!(load.as_ref(), KindQueryLoad::Loaded(_)))
    {
        return None;
    }

    // Sort by language so the wire order is deterministic — the memo is a
    // HashMap, whose iteration order would otherwise vary per process.
    // Within a language, the loader already reports skipped patterns in
    // file order. `Broken` languages contribute their diagnostics too:
    // a wholly-invalid kind file would otherwise be silently invisible
    // exactly when other layers still produce matches.
    let mut reportable: Vec<(&String, &[crate::language::query_loader::SkippedPattern])> =
        kind_queries
            .iter()
            .filter_map(|(lang, load)| match load.as_ref() {
                KindQueryLoad::Loaded(kq) => Some((lang, kq.skipped.as_slice())),
                KindQueryLoad::Broken(skipped) => Some((lang, skipped.as_slice())),
                KindQueryLoad::Unavailable => None,
            })
            .collect();
    reportable.sort_by(|a, b| a.0.cmp(b.0));
    let skipped: Vec<Value> = reportable
        .into_iter()
        .flat_map(|(lang, patterns)| {
            patterns.iter().map(move |s| {
                json!({
                    "language": lang,
                    "startLine": s.start_line,
                    "endLine": s.end_line,
                    "reason": s.error,
                })
            })
        })
        .collect();

    Some((matches, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(ns: &[i64]) -> Vec<Value> {
        ns.iter().map(|n| json!(n)).collect()
    }

    /// Pins the wire shape of the struct-based response against the
    /// `json!`-built shape it replaced: same keys, same values, matches/
    /// skipped still arrays (not re-wrapped or renamed) despite now being
    /// serialized off `Arc<Vec<Value>>` fields instead of a pre-built `Value`.
    #[test]
    fn captures_full_response_serializes_like_the_old_json_macro_shape() {
        let response = CapturesFullResponse {
            result_id: "r1".to_string(),
            matches: Arc::new(vals(&[1, 2])),
            skipped: Arc::new(vals(&[3])),
        };
        let expected = json!({
            "resultId": "r1",
            "matches": [1, 2],
            "skipped": [3],
        });
        assert_eq!(serde_json::to_value(&response).unwrap(), expected);
    }

    #[test]
    fn captures_range_response_serializes_like_the_old_json_macro_shape() {
        let response = CapturesRangeResponse {
            matches: Arc::new(vals(&[1])),
            skipped: Arc::new(Vec::new()),
        };
        let expected = json!({ "matches": [1], "skipped": [] });
        assert_eq!(serde_json::to_value(&response).unwrap(), expected);
    }

    #[test]
    fn captures_delta_response_edits_variant_serializes_like_the_old_json_macro_shape() {
        let response = CapturesDeltaResponse::Edits {
            result_id: "r2".to_string(),
            edits: vec![CapturesDeltaEdit {
                start: 0,
                delete_count: 1,
                data: vals(&[1]),
            }],
        };
        let expected = json!({
            "resultId": "r2",
            "edits": [{"start": 0, "deleteCount": 1, "data": [1]}],
        });
        assert_eq!(serde_json::to_value(&response).unwrap(), expected);
    }

    #[test]
    fn captures_delta_response_full_variant_serializes_like_the_old_json_macro_shape() {
        let response = CapturesDeltaResponse::Full(CapturesFullResponse {
            result_id: "r3".to_string(),
            matches: Arc::new(vals(&[9])),
            skipped: Arc::new(Vec::new()),
        });
        let expected = json!({
            "resultId": "r3",
            "matches": [9],
            "skipped": [],
        });
        assert_eq!(serde_json::to_value(&response).unwrap(), expected);
    }

    /// Serve-current (ADR §3, revised): a captures request arriving while the
    /// latest snapshot trails the live text parks until the current snapshot
    /// publishes instead of walking the trailing one (whose result the
    /// lineage gate would void into a `null` → re-request busy-loop).
    #[tokio::test(start_paused = true)]
    async fn captures_full_parks_until_current_snapshot() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///captures_serve_current.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = service
            .inner()
            .documents
            .latest_snapshot(&uri)
            .expect("document just inserted")
            .slot
            .current_incarnation;
        let publish = |text: &str, parsed_version: u64| {
            let landed = service
                .inner()
                .documents
                .get(&uri)
                .map(|doc| {
                    doc.publish_snapshot(std::sync::Arc::new(
                        crate::document::snapshot::ParseSnapshot {
                            text: std::sync::Arc::from(text),
                            tree: None,
                            language: Some("rust".to_string()),
                            parsed_version,
                            incarnation,
                            injection_regions: None,
                            bridge_regions: None,
                            resolved_regions: None,
                            layer_trees: std::sync::OnceLock::new(),
                        },
                    ))
                })
                .unwrap_or(false);
            assert!(landed, "test snapshot must land");
        };
        publish("fn main() {}", 0);
        // Edit: content_version moves to 1, the v0 snapshot now trails.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .kakehashi_captures_full(CapturesFullParams {
                        text_document: TextDocumentIdentifier {
                            uri: crate::lsp::lsp_impl::url_to_uri(&uri)
                                .expect("test URI should convert"),
                        },
                        kind: "highlights".to_string(),
                        injection: false,
                    })
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the handler must park on the trailing snapshot, not answer from it"
        );
        publish("fn main() { }", 1);
        let woke_at = tokio::time::Instant::now();
        let result = request
            .await
            .expect("handler must not panic")
            .expect("captures full must not error");
        // Tree-less snapshot → the walk degrades to the protocol null; what
        // matters here is WHEN it answered (after currency), not the shape.
        assert!(result.is_none());
        // Discriminate woke-on-publish from backstop-timeout: under the
        // paused clock a handler that missed the publish would only answer
        // after auto-advancing through the settle backstop.
        assert!(
            woke_at.elapsed() < crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP,
            "the handler must wake on the publish, not ride out the backstop"
        );
    }

    /// `captures/range` is the position/range class: a trailing snapshot must
    /// re-sync via `null` IMMEDIATELY (zero stale wait) — the parked settle
    /// wait is the full/delta policy only. A per-redraw viewport request that
    /// parked would hold an ingress slot for up to the backstop.
    #[tokio::test(start_paused = true)]
    async fn captures_range_stale_rejects_immediately_via_null() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///captures_range_stale.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = service
            .inner()
            .documents
            .latest_snapshot(&uri)
            .expect("document just inserted")
            .slot
            .current_incarnation;
        let landed = service
            .inner()
            .documents
            .get(&uri)
            .map(|doc| {
                doc.publish_snapshot(std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from("fn main() {}"),
                        tree: None,
                        language: Some("rust".to_string()),
                        parsed_version: 0,
                        incarnation,
                        injection_regions: None,
                        bridge_regions: None,
                        resolved_regions: None,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                ))
            })
            .unwrap_or(false);
        assert!(landed, "test snapshot must land");
        // Edit past the published parse: the v0 snapshot now trails.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let started_at = tokio::time::Instant::now();
        let result = service
            .inner()
            .kakehashi_captures_range(CapturesRangeParams {
                text_document: TextDocumentIdentifier {
                    uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
                },
                kind: "highlights".to_string(),
                range: Range::default(),
                injection: false,
            })
            .await
            .expect("range must not error");
        assert!(result.is_none(), "trailing snapshot re-syncs via null");
        assert!(
            started_at.elapsed() < crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP,
            "range must reject immediately, not park out the settle backstop"
        );
    }

    /// Single-flight: a request that finds another walk in flight for the
    /// same `(uri, kind, injection)` parks on the winner's `Notify`, and on
    /// wake serves the winner's memo instead of re-executing the walk. Pinned
    /// with a sentinel memo no real walk could produce.
    #[tokio::test(start_paused = true)]
    async fn captures_full_single_flight_loser_serves_winner_memo() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///captures_single_flight.rs").expect("valid test uri");
        let text = "fn main() {}";

        service.inner().documents.insert(
            uri.clone(),
            text.to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = service
            .inner()
            .documents
            .latest_snapshot(&uri)
            .expect("document just inserted")
            .slot
            .current_incarnation;
        // A real tree so the handler passes the tree check and reaches the
        // single-flight loop (a tree-less snapshot short-circuits to null).
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar loads");
        let tree = parser.parse(text, None).expect("parse rust");
        let landed = service
            .inner()
            .documents
            .get(&uri)
            .map(|doc| {
                doc.publish_snapshot(std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from(text),
                        tree: Some(tree),
                        language: Some("rust".to_string()),
                        parsed_version: 0,
                        incarnation,
                        injection_regions: None,
                        bridge_regions: None,
                        resolved_regions: None,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                ))
            })
            .unwrap_or(false);
        assert!(landed, "test snapshot must land");
        let generation = service.inner().cache.semantic_token_generation();

        // Simulate a winner already walking this key.
        let key = (uri.clone(), "highlights".to_string(), false);
        let winner_flight = std::sync::Arc::new(CapturesWalkFlight::new(
            CapturesWalkTag {
                incarnation,
                generation,
                parsed_version: 0,
            },
            crate::cancel::CancelToken::default(),
        ));
        let winner_notify = std::sync::Arc::clone(&winner_flight.notify);
        service
            .inner()
            .captures_walk_inflight
            .insert(key.clone(), winner_flight);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .kakehashi_captures_full(CapturesFullParams {
                        text_document: TextDocumentIdentifier {
                            uri: crate::lsp::lsp_impl::url_to_uri(&uri)
                                .expect("test URI should convert"),
                        },
                        kind: "highlights".to_string(),
                        injection: false,
                    })
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the loser must park on the winner's in-flight marker"
        );

        // Winner completes: memo BEFORE marker removal + wake — the same
        // ordering the handler's `_flight_guard` drop guarantees.
        let sentinel = json!({"sentinel": "from-winner-memo"});
        service.inner().captures_walk_cache.insert(
            key.clone(),
            CachedCapturesWalk {
                parsed_version: 0,
                incarnation,
                generation,
                result: Some((
                    std::sync::Arc::new(vec![sentinel.clone()]),
                    std::sync::Arc::new(Vec::new()),
                )),
            },
        );
        service.inner().captures_walk_inflight.remove(&key);
        winner_notify.notify_waiters();

        let result = request
            .await
            .expect("handler task must not panic")
            .expect("captures full must not error")
            .expect("winner memo is Some");
        assert_eq!(
            result.matches[0], sentinel,
            "the loser must serve the winner's memo, not run its own walk"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn superseded_captures_loser_does_not_revive_old_memo() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///captures_stale_loser.rs").unwrap();
        let text = "fn main() {}";
        service.inner().documents.insert(
            uri.clone(),
            text.to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = service
            .inner()
            .documents
            .latest_snapshot(&uri)
            .unwrap()
            .slot
            .current_incarnation;
        let parse = |source: &str| {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            parser.parse(source, None).unwrap()
        };
        let publish = |version, source: &'static str| {
            service
                .inner()
                .documents
                .get(&uri)
                .unwrap()
                .publish_snapshot(std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from(source),
                        tree: Some(parse(source)),
                        language: Some("rust".to_string()),
                        parsed_version: version,
                        incarnation,
                        injection_regions: None,
                        bridge_regions: None,
                        resolved_regions: None,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                ))
        };
        assert!(publish(0, text));

        let generation = service.inner().cache.semantic_token_generation();
        let key = (uri.clone(), "highlights".to_string(), false);
        let old_flight = std::sync::Arc::new(CapturesWalkFlight::new(
            CapturesWalkTag {
                incarnation,
                generation,
                parsed_version: 0,
            },
            crate::cancel::CancelToken::default(),
        ));
        service
            .inner()
            .captures_walk_inflight
            .insert(key.clone(), old_flight);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .kakehashi_captures_full(CapturesFullParams {
                        text_document: TextDocumentIdentifier {
                            uri: crate::lsp::lsp_impl::url_to_uri(&uri).unwrap(),
                        },
                        kind: "highlights".to_string(),
                        injection: false,
                    })
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!request.is_finished());

        let newer = "fn newer() {}";
        service
            .inner()
            .documents
            .update_document(uri.clone(), newer.to_string(), None);
        assert!(publish(1, newer));
        let WalkFlightClaim::Winner(new_flight) = claim_walk_flight(
            &service.inner().captures_walk_inflight,
            &key,
            CapturesWalkTag {
                incarnation,
                generation,
                parsed_version: 1,
            },
            crate::cancel::CancelToken::default(),
        ) else {
            panic!("new snapshot must supersede old flight");
        };
        service
            .inner()
            .captures_walk_inflight
            .remove_if(&key, |_, current| {
                std::sync::Arc::ptr_eq(current, &new_flight)
            });
        new_flight.notify.notify_waiters();

        let result = request
            .await
            .unwrap()
            .expect("internal supersession is null");
        assert!(result.is_none());
        assert!(
            service.inner().captures_walk_cache.get(&key).is_none(),
            "the delayed old loser must not recreate an obsolete memo"
        );
    }

    #[test]
    fn newer_snapshot_cancels_older_captures_flight() {
        let flights = dashmap::DashMap::new();
        let uri = Url::parse("file:///captures_supersede.rs").expect("valid test uri");
        let key = (uri, "highlights".to_string(), false);
        let old_cancel = crate::cancel::CancelToken::default();
        let old_flight = std::sync::Arc::new(CapturesWalkFlight::new(
            CapturesWalkTag {
                incarnation: 1,
                generation: 4,
                parsed_version: 0,
            },
            old_cancel.clone(),
        ));
        flights.insert(key.clone(), std::sync::Arc::clone(&old_flight));
        let old_guard = WalkFlightGuard {
            map: &flights,
            key: key.clone(),
            flight: old_flight,
        };

        let new_cancel = crate::cancel::CancelToken::default();
        let new_tag = CapturesWalkTag {
            incarnation: 1,
            generation: 4,
            parsed_version: 1,
        };
        let WalkFlightClaim::Winner(new_flight) =
            claim_walk_flight(&flights, &key, new_tag, new_cancel.clone())
        else {
            panic!("the newer snapshot must become the winner");
        };

        assert!(
            old_cancel.is_cancelled(),
            "a current-snapshot request must stop the obsolete walk"
        );
        assert!(!new_cancel.is_cancelled());
        drop(old_guard);
        assert!(
            flights
                .get(&key)
                .is_some_and(|current| std::sync::Arc::ptr_eq(&current, &new_flight)),
            "the old guard must not remove the atomically installed successor"
        );

        let WalkFlightClaim::Join(joined) = claim_walk_flight(
            &flights,
            &key,
            new_tag,
            crate::cancel::CancelToken::default(),
        ) else {
            panic!("an identical snapshot must join the winner");
        };
        assert!(std::sync::Arc::ptr_eq(&joined, &new_flight));

        let older = CapturesWalkTag {
            parsed_version: 0,
            ..new_tag
        };
        assert!(matches!(
            claim_walk_flight(&flights, &key, older, crate::cancel::CancelToken::default()),
            WalkFlightClaim::Obsolete
        ));
        assert!(
            !new_cancel.is_cancelled(),
            "an obsolete requester must not cancel the newer winner"
        );

        let reopened_cancel = crate::cancel::CancelToken::default();
        let reopened_tag = CapturesWalkTag {
            incarnation: 2,
            generation: 4,
            parsed_version: 0,
        };
        assert!(matches!(
            claim_walk_flight(&flights, &key, reopened_tag, reopened_cancel.clone()),
            WalkFlightClaim::Winner(_)
        ));
        assert!(
            new_cancel.is_cancelled(),
            "a reopened document must supersede the prior lifetime despite version reset"
        );
        assert!(!reopened_cancel.is_cancelled());
    }

    #[test]
    fn document_close_cancels_only_its_captures_flights() {
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let closed_uri = Url::parse("file:///captures-close.rs").expect("valid test uri");
        let live_uri = Url::parse("file:///captures-live.rs").expect("valid test uri");
        let tag = CapturesWalkTag {
            incarnation: 1,
            generation: 0,
            parsed_version: 0,
        };
        let closed_cancel = crate::cancel::CancelToken::default();
        let live_cancel = crate::cancel::CancelToken::default();
        service.inner().captures_walk_inflight.insert(
            (closed_uri.clone(), "highlights".to_string(), false),
            std::sync::Arc::new(CapturesWalkFlight::new(tag, closed_cancel.clone())),
        );
        service.inner().captures_walk_inflight.insert(
            (live_uri.clone(), "highlights".to_string(), false),
            std::sync::Arc::new(CapturesWalkFlight::new(tag, live_cancel.clone())),
        );

        service
            .inner()
            .cancel_captures_walks_for_document(&closed_uri);

        assert!(closed_cancel.is_cancelled());
        assert!(!live_cancel.is_cancelled());
        assert!(
            service
                .inner()
                .captures_walk_inflight
                .iter()
                .all(|entry| entry.key().0 != closed_uri)
        );
        assert!(service.inner().captures_walk_inflight.contains_key(&(
            live_uri,
            "highlights".to_string(),
            false
        )));
    }

    /// A `$/cancelRequest` arriving while the handler is parked on a trailing
    /// snapshot must answer RequestCancelled promptly — not sit out the
    /// settle backstop.
    #[tokio::test(start_paused = true)]
    async fn captures_full_cancel_while_parked_answers_request_cancelled() {
        use tower_lsp_server::jsonrpc::Id;

        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///captures_cancel_parked.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // No snapshot published for the live content version: with an edit
        // past the (absent) parse, the handler parks in the serve-current
        // wait.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            // The upstream request id rides task-local storage (installed by
            // the RequestIdCapture middleware in production).
            tokio::spawn(crate::lsp::request_id::CURRENT_REQUEST_ID.scope(
                Some(Id::Number(77)),
                async move {
                    service
                        .inner()
                        .kakehashi_captures_full(CapturesFullParams {
                            text_document: TextDocumentIdentifier {
                                uri: crate::lsp::lsp_impl::url_to_uri(&uri)
                                    .expect("test URI should convert"),
                            },
                            kind: "highlights".to_string(),
                            injection: false,
                        })
                        .await
                },
            ))
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the handler must be parked awaiting the current snapshot"
        );

        service
            .inner()
            .bridge
            .cancel_forwarder()
            .forward_cancel(crate::lsp::bridge::UpstreamId::Number(77))
            .await
            .expect("cancel forward must not error");

        let result = request.await.expect("handler task must not panic");
        let err = result.expect_err("a cancelled parked request must error, not answer");
        assert_eq!(
            err.code,
            JsonRpcError::request_cancelled().code,
            "the parked handler must answer RequestCancelled on $/cancelRequest"
        );
    }

    fn first_node_id(matches: &[Value]) -> ulid::Ulid {
        matches[0]["captures"][0]["node"]["id"]
            .as_str()
            .expect("capture carries a node id")
            .parse()
            .expect("node id is a ULID")
    }

    /// The NodeTracker is a live-coordinate index; the walk CORE, run
    /// directly against a snapshot that trails the live document, must still
    /// serve matches but must NOT register its ids in the tracker — a stale
    /// read never mints (see the currency gate in `execute_captures_walk`).
    /// The handler above it parks for the current snapshot (serve-current),
    /// so this is defense in depth for the close/reopen races the handler's
    /// own incarnation checks bound but cannot eliminate.
    #[test]
    fn stale_serve_returns_matches_but_never_mints_into_the_tracker() {
        use crate::config::WorkspaceSettings;
        use crate::document::DocumentStore;
        use crate::language::{LanguageCoordinator, NodeTracker};
        use std::sync::Arc;

        // Statically-linked grammar + a temp query root: no dylib fixtures.
        let dir = tempfile::tempdir().unwrap();
        let query_dir = dir.path().join("queries/rust");
        std::fs::create_dir_all(&query_dir).unwrap();
        std::fs::write(
            query_dir.join("locals.scm"),
            "(identifier) @local.reference\n",
        )
        .unwrap();

        let coordinator = Arc::new(LanguageCoordinator::new());
        let settings = WorkspaceSettings {
            search_paths: vec![dir.path().to_string_lossy().into_owned()],
            ..Default::default()
        };
        coordinator.load_settings(&settings);
        coordinator
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        let text = "fn main() { let x = 1; }\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(text, None).unwrap();

        let store = DocumentStore::new();
        let uri = Url::parse("file:///stale_mint.rs").unwrap();
        store.insert(uri.clone(), text.to_string(), Some("rust".into()), None);
        // Current serve: the caller-computed gate is true (minting live).
        let tracker = NodeTracker::new();
        let match_cache = super::super::captures_match_cache::CapturesMatchCache::new();
        let (matches, _) = execute_captures_walk(
            &uri,
            "locals",
            None,
            false,
            "rust",
            text,
            &tree,
            None,
            &coordinator,
            &tracker,
            &store,
            tracker.mint_epoch(&uri),
            true,
            0,
            &match_cache,
            None,
        )
        .expect("kind query should load");
        assert!(!matches.is_empty(), "identifiers should match");
        let id = first_node_id(&matches);
        assert!(
            tracker.lookup_node(&uri, &id).is_some(),
            "a current serve mints resolvable ids"
        );

        // Trailing (an edit bumped content_version past parsed_version):
        // matches still serve, and positions the tracker already knows reuse
        // their live id read-only (delta lineage stays a minimal diff).
        store.update_document(uri.clone(), text.to_string(), None);
        let (stale_matches, _) = execute_captures_walk(
            &uri,
            "locals",
            None,
            false,
            "rust",
            text,
            &tree,
            None,
            &coordinator,
            &tracker,
            &store,
            tracker.mint_epoch(&uri),
            false,
            0,
            &match_cache,
            None,
        )
        .expect("kind query should load");
        assert!(
            !stale_matches.is_empty(),
            "a stale walk still returns matches"
        );
        assert_eq!(
            first_node_id(&stale_matches),
            id,
            "a stale serve reuses the live id for an unshifted position"
        );

        // A stale serve against a tracker with no entry for the position must
        // not create one — the id it hands out stays unregistered.
        let stale_tracker = NodeTracker::new();
        let (stale_matches, _) = execute_captures_walk(
            &uri,
            "locals",
            None,
            false,
            "rust",
            text,
            &tree,
            None,
            &coordinator,
            &stale_tracker,
            &store,
            stale_tracker.mint_epoch(&uri),
            false,
            0,
            &match_cache,
            None,
        )
        .expect("kind query should load");
        let id = first_node_id(&stale_matches);
        assert!(
            stale_tracker.lookup_node(&uri, &id).is_none(),
            "a stale serve must not mint into the live tracker"
        );
    }

    /// Build a rust "injection layer": the full text parsed with included
    /// ranges restricted to `[start, end)` — the shape `SnapshotLayerTree`
    /// carries for a real fence.
    fn rust_layer(text: &str, start: usize, end: usize) -> crate::document::SnapshotLayerTree {
        rust_layer_ranges(text, &[(start, end)])
    }

    /// Multi-range variant of [`rust_layer`] — the blockquote-gap shape,
    /// where a layer's included ranges are disjoint spans of the host text.
    fn rust_layer_ranges(
        text: &str,
        ranges: &[(usize, usize)],
    ) -> crate::document::SnapshotLayerTree {
        let point_at = |byte: usize| {
            let line = text[..byte].matches('\n').count();
            let col = byte - text[..byte].rfind('\n').map_or(0, |i| i + 1);
            tree_sitter::Point::new(line, col)
        };
        let ts_ranges: Vec<tree_sitter::Range> = ranges
            .iter()
            .map(|&(start, end)| tree_sitter::Range {
                start_byte: start,
                end_byte: end,
                start_point: point_at(start),
                end_point: point_at(end),
            })
            .collect();
        let (start, end) = (ranges[0].0, ranges[ranges.len() - 1].1);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.set_included_ranges(&ts_ranges).unwrap();
        crate::document::SnapshotLayerTree {
            language: "rust".to_string(),
            tree: parser.parse(text, None).unwrap(),
            depth: 1,
            span: start..end,
        }
    }

    /// Test rig for the cross-snapshot match cache: a statically-linked rust
    /// grammar with a locals.scm, so both the host layer and a synthetic
    /// injected layer produce captures.
    struct MatchCacheRig {
        _dir: tempfile::TempDir,
        coordinator: std::sync::Arc<crate::language::LanguageCoordinator>,
        store: crate::document::DocumentStore,
        tracker: crate::language::NodeTracker,
        match_cache: super::super::captures_match_cache::CapturesMatchCache,
        uri: Url,
    }

    impl MatchCacheRig {
        fn new(uri: &str, text: &str) -> Self {
            Self::new_with_query(uri, text, "(identifier) @local.reference\n")
        }

        fn new_with_query(uri: &str, text: &str, locals_scm: &str) -> Self {
            use crate::config::WorkspaceSettings;
            let dir = tempfile::tempdir().unwrap();
            let query_dir = dir.path().join("queries/rust");
            std::fs::create_dir_all(&query_dir).unwrap();
            std::fs::write(query_dir.join("locals.scm"), locals_scm).unwrap();
            let coordinator = std::sync::Arc::new(crate::language::LanguageCoordinator::new());
            coordinator.load_settings(&WorkspaceSettings {
                search_paths: vec![dir.path().to_string_lossy().into_owned()],
                ..Default::default()
            });
            coordinator
                .language_registry_for_parallel()
                .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
            let store = crate::document::DocumentStore::new();
            let uri = Url::parse(uri).unwrap();
            store.insert(uri.clone(), text.to_string(), Some("rust".into()), None);
            Self {
                _dir: dir,
                coordinator,
                store,
                tracker: crate::language::NodeTracker::new(),
                match_cache: super::super::captures_match_cache::CapturesMatchCache::new(),
                uri,
            }
        }

        fn walk(
            &self,
            text: &str,
            layers: &[crate::document::SnapshotLayerTree],
            parsed_version: u64,
            lsp_range: Option<Range>,
        ) -> Option<(Vec<Value>, Vec<Value>)> {
            self.walk_with(text, layers, parsed_version, lsp_range, true, None, 0)
        }

        #[allow(clippy::too_many_arguments)]
        fn walk_with(
            &self,
            text: &str,
            layers: &[crate::document::SnapshotLayerTree],
            parsed_version: u64,
            lsp_range: Option<Range>,
            injection: bool,
            cancel: Option<&crate::cancel::CancelToken>,
            // The kind-query compile cache is PROCESS-WIDE, keyed
            // (language, kind file, generation) — a rig with a custom
            // locals.scm must walk under a generation no other test uses,
            // or it silently serves another rig's compiled query.
            generation: u64,
        ) -> Option<(Vec<Value>, Vec<Value>)> {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(text, None).unwrap();
            let incarnation = self
                .store
                .latest_snapshot(&self.uri)
                .unwrap()
                .slot
                .current_incarnation;
            // Latch-then-validate, mirroring the production caller
            // (`compute_captures` takes both under the edit lock).
            let entry_mint_epoch = self.tracker.mint_epoch(&self.uri);
            let mint_into_tracker = self.store.latest_snapshot(&self.uri).is_some_and(|view| {
                view.slot.current_incarnation == incarnation
                    && view.content_version == parsed_version
            });
            execute_captures_walk(
                &self.uri,
                "locals",
                lsp_range,
                injection,
                "rust",
                text,
                &tree,
                injection.then_some(layers),
                &self.coordinator,
                &self.tracker,
                &self.store,
                entry_mint_epoch,
                mint_into_tracker,
                generation,
                &self.match_cache,
                cancel,
            )
        }
    }

    /// Batch id reconciliation alignment: every capture in a current serve
    /// carries the id of ITS OWN span — resolvable via `lookup_node` to
    /// exactly the byte range and kind the response reports for it. An
    /// off-by-one in the one-id-per-capture alignment (e.g. a dropped
    /// capture shifting later ids) would pair some capture with a
    /// neighbor's id and fail the span equality.
    #[test]
    fn current_serve_ids_align_one_per_capture_with_their_own_spans() {
        let code = "let alpha = beta + gamma;\n";
        let text = format!("AAAA\n{code}");
        let start = text.find(code).unwrap();
        let rig = MatchCacheRig::new("file:///batch_align.rs", &text);
        let layer = rust_layer(&text, start, text.len());
        let (matches, _) = rig
            .walk(&text, &[layer], 0, None)
            .expect("kind query should load");
        let mapper = PositionMapper::new(&text);
        let to_pos = |p: &Value| {
            tower_lsp_server::ls_types::Position::new(
                u32::try_from(p["line"].as_u64().unwrap()).unwrap(),
                u32::try_from(p["character"].as_u64().unwrap()).unwrap(),
            )
        };
        let mut seen = 0;
        for m in &matches {
            for c in m["captures"].as_array().unwrap() {
                let id: ulid::Ulid = c["node"]["id"].as_str().unwrap().parse().unwrap();
                let (s, e, kind, _layer) = rig
                    .tracker
                    .lookup_node(&rig.uri, &id)
                    .expect("a current serve mints resolvable ids");
                assert_eq!(
                    mapper.position_to_byte_clamped(to_pos(&c["range"]["start"])),
                    s,
                    "capture start must be the id's own tracked start"
                );
                assert_eq!(
                    mapper.position_to_byte_clamped(to_pos(&c["range"]["end"])),
                    e,
                    "capture end must be the id's own tracked end"
                );
                assert_eq!(c["node"]["kind"].as_str().unwrap(), kind);
                seen += 1;
            }
        }
        assert!(
            seen >= 4,
            "host and layer captures should both contribute (got {seen})"
        );
    }

    /// The wire shape of a match envelope, pinned byte-for-byte — field
    /// order included. The shaping loop builds envelopes by direct Map
    /// construction (moving sub-values instead of re-serializing them
    /// through the json! macro), and this serialization is what keeps that
    /// construction from drifting the JSON the captures protocol documents.
    #[test]
    fn envelope_wire_shape_is_stable() {
        let text = "fn x() {}\n";
        let rig = MatchCacheRig::new("file:///wire_shape.rs", text);
        let (matches, _) = rig
            .walk(text, &[], 0, None)
            .expect("kind query should load");
        assert_eq!(matches.len(), 1, "one identifier expected");
        let m = &matches[0];
        let id = m["captures"][0]["node"]["id"].as_str().unwrap().to_owned();
        let expected = format!(
            "{{\"patternIndex\":0,\"language\":\"rust\",\"captures\":[\
             {{\"name\":\"local.reference\",\
             \"node\":{{\"id\":\"{id}\",\"kind\":\"identifier\"}},\
             \"range\":{{\"start\":{{\"line\":0,\"character\":3}},\
             \"end\":{{\"line\":0,\"character\":4}}}}}}]}}"
        );
        assert_eq!(serde_json::to_string(m).unwrap(), expected);
    }

    /// The dropped-capture half of the alignment invariant: a capture whose
    /// span fails position mapping is dropped from the response, and the
    /// SURVIVING captures must keep their own ids — consuming the batch
    /// index after the drop (instead of before) would hand the survivor the
    /// dropped capture's id. Unmappable spans cannot come from a real tree,
    /// so one is planted as a cached-matches sentinel (the walk trusts the
    /// content-addressed cache).
    #[test]
    fn dropped_capture_does_not_shift_surviving_capture_ids() {
        let text = "fn x() {}\n";
        let rig = MatchCacheRig::new("file:///drop_align.rs", text);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(text, None).unwrap();
        let (anchor, hash) =
            super::super::captures_match_cache::tree_cache_key("locals", "rust", &tree, text);
        assert_eq!(anchor, 0, "host layer anchors at 0");
        rig.match_cache.store_host(
            &rig.uri,
            "locals",
            hash,
            0,
            std::sync::Arc::new(vec![crate::language::query_exec::MatchData {
                pattern_index: 0,
                captures: vec![
                    // Unmappable span (beyond EOF): dropped by the position
                    // filter AFTER its id was consumed from the batch.
                    crate::language::query_exec::CapturedNode {
                        name: "dropped".to_string(),
                        start_byte: text.len() + 10,
                        end_byte: text.len() + 12,
                        kind: "identifier",
                        metadata: Vec::new(),
                    },
                    crate::language::query_exec::CapturedNode {
                        name: "survivor".to_string(),
                        start_byte: 3,
                        end_byte: 4,
                        kind: "identifier",
                        metadata: Vec::new(),
                    },
                ],
                metadata: Vec::new(),
            }]),
            || true,
        );

        let (matches, _) = rig
            .walk(text, &[], 0, None)
            .expect("kind query should load");
        let envelope = &matches[0];
        let captures = envelope["captures"].as_array().unwrap();
        assert_eq!(captures.len(), 1, "the unmappable capture is dropped");
        assert_eq!(captures[0]["name"], "survivor");
        let id: ulid::Ulid = captures[0]["node"]["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            rig.tracker.lookup_node(&rig.uri, &id),
            Some((3, 4, "identifier", 0)),
            "the survivor must keep ITS OWN id, not inherit the dropped capture's"
        );
    }

    /// Wire position of the optional `metadata` field, pinned byte-for-byte
    /// at both levels: last key of the capture object and last key of the
    /// match envelope (the `#set!` directives' documented shape).
    #[test]
    fn metadata_wire_position_is_stable() {
        let text = "fn x() {}\n";
        let rig = MatchCacheRig::new_with_query(
            "file:///wire_shape_meta.rs",
            text,
            "((identifier) @local.reference\n  (#set! @local.reference ck \"cv\")\n  (#set! mk \"mv\"))\n",
        );
        // Unique generation: the process-wide kind-query compile cache would
        // otherwise serve another rig's plain locals.scm (see walk_with).
        let (matches, _) = rig
            .walk_with(text, &[], 0, None, true, None, 7777)
            .expect("kind query should load");
        assert_eq!(matches.len(), 1, "one identifier expected");
        let m = &matches[0];
        let id = m["captures"][0]["node"]["id"].as_str().unwrap().to_owned();
        let expected = format!(
            "{{\"patternIndex\":0,\"language\":\"rust\",\"captures\":[\
             {{\"name\":\"local.reference\",\
             \"node\":{{\"id\":\"{id}\",\"kind\":\"identifier\"}},\
             \"range\":{{\"start\":{{\"line\":0,\"character\":3}},\
             \"end\":{{\"line\":0,\"character\":4}}}},\
             \"metadata\":{{\"ck\":\"cv\"}}}}],\
             \"metadata\":{{\"mk\":\"mv\"}}}}"
        );
        assert_eq!(serde_json::to_string(m).unwrap(), expected);
    }

    fn capture_names(matches: &[Value]) -> Vec<String> {
        matches
            .iter()
            .flat_map(|m| m["captures"].as_array().unwrap())
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// The read path of the cross-snapshot match cache: after an edit ABOVE
    /// the layer (content translated, not changed), the walk must serve the
    /// layer's cached matches instead of re-executing the query. Pinned with
    /// a sentinel entry, pre-stored under the shifted layer's key, that no
    /// real query execution could produce.
    #[test]
    fn translated_layer_serves_cached_matches_without_reexecuting() {
        let code = "let a = 1;\n";
        let text_v1 = format!("AAAA\n{code}");
        let start_v1 = text_v1.find(code).unwrap();
        let rig = MatchCacheRig::new("file:///match_cache_translated.rs", &text_v1);

        let layer_v1 = rust_layer(&text_v1, start_v1, text_v1.len());
        rig.walk(&text_v1, &[layer_v1], 0, None)
            .expect("kind query should load");

        // Edit above the layer: same layer content, shifted by 4 bytes.
        let text_v2 = format!("AAAABBBB\n{code}");
        let start_v2 = text_v2.find(code).unwrap();
        let layer_v2 = rust_layer(&text_v2, start_v2, text_v2.len());
        rig.store
            .update_document(rig.uri.clone(), text_v2.clone(), None);

        // The shifted layer must key IDENTICALLY to what v1 stored
        // (translation invariance); plant a sentinel there to discriminate
        // "served from cache" from "silently re-executed".
        let (anchor_v2, hash_v2) = super::super::captures_match_cache::tree_cache_key(
            "locals",
            "rust",
            &layer_v2.tree,
            &text_v2,
        );
        assert_eq!(anchor_v2, start_v2);
        assert!(
            rig.match_cache.get_layer(&rig.uri, hash_v2, 0).is_some(),
            "the translated layer's key must hit what the v1 walk stored"
        );
        rig.match_cache.store_layer(
            &rig.uri,
            "locals",
            hash_v2,
            0,
            std::sync::Arc::new(vec![crate::language::query_exec::MatchData {
                pattern_index: 0,
                captures: vec![crate::language::query_exec::CapturedNode {
                    name: "sentinel-from-cache".to_string(),
                    start_byte: 4,
                    end_byte: 5,
                    kind: "identifier",
                    metadata: Vec::new(),
                }],
                metadata: Vec::new(),
            }]),
            || true,
        );

        let (matches, _) = rig
            .walk(&text_v2, &[layer_v2], 1, None)
            .expect("kind query should load");
        let names = capture_names(&matches);
        assert!(
            names.iter().any(|n| n == "sentinel-from-cache"),
            "the layer walk must serve the cached entry, not re-execute: {names:?}"
        );
        // And the cached (anchor-relative) offsets must be re-anchored to the
        // layer's CURRENT position: rel 4..5 inside the layer is `a`.
        let sentinel = matches
            .iter()
            .flat_map(|m| m["captures"].as_array().unwrap())
            .find(|c| c["name"] == "sentinel-from-cache")
            .unwrap();
        assert_eq!(
            sentinel["range"]["start"]["line"], 1,
            "anchor must be the v2 layer start, not the v1 one"
        );
        assert_eq!(sentinel["range"]["start"]["character"], 4);
    }

    /// Correctness of the reuse: a walk served from the (translated) cache
    /// must be byte-identical on the wire to a walk computed fresh — same
    /// positions, same node ids (`get_or_create` on the same tracker), same
    /// metadata. On the caching axis this test alone would pass vacuously
    /// (no hit → cached == fresh trivially); the sentinel test above proves
    /// hits actually occur for this exact fixture shape.
    #[test]
    fn cached_and_fresh_walks_agree_on_the_wire() {
        let code = "let a = 1;\n";
        let text_v1 = format!("AAAA\n{code}");
        let start_v1 = text_v1.find(code).unwrap();
        let rig = MatchCacheRig::new("file:///match_cache_equality.rs", &text_v1);
        rig.walk(
            &text_v1,
            &[rust_layer(&text_v1, start_v1, text_v1.len())],
            0,
            None,
        )
        .expect("kind query should load");

        let text_v2 = format!("AAAABBBB\n{code}");
        let start_v2 = text_v2.find(code).unwrap();
        rig.store
            .update_document(rig.uri.clone(), text_v2.clone(), None);

        let (cached, _) = rig
            .walk(
                &text_v2,
                &[rust_layer(&text_v2, start_v2, text_v2.len())],
                1,
                None,
            )
            .expect("kind query should load");
        // Fresh compute of the same walk, cache emptied: must agree exactly.
        rig.match_cache.clear_document(&rig.uri);
        let (fresh, _) = rig
            .walk(
                &text_v2,
                &[rust_layer(&text_v2, start_v2, text_v2.len())],
                1,
                None,
            )
            .expect("kind query should load");
        assert_eq!(cached, fresh, "cache-served wire output must be identical");
    }

    /// Range walks produce viewport-clipped results: they must neither store
    /// into nor serve from the cross-snapshot match cache.
    #[test]
    fn range_walks_bypass_the_match_cache() {
        let text = "let a = 1;\n";
        let rig = MatchCacheRig::new("file:///match_cache_range.rs", text);
        let layer = rust_layer(text, 0, text.len());
        let (_, host_hash) =
            super::super::captures_match_cache::tree_cache_key("locals", "rust", &layer.tree, text);

        rig.walk(
            text,
            &[layer],
            0,
            Some(Range::new(
                tower_lsp_server::ls_types::Position::new(0, 0),
                tower_lsp_server::ls_types::Position::new(0, 5),
            )),
        )
        .expect("kind query should load");
        assert!(
            rig.match_cache.get_layer(&rig.uri, host_hash, 0).is_none()
                && rig
                    .match_cache
                    .get_host(&rig.uri, "locals", host_hash, 0)
                    .is_none(),
            "a range walk must not populate the match cache"
        );
    }

    /// The read half of the range-walk gate: even when full-key entries
    /// exist, a range walk must not serve them (its results are clipped to
    /// the viewport, and a full-layer cache entry is not).
    #[test]
    fn range_walks_do_not_serve_cached_entries() {
        let text = "let a = 1;\n";
        let rig = MatchCacheRig::new("file:///match_cache_range_read.rs", text);
        let layer = rust_layer(text, 0, text.len());
        let (_, layer_hash) =
            super::super::captures_match_cache::tree_cache_key("locals", "rust", &layer.tree, text);
        rig.match_cache.store_layer(
            &rig.uri,
            "locals",
            layer_hash,
            0,
            std::sync::Arc::new(vec![crate::language::query_exec::MatchData {
                pattern_index: 0,
                captures: vec![crate::language::query_exec::CapturedNode {
                    name: "sentinel-must-not-serve".to_string(),
                    start_byte: 0,
                    end_byte: 3,
                    kind: "identifier",
                    metadata: Vec::new(),
                }],
                metadata: Vec::new(),
            }]),
            || true,
        );

        let (matches, _) = rig
            .walk(
                text,
                &[layer],
                0,
                Some(Range::new(
                    tower_lsp_server::ls_types::Position::new(0, 0),
                    tower_lsp_server::ls_types::Position::new(0, 11),
                )),
            )
            .expect("kind query should load");
        assert!(
            !capture_names(&matches)
                .iter()
                .any(|n| n == "sentinel-must-not-serve"),
            "a range walk must compute fresh, not serve full-walk entries"
        );
    }

    /// A capture in a LATER included range of a multi-range (blockquote-gap
    /// shaped) layer: rebase is relative to the FIRST range's start, so this
    /// is where an anchor off-by-one would hide. After a vertical shift the
    /// hit must place the later-range capture at its exact new position,
    /// byte-identical to a fresh compute.
    #[test]
    fn multi_range_layer_hit_reanchors_later_range_captures_exactly() {
        let (part_a, gap, part_b) = ("let a = 1;\n", "// gap\n", "let b = 2;\n");
        let make = |pad: &str| {
            let text = format!("{pad}{part_a}{gap}{part_b}");
            let a_start = text.find(part_a).unwrap();
            let b_start = text.find(part_b).unwrap();
            let ranges = [
                (a_start, a_start + part_a.len()),
                (b_start, b_start + part_b.len()),
            ];
            let layer = rust_layer_ranges(&text, &ranges);
            (text, layer)
        };

        let (text_v1, layer_v1) = make("AAAA\n");
        let rig = MatchCacheRig::new("file:///match_cache_multirange.rs", &text_v1);
        rig.walk(&text_v1, &[layer_v1], 0, None)
            .expect("kind query should load");

        // Shift down by one line: both ranges translate, geometry unchanged.
        let (text_v2, layer_v2) = make("AAAA\nBBBB\n");
        rig.store
            .update_document(rig.uri.clone(), text_v2.clone(), None);
        let (_, hash_v2) = super::super::captures_match_cache::tree_cache_key(
            "locals",
            "rust",
            &layer_v2.tree,
            &text_v2,
        );
        assert!(
            rig.match_cache.get_layer(&rig.uri, hash_v2, 0).is_some(),
            "the translated multi-range layer must hit the v1 store"
        );

        let (cached, _) = rig
            .walk(&text_v2, &[layer_v2], 1, None)
            .expect("kind query should load");
        // The later-range capture (`b`) must sit at its exact v2 position.
        let b_line = text_v2[..text_v2.find(part_b).unwrap()]
            .matches('\n')
            .count() as u64;
        let b = cached
            .iter()
            .flat_map(|m| m["captures"].as_array().unwrap())
            .find(|c| {
                c["range"]["start"]["line"] == b_line && c["range"]["start"]["character"] == 4
            });
        assert!(
            b.is_some(),
            "later-range capture must reanchor to line {b_line} col 4: {cached:?}"
        );
        // And the whole wire output must equal a fresh compute.
        rig.match_cache.clear_document(&rig.uri);
        let (fresh, _) = rig
            .walk(
                &text_v2,
                &[{
                    let (_, layer) = make("AAAA\nBBBB\n");
                    layer
                }],
                1,
                None,
            )
            .expect("kind query should load");
        assert_eq!(cached, fresh, "multi-range hit must match fresh compute");
    }

    /// A cancelled walk bails before the sweep: its (empty) touched set must
    /// not evict the live entries a completed walk stored. Guards the
    /// ordering of the cancel bail vs the sweep in `execute_captures_walk`.
    #[test]
    fn cancelled_walk_does_not_sweep_live_entries() {
        let code = "let a = 1;\n";
        let text = format!("AAAA\n{code}");
        let start = text.find(code).unwrap();
        let rig = MatchCacheRig::new("file:///match_cache_cancel_sweep.rs", &text);
        rig.walk(&text, &[rust_layer(&text, start, text.len())], 0, None)
            .expect("kind query should load");
        let (_, hash) = super::super::captures_match_cache::tree_cache_key(
            "locals",
            "rust",
            &rust_layer(&text, start, text.len()).tree,
            &text,
        );
        assert!(rig.match_cache.get_layer(&rig.uri, hash, 0).is_some());

        // A pre-cancelled walk visits nothing (touched set empty) and must
        // return None WITHOUT sweeping the entry away.
        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel();
        let cancelled = rig.walk_with(
            &text,
            &[rust_layer(&text, start, text.len())],
            0,
            None,
            true,
            Some(&cancel),
            0,
        );
        assert!(cancelled.is_none(), "a cancelled walk serves nothing");
        assert!(
            rig.match_cache.get_layer(&rig.uri, hash, 0).is_some(),
            "a cancelled walk must not sweep live entries"
        );
    }

    /// `injection = false` walks visit only the host layer (depth 0): the
    /// host slot must serve on exact content recurrence (undo/redo), pinned
    /// with a sentinel like the layer test above.
    #[test]
    fn host_slot_serves_on_content_recurrence() {
        let text = "let a = 1;\n";
        let rig = MatchCacheRig::new("file:///match_cache_host.rs", text);
        rig.walk_with(text, &[], 0, None, false, None, 0)
            .expect("kind query should load");

        // Host key for the same content: parse without included ranges.
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let host_tree = parser.parse(text, None).unwrap();
        let (anchor, host_hash) =
            super::super::captures_match_cache::tree_cache_key("locals", "rust", &host_tree, text);
        assert_eq!(anchor, 0, "host layer anchors at 0");
        assert!(
            rig.match_cache
                .get_host(&rig.uri, "locals", host_hash, 0)
                .is_some(),
            "the host walk must store under the host slot"
        );
        rig.match_cache.store_host(
            &rig.uri,
            "locals",
            host_hash,
            0,
            std::sync::Arc::new(vec![crate::language::query_exec::MatchData {
                pattern_index: 0,
                captures: vec![crate::language::query_exec::CapturedNode {
                    name: "sentinel-host-slot".to_string(),
                    start_byte: 4,
                    end_byte: 5,
                    kind: "identifier",
                    metadata: Vec::new(),
                }],
                metadata: Vec::new(),
            }]),
            || true,
        );

        // Same content again (undo/redo shape): must serve the host slot.
        let (matches, _) = rig
            .walk_with(text, &[], 0, None, false, None, 0)
            .expect("kind query should load");
        assert!(
            capture_names(&matches)
                .iter()
                .any(|n| n == "sentinel-host-slot"),
            "an unchanged host must serve the host slot, not re-execute"
        );
    }

    #[test]
    fn delta_identical_arrays_yield_no_edit() {
        assert_eq!(
            matches_delta_edit(&vals(&[1, 2, 3]), &vals(&[1, 2, 3])),
            None
        );
        assert_eq!(matches_delta_edit(&[], &[]), None);
    }

    #[test]
    fn delta_append() {
        let (start, del, data) = matches_delta_edit(&vals(&[1, 2]), &vals(&[1, 2, 3])).unwrap();
        assert_eq!((start, del), (2, 0));
        assert_eq!(data, vals(&[3]));
    }

    #[test]
    fn delta_prepend() {
        let (start, del, data) = matches_delta_edit(&vals(&[2, 3]), &vals(&[1, 2, 3])).unwrap();
        assert_eq!((start, del), (0, 0));
        assert_eq!(data, vals(&[1]));
    }

    #[test]
    fn delta_middle_replacement() {
        let (start, del, data) = matches_delta_edit(&vals(&[1, 2, 3]), &vals(&[1, 9, 3])).unwrap();
        assert_eq!((start, del), (1, 1));
        assert_eq!(data, vals(&[9]));
    }

    #[test]
    fn delta_removal() {
        let (start, del, data) = matches_delta_edit(&vals(&[1, 2, 3]), &vals(&[1, 3])).unwrap();
        assert_eq!((start, del), (1, 1));
        assert!(data.is_empty());
    }

    #[test]
    fn delta_clear_and_fill() {
        let (start, del, data) = matches_delta_edit(&vals(&[1, 2]), &[]).unwrap();
        assert_eq!((start, del, data.len()), (0, 2, 0));

        let (start, del, data) = matches_delta_edit(&[], &vals(&[1])).unwrap();
        assert_eq!((start, del), (0, 0));
        assert_eq!(data, vals(&[1]));
    }

    #[test]
    fn metadata_object_is_none_when_no_directives() {
        // Patterns without #set! must keep their pre-metadata wire shape —
        // an empty object would churn every delta lineage.
        assert_eq!(metadata_object(&[]), None);
    }

    #[test]
    fn metadata_object_flag_form_maps_to_true() {
        // (#set! key) without a value is a flag (e.g. injection.combined
        // style); JSON true lets clients test for it, where Neovim's nil
        // assignment would make the key undetectable.
        let pairs = vec![("combined".to_string(), None)];
        assert_eq!(metadata_object(&pairs), Some(json!({ "combined": true })));
    }

    #[test]
    fn metadata_object_duplicate_keys_last_write_wins() {
        // Neovim applies directives in order, so a later #set! of the same
        // key overwrites the earlier one.
        let pairs = vec![
            ("kind".to_string(), Some("first".to_string())),
            ("kind".to_string(), Some("second".to_string())),
        ];
        assert_eq!(metadata_object(&pairs), Some(json!({ "kind": "second" })));
    }

    #[test]
    fn kind_whitelist() {
        assert!(is_valid_kind("context"));
        assert!(is_valid_kind("my-kind_2"));
        assert!(!is_valid_kind(""));
        assert!(!is_valid_kind("../evil"));
        assert!(!is_valid_kind("a/b"));
        assert!(!is_valid_kind("a.scm"));
    }
}
