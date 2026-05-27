//! `kakehashi/node` — position → NodeInfo entry point (ADR-0025).
//!
//! Resolves a `Position` to the smallest tree-sitter node (named or anonymous)
//! containing that byte at the layer selected by the `injection` parameter
//! (ADR-0025 PR-4). When `injection` is absent or `false`/`0`, the host tree
//! is used. When `injection` is `true`, the deepest layer at the cursor is
//! used (saturating). When `injection` is a non-zero integer, the layer is
//! resolved via `stack[n]` for positive `n` and `stack[stack.len() + n]` for
//! negative `n`, with strict out-of-bounds returning `null`.
//!
//! Returns `null` (serialized as JSON `null`) when:
//! - the URI is unknown,
//! - the document has not yet been parsed (no tree),
//! - the position cannot be converted to a byte offset,
//! - the position is outside the document (`b > L`),
//! - the document is empty (`L == 0`),
//! - the requested `injection` layer does not exist at the position (strict
//!   integer index out of bounds), or
//! - the `injection` parameter has an unsupported JSON type (not bool, not
//!   an integer number).

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{Position, TextDocumentIdentifier};
use url::Url;

use crate::lsp::lsp_impl::kakehashi::node::injection_stack::injection_stack_at;
use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};
use crate::text::PositionMapper;

/// Request parameters for `kakehashi/node`.
///
/// The `injection` field is a `boolean | number` per ADR-0025 PR-4. We
/// deserialize it as a raw `Value` and dispatch on the JSON shape ourselves
/// because serde-tagged enums would reject the natural `true` / `1` / `-2`
/// shorthand the spec mandates.
///
/// `pub` is required because `Kakehashi::kakehashi_node` is registered as a
/// custom LSP method in the `kakehashi` binary, which lives outside the
/// library crate's visibility scope.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeParams {
    pub text_document: TextDocumentIdentifier,
    pub position: Position,
    /// `boolean | number` per ADR-0025 §"The `injection` Parameter". Absent /
    /// `false` / `0` selects the host layer; `true` saturates to the deepest
    /// layer; a non-zero integer indexes the stack strictly.
    #[serde(default)]
    pub injection: Option<Value>,
}

/// Layer selector parsed from the `injection` JSON parameter.
///
/// Keeping this as a small enum (rather than just an `i64`) lets the
/// handler keep the `true` saturation case and the strict-index case
/// visibly distinct — they share the result for `-1` but differ in
/// the bounds-check semantics ADR-0025 spells out.
enum InjectionSelector {
    /// Host layer (stack[0]). Triggered by absent, `false`, or `0`.
    Host,
    /// Saturate to the deepest layer at the cursor (`true`).
    Saturating,
    /// Strict integer index. Positive `n` -> `stack[n]`; negative `n` ->
    /// `stack[stack.len() + n]`. Out-of-bounds returns null at the caller.
    Index(i64),
    /// Unsupported JSON shape (non-bool / non-integer). Treated as null.
    Invalid,
}

/// Parse the `injection` parameter into an [`InjectionSelector`].
///
/// Per ADR-0025: `false` and `0` are equivalent (host); `true` saturates;
/// integer values index strictly. Anything else (string, array, fractional
/// number) is rejected as `Invalid` so the handler returns null with a log
/// warning, rather than silently coercing.
fn parse_injection_selector(value: Option<&Value>) -> InjectionSelector {
    match value {
        None | Some(Value::Null) => InjectionSelector::Host,
        Some(Value::Bool(true)) => InjectionSelector::Saturating,
        Some(Value::Bool(false)) => InjectionSelector::Host,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(0) => InjectionSelector::Host,
            Some(i) => InjectionSelector::Index(i),
            None => InjectionSelector::Invalid,
        },
        _ => InjectionSelector::Invalid,
    }
}

