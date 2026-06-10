//! Shared request shapes and the id-resolution helper for the id-based
//! `kakehashi/node/*` accessor methods (node-reference-protocol).
//!
//! Every accessor (`kind`, `childCount`, `child`, `nextSibling`, …) repeats the
//! same prelude: convert the URI, parse the ULID, look the tracked
//! `(start, end, kind, layer)` up, ensure the document is parsed, snapshot it,
//! and resolve the node **in the layer that minted it** via
//! [`with_resolved_node`]. [`Kakehashi::with_node_by_id`] centralises that
//! prelude so each handler shrinks to "run a closure on the `Node`, shape the
//! result". This keeps the per-layer Scope rule (node-reference-protocol
//! §"Navigation Methods") enforced uniformly: a node minted in an injected
//! layer is never re-resolved against a different layer's tree.

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::ls_types::{Position, TextDocumentIdentifier, Uri};
use ulid::Ulid;
use url::Url;

use crate::lsp::lsp_impl::kakehashi::node::injection_stack::with_resolved_node;
use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};

/// A tracked node's `(start_byte, end_byte, kind)` triple, as produced by a
/// navigation closure and consumed by the re-minting helpers below. `kind` is
/// `&'static str` because tree-sitter interns node kinds in the grammar's
/// static data, so it outlives the borrowed tree.
type NodeTriple = (usize, usize, &'static str);

/// Request parameters for the id-only accessors (`kind`, `byteRange`,
/// `childCount`, `nextSibling`, `namedChildren`, …).
///
/// `pub` because the handlers are registered as custom LSP methods in the
/// `kakehashi` binary (see `src/bin/main.rs`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
}

/// Request parameters for index-based accessors (`child`, `namedChild`,
/// `fieldNameForChild`, `fieldNameForNamedChild`).
///
/// `index` is deserialized as a signed integer so an out-of-range or negative
/// value collapses to `null` (node-reference-protocol universal null semantics)
/// instead of producing a JSON-RPC deserialization error. tree-sitter's child
/// indices are `u32`; a value outside `0..=u32::MAX` can never match a child
/// and is treated as "no such child".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIndexParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
    pub index: i64,
}

/// Request parameters for field-name accessors (`childByFieldName`,
/// `childrenByFieldName`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeFieldNameParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
    pub name: String,
}

/// Request parameters for `firstChildForByte`.
///
/// `byte` is a UTF-8 byte offset in host-document coordinates — the same space
/// `startByte` / `endByte` report — deserialized as a signed integer so a
/// negative value collapses to `null` rather than erroring.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeByteParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
    pub byte: i64,
}

/// Request parameters for byte-range descendant lookups
/// (`descendantForByteRange`, `namedDescendantForByteRange`).
///
/// Both bounds are UTF-8 byte offsets in host-document coordinates, signed so
/// negatives collapse to `null`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeByteRangeParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
    pub start_byte: i64,
    pub end_byte: i64,
}

/// Request parameters for point-range descendant lookups
/// (`descendantForPointRange`, `namedDescendantForPointRange`).
///
/// `start` / `end` are LSP `Position`s (`{ line, character }`, **UTF-16** code
/// units per the protocol's position encoding). The server converts each to a
/// UTF-8 byte offset via [`PositionMapper`](crate::text::PositionMapper) before
/// searching, so clients use the same coordinate space as every other LSP
/// request — never tree-sitter's byte-column points.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodePointRangeParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
    pub start: Position,
    pub end: Position,
}

