//! Code action method for Kakehashi.
//!
//! First increment (#352): bridges `textDocument/codeAction` to the single
//! injection region under `range.start` (single-region, like `inlay_hint`'s
//! virt layer). The host range is translated to virtual for the request; edit
//! and diagnostic ranges in the response are translated back to host.
//!
//! Deferred follow-ups (intentionally out of scope here):
//! - Request `context.diagnostics` is sent empty. Translating the editor's
//!   host-coordinate diagnostics back to virtual and filtering them to the
//!   region is a follow-up.
//! - `codeAction/resolve` is not implemented. If a downstream server returns a
//!   `data` field, the editor's subsequent `codeAction/resolve` request would be
//!   unhandled (method not found). Wiring resolve through the origin server is a
//!   follow-up.
//! - `WorkspaceEdit.documentChanges` translation is a follow-up; an action that
//!   carries `documentChanges` has its whole `edit` dropped (see bridge module).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CodeActionParams, CodeActionResponse, Range, Uri};

use super::super::Kakehashi;

const METHOD: &str = "textDocument/codeAction";

impl Kakehashi {
    pub(crate) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        // Pass through the requested filter and trigger reason; diagnostics are
        // intentionally empty in this increment (see module docs).
        let only = params.context.only;
        let trigger_kind = params.context.trigger_kind;

        self.code_action_virt_layer(&lsp_uri, range, only, trigger_kind)
            .await
    }

    /// Virt layer: bridge the injection region under the requested range.
    async fn code_action_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        only: Option<Vec<tower_lsp_server::ls_types::CodeActionKind>>,
        trigger_kind: Option<tower_lsp_server::ls_types::CodeActionTriggerKind>,
    ) -> Result<Option<CodeActionResponse>> {
        let Some(ctx) = self.resolve_bridge_contexts_for_range(lsp_uri, range, METHOD) else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let result = crate::lsp::aggregation::server::dispatch_preferred(
            &ctx.document,
            pool.clone(),
            // `only` is a non-Copy Vec and the closure is invoked once per
            // fan-out target, so clone it per call.
            move |t| {
                let only = only.clone();
                async move {
                    t.pool
                        .send_code_action_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            range,
                            only,
                            trigger_kind,
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                        )
                        .await
                }
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result.handle(&self.client, "code action", None, Ok).await
    }
}
