//! `workspace/configuration` server-request handler.
//!
//! Inbound (downstream → bridge). The bridge advertises the
//! `workspace.configuration` capability, so a downstream server pulls its
//! workspace settings; the bridge answers from this connection's current
//! settings cell (the resolved `BridgeServerConfig.settings`), which the pool
//! seeds at spawn and re-stores on a merge change (downstream-settings-propagation).
//!
//! The editor cannot answer for a downstream server's config section (it
//! configures kakehashi, not `rust-analyzer`), so the bridge owns and serves
//! the configuration itself.
//!
//! Limitations (downstream-settings-propagation, deferred scope):
//! `ConfigurationItem.scopeUri` is ignored — the one per-server settings value
//! answers every scope. `section` is treated as a dotted path into the settings
//! root (the editor convention), not a JSON Pointer; a segment is a literal
//! object key, so a section that indexes into an array or a scalar, or that has
//! an empty segment (`"a."`, `".a"`, `"a..b"`), resolves to `null`.

use log::debug;
use serde_json::Value;
use tower_lsp_server::jsonrpc;

use crate::lsp::bridge::actor::ServerRequestDeps;

/// Handle a `workspace/configuration` request, returning the JSON-RPC body the
/// dispatcher wraps in a response.
///
/// The response is an `LSPAny[]` with one element per requested item, in the
/// same order, resolved against this server's settings root (see
/// [`resolve_section`]). A missing item resolves to JSON `null` (never an
/// omitted element), keeping the array aligned with the request's `items`.
pub(in crate::lsp::bridge) fn handle(
    message: &Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) -> jsonrpc::Result<Value> {
    let items = message
        .get("params")
        .and_then(|p| p.get("items"))
        .and_then(|i| i.as_array());

    // Log the incoming items (section + scopeUri) so the resolution convention
    // can be validated against a real downstream server, which is the only way
    // to confirm what `section` it actually sends (downstream-settings-propagation).
    // `{:?}` borrows `items` — no clone or intermediate string on this path.
    debug!(
        target: "kakehashi::bridge::reader",
        "{}Answering workspace/configuration for items: {:?}",
        server_prefix,
        items
    );

    let guard = deps.settings.load();
    let root = guard.as_ref().map(|arc| arc.as_ref());

    Ok(Value::Array(build_response(root, items.map(Vec::as_slice))))
}

/// Build the `LSPAny[]` response: one element per requested item, in request
/// order, each resolved against `root`. A missing `items` (malformed request,
/// `items` is spec-required) yields an empty array rather than an error,
/// mirroring the bridge's lenient request handling.
fn build_response(root: Option<&Value>, items: Option<&[Value]>) -> Vec<Value> {
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| {
            let section = item.get("section").and_then(Value::as_str);
            resolve_section(root, section)
        })
        .collect()
}

