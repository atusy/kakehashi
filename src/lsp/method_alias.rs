//! Backward-compatible aliases for the pre-scope-first custom method names.
//!
//! The document-scoped custom methods moved under a `textDocument/` scope
//! segment (`kakehashi/node` → `kakehashi/textDocument/node`) so the vendor
//! namespace mirrors LSP's own scope-first axis (`textDocument/diagnostic` vs
//! `workspace/diagnostic`). The old spellings stay callable for a deprecation
//! window: [`canonical_method`] maps an old name to its canonical replacement,
//! and the alias middleware rewrites the request before it reaches any other
//! layer.
//!
//! The mapping is an explicit allowlist rather than a `kakehashi/` prefix
//! rewrite because not every custom method is document-scoped —
//! `kakehashi/internal/effectiveConfiguration` takes empty params and keeps
//! its name.

use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use dashmap::DashSet;
use tower::Service;
use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::Request;
use tower_lsp_server::ls_types::MessageType;

/// The document-scoped custom methods that existed when the `textDocument/`
/// scope segment was introduced, in their canonical spelling.
///
/// **Frozen list.** It exists to translate names that clients already shipped
/// against, so it must not grow as new methods are added — a method introduced
/// after the rename never had an old spelling to alias, and adding it here
/// would invent a deprecated name nobody ever called. It shrinks only when a
/// deprecation window closes.
///
/// It therefore duplicates part of the registration chain in `src/bin/main.rs`
/// on purpose; see custom-method-namespace for why deriving one from the other
/// would be wrong. Every entry must nonetheless stay registered there —
/// `tests/e2e_kakehashi_node.rs` walks all of them asserting none has become
/// `MethodNotFound`.
const ALIASED_METHODS: &[&str] = &[
    "kakehashi/textDocument/captures/full",
    "kakehashi/textDocument/captures/full/delta",
    "kakehashi/textDocument/captures/range",
    "kakehashi/textDocument/node",
    "kakehashi/textDocument/node/text",
    "kakehashi/textDocument/node/parent",
    "kakehashi/textDocument/node/children",
    "kakehashi/textDocument/node/kind",
    "kakehashi/textDocument/node/grammarName",
    "kakehashi/textDocument/node/isNamed",
    "kakehashi/textDocument/node/isExtra",
    "kakehashi/textDocument/node/hasError",
    "kakehashi/textDocument/node/isError",
    "kakehashi/textDocument/node/isMissing",
    "kakehashi/textDocument/node/startByte",
    "kakehashi/textDocument/node/endByte",
    "kakehashi/textDocument/node/byteRange",
    "kakehashi/textDocument/node/childCount",
    "kakehashi/textDocument/node/namedChildCount",
    "kakehashi/textDocument/node/descendantCount",
    "kakehashi/textDocument/node/toSexp",
    "kakehashi/textDocument/node/child",
    "kakehashi/textDocument/node/namedChild",
    "kakehashi/textDocument/node/namedChildren",
    "kakehashi/textDocument/node/childWithDescendant",
    "kakehashi/textDocument/node/nextSibling",
    "kakehashi/textDocument/node/prevSibling",
    "kakehashi/textDocument/node/nextNamedSibling",
    "kakehashi/textDocument/node/prevNamedSibling",
    "kakehashi/textDocument/node/firstChildForByte",
    "kakehashi/textDocument/node/descendantForByteRange",
    "kakehashi/textDocument/node/namedDescendantForByteRange",
    "kakehashi/textDocument/node/range",
    "kakehashi/textDocument/node/startPosition",
    "kakehashi/textDocument/node/endPosition",
    "kakehashi/textDocument/node/descendantForPointRange",
    "kakehashi/textDocument/node/namedDescendantForPointRange",
    "kakehashi/textDocument/node/childByFieldName",
    "kakehashi/textDocument/node/childrenByFieldName",
    "kakehashi/textDocument/node/fieldNameForChild",
    "kakehashi/textDocument/node/fieldNameForNamedChild",
];

