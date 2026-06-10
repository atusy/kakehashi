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

/// A kind query compiled for one language, with the patterns tolerant
/// compilation dropped.
struct KindQuery {
    query: tree_sitter::Query,
    skipped: Vec<crate::language::query_loader::SkippedPattern>,
}

/// Resolve and compile `queries/<language_id>/<file_name>` for one language.
///
/// `None` covers both "no grammar loaded" and "no kind file / broken asset":
/// the caller treats every `None` as "this layer contributes nothing".
/// Logging separates the expected from the troubling: a missing file is the
/// common "no such kind for this language" case (debug); a file that exists
/// but fails to load or compiles to nothing is asset trouble (warn) —
/// captures-protocol §"Null vs. error semantics".
fn load_kind_query(
    registry: &LanguageRegistry,
    search_paths: &[PathBuf],
    language_id: &str,
    file_name: &str,
) -> Option<KindQuery> {
    let language = registry.get(language_id)?;
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
            return None;
        }
    };
    let Some(query) = parsed.query else {
        log::warn!(
            target: "kakehashi::captures",
            "{file_name} for {language_id} compiled to nothing: {:?}",
            parsed.failure_reason
        );
        return None;
    };
    Some(KindQuery {
        query,
        skipped: parsed.skipped,
    })
}

/// Outcome of the shared resolution + execution pipeline: the resolved `Url`
/// (cache key half) and the wire-shaped `matches` / `skipped` arrays.
struct ComputedCaptures {
    uri: Url,
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
        // Skip caching when the document closed while this request was in
        // flight — didClose clears the cache, and an unconditional insert
        // would resurrect a stale lineage entry for the closed document. A
        // close racing past this check leaves at most one entry, which the
        // next didClose for the URI clears again.
        if self.documents.get(&c.uri).is_some() {
            self.captures_cache.insert(
                (c.uri, params.kind),
                (result_id, params.injection, c.matches),
            );
        }
        Ok(response)
    }

    /// Handler for `kakehashi/captures/full/delta`.
    ///
    /// Recomputes the current matches under the lineage's stored `injection`
    /// mode (delta saves wire bytes, not compute — same trade-off as semantic
    /// tokens) and diffs against the stored result when `previousResultId`
    /// matches; a stale id falls back to a full result under the stored mode.
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

        // The lineage carries the inherited injection mode. Without it the
        // mode is unknowable — a full fallback would have to guess, and a
        // host-only guess would silently serve an injection client the wrong
        // layer set — so a lineage-less delta is `null` ("re-acquire via
        // full"), the protocol's one deliberate deviation from semanticTokens'
        // always-full fallback (captures-protocol §"Delta semantics").
        let Some(injection) = self.captures_cache.get(&key).map(|e| e.value().1) else {
            return Ok(Value::Null);
        };

        let computed = self
            .compute_captures(&params.text_document.uri, &params.kind, None, injection)
            .await?;
        let Some(c) = computed else {
            return Ok(Value::Null);
        };

        let result_id = next_result_id();
        // Diff against the cached entry while borrowing it in place — cloning
        // the previous matches would tax every delta request, the hot repeat
        // path. The read guard (the match scrutinee temporary) drops at the
        // end of this statement, before the insert below touches the same
        // DashMap shard. `json!` serializes by reference, so `result_id` and
        // `c.matches` stay owned here and move into the cache insert.
        let response = match self.captures_cache.get(&key) {
            Some(entry) if entry.value().0 == params.previous_result_id => {
                let edits = match matches_delta_edit(&entry.value().2, &c.matches) {
                    None => json!([]),
                    Some((start, delete_count, data)) => json!([{
                        "start": start,
                        "deleteCount": delete_count,
                        "data": data,
                    }]),
                };
                json!({ "resultId": result_id, "edits": edits })
            }
            // Stale previousResultId: full result under the stored mode (LSP
            // convention — a server may always answer a delta with a full).
            _ => json!({
                "resultId": result_id,
                "matches": c.matches,
                "skipped": c.skipped,
            }),
        };
        // Same didClose-race guard as `full`: never resurrect lineage state
        // for a document that closed while this request was in flight.
        if self.documents.get(&key.0).is_some() {
            self.captures_cache
                .insert(key, (result_id, injection, c.matches));
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
        }

        let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot()) else {
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
        let mut kind_queries: HashMap<String, Option<KindQuery>> = HashMap::new();
        let mut matches: Vec<Value> = Vec::new();

        let mut visit = |layer_language: &str, layer_tree: &tree_sitter::Tree, depth: usize| {
            let entry = kind_queries
                .entry(layer_language.to_string())
                .or_insert_with(|| {
                    load_kind_query(&registry, &search_paths, layer_language, &file_name)
                });
            let Some(kind_query) = entry else {
                return;
            };
            for m in execute_query(&kind_query.query, layer_tree, text, byte_range.clone()) {
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
                        Some(json!({
                            "name": c.name,
                            "node": node,
                            "range": { "start": start, "end": end },
                        }))
                    })
                    .collect();
                // Mirror execute_query's invariant: clients never see an empty
                // match envelope, even when every capture span fails to map.
                if captures.is_empty() {
                    continue;
                }
                matches.push(json!({
                    "patternIndex": m.pattern_index,
                    "language": layer_language,
                    "captures": captures,
                }));
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

        // The kind is "available" when at least one visited language has the
        // file — a host without a context.scm still surfaces the embedded
        // layers' contexts. No language having it collapses to null.
        if kind_queries.values().all(Option::is_none) {
            return Ok(None);
        }

        let skipped: Vec<Value> = kind_queries
            .iter()
            .filter_map(|(lang, kq)| kq.as_ref().map(|kq| (lang, kq)))
            .flat_map(|(lang, kq)| {
                kq.skipped.iter().map(move |s| {
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
    fn kind_whitelist() {
        assert!(is_valid_kind("context"));
        assert!(is_valid_kind("my-kind_2"));
        assert!(!is_valid_kind(""));
        assert!(!is_valid_kind("../evil"));
        assert!(!is_valid_kind("a/b"));
        assert!(!is_valid_kind("a.scm"));
    }
}
