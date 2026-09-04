//! Type-hierarchy preparation and expansion across virtual and host bridge layers.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    NumberOrString, Position, TypeHierarchyItem, TypeHierarchyPrepareParams,
    TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Uri,
};

use super::super::Kakehashi;
use super::super::region_offset::resolve_region_offset_and_language;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{
    HostDocument, TypeHierarchyDocumentRevision, TypeHierarchyEnvelope,
    envelope_host_type_hierarchy_items, extract_type_hierarchy_envelope,
    parse_type_hierarchy_items,
};
use crate::lsp::current_upstream_id;

const METHOD: &str = "textDocument/prepareTypeHierarchy";

impl Kakehashi {
    pub(crate) async fn prepare_type_hierarchy_impl(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let work_done_token = params.work_done_progress_params.work_done_token;
        let virt = self.type_hierarchy_prepare_virt_layer(&lsp_uri, position, work_done_token);
        let host = self.type_hierarchy_prepare_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |items: &Vec<TypeHierarchyItem>| !items.is_empty(),
        )
        .await
    }

    pub(crate) async fn type_hierarchy_supertypes_impl(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(envelope) = extract_type_hierarchy_envelope(&params.item) else {
            return Ok(None);
        };
        let pool = self.bridge.pool_arc();
        if !self.type_hierarchy_envelope_is_fresh(&envelope, &pool) {
            return Ok(None);
        }
        let settings = self.settings_manager.load_settings();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_type_hierarchy_supertypes(params, &settings, upstream_id);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        let items = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                items = dispatch => Ok(items),
            },
            None => Ok(dispatch.await),
        }?;
        if !self.type_hierarchy_envelope_is_fresh(&envelope, &pool) {
            return Ok(None);
        }
        Ok(items)
    }

    pub(crate) async fn type_hierarchy_subtypes_impl(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let Some(envelope) = extract_type_hierarchy_envelope(&params.item) else {
            return Ok(None);
        };
        let pool = self.bridge.pool_arc();
        if !self.type_hierarchy_envelope_is_fresh(&envelope, &pool) {
            return Ok(None);
        }
        let settings = self.settings_manager.load_settings();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_type_hierarchy_subtypes(params, &settings, upstream_id);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        let items = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                items = dispatch => Ok(items),
            },
            None => Ok(dispatch.await),
        }?;
        if !self.type_hierarchy_envelope_is_fresh(&envelope, &pool) {
            return Ok(None);
        }
        Ok(items)
    }

    fn type_hierarchy_envelope_is_fresh(
        &self,
        envelope: &TypeHierarchyEnvelope,
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
        let geometry_is_current = resolve_region_offset_and_language(
            &self.documents,
            &self.language,
            &self.bridge,
            &uri,
            &envelope.region_id,
        )
        .is_some_and(|(offset, _, contiguous, injection_language)| {
            contiguous
                && offset == crate::lsp::bridge::RegionOffset::from(&envelope.offset)
                && injection_language == envelope.injection_language
        });
        geometry_is_current && lineage_is_current()
    }

    async fn type_hierarchy_prepare_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
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
                    let Ok(items) = parse_type_hierarchy_items(raw.value) else {
                        return Ok(None);
                    };
                    Ok(Some(HostTypeHierarchyItems {
                        items,
                        server_name: t.server_name,
                        host_uri: t.uri.to_string(),
                        revision: TypeHierarchyDocumentRevision {
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
            won.map(HostTypeHierarchyItems::into_enveloped_items)
        })
        .await
    }

    async fn type_hierarchy_prepare_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        work_done_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
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
                    .send_type_hierarchy_prepare_request(
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
            .handle(&self.notifier(), "prepare type hierarchy", None, Ok)
            .await
    }
}

struct HostTypeHierarchyItems {
    items: Vec<TypeHierarchyItem>,
    server_name: String,
    host_uri: String,
    revision: TypeHierarchyDocumentRevision,
    connection_generation: u64,
    connection_key: crate::lsp::bridge::ConnectionKey,
}

impl HostTypeHierarchyItems {
    fn into_enveloped_items(mut self) -> Vec<TypeHierarchyItem> {
        envelope_host_type_hierarchy_items(
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