/// Resolve one `ConfigurationItem.section` against this server's settings root.
///
/// The settings root is treated as the editor-style configuration object, so a
/// `section` is a dotted path indexed into it — the standard editor convention,
/// which works whether a server sends its own namespace (`"rust-analyzer"`) or
/// `null` for the whole object:
/// - `None` / empty string → the whole root,
/// - `"a.b"` → `root["a"]["b"]`,
/// - any missing segment or absent root → JSON `null`.
fn resolve_section(root: Option<&Value>, section: Option<&str>) -> Value {
    let Some(root) = root else {
        return Value::Null;
    };
    match section {
        None | Some("") => root.clone(),
        Some(path) => {
            let mut current = root;
            for segment in path.split('.') {
                // An empty segment (leading/trailing/double dot) is a malformed
                // path, not a lookup of a literal `""` key — reject it explicitly
                // so the result never depends on whether settings happens to
                // contain an empty-string key.
                if segment.is_empty() {
                    return Value::Null;
                }
                match current.get(segment) {
                    Some(next) => current = next,
                    None => return Value::Null,
                }
            }
            current.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> Value {
        json!({
            "rust-analyzer": {
                "cargo": { "features": "all" },
                "check": { "command": "clippy" }
            },
            "top": 1
        })
    }

    #[test]
    fn none_section_returns_whole_root() {
        let r = root();
        assert_eq!(resolve_section(Some(&r), None), r);
    }

    #[test]
    fn empty_section_returns_whole_root() {
        let r = root();
        assert_eq!(resolve_section(Some(&r), Some("")), r);
    }

    #[test]
    fn dotted_section_indexes_into_root() {
        let r = root();
        assert_eq!(
            resolve_section(Some(&r), Some("rust-analyzer.cargo")),
            json!({ "features": "all" })
        );
        assert_eq!(
            resolve_section(Some(&r), Some("rust-analyzer.check.command")),
            json!("clippy")
        );
        assert_eq!(resolve_section(Some(&r), Some("top")), json!(1));
    }

    #[test]
    fn missing_segment_resolves_to_null() {
        let r = root();
        assert_eq!(resolve_section(Some(&r), Some("nope")), Value::Null);
        assert_eq!(
            resolve_section(Some(&r), Some("rust-analyzer.nope")),
            Value::Null
        );
        // Indexing into a scalar fails to null rather than panicking.
        assert_eq!(resolve_section(Some(&r), Some("top.deeper")), Value::Null);
    }

    #[test]
    fn absent_root_resolves_every_section_to_null() {
        assert_eq!(resolve_section(None, None), Value::Null);
        assert_eq!(resolve_section(None, Some("rust-analyzer")), Value::Null);
    }

    #[test]
    fn empty_segments_resolve_to_null() {
        // A literal-key dotted path: an empty segment indexes a `""` key, which
        // never exists, so trailing/leading/double dots resolve to null
        // (panic-free).
        let r = root();
        for section in [
            "rust-analyzer.",
            ".rust-analyzer",
            "rust-analyzer..cargo",
            ".",
        ] {
            assert_eq!(
                resolve_section(Some(&r), Some(section)),
                Value::Null,
                "section {section:?}"
            );
        }
    }

    #[test]
    fn empty_segment_does_not_reach_a_literal_empty_string_key() {
        // Even when settings contains an empty-string key, an empty path segment
        // resolves to null rather than indexing it (the path is malformed).
        let r = json!({ "server": { "": "trap" }, "": "trap2" });
        assert_eq!(resolve_section(Some(&r), Some("server.")), Value::Null);
        assert_eq!(resolve_section(Some(&r), Some(".server")), Value::Null);
        // …but a non-empty key is still reachable.
        assert_eq!(
            resolve_section(Some(&r), Some("server")),
            json!({ "": "trap" })
        );
    }

    #[test]
    fn explicit_null_leaf_is_returned_as_null() {
        // A section whose value is genuinely JSON `null` returns that null — the
        // same wire value as a miss, which is correct for `LSPAny`.
        let r = json!({ "server": { "feature": Value::Null } });
        assert_eq!(
            resolve_section(Some(&r), Some("server.feature")),
            Value::Null
        );
    }

    #[test]
    fn indexing_into_an_array_resolves_to_null() {
        // A string segment can only index an object; descending into an array
        // value yields null rather than panicking or coercing.
        let r = json!({ "server": { "list": [1, 2, 3] } });
        assert_eq!(
            resolve_section(Some(&r), Some("server.list.0")),
            Value::Null
        );
        // …but addressing the array itself returns it verbatim.
        assert_eq!(
            resolve_section(Some(&r), Some("server.list")),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn response_preserves_item_order_and_aligns_null_for_misses() {
        let r = root();
        let items = vec![
            json!({ "section": "rust-analyzer.cargo" }),
            json!({ "section": "missing" }),
            json!({}), // no section → whole root
        ];
        let resp = build_response(Some(&r), Some(&items));
        assert_eq!(resp.len(), items.len(), "one element per item, same length");
        assert_eq!(resp[0], json!({ "features": "all" }));
        assert_eq!(resp[1], Value::Null, "miss is JSON null, not omitted");
        assert_eq!(resp[2], r, "no section → whole root");
    }

    #[test]
    fn response_is_empty_array_when_items_missing() {
        assert!(build_response(Some(&root()), None).is_empty());
    }

    #[test]
    fn response_is_all_null_when_no_settings_configured() {
        let items = vec![json!({ "section": "rust-analyzer" }), json!({})];
        let resp = build_response(None, Some(&items));
        assert_eq!(resp, vec![Value::Null, Value::Null]);
    }
}