/// The segment the rename inserted between the vendor namespace and the
/// feature name.
const CANONICAL_PREFIX: &str = "kakehashi/textDocument/";

/// The vendor namespace every custom method shares.
const VENDOR_PREFIX: &str = "kakehashi/";

/// The canonical spelling of a deprecated custom method name, or `None` when
/// the method needs no rewrite.
///
/// `None` covers three distinct cases the caller treats identically: a name
/// that is already canonical (so the mapping is idempotent — a second pass can
/// never double-prefix), a `kakehashi/` method that is deliberately not
/// document-scoped, and every standard LSP method.
///
/// Returns `&'static str` rather than `String` so the result doubles as the
/// dedup key for the once-per-method deprecation warning; the wire method name
/// is owned and would otherwise have to be cloned into the seen-set.
fn canonical_method(method: &str) -> Option<&'static str> {
    let feature = method.strip_prefix(VENDOR_PREFIX)?;
    // Short-circuit the canonical spellings. This is a HOT-PATH guard, not a
    // correctness one: the lookup below would also miss, because stripping
    // `CANONICAL_PREFIX` from an entry can never leave a suffix that starts
    // with `textDocument/`. Without it, every canonical call — and clients
    // walking a tree fire these in bursts — would scan all 41 entries to
    // conclude nothing matches.
    if method.starts_with(CANONICAL_PREFIX) {
        return None;
    }
    ALIASED_METHODS
        .iter()
        .copied()
        .find(|canonical| canonical.strip_prefix(CANONICAL_PREFIX) == Some(feature))
}

/// Tower middleware rewriting deprecated custom method names to their
/// canonical spelling, warning once per distinct old name.
///
/// **Must wrap the ordering gate**, never the reverse — see [`ingress_stack`],
/// which is the only place the two are composed and where the reasoning lives.
///
/// [`ingress_stack`]: crate::lsp::ingress_stack
pub(crate) struct DeprecatedMethodAlias<S> {
    inner: S,
    /// Old names already warned about, so a client that never migrates gets one
    /// line per name rather than one per keystroke.
    ///
    /// Bounded at 41 entries — the keys are `&'static str` drawn from the frozen
    /// table, not from the wire. Interior mutability rather than `&mut self`
    /// because `Service::call` hands out `&mut self` but `rewrite` is shared
    /// through it.
    warned: DashSet<&'static str>,
    /// The handle used to put the deprecation notice in the editor's LSP log.
    ///
    /// A `OnceLock` because the `Client` does not exist until `LspService::build`
    /// runs its factory closure, which is also where the service this layer
    /// wraps is created. Empty in unit tests, where the notice is `log::warn!`
    /// only and nothing is spawned.
    client: Arc<OnceLock<Client>>,
}

impl<S> DeprecatedMethodAlias<S> {
    pub(crate) fn new(inner: S, client: Arc<OnceLock<Client>>) -> Self {
        Self {
            inner,
            warned: DashSet::new(),
            client,
        }
    }

    /// Rewrite a deprecated method name in place, leaving `id` and `params`
    /// untouched. Non-deprecated requests are returned unchanged.
    fn rewrite(&self, req: Request) -> Request {
        let Some(canonical) = canonical_method(req.method()) else {
            return req;
        };
        // `insert` returns false when the key was already present, so the
        // warning fires once per distinct old name rather than once per call —
        // a client on an old name would otherwise flood the log on every
        // keystroke.
        if self.warned.insert(canonical) {
            self.warn_deprecated(req.method(), canonical);
        }
        let (_, id, params) = req.into_parts();
        let mut builder = Request::build(canonical);
        if let Some(params) = params {
            builder = builder.params(params);
        }
        if let Some(id) = id {
            builder = builder.id(id);
        }
        builder.finish()
    }

