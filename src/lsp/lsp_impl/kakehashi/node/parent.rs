//! `kakehashi/node/parent` — id → immediate-parent NodeInfo (ADR-0025).
//!
//! Resolves a previously-issued ULID to its tracked `(start_byte, end_byte, kind)`
//! triple, locates the matching tree-sitter node in the current parse tree, and
//! returns a [`NodeInfo`](../../../../../docs/adr/0025-node-reference-protocol.md#nodeinfo-type)
//! for its tree-sitter parent.
//!
//! Per ADR-0025 §"Navigation Methods", navigation stays within a single language
//! tree: calling `parent` on the root of an injected tree must **not** cross
//! into the host node that contains the injection. This handler currently only
//! operates on the host tree (matching PR-1); PR-4 will extend the protocol with
//! explicit injection-layer addressing.
//!
//! Returns `null` (serialized as JSON `null`) when:
//! - the URI is unknown or invalid,
//! - the ULID is malformed or was never issued / has been invalidated,
//! - the document has not yet been parsed,
//! - the tracked range cannot be matched against a node in the current tree
//!   (defensive: should not happen while the tracker is in sync), or
//! - the matched node is the root of the tree (no parent).

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::TextDocumentIdentifier;
use ulid::Ulid;

use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};

/// Request parameters for `kakehashi/node/parent`.
///
/// `pub` because the handler is registered as a custom LSP method in the
/// `kakehashi` binary (see `src/bin/main.rs`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeParentParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
}

impl Kakehashi {
    /// Handler for `kakehashi/node/parent`.
    pub async fn kakehashi_node_parent(&self, params: NodeParentParams) -> Result<Value> {
        let lsp_uri = params.text_document.uri;
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::node::parent", "invalid URI: {}", lsp_uri.as_str());
            return Ok(Value::Null);
        };

        // Malformed ULID collapses to null per ADR-0025 §"Invalidate vs Not-Found".
        let Ok(ulid) = params.id.parse::<Ulid>() else {
            return Ok(Value::Null);
        };

        // Look up the tracked node's byte range and kind. None means: never
        // issued, invalidated by a prior edit, or this URI has no entries.
        let Some((start, end, kind)) = self.bridge.node_tracker().lookup_position(&uri, &ulid)
        else {
            return Ok(Value::Null);
        };

        // Ensure the document is parsed before snapshotting — same race as in
        // `kakehashi/node`: didOpen schedules an async parse, a client that
        // immediately follows up with `parent` must not see `tree: None`.
        self.ensure_parsed_for_node_lookup(&uri).await;

        // Snapshot the document so we operate on a consistent (text, tree) pair.
        let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot()) else {
            log::debug!(target: "kakehashi::node::parent", "no parsed document for {}", uri);
            return Ok(Value::Null);
        };
        let tree = snapshot.tree();

        // Find the tree-sitter node matching the tracked (start, end, kind).
        // Defensive: the tracker stays in sync with didChange, so this should
        // always succeed for a non-null lookup.
        let Some(node) = find_node_at(tree, start, end, &kind) else {
            log::warn!(
                target: "kakehashi::node::parent",
                "tracker hit but no matching node in tree for ulid={} uri={} range=[{},{}) kind={}",
                ulid, uri, start, end, kind
            );
            return Ok(Value::Null);
        };

        // ADR-0025 "Scope rule": parent navigation stays within a single tree.
        // tree-sitter's `node.parent()` returns None for the tree root, which is
        // the exact semantics we want — do NOT chase into an enclosing host
        // injection node.
        let Some(parent) = node.parent() else {
            return Ok(Value::Null);
        };

        // Issue / reuse a stable ULID for the parent (ADR-0019 lazy assignment).
        let parent_ulid = self.bridge.node_tracker().get_or_create(
            &uri,
            parent.start_byte(),
            parent.end_byte(),
            parent.kind(),
        );

        Ok(json!({
            "id": parent_ulid.to_string(),
            "type": parent.kind(),
        }))
    }
}

/// Find a tree-sitter node in `tree` whose `(start_byte, end_byte, kind)` matches
/// the tracked triple.
///
/// Tree-sitter does not expose a direct "find node by composite key" API, so we
/// start from the smallest descendant covering `[start, end)` (which is the
/// usual case for a tracked node) and walk up to the root looking for an exact
/// match. The walk is bounded by tree depth and runs in O(depth) time.
fn find_node_at<'tree>(
    tree: &'tree tree_sitter::Tree,
    start: usize,
    end: usize,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    // Defensive: reject obviously-invalid ranges before handing them to
    // tree-sitter. A stale tracker entry (or future bug) could pass a range
    // outside the parsed tree's bounds; the underlying C bindings have, at
    // various tree-sitter versions, exhibited surprising behavior for such
    // inputs. Returning None preserves the caller's null-collapse semantics.
    let root = tree.root_node();
    if start > end || end > root.end_byte() {
        return None;
    }

    // `descendant_for_byte_range(start, end)` returns the smallest node whose
    // byte range contains `[start, end)`. For a tracked node, that's the node
    // itself; for stale ranges it returns the containing node, in which case
    // the upward walk below will fail to find a match (and we return None).
    let mut current = root.descendant_for_byte_range(start, end)?;

    loop {
        if current.start_byte() == start && current.end_byte() == end && current.kind() == kind {
            return Some(current);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}
