//! willSaveWaitUntil request handler for Kakehashi (#357).

use std::time::Duration;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{TextEdit, WillSaveTextDocumentParams};

use super::super::Kakehashi;

/// Upper bound on a willSaveWaitUntil round-trip to host-bridge servers before
/// kakehashi gives up and lets the editor save with no save-time edits.
///
/// The LSP spec warns clients drop slow willSaveWaitUntil results to keep saves
/// fast and reliable (issue #357 Q3). The underlying per-request wait is bounded
/// at 30s — far longer than a save should ever block — so this caps it to the
/// same 5s budget the concatenated formatting pipeline uses for explicit user
/// actions, rather than introducing a new knob.
const WILL_SAVE_WAIT_UNTIL_BUDGET: Duration = Duration::from_secs(5);

impl Kakehashi {
    /// Handle `textDocument/willSaveWaitUntil`.
    ///
    /// Forwards the request verbatim (real URI, real `reason`) to host-bridge
    /// servers and returns their save-time `TextEdit[]` (host-document-bridge,
    /// #357). Host-only: the host document is the one being saved, so the edits
    /// need no virtual→host translation, and per-virtual-doc fan-out (which
    /// overlaps with format-on-save semantics the concatenated pipeline owns) is
    /// deferred to a follow-up should a concrete need appear.
    ///
    /// Aggregation reuses the `preferred` host layer (first non-empty wins,
    /// falling through to lower-priority host servers). This is deliberately
    /// **not** the formatting path: formatting treats a capable server's `null`
    /// as the authoritative "already formatted" and stops fall-through, but a
    /// save hook returning no edits should let the next host server contribute —
    /// exactly the semantics `host_layer_value_with_ctx` gives via
    /// `is_empty_layer_value`.
    pub(crate) async fn will_save_wait_until_impl(
        &self,
        params: WillSaveTextDocumentParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let lsp_uri = &params.text_document.uri;
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, "textDocument/willSaveWaitUntil")
        else {
            return Ok(None);
        };

        let raw_params = match serde_json::to_value(&params) {
            Ok(value) => value,
            Err(e) => {
                log::warn!("willSaveWaitUntil: failed to serialize params: {e}");
                return Ok(None);
            }
        };

        let fut =
            self.host_layer_value_with_ctx(&ctx, "textDocument/willSaveWaitUntil", raw_params);
        let value = match tokio::time::timeout(WILL_SAVE_WAIT_UNTIL_BUDGET, fut).await {
            Ok(result) => result?,
            Err(_) => {
                // Bounded save latency (#357 Q3): abandon the in-flight host
                // request and let the editor save without save-time edits.
                // The per-server requests run as spawned tasks; dropping `fut`
                // aborts them, and an aborted task may not reach its own
                // upstream-registry unregister, so sweep the id here to avoid
                // leaking the cancel mapping. (The abort does not send a
                // downstream $/cancelRequest — the host server keeps computing,
                // which the 5s budget accepts in exchange for a fast save.)
                log::warn!(
                    "willSaveWaitUntil timed out after {WILL_SAVE_WAIT_UNTIL_BUDGET:?}; \
                     saving without host save-time edits"
                );
                self.bridge
                    .pool_arc()
                    .unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
                return Ok(None);
            }
        };

        Ok(parse_will_save_wait_until_edits(value))
    }
}

/// Convert the raw host-layer result into the editor-facing `TextEdit[]`.
///
/// `None` (no host response, an empty list, or a malformed payload) means "let
/// the save proceed with no save-time edits". The dispatch already filters
/// empty/`null` results, but the empty guard is kept so a stray `[]` can never
/// reach the editor as "apply these (zero) edits".
fn parse_will_save_wait_until_edits(value: Option<serde_json::Value>) -> Option<Vec<TextEdit>> {
    let value = value?;
    match serde_json::from_value::<Vec<TextEdit>>(value) {
        Ok(edits) if edits.is_empty() => None,
        Ok(edits) => Some(edits),
        Err(e) => {
            log::warn!("willSaveWaitUntil: malformed TextEdit[] from host server: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_none_is_no_edits() {
        assert!(parse_will_save_wait_until_edits(None).is_none());
    }

    #[test]
    fn parse_empty_list_is_no_edits() {
        assert!(parse_will_save_wait_until_edits(Some(json!([]))).is_none());
    }

    #[test]
    fn parse_malformed_payload_is_no_edits() {
        // A host server answering with the wrong shape must not abort the save.
        assert!(parse_will_save_wait_until_edits(Some(json!({"unexpected": true}))).is_none());
    }

    #[test]
    fn parse_edits_pass_through_verbatim() {
        let value = json!([{
            "range": { "start": { "line": 2, "character": 0 },
                       "end": { "line": 2, "character": 4 } },
            "newText": "fixed"
        }]);
        let edits = parse_will_save_wait_until_edits(Some(value)).expect("edits expected");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "fixed");
        // Host coordinates are forwarded untranslated.
        assert_eq!(edits[0].range.start.line, 2);
    }
}
