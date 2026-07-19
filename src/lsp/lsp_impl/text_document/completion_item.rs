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
    /// envelope, routes to the origin server, transforms coordinates, and
    /// re-envelopes the result. Falls back gracefully at every failure point.
    pub(crate) async fn completion_resolve_impl(
        &self,
        params: CompletionItem,
    ) -> Result<CompletionItem> {
        // A lazy resolve can arrive after edits changed a formerly contiguous
        // combined document into one with masked host gaps. Fail closed for
        // legacy/stale envelopes before the downstream can add new edits.
        if let Some(envelope) = extract_envelope(&params)
            && !self.completion_envelope_is_fresh(&envelope).await
        {
            return Ok(params);
        }
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let item = pool
            .dispatch_completion_resolve(params, &settings, upstream_id)
            .await;
        Ok(item)
    }

    async fn completion_envelope_is_fresh(&self, envelope: &KakehashiEnvelope) -> bool {
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            return false;
        };
        let Some((offset, region_end, contiguous)) = resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            Some(&self.tree_worker_shadow),
            &host_url,
            &envelope.region_id,
        )
        .await
        else {
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
