//! Scalar node accessors for the Node Reference Protocol (node-reference-protocol).
//!
//! These handlers resolve a held ULID to its tree-sitter node and report a
//! single intrinsic property — kind, namedness, error flags, byte span, child
//! counts, or the s-expression. They mirror the like-named methods on
//! tree-sitter's [`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html)
//! one-for-one.
//!
//! Each response wraps the value in a single-field object (e.g.
//! `{ "kind": "fenced_code_block" }`), matching the shape `kakehashi/node/text`
//! established (`{ "text": ... }`): self-describing on the wire and open to
//! future additive fields without a breaking change. Any unresolvable
//! reference collapses to JSON `null` (node-reference-protocol §"Invalidate vs
//! Not-Found").
//!
//! **Byte offsets are UTF-8** in host-document coordinates — tree-sitter's
//! native space, the same one `kakehashi/node/text` slices against. Line/column
//! `Position` reporting is deliberately *not* here: it needs the UTF-16 vs
//! byte-column encoding decision the protocol defers to a dedicated
//! `kakehashi/node/range` endpoint.

use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;

use crate::lsp::lsp_impl::kakehashi::node::common::NodeIdParams;
use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};
use crate::tree_worker::{NodeScalarOperation, NodeScalarValue};

fn scalar_json(field: &str, value: NodeScalarValue) -> Option<Value> {
    let value = match value {
        NodeScalarValue::String(value) => Value::String(value),
        NodeScalarValue::Bool(value) => Value::Bool(value),
        NodeScalarValue::U64(value) => Value::Number(value.into()),
        NodeScalarValue::ByteRange { .. } => return None,
    };
    let mut object = serde_json::Map::new();
    object.insert(field.into(), value);
    Some(Value::Object(object))
}

/// Run `f` on the resolved node and wrap its result under `field`, or return
/// JSON `null` if the id does not resolve.
macro_rules! scalar_accessor {
    ($self:ident, $params:ident, $field:literal, $operation:expr, $f:expr) => {{
        let shadow = match uri_to_url(&$params.text_document.uri) {
            Ok(uri) => {
                $self
                    .tree_worker_shadow
                    .node_scalar(&uri, &$params.id, $operation)
                    .await
            }
            Err(_) => None,
        };
        let value = $self
            .with_node_by_id(&$params.text_document.uri, &$params.id, $f)
            .await
            .map(|(_uri, _layer, _incarnation, v)| json!({ $field: v }))
            .unwrap_or(Value::Null);
        if let Some(shadow) = shadow.and_then(|shadow| scalar_json($field, shadow))
            && shadow != value
        {
            log::debug!(
                target: "kakehashi::tree_worker_shadow",
                "node scalar mismatch operation={:?} authoritative={} worker={}",
                $operation,
                value,
                shadow,
            );
        }
        Ok(value)
    }};
}

impl Kakehashi {
    /// `kakehashi/node/kind` — the node's grammar symbol name (`Node::kind`).
    pub async fn kakehashi_node_kind(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(self, params, "kind", NodeScalarOperation::Kind, |n| n
            .kind())
    }

