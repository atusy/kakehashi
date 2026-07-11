//! `kakehashi/node/parent` — id → immediate-parent NodeInfo (node-reference-protocol).
//!
//! Resolves a previously-issued ULID to its tracked `(start_byte, end_byte, kind, layer)`
//! key, locates the matching tree-sitter node in the current parse tree, and
//! returns a [`NodeInfo`](../../../../../docs/architecture-decisions/node-reference-protocol.md#nodeinfo-type)
//! for its tree-sitter parent.
//!
//! Per node-reference-protocol §"Navigation Methods", navigation stays within a single
//! language tree: calling `parent` on the root of an injected tree must **not**
//! cross into the host node that contains the injection. To find the node in
//! the correct tree we resolve it **only** in the layer that minted it —
//! `stack[layer]` for the tracked `layer` (see
//! [`with_resolved_node`](super::injection_stack::with_resolved_node)) — so a
//! node minted by `kakehashi/node` against an injected layer is never re-matched
//! against a different layer's tree.
//!
//! Returns `null` (serialized as JSON `null`) when:
//! - the URI is unknown or invalid,
//! - the ULID is malformed or was never issued / has been invalidated,
//! - the document has not yet been parsed,
//! - the tracked range cannot be matched against a node in the minting layer's
//!   tree (e.g. an edit restructured the injection nesting), or
//! - the matched node is the root of its tree (no parent — applies to host
//!   root AND to the root of any injected tree, per the Scope rule).

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::TextDocumentIdentifier;
use ulid::Ulid;

use crate::lsp::lsp_impl::kakehashi::node::injection_stack::with_resolved_node;
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

        // Malformed ULID collapses to null per node-reference-protocol §"Invalidate vs Not-Found".
        let Ok(ulid) = params.id.parse::<Ulid>() else {
            return Ok(Value::Null);
        };

        // Look up the tracked node's byte range, kind, and injection layer.
        // None means: never issued, invalidated by a prior edit, or this URI
        // has no entries. `layer` pins resolution to the language tree that
        // minted the node so navigation stays in-layer (node-reference-protocol
        // Scope rule).
        let Some((start, end, kind, layer, tracked_incarnation)) =
            self.bridge.node_tracker().lookup_node(&uri, &ulid)
        else {
            return Ok(Value::Null);
        };

        // Ensure the document is parsed before snapshotting — same race as in
        // `kakehashi/node`: didOpen schedules an async parse, a client that
        // immediately follows up with `parent` must not see `tree: None`.
        // Resolve a CURRENT parse snapshot (parse-snapshot ADR §3): a node id
        // names a live-position node, so a trailing snapshot rejects
        // immediately with the protocol's universal null; only the bounded
        // first-parse wait applies. Currency is also what keeps the layer
        // re-mints below sound (a stale read never mints).
        let Some(snapshot) = self.current_snapshot(&uri).await else {
            log::debug!(target: "kakehashi::node::parent", "no current parse snapshot for {}", uri);
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
            log::debug!(target: "kakehashi::node::parent", "no host language for {}", uri);
            return Ok(Value::Null);
        };

        // Resolve in the minting layer only (`stack[layer]`), never falling back
        // to other layers. node-reference-protocol "Scope rule" applies per
        // layer: tree-sitter's `node.parent()` returns None for any tree root
        // (host root AND injected root), which is the intended semantics — do
        // NOT chase into the host node that contains the injection.
        let parent_info = with_resolved_node(
            &self.language,
            &host_language,
            host_text,
            host_tree,
            start,
            end,
            kind,
            layer,
            |node| {
                node.parent()
                    .map(|p| (p.start_byte(), p.end_byte(), p.kind()))
            },
        );
        let Some(Some((p_start, p_end, p_kind))) = parent_info else {
            if parent_info.is_none() {
                log::warn!(
                    target: "kakehashi::node::parent",
                    "tracker hit but no matching node in minting layer {} for ulid={} uri={} range=[{},{}) kind={}",
                    layer, ulid, uri, start, end, kind
                );
            }
            return Ok(Value::Null);
        };

        // Issue / reuse a stable ULID for the parent (lazy-node-identity-tracking
        // lazy assignment). The parent lives in the same tree as the child, so
        // it is minted in the same `layer`.
        let parent_ulid = self
            .bridge
            .node_tracker()
            .get_or_create_in_layer_for_incarnation(
                &uri,
                p_start,
                p_end,
                p_kind,
                layer,
                incarnation,
            );

        Ok(json!({
            "id": parent_ulid.to_string(),
            "kind": p_kind,
        }))
    }
}
