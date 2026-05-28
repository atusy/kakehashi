//! `kakehashi/node/text` — id → current node text (ADR-0025).
//!
//! Resolves a previously-issued ULID back to its tracked byte range via
//! [`NodeTracker::lookup_position`] and slices the current document text at
//! that range. Because [ADR-0019](../../../../../docs/adr/0019-lazy-node-identity-tracking.md)
//! adjusts the tracker synchronously inside `didChange`, the returned slice is
//! always consistent with the document state the client most recently saw.
//!
//! Returns `null` for any unresolvable reference — never-issued, invalidated,
//! mismatched URI, or stale byte range falling outside the current text. The
//! three cases are deliberately indistinguishable (ADR-0025 §"Invalidate vs
//! Not-Found").

use serde::Deserialize;
use serde_json::{Value, json};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::TextDocumentIdentifier;
use ulid::Ulid;

use crate::lsp::lsp_impl::{Kakehashi, uri_to_url};

/// Request parameters for `kakehashi/node/text`.
///
/// `pub` because the handler is registered as a custom LSP method in the
/// `kakehashi` binary (see `src/bin/main.rs`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeTextParams {
    pub text_document: TextDocumentIdentifier,
    pub id: String,
}

impl Kakehashi {
    /// Handler for `kakehashi/node/text`.
    pub async fn kakehashi_node_text(&self, params: NodeTextParams) -> Result<Value> {
        let lsp_uri = params.text_document.uri;
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(target: "kakehashi::node::text", "invalid URI: {}", lsp_uri.as_str());
            return Ok(Value::Null);
        };

        // Malformed ULID: ADR-0025 says any unresolvable reference collapses to null,
        // so we treat a parse failure the same as a never-issued ULID rather than
        // raising an LSP error.
        let Ok(ulid) = params.id.parse::<Ulid>() else {
            return Ok(Value::Null);
        };

        // Look up the tracked range. None means: never issued, invalidated by a
        // prior edit, or this URI's tracker entries have been cleared.
        let Some((start, end, _kind)) = self.bridge.node_tracker().lookup_position(&uri, &ulid)
        else {
            return Ok(Value::Null);
        };

        // Slice the current document text. The tracker keeps positions in sync
        // with didChange (ADR-0019 adjust_for_edits), so start/end should land
        // on UTF-8 boundaries; guard against unexpected out-of-range or mid-
        // char values defensively. Scope the DashMap read guard to the slice
        // extraction only so concurrent writers (didChange) are not blocked by
        // the JSON serialization afterwards.
        let slice = {
            let Some(doc) = self.documents.get(&uri) else {
                return Ok(Value::Null);
            };
            let text = doc.text();
            if end > text.len() || start > end {
                log::warn!(
                    target: "kakehashi::node::text",
                    "tracked range [{}, {}) exceeds document length {} for {}",
                    start, end, text.len(), uri
                );
                return Ok(Value::Null);
            }
            let Some(slice) = text.get(start..end) else {
                log::warn!(
                    target: "kakehashi::node::text",
                    "tracked range [{}, {}) is not on UTF-8 boundaries for {}",
                    start, end, uri
                );
                return Ok(Value::Null);
            };
            slice.to_string()
        };

        Ok(json!({ "text": slice }))
    }
}
