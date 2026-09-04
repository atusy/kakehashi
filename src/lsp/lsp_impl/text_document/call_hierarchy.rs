//! Call-hierarchy preparation across virtual and host bridge layers.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CallHierarchyItem, CallHierarchyPrepareParams, NumberOrString, Position, Uri,
};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{
    CallHierarchyDocumentRevision, HostDocument, envelope_host_call_hierarchy_items,
};
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/prepareCallHierarchy";

impl Kakehashi {
    pub(crate) async fn prepare_call_hierarchy_impl(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let work_done_token = params.work_done_progress_params.work_done_token;
        let virt = self.call_hierarchy_prepare_virt_layer(&lsp_uri, position, work_done_token);
        let host = self.call_hierarchy_prepare_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |items: &Vec<CallHierarchyItem>| !items.is_empty(),
        )
        .await
    }

    async fn call_hierarchy_prepare_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let incarnation = ctx.incarnation;
        let content_version = ctx.content_version;
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let fan_in = dispatch_host_preferred(
            &ctx,
            pool,
            move |t: HostFanOutTask| {
                let params = raw_params.clone();
                async move {
                    let raw = t
                        .pool
                        .send_host_raw_request_for_incarnation(
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
                            incarnation,
                        )
                        .await?;
                    let Some(raw) = raw else {
                        return Ok(None);
                    };
                    let Some(items) = parse_host_verbatim::<Vec<CallHierarchyItem>>(raw.value)
                    else {
                        return Ok(None);
                    };
                    Ok(Some(HostCallHierarchyItems {
                        items,
                        server_name: t.server_name,
                        host_uri: t.uri.to_string(),
                        revision: CallHierarchyDocumentRevision {
                            incarnation: Some(raw.incarnation),
                            content_version,
                        },
                        connection_generation: raw.connection_generation,
                        connection_key: raw.handle.key().clone(),
                    }))
                }
            },
            |opt| matches!(opt, Some(v) if !v.items.is_empty()),
            cancel_rx,
        )
        .await;
        self.host_layer_result(fan_in, METHOD, |won| {
            won.map(HostCallHierarchyItems::into_enveloped_items)
        })
        .await
    }

    async fn call_hierarchy_prepare_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        work_done_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let Some(mut ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };
        ctx.document.client_progress_token = work_done_token;
        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let incarnation = ctx.incarnation;
        let content_version = ctx.content_version;
        let result = dispatch_preferred(
            &ctx.document,
            pool,
            |t| async move {
                t.pool
                    .send_call_hierarchy_prepare_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
                        t.region_end(),
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        incarnation,
                        content_version,
                        t.upstream_id,
                        t.client_progress_token,
                    )
                    .await
            },
            |opt| matches!(opt, Some(v) if !v.is_empty()),
            cancel_rx,
        )
        .await;
        result
            .handle(&self.notifier(), "prepare call hierarchy", None, Ok)
            .await
    }
}

struct HostCallHierarchyItems {
    items: Vec<CallHierarchyItem>,
    server_name: String,
    host_uri: String,
    revision: CallHierarchyDocumentRevision,
    connection_generation: u64,
    connection_key: crate::lsp::bridge::ConnectionKey,
}

impl HostCallHierarchyItems {
    fn into_enveloped_items(mut self) -> Vec<CallHierarchyItem> {
        envelope_host_call_hierarchy_items(
            &mut self.items,
            &self.server_name,
            &self.host_uri,
            self.revision,
            self.connection_generation,
            &self.connection_key,
        );
        self.items
    }
}
