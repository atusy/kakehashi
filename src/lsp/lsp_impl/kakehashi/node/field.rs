//! Field-aware accessors for the Node Reference Protocol (node-reference-protocol).
//!
//! tree-sitter grammars label some children with *field names* (e.g. a
//! `function_definition`'s `name` and `body`). These handlers expose the
//! name-keyed half of that API — looking children up by field name, and
//! reporting the field name of a child by index — mirroring
//! [`Node::child_by_field_name`], [`Node::children_by_field_name`],
//! [`Node::field_name_for_child`], and [`Node::field_name_for_named_child`].
//!
//! Field *ids* (the numeric, grammar-internal counterpart) are intentionally
//! not exposed here: they are opaque and grammar-version-specific, a design
//! question tracked separately rather than implemented blindly.

use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::lsp_impl::kakehashi::node::common::{NodeFieldNameParams, NodeIndexParams};

impl Kakehashi {
    /// `kakehashi/node/childByFieldName` — the child labelled `name`, per
    /// `Node::child_by_field_name`. `null` when no child carries that field (or
    /// the id is unresolvable).
    pub async fn kakehashi_node_child_by_field_name(
        &self,
        params: NodeFieldNameParams,
    ) -> Result<Value> {
        let name = params.name.clone();
        Ok(self
            .navigate_to_node(&params.text_document.uri, &params.id, move |n| {
                n.child_by_field_name(&name)
                    .map(|c| (c.start_byte(), c.end_byte(), c.kind()))
            })
            .await)
    }

    /// `kakehashi/node/childrenByFieldName` — all children labelled `name` in
    /// document order, per `Node::children_by_field_name`. A resolvable node with
    /// no such children yields `[]`; an unresolvable id yields `null`.
    pub async fn kakehashi_node_children_by_field_name(
        &self,
        params: NodeFieldNameParams,
    ) -> Result<Value> {
        let name = params.name.clone();
        Ok(self
            .navigate_to_nodes(&params.text_document.uri, &params.id, move |n| {
                let mut cursor = n.walk();
                n.children_by_field_name(&name, &mut cursor)
                    .map(|c| (c.start_byte(), c.end_byte(), c.kind()))
                    .collect()
            })
            .await)
    }

    /// `kakehashi/node/fieldNameForChild` — the field name of the child at
    /// `index` (named + anonymous), per `Node::field_name_for_child`.
    ///
    /// Returns `{ "fieldName": string | null }` when the node resolves —
    /// `fieldName` is `null` for a child with no field, or an out-of-range
    /// index — and top-level `null` only when the id itself is unresolvable.
    pub async fn kakehashi_node_field_name_for_child(
        &self,
        params: NodeIndexParams,
    ) -> Result<Value> {
        let index = u32::try_from(params.index).ok();
        let value = self
            .with_node_by_id(&params.text_document.uri, &params.id, move |n| {
                index.and_then(|i| n.field_name_for_child(i))
            })
            .await
            .map(|(_uri, _layer, field)| json!({ "fieldName": field }))
            .unwrap_or(Value::Null);
        Ok(value)
    }

    /// `kakehashi/node/fieldNameForNamedChild` — the field name of the *named*
    /// child at `index`, per `Node::field_name_for_named_child`. Same response
    /// shape as `fieldNameForChild`.
    pub async fn kakehashi_node_field_name_for_named_child(
        &self,
        params: NodeIndexParams,
    ) -> Result<Value> {
        let index = u32::try_from(params.index).ok();
        let value = self
            .with_node_by_id(&params.text_document.uri, &params.id, move |n| {
                index.and_then(|i| n.field_name_for_named_child(i))
            })
            .await
            .map(|(_uri, _layer, field)| json!({ "fieldName": field }))
            .unwrap_or(Value::Null);
        Ok(value)
    }
}
