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
        let envelope = extract_envelope(&params);
        // The region's live geometry, rebuilt by the gate below, so the
        // resolved edits are translated and validated against the region as
        // it is now rather than as it was when the item was produced.
        let mut live_geometry = None;
        if let Some(envelope) = &envelope {
            let Ok(host_url) = Url::parse(&envelope.host_uri) else {
                log::warn!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve: envelope host_uri {:?} is not a valid URL",
                    envelope.host_uri
                );
                return Ok(params);
            };
            // No text-revision gate here, unlike the inlay hint and code
            // action resolves: a completion list is designed to outlive edits
            // (clients filter it locally while the user keeps typing and
            // resolve on accept, which itself edits), and the downstream
            // computes the lazy fields against its own copy of the text,
            // which the bridge keeps in step (#1053 tracks the window where
            // a resolve overtakes the forwarded virtual didChange). Only the
            // lifetime and, on the virt layer, the region geometry are
            // checked.
            if !self.host_incarnation_is_current(&host_url, envelope.incarnation) {
                log::debug!(
                    target: "kakehashi::bridge",
                    "completionItem/resolve: {} was reopened since the item was produced; returning item unresolved",
                    envelope.host_uri
                );
                return Ok(params);
            }
            if !envelope.is_host_layer() {
                match self.completion_envelope_is_fresh(envelope).await {
                    Some(geometry) => live_geometry = Some(geometry),
                    None => return Ok(params),
                }
            }
        }
        // Kept for the post-response check; the gates above return `params`
        // itself, so only a resolve that is actually dispatched pays for it.
        let unresolved = envelope.as_ref().map(|_| params.clone());
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        // Propagate a client `$/cancelRequest` as RequestCancelled instead of
        // masking it as an unresolved-item success: the cancel IS forwarded
        // downstream, and the -32800 that comes back is collapsed to "no usable
        // response" by the fail-soft parsing. Mirrors `code_action_resolve_impl`.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let dispatch =
            pool.dispatch_completion_resolve(params, &settings, upstream_id, live_geometry);
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
        // A didChange/didClose/didOpen is allowed to proceed once the resolve
        // was enqueued. Revalidate after the response: the lifetime for both
        // layers, and for the virt layer the region's identity and start again
        // — the reply's edits were computed for the region where it was, and
        // translating them into where it is now would land them on the fence
        // or on unrelated text. The region's END may still have moved (typing
        // inside the region keeps the item resolvable). The rebuild can wait
        // for a reparse, so it sits INSIDE the cancellable future.
        let resolve = async {
            let resolved = dispatch.await;
            if let (Some(envelope), Some(unresolved)) = (envelope, unresolved) {
                let Ok(host_url) = Url::parse(&envelope.host_uri) else {
                    return unresolved;
                };
                if !self.host_incarnation_is_current(&host_url, envelope.incarnation) {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "completionItem/resolve: {} was reopened while resolving; returning item unresolved",
                        envelope.host_uri
                    );
                    return unresolved;
                }
                // The same rule as before dispatch: identity, start,
                // contiguity and language — not the whole offset, which
                // typing inside a blockquoted fence grows.
                if !envelope.is_host_layer()
                    && self.completion_envelope_is_fresh(&envelope).await.is_none()
                {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "completionItem/resolve: the region of {} moved while resolving; returning item unresolved",
                        envelope.host_uri
                    );
                    return unresolved;
                }
            }
            resolved
        };
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                item = resolve => Ok(item),
            },
            None => Ok(resolve.await),
        }
    }

    /// Whether the envelope still names the region it was produced for; on
    /// success, the region's live offset and end. Only the region's identity,
    /// start and contiguity are compared — its end, and the per-line column
    /// vector of a blockquoted fence, move with ordinary typing inside the
    /// region, which a completion list is designed to outlive.
    async fn completion_envelope_is_fresh(
        &self,
        envelope: &KakehashiEnvelope,
    ) -> Option<(RegionOffset, Position)> {
        let Ok(host_url) = Url::parse(&envelope.host_uri) else {
            return None;
        };
        // A resolve issued while the post-edit reparse is still running would
        // find no snapshot and read as a stale region; wait (bounded) for the
        // current parse before rebuilding the region.
        self.wait_for_resolve_parse(&host_url).await;
        let Some((offset, region_end, contiguous, live_language)) = resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            &host_url,
            &envelope.region_id,
        ) else {
            log::debug!(
                target: "kakehashi::bridge",
                "completionItem/resolve: region {} of {} is stale; returning item unresolved",
                envelope.region_id,
                envelope.host_uri
            );
            return None;
        };
        if !completion_geometry_matches(envelope, &offset, contiguous, &live_language) {
            log::debug!(
                target: "kakehashi::bridge",
                "completionItem/resolve: region {} of {} moved, lost contiguity or changed language \
                 (live offset {:?}, contiguous {contiguous}, language {live_language}); returning item unresolved",
                envelope.region_id,
                envelope.host_uri,
                offset
            );
            return None;
        }
        Some((offset, region_end))
    }
}

fn completion_geometry_matches(
    envelope: &KakehashiEnvelope,
    live_offset: &RegionOffset,
    contiguous: bool,
    live_language: &str,
) -> bool {
    let produced_at = RegionOffset::from(&envelope.offset);
    !envelope.region_id.is_empty()
        && contiguous
        // The start identifies the region; the per-line columns below it
        // grow with the region and are read live for translation.
        && produced_at.line() == live_offset.line()
        && produced_at.columns().first() == live_offset.columns().first()
        // The region may have been re-routed (a shebang edit under an
        // `unknown` injection) without moving; the item belongs to the
        // language it was produced for.
        && envelope.injection_language == live_language
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(region_id: &str) -> KakehashiEnvelope {
        serde_json::from_value(serde_json::json!({
            "origin": "lua-ls",
            "injection_language": "lua",
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
        let region = envelope("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(completion_geometry_matches(&region, &offset, true, "lua"));
        assert!(!completion_geometry_matches(
            &envelope(""),
            &offset,
            true,
            "lua"
        ));
        assert!(!completion_geometry_matches(&region, &offset, false, "lua"));
        let moved = RegionOffset::with_per_line_offsets(4, vec![2]);
        assert!(
            !completion_geometry_matches(&region, &moved, true, "lua"),
            "a region that moved is not the region the item was produced for"
        );
        assert!(
            !completion_geometry_matches(&region, &offset, true, "python"),
            "a region re-routed to another language is not the region the item was produced for"
        );
        let grown = RegionOffset::with_per_line_offsets(3, vec![2, 2, 2]);
        assert!(
            completion_geometry_matches(&region, &grown, true, "lua"),
            "a blockquoted region that grew is still the region the item was produced for"
        );
    }
}
