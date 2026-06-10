//! `kakehashi/captures/{full, full/delta, range}` — run a server-owned query
//! kind over a document (captures-protocol).
//!
//! The triple mirrors `textDocument/semanticTokens`: `full` computes every
//! match and hands out a `resultId`; `full/delta` answers a repeat request
//! with a positional single-edit diff over the matches array (or a full
//! result when the `previousResultId` is unknown — the standard LSP
//! fallback); `range` scopes the query walk to a viewport.
//!
//! The query itself is not sent by the client: `kind` names a per-language
//! asset resolved as `queries/<lang>/<kind>.scm` across the configured
//! `searchPaths`, through the same loader (`; inherits:`, tolerant
//! per-pattern compilation) used for highlights. Kinds are therefore
//! open-ended — defined by what files exist, not by an enum.
//!
//! Null vs. error (captures-protocol §"Null vs. error semantics"):
//! - JSON `null` for an unresolvable document, a kind with no query file for
//!   this language, or a query asset that compiles to nothing (warned —
//!   that is a configuration problem, mirroring broken highlight queries).
//! - JSON-RPC `InvalidParams` for a malformed `kind` (fails the character
//!   whitelist guarding the filesystem lookup).
//!
//! Scope is the **host** layer only in v1; result nodes are minted in layer 0.

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::{Error as JsonRpcError, Result};
use tower_lsp_server::ls_types::{Range, TextDocumentIdentifier, Uri};
use url::Url;

use crate::analysis::next_result_id;
use crate::language::query_exec::execute_query;
use crate::language::query_loader::QueryLoader;
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
}

