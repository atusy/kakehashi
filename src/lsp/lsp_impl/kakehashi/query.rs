//! `kakehashi/query` — run a client-supplied tree-sitter query over a document
//! (query-execution-protocol).
//!
//! The handler is a thin adapter over [`run_query`](crate::language::query_exec::run_query):
//! resolve the document and its host grammar, run the query, then shape each
//! capture into the protocol's wire form — a trackable `NodeInfo` plus an inline
//! LSP `Range` so a bulk result needs no per-capture follow-up call.
//!
//! Scope is the **host** layer only in v1 (query-execution-protocol §"Scope"):
//! a query string is compiled against one grammar, so injected-language querying
//! waits on a way for clients to discover each layer's language. Result nodes are
//! therefore minted in layer 0.
//!
//! Null vs. error (query-execution-protocol §"Null vs. error semantics"):
//! - JSON `null` for an unresolvable document — unknown URI, not-yet-parsed, or
//!   no detectable host language — matching node-reference-protocol's null.
//! - a JSON-RPC `InvalidParams` error when the `query` string itself cannot be
//!   compiled, since a malformed query is an actionable client bug.

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::{Error as JsonRpcError, Result};
use tower_lsp_server::ls_types::TextDocumentIdentifier;
use url::Url;

use crate::language::query_exec::{QueryExecOutcome, run_query};
use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};
use crate::text::PositionMapper;

/// Server safety cap on matches returned when the client gives no `matchLimit`,
/// so a broad query over a large file cannot produce an unbounded response.
/// Clients that need more pass an explicit larger `matchLimit`.
const DEFAULT_MATCH_LIMIT: usize = 10_000;

/// Request parameters for `kakehashi/query`.
///
/// `pub` because the handler is registered as a custom LSP method in the
/// `kakehashi` binary (see `src/bin/main.rs`), outside the library crate.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryParams {
    pub text_document: TextDocumentIdentifier,
    /// Tree-sitter S-expression query (e.g. the body of a `context.scm`),
    /// compiled against the document's host grammar.
    pub query: String,
    /// Optional cap on the number of matches returned. Absent → the server's
    /// [`DEFAULT_MATCH_LIMIT`].
    #[serde(default)]
    pub match_limit: Option<usize>,
}

impl Kakehashi {
    /// Handler for `kakehashi/query`.
    pub async fn kakehashi_query(&self, params: QueryParams) -> Result<Value> {
        let lsp_uri = params.text_document.uri;

        // URI conversion failure → null (universal null, as node-reference-protocol).
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::query", "invalid URI: {}", lsp_uri.as_str());
            return Ok(Value::Null);
        };

        // Same didOpen→async-parse race the node handlers guard against.
        self.ensure_parsed_for_node_lookup(&uri).await;

        let snapshot = match self.documents.get(&uri).and_then(|doc| doc.snapshot()) {
            Some(s) => s,
            None => {
                log::debug!(target: "kakehashi::query", "no parsed document for {uri}");
                return Ok(Value::Null);
            }
        };
        let text = snapshot.text();
        let tree = snapshot.tree();

        // The host grammar the client's query is compiled against.
        let Some(language_id) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::query", "no host language detected for {uri}");
            return Ok(Value::Null);
        };
        let Some(language) = self
            .language
            .language_registry_for_parallel()
            .get(&language_id)
        else {
            log::debug!(
                target: "kakehashi::query",
                "host grammar {language_id} not loaded for {uri}"
            );
            return Ok(Value::Null);
        };

        let limit = params.match_limit.or(Some(DEFAULT_MATCH_LIMIT));
        match run_query(&language, tree, text, &params.query, limit) {
            Ok(outcome) => Ok(self.shape_query_outcome(&uri, text, outcome)),
            Err(err) => {
                // A malformed query is a client bug — surface it, don't hide it
                // behind null. The compile reason aids debugging the .scm.
                log::debug!(
                    target: "kakehashi::query",
                    "query compile failed for {uri}: {}", err.reason
                );
                Err(JsonRpcError::invalid_params(format!(
                    "query compilation failed: {}",
                    err.reason
                )))
            }
        }
    }

    /// Shape a [`QueryExecOutcome`] into the protocol's JSON: matches grouped by
    /// pattern, each capture carrying a `NodeInfo` (minted in the host layer) and
    /// an inline LSP `Range`.
    fn shape_query_outcome(&self, uri: &Url, text: &str, outcome: QueryExecOutcome) -> Value {
        let mapper = PositionMapper::new(text);

        let matches: Vec<Value> = outcome
            .matches
            .into_iter()
            .map(|m| {
                let captures: Vec<Value> = m
                    .captures
                    .into_iter()
                    .filter_map(|c| {
                        // A capture whose bytes don't map to positions (corrupt
                        // span) is dropped rather than failing the whole query.
                        let start = mapper.byte_to_position(c.start_byte)?;
                        let end = mapper.byte_to_position(c.end_byte)?;
                        // Host layer (0): v1 runs against the host tree only.
                        let node = self.mint_node_info(uri, 0, (c.start_byte, c.end_byte, c.kind));
                        Some(json!({
                            "name": c.name,
                            "node": node,
                            "range": { "start": start, "end": end },
                        }))
                    })
                    .collect();
                json!({ "patternIndex": m.pattern_index, "captures": captures })
            })
            .collect();

        let skipped: Vec<Value> = outcome
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

        json!({ "matches": matches, "skipped": skipped })
    }
}
