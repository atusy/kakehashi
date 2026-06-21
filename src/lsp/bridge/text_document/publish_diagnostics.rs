//! Inbound `textDocument/publishDiagnostics` handling.
//!
//! Unlike the other `text_document/*` files (outbound request senders), this one
//! is **inbound** (downstream → bridge): a downstream server pushes diagnostics.
//! The bridge does not forward push diagnostics in general (#380); the one case
//! it must act on is a push targeting a concatenated-formatting *scratch*
//! virtual document, which is discarded structurally by the dispatcher in
//! [`actor::reader`](crate::lsp::bridge::actor) using [`is_scratch_publish_diagnostics`].

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