    /// `kakehashi/node/grammarName` — the node's grammar name, which differs
    /// from `kind` for nodes aliased by the grammar (`Node::grammar_name`).
    pub async fn kakehashi_node_grammar_name(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "grammarName",
            NodeScalarOperation::GrammarName,
            |n| n.grammar_name()
        )
    }

    /// `kakehashi/node/isNamed` — whether the node is *named* (vs an anonymous
    /// token), per `Node::is_named`.
    pub async fn kakehashi_node_is_named(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(self, params, "isNamed", NodeScalarOperation::IsNamed, |n| n
            .is_named())
    }

    /// `kakehashi/node/isExtra` — whether the node is an *extra* (e.g. a comment
    /// the grammar permits anywhere), per `Node::is_extra`.
    pub async fn kakehashi_node_is_extra(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(self, params, "isExtra", NodeScalarOperation::IsExtra, |n| n
            .is_extra())
    }

    /// `kakehashi/node/hasError` — whether the node's subtree contains a syntax
    /// error, per `Node::has_error`.
    pub async fn kakehashi_node_has_error(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "hasError",
            NodeScalarOperation::HasError,
            |n| n.has_error()
        )
    }

    /// `kakehashi/node/isError` — whether the node itself is an `ERROR` node,
    /// per `Node::is_error`.
    pub async fn kakehashi_node_is_error(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(self, params, "isError", NodeScalarOperation::IsError, |n| n
            .is_error())
    }

    /// `kakehashi/node/isMissing` — whether the node is a zero-width `MISSING`
    /// node the parser inserted during error recovery, per `Node::is_missing`.
    pub async fn kakehashi_node_is_missing(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "isMissing",
            NodeScalarOperation::IsMissing,
            |n| n.is_missing()
        )
    }

    /// `kakehashi/node/startByte` — the node's start byte (UTF-8, host coords),
    /// per `Node::start_byte`.
    pub async fn kakehashi_node_start_byte(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "startByte",
            NodeScalarOperation::StartByte,
            |n| n.start_byte()
        )
    }

    /// `kakehashi/node/endByte` — the node's end byte (UTF-8, host coords),
    /// per `Node::end_byte`.
    pub async fn kakehashi_node_end_byte(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(self, params, "endByte", NodeScalarOperation::EndByte, |n| n
            .end_byte())
    }

    /// `kakehashi/node/byteRange` — the node's `[startByte, endByte)` span
    /// (UTF-8, host coords) in one call, per `Node::byte_range`.
    pub async fn kakehashi_node_byte_range(&self, params: NodeIdParams) -> Result<Value> {
        let shadow = match uri_to_url(&params.text_document.uri) {
            Ok(uri) => {
                self.tree_worker_shadow
                    .node_scalar(&uri, &params.id, NodeScalarOperation::ByteRange)
                    .await
            }
            Err(_) => None,
        };
        let value = self
            .with_node_by_id(
                &params.text_document.uri,
                &params.id,
                |n| json!({ "startByte": n.start_byte(), "endByte": n.end_byte() }),
            )
            .await
            .map(|(_uri, _layer, _incarnation, v)| v)
            .unwrap_or(Value::Null);
        if let Some(NodeScalarValue::ByteRange {
            start_byte,
            end_byte,
        }) = shadow
        {
            let shadow = json!({ "startByte": start_byte, "endByte": end_byte });
            if shadow != value {
                log::debug!(
                    target: "kakehashi::tree_worker_shadow",
                    "node scalar mismatch operation={:?} authoritative={} worker={}",
                    NodeScalarOperation::ByteRange,
                    value,
                    shadow,
                );
            }
        }
        Ok(value)
    }

    /// `kakehashi/node/childCount` — number of children (named + anonymous),
    /// per `Node::child_count`.
    pub async fn kakehashi_node_child_count(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "childCount",
            NodeScalarOperation::ChildCount,
            |n| n.child_count()
        )
    }

    /// `kakehashi/node/namedChildCount` — number of *named* children, per
    /// `Node::named_child_count`.
    pub async fn kakehashi_node_named_child_count(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "namedChildCount",
            NodeScalarOperation::NamedChildCount,
            |n| n.named_child_count()
        )
    }

    /// `kakehashi/node/descendantCount` — number of descendants including the
    /// node itself, per `Node::descendant_count`.
    pub async fn kakehashi_node_descendant_count(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "descendantCount",
            NodeScalarOperation::DescendantCount,
            |n| n.descendant_count()
        )
    }

    /// `kakehashi/node/toSexp` — the node's subtree as an s-expression string,
    /// per `Node::to_sexp`. Useful for debugging and snapshot tests.
    pub async fn kakehashi_node_to_sexp(&self, params: NodeIdParams) -> Result<Value> {
        scalar_accessor!(
            self,
            params,
            "sexp",
            NodeScalarOperation::SExpression,
            |n| n.to_sexp()
        )
    }
}
