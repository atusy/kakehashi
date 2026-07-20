//! `kakehashi/node` — position → NodeInfo entry point (node-reference-protocol).
//!
//! Resolves a `Position` to the smallest tree-sitter node (named or anonymous)
//! containing that byte at the layer selected by the `injection` parameter
//! (node-reference-protocol PR-4). When `injection` is absent or `false`/`0`, the host tree
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
/// The `injection` field is a `boolean | number` per node-reference-protocol PR-4. We
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
    /// `boolean | number` per node-reference-protocol §"The `injection` Parameter". Absent /
    /// `false` / `0` selects the host layer; `true` saturates to the deepest
    /// layer; a non-zero integer indexes the stack strictly.
    ///
    /// We deserialize via a custom helper so that explicit JSON `null`
    /// reaches the handler as `Some(Value::Null)` (rejected as invalid) while
    /// an absent field stays `None` (defaults to host). Plain
    /// `Option<Value>` would collapse the two — serde's default Option
    /// deserialization treats `null` and missing identically — which would
    /// silently accept an unsupported shape against node-reference-protocol.
    #[serde(default, deserialize_with = "deserialize_present_value")]
    pub injection: Option<Value>,
}

/// Deserialize a field as `Some(Value)` whenever it's *present* in the JSON,
/// even when the value is explicit `null`. Combined with `#[serde(default)]`
/// this lets the caller distinguish a missing field (`None`) from an
/// explicit `null` (`Some(Value::Null)`).
fn deserialize_present_value<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// Layer selector parsed from the `injection` JSON parameter.
///
/// Keeping this as a small enum (rather than just an `i64`) lets the
/// handler keep the `true` saturation case and the strict-index case
/// visibly distinct — they share the result for `-1` but differ in
/// the bounds-check semantics node-reference-protocol spells out.
#[derive(Clone, Copy)]
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

impl InjectionSelector {
    fn worker_layer(self) -> crate::tree_worker::NodeLayerSelector {
        match self {
            Self::Host => crate::tree_worker::NodeLayerSelector::Host,
            Self::Saturating => crate::tree_worker::NodeLayerSelector::Deepest,
            Self::Index(index) => crate::tree_worker::NodeLayerSelector::Index(index),
            Self::Invalid => unreachable!("invalid selectors return before worker dispatch"),
        }
    }
}

