//! completionItem/resolve implementation for Kakehashi.
//!
//! Routes the resolve request to the single downstream server that produced
//! the completion item, identified by the Kakehashi envelope embedded in
//! `CompletionItem.data` during the original completion fan-out.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{CompletionItem, Position};
use url::Url;

use super::super::Kakehashi;
use crate::lsp::bridge::RegionOffset;
use crate::lsp::bridge::{KakehashiEnvelope, extract_envelope};
use crate::lsp::current_upstream_id;
use crate::lsp::lsp_impl::region_offset::resolve_region_offset;

impl Kakehashi {
    /// Handle a `completionItem/resolve` request.
    ///
    /// Delegates to the pool's `dispatch_completion_resolve`, which strips the
    /// envelope, routes to the origin server, and re-envelopes the result —
    /// transforming coordinates on the virt path only, since a host-layer item
    /// is already in host coordinates. Falls back gracefully at every failure
    /// point, except a client cancel, which surfaces as `RequestCancelled`.
    pub(crate) async fn completion_resolve_impl(
        &self,
        params: CompletionItem,
    ) -> Result<CompletionItem> {
        // A lazy resolve can arrive after edits changed a formerly contiguous
        // combined document into one with masked host gaps. Fail closed for
        // legacy/stale envelopes before the downstream can add new edits.
        //
        // A genuine HOST-layer item (#958) carries no region — it is forwarded
        // verbatim in host coordinates — so this gate, which resolves the
        // envelope's `region_id` and would find nothing for the empty host one,
        // is skipped. `is_host_layer` additionally requires that empty
        // `region_id`, so a conforming client can't skip the gate merely by
        // toggling `host_layer` on a virt envelope. It is not a security
        // boundary (the envelope round-trips through unprotected client `data`)
        // — it guards against accidental bypass, and the host path fails soft.
        if let Some(envelope) = extract_envelope(&params)
            && !envelope.is_host_layer()
            && !self.completion_envelope_is_fresh(&envelope)
        {
            return Ok(params);
        }
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        // Propagate a client `$/cancelRequest` as RequestCancelled instead of
        // masking it as an unresolved-item success: the cancel IS forwarded
        // downstream, and the -32800 that comes back is collapsed to "no usable
        // response" by the fail-soft parsing. Mirrors `code_action_resolve_impl`.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch = pool.dispatch_completion_resolve(params, &settings, upstream_id);
        // The cancel arm DROPS the in-flight dispatch, which then never reaches
        // its own unregister. An RAII sweep covers that — and, unlike a trailing
        // statement, also runs when this whole handler future is dropped (client
        // disconnect / shutdown), which is how the entry leaked before. The
        // CAPTURED id, not a re-read of the task-local: the sweep must target
        // exactly the id the dispatch registered under.
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                item = dispatch => Ok(item),
            },
            None => Ok(dispatch.await),
        }
    }

    fn completion_envelope_is_fresh(&self, envelope: &KakehashiEnvelope) -> bool {
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            return false;
        };
        let Some((offset, region_end, contiguous)) = resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            &host_url,
            &envelope.region_id,
        ) else {
            return false;
        };
        completion_geometry_matches(envelope, &offset, region_end, contiguous)
    }
}

fn completion_geometry_matches(
    envelope: &KakehashiEnvelope,
    live_offset: &RegionOffset,
    live_end: Position,
    contiguous: bool,
) -> bool {
    !envelope.region_id.is_empty()
        && contiguous
        && RegionOffset::from(&envelope.offset) == *live_offset
        && envelope.region_end == Some((live_end.line, live_end.character))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(region_id: &str) -> KakehashiEnvelope {
        serde_json::from_value(serde_json::json!({
            "origin": "lua-ls",
            "host_uri": "file:///test.md",
            "region_id": region_id,
            "inner": null,
            "offset": { "line": 3, "column": 2, "line_column_offsets": [2] },
            "region_end": [3, 8]
        }))
        .expect("valid envelope")
    }

    #[test]
    fn completion_resolve_requires_current_contiguous_geometry() {
        let offset = RegionOffset::with_per_line_offsets(3, vec![2]);
        let end = Position::new(3, 8);
        assert!(completion_geometry_matches(
            &envelope("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            &offset,
            end,
            true
        ));
        assert!(!completion_geometry_matches(
            &envelope(""),
            &offset,
            end,
            true
        ));
        assert!(!completion_geometry_matches(
            &envelope("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            &offset,
            end,
            false
        ));
    }
}