/// Resolve a non-zero integer index against a stack of length `stack_len`,
/// following ADR-0025 §"The `injection` Parameter":
///
/// - positive `n`: `stack[n]` directly, `null` if `n >= stack_len`
/// - negative `n`: `stack[stack_len + n]`, `null` if the result is < 0
///
/// `stack_len` must be at least 1 in practice (the host layer is always
/// present); we keep the signature general so callers can defensively
/// short-circuit on an empty stack without panicking.
fn resolve_index(n: i64, stack_len: usize) -> Option<usize> {
    if n > 0 {
        let idx = usize::try_from(n).ok()?;
        if idx < stack_len { Some(idx) } else { None }
    } else {
        // n is negative (n == 0 is handled by the Host branch upstream).
        // Convert via i64 arithmetic to avoid usize underflow on `len + n`.
        let len_i64 = i64::try_from(stack_len).ok()?;
        let resolved = len_i64.checked_add(n)?;
        if resolved < 0 {
            return None;
        }
        usize::try_from(resolved).ok()
    }
}

impl Kakehashi {
    /// Handler for `kakehashi/node`.
    pub async fn kakehashi_node(&self, params: NodeParams) -> Result<Value> {
        let lsp_uri = params.text_document.uri;
        let position = params.position;
        let injection = params.injection;

        // Parse the injection selector up-front so an invalid JSON shape
        // short-circuits before we touch the document store.
        let selector = parse_injection_selector(injection.as_ref());
        if matches!(selector, InjectionSelector::Invalid) {
            log::warn!(
                target: "kakehashi::node",
                "unsupported injection parameter shape: {:?}", injection
            );
            return Ok(Value::Null);
        }

        // URI conversion failure → null (ADR-0025 universal null semantics).
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::node", "invalid URI: {}", lsp_uri.as_str());
            return Ok(Value::Null);
        };

        // Ensure the document is parsed before snapshotting. `didOpen` inserts the
        // document with `tree: None` and schedules an async parse; a client that
        // calls `kakehashi/node` quickly afterwards must not race with that parse.
        self.ensure_parsed_for_node_lookup(&uri).await;

        // Snapshot the document so we hold the read lock for as short as possible.
        let snapshot = match self.documents.get(&uri).and_then(|doc| doc.snapshot()) {
            Some(s) => s,
            None => {
                log::debug!(target: "kakehashi::node", "no parsed document for {}", uri);
                return Ok(Value::Null);
            }
        };

        let text = snapshot.text();
        let tree = snapshot.tree();
        let mapper = PositionMapper::new(text);

        // Empty document: end-of-document exception is gated on `L > 0`,
        // and any position is either at byte 0 (no node spans `[0, 0)`) or
        // out of bounds. ADR-0025 explicitly says empty documents return null.
        let doc_len = text.len();
        if doc_len == 0 {
            return Ok(Value::Null);
        }

        // Convert LSP position (UTF-16 code units) to a UTF-8 byte offset.
        let Some(byte) = mapper.position_to_byte(position) else {
            return Ok(Value::Null);
        };
        if byte > doc_len {
            return Ok(Value::Null);
        }

        // Host layer fast path: skip the stack enumeration entirely. This
        // keeps the no-injection request shape (the dominant case for plain
        // documents) at PR-1 cost.
        if matches!(selector, InjectionSelector::Host) {
            return Ok(self.resolve_host_layer_node(&uri, tree, byte, doc_len));
        }

        // We need the host language to seed `injection_stack_at` with the
        // right injection query. Detect it from the document store.
        let Some(host_language) = self.document_language(&uri) else {
            log::debug!(
                target: "kakehashi::node",
                "no host language detected for {} — cannot resolve injection layers",
                uri
            );
            return Ok(Value::Null);
        };

        // Ensure injection-language parsers are loaded. PR-1 only loaded
        // the host parser on-demand; injection layers require their own
        // grammars to be available before `injection_stack_at` can parse
        // them. Skip the expensive load for the host-only path above.
        self.ensure_injection_languages_loaded(&uri, &host_language, text)
            .await;

        let stack = injection_stack_at(&self.language, &host_language, text, tree, byte);

