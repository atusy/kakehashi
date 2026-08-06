//! Completion method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing completion items, or an
//! incomplete `CompletionList` that asks the client to re-query, wins
//! (`preferred`).
//!
//! The host layer cannot use the generic verbatim raw-value walk: its items
//! need the routing envelope that makes `completionItem/resolve` reach the host
//! server that produced them (#958), so it dispatches typed per server to keep
//! the server name and its advertised capabilities, and hands both to the
//! bridge's `bridge_host_completion_items` policy.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CompletionList, CompletionParams, CompletionResponse, MessageType, Position, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    FanInResult, HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{HostDocument, bridge_host_completion_items};
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/completion";

impl Kakehashi {
    pub(crate) async fn completion_impl(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let virt = self.completion_virt_layer(&lsp_uri, position);
        let host = self.completion_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            completion_response_has_result,
        )
        .await
    }

    /// Host layer: forward the params verbatim to the host language's own
    /// servers, then envelope each item so a later `completionItem/resolve`
    /// routes back to the server that produced it (#958). Coordinates stay
    /// untranslated — they are already real.
    ///
    /// The envelope is minted only for a server that advertises
    /// `completionItem/resolve`: for one that does not, it would be pure wire
    /// weight on every item of every completion, and the resolve would fail
    /// soft back to the unresolved item anyway.
    async fn completion_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<CompletionResponse>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let f = move |t: HostFanOutTask| {
            let params = raw_params.clone();
            async move {
                let raw = t
                    .pool
                    .send_host_raw_request(
                        &t.server_name,
                        &t.server_config,
                        &HostDocument {
                            uri: &t.uri,
                            language_id: &t.language_id,
                            text: &t.text,
                        },
                        METHOD,
                        params,
                        t.upstream_id,
                    )
                    .await?;
                let Some(raw) = raw else {
                    return Ok(None);
                };
                let Some(mut response) = parse_host_verbatim::<CompletionResponse>(raw.value)
                else {
                    return Ok(None);
                };
                bridge_host_completion_items(
                    &mut response,
                    &t.server_name,
                    t.uri.as_str(),
                    raw.handle.has_capability("completionItem/resolve"),
                );
                Ok(Some(response))
            }
        };
        // No layer-level `unregister_all` here: the virt layer runs
        // concurrently under the SAME upstream id, so wiping the registry on
        // this layer's completion would drop the sibling's live cancel
        // registrations. `run_layer_race` sweeps after the whole race.
        let fan_in = dispatch_host_preferred(
            &ctx,
            pool.clone(),
            f,
            |opt| matches!(opt, Some(v) if completion_response_has_result(v)),
            cancel_rx,
        )
        .await;
        // Quieter than `FanInResult::handle`: an all-empty host layer is the
        // normal outcome whenever virt answers, so only real failures surface.
        match fan_in {
            FanInResult::Done(value) => Ok(value),
            FanInResult::NoResult { errors } => {
                if errors > 0 {
                    self.notifier()
                        .log(
                            MessageType::WARNING,
                            format!("No {METHOD} response from any host bridge server"),
                        )
                        .await;
                }
                Ok(None)
            }
            FanInResult::Cancelled => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
        }
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn completion_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
    ) -> Result<Option<CompletionResponse>> {
        // Use shared preamble to resolve injection context with ALL matching servers
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out completion requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_completion_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
            |opt| opt.as_ref().is_some_and(completion_list_has_result),
            cancel_rx,
        )
        .await;

        result
            .handle(&self.notifier(), "completion", None, |v| {
                Ok(v.map(CompletionResponse::List))
            })
            .await
    }
}

fn completion_response_has_result(resp: &CompletionResponse) -> bool {
    match resp {
        CompletionResponse::Array(items) => !items.is_empty(),
        CompletionResponse::List(list) => completion_list_has_result(list),
    }
}

fn completion_list_has_result(list: &CompletionList) -> bool {
    // Keep the incomplete-empty rule in sync with `is_empty_layer_value`,
    // which applies the same LSP CompletionList semantics to raw JSON.
    list.is_incomplete || !list.items.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_empty_completion_list_counts_as_result() {
        let response = CompletionResponse::List(CompletionList {
            is_incomplete: true,
            items: vec![],
        });

        assert!(completion_response_has_result(&response));
    }

    #[test]
    fn complete_empty_completion_list_counts_as_empty() {
        let response = CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items: vec![],
        });

        assert!(!completion_response_has_result(&response));
    }
}
