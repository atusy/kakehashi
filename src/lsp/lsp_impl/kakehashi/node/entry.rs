//! `kakehashi/node` — position → NodeInfo entry point (ADR-0025).
//!
//! Resolves a `Position` to the smallest tree-sitter node (named or anonymous)
//! containing that byte under the **host** language. Injection-aware lookup is
//! deferred to PR-4; for now, any client-supplied `injection` value is accepted
//! but ignored — the handler always returns the host-language node.
//!
//! Returns `null` (serialized as JSON `null`) when:
//! - the URI is unknown,
//! - the document has not yet been parsed (no tree),
//! - the position cannot be converted to a byte offset,
//! - the position is outside the document (`b > L`), or
//! - the document is empty (`L == 0`).

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Position, TextDocumentIdentifier};
use url::Url;

use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};
use crate::text::PositionMapper;

/// Request parameters for `kakehashi/node`.
///
/// The `injection` field is parsed but ignored in PR-1; see module-level doc.
///
/// `pub` is required because `Kakehashi::kakehashi_node` is registered as a
/// custom LSP method in the `kakehashi` binary, which lives outside the
/// library crate's visibility scope.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    /// Reserved for PR-4 (`boolean | number`). Accepted but ignored in PR-1.
    #[serde(default)]
    pub injection: Option<Value>,
}

impl Kakehashi {
    /// Handler for `kakehashi/node`.
    pub async fn kakehashi_node(&self, params: NodeParams) -> Result<Value> {
        let lsp_uri = params.text_document.uri;
        let position = params.position;

        // URI conversion failure → null (ADR-0025 universal null semantics).
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::node", "invalid URI: {}", lsp_uri.as_str());
            return Ok(Value::Null);
        };

        // Ensure the document is parsed before snapshotting. `didOpen` inserts the
        // document with `tree: None` and schedules an async parse; a client that
        // calls `kakehashi/node` quickly afterwards must not race with that parse.
        self.ensure_parsed_for_node_lookup(&uri).await;

        // Snapshot the document so we hold the read lock for as short as possible.
        let snapshot = match self.documents.get(&uri).and_then(|doc| doc.snapshot()) {
            Some(s) => s,
            None => {
                log::debug!(target: "kakehashi::node", "no parsed document for {}", uri);
                return Ok(Value::Null);
            }
        };

        let text = snapshot.text();
        let tree = snapshot.tree();
        let mapper = PositionMapper::new(text);

        // Empty document: end-of-document exception is gated on `L > 0`,
        // and any position is either at byte 0 (no node spans `[0, 0)`) or
        // out of bounds. ADR-0025 explicitly says empty documents return null.
        let doc_len = text.len();
        if doc_len == 0 {
            return Ok(Value::Null);
        }

        // Convert LSP position (UTF-16 code units) to a UTF-8 byte offset.
        let Some(byte) = mapper.position_to_byte(position) else {
            return Ok(Value::Null);
        };
        if byte > doc_len {
            return Ok(Value::Null);
        }

        // Resolve smallest containing node under the host tree.
        let Some(node) = smallest_containing_node(tree, byte, doc_len) else {
            return Ok(Value::Null);
        };

        // Issue / reuse a stable ULID for this node via the NodeTracker.
        let ulid = self.bridge.node_tracker().get_or_create(
            &uri,
            node.start_byte(),
            node.end_byte(),
            node.kind(),
        );

