//! Inlay hint methods for Kakehashi.
//!
//! `textDocument/inlayHint` walks the resolved layer order
//! (cross-layer-aggregation): the virt layer bridges the injection region
//! under the requested range, the host layer (host-document-bridge) bridges
//! the host document itself with the real URI and its coordinates verbatim.
//! Either layer re-keys label-part commands for `workspace/executeCommand`
//! routing and envelopes `data` for `inlayHint/resolve` routing. The first
//! layer producing a non-empty result wins (`preferred`).
//!
//! `inlayHint/resolve` gates the echoed envelope on freshness (content
//! version, and for the virt layer the live region offset and contiguity),
//! forwards to the exact producing connection, and re-checks the content
//! version and host incarnation after the response.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams, NumberOrString, Range, Uri};

use super::super::Kakehashi;
use super::super::region_offset::resolve_region_offset;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{
    HostDocument, InlayHintDocumentRevision, InlayHintEnvelope, envelope_host_inlay_hints,
    extract_inlay_hint_envelope,
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
        let Some(content_version) = url::Url::parse(lsp_uri.as_str())
            .ok()
            .and_then(|uri| self.documents.get(&uri).map(|doc| doc.content_version()))
        else {
            return Ok(None);
        };

        let virt = self.inlay_hint_virt_layer(&lsp_uri, range, work_done_token, content_version);
        let host = self.inlay_hint_host_layer(&lsp_uri, raw_params, content_version);
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
        content_version: u64,
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
                        content_version,
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
        content_version: u64,
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
            .handle(&self.notifier(), "inlay hint", None, Ok)
            .await
    }

    pub(crate) async fn inlay_hint_resolve_impl(&self, hint: InlayHint) -> Result<InlayHint> {
        let Some(envelope) = extract_inlay_hint_envelope(&hint) else {
            return Ok(hint);
        };
        // One parse serves every gate below. An unparsable host URI can only
        // be a mangled echo and fails soft like any other stale envelope.
        let Ok(host_url) = url::Url::parse(&envelope.host_uri) else {
            return Ok(hint);
        };
        if !self.inlay_hint_content_is_fresh(&host_url, &envelope) {
            return Ok(hint);
        }
        let region_end = if envelope.is_host_layer() {
            None
        } else {
            let Some((offset, region_end, contiguous)) =
                self.inlay_hint_region_geometry(&host_url, &envelope)
            else {
                return Ok(hint);
            };
            if !inlay_hint_region_is_resolvable(&envelope, &offset, contiguous) {
                return Ok(hint);
            }
            Some(region_end)
        };

        // Kept for the post-response check; the gates above return `hint`
        // itself, so only a resolve that is actually dispatched pays for it.
        let unresolved = hint.clone();
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
        // enqueued. Revalidate after the response so lazy edits/locations
        // computed against the content this resolve observed are not
        // surfaced into a revised or reopened document. The region is
        // deliberately not rebuilt a second time (an injection walk over the
        // whole tree): a content edit advances the version, a reopen changes
        // the incarnation, and a settings/query reload that could re-track
        // the region invalidates every parse, which advances the version as
        // well (`Document::invalidate_parse`), so the two stamps cover every
        // way the geometry can move.
        if !self.inlay_hint_envelope_is_fresh(&host_url, &envelope, &pool) {
            return Ok(unresolved);
        }
        Ok(resolved)
    }

    fn inlay_hint_envelope_is_fresh(
        &self,
        host_url: &url::Url,
        envelope: &InlayHintEnvelope,
        pool: &crate::lsp::bridge::LanguageServerPool,
    ) -> bool {
        self.inlay_hint_content_is_fresh(host_url, envelope)
            && envelope
                .incarnation
                .is_some_and(|expected| pool.current_host_incarnation(host_url) == Some(expected))
    }

    fn inlay_hint_content_is_fresh(
        &self,
        host_url: &url::Url,
        envelope: &InlayHintEnvelope,
    ) -> bool {
        envelope.content_version.is_some_and(|expected| {
            self.documents
                .get(host_url)
                .is_some_and(|document| document.content_version() == expected)
        })
    }

    fn inlay_hint_region_geometry(
        &self,
        host_url: &url::Url,
        envelope: &InlayHintEnvelope,
    ) -> Option<(
        crate::lsp::bridge::RegionOffset,
        tower_lsp_server::ls_types::Position,
        bool,
    )> {
        resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            host_url,
            &envelope.region_id,
        )
    }
}

fn inlay_hint_region_is_resolvable(
    envelope: &InlayHintEnvelope,
    offset: &crate::lsp::bridge::RegionOffset,
    contiguous: bool,
) -> bool {
    contiguous && *offset == crate::lsp::bridge::RegionOffset::from(&envelope.offset)
}

struct HostInlayHints {
    hints: Vec<InlayHint>,
    server_name: String,
    host_uri: String,
    incarnation: Option<u64>,
    content_version: u64,
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
            InlayHintDocumentRevision {
                incarnation: self.incarnation,
                content_version: self.content_version,
            },
            self.connection_generation,
            &self.connection_key,
            self.server_resolves,
        );
        self.hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> InlayHintEnvelope {
        serde_json::from_value(serde_json::json!({
            "origin": "test",
            "host_uri": "file:///test.md",
            "region_id": "region",
            "injection_language": "lua",
            "incarnation": 1,
            "content_version": 1,
            "connection_generation": 1,
            "offset": { "line": 3, "column": 2 },
            "inner": null
        }))
        .unwrap()
    }

    #[test]
    fn inlay_hint_resolve_rejects_non_contiguous_region() {
        let envelope = envelope();
        let offset = crate::lsp::bridge::RegionOffset::new(3, 2);

        assert!(inlay_hint_region_is_resolvable(&envelope, &offset, true));
        assert!(!inlay_hint_region_is_resolvable(&envelope, &offset, false));
    }
}
