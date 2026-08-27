//! Call-hierarchy preparation across virtual and host bridge layers.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyPrepareParams, NumberOrString, Position, Uri,
};

use super::super::Kakehashi;
use super::super::region_offset::resolve_region_offset_and_language;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{
    CallHierarchyDocumentRevision, CallHierarchyEnvelope, HostDocument,
    envelope_host_call_hierarchy_items, extract_call_hierarchy_envelope,
};
use crate::lsp::current_upstream_id;
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

    pub(crate) async fn call_hierarchy_incoming_impl(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let Some(envelope) = extract_call_hierarchy_envelope(&params.item) else {
            return Ok(None);
        };
        let pool = self.bridge.pool_arc();
        if !self.call_hierarchy_envelope_is_fresh(&envelope, &pool) {
            return Ok(None);
        }

        let settings = self.settings_manager.load_settings();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_call_hierarchy_incoming(params, &settings, upstream_id);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        let calls = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                calls = dispatch => Ok(calls),
            },
            None => Ok(dispatch.await),
        }?;

        if !self.call_hierarchy_envelope_is_fresh(&envelope, &pool) {
            return Ok(None);
        }
        Ok(calls)
    }

    fn call_hierarchy_envelope_is_fresh(
        &self,
        envelope: &CallHierarchyEnvelope,
        pool: &crate::lsp::bridge::LanguageServerPool,
    ) -> bool {
        let Ok(uri) = url::Url::parse(&envelope.host_uri) else {
            return false;
        };
        let Some(expected_incarnation) = envelope.incarnation else {
            return false;
        };
        let lineage_is_current = || {
            self.documents.get(&uri).is_some_and(|document| {
                document.content_version() == envelope.content_version
                    && document.incarnation() == expected_incarnation
            }) && pool.current_host_incarnation(&uri) == Some(expected_incarnation)
        };
        if !lineage_is_current() {
            return false;
        }
        if envelope.is_host_layer() {
            return true;
        }

        // `resolve_region_offset` reads the document store itself. Do not keep
        // a DashMap read guard across that nested lookup; a queued didChange
        // writer could otherwise deadlock a task-fair shard lock. Revalidate
        // the lineage after geometry resolution to close the intervening race.
        let geometry_is_current = resolve_region_offset_and_language(
            &self.documents,
            &self.language,
            &self.bridge,
            &uri,
            &envelope.region_id,
        )
        .is_some_and(|(offset, _, contiguous, injection_language)| {
            call_hierarchy_region_geometry_is_fresh(
                &crate::lsp::bridge::RegionOffset::from(&envelope.offset),
                &offset,
                contiguous,
                &envelope.injection_language,
                &injection_language,
            )
        });
        geometry_is_current && lineage_is_current()
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

fn call_hierarchy_region_geometry_is_fresh(
    expected: &crate::lsp::bridge::RegionOffset,
    current: &crate::lsp::bridge::RegionOffset,
    contiguous: bool,
    expected_language: &str,
    current_language: &str,
) -> bool {
    contiguous && current == expected && current_language == expected_language
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::RegionOffset;

    #[test]
    fn incoming_calls_require_current_contiguous_region_geometry() {
        let expected = RegionOffset::new(3, 2);
        assert!(call_hierarchy_region_geometry_is_fresh(
            &expected, &expected, true, "lua", "lua"
        ));
        assert!(!call_hierarchy_region_geometry_is_fresh(
            &expected, &expected, false, "lua", "lua"
        ));
        assert!(!call_hierarchy_region_geometry_is_fresh(
            &expected,
            &RegionOffset::new(4, 2),
            true,
            "lua",
            "lua"
        ));
        assert!(!call_hierarchy_region_geometry_is_fresh(
            &expected, &expected, true, "lua", "luau"
        ));
    }
}