        Ok(json!({
            "id": ulid.to_string(),
            "type": node.kind(),
        }))
    }

    /// Parse the document on-demand if its tree has not been built yet.
    ///
    /// `didOpen` inserts the document immediately with `tree: None` and
    /// schedules an asynchronous parse. `kakehashi/node` requests issued
    /// straight after `didOpen` would otherwise race with that parse and
    /// see `snapshot()` return `None`. This helper mirrors the on-demand
    /// parsing path used by `selection_range_impl`: load the language,
    /// parse via the shared pool, and update the document store atomically.
    ///
    /// Race protection: an in-flight `didChange` parse sets `has_tree=false`
    /// via `mark_parse_started` while the old `Document::tree()` may briefly
    /// remain populated. Waiting on `wait_for_parse_completion` first guarantees
    /// the snapshot returned by the caller is the *current* (text, tree) pair,
    /// not a stale combination produced mid-parse. The timeout matches the
    /// `semantic_tokens` budget so this helper stays responsive even if the
    /// parser hangs on a pathological input.
    async fn ensure_parsed_for_node_lookup(&self, uri: &Url) {
        self.documents
            .wait_for_parse_completion(uri, std::time::Duration::from_millis(200))
            .await;

        // If a tree is now available (either it always was, or didChange's
        // parse just finished), nothing to do.
        if let Some(doc) = self.documents.get(uri)
            && doc.tree().is_some()
        {
            return;
        }

        let Some(language_name) = self.document_language(uri) else {
            return;
        };

        let load_result = self.language.ensure_language_loaded(&language_name);
        if !load_result.success {
            return;
        }

        // Take a fresh read to grab the current text (the doc may still be missing a tree).
        let Some(doc) = self.documents.get(uri) else {
            return;
        };
        let text = doc.text().to_string();
        drop(doc);

        let text_clone = text.clone();
        let parsed = self
            .parse_coordinator()
            .parse_with_pool(&language_name, uri, text.len(), move |mut parser| {
                let tree = parser.parse(&text_clone, None);
                (parser, tree)
            })
            .await;

        if let Some(tree) = parsed {
            // Race guard: between the text snapshot above and parse completion,
            // a didChange may have updated the document. Storing our tree would
            // associate it with stale text, breaking the (text, tree) consistency
            // invariant. Compare against the current text and discard if it has
            // moved; the next request will re-trigger the parse against the
            // newer text.
            let text_unchanged = self
                .documents
                .get(uri)
                .map(|doc| doc.text() == text)
                .unwrap_or(false);
            if text_unchanged {
                self.documents
                    .update_document(uri.clone(), text, Some(tree));
            } else {
                log::debug!(
                    target: "kakehashi::node",
                    "discarding on-demand parse for {} — text changed during parse",
                    uri
                );
            }
        }
    }
}

/// Find the smallest node containing `byte` under the half-open `[start, end)` rule,
/// with the ADR-0025 end-of-document exception.
///
/// PR-1 only honours the exception case at the document end; the rest of the
/// lookup uses tree-sitter's `descendant_for_byte_range(byte, byte)`, which
/// already returns the smallest containing node when given equal start/end.
fn smallest_containing_node(
    tree: &tree_sitter::Tree,
    byte: usize,
    doc_len: usize,
) -> Option<tree_sitter::Node<'_>> {
    let root = tree.root_node();

    // End-of-document exception (ADR-0025 §"End-of-Document Exception"):
    //   gated on doc_len > 0. The empty-document path returns null earlier.
    if byte == doc_len {
        // Pick the smallest descendant whose end_byte == doc_len.
        // Tree-sitter's `descendant_for_byte_range(L, L)` returns None at end-of-document
        // because no node strictly contains the past-the-end byte. Walk the right spine
        // of the root manually instead.
        //
        // Guard against pathological trees whose root end_byte < doc_len (trailing
        // bytes that the parser failed to attach to any node, e.g. an unparsed
        // tail after an error). In that case there is no node whose end coincides
        // with the document end and the exception cannot apply.
        let candidate = deepest_node_ending_at(root, doc_len);
        return if candidate.end_byte() == doc_len {
            Some(candidate)
        } else {
            None
        };
    }

    // Standard half-open lookup: smallest node with start_byte <= byte < end_byte.
    let node = root.descendant_for_byte_range(byte, byte)?;

    // Defensive check: tree-sitter may return a node whose end_byte equals `byte`
    // when there is no smaller descendant — half-open semantics say such a cursor
    // is *outside* that node. Walk up until we find one that properly contains it,
    // or fall back to null.
    let mut current = Some(node);
    while let Some(n) = current {
        if n.start_byte() <= byte && byte < n.end_byte() {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// Walk down the right spine of `node`, returning the deepest descendant whose
/// `end_byte` equals `target_end`. Used for the end-of-document exception.
///
/// Implementation note: tree-sitter direct siblings are non-overlapping with
/// monotonically non-decreasing `end_byte`, so among children only the LAST
/// can match `target_end == parent.end_byte()`. Navigating to the last child
/// via a `TreeCursor` is O(children-per-level), giving overall O(depth × max
/// breadth) instead of the O(N²) `current.child(i).rev()` pattern.
fn deepest_node_ending_at(node: tree_sitter::Node<'_>, target_end: usize) -> tree_sitter::Node<'_> {
    let mut cursor = node.walk();
    let mut current = node;
    while cursor.goto_first_child() {
        while cursor.goto_next_sibling() {}
        let last_child = cursor.node();
        if last_child.end_byte() == target_end {
            current = last_child;
        } else {
            break;
        }
    }
    current
}
