//! `kakehashi/node/children` — id → immediate-children NodeInfo array (ADR-0025).
//!
//! Resolves a previously-issued ULID to its tracked `(start_byte, end_byte, kind)`
//! triple, locates the matching tree-sitter node in the current parse tree, and
//! returns one [`NodeInfo`](../../../../../docs/adr/0025-node-reference-protocol.md#nodeinfo-type)
//! per immediate child in **document order** (ascending `start_byte`).
//!
//! Per ADR-0025 §"Navigation Methods":
//! - Children include **both named and anonymous** nodes. A future `namedOnly`
//!   parameter may filter, but PR-3 returns the full child list.
//! - The order is tree-sitter's native child iteration, which equals ascending
//!   `start_byte` because direct siblings in a tree-sitter tree are
//!   non-overlapping by construction.
//! - Navigation stays within a single language tree: children of an
//!   injection-host node return ONLY the host-tree children, NOT the root of
//!   the injected tree. Crossing injection boundaries is PR-4 scope.
//!
//! Return-value distinction (ADR-0025 §"Empty children"):
//! - `[]` — id resolves to an existing node that has no children
//! - `null` — id is not currently resolvable (never issued / invalidated /
//!   mismatched URI / unparsed document / drift)
//!
//! The two cases are deliberately distinct so clients can tell "leaf node"
//! apart from "this reference is gone".

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::TextDocumentIdentifier;
use ulid::Ulid;

use crate::lsp::lsp_impl::kakehashi::node::lookup::find_node_at;
use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};

/// Request parameters for `kakehashi/node/children`.
///
/// `pub` because the handler is registered as a custom LSP method in the
/// `kakehashi` binary (see `src/bin/main.rs`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeChildrenParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
}

impl Kakehashi {
    /// Handler for `kakehashi/node/children`.
    pub async fn kakehashi_node_children(&self, params: NodeChildrenParams) -> Result<Value> {
        let lsp_uri = params.text_document.uri;
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::node::children", "invalid URI: {}", lsp_uri.as_str());
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
        // `kakehashi/node` and `kakehashi/node/parent`: didOpen schedules an
        // async parse, and a `didChange` mid-flight can briefly leave
        // `Document::tree()` populated with a stale tree while NodeTracker has
        // already been adjusted. The helper waits on the parse-state watch
        // channel and re-parses on demand so the snapshot below is consistent.
        self.ensure_parsed_for_node_lookup(&uri).await;

        // Snapshot the document so we operate on a consistent (text, tree) pair.
        let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot()) else {
            log::debug!(target: "kakehashi::node::children", "no parsed document for {}", uri);
            return Ok(Value::Null);
        };
        let tree = snapshot.tree();

        // Find the tree-sitter node matching the tracked (start, end, kind).
        // Defensive: the tracker stays in sync with didChange, so this should
        // always succeed for a non-null lookup.
        let Some(node) = find_node_at(tree, start, end, &kind) else {
            log::warn!(
                target: "kakehashi::node::children",
                "tracker hit but no matching node in tree for ulid={} uri={} range=[{},{}) kind={}",
                ulid, uri, start, end, kind
            );
            return Ok(Value::Null);
        };

        // tree-sitter's `Node::children(&mut cursor)` iterates BOTH named and
        // anonymous children in document order — exactly the contract ADR-0025
        // requires for PR-3. A leaf node yields an empty iterator, which we
        // serialize as `[]` (NOT `null`).
        let tracker = self.bridge.node_tracker();
        let mut cursor = node.walk();
        let infos: Vec<Value> = node
            .children(&mut cursor)
            .map(|child| {
                let child_ulid =
                    tracker.get_or_create(&uri, child.start_byte(), child.end_byte(), child.kind());
                json!({
                    "id": child_ulid.to_string(),
                    "type": child.kind(),
                })
            })
            .collect();

        Ok(Value::Array(infos))
    }
}
