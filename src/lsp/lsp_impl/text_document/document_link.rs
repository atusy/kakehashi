//! Document link method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentLink, DocumentLinkParams};

use super::super::Kakehashi;
use crate::lsp::bridge::{envelope_host_document_links, extract_document_link_envelope};
use crate::lsp::current_upstream_id;

impl Kakehashi {
    pub(crate) async fn document_link_impl(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        self.whole_document_fan_out(
            &params.text_document.uri,
            "textDocument/documentLink",
            raw_params,
            // documentLink is fast; not advertised for client progress (#437), so
            // no token is carried.
            None,
            |t| async move {
                t.pool
                    .send_document_link_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
            |mut won| {
                let server_resolves = won.handle.has_capability("documentLink/resolve");
                envelope_host_document_links(
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

    pub(crate) async fn document_link_resolve_impl(
        &self,
        link: DocumentLink,
    ) -> Result<DocumentLink> {
        let Some(envelope) = extract_document_link_envelope(&link) else {
            return Ok(link);
        };
        if !envelope.is_host_layer()
            && !self
                .region_offset_is_fresh(
                    &envelope.host_uri,
                    &envelope.region_id,
                    &envelope.offset,
                    envelope.incarnation,
                    &envelope.injection_language,
                )
                .await
        {
            return Ok(link);
        }

        // Kept for the post-response check; the gate above returns `link`
        // itself, so only a resolve that is actually dispatched pays for it.
        let unresolved = link.clone();
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_document_link_resolve(link, &settings, upstream_id);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        // A didChange/didClose/didOpen is allowed to proceed once the resolve
        // was enqueued. Revalidate after the response: the lifetime for both
        // layers, and for the virt layer the region geometry again (these
        // envelopes carry no revision stamp), so a command or target resolved
        // for a region that moved, or for the closed document, is not
        // surfaced into the current one. The revalidation can wait for a
        // reparse, so it sits INSIDE the cancellable future: a cancel that
        // lands during that wait is honoured, not answered with a result.
        let resolve = async {
            let resolved = dispatch.await;
            let still_fresh = if envelope.is_host_layer() {
                url::Url::parse(&envelope.host_uri).is_ok_and(|host_url| {
                    self.host_incarnation_is_current(&host_url, envelope.incarnation)
                })
            } else {
                self.region_offset_is_fresh(
                    &envelope.host_uri,
                    &envelope.region_id,
                    &envelope.offset,
                    envelope.incarnation,
                    &envelope.injection_language,
                )
                .await
            };
            if !still_fresh {
                log::debug!(
                    target: "kakehashi::bridge",
                    "documentLink/resolve: {} was revised or reopened while resolving; returning link unresolved",
                    envelope.host_uri
                );
                return unresolved;
            }
            resolved
        };
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                link = resolve => Ok(link),
            },
            None => Ok(resolve.await),
        }
    }
}
