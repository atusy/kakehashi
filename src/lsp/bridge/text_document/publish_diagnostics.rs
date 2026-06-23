//! Inbound `textDocument/publishDiagnostics` handling.
//!
//! Unlike the other `text_document/*` files (outbound request senders), this one
//! is **inbound** (downstream → bridge): a downstream server pushes diagnostics.
//! A non-scratch push for an injection region's virtual document is **routed**
//! into the proactive diagnostics cache ([`forward_push`],
//! push-propagation-diagnostic-forwarding, closing #380); a push targeting a
//! concatenated-formatting *scratch* virtual document is discarded structurally
//! by the dispatcher in [`actor::reader`](crate::lsp::bridge::actor) using
//! [`is_scratch_publish_diagnostics`].

/// Whether `message` is a `textDocument/publishDiagnostics` notification
/// targeting a concatenated-formatting *scratch* virtual document
/// ([`VirtualDocumentUri::is_scratch_uri`]).
///
/// Scratch documents carry speculative pipeline text the editor has never
/// seen; diagnostics computed against them are meaningless to the user and
/// must be discarded, not forwarded (concatenated-formatting-pipeline
/// Decision point 7). The prompt `didClose` after each pipeline run shrinks
/// but cannot eliminate the window in which a downstream server pushes them.
///
/// [`VirtualDocumentUri::is_scratch_uri`]: crate::lsp::bridge::VirtualDocumentUri::is_scratch_uri
pub(in crate::lsp::bridge) fn is_scratch_publish_diagnostics(message: &serde_json::Value) -> bool {
    // `Value` indexing returns `Null` for missing keys / non-objects, so the
    // lookups below are panic-free on malformed messages.
    message["method"].as_str() == Some("textDocument/publishDiagnostics")
        && message["params"]["uri"]
            .as_str()
            .is_some_and(crate::lsp::bridge::VirtualDocumentUri::is_scratch_uri)
}

/// Route a non-scratch downstream `textDocument/publishDiagnostics` into the
/// proactive diagnostics cache (push-propagation-diagnostic-forwarding). The
/// forwarding loop classifies the URI: a virtual injection URI becomes a region
/// push; a real URI is a candidate `_self` host-layer push, accepted only if it
/// names an open host-bridged document (the publisher filters the server's own
/// workspace pushes). Either way the merged host set is republished.
pub(in crate::lsp::bridge) fn forward_push(
    mut message: serde_json::Value,
    deps: &crate::lsp::bridge::actor::ServerRequestDeps,
) {
    // The server name keys the cache slot; production always sets it (readers are
    // spawned with `Some(server_name)`). Check it by reference up front so a push
    // that lacks one is dropped before any parse/enqueue work; the actual clone is
    // deferred to the send below, past every early-return guard.
    let Some(server_name) = deps.server_name.as_deref() else {
        log::debug!(
            target: "kakehashi::bridge::reader",
            "dropping region push without a server name"
        );
        return;
    };
    // `params` must be a JSON object. Using `get_mut(...).as_object_mut()` rather
    // than `IndexMut` (`message["params"]["uri"]`) avoids a panic on adversarial
    // input where `params` exists but is a string/number/array; `Map::remove`
    // still moves the owned values out (no clone).
    let Some(params) = message
        .get_mut("params")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let Some(serde_json::Value::String(uri)) = params.remove("uri") else {
        return;
    };
    // Deserialize the moved-out diagnostics value (no clone) — `from_value` moves
    // the diagnostics' owned strings. A parse failure (including an absent or
    // non-array `diagnostics`) is *dropped* (not treated as an empty/clearing push,
    // which would silently wipe the region's diagnostics); an empty `[]` array
    // yields an empty Vec and clears legitimately.
    let diagnostics_value = params
        .remove("diagnostics")
        .unwrap_or(serde_json::Value::Null);
    let diagnostics = match serde_json::from_value::<Vec<tower_lsp_server::ls_types::Diagnostic>>(
        diagnostics_value,
    ) {
        Ok(diagnostics) => diagnostics,
        Err(e) => {
            log::debug!(
                target: "kakehashi::bridge::reader",
                "dropping malformed publishDiagnostics for {uri}: {e}"
            );
            return;
        }
    };

    let _ = deps.upstream_tx.send(
        crate::lsp::bridge::actor::UpstreamNotification::PublishDiagnostics {
            uri,
            server: server_name.to_owned(),
            diagnostics,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_scratch_publish_diagnostics_matches_only_scratch_targets() {
        let scratch = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///p/kakehashi-virtual-uri-R-kakehashi-scratch-0-1.py", "diagnostics": []}
        });
        assert!(is_scratch_publish_diagnostics(&scratch));

        // Canonical virtual document: not scratch.
        let canonical = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///p/kakehashi-virtual-uri-R.py", "diagnostics": []}
        });
        assert!(!is_scratch_publish_diagnostics(&canonical));

        // Different method on a scratch URI: not publishDiagnostics.
        let other_method = json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {"uri": "file:///p/kakehashi-virtual-uri-R-kakehashi-scratch-0-1.py"}
        });
        assert!(!is_scratch_publish_diagnostics(&other_method));

        // Missing params: must not panic, just no match.
        let no_params = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics"
        });
        assert!(!is_scratch_publish_diagnostics(&no_params));
    }
}
