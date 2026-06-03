//! `kakehashi/node/parent` — id → immediate-parent NodeInfo (node-reference-protocol).
//!
//! Resolves a previously-issued ULID to its tracked `(start_byte, end_byte, kind)`
//! triple, locates the matching tree-sitter node in the current parse tree, and
//! returns a [`NodeInfo`](../../../../../docs/architecture-decisions/node-reference-protocol.md#nodeinfo-type)
//! for its tree-sitter parent.
//!
//! Per node-reference-protocol §"Navigation Methods", navigation stays within a single
//! language tree: calling `parent` on the root of an injected tree must **not**
//! cross into the host node that contains the injection. To find the node in
//! the correct tree we search the host tree first and then each injected layer
//! at the tracked `start_byte` (see
//! [`with_resolved_node`](super::injection_stack::with_resolved_node)) — that
//! way a node minted by `kakehashi/node` against an injected layer remains
//! navigable from the same layer that produced it.
//!
//! Returns `null` (serialized as JSON `null`) when:
//! - the URI is unknown or invalid,
//! - the ULID is malformed or was never issued / has been invalidated,
//! - the document has not yet been parsed,
//! - the tracked range cannot be matched against a node in the host tree or
//!   any injected layer, or
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
        let host_text = snapshot.text();
        let host_tree = snapshot.tree();

        let Some(host_language) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::node::parent", "no host language for {}", uri);
            return Ok(Value::Null);
        };

        // Search the host tree first, then injected layers at `start`. node-reference-protocol
        // "Scope rule" applies per layer: tree-sitter's `node.parent()` returns
        // None for any tree root (host root AND injected root), which is the
        // intended semantics — do NOT chase into the host node that contains
        // the injection.
        let parent_info = with_resolved_node(
            &self.language,
            &host_language,
            host_text,
            host_tree,
            start,
            end,
            kind,
            |node| {
                node.parent()
                    .map(|p| (p.start_byte(), p.end_byte(), p.kind()))
            },
        );
        let Some(Some((p_start, p_end, p_kind))) = parent_info else {
            if parent_info.is_none() {
                log::warn!(
                    target: "kakehashi::node::parent",
                    "tracker hit but no matching node in any layer for ulid={} uri={} range=[{},{}) kind={}",
                    ulid, uri, start, end, kind
                );
            }
            return Ok(Value::Null);
        };

        // Issue / reuse a stable ULID for the parent (lazy-node-identity-tracking lazy assignment).
        let parent_ulid = self
            .bridge
            .node_tracker()
            .get_or_create(&uri, p_start, p_end, p_kind);

        Ok(json!({
            "id": parent_ulid.to_string(),
            "type": p_kind,
        }))
    }
}
