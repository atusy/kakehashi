//! Inlay hint method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the requested range, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams, NumberOrString, Range, Uri};

use super::super::Kakehashi;
use super::super::region_offset::resolve_region_offset;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{
    HostDocument, InlayHintEnvelope, envelope_host_inlay_hints, extract_inlay_hint_envelope,
};
use crate::lsp::current_upstream_id;
use crate::lsp::lsp_impl::bridge_context::parse_host_verbatim;

const METHOD: &str = "textDocument/inlayHint";

impl Kakehashi {
    pub(crate) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        // Move (not clone) the token out — `params` is consumed below.
        let work_done_token = params.work_done_progress_params.work_done_token;
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        let virt = self.inlay_hint_virt_layer(&lsp_uri, range, work_done_token);
        let host = self.inlay_hint_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |hints: &Vec<InlayHint>| !hints.is_empty(),
        )
        .await
    }

    async fn inlay_hint_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<InlayHint>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
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
                    let Some(hints) = parse_host_verbatim::<Vec<InlayHint>>(raw.value) else {
                        return Ok(None);
                    };
                    Ok(Some(HostInlayHints {
                        hints,
                        server_name: t.server_name,
                        host_uri: t.uri.to_string(),
                        incarnation: Some(raw.incarnation),
                        connection_generation: raw.connection_generation,
                        server_resolves: raw.handle.has_capability("inlayHint/resolve"),
                        connection_key: raw.handle.key().clone(),
                    }))
                }
            },
            |opt| matches!(opt, Some(v) if !v.hints.is_empty()),
            cancel_rx,
        )
        .await;
        self.host_layer_result(fan_in, METHOD, |won| {
            won.map(HostInlayHints::into_enveloped_hints)
        })
        .await
    }

    /// Virt layer: bridge the injection region under the requested range.
    async fn inlay_hint_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        client_progress_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<InlayHint>>> {
        let Some(mut ctx) = self
            .resolve_bridge_contexts_for_range(lsp_uri, range, METHOD)
            .await
        else {
            return Ok(None);
        };
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out inlay hint requests to all matching servers
        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_inlay_hint_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        range,
                        t.region_end(),
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
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
            .handle(&self.notifier(), "inlay hint", None, Ok)
            .await
    }

    pub(crate) async fn inlay_hint_resolve_impl(&self, hint: InlayHint) -> Result<InlayHint> {
        let Some(envelope) = extract_inlay_hint_envelope(&hint) else {
            return Ok(hint);
        };
        let unresolved = hint.clone();
        let region_geometry = if envelope.is_host_layer() {
            None
        } else {
            let Some((offset, region_end, contiguous)) = self.inlay_hint_region_geometry(&envelope)
            else {
                return Ok(hint);
            };
            if offset != crate::lsp::bridge::RegionOffset::from(&envelope.offset) {
                return Ok(hint);
            }
            Some((region_end, contiguous))
        };
        let region_end = region_geometry.map(|(end, _)| end);

        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_inlay_hint_resolve(hint, &settings, upstream_id, region_end);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        let resolved = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                hint = dispatch => Ok(hint),
            },
            None => Ok(dispatch.await),
        }?;

        // A later didChange/didClose is allowed to proceed once the resolve was
        // enqueued. Revalidate after the response so old lazy edits/locations
        // are never surfaced into a moved region or reopened document.
        if !self.inlay_hint_envelope_is_fresh(&envelope, &pool, region_geometry) {
            return Ok(unresolved);
        }
        Ok(resolved)
    }

    fn inlay_hint_envelope_is_fresh(
        &self,
        envelope: &InlayHintEnvelope,
        pool: &crate::lsp::bridge::LanguageServerPool,
        expected_region_geometry: Option<(tower_lsp_server::ls_types::Position, bool)>,
    ) -> bool {
        let Ok(uri) = url::Url::parse(&envelope.host_uri) else {
            return false;
        };
        if envelope
            .incarnation
            .is_none_or(|expected| pool.current_host_incarnation(&uri) != Some(expected))
        {
            return false;
        }
        envelope.is_host_layer()
            || self.inlay_hint_region_geometry(envelope).is_some_and(
                |(offset, region_end, contiguous)| {
                    offset == crate::lsp::bridge::RegionOffset::from(&envelope.offset)
                        && expected_region_geometry == Some((region_end, contiguous))
                },
            )
    }

    fn inlay_hint_region_geometry(
        &self,
        envelope: &InlayHintEnvelope,
    ) -> Option<(
        crate::lsp::bridge::RegionOffset,
        tower_lsp_server::ls_types::Position,
        bool,
    )> {
        let uri = url::Url::parse(&envelope.host_uri).ok()?;
        resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            &uri,
            &envelope.region_id,
        )
    }
}

struct HostInlayHints {
    hints: Vec<InlayHint>,
    server_name: String,
    host_uri: String,
    incarnation: Option<u64>,
    connection_generation: u64,
    server_resolves: bool,
    connection_key: crate::lsp::bridge::ConnectionKey,
}

impl HostInlayHints {
    fn into_enveloped_hints(mut self) -> Vec<InlayHint> {
        envelope_host_inlay_hints(
            &mut self.hints,
            &self.server_name,
            &self.host_uri,
            self.incarnation,
            self.connection_generation,
            &self.connection_key,
            self.server_resolves,
        );
        self.hints
    }
}
