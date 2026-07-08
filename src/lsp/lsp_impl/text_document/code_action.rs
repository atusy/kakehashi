//! Code action method for Kakehashi (#568, edit-carrying actions only).
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the requested range, the host layer
//! (host-document-bridge) bridges the host document itself. Both apply the
//! `"{title} — {server}"` suffix, so the host arm cannot use the generic
//! verbatim raw-value walk — it dispatches typed per server to keep the
//! server name ([`Kakehashi::walk_layer_futures`]).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CodeActionContext, CodeActionParams, CodeActionResponse, MessageType, NumberOrString, Range,
    Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{FanInResult, dispatch_host_preferred, dispatch_preferred};
use crate::lsp::bridge::{
    HostDocument, UpstreamCodeActionCaps, bridge_code_actions, parse_code_actions_leniently,
};

const METHOD: &str = "textDocument/codeAction";

impl Kakehashi {
    pub(crate) async fn code_action_impl(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let work_done_token = params.work_done_progress_params.work_done_token;
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let context = params.context;
        let upstream_caps = self.upstream_code_action_caps();

        let virt =
            self.code_action_virt_layer(&lsp_uri, range, context, work_done_token, upstream_caps);
        let host = self.code_action_host_layer(&lsp_uri, raw_params, upstream_caps);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |actions: &CodeActionResponse| !actions.is_empty(),
        )
        .await
    }

    /// The upstream client's codeAction capabilities that gate the bridge
    /// policy: `disabledSupport` (LSP 3.16) drives disable-vs-drop for
    /// actions the bridge cannot execute yet; `isPreferredSupport`
    /// (LSP 3.15) gates the isPreferred passthrough.
    fn upstream_code_action_caps(&self) -> UpstreamCodeActionCaps {
        let code_action = self
            .settings_manager
            .client_capabilities_lock()
            .get()
            .and_then(|caps| caps.text_document.as_ref())
            .and_then(|td| td.code_action.as_ref());
        UpstreamCodeActionCaps {
            disabled_support: code_action
                .and_then(|ca| ca.disabled_support)
                .unwrap_or(false),
            is_preferred_support: code_action
                .and_then(|ca| ca.is_preferred_support)
                .unwrap_or(false),
        }
    }

    /// Virt layer: bridge the injection region under the requested range.
    async fn code_action_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        context: CodeActionContext,
        client_progress_token: Option<NumberOrString>,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Result<Option<CodeActionResponse>> {
        let Some(mut ctx) = self
            .resolve_bridge_contexts_for_range_overlap(lsp_uri, range, METHOD)
            .await
        else {
            return Ok(None);
        };
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| {
                let context = context.clone();
                async move {
                    t.pool
                        .send_code_action_request(
                            &t.server_name,
                            &t.server_config,
                            &t.uri,
                            range,
                            context,
                            &t.injection_language,
                            &t.region_id,
                            t.offset,
                            &t.virtual_content,
                            t.upstream_id,
                            t.client_progress_token,
                            upstream_caps,
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

    /// Host layer: forward the params verbatim to the host language's own
    /// servers, then apply the bridge action policy (suffix, command
    /// disabling) per winning server. Edits stay verbatim — real URIs and
    /// coordinates need no translation.
    async fn code_action_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
        upstream_caps: UpstreamCodeActionCaps,
    ) -> Result<Option<CodeActionResponse>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let result = dispatch_host_preferred(
            &ctx,
            pool.clone(),
            move |t| {
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
                    Ok(raw.and_then(|value| {
                        let actions = parse_code_actions_leniently(value)?;
                        Some(bridge_code_actions(
                            actions,
                            &t.server_name,
                            upstream_caps,
                            None,
                        ))
                    }))
                }
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
        // Same quieting as the generic host arm: an all-empty host layer is
        // the normal outcome whenever virt answers; only real failures log.
        match result {
            FanInResult::Done(value) => Ok(value),
            FanInResult::NoResult { errors } => {
                if errors > 0 {
                    self.client
                        .log_message(
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
}
