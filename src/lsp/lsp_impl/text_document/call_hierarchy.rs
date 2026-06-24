//! Call hierarchy methods for Kakehashi (#353): position-based `prepare`, and
//! the envelope-routed `incomingCalls` / `outgoingCalls` round-trips.
//!
//! `prepare` mirrors `references_virt_layer` (single region, `dispatch_preferred`)
//! but VIRT-only — no host-layer `walk_layers` (a follow-up, like codeAction).
//! incoming/outgoing mirror `code_lens_resolve_impl`: extract the envelope from
//! `params.item`, freshness-gate the region, and route to the origin server.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    Position, Uri,
};
use ulid::Ulid;
use url::Url;

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::bridge::{CallHierarchyEnvelope, extract_call_hierarchy_envelope};
use crate::text::PositionMapper;

const PREPARE_METHOD: &str = "textDocument/prepareCallHierarchy";

impl Kakehashi {
    pub(crate) async fn prepare_call_hierarchy_impl(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let work_done_token = params.work_done_progress_params.work_done_token;

        self.prepare_call_hierarchy_virt_layer(&lsp_uri, position, work_done_token)
            .await
    }

    /// VIRT layer: bridge the injection region under the cursor.
    async fn prepare_call_hierarchy_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        client_progress_token: Option<tower_lsp_server::ls_types::NumberOrString>,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let Some(mut ctx) = self.resolve_bridge_contexts(lsp_uri, position, PREPARE_METHOD) else {
            return Ok(None);
        };
        // Aggregate the fanned-out servers' progress onto the editor's token
        // (ls-bridge-client-progress).
        ctx.document.client_progress_token = client_progress_token;

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_prepare_call_hierarchy_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
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
        pool.unregister_all_for_upstream_id(ctx.document.upstream_request_id.as_ref());

        result
            .handle(&self.client, "prepareCallHierarchy", None, Ok)
            .await
    }

    /// `callHierarchy/incomingCalls`: route `params.item` back to the downstream
    /// that produced it, identified by the envelope in `item.data` (#353).
    ///
    /// Fails soft: a non-enveloped item (host-layer or foreign) or a stale region
    /// returns `None`.
    pub(crate) async fn incoming_calls_impl(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let Some(envelope) = extract_call_hierarchy_envelope(&params.item) else {
            return Ok(None);
        };
        if !self.call_hierarchy_region_is_fresh(&envelope) {
            log::debug!(
                target: "kakehashi::bridge",
                "callHierarchy/incomingCalls: region {} is stale; returning None",
                envelope.region_id
            );
            return Ok(None);
        }
        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();
        let pool = self.bridge.pool_arc();
        Ok(pool
            .dispatch_incoming_calls(params.item, &settings, upstream_id)
            .await)
    }

    /// `callHierarchy/outgoingCalls`: same routing as incoming (#353).
    pub(crate) async fn outgoing_calls_impl(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let Some(envelope) = extract_call_hierarchy_envelope(&params.item) else {
            return Ok(None);
        };
        if !self.call_hierarchy_region_is_fresh(&envelope) {
            log::debug!(
                target: "kakehashi::bridge",
                "callHierarchy/outgoingCalls: region {} is stale; returning None",
                envelope.region_id
            );
            return Ok(None);
        }
        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();
        let pool = self.bridge.pool_arc();
        Ok(pool
            .dispatch_outgoing_calls(params.item, &settings, upstream_id)
            .await)
    }

    /// Whether the envelope's injection region still exists at the position it
    /// had when the item was minted (mirrors `code_lens_region_is_fresh`).
    fn call_hierarchy_region_is_fresh(&self, envelope: &CallHierarchyEnvelope) -> bool {
        let Ok(uri) = Url::parse(&envelope.host_uri) else {
            return false;
        };
        let Ok(ulid) = envelope.region_id.parse::<Ulid>() else {
            return false;
        };
        let Some((start_byte, _end, _kind)) =
            self.bridge.node_tracker().lookup_position(&uri, &ulid)
        else {
            return false;
        };
        let Some(doc) = self.documents.get(&uri) else {
            return false;
        };
        let mapper = PositionMapper::new(doc.text());
        let Some(position) = mapper.byte_to_position(start_byte) else {
            return false;
        };
        position.line == envelope.offset.line && position.character == envelope.offset.column
    }
}
