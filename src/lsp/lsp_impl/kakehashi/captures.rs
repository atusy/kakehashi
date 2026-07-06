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

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
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
fn metadata_object(pairs: Vec<(String, Option<String>)>) -> Option<Value> {
    if pairs.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        map.insert(key, value.map_or(Value::Bool(true), Value::String));
    }
    Some(Value::Object(map))
}

/// A kind query compiled for one language, with the patterns tolerant
/// compilation dropped.
struct KindQuery {
    query: tree_sitter::Query,
    skipped: Vec<crate::language::query_loader::SkippedPattern>,
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
    /// The live `content_version` at request entry. The lineage store re-checks
    /// it so an edit landing DURING the request voids the store: the response
    /// was already outdated at delivery, and lineage must not record matches
    /// for a text the client no longer has (ADR §3). A snapshot that merely
    /// *trailed* at entry still serves and stores — that is the deliberate
    /// serve-stale relaxation, healed by the client's per-keystroke re-request.
    entry_content_version: u64,
    matches: Vec<Value>,
    skipped: Vec<Value>,
}

impl Kakehashi {
    /// Handler for `kakehashi/captures/full`.
    pub async fn kakehashi_captures_full(&self, params: CapturesFullParams) -> Result<Value> {
        let computed = self
            .compute_captures(
                &params.text_document.uri,
                &params.kind,
                None,
                params.injection,
            )
            .await?;
        let Some(c) = computed else {
            return Ok(Value::Null);
        };
        let result_id = next_result_id();
        // `json!` serializes interpolated values **by reference**, so the
        // originals are still owned afterwards and move into the cache insert
        // below — exactly one materialization of the matches per request.
        let response = json!({
            "resultId": result_id,
            "matches": c.matches,
            "skipped": c.skipped,
        });
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
            return Ok(Value::Null);
        }
        Ok(response)
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
    ///   stored and the caller answers `null`. A snapshot that merely trailed
    ///   at entry still stores — the serve-stale relaxation.
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
    ) -> Result<Value> {
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
            return Ok(Value::Null);
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
            return Ok(Value::Null);
        };

        let computed = self
            .compute_captures(&params.text_document.uri, &params.kind, None, injection)
            .await?;
        let Some(c) = computed else {
            return Ok(Value::Null);
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
                return Ok(Value::Null);
            }
        }

        let result_id = next_result_id();
        // Diff against the cached slot while borrowing it in place — cloning
        // the previous matches would tax every delta request, the hot repeat
        // path. The read guard (the match scrutinee temporary) drops at the
        // end of this statement. The slot is re-verified against
        // `previousResultId` because another request for the same mode may
        // have advanced the lineage during the await above; if it did, fall
        // back to a full result. `json!` serializes by reference, so
        // `result_id` and `c.matches` stay owned and move into the store.
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
                let edits = match matches_delta_edit(prev_matches, &c.matches) {
                    None => json!([]),
                    Some((start, delete_count, data)) => json!([{
                        "start": start,
                        "deleteCount": delete_count,
                        "data": data,
                    }]),
                };
                json!({ "resultId": result_id, "edits": edits })
            }
            // Stale id (or lineage advanced during the await): full result
            // under the resolved mode (LSP convention — a server may always
            // answer a delta with a full).
            _ => json!({
                "resultId": result_id,
                "matches": c.matches,
                "skipped": c.skipped,
            }),
        };
        // Same gate as `full`: a delta whose fresh lineage cannot install
        // (text moved past the computed snapshot, or close/reopen) answers
        // `null` — the client re-acquires via `full` (its baseline slot may
        // have been superseded by the failed rotation's circumstances anyway).
        if !self.store_lineage(c, key.1, injection, result_id).await {
            return Ok(Value::Null);
        }
        Ok(response)
    }

    /// Handler for `kakehashi/captures/range`.
    ///
    /// No `resultId` — range results depend on the viewport, so there is no
    /// stable lineage to diff against (matching semanticTokens/range, which
    /// likewise has no delta variant).
    pub async fn kakehashi_captures_range(&self, params: CapturesRangeParams) -> Result<Value> {
        let computed = self
            .compute_captures(
                &params.text_document.uri,
                &params.kind,
                Some(params.range),
                params.injection,
            )
            .await?;
        Ok(match computed {
            Some(c) => json!({ "matches": c.matches, "skipped": c.skipped }),
            None => Value::Null,
        })
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

        // Resolve the latest parse snapshot, with only the bounded first-parse
        // wait (parse-snapshot ADR §3). `full` SERVES-STALE — it is a
        // per-keystroke highlighter, and `null`-on-routine-staleness against a
        // client contracted to re-request on `null` would busy-spin; `null` is
        // reserved for no-snapshot (pre-first-parse) and the lineage gate
        // below. The injected-grammar install is no longer triggered inline
        // here: the parse loop's downstream (`process_injections`) detects
        // missing injected grammars and spawns the install as a detached task;
        // a layer whose grammar is still installing is skipped by the walk and
        // heals on a later request.
        let Some(snapshot) = self.snapshot_for_tokens(&uri).await else {
            log::debug!(target: "kakehashi::captures", "no parsed document for {uri}");
            return Ok(None);
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
        // `range` is the position/range class: its byte range is authored
        // against the LIVE text, so a trailing snapshot cannot answer it.
        // `null` — the captures protocol's re-sync signal — not ContentModified
        // (a JSON-RPC error would violate the captures contract).
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
        let uri_for_walk = uri.clone();
        let kind = kind.to_string();
        let language = std::sync::Arc::clone(&self.language);
        let tracker = self.bridge.node_tracker_arc();
        let documents = std::sync::Arc::clone(&self.documents);
        let parsed_version = snapshot.parsed_version;
        let walked = self
            .compute_pool
            .run(None, move || {
                execute_captures_walk(
                    &uri_for_walk,
                    &kind,
                    lsp_range,
                    injection,
                    &language_id,
                    &text,
                    &tree,
                    &language,
                    &tracker,
                    &documents,
                    parsed_version,
                    incarnation,
                )
            })
            .await;
        let Some(walked) = walked else {
            return Ok(None);
        };
        Ok(walked.map(|(matches, skipped)| ComputedCaptures {
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
    language: &crate::language::LanguageCoordinator,
    tracker: &crate::language::NodeTracker,
    documents: &crate::document::DocumentStore,
    parsed_version: u64,
    incarnation: u64,
) -> Option<(Vec<Value>, Vec<Value>)> {
    // Tracker minting is currency-gated: the NodeTracker is a LIVE-coordinate
    // index (didChange shifts every entry before the reparse), so a trailing
    // snapshot's byte offsets must never be written into it — they would be
    // wrong-space entries that later edits keep shifting as if they were live
    // (the "a stale read never mints" invariant, see snapshot_read.rs). A
    // stale serve still returns matches — serve-stale is the point of the
    // per-keystroke `full` mode — but goes READ-ONLY on the tracker: a
    // position the intervening edits did not shift reuses its live id (so
    // the delta lineage stays a minimal diff), and an unknown position gets
    // an unregistered id — resolving one via `kakehashi/node/*` yields
    // `null`, the protocol's re-sync signal, and the client's next
    // current-snapshot request mints real ones. Checked here inside the
    // work-unit so the entry window is the walk itself, not the pool-queue
    // wait; the walk-duration residual — an edit landing DURING the walk —
    // is closed by the post-walk reconciliation below, which purges this
    // walk's own mints when the snapshot is no longer current on exit.
    // Latched BEFORE the currency check: an edit landing between the two
    // bumps the generation (making later mints purge-eligible) or fails the
    // currency check itself — either way no wrong-space mint survives. The
    // epoch half covers close/reopen, which REPLACES the per-URI index (its
    // generation restarts at 0).
    let (entry_shift_gen, entry_cleanup_epoch) = tracker.mint_epoch(uri);
    let mint_into_tracker = documents.latest_snapshot(uri).is_some_and(|view| {
        view.slot.current_incarnation == incarnation && view.content_version == parsed_version
    });
    // Ids THIS walk created (as opposed to reused), with the shift
    // generation each creation observed — the purge set for the post-walk
    // reconciliation.
    let mut minted_ids: Vec<(ulid::Ulid, u64)> = Vec::new();
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

    // One kind-query load per language per request: with many regions of
    // the same language (e.g. dozens of python blocks), the memo keeps
    // file IO + compilation at one per language, and yields each
    // language's `skipped` exactly once.
    let mut kind_queries: HashMap<String, KindQueryLoad> = HashMap::new();
    let mut matches: Vec<Value> = Vec::new();

    let mut visit = |layer_language: &str, layer_tree: &tree_sitter::Tree, depth: usize| {
        let entry = kind_queries
            .entry(layer_language.to_string())
            .or_insert_with(|| {
                load_kind_query(&registry, &search_paths, layer_language, &file_name)
            });
        let KindQueryLoad::Loaded(kind_query) = entry else {
            return;
        };
        for m in execute_query(&kind_query.query, layer_tree, text, byte_range.clone()) {
            let match_metadata = metadata_object(m.metadata);
            let captures: Vec<Value> = m
                .captures
                .into_iter()
                .filter_map(|c| {
                    // A capture whose bytes don't map to positions (corrupt
                    // span) is dropped rather than failing the whole request.
                    let start = mapper.byte_to_position(c.start_byte)?;
                    let end = mapper.byte_to_position(c.end_byte)?;
                    // Minted in the layer's depth, so the id resolves in
                    // its minting layer via kakehashi/node/* (per-layer
                    // Scope rule). Same mint as `mint_node_info`, done via
                    // the tracker handle since this runs off `self`. On a
                    // stale serve the tracker is read-only (see the
                    // currency gate above).
                    let ulid = if mint_into_tracker {
                        let (ulid, created, shift_gen) = tracker.get_or_create_in_layer_tracked(
                            uri,
                            c.start_byte,
                            c.end_byte,
                            c.kind,
                            depth,
                        );
                        // Recorded for the post-walk purge: if an edit lands
                        // mid-walk, entries created after its shift were
                        // minted in a superseded coordinate space and must
                        // not persist. Both `created` and the observed
                        // generation are determined atomically under the
                        // tracker's entry lock, so an id a concurrent,
                        // still-current request created (and handed out) is
                        // never mis-attributed here and wrongly purged.
                        if created {
                            minted_ids.push((ulid, shift_gen));
                        }
                        ulid
                    } else {
                        match tracker.lookup_in_layer(uri, c.start_byte, c.end_byte, c.kind, depth)
                        {
                            Some(live) => live,
                            // NOT Ulid::default() (the nil id): unregistered
                            // ids must still be unique per capture.
                            None => ulid::Ulid::new(),
                        }
                    };
                    let node = json!({ "id": ulid.to_string(), "kind": c.kind });
                    let mut capture = json!({
                        "name": c.name,
                        "node": node,
                        "range": { "start": start, "end": end },
                    });
                    // Capture-scoped `#set! @cap key value` metadata,
                    // only when the capture was annotated.
                    if let Some(meta) = metadata_object(c.metadata) {
                        capture["metadata"] = meta;
                    }
                    Some(capture)
                })
                .collect();
            // Mirror execute_query's invariant: clients never see an empty
            // match envelope, even when every capture span fails to map.
            if captures.is_empty() {
                continue;
            }
            let mut envelope = json!({
                "patternIndex": m.pattern_index,
                "language": layer_language,
                "captures": captures,
            });
            // Match-level `#set!` metadata, only when the pattern set any
            // (treesitter-directive-set!) — absent otherwise, so patterns
            // without directives keep their pre-metadata wire shape.
            if let Some(meta) = match_metadata {
                envelope["metadata"] = meta;
            }
            matches.push(envelope);
        }
    };

    if injection {
        walk_document_layers(
            language,
            language_id,
            text,
            tree,
            byte_range.as_ref(),
            &mut visit,
        );
    } else {
        visit(language_id, tree, 0);
    }

    // Post-walk reconciliation, the other half of the currency gate: the
    // entry latch cannot see an edit (or close/reopen) that lands DURING the
    // walk. A creation whose observed shift generation still equals the
    // entry latch — with the cleanup epoch also unchanged — is
    // correct-at-birth: no edit intervened, and later edits shift it like
    // any live entry. A generation mismatch means the mint landed AFTER some
    // edit's shift, i.e. in a superseded coordinate space — coordinate
    // comparison could not detect this once a second edit moves the
    // wrong-space entry again. An epoch mismatch means a close/reopen
    // replaced some index mid-walk (per-URI generations restart at 0, so
    // the generation alone could ABA); purging our own mints then is
    // conservative but bounded — one null re-sync. The response still
    // carries purged ids, which resolve `null` (the protocol's re-sync
    // signal), the same degradation as a stale-at-entry serve.
    if !minted_ids.is_empty() {
        let (_, exit_cleanup_epoch) = tracker.mint_epoch(uri);
        for (id, shift_gen) in &minted_ids {
            if *shift_gen != entry_shift_gen || exit_cleanup_epoch != entry_cleanup_epoch {
                tracker.remove_id(uri, id);
            }
        }
    }

    // The kind is "available" when at least one visited language COMPILED
    // the file — a host without a context.scm still surfaces the embedded
    // layers' contexts. A `Broken` file does not make the kind available
    // (a sole broken asset degrades to null, per the ADR), but its
    // diagnostics are surfaced below whenever another language yields a
    // result.
    if !kind_queries
        .values()
        .any(|load| matches!(load, KindQueryLoad::Loaded(_)))
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
            .filter_map(|(lang, load)| match load {
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

    fn first_node_id(matches: &[Value]) -> ulid::Ulid {
        matches[0]["captures"][0]["node"]["id"]
            .as_str()
            .expect("capture carries a node id")
            .parse()
            .expect("node id is a ULID")
    }

    /// The NodeTracker is a live-coordinate index; a walk over a snapshot
    /// that trails the live document must still serve matches (serve-stale)
    /// but must NOT register its ids in the tracker — a stale read never
    /// mints (see the currency gate in `execute_captures_walk`).
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
        let incarnation = store
            .latest_snapshot(&uri)
            .unwrap()
            .slot
            .current_incarnation;

        // Current (parsed_version == content_version == 0): minting is live.
        let tracker = NodeTracker::new();
        let (matches, _) = execute_captures_walk(
            &uri,
            "locals",
            None,
            false,
            "rust",
            text,
            &tree,
            &coordinator,
            &tracker,
            &store,
            0,
            incarnation,
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
            &coordinator,
            &tracker,
            &store,
            0,
            incarnation,
        )
        .expect("kind query should load");
        assert!(
            !stale_matches.is_empty(),
            "serve-stale still returns matches"
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
            &coordinator,
            &stale_tracker,
            &store,
            0,
            incarnation,
        )
        .expect("kind query should load");
        let id = first_node_id(&stale_matches);
        assert!(
            stale_tracker.lookup_node(&uri, &id).is_none(),
            "a stale serve must not mint into the live tracker"
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
        assert_eq!(metadata_object(vec![]), None);
    }

    #[test]
    fn metadata_object_flag_form_maps_to_true() {
        // (#set! key) without a value is a flag (e.g. injection.combined
        // style); JSON true lets clients test for it, where Neovim's nil
        // assignment would make the key undetectable.
        let pairs = vec![("combined".to_string(), None)];
        assert_eq!(metadata_object(pairs), Some(json!({ "combined": true })));
    }

    #[test]
    fn metadata_object_duplicate_keys_last_write_wins() {
        // Neovim applies directives in order, so a later #set! of the same
        // key overwrites the earlier one.
        let pairs = vec![
            ("kind".to_string(), Some("first".to_string())),
            ("kind".to_string(), Some("second".to_string())),
        ];
        assert_eq!(metadata_object(pairs), Some(json!({ "kind": "second" })));
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
