//! Document link method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentLink, DocumentLinkParams};

use super::super::Kakehashi;
use super::super::bridge_context::parse_host_verbatim;
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
            None,
            None,
            None,
            None,
            false,
            false,
            false,
            true,
            std::future::ready(Ok(None)),
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
            parse_host_verbatim::<Vec<DocumentLink>>,
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
            |mut acc, next| {
                acc.extend(next);
                acc
            },
            |mut acc, next| {
                acc.extend(next);
                acc
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
            && !self.region_offset_is_fresh(
                &envelope.host_uri,
                &envelope.region_id,
                &envelope.offset,
            )
        {
            return Ok(link);
        }

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
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                link = dispatch => Ok(link),
            },
            None => Ok(dispatch.await),
        }
    }
}
