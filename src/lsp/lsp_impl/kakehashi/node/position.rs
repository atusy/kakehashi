//! Position / range accessors for the Node Reference Protocol (node-reference-protocol).
//!
//! These mirror tree-sitter's
//! [`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html)
//! `range` / `start_position` / `end_position` /
//! `descendant_for_point_range` / `named_descendant_for_point_range`, but speak
//! **LSP `Position` (UTF-16 code units)** on the wire rather than tree-sitter's
//! native `Point` (whose `column` is a UTF-8 byte offset).
//!
//! ## Why LSP `Position`, not tree-sitter `Point`
//!
//! The whole protocol accepts and returns LSP `Position` at its boundary
//! (node-reference-protocol §"Position Encoding"); kakehashi advertises no
//! `positionEncoding`, so `character` is a UTF-16 code unit per LSP 3.18. A
//! tree-sitter `Point.column` is a UTF-8 byte offset, which diverges from LSP
//! around any non-ASCII character (emoji, CJK). Returning native points would
//! make these the only accessors a client must special-case. Instead the server
//! converts each end via [`PositionMapper`](crate::text::PositionMapper):
//! byte → `Position` on output, `Position` → byte on input. Byte-native spans
//! remain available unambiguously via `startByte` / `endByte` / `byteRange` and
//! `descendant*ForByteRange`.
//!
//! Any unresolvable reference (and any `Position` that cannot be mapped into the
//! current document) collapses to JSON `null`.

use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::lsp_impl::kakehashi::node::common::{NodeIdParams, NodePointRangeParams};

impl Kakehashi {
    /// `kakehashi/node/range` — the node's span as a pair of LSP `Position`s,
    /// per `Node::range`. Returns `{ "start": Position, "end": Position }` or
    /// `null`.
    pub async fn kakehashi_node_range(&self, params: NodeIdParams) -> Result<Value> {
        let value = self
            .with_node_mapped(&params.text_document.uri, &params.id, |n, mapper| {
                Some((
                    mapper.byte_to_position(n.start_byte())?,
                    mapper.byte_to_position(n.end_byte())?,
                ))
            })
            .await
            .and_then(|(_uri, _layer, span)| span)
            .map(|(start, end)| json!({ "start": start, "end": end }))
            .unwrap_or(Value::Null);
        Ok(value)
    }

    /// `kakehashi/node/startPosition` — the node's start as an LSP `Position`,
    /// per `Node::start_position`. Returns `{ "startPosition": Position }` or
    /// `null`.
    pub async fn kakehashi_node_start_position(&self, params: NodeIdParams) -> Result<Value> {
        let value = self
            .with_node_mapped(&params.text_document.uri, &params.id, |n, mapper| {
                mapper.byte_to_position(n.start_byte())
            })
            .await
            .and_then(|(_uri, _layer, pos)| pos)
            .map(|pos| json!({ "startPosition": pos }))
            .unwrap_or(Value::Null);
        Ok(value)
    }

    /// `kakehashi/node/endPosition` — the node's end as an LSP `Position`, per
    /// `Node::end_position`. Returns `{ "endPosition": Position }` or `null`.
    pub async fn kakehashi_node_end_position(&self, params: NodeIdParams) -> Result<Value> {
        let value = self
            .with_node_mapped(&params.text_document.uri, &params.id, |n, mapper| {
                mapper.byte_to_position(n.end_byte())
            })
            .await
            .and_then(|(_uri, _layer, pos)| pos)
            .map(|pos| json!({ "endPosition": pos }))
            .unwrap_or(Value::Null);
        Ok(value)
    }

    /// `kakehashi/node/descendantForPointRange` — the smallest descendant
    /// (named + anonymous) spanning `[start, end)` within this node's subtree,
    /// per `Node::descendant_for_point_range`, with the bounds given as LSP
    /// `Position`s. `null` for an unresolvable id, an unmappable `Position`, or an
    /// inverted / out-of-bounds range.
    pub async fn kakehashi_node_descendant_for_point_range(
        &self,
        params: NodePointRangeParams,
    ) -> Result<Value> {
        self.point_range_descendant(params, false).await
    }

    /// `kakehashi/node/namedDescendantForPointRange` — the smallest *named*
    /// descendant spanning `[start, end)` within this node's subtree, per
    /// `Node::named_descendant_for_point_range`. Same `Position` handling as the
    /// anonymous-inclusive variant.
    pub async fn kakehashi_node_named_descendant_for_point_range(
        &self,
        params: NodePointRangeParams,
    ) -> Result<Value> {
        self.point_range_descendant(params, true).await
    }

    /// Shared body for the two point-range descendant lookups. Converts the LSP
    /// `Position` bounds to byte offsets, applies the same inverted /
    /// out-of-bounds guards as the byte-range accessors, then mints the result in
    /// the input node's layer.
    async fn point_range_descendant(
        &self,
        params: NodePointRangeParams,
        named: bool,
    ) -> Result<Value> {
        let start = params.start;
        let end = params.end;
        let resolved = self
            .with_node_mapped(&params.text_document.uri, &params.id, |n, mapper| {
                // Strict conversion: a `character` past a line's end must mean
                // "no such location" (null), NOT spill onto a later line as plain
                // `position_to_byte` would (it computes line_start + character).
                let start_byte = mapper.position_to_byte_strict(start)?;
                let end_byte = mapper.position_to_byte_strict(end)?;
                // Mirror the byte-range guards: reject inverted ranges and any
                // range not contained in the node's own span, whose tree-sitter
                // behaviour is unspecified (see navigation.rs / lookup::find_node_at).
                if start_byte < n.start_byte() || start_byte > end_byte || end_byte > n.end_byte() {
                    return None;
                }
                let descendant = if named {
                    n.named_descendant_for_byte_range(start_byte, end_byte)
                } else {
                    n.descendant_for_byte_range(start_byte, end_byte)
                };
                descendant.map(|d| (d.start_byte(), d.end_byte(), d.kind()))
            })
            .await;

        let value = match resolved {
            Some((uri, layer, Some(triple))) => self.mint_node_info(&uri, layer, triple),
            _ => Value::Null,
        };
        Ok(value)
    }
}