/// Request parameters for `kakehashi/captures/full/delta`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturesDeltaParams {
    pub text_document: TextDocumentIdentifier,
    pub kind: String,
    /// The `resultId` from a prior `full` (or delta) response for the same
    /// `(textDocument, kind)`. Unknown / stale ids get a full result back.
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
            .compute_captures(&params.text_document.uri, &params.kind, None)
            .await?;
        let Some(c) = computed else {
            return Ok(Value::Null);
        };
        let result_id = next_result_id();
        // Clone what the delta cache needs up front; the originals then flow
        // into the response, keeping the ownership split explicit.
        self.captures_cache
            .insert((c.uri, params.kind), (result_id.clone(), c.matches.clone()));
        Ok(json!({
            "resultId": result_id,
            "matches": c.matches,
            "skipped": c.skipped,
        }))
    }

    /// Handler for `kakehashi/captures/full/delta`.
    ///
    /// Recomputes the current matches (delta saves wire bytes, not compute —
    /// same trade-off as semantic tokens) and diffs against the stored result
    /// when `previousResultId` matches; otherwise falls back to a full result.
    pub async fn kakehashi_captures_full_delta(
        &self,
        params: CapturesDeltaParams,
    ) -> Result<Value> {
        let computed = self
            .compute_captures(&params.text_document.uri, &params.kind, None)
            .await?;
        let Some(c) = computed else {
            return Ok(Value::Null);
        };

        let key = (c.uri, params.kind);
        let result_id = next_result_id();
        // Diff against the cached entry while borrowing it in place — cloning
        // the previous matches would tax every delta request, the hot repeat
        // path. The read guard (the match scrutinee temporary) drops at the
        // end of this statement, before the insert below touches the same
        // DashMap shard. Per-branch clones feed the response; the originals
        // move into the cache insert.
        let response = match self.captures_cache.get(&key) {
            Some(entry) if entry.value().0 == params.previous_result_id => {
                let edits = match matches_delta_edit(&entry.value().1, &c.matches) {
                    None => json!([]),
                    Some((start, delete_count, data)) => json!([{
                        "start": start,
                        "deleteCount": delete_count,
                        "data": data,
                    }]),
                };
                json!({ "resultId": result_id.clone(), "edits": edits })
            }
            // Unknown or stale previousResultId: full result (LSP convention —
            // a server may always answer a delta request with a full result).
            _ => json!({
                "resultId": result_id.clone(),
                "matches": c.matches.clone(),
                "skipped": c.skipped,
            }),
        };
        self.captures_cache.insert(key, (result_id, c.matches));
        Ok(response)
    }

    /// Handler for `kakehashi/captures/range`.
    ///
    /// No `resultId` — range results depend on the viewport, so there is no
    /// stable lineage to diff against (matching semanticTokens/range, which
    /// likewise has no delta variant).
    pub async fn kakehashi_captures_range(&self, params: CapturesRangeParams) -> Result<Value> {
        let computed = self
            .compute_captures(&params.text_document.uri, &params.kind, Some(params.range))
            .await?;
        Ok(match computed {
            Some(c) => json!({ "matches": c.matches, "skipped": c.skipped }),
            None => Value::Null,
        })
    }

    /// Shared pipeline: validate `kind`, resolve the document and its host
    /// grammar, load + compile `queries/<lang>/<kind>.scm` from the search
    /// paths, execute, and shape the wire JSON.
    ///
    /// `Err` only for a malformed `kind` (client bug); every "not currently
    /// resolvable" case — unknown document, kind file absent for this
    /// language, asset compiling to nothing — is `Ok(None)` → JSON `null`.
    async fn compute_captures(
        &self,
        lsp_uri: &Uri,
        kind: &str,
        lsp_range: Option<Range>,
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

        let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot()) else {
            log::debug!(target: "kakehashi::captures", "no parsed document for {uri}");
            return Ok(None);
        };
        let text = snapshot.text();
        let tree = snapshot.tree();

        // The host grammar the kind query is compiled against.
        let Some(language_id) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::captures", "no host language detected for {uri}");
            return Ok(None);
        };
        let Some(language) = self
            .language
            .language_registry_for_parallel()
            .get(&language_id)
        else {
            log::debug!(
                target: "kakehashi::captures",
                "host grammar {language_id} not loaded for {uri}"
            );
            return Ok(None);
        };

        // Resolve queries/<lang>/<kind>.scm on the configured search paths.
        // A missing file is the expected "no such kind for this language"
        // case; a file that compiles to nothing is a broken asset — warn.
        let search_paths = self.language.search_paths();
        let file_name = format!("{kind}.scm");
        let parsed = match QueryLoader::load_query_with_inheritance(
            &language,
            &search_paths,
            &language_id,
            &file_name,
        ) {
            Ok(parsed) => parsed,
            Err(err) => {
                // "No such kind for this language" is the expected common case
                // and stays quiet; a file that exists but fails to load (IO
                // error, broken `; inherits:` chain) is asset trouble and must
                // be visible (captures-protocol §"Null vs. error semantics").
                if QueryLoader::find_query_file(&search_paths, &language_id, &file_name).is_none() {
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
                return Ok(None);
            }
        };
        let Some(query) = parsed.query else {
            log::warn!(
                target: "kakehashi::captures",
                "{file_name} for {language_id} compiled to nothing: {:?}",
                parsed.failure_reason
            );
            return Ok(None);
        };

        let mapper = PositionMapper::new(text);

        // Range scoping: clamped conversion (like other viewport-shaped
        // requests) — an out-of-bounds position means "to the document edge",
        // not an error. An inverted range can't name any bytes → null.
        let byte_range = match lsp_range {
            None => None,
            Some(r) => {
                let start = mapper.position_to_byte_clamped(r.start);
                let end = mapper.position_to_byte_clamped(r.end);
                if start > end {
                    return Ok(None);
                }
                Some(start..end)
            }
        };

        let match_data = execute_query(&query, tree, text, byte_range);

        let matches: Vec<Value> = match_data
            .into_iter()
            .filter_map(|m| {
                let captures: Vec<Value> = m
                    .captures
                    .into_iter()
                    .filter_map(|c| {
                        // A capture whose bytes don't map to positions (corrupt
                        // span) is dropped rather than failing the whole request.
                        let start = mapper.byte_to_position(c.start_byte)?;
                        let end = mapper.byte_to_position(c.end_byte)?;
                        // Host layer (0): v1 runs against the host tree only.
                        let node = self.mint_node_info(&uri, 0, (c.start_byte, c.end_byte, c.kind));
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
                    return None;
                }
                Some(json!({ "patternIndex": m.pattern_index, "captures": captures }))
            })
            .collect();

        let skipped: Vec<Value> = parsed
            .skipped
            .into_iter()
            .map(|s| {
                json!({
                    "startLine": s.start_line,
                    "endLine": s.end_line,
                    "reason": s.error,
                })
            })
            .collect();

        Ok(Some(ComputedCaptures {
            uri,
            matches,
            skipped,
        }))
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
