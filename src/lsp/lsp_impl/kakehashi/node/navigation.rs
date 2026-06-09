//! Tree-walking accessors for the Node Reference Protocol (node-reference-protocol).
//!
//! Each handler resolves a held ULID and returns a *related* node (or list of
//! nodes) as `NodeInfo`, mirroring the like-named methods on tree-sitter's
//! [`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html):
//! indexed children, siblings, the first child past a byte, and byte-range
//! descendants. All results are minted in the **same injection layer** as the
//! input node (they live in the same tree), preserving the per-layer Scope rule
//! (node-reference-protocol §"Navigation Methods").
//!
//! Single-node lookups return `NodeInfo | null` (`null` when there is no such
//! node *or* the id is unresolvable — both collapse to the universal null).
//! List lookups return `NodeInfo[] | null` (`null` only when the id itself is
//! unresolvable; an empty relation yields `[]`).

use serde_json::Value;
use tower_lsp_server::jsonrpc::Result;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::lsp_impl::kakehashi::node::common::{
    NodeByteParams, NodeByteRangeParams, NodeIdParams, NodeIndexParams,
};

/// Map a tree-sitter node to the `(start, end, kind)` triple the navigation
/// helpers re-mint from.
fn triple(node: tree_sitter::Node<'_>) -> (usize, usize, &'static str) {
    (node.start_byte(), node.end_byte(), node.kind())
}

impl Kakehashi {
    /// `kakehashi/node/child` — the child at `index` (named + anonymous), per
    /// `Node::child`. Out-of-range / negative indices resolve to `null`.
    pub async fn kakehashi_node_child(&self, params: NodeIndexParams) -> Result<Value> {
        // tree-sitter child indices are u32; anything outside that range can
        // never match and is treated as "no such child".
        let index = u32::try_from(params.index).ok();
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                index.and_then(|i| n.child(i)).map(triple)
            })
            .await)
    }

    /// `kakehashi/node/namedChild` — the *named* child at `index`, per
    /// `Node::named_child`. Out-of-range / negative indices resolve to `null`.
    pub async fn kakehashi_node_named_child(&self, params: NodeIndexParams) -> Result<Value> {
        let index = u32::try_from(params.index).ok();
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                index.and_then(|i| n.named_child(i)).map(triple)
            })
            .await)
    }

    /// `kakehashi/node/namedChildren` — all *named* children in document order,
    /// per `Node::named_children`. Complements `kakehashi/node/children`
    /// (named + anonymous) and mints IDs only for named nodes.
    pub async fn kakehashi_node_named_children(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_nodes(&params.text_document.uri, &params.id, |n| {
                let mut cursor = n.walk();
                n.named_children(&mut cursor).map(triple).collect()
            })
            .await)
    }

    /// `kakehashi/node/nextSibling` — the next sibling (named + anonymous), per
    /// `Node::next_sibling`. `null` for the last child or an unresolvable id.
    pub async fn kakehashi_node_next_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                n.next_sibling().map(triple)
            })
            .await)
    }

    /// `kakehashi/node/prevSibling` — the previous sibling (named + anonymous),
    /// per `Node::prev_sibling`. `null` for the first child or an unresolvable id.
    pub async fn kakehashi_node_prev_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                n.prev_sibling().map(triple)
            })
            .await)
    }

    /// `kakehashi/node/nextNamedSibling` — the next *named* sibling, per
    /// `Node::next_named_sibling`.
    pub async fn kakehashi_node_next_named_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                n.next_named_sibling().map(triple)
            })
            .await)
    }

    /// `kakehashi/node/prevNamedSibling` — the previous *named* sibling, per
    /// `Node::prev_named_sibling`.
    pub async fn kakehashi_node_prev_named_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                n.prev_named_sibling().map(triple)
            })
            .await)
    }

    /// `kakehashi/node/firstChildForByte` — the node's first child extending
    /// beyond `byte` (UTF-8, host coords), per `Node::first_child_for_byte`.
    /// Negative or out-of-bounds (past the node's `end_byte`) bytes resolve to
    /// `null`.
    pub async fn kakehashi_node_first_child_for_byte(
        &self,
        params: NodeByteParams,
    ) -> Result<Value> {
        let byte = usize::try_from(params.byte).ok();
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                // Reject a byte past the node's end before handing it to
                // tree-sitter, whose behaviour for out-of-bounds offsets is
                // version-dependent (mirrors lookup::find_node_at).
                byte.filter(|&b| b <= n.end_byte())
                    .and_then(|b| n.first_child_for_byte(b))
                    .map(triple)
            })
            .await)
    }

    /// `kakehashi/node/descendantForByteRange` — the smallest descendant
    /// (named + anonymous) spanning `[startByte, endByte)` within this node's
    /// subtree, per `Node::descendant_for_byte_range`. Negative, inverted
    /// (`startByte > endByte`), or out-of-bounds (past the node's `end_byte`)
    /// ranges resolve to `null`.
    pub async fn kakehashi_node_descendant_for_byte_range(
        &self,
        params: NodeByteRangeParams,
    ) -> Result<Value> {
        let range = byte_range(&params);
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                range
                    .filter(|&(_s, e)| e <= n.end_byte())
                    .and_then(|(s, e)| n.descendant_for_byte_range(s, e))
                    .map(triple)
            })
            .await)
    }

    /// `kakehashi/node/namedDescendantForByteRange` — the smallest *named*
    /// descendant spanning `[startByte, endByte)` within this node's subtree, per
    /// `Node::named_descendant_for_byte_range`. Negative, inverted
    /// (`startByte > endByte`), or out-of-bounds (past the node's `end_byte`)
    /// ranges resolve to `null`.
    pub async fn kakehashi_node_named_descendant_for_byte_range(
        &self,
        params: NodeByteRangeParams,
    ) -> Result<Value> {
        let range = byte_range(&params);
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, |n| {
                range
                    .filter(|&(_s, e)| e <= n.end_byte())
                    .and_then(|(s, e)| n.named_descendant_for_byte_range(s, e))
                    .map(triple)
            })
            .await)
    }
}

/// Convert the signed byte bounds to `usize`, returning `None` (→ `null`) if
/// either is negative or the range is inverted (`start > end`).
///
/// The inversion guard mirrors `lookup::find_node_at`: tree-sitter's
/// `descendant_for_byte_range` is not specified for `start > end`, so we reject
/// it up front rather than relying on the C bindings' behaviour.
fn byte_range(params: &NodeByteRangeParams) -> Option<(usize, usize)> {
    let start = usize::try_from(params.start_byte).ok()?;
    let end = usize::try_from(params.end_byte).ok()?;
    if start > end {
        return None;
    }
    Some((start, end))
}
