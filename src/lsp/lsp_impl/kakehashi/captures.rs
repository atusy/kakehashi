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
use crate::lsp::lsp_impl::kakehashi::node::injection_stack::{
    collect_injection_languages_in_document, walk_document_layers,
};
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
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        let value = value.as_ref().map_or(json!(true), |v| json!(v));
        map.insert(key.clone(), value);
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
/// (cache key half), the document's open generation at snapshot time (so the
/// lineage store can detect a close-then-reopen racing the request), and the
/// wire-shaped `matches` / `skipped` arrays.
struct ComputedCaptures {
    uri: Url,
    open_generation: u64,
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
        self.store_lineage(c, params.kind, params.injection, result_id)
            .await;
        Ok(response)
    }

    /// Store a fresh lineage in this mode's slot, unless the document closed
    /// (or closed-and-reopened) while the request was in flight.
    ///
    /// Two guards, both under the same per-URI edit lock `didClose` holds
    /// around its document removal:
    /// - liveness: a close that won the lock first leaves the document gone,
    ///   so the check skips the insert; an insert that won first is cleared by
    ///   the close's cache cleanup, which runs after the removal;
    /// - generation: a close **followed by a fast reopen** makes the URI look
    ///   alive again, but the reopened document draws a fresh open generation,
    ///   so a stale request's snapshot-time generation no longer matches and
    ///   the insert is skipped — a reopened document starts its lineage fresh
    ///   (captures-protocol §"Delta semantics").
    async fn store_lineage(
        &self,
        computed: ComputedCaptures,
        kind: String,
        injection: bool,
        result_id: String,
    ) {
        let uri = computed.uri;
        let edit_lock = self.documents.edit_lock(&uri);
        let _guard = edit_lock.lock().await;
        if self.documents.get(&uri).is_some()
            && self.documents.open_generation(&uri) == computed.open_generation
        {
            let mut entry = self.captures_cache.entry((uri, kind)).or_default();
            entry[usize::from(injection)] = Some((result_id, computed.matches));
        }
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
        self.store_lineage(c, key.1, injection, result_id).await;
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

        // Same didOpen→async-parse race the node handlers guard against.
        self.ensure_parsed_for_node_lookup(&uri).await;

        let Some(language_id) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::captures", "no host language detected for {uri}");
            return Ok(None);
        };

        // Injection layers need their grammars loaded before the walk can
        // parse them. The pre-await snapshot is only used for *discovery* —
        // it must NOT feed minting: a didChange processed during the await
        // would adjust the tracker while our byte ranges stay stale. We
        // re-snapshot below, after the await (mirroring `kakehashi/node`).
        if injection && let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot())
        {
            self.ensure_injection_languages_loaded_for_document(
                &uri,
                &language_id,
                snapshot.text(),
                snapshot.tree(),
            )
            .await;
            // A didChange processed during the await above may have scheduled
            // a reparse; wait for it like the initial prelude does, so the
            // snapshot below is the settled (text, tree) pair and never a
            // mid-parse combination.
            self.ensure_parsed_for_node_lookup(&uri).await;
        }

        // Fetch the open generation BEFORE snapshotting: if a close+reopen
        // races in between, the stale generation makes store_lineage's
        // comparison fail (conservative skip). The reverse order could pair
        // an old snapshot with the reopened document's generation and seed
        // the new document with stale lineage.
        let open_generation = self.documents.open_generation(&uri);
        let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot()) else {
            // The generation ask above may have lazily created an entry for a
            // URI that a racing didClose just removed; drop it again so
            // closed URIs don't accumulate entries (safety never depended on
            // it — the counter is monotonic).
            self.documents.forget_open_generation_if_closed(&uri);
            log::debug!(target: "kakehashi::captures", "no parsed document for {uri}");
            return Ok(None);
        };
        let text = snapshot.text();
        let tree = snapshot.tree();

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

        let registry = self.language.language_registry_for_parallel();
        let search_paths = self.language.search_paths();
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
                let match_metadata = metadata_object(&m.metadata);
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
                        // Scope rule).
                        let node =
                            self.mint_node_info(&uri, depth, (c.start_byte, c.end_byte, c.kind));
                        let mut capture = json!({
                            "name": c.name,
                            "node": node,
                            "range": { "start": start, "end": end },
                        });
                        // Capture-scoped `#set! @cap key value` metadata,
                        // only when the capture was annotated.
                        if let Some(meta) = metadata_object(&c.metadata) {
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
                &self.language,
                &language_id,
                text,
                tree,
                byte_range.as_ref(),
                &mut visit,
            );
        } else {
            visit(&language_id, tree, 0);
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
            return Ok(None);
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

        Ok(Some(ComputedCaptures {
            uri,
            open_generation,
            matches,
            skipped,
        }))
    }

    /// Ensure the grammars for every injection language appearing in the
    /// document are loaded (auto-installing where configured), iterating to a
    /// fixpoint: each round can only discover one tier deeper than the
    /// grammars available in the previous round. Document-wide analog of
    /// `kakehashi/node`'s position-based `ensure_injection_languages_loaded`
    /// (see entry.rs for the registry re-read and `seen` rationale).
    async fn ensure_injection_languages_loaded_for_document(
        &self,
        uri: &Url,
        host_language: &str,
        text: &str,
        host_tree: &tree_sitter::Tree,
    ) {
        use std::collections::HashSet;

        let coordinator = self.injection_coordinator();
        let mut seen: HashSet<String> = HashSet::new();

        for _round in 0..crate::language::injection::MAX_INJECTION_DEPTH {
            let discovered = collect_injection_languages_in_document(
                &self.language,
                host_language,
                text,
                host_tree,
            );
            let registry = self.language.language_registry_for_parallel();
            let fresh: HashSet<String> = discovered
                .into_iter()
                .filter(|lang| registry.get(lang).is_none())
                .filter(|lang| !seen.contains(lang))
                .collect();
            drop(registry);
            if fresh.is_empty() {
                break;
            }
            coordinator
                .check_injected_languages_auto_install(uri, &fresh)
                .await;
            seen.extend(fresh);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(ns: &[i64]) -> Vec<Value> {
        ns.iter().map(|n| json!(n)).collect()
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
        let pairs = [("combined".to_string(), None)];
        assert_eq!(metadata_object(&pairs), Some(json!({ "combined": true })));
    }

    #[test]
    fn metadata_object_duplicate_keys_last_write_wins() {
        // Neovim applies directives in order, so a later #set! of the same
        // key overwrites the earlier one.
        let pairs = [
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