impl Kakehashi {
    /// Resolve `id` to its tree-sitter node and run `f` on it, returning the
    /// minting `layer` alongside the closure's result.
    ///
    /// This is the shared prelude for every id-based accessor. It returns
    /// `None` — which handlers serialize as JSON `null` — for any unresolvable
    /// reference (node-reference-protocol §"Invalidate vs Not-Found"):
    /// - the URI is invalid,
    /// - the ULID is malformed / never issued / invalidated,
    /// - the document has not been parsed,
    /// - no host language is known, or
    /// - the tracked range no longer matches a node in the minting layer's tree
    ///   (e.g. an edit restructured the injection nesting).
    ///
    /// The `layer` is handed back so navigation handlers can re-mint result
    /// nodes (children, siblings, descendants) in the **same** layer — they live
    /// in the same tree as the resolved node.
    pub(super) async fn with_node_by_id<R>(
        &self,
        lsp_uri: &Uri,
        id: &str,
        mut f: impl FnMut(tree_sitter::Node<'_>) -> R,
    ) -> Option<(Url, usize, R)> {
        // Most accessors don't need the document text; ignore it.
        self.with_node_text(lsp_uri, id, move |node, _text| f(node))
            .await
    }

    /// Like [`with_node_by_id`](Self::with_node_by_id) but also hands the closure
    /// the host document text, so the position/range accessors can build a
    /// [`PositionMapper`](crate::text::PositionMapper) to convert tree-sitter byte
    /// offsets ↔ LSP `Position` (UTF-16) without a second snapshot.
    ///
    /// The text — not a pre-built `PositionMapper` — is passed because
    /// `PositionMapper::new` indexes the whole document (O(doc)); building it
    /// unconditionally here would tax every scalar/navigation accessor that never
    /// touches positions. Only the handful of position accessors build the mapper,
    /// inside their own closure.
    pub(super) async fn with_node_text<R>(
        &self,
        lsp_uri: &Uri,
        id: &str,
        mut f: impl FnMut(tree_sitter::Node<'_>, &str) -> R,
    ) -> Option<(Url, usize, R)> {
        // An unparseable URI signals a misbehaving client; warn for parity with
        // `node` / `node/text` / `node/parent` / `node/children` while still
        // collapsing to `null`.
        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!(target: "kakehashi::node", "invalid URI: {}", lsp_uri.as_str());
            return None;
        };

        // Malformed ULID collapses to null, like a never-issued id.
        let ulid = id.parse::<Ulid>().ok()?;

        // Tracked `(start, end, kind, layer)`. `layer` pins resolution to the
        // language tree that minted the node so navigation stays in-layer.
        let (start, end, kind, layer) = self.bridge.node_tracker().lookup_node(&uri, &ulid)?;

        // Same race as the other handlers: didOpen schedules an async parse, so
        // a request issued immediately after must not see `tree: None`.
        self.ensure_parsed_for_node_lookup(&uri).await;

        // Snapshot so we operate on a consistent (text, tree) pair.
        let snapshot = self.documents.get(&uri).and_then(|doc| doc.snapshot())?;
        let host_text = snapshot.text();

        // Defensively reject an invalid or out-of-bounds tracked range before any
        // tree work: an inverted range or one extending past the current text
        // can't name a real node (and could panic byte slicing downstream). This
        // collapses to `null` like the other not-found cases.
        if start > end || end > host_text.len() {
            return None;
        }

        let host_tree = snapshot.tree();
        let host_language = self.document_language(&uri)?;

        // A tracker hit that fails to resolve in its minting layer means the
        // tree drifted out from under the tracked range (e.g. an edit
        // restructured the injection nesting). That is worth a warning for
        // diagnosing drift — mirroring `node/parent` and `node/children` — and
        // is distinct from the silent `None` cases above (never-issued ULID,
        // unparsed document), which are expected and collapse to `null` quietly.
        let Some(result) = with_resolved_node(
            &self.language,
            &host_language,
            host_text,
            host_tree,
            start,
            end,
            kind,
            layer,
            |node| f(node, host_text),
        ) else {
            log::warn!(
                target: "kakehashi::node",
                "tracker hit but no matching node in minting layer {} for ulid={} uri={} range=[{},{}) kind={}",
                layer, ulid, uri, start, end, kind
            );
            return None;
        };
        Some((uri, layer, result))
    }

    /// Mint (or reuse) a stable ULID for a related node in `layer` and shape it
    /// as a `NodeInfo`. Shared by every handler that returns a single resolved
    /// node, so the wire shape stays identical.
    pub(super) fn mint_node_info(&self, uri: &Url, layer: usize, triple: NodeTriple) -> Value {
        let (start, end, kind) = triple;
        let ulid = self
            .bridge
            .node_tracker()
            .get_or_create_in_layer(uri, start, end, kind, layer);
        json!({ "id": ulid.to_string(), "kind": kind })
    }

    /// Resolve `id`, run `f` to pick a single related node (child, sibling,
    /// descendant, …), and return its `NodeInfo` — or JSON `null`.
    ///
    /// `f` returns `None` when there is no such node (e.g. `next_sibling` on the
    /// last child), which is reported as `null` exactly like an unresolvable id:
    /// the protocol's universal null covers both. The result node lives in the
    /// same tree as the input, so it is re-minted in the **same** `layer`,
    /// keeping host and injected identities distinct (node-reference-protocol
    /// Scope rule, issue #313).
    pub(super) async fn navigate_to_node(
        &self,
        lsp_uri: &Uri,
        id: &str,
        f: impl FnMut(tree_sitter::Node<'_>) -> Option<NodeTriple>,
    ) -> Value {
        let Some((uri, layer, picked)) = self.with_node_by_id(lsp_uri, id, f).await else {
            return Value::Null;
        };
        match picked {
            Some(triple) => self.mint_node_info(&uri, layer, triple),
            None => Value::Null,
        }
    }

    /// Resolve `id`, run `f` to collect a list of related nodes (children,
    /// named children, field children, …), and return them as a `NodeInfo[]`.
    ///
    /// Returns JSON `null` only when the id itself does not resolve; a resolved
    /// node with no matching relatives yields `[]` (node-reference-protocol
    /// §"Empty children"). All results are minted in the input node's `layer`.
    pub(super) async fn navigate_to_nodes(
        &self,
        lsp_uri: &Uri,
        id: &str,
        f: impl FnMut(tree_sitter::Node<'_>) -> Vec<NodeTriple>,
    ) -> Value {
        let Some((uri, layer, items)) = self.with_node_by_id(lsp_uri, id, f).await else {
            return Value::Null;
        };
        let infos: Vec<Value> = items
            .into_iter()
            .map(|triple| self.mint_node_info(&uri, layer, triple))
            .collect();
        Value::Array(infos)
    }
}
