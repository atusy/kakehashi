//! LinkedEditingRange method for Kakehashi.
//!
//! Walks the resolved layer order (cross-layer-aggregation): the virt layer
//! bridges the injection region under the cursor, the host layer
//! (host-document-bridge) bridges the host document itself with the real URI
//! and the response verbatim. The first layer producing a non-empty result
//! wins (`preferred`).

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{LinkedEditingRangeParams, LinkedEditingRanges, Position, Uri};

use super::super::Kakehashi;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::{Sourced, parse_host_verbatim};

const METHOD: &str = "textDocument/linkedEditingRange";

impl Kakehashi {
    pub(crate) async fn linked_editing_range_impl(
        &self,
        params: LinkedEditingRangeParams,
    ) -> Result<Option<LinkedEditingRanges>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Before any layer reads the host's text (see `PreparedRace`).
        let race = self.prepared_race(&lsp_uri);
        let virt = self.linked_editing_range_virt_layer(&lsp_uri, position, race.revision());
        let answer = self
            .walk_layers(
                &lsp_uri,
                METHOD,
                METHOD,
                raw_params,
                virt,
                |value| parse_host_verbatim::<LinkedEditingRanges>(value).map(Sourced::unprepared),
                // Read as the race decides: a prepared answer moved or stale
                // is no answer of its layer, and the race falls through to
                // the next layer's.
                |sourced: &Sourced<LinkedEditingRanges>| {
                    race.answers(sourced, |linked| !linked.ranges.is_empty())
                },
            )
            .await;
        // The final answer chosen (see `PreparedRace::conclude`): a range
        // in host coordinates, checked once more against the host, which
        // may have moved while it was held (text above the region edited,
        // say); another layer's standing in for one passed over only while
        // the host is still the text the request read. A dropped answer is
        // `ContentModified`: `null` would say there is nothing here.
        race.conclude(answer, &lsp_uri, "linkedEditingRange", log::Level::Debug)
    }

    /// Virt layer: bridge the injection region under the cursor.
    async fn linked_editing_range_virt_layer(
        &self,
        lsp_uri: &Uri,
        position: Position,
        revision: Option<crate::lsp::bridge::HostRevision>,
    ) -> Result<Option<Sourced<LinkedEditingRanges>>> {
        let Some(ctx) = self
            .resolve_bridge_contexts(lsp_uri, position, METHOD)
            .await
        else {
            return Ok(None);
        };

        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());

        // Fan-out linkedEditingRange requests to all matching servers
        let pool = self.bridge.pool_arc();
        let position = ctx.position;
        let result = dispatch_preferred(
            &ctx.document,
            pool.clone(),
            |t| async move {
                t.pool
                    .send_linked_editing_range_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        position,
                        t.region_end(),
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        t.upstream_id,
                    )
                    .await
            },
            |opt| opt.is_some(),
            cancel_rx,
        )
        .await;
        let ranges = result
            .handle(&self.notifier(), "linkedEditingRange", None, Ok)
            .await?;
        Ok(ranges.map(|ranges| Sourced::from_region(&ctx.document, ranges, revision)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under a cross-layer `preferred`, a prepared answer the race finds
    /// stale is no answer: the host layer's stands in, waited for when it
    /// is still running. One the host moved under leaves none to stand in
    /// (the host layer's ranges were read for the text it moved from too):
    /// `ContentModified`. One still current wins as ever.
    #[tokio::test]
    async fn linked_ranges_that_may_not_be_kept_fall_through_to_the_host_layers() {
        let range = |line: u32| LinkedEditingRanges {
            ranges: vec![tower_lsp_server::ls_types::Range::new(
                Position::new(line, 0),
                Position::new(line, 1),
            )],
            word_pattern: None,
        };
        use crate::lsp::lsp_impl::bridge_context::Verdict;
        for (verdict, host_delay_ms) in [
            (Verdict::Current, 0),
            (Verdict::Stale, 0),
            (Verdict::Stale, 20),
            (Verdict::Moved, 0),
            (Verdict::Moved, 20),
        ] {
            let won = super::super::super::bridge_context::race_prepared_virt_against_host(
                range(1),
                range(2),
                host_delay_ms,
                verdict,
                |linked: &LinkedEditingRanges| !linked.ranges.is_empty(),
            )
            .await;
            match verdict {
                Verdict::Current => assert_eq!(won.unwrap(), Some(range(1))),
                Verdict::Stale => assert_eq!(won.unwrap(), Some(range(2)), "{host_delay_ms}"),
                Verdict::Moved => assert_eq!(
                    won.unwrap_err().code,
                    tower_lsp_server::jsonrpc::ErrorCode::ServerError(-32801),
                    "{host_delay_ms}"
                ),
            }
        }
    }
}
