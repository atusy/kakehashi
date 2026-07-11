//! `kakehashi/node/children` — id → immediate-children NodeInfo array (node-reference-protocol).
//!
//! Resolves a previously-issued ULID to its tracked `(start_byte, end_byte, kind, layer)`
//! key, locates the matching tree-sitter node in the current parse tree, and
//! returns one [`NodeInfo`](../../../../../docs/architecture-decisions/node-reference-protocol.md#nodeinfo-type)
//! per immediate child in **document order** (ascending `start_byte`).
//!
//! Per node-reference-protocol §"Navigation Methods":
//! - Children include **both named and anonymous** nodes. A future `namedOnly`
//!   parameter may filter, but PR-3 returns the full child list.
//! - The order is tree-sitter's native child iteration, which equals ascending
//!   `start_byte` because direct siblings in a tree-sitter tree are
//!   non-overlapping by construction.
//! - Navigation stays within a single language tree: children of an
//!   injection-host node return ONLY the host-tree children, NOT the root of
//!   the injected tree. Crossing injection boundaries is PR-4 scope.
//!
//! Return-value distinction (node-reference-protocol §"Empty children"):
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

use crate::lsp::lsp_impl::kakehashi::node::injection_stack::with_resolved_node;
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

        // Malformed ULID collapses to null per node-reference-protocol §"Invalidate vs Not-Found".
        let Ok(ulid) = params.id.parse::<Ulid>() else {
            return Ok(Value::Null);
        };

        // Look up the tracked node's byte range, kind, and injection layer.
        // None means: never issued, invalidated by a prior edit, or this URI
        // has no entries. `layer` pins resolution and child minting to the
        // language tree that minted the node (node-reference-protocol Scope rule).
        let Some((start, end, kind, layer, tracked_incarnation)) =
            self.bridge.node_tracker().lookup_node(&uri, &ulid)
        else {
            return Ok(Value::Null);
        };

        // Ensure the document is parsed before snapshotting — same race as in
        // `kakehashi/node` and `kakehashi/node/parent`: didOpen schedules an
        // async parse, and a `didChange` mid-flight can briefly leave
        // `Document::tree()` populated with a stale tree while NodeTracker has
        // already been adjusted. The helper waits on the parse-state watch
        // channel and re-parses on demand so the snapshot below is consistent.
        // Resolve a CURRENT parse snapshot (parse-snapshot ADR §3): a node id
        // names a live-position node, so a trailing snapshot rejects
        // immediately with the protocol's universal null; only the bounded
        // first-parse wait applies. Currency is also what keeps the layer
        // re-mints below sound (a stale read never mints).
        let Some(snapshot) = self.current_snapshot(&uri).await else {
            log::debug!(target: "kakehashi::node::children", "no current parse snapshot for {}", uri);
            return Ok(Value::Null);
        };
        let incarnation = snapshot.incarnation;
        if incarnation != tracked_incarnation {
            return Ok(Value::Null);
        }
        let host_text: &str = &snapshot.text;
        let Some(host_tree) = snapshot.tree.as_ref() else {
            return Ok(Value::Null);
        };

        let Some(host_language) = snapshot.language.clone() else {
            log::debug!(target: "kakehashi::node::children", "no host language for {}", uri);
            return Ok(Value::Null);
        };

        // Resolve in the minting layer only (`stack[layer]`), never falling back
        // to other layers. node-reference-protocol "Navigation Methods":
        // children stay within a single language tree, so an injected node's
        // children come from the injected tree — not from the host node that
        // contains the injection.
        //
        // tree-sitter's `Node::children(&mut cursor)` iterates BOTH named and
        // anonymous children in document order. A leaf node yields an empty
        // iterator, which we serialize as `[]` (NOT `null`).
        let child_infos: Option<Vec<(usize, usize, &'static str)>> = with_resolved_node(
            &self.language,
            &host_language,
            host_text,
            host_tree,
            start,
            end,
            kind,
            layer,
            |node| {
                let mut cursor = node.walk();
                node.children(&mut cursor)
                    .map(|child| (child.start_byte(), child.end_byte(), child.kind()))
                    .collect()
            },
        );
        let Some(child_infos) = child_infos else {
            log::warn!(
                target: "kakehashi::node::children",
                "tracker hit but no matching node in minting layer {} for ulid={} uri={} range=[{},{}) kind={}",
                layer, ulid, uri, start, end, kind
            );
            return Ok(Value::Null);
        };

        let tracker = self.bridge.node_tracker();
        let infos: Vec<Value> = child_infos
            .into_iter()
            .map(|(c_start, c_end, c_kind)| {
                // Children live in the same tree as their parent, so they are
                // minted in the same `layer`.
                let child_ulid = tracker.get_or_create_in_layer_for_incarnation(
                    &uri,
                    c_start,
                    c_end,
                    c_kind,
                    layer,
                    incarnation,
                );
                json!({
                    "id": child_ulid.to_string(),
                    "kind": c_kind,
                })
            })
            .collect();

        Ok(Value::Array(infos))
    }
}