        let layer_index = match selector {
            InjectionSelector::Host => unreachable!("handled above"),
            InjectionSelector::Invalid => unreachable!("handled above"),
            InjectionSelector::Saturating => {
                // `true` saturates to the deepest layer. The stack always
                // contains at least the host (layer 0), so this never
                // under-indexes.
                stack.len() - 1
            }
            InjectionSelector::Index(n) => {
                let Some(idx) = resolve_index(n, stack.len()) else {
                    return Ok(Value::Null);
                };
                idx
            }
        };

        let Some(layer) = stack.get(layer_index) else {
            return Ok(Value::Null);
        };

        let Some(node) = smallest_containing_node(&layer.tree, byte, doc_len) else {
            return Ok(Value::Null);
        };

        let ulid = self.bridge.node_tracker().get_or_create(
            &uri,
            node.start_byte(),
            node.end_byte(),
            node.kind(),
        );

        Ok(json!({
            "id": ulid.to_string(),
            "type": node.kind(),
        }))
    }

    /// Host-layer lookup, factored out so the no-injection request keeps
    /// the same shape it had in PR-1.
    fn resolve_host_layer_node(
        &self,
        uri: &Url,
        tree: &tree_sitter::Tree,
        byte: usize,
        doc_len: usize,
    ) -> Value {
        let Some(node) = smallest_containing_node(tree, byte, doc_len) else {
            return Value::Null;
        };

        let ulid = self.bridge.node_tracker().get_or_create(
            uri,
            node.start_byte(),
            node.end_byte(),
            node.kind(),
        );

        json!({
            "id": ulid.to_string(),
            "type": node.kind(),
        })
    }

    /// Ensure parsers for every injection language reachable from this
    /// document are loaded. `didOpen` triggers this via `process_injections`,
    /// but a client may call `kakehashi/node` quickly enough to race that
    /// load — re-invoke it here defensively so PR-4's injection-aware path
    /// has a parser to work with.
    ///
    /// Cheap when languages are already loaded (`ensure_language_loaded`
    /// short-circuits on a registry hit).
    async fn ensure_injection_languages_loaded(&self, uri: &Url, host_language: &str, _text: &str) {
        use std::collections::HashSet;

        // Re-resolve the injection set from the host language's injection
        // query. We could mine this from `injection_stack_at` but we'd then
        // have to plumb it back; the duplicate work is negligible and keeps
        // the stack helper synchronous-only.
        let coordinator = self.injection_coordinator();
        let injections = coordinator.resolve_injection_data(uri, host_language);
        if injections.is_empty() {
            return;
        }
        let languages: HashSet<String> = injections.into_iter().map(|i| i.language).collect();
        coordinator
            .check_injected_languages_auto_install(uri, &languages)
            .await;
    }

    /// Parse the document on-demand if its tree has not been built yet.
    ///
    /// `didOpen` inserts the document immediately with `tree: None` and
    /// schedules an asynchronous parse. `kakehashi/node` requests issued
    /// straight after `didOpen` would otherwise race with that parse and
    /// see `snapshot()` return `None`. This helper mirrors the on-demand
    /// parsing path used by `selection_range_impl`: load the language,
    /// parse via the shared pool, and update the document store atomically.
    ///
    /// Race protection: an in-flight `didChange` parse sets `has_tree=false`
    /// via `mark_parse_started` while the old `Document::tree()` may briefly
    /// remain populated. Waiting on `wait_for_parse_completion` first guarantees
    /// the snapshot returned by the caller is the *current* (text, tree) pair,
    /// not a stale combination produced mid-parse. The timeout matches the
    /// `semantic_tokens` budget so this helper stays responsive even if the
    /// parser hangs on a pathological input.
    pub(super) async fn ensure_parsed_for_node_lookup(&self, uri: &Url) {
        self.documents
            .wait_for_parse_completion(uri, std::time::Duration::from_millis(200))
            .await;

        // If a tree is now available (either it always was, or didChange's
        // parse just finished), nothing to do.
        if let Some(doc) = self.documents.get(uri)
            && doc.tree().is_some()
        {
            return;
        }

        let Some(language_name) = self.document_language(uri) else {
            return;
        };

        let load_result = self.language.ensure_language_loaded(&language_name);
        if !load_result.success {
            return;
        }

        // Take a fresh read to grab the current text (the doc may still be missing a tree).
        let Some(doc) = self.documents.get(uri) else {
            return;
        };
        let text = doc.text().to_string();
        drop(doc);

        let text_clone = text.clone();
        let parsed = self
            .parse_coordinator()
            .parse_with_pool(&language_name, uri, text.len(), move |mut parser| {
                let tree = parser.parse(&text_clone, None);
                (parser, tree)
            })
            .await;

        if let Some(tree) = parsed {
            // Race guard: between the text snapshot above and parse completion,
            // a didChange may have updated the document. Storing our tree would
            // associate it with stale text, breaking the (text, tree) consistency
            // invariant. Compare against the current text and discard if it has
            // moved; the next request will re-trigger the parse against the
            // newer text.
            let text_unchanged = self
                .documents
                .get(uri)
                .map(|doc| doc.text() == text)
                .unwrap_or(false);
            if text_unchanged {
                self.documents
                    .update_document(uri.clone(), text, Some(tree));
            } else {
                log::debug!(
                    target: "kakehashi::node",
                    "discarding on-demand parse for {} — text changed during parse",
                    uri
                );
            }
        }
    }
}

