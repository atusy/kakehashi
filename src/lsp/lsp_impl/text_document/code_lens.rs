//! Code lens methods for Kakehashi: the whole-document fan-out and the
//! `codeLens/resolve` round-trip (#355).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CodeLens, CodeLensParams};

use super::super::Kakehashi;
use crate::lsp::bridge::{envelope_host_code_lenses, extract_code_lens_envelope};
use crate::lsp::current_upstream_id;

impl Kakehashi {
    pub(crate) async fn code_lens_impl(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token;
        self.whole_document_fan_out(
            &params.text_document.uri,
            "textDocument/codeLens",
            raw_params,
            work_done_token,
            |t| async move {
                t.pool
                    .send_code_lens_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                        t.client_progress_token,
                    )
                    .await
            },
            |mut won| {
                let server_resolves = won.handle.has_capability("codeLens/resolve");
                envelope_host_code_lenses(
                    &mut won.items,
                    &won.server_name,
                    won.host_uri.as_str(),
                    won.incarnation,
                    won.connection_generation,
                    won.handle.key(),
                    server_resolves,
                );
                Some(won.items)
            },
        )
        .await
    }

    /// `codeLens/resolve`: route the lens back to the downstream server that
    /// produced it, identified by the envelope in `lens.data` (#355).
    ///
    /// Fails soft except for client cancellation: a lens without an envelope
    /// (foreign, or from a server without resolve support on either layer)
    /// passes through unchanged, and a stale region returns the lens
    /// unresolved with its envelope intact — clients re-request lenses on
    /// change, so the staleness window is short.
    pub(crate) async fn code_lens_resolve_impl(&self, lens: CodeLens) -> Result<CodeLens> {
        let Some(envelope) = extract_code_lens_envelope(&lens) else {
            return Ok(lens);
        };

        // Fail-soft staleness gate: resolving against a moved or invalidated
        // region would translate coordinates with a stale offset and bind the
        // lens to content the user has since edited.
        if !envelope.is_host_layer()
            && !self
                .region_offset_is_fresh(
                    &envelope.host_uri,
                    &envelope.region_id,
                    &envelope.offset,
                    envelope.incarnation,
                )
                .await
        {
            log::debug!(
                target: "kakehashi::bridge",
                "codeLens/resolve: region {} is stale; returning lens unresolved",
                envelope.region_id
            );
            return Ok(lens);
        }

        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_code_lens_resolve(lens, &settings, upstream_id);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                lens = dispatch => Ok(lens),
            },
            None => Ok(dispatch.await),
        }
    }
}
