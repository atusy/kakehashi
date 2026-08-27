//! Document link method for Kakehashi.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentLink, DocumentLinkParams};
use ulid::Ulid;
use url::Url;

use super::super::Kakehashi;
use crate::lsp::bridge::{
    DocumentLinkEnvelope, envelope_host_document_links, extract_document_link_envelope,
};
use crate::lsp::current_upstream_id;
use crate::text::PositionMapper;

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
        if !envelope.is_host_layer() && !self.document_link_region_is_fresh(&envelope) {
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

    fn document_link_region_is_fresh(&self, envelope: &DocumentLinkEnvelope) -> bool {
        let Ok(uri) = Url::parse(&envelope.host_uri) else {
            return false;
        };
        let Ok(ulid) = envelope.region_id.parse::<Ulid>() else {
            return false;
        };
        let Some((start_byte, _end, _kind, tracked_incarnation)) =
            self.bridge.node_tracker().lookup_position(&uri, &ulid)
        else {
            return false;
        };
        let Some(doc) = self.documents.get(&uri) else {
            return false;
        };
        if doc.incarnation() != tracked_incarnation {
            return false;
        }
        let mapper = PositionMapper::new(doc.text());
        let Some(position) = mapper.byte_to_position(start_byte) else {
            return false;
        };
        position.line == envelope.offset.line && position.character == envelope.offset.column
    }
}
