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
    NodeByteParams, NodeByteRangeParams, NodeDescendantParams, NodeIdParams, NodeIndexParams,
};
use crate::lsp::lsp_impl::kakehashi::node::injection_stack::ranges_contain_byte;

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
        let Some(index) = index else {
            return Ok(Value::Null);
        };
        Ok(self
            .navigate_to_node_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::Child { index },
                move |n| n.child(index).map(triple),
            )
            .await)
    }

    /// `kakehashi/node/namedChild` — the *named* child at `index`, per
    /// `Node::named_child`. Out-of-range / negative indices resolve to `null`.
    pub async fn kakehashi_node_named_child(&self, params: NodeIndexParams) -> Result<Value> {
        let index = u32::try_from(params.index).ok();
        let Some(index) = index else {
            return Ok(Value::Null);
        };
        Ok(self
            .navigate_to_node_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::NamedChild { index },
                move |n| n.named_child(index).map(triple),
            )
            .await)
    }

    /// `kakehashi/node/namedChildren` — all *named* children in document order,
    /// per `Node::named_children`. Complements `kakehashi/node/children`
    /// (named + anonymous) and mints IDs only for named nodes.
    pub async fn kakehashi_node_named_children(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_nodes_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::NamedChildren,
                move |n| {
                    let mut cursor = n.walk();
                    n.named_children(&mut cursor).map(triple).collect()
                },
            )
            .await)
    }

    /// `kakehashi/node/childWithDescendant` — the immediate child of `id` that
    /// contains `descendantId`, per `Node::child_with_descendant` (issue #335).
    ///
    /// The only two-id accessor: both ids must have been minted in the
    /// **same** injection layer (a cross-layer pair is `null`), and `null`
    /// also covers a descendant that is not actually inside `id` — including
    /// `descendantId == id`, since no child contains the node itself.
    pub async fn kakehashi_node_child_with_descendant(
        &self,
        params: NodeDescendantParams,
    ) -> Result<Value> {
        Ok(self
            .navigate_with_descendant_shadowed(
                &params.text_document.uri,
                &params.id,
                &params.descendant_id,
                |node, descendant| {
                    // tree-sitter leaves child_with_descendant undefined when
                    // `descendant` is not in `node`'s subtree: byte containment
                    // ties on equal-range unary chains (e.g. markdown's
                    // document → section) make it return a child for
                    // descendant == self or an ancestor. The first step of the
                    // descent is the answer; keep descending until we actually
                    // reach `descendant` by node id, so every unrelated pair
                    // normalizes to the protocol's universal null instead.
                    let answer = node.child_with_descendant(descendant)?;
                    let mut current = answer;
                    while current.id() != descendant.id() {
                        current = current.child_with_descendant(descendant)?;
                    }
                    Some(triple(answer))
                },
            )
            .await)
    }

    /// `kakehashi/node/nextSibling` — the next sibling (named + anonymous), per
    /// `Node::next_sibling`. `null` for the last child or an unresolvable id.
    pub async fn kakehashi_node_next_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::NextSibling,
                move |n| n.next_sibling().map(triple),
            )
            .await)
    }

    /// `kakehashi/node/prevSibling` — the previous sibling (named + anonymous),
    /// per `Node::prev_sibling`. `null` for the first child or an unresolvable id.
    pub async fn kakehashi_node_prev_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::PreviousSibling,
                move |n| n.prev_sibling().map(triple),
            )
            .await)
    }

    /// `kakehashi/node/nextNamedSibling` — the next *named* sibling, per
    /// `Node::next_named_sibling`.
    pub async fn kakehashi_node_next_named_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::NextNamedSibling,
                move |n| n.next_named_sibling().map(triple),
            )
            .await)
    }

    /// `kakehashi/node/prevNamedSibling` — the previous *named* sibling, per
    /// `Node::prev_named_sibling`.
    pub async fn kakehashi_node_prev_named_sibling(&self, params: NodeIdParams) -> Result<Value> {
        Ok(self
            .navigate_to_node_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::PreviousNamedSibling,
                move |n| n.prev_named_sibling().map(triple),
            )
            .await)
    }

    /// `kakehashi/node/firstChildForByte` — the node's first child extending
    /// beyond `byte` (UTF-8, host coords), per `Node::first_child_for_byte`.
    /// `byte` is rejected (→ `null`) unless `node.start_byte <= byte <=
    /// node.end_byte` — i.e. negative, before the node, or past its end — and,
    /// for injected layers, unless it lies in the layer's included ranges
    /// (an excluded-gap byte is not injected content, #341).
    pub async fn kakehashi_node_first_child_for_byte(
        &self,
        params: NodeByteParams,
    ) -> Result<Value> {
        let byte = usize::try_from(params.byte).ok();
        let Some(byte) = byte else {
            return Ok(Value::Null);
        };
        Ok(self
            .navigate_to_node_in_ranges_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::FirstChildForByte { byte },
                move |n, text, ranges| {
                    // Keep the byte inside the node's own span before handing it to
                    // tree-sitter, whose behaviour for offsets outside the queried
                    // node is version-dependent (mirrors lookup::find_node_at). These
                    // are node-scoped accessors, so an out-of-node argument is null.
                    Some(byte)
                        .filter(|&b| n.start_byte() <= b && b <= n.end_byte())
                        .filter(|&b| ranges_contain_byte(ranges, b, text.len()))
                        .and_then(|b| n.first_child_for_byte(b))
                        .map(triple)
                },
            )
            .await)
    }

    /// `kakehashi/node/descendantForByteRange` — the smallest descendant
    /// (named + anonymous) spanning `[startByte, endByte)` within this node's
    /// subtree, per `Node::descendant_for_byte_range`. The range is rejected
    /// (→ `null`) unless `node.start_byte <= startByte <= endByte <=
    /// node.end_byte` — i.e. negative, inverted, or reaching outside the node —
    /// and unless both bounds clear the minting layer's included ranges (#341).
    pub async fn kakehashi_node_descendant_for_byte_range(
        &self,
        params: NodeByteRangeParams,
    ) -> Result<Value> {
        let range = byte_range(&params);
        let Some((start_byte, end_byte)) = range else {
            return Ok(Value::Null);
        };
        Ok(self
            .navigate_to_node_in_ranges_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::DescendantForByteRange {
                    start_byte,
                    end_byte,
                    named: false,
                },
                move |n, text, ranges| {
                    Some((start_byte, end_byte))
                        .filter(|&(s, e)| n.start_byte() <= s && e <= n.end_byte())
                        .filter(|&(s, e)| range_bounds_in_ranges(ranges, s, e, text.len()))
                        .and_then(|(s, e)| n.descendant_for_byte_range(s, e))
                        .map(triple)
                },
            )
            .await)
    }

    /// `kakehashi/node/namedDescendantForByteRange` — the smallest *named*
    /// descendant spanning `[startByte, endByte)` within this node's subtree, per
    /// `Node::named_descendant_for_byte_range`. The range is rejected (→ `null`)
    /// unless `node.start_byte <= startByte <= endByte <= node.end_byte` — i.e.
    /// negative, inverted, or reaching outside the node — and unless both bounds
    /// clear the minting layer's included ranges (#341).
    pub async fn kakehashi_node_named_descendant_for_byte_range(
        &self,
        params: NodeByteRangeParams,
    ) -> Result<Value> {
        let range = byte_range(&params);
        let Some((start_byte, end_byte)) = range else {
            return Ok(Value::Null);
        };
        Ok(self
            .navigate_to_node_in_ranges_shadowed(
                &params.text_document.uri,
                &params.id,
                crate::tree_worker::NodeNavigation::DescendantForByteRange {
                    start_byte,
                    end_byte,
                    named: true,
                },
                move |n, text, ranges| {
                    Some((start_byte, end_byte))
                        .filter(|&(s, e)| n.start_byte() <= s && e <= n.end_byte())
                        .filter(|&(s, e)| range_bounds_in_ranges(ranges, s, e, text.len()))
                        .and_then(|(s, e)| n.named_descendant_for_byte_range(s, e))
                        .map(triple)
                },
            )
            .await)
    }
}

