//! Inlay hint methods for Kakehashi.
//!
//! `textDocument/inlayHint` walks the resolved layer order
//! (cross-layer-aggregation): the virt layer bridges the injection region
//! under the requested range, the host layer (host-document-bridge) bridges
//! the host document itself with the real URI and its coordinates verbatim.
//! Either layer re-keys label-part commands for `workspace/executeCommand`
//! routing and, when the producer advertises `inlayHint/resolve` (or its
//! payload occupies the reserved key), envelopes `data` for resolve routing.
//! The first layer producing a non-empty result wins (`preferred`).
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
        // Read before either layer snapshots the document, so the stamp can
        // only be older than the content the hints were computed on, never
        // newer: an edit landing in between makes every hint of this
        // response fail soft on resolve until the editor's next request,
        // which follows an edit anyway. Threading the snapshot's own version
        // through the shared bridge preamble would close that window.
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
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: envelope host URI {:?} does not parse; returning hint unresolved",
                envelope.host_uri
            );
            return Ok(hint);
        };
        // Every fail-soft exit below is the steady state of a hint the editor
        // kept past an edit, so it logs at debug like the sibling gates do.
        if !self.inlay_hint_content_is_fresh(&host_url, &envelope) {
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: {} was revised since the hint was produced; returning hint unresolved",
                envelope.host_uri
            );
            return Ok(hint);
        }
        let region_end = if envelope.is_host_layer() {
            None
        } else {
            // `didChange` clears the tree and reparses off-ingress; a resolve
            // issued in that window would find no snapshot and fail soft as a
            // stale region. Wait for the current parse the way the virt
            // request handlers do before their preamble snapshots it.
            self.ensure_document_parsed(&host_url).await;
            let Some((offset, region_end, contiguous, live_language)) =
                self.inlay_hint_region_geometry(&host_url, &envelope)
            else {
                log::debug!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve: region {} is stale; returning hint unresolved",
                    envelope.region_id
                );
                return Ok(hint);
            };
            if !inlay_hint_region_is_resolvable(&envelope, &offset, contiguous, &live_language) {
                log::debug!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve: region {} moved or is no longer contiguous; returning hint unresolved",
                    envelope.region_id
                );
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
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: {} was revised or reopened while resolving; returning hint unresolved",
                envelope.host_uri
            );
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
        String,
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

/// Whether the live region still matches what the envelope was minted from:
/// contiguous, at the same offset, and of the same injection language. The
/// language is client-echoed data that names the virtual document the
/// request goes to; an empty or different one would address a document the
/// producer never opened (and trips the virtual-URI invariant in debug
/// builds), so it fails soft here with the rest.
fn inlay_hint_region_is_resolvable(
    envelope: &InlayHintEnvelope,
    offset: &crate::lsp::bridge::RegionOffset,
    contiguous: bool,
    live_language: &str,
) -> bool {
    contiguous
        && *offset == crate::lsp::bridge::RegionOffset::from(&envelope.offset)
        && envelope.injection_language == live_language
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

        assert!(inlay_hint_region_is_resolvable(
            &envelope, &offset, true, "lua"
        ));
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &offset, false, "lua"
        ));
    }

    /// The envelope's language is client-echoed and names the virtual
    /// document the request is sent to; one that no longer matches the live
    /// region, or was emptied, must fail soft before a URI is built from it.
    #[test]
    fn inlay_hint_resolve_rejects_a_region_whose_language_differs() {
        let mut envelope = envelope();
        let offset = crate::lsp::bridge::RegionOffset::new(3, 2);

        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &offset, true, "python"
        ));
        envelope.injection_language.clear();
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &offset, true, "lua"
        ));
    }

    /// A region that moved (a different start line or first-line column) or
    /// whose per-line column table changed is a different geometry from the
    /// one the envelope was minted under; contiguity alone must not admit it.
    #[test]
    fn inlay_hint_resolve_rejects_a_region_whose_offset_moved() {
        let envelope = envelope();

        let moved = crate::lsp::bridge::RegionOffset::new(4, 2);
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &moved, true, "lua"
        ));
        let shifted_column = crate::lsp::bridge::RegionOffset::new(3, 3);
        assert!(!inlay_hint_region_is_resolvable(
            &envelope,
            &shifted_column,
            true,
            "lua"
        ));
        let per_line = crate::lsp::bridge::RegionOffset::with_per_line_offsets(3, vec![2, 1]);
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &per_line, true, "lua"
        ));
    }
}