    /// Announce a deprecated spelling once, to both the server log and the
    /// editor's LSP log.
    ///
    /// `window/logMessage` is what actually reaches the audience. The server's
    /// own `log::warn!` goes to stderr and, with `RUST_LOG` unset, env_logger
    /// filters at `Error` — so a log-only notice is invisible in a default run,
    /// to precisely the client authors who need to see it. `showMessage` is
    /// wrong in the other direction: up to 41 popups in a session.
    ///
    /// Detached rather than awaited. `call` must reach the ordering gate
    /// synchronously, so the request path stays straight-line and only this
    /// once-per-name notification is spawned; it carries no ordering
    /// obligation of its own.
    fn warn_deprecated(&self, deprecated: &str, canonical: &'static str) {
        let notice = format!(
            "kakehashi: custom method `{deprecated}` is deprecated; use \
             `{canonical}`. The old name still works for now but may be removed \
             in a future release."
        );
        log::warn!(target: "kakehashi::deprecated", "{notice}");
        if let Some(client) = self.client.get() {
            let client = client.clone();
            tokio::spawn(async move {
                client.log_message(MessageType::WARNING, notice).await;
            });
        }
    }
}

impl<S> Service<Request> for DeprecatedMethodAlias<S>
where
    S: Service<Request>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // Stays synchronous through to the delegate: every layer below relies
        // on `call` being invoked in wire order, so this must not await, spawn,
        // or reorder.
        self.inner.call(self.rewrite(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_the_node_entry_point() {
        assert_eq!(
            canonical_method("kakehashi/node"),
            Some("kakehashi/textDocument/node")
        );
    }

    #[test]
    fn rewrites_every_node_accessor() {
        assert_eq!(
            canonical_method("kakehashi/node/parent"),
            Some("kakehashi/textDocument/node/parent")
        );
        assert_eq!(
            canonical_method("kakehashi/node/namedDescendantForByteRange"),
            Some("kakehashi/textDocument/node/namedDescendantForByteRange")
        );
        assert_eq!(
            canonical_method("kakehashi/node/childrenByFieldName"),
            Some("kakehashi/textDocument/node/childrenByFieldName")
        );
    }

    #[test]
    fn rewrites_the_captures_triple() {
        assert_eq!(
            canonical_method("kakehashi/captures/full"),
            Some("kakehashi/textDocument/captures/full")
        );
        assert_eq!(
            canonical_method("kakehashi/captures/full/delta"),
            Some("kakehashi/textDocument/captures/full/delta")
        );
        assert_eq!(
            canonical_method("kakehashi/captures/range"),
            Some("kakehashi/textDocument/captures/range")
        );
    }

    #[test]
    fn every_aliased_method_round_trips_and_is_idempotent() {
        for canonical in ALIASED_METHODS {
            let feature = canonical
                .strip_prefix(CANONICAL_PREFIX)
                .expect("every entry is spelled canonically");
            let old = format!("{VENDOR_PREFIX}{feature}");
            assert_eq!(canonical_method(&old), Some(*canonical), "{old}");
            assert_eq!(
                canonical_method(canonical),
                None,
                "{canonical} must not rewrite again"
            );
        }
    }

    #[test]
    fn leaves_the_non_document_scoped_method_alone() {
        // `effectiveConfiguration` takes empty params — it is server-scoped, so
        // a blanket `kakehashi/` prefix rewrite would corrupt it.
        assert_eq!(
            canonical_method("kakehashi/internal/effectiveConfiguration"),
            None
        );
    }

    #[test]
    fn is_idempotent_on_already_canonical_names() {
        // The middleware must be safe to apply twice: returning a rewrite here
        // would produce `kakehashi/textDocument/textDocument/node`.
        assert_eq!(canonical_method("kakehashi/textDocument/node"), None);
        assert_eq!(canonical_method("kakehashi/textDocument/node/parent"), None);
        assert_eq!(
            canonical_method("kakehashi/textDocument/captures/full"),
            None
        );
    }

    /// The rewrite is independent of the inner service, so tests drive it
    /// through a middleware wrapping the unit type rather than a stub service.
    fn alias() -> DeprecatedMethodAlias<()> {
        // No client: the notice is `log::warn!` only, so `rewrite` spawns
        // nothing and these stay plain `#[test]`s with no runtime.
        DeprecatedMethodAlias::new((), Arc::new(OnceLock::new()))
    }

    #[test]
    fn the_warning_fires_once_per_name_not_once_per_call() {
        // The regression this guards is silent and unbounded: inverting the
        // `insert` condition warns on every call, so a client that never
        // migrates floods the log at keystroke rate. Nothing else would fail.
        let alias = alias();
        let deprecated = || Request::build("kakehashi/node/parent").id(1).finish();

        alias.rewrite(deprecated());
        alias.rewrite(deprecated());
        alias.rewrite(deprecated());
        assert_eq!(alias.warned.len(), 1, "one entry for three calls");

        alias.rewrite(Request::build("kakehashi/captures/full").id(2).finish());
        assert_eq!(alias.warned.len(), 2, "a distinct name warns separately");

        // A canonical name is not a deprecation and must not be recorded.
        alias.rewrite(
            Request::build("kakehashi/textDocument/node/parent")
                .id(3)
                .finish(),
        );
        assert_eq!(
            alias.warned.len(),
            2,
            "canonical names are not warned about"
        );
    }

    #[test]
    fn rewrite_preserves_the_request_id_and_params() {
        let params = serde_json::json!({
            "textDocument": { "uri": "file:///a.md" },
            "id": "01HX",
        });
        let req = Request::build("kakehashi/node/parent")
            .params(params.clone())
            .id(7)
            .finish();

        let rewritten = alias().rewrite(req);

        assert_eq!(rewritten.method(), "kakehashi/textDocument/node/parent");
        assert_eq!(rewritten.params(), Some(&params));
        assert_eq!(rewritten.id(), Some(&7.into()));
    }

    #[test]
    fn rewrite_keeps_a_notification_a_notification() {
        // Re-attaching an id would turn a notification into a request the
        // client never sent, and the server would answer into the void.
        let req = Request::build("kakehashi/captures/full")
            .params(serde_json::json!({ "textDocument": { "uri": "file:///a.md" } }))
            .finish();

        let rewritten = alias().rewrite(req);

        assert_eq!(rewritten.method(), "kakehashi/textDocument/captures/full");
        assert_eq!(rewritten.id(), None, "must not gain an id");
    }

    #[test]
    fn rewrite_passes_untouched_methods_through_unchanged() {
        let req = Request::build("textDocument/didChange")
            .params(serde_json::json!({ "textDocument": { "uri": "file:///a.md" } }))
            .finish();
        let expected = req.clone();

        assert_eq!(alias().rewrite(req), expected);
    }

    #[test]
    fn rewrite_tolerates_a_deprecated_request_without_params() {
        // A malformed call must still reach the handler that rejects it, not
        // gain an empty `params` object that changes the deserialization error.
        let req = Request::build("kakehashi/node").id(1).finish();

        let rewritten = alias().rewrite(req);

        assert_eq!(rewritten.method(), "kakehashi/textDocument/node");
        assert_eq!(rewritten.params(), None);
    }

    #[test]
    fn leaves_standard_lsp_methods_alone() {
        assert_eq!(canonical_method("textDocument/semanticTokens/full"), None);
        assert_eq!(canonical_method("textDocument/didChange"), None);
        assert_eq!(canonical_method("initialize"), None);
    }

    #[test]
    fn rejects_unknown_kakehashi_methods() {
        // An allowlist, not a prefix rule: a method that never existed must not
        // be silently rewritten into a name that does not exist either.
        assert_eq!(canonical_method("kakehashi/node/notAMethod"), None);
        assert_eq!(canonical_method("kakehashi/captures/kinds"), None);
        assert_eq!(canonical_method("kakehashi/somethingElse"), None);
    }
}