/// Gap check for a `[start, end)` coordinate-range argument against the
/// minting layer's included ranges (#341): both the first queried byte
/// (`start`) and the last queried byte (`end - 1`, since `end` is exclusive)
/// must lie in included content under the entry point's half-open rule.
/// `end` itself may sit on a gap start — one past the last included byte —
/// but an `end` whose final queried byte falls in a gap (including an `end`
/// anchored to a *later* range's start across a gap) queries excluded
/// content and is rejected. Per-bound — not "both in one range" — because
/// nodes legitimately span gaps (a multi-line statement in blockquoted code),
/// and querying across a gap with both bounds on real content should still
/// resolve that spanning node.
pub(super) fn range_bounds_in_ranges(
    ranges: &[tree_sitter::Range],
    start: usize,
    end: usize,
    host_len: usize,
) -> bool {
    if !ranges_contain_byte(ranges, start, host_len) {
        return false;
    }
    // Zero-width queries are covered by the start check alone.
    end == start || ranges_contain_byte(ranges, end - 1, host_len)
}

/// Convert the signed byte bounds to `usize`, returning `None` (→ `null`) if
/// either is negative or the range is inverted (`start > end`).
///
/// The inversion guard mirrors `lookup::find_node_at`: tree-sitter's
/// `descendant_for_byte_range` is not specified for `start > end`, so we reject
/// it up front rather than relying on the C bindings' behaviour.
///
/// Per-call the handlers additionally bound the range to the queried node's
/// contiguous span and to the minting layer's included ranges
/// ([`range_bounds_in_ranges`], #341).
fn byte_range(params: &NodeByteRangeParams) -> Option<(usize, usize)> {
    let start = usize::try_from(params.start_byte).ok()?;
    let end = usize::try_from(params.end_byte).ok()?;
    if start > end {
        return None;
    }
    Some((start, end))
}