/// Parse the `injection` parameter into an [`InjectionSelector`].
///
/// Per node-reference-protocol: `false` and `0` are equivalent (host); `true` saturates;
/// integer values index strictly. Anything else (string, array, fractional
/// number, **explicit JSON `null`**) is rejected as `Invalid` so the handler
/// returns null with a log warning, rather than silently coercing. An absent
/// field (`None`) defaults to host per the spec.
fn parse_injection_selector(value: Option<&Value>) -> InjectionSelector {
    match value {
        None => InjectionSelector::Host,
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
/// following node-reference-protocol §"The `injection` Parameter":
///
/// - positive `n`: `stack[n]` directly, `null` if `n >= stack_len`
/// - negative `n`: `stack[stack_len + n]`, `null` if the result is < 0
///
/// `stack_len` must be at least 1 in practice (the host layer is always
/// present); we keep the signature general so callers can defensively
/// short-circuit on an empty stack without panicking.
fn resolve_index(n: i64, stack_len: usize) -> Option<usize> {
    if n >= 0 {
        // Non-negative: direct stack[n] lookup. n == 0 is normally routed
        // through the Host branch upstream, but accepting it here makes the
        // helper self-contained — `stack[0]` is the host layer, so the result
        // is correct either way.
        let idx = usize::try_from(n).ok()?;
        if idx < stack_len { Some(idx) } else { None }
    } else {
        // Negative: stack[stack.len + n]. Convert via i64 to avoid usize
        // underflow when |n| > stack_len.
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
        self.kakehashi_node_after_injection_load(params, std::future::ready(()))
            .await
    }

    async fn kakehashi_node_after_injection_load<F>(
        &self,
        params: NodeParams,
        after_injection_load: F,
    ) -> Result<Value>
    where
        F: std::future::Future<Output = ()>,
    {
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

        // URI conversion failure → null (node-reference-protocol universal null semantics).
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::node", "invalid URI: {}", lsp_uri.as_str());
            return Ok(Value::Null);
        };

        if self.tree_worker_shadow.is_authoritative() {
            let Some((text, incarnation, content_version, language_name)) =
                self.documents.get(&uri).and_then(|document| {
                    let text = document.text_arc();
                    let language = self.language.detect_language(
                        uri.path(),
                        &text,
                        None,
                        document.language_id(),
                    )?;
                    Some((
                        text,
                        document.incarnation(),
                        document.content_version(),
                        language,
                    ))
                })
            else {
                return Ok(Value::Null);
            };
            if text.is_empty() {
                return Ok(Value::Null);
            }
            let Some(byte) = PositionMapper::new(&text).position_to_byte(position) else {
                return Ok(Value::Null);
            };
            let load = self
                .language
                .ensure_language_loaded_async(&language_name)
                .await;
            if !load.success {
                return Ok(Value::Null);
            }
            let Some(current) = self.documents.get(&uri) else {
                return Ok(Value::Null);
            };
            if current.incarnation() != incarnation || current.content_version() != content_version
            {
                return Ok(Value::Null);
            }
            drop(current);
            let Some(grammar) = self.language.worker_grammar_descriptor(&language_name) else {
                return Ok(Value::Null);
            };
            let generation = self.language.configuration_generation();
            if self.tree_worker_shadow.needs_document_sync(
                &uri,
                incarnation,
                content_version,
                generation,
                &grammar.queries,
            ) {
                self.refresh_tree_worker_documents(std::slice::from_ref(&uri));
            }
            let worker = self
                .tree_worker_shadow
                .resolve_node(
                    &uri,
                    incarnation,
                    content_version,
                    generation,
                    byte,
                    selector.worker_layer(),
                )
                .await;
            if let Some(result) = worker.as_ref() {
                for node in &result.nodes {
                    let public = serde_json::json!({ "id": node.id.local_id, "kind": node.kind });
                    self.tree_worker_shadow
                        .record_node_mapping(&uri, &public, node);
                }
            }
            return Ok(self.tree_worker_shadow.public_node_result(worker, false));
        }

        // Resolve a CURRENT parse snapshot (parse-snapshot ADR §3): the
        // request's position is authored against the live text, so a trailing
        // snapshot rejects immediately with the universal null; only the
        // bounded first-parse wait applies. Currency keeps the mint below
        // sound (a stale read never mints).
        let Some(snapshot) = self.current_snapshot(&uri).await else {
            log::debug!(target: "kakehashi::node", "no current parse snapshot for {}", uri);
            return Ok(Value::Null);
        };
        let incarnation = snapshot.incarnation;

        let text: &str = &snapshot.text;
        let Some(tree) = snapshot.tree.as_ref() else {
            return Ok(Value::Null);
        };
        let mapper = PositionMapper::new(text);

        // Empty document: end-of-document exception is gated on `L > 0`,
        // and any position is either at byte 0 (no node spans `[0, 0)`) or
        // out of bounds. node-reference-protocol explicitly says empty documents return null.
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
            let worker_configuration_generation = self.language.configuration_generation();
            if let Some(language) = snapshot.language.as_deref()
                && let Some(grammar) = self.language.worker_grammar_descriptor(language)
                && self.tree_worker_shadow.needs_document_sync(
                    &uri,
                    incarnation,
                    snapshot.parsed_version,
                    worker_configuration_generation,
                    &grammar.queries,
                )
            {
                self.refresh_tree_worker_documents(std::slice::from_ref(&uri));
            }
            let authoritative =
                self.resolve_host_layer_node(&uri, tree, byte, doc_len, incarnation);
            let worker = self
                .tree_worker_shadow
                .resolve_node(
                    &uri,
                    incarnation,
                    snapshot.parsed_version,
                    worker_configuration_generation,
                    byte,
                    crate::tree_worker::NodeLayerSelector::Host,
                )
                .await;
            if let Some(shadow) = worker.as_ref()
                && let Some(node) = shadow.nodes.first()
            {
                let authoritative_kind = authoritative.get("kind").and_then(Value::as_str);
                let authoritative_identity = authoritative
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<ulid::Ulid>().ok())
                    .and_then(|id| self.bridge.node_tracker().lookup_node(&uri, &id));
                let identities_match = authoritative_identity.is_some_and(
                    |(start, end, kind, layer, tracked_incarnation)| {
                        start == node.start_byte
                            && end == node.end_byte
                            && kind == node.kind
                            && layer == 0
                            && node.layer == 0
                            && tracked_incarnation == incarnation
                    },
                );
                if authoritative_kind != Some(node.kind.as_str()) || !identities_match {
                    log::debug!(
                        target: "kakehashi::tree_worker_shadow",
                        "host node mismatch uri={} version={} byte={} authoritative={:?} worker=({}, {}, {})",
                        uri,
                        snapshot.parsed_version,
                        byte,
                        authoritative_identity,
                        node.start_byte,
                        node.end_byte,
                        node.kind,
                    );
                } else {
                    self.tree_worker_shadow
                        .record_node_mapping(&uri, &authoritative, node);
                }
            }
            if !self.documents.latest_snapshot(&uri).is_some_and(|view| {
                view.content_version == snapshot.parsed_version
                    && view.slot.current_incarnation == incarnation
            }) {
                return Ok(Value::Null);
            }
            return if self.tree_worker_shadow.is_authoritative() {
                Ok(self.tree_worker_shadow.public_node_result(worker, false))
            } else {
                Ok(authoritative)
            };
        }

        // We need the host language to seed `injection_stack_at` with the
        // right injection query — the snapshot's own detected language.
        let Some(host_language) = snapshot.language.clone() else {
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
        //
        // The pre-await snapshot (`text` / `tree` / `byte`) is fine for
        // *discovering* which grammars to install, but must NOT be reused to
        // mint a ULID: a `didChange` processed while grammars install would
        // adjust the tracker, leaving our stale byte ranges un-adjusted and
        // minting an id for bytes the edit moved.
        self.ensure_injection_languages_loaded(&uri, &host_language, text, tree, byte, incarnation)
            .await;
        after_injection_load.await;

        // Re-resolve a CURRENT snapshot after the await and recompute the
        // position mapping: a `didChange` processed during the (awaited)
        // grammar load moved the version on, and the mint below must run
        // against the live state only. From here on we operate strictly on the
        // post-await snapshot.
        let Some(snapshot) = self.current_snapshot(&uri).await else {
            log::debug!(target: "kakehashi::node", "no current parse snapshot for {} after load", uri);
            return Ok(Value::Null);
        };
        if snapshot.incarnation != incarnation {
            return Ok(Value::Null);
        }
        let Some(host_language) = snapshot.language.clone() else {
            return Ok(Value::Null);
        };
        let incarnation = snapshot.incarnation;
        let text = std::sync::Arc::clone(&snapshot.text);
        let Some(tree) = snapshot.tree.clone() else {
            return Ok(Value::Null);
        };
        let doc_len = text.len();
        if doc_len == 0 {
            return Ok(Value::Null);
        }
        let mapper = PositionMapper::new(&text);
        let Some(byte) = mapper.position_to_byte(position) else {
            return Ok(Value::Null);
        };
        if byte > doc_len {
            return Ok(Value::Null);
        }

        // The stack enumeration re-runs the injection query over the host tree
        // (O(regions)) and re-parses each containing layer — synchronous
        // tree-CPU, run as a compute-pool work-unit (parse-snapshot ADR §4),
        // together with the layer selection and the mint over its result.
        let language = std::sync::Arc::clone(&self.language);
        let tracker = self.bridge.node_tracker_arc();
        let worker_configuration_generation = self.language.configuration_generation();
        if let Some(grammar) = self.language.worker_grammar_descriptor(&host_language)
            && self.tree_worker_shadow.needs_document_sync(
                &uri,
                incarnation,
                snapshot.parsed_version,
                worker_configuration_generation,
                &grammar.queries,
            )
        {
            self.refresh_tree_worker_documents(std::slice::from_ref(&uri));
        }
        let worker = self.tree_worker_shadow.resolve_node(
            &uri,
            incarnation,
            snapshot.parsed_version,
            worker_configuration_generation,
            byte,
            selector.worker_layer(),
        );
        let compute_uri = uri.clone();
        let authoritative = self.compute_pool.run(None, move || {
            let stack = injection_stack_at(&language, &host_language, &text, &tree, byte);

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
                        return Value::Null;
                    };
                    idx
                }
            };

            let Some(layer) = stack.get(layer_index) else {
                return Value::Null;
            };

            let Some(node) = smallest_containing_node(&layer.tree, byte, doc_len) else {
                return Value::Null;
            };

            // Mint with the resolved layer index so a host and injected node
            // sharing (start, end, kind) get distinct ULIDs and stay navigable
            // in their own tree (lazy-node-identity-tracking §"Node Uniqueness
            // Key", issue #313).
            let ulid = tracker.get_or_create_in_layer_for_incarnation(
                &compute_uri,
                node.start_byte(),
                node.end_byte(),
                node.kind(),
                layer_index,
                incarnation,
            );
            let Some(ulid) = ulid else {
                return Value::Null;
            };

            json!({
                "id": ulid.to_string(),
                "kind": node.kind(),
            })
        });
        let (worker, result) = tokio::join!(worker, authoritative);

        // None = the work-unit panicked (logged by the pool) → protocol null.
        let authoritative = result.unwrap_or(Value::Null);
        if let Some(worker) = worker.as_ref() {
            let worker_node = worker.nodes.first();
            let authoritative_identity = authoritative
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<ulid::Ulid>().ok())
                .and_then(|id| self.bridge.node_tracker().lookup_node(&uri, &id));
            let matches = match (authoritative_identity, worker_node) {
                (None, None) => true,
                (Some((start, end, kind, layer, tracked_incarnation)), Some(node)) => {
                    start == node.start_byte
                        && end == node.end_byte
                        && kind == node.kind
                        && layer == node.layer
                        && tracked_incarnation == incarnation
                        && match selector {
                            InjectionSelector::Host => layer == 0,
                            InjectionSelector::Saturating | InjectionSelector::Index(_) => true,
                            InjectionSelector::Invalid => unreachable!(),
                        }
                }
                _ => false,
            };
            if matches {
                log::debug!(
                    target: "kakehashi::tree_worker_shadow",
                    "injection node matched uri={} version={} byte={}",
                    uri,
                    snapshot.parsed_version,
                    byte,
                );
                if let Some(node) = worker_node {
                    self.tree_worker_shadow
                        .record_node_mapping(&uri, &authoritative, node);
                }
            } else {
                log::debug!(
                    target: "kakehashi::tree_worker_shadow",
                    "injection node mismatch uri={} version={} byte={} authoritative={:?} worker={:?}",
                    uri,
                    snapshot.parsed_version,
                    byte,
                    authoritative_identity,
                    worker_node,
                );
            }
        }
        if self.tree_worker_shadow.is_authoritative() {
            Ok(self.tree_worker_shadow.public_node_result(worker, false))
        } else {
            Ok(authoritative)
        }
    }

    /// Host-layer lookup, factored out so the no-injection request keeps
    /// the same shape it had in PR-1.
    fn resolve_host_layer_node(
        &self,
        uri: &Url,
        tree: &tree_sitter::Tree,
        byte: usize,
        doc_len: usize,
        incarnation: u64,
    ) -> Value {
        let Some(node) = smallest_containing_node(tree, byte, doc_len) else {
            return Value::Null;
        };

        let ulid = self.bridge.node_tracker().get_or_create_for_incarnation(
            uri,
            node.start_byte(),
            node.end_byte(),
            node.kind(),
            incarnation,
        );
        let Some(ulid) = ulid else {
            return Value::Null;
        };

        json!({
            "id": ulid.to_string(),
            "kind": node.kind(),
        })
    }

    /// Ensure parsers for every injection language **along the cursor's
    /// injection path** are loaded. `didOpen` triggers a first-level load via
    /// `process_injections`, but a client may call `kakehashi/node` quickly
    /// enough to race it, and nested grammars (Markdown → Python → Regex) are
    /// never first-level. Re-run defensively here so PR-4's injection-aware
    /// path has parsers for the whole chain at `byte`.
    ///
    /// Discovery is a fixpoint over `collect_injection_languages_at`, which can
    /// only parse *into* layers whose grammar is already loaded, so each round
    /// surfaces the next language on the cursor's path. We auto-install each
    /// round's newly-seen languages, then recollect — converging once a round
    /// adds nothing new (or the depth cap is hit). Localized to `byte` so the
    /// cost scales with nesting depth, not the number of injections in the
    /// document.
    async fn ensure_injection_languages_loaded(
        &self,
        uri: &Url,
        host_language: &str,
        text: &str,
        host_tree: &tree_sitter::Tree,
        byte: usize,
        incarnation: u64,
    ) {
        use std::collections::HashSet;

        let coordinator = self.injection_coordinator();
        let mut seen: HashSet<String> = HashSet::new();

        // Bound the outer loop independently of the per-branch depth cap inside
        // `collect_injection_languages_at`; MAX rounds is generous since each
        // round must reveal at least one new language to continue.
        for _round in 0..crate::language::injection::MAX_INJECTION_DEPTH {
            let discovered =
                crate::lsp::lsp_impl::kakehashi::node::injection_stack::collect_injection_languages_at(
                    &self.language,
                    host_language,
                    text,
                    host_tree,
                    byte,
                );
            // Re-read the registry *each round* and drop it before the await:
            // a snapshot taken once would not reflect the grammars installed by
            // the previous round, so already-installed languages would keep
            // looking "missing" and rely on `seen` alone. Holding it across the
            // `check_injected_languages_auto_install` await would also risk
            // writer contention while the install writes to the registry.
            //
            // Only languages still missing need a round: an already-loaded one
            // was *also* descended into during this same `collect` call, so it
            // never gates discovery of a deeper tier. `seen` still guards a
            // failed install from being retried every round.
            let registry = self.language.language_registry_for_parallel();
            let fresh: HashSet<String> = discovered
                .into_iter()
                .filter(|lang| registry.get(lang).is_none())
                .filter(|lang| !seen.contains(lang))
                .collect();
            drop(registry);
            if fresh.is_empty() {
                break;
            }
            coordinator
                .check_injected_languages_auto_install(uri, &fresh, incarnation)
                .await;
            seen.extend(fresh);
        }
    }

    /// Bounded wait for `uri`'s parse snapshot to become **current** — the
    /// shared post-edit freshness helper for handlers that still read the
    /// legacy store tree. It never parses inline (parse-snapshot ADR §3:
    /// readers never parse; the former on-demand fallback was one of the two
    /// reader resurrection vectors). If the off-ingress reparse does not land
    /// within the wait, the caller's subsequent store read simply finds no
    /// tree and degrades to its empty fallback, self-correcting on the
    /// client's next request.
    pub(crate) async fn ensure_document_parsed(&self, uri: &Url) {
        let _ = self
            .wait_for_current_snapshot(uri, std::time::Duration::from_millis(200))
            .await;
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use tower_lsp_server::LspService;

    #[tokio::test]
    async fn injection_node_rejects_close_reopen_during_language_load() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        server
            .language
            .language_registry_for_parallel()
            .register("go".to_string(), tree_sitter_go::LANGUAGE.into());
        let uri = Url::parse("file:///workspace/reopened.rs").unwrap();
        let old_incarnation = server.documents.insert(
            uri.clone(),
            "fn old() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        assert!(
            server
                .parse_coordinator()
                .parse_document(uri.clone(), Some("rust"), None)
                .await
        );
        let params = NodeParams {
            text_document: TextDocumentIdentifier {
                uri: uri.as_str().parse().unwrap(),
            },
            position: Position::new(0, 0),
            injection: Some(Value::Bool(true)),
        };

        let result = server
            .kakehashi_node_after_injection_load(params, async {
                server.documents.remove(&uri);
                let new_incarnation = server.documents.insert(
                    uri.clone(),
                    "package main".to_string(),
                    Some("go".to_string()),
                    None,
                );
                assert_ne!(new_incarnation, old_incarnation);
                assert!(
                    server
                        .parse_coordinator()
                        .parse_document(uri.clone(), Some("go"), None)
                        .await
                );
            })
            .await
            .unwrap();

        assert_eq!(result, Value::Null);
    }
}

/// Find the smallest node containing `byte` under the half-open `[start, end)` rule,
/// with the node-reference-protocol end-of-document exception.
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

    // End-of-document exception (node-reference-protocol §"End-of-Document Exception"):
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
