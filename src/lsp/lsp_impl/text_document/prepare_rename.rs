//! PrepareRename method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    Position, PrepareRenameResponse, TextDocumentPositionParams, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::{Sourced, parse_host_verbatim};

const METHOD: &str = "textDocument/prepareRename";

impl Kakehashi {
    pub(crate) async fn prepare_rename_impl(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri;
        let position = params.position;

        let virt = self.prepare_rename_virt_layer(&lsp_uri, position);
        let native = async {
            let answer = self
                .native_bindings_answer(&lsp_uri, position, |ctx| {
                    super::native_bindings::native_prepare_rename(ctx)
                })
                .await?;
            Ok(answer.map(Sourced::unprepared))
        };
        let answer = self
            .walk_layers_with_native(
                &lsp_uri,
                METHOD,
                METHOD,
                raw_params,
                virt,
                native,
                |value| {
                    parse_host_verbatim::<PrepareRenameResponse>(value).map(Sourced::unprepared)
                },
                // Read as the race decides: a prepared answer moved or stale
                // is no answer of its layer, and the race falls through to
                // the next layer's.
                |sourced: &Sourced<PrepareRenameResponse>| {
                    super::super::bridge_context::layer_answers(
                        sourced,
                        |_| true,
                        |sourced| self.answer_may_keep(&lsp_uri, sourced),
                    )
                },
            )
            .await?;
        // The final answer chosen: a range in host coordinates, checked once
        // more against the host text, which may have moved while it was held
        // (text above the region edited, say), by the same check as other
        // prepared answers (see `sources_verdict`): waited on only while the
        // host's last edit is settling, a moved or stale answer none at once.
        let Some(sourced) = answer else {
            return Ok(None);
        };
        let verdict = self
            .settled_verdict_of(&lsp_uri, std::iter::once(&sourced))
            .await;
        if !verdict.keeps() {
            log::debug!(
                target: "kakehashi::bridge",
                "prepareRename result for {} dropped: {verdict}",
                lsp_uri.as_str()
            );
            return Ok(None);
        }
        Ok(Some(sourced.value))
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn prepare_rename_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
    ) -> Result<Option<Sourced<PrepareRenameResponse>>> {
        // Before the text is read: see `Sourced::from_region`.
        let revision = self.prepared_request_revision(lsp_uri);
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out prepareRename requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_prepare_rename_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
                        t.region_end(),
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
            |opt| opt.is_some(),
            cancel_rx,
        )
        .await;
        let answer = result
            .handle(&self.notifier(), "prepareRename", None, Ok)
            .await?;
        Ok(answer.map(|answer| {
            Sourced::from_region(&ctx.document, answer, self.bridge.node_tracker(), revision)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under a cross-layer `preferred`, a prepared answer the race finds
    /// moved or stale is no answer: the host layer's wins, waited for when
    /// it is still running. One that may be kept wins as ever.
    #[tokio::test]
    async fn a_rename_range_that_may_not_be_kept_falls_through_to_the_host_layers() {
        let range = |line: u32| {
            PrepareRenameResponse::Range(tower_lsp_server::ls_types::Range::new(
                Position::new(line, 0),
                Position::new(line, 1),
            ))
        };
        for (may_keep, host_delay_ms) in [(true, 0), (false, 0), (false, 20)] {
            let won = super::super::super::bridge_context::race_prepared_virt_against_host(
                range(1),
                range(2),
                host_delay_ms,
                may_keep,
                |_| true,
            )
            .await;
            assert_eq!(
                won,
                range(if may_keep { 1 } else { 2 }),
                "{may_keep} {host_delay_ms}"
            );
        }
    }
}
