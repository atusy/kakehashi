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

use std::sync::Arc;
use std::task::{Context, Poll};

use dashmap::DashSet;
use tower::Service;
use tower_lsp_server::jsonrpc::Request;

/// The document-scoped custom methods that existed when the `textDocument/`
/// scope segment was introduced, in their canonical spelling.
///
/// **Frozen list.** It exists to translate names that clients already shipped
/// against, so it must not grow as new methods are added — a method introduced
/// after the rename never had an old spelling to alias, and adding it here
/// would invent a deprecated name nobody ever called. It shrinks only when a
/// deprecation window closes.
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
pub(crate) fn canonical_method(method: &str) -> Option<&'static str> {
    let feature = method.strip_prefix(VENDOR_PREFIX)?;
    // Already canonical. Checked before the lookup so the mapping is
    // idempotent: without this, `kakehashi/textDocument/node` would fail the
    // lookup anyway, but a future `kakehashi/textDocument/…` method that
    // happens to share a suffix would not.
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
/// **Must be the OUTERMOST layer** — above [`IngressOrderGate`]. That gate
/// classifies requests by method name to assign per-document wire-order
/// tickets, and knows only the canonical spellings. Placed below the gate,
/// this layer would let an old name arrive unrecognized, pass through ungated,
/// and read a tree that is missing edits which preceded it on the wire.
///
/// [`IngressOrderGate`]: crate::lsp::IngressOrderGate
pub struct DeprecatedMethodAlias<S> {
    inner: S,
    /// Old names already warned about. Shared behind an `Arc` because tower
    /// clones services, and the warning is once per *session*, not per clone.
    warned: Arc<DashSet<&'static str>>,
}

impl<S> DeprecatedMethodAlias<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            warned: Arc::new(DashSet::new()),
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

impl<S> DeprecatedMethodAlias<S> {
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
            log::warn!(
                target: "kakehashi::deprecated",
                "custom method `{}` is deprecated; use `{}`. The old name still \
                 works for now but may be removed in a future release.",
                req.method(),
                canonical
            );
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
        DeprecatedMethodAlias::new(())
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
