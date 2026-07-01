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

use crate::lsp::lsp_impl::kakehashi::node::injection_stack::{
    with_resolved_node_pair, with_resolved_node_ranges,
};
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

/// Request parameters for the two-id accessor `childWithDescendant`
/// (issue #335): `id` names the prospective ancestor, `descendantId` the node
/// whose containing immediate child is requested. Both ids must have been
/// minted in the same injection layer; a cross-layer pair collapses to `null`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDescendantParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
    pub descendant_id: String,
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
    pub(super) async fn with_node_by_id<R: Send + 'static>(
        &self,
        lsp_uri: &Uri,
        id: &str,
        mut f: impl FnMut(tree_sitter::Node<'_>) -> R + Send + 'static,
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
    pub(super) async fn with_node_text<R: Send + 'static>(
        &self,
        lsp_uri: &Uri,
        id: &str,
        mut f: impl FnMut(tree_sitter::Node<'_>, &str) -> R + Send + 'static,
    ) -> Option<(Url, usize, R)> {
        // Most accessors don't need the minting layer's included ranges.
        self.with_node_text_ranges(lsp_uri, id, move |node, text, _ranges| f(node, text))
            .await
    }

    /// Like [`with_node_text`](Self::with_node_text) but additionally hands the
    /// closure the included ranges the minting layer's tree was parsed against
    /// (host coordinates; the whole document for layer 0). The coordinate-input
    /// accessors use them to reject byte/point arguments that land in an
    /// injected layer's excluded gaps — e.g. blockquote `> ` prefixes — which
    /// are inside a node's contiguous span but are not injected content (#341).
    pub(super) async fn with_node_text_ranges<R: Send + 'static>(
        &self,
        lsp_uri: &Uri,
        id: &str,
        mut f: impl FnMut(tree_sitter::Node<'_>, &str, &[tree_sitter::Range]) -> R + Send + 'static,
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
        self.ensure_document_parsed(&uri).await;

        // Snapshot so we operate on a consistent (text, tree) pair.
        let snapshot = self.documents.get(&uri).and_then(|doc| doc.snapshot())?;
        let (host_text, host_tree, _incarnation) = snapshot.into_parts();

        // Defensively reject an invalid or out-of-bounds tracked range before any
        // tree work: an inverted range or one extending past the current text
        // can't name a real node (and could panic byte slicing downstream). This
        // collapses to `null` like the other not-found cases.
        if start > end || end > host_text.len() {
            return None;
        }

        let host_language = self.document_language(&uri)?;

        // Resolving the minting layer re-runs the injection query over the host
        // tree (O(regions)) and re-parses the containing layer chain —
        // synchronous tree-CPU, run as a compute-pool work-unit together with
        // the accessor closure over the resolved node (parse-snapshot ADR §4).
        let language = std::sync::Arc::clone(&self.language);
        let resolved = self
            .compute_pool
            .run(None, move || {
                with_resolved_node_ranges(
                    &language,
                    &host_language,
                    &host_text,
                    &host_tree,
                    start,
                    end,
                    kind,
                    layer,
                    |node, ranges| f(node, &host_text, ranges),
                )
            })
            .await?; // outer None = the work-unit panicked (logged by the pool)

        // A tracker hit that fails to resolve in its minting layer means the
        // tree drifted out from under the tracked range (e.g. an edit
        // restructured the injection nesting). That is worth a warning for
        // diagnosing drift — mirroring `node/parent` and `node/children` — and
        // is distinct from the silent `None` cases above (never-issued ULID,
        // unparsed document), which are expected and collapse to `null` quietly.
        let Some(result) = resolved else {
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
    pub(in crate::lsp::lsp_impl::kakehashi) fn mint_node_info(
        &self,
        uri: &Url,
        layer: usize,
        triple: NodeTriple,
    ) -> Value {
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
        f: impl FnMut(tree_sitter::Node<'_>) -> Option<NodeTriple> + Send + 'static,
    ) -> Value {
        let Some((uri, layer, picked)) = self.with_node_by_id(lsp_uri, id, f).await else {
            return Value::Null;
        };
        match picked {
            Some(triple) => self.mint_node_info(&uri, layer, triple),
            None => Value::Null,
        }
    }

    /// Like [`navigate_to_node`](Self::navigate_to_node), but `f` also receives
    /// the host text and the minting layer's included ranges. Used by the
    /// coordinate-input accessors so they can reject byte/point arguments
    /// landing in an injected layer's excluded gaps (#341).
    pub(super) async fn navigate_to_node_in_ranges(
        &self,
        lsp_uri: &Uri,
        id: &str,
        f: impl FnMut(tree_sitter::Node<'_>, &str, &[tree_sitter::Range]) -> Option<NodeTriple>
        + Send
        + 'static,
    ) -> Value {
        let Some((uri, layer, picked)) = self.with_node_text_ranges(lsp_uri, id, f).await else {
            return Value::Null;
        };
        match picked {
            Some(triple) => self.mint_node_info(&uri, layer, triple),
            None => Value::Null,
        }
    }

    /// Resolve `id` **and** `descendant_id` in the layer that minted them, run
    /// `f` on the pair to pick a related node, and return its `NodeInfo` — or
    /// JSON `null` (issue #335, two-id contract).
    ///
    /// On top of the single-id null cases (invalid URI, malformed/unknown
    /// ULID, unparsed document, drifted range), the pair collapses to `null`
    /// when the two ids were minted in **different** layers: both nodes must
    /// live in one tree for tree-sitter's ancestor/descendant queries to be
    /// meaningful, and resolving them against different layers' trees would
    /// break the per-layer Scope rule. The result is re-minted in that shared
    /// layer, like every other navigation accessor.
    pub(super) async fn navigate_with_descendant(
        &self,
        lsp_uri: &Uri,
        id: &str,
        descendant_id: &str,
        f: impl FnMut(tree_sitter::Node<'_>, tree_sitter::Node<'_>) -> Option<NodeTriple>
        + Send
        + 'static,
    ) -> Value {
        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!(target: "kakehashi::node", "invalid URI: {}", lsp_uri.as_str());
            return Value::Null;
        };

        // Either ULID being malformed collapses to null, like a never-issued id.
        let Ok(ulid) = id.parse::<Ulid>() else {
            return Value::Null;
        };
        let Ok(descendant_ulid) = descendant_id.parse::<Ulid>() else {
            return Value::Null;
        };

        let tracker = self.bridge.node_tracker();
        let Some((start, end, kind, layer)) = tracker.lookup_node(&uri, &ulid) else {
            return Value::Null;
        };
        let Some((desc_start, desc_end, desc_kind, desc_layer)) =
            tracker.lookup_node(&uri, &descendant_ulid)
        else {
            return Value::Null;
        };
        // Two-id same-layer contract: ids minted in different layers never
        // share a tree, so the relation is undefined — null, not an error.
        if layer != desc_layer {
            return Value::Null;
        }

        // Same didOpen race guard as the single-id prelude.
        self.ensure_document_parsed(&uri).await;
        let Some(snapshot) = self.documents.get(&uri).and_then(|doc| doc.snapshot()) else {
            return Value::Null;
        };
        let host_text = snapshot.text();
        let host_tree = snapshot.tree();
        let Some(host_language) = self.document_language(&uri) else {
            return Value::Null;
        };

        // Silent stale-range guard, mirroring the single-id prelude: an
        // invalid tracked range can't name a real node and must collapse to
        // null *without* the drift warning below — `with_resolved_node_pair`
        // would also reject it, but through the warn-logging arm.
        if start > end
            || end > host_text.len()
            || desc_start > desc_end
            || desc_end > host_text.len()
        {
            return Value::Null;
        }

        let Some(picked) = with_resolved_node_pair(
            &self.language,
            &host_language,
            host_text,
            host_tree,
            (start, end, kind),
            (desc_start, desc_end, desc_kind),
            layer,
            f,
        ) else {
            // Unlike the single-id drift warning, pair-resolution failure is
            // an expected outcome, not just drift: ids minted at the same
            // depth in *different* regions (two separate code blocks, or the
            // #350 overlap caveat) legitimately fail to share a tree and
            // collapse to the contract's null. debug, not warn — a normal
            // cross-region query must not look like document drift in logs.
            log::debug!(
                target: "kakehashi::node",
                "pair did not resolve in one minting-layer tree (layer {}) for uri={} self=[{},{}) {} descendant=[{},{}) {}",
                layer, uri, start, end, kind, desc_start, desc_end, desc_kind
            );
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
        f: impl FnMut(tree_sitter::Node<'_>) -> Vec<NodeTriple> + Send + 'static,
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