/// Find the smallest node containing `byte` under the half-open `[start, end)` rule,
/// with the ADR-0025 end-of-document exception.
///
/// PR-1 only honours the exception case at the document end; the rest of the
/// lookup uses tree-sitter's `descendant_for_byte_range(byte, byte)`, which
/// already returns the smallest containing node when given equal start/end.
fn smallest_containing_node(
    tree: &tree_sitter::Tree,
    byte: usize,
    doc_len: usize,
) -> Option<tree_sitter::Node<'_>> {
    let root = tree.root_node();

    // End-of-document exception (ADR-0025 §"End-of-Document Exception"):
    //   gated on doc_len > 0. The empty-document path returns null earlier.
    if byte == doc_len {
        // Pick the smallest descendant whose end_byte == doc_len.
        // Tree-sitter's `descendant_for_byte_range(L, L)` returns None at end-of-document
        // because no node strictly contains the past-the-end byte. Walk the right spine
        // of the root manually instead.
        //
        // Guard against pathological trees whose root end_byte < doc_len (trailing
        // bytes that the parser failed to attach to any node, e.g. an unparsed
        // tail after an error). In that case there is no node whose end coincides
        // with the document end and the exception cannot apply.
        let candidate = deepest_node_ending_at(root, doc_len);
        return if candidate.end_byte() == doc_len {
            Some(candidate)
        } else {
            None
        };
    }

    // Standard half-open lookup: smallest node with start_byte <= byte < end_byte.
    let node = root.descendant_for_byte_range(byte, byte)?;

    // Defensive check: tree-sitter may return a node whose end_byte equals `byte`
    // when there is no smaller descendant — half-open semantics say such a cursor
    // is *outside* that node. Walk up until we find one that properly contains it,
    // or fall back to null.
    let mut current = Some(node);
    while let Some(n) = current {
        if n.start_byte() <= byte && byte < n.end_byte() {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// Walk down the right spine of `node`, returning the deepest descendant whose
/// `end_byte` equals `target_end`. Used for the end-of-document exception.
///
/// Implementation note: tree-sitter direct siblings are non-overlapping with
/// monotonically non-decreasing `end_byte`, so among children only the LAST
/// can match `target_end == parent.end_byte()`. Navigating to the last child
/// via a `TreeCursor` is O(children-per-level), giving overall O(depth × max
/// breadth) instead of the O(N²) `current.child(i).rev()` pattern.
fn deepest_node_ending_at(node: tree_sitter::Node<'_>, target_end: usize) -> tree_sitter::Node<'_> {
    let mut cursor = node.walk();
    let mut current = node;
    while cursor.goto_first_child() {
        while cursor.goto_next_sibling() {}
        let last_child = cursor.node();
        if last_child.end_byte() == target_end {
            current = last_child;
        } else {
            break;
        }
    }
    current
}
