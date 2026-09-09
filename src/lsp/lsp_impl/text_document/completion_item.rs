//! completionItem/resolve implementation for Kakehashi.
//!
//! Routes the resolve request to the single downstream server that produced
//! the completion item, identified by the Kakehashi envelope embedded in
//! `CompletionItem.data` during the original completion fan-out.

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::CompletionItem;
use url::Url;

use super::super::Kakehashi;
use crate::lsp::bridge::{CompletionResolveDocument, HostRevision, RegionOffset};
use crate::lsp::bridge::{KakehashiEnvelope, extract_envelope};
use crate::lsp::current_upstream_id;
use crate::lsp::lsp_impl::region_offset::{resolve_region, resolved_region_geometry};

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
        if let Some(envelope) = &envelope {
            let Ok(uri) = Url::parse(&envelope.host_uri) else {
                return Ok(params);
            };
            // Refuse an old lifetime before connection acquisition or a parse
            // wait on the reopened document. Rechecked under the edit lock.
            if !self.host_incarnation_is_current(&uri, envelope.incarnation) {
                return Ok(params);
            }
        }
        let unresolved = envelope.as_ref().map(|_| params.clone());
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            upstream_id.clone(),
        );
        let resolve = async {
            let mut revision_then = None;
            // Polled only after the origin connection is ready. Do not hold an
            // edit lock during a handshake, parse wait, or downstream reply.
            let document = async {
                let envelope = envelope.as_ref()?;
                let host_url = Url::parse(&envelope.host_uri).ok()?;
                if !self.host_incarnation_is_current(&host_url, envelope.incarnation) {
                    return None;
                }
                if !envelope.is_host_layer() {
                    self.wait_for_resolve_parse(&host_url).await;
                }
                let edit_lock = self.documents.edit_lock(&host_url);
                let edit_guard = std::sync::Arc::clone(&edit_lock).lock_owned().await;
                let prepared = (|| {
                    if !self.host_incarnation_is_current(&host_url, envelope.incarnation) {
                        return None;
                    }
                    let (host_text, incarnation, content_version) = {
                        let doc = self.documents.get(&host_url)?;
                        (doc.text_arc(), doc.incarnation(), doc.content_version())
                    };
                    if Some(incarnation) != envelope.incarnation {
                        return None;
                    }
                    let (language_id, text, geometry) = if envelope.is_host_layer() {
                        (self.document_language(&host_url)?, host_text, None)
                    } else {
                        let region = resolve_region(
                            &self.documents,
                            &self.language,
                            &self.bridge,
                            &host_url,
                            &envelope.region_id,
                        )?;
                        let text = std::sync::Arc::from(region.virtual_content.as_str());
                        let (offset, end, contiguous, language) = resolved_region_geometry(region);
                        if !completion_geometry_matches(envelope, &offset, contiguous, &language) {
                            return None;
                        }
                        (language, text, Some((offset, end)))
                    };
                    Some((
                        language_id,
                        text,
                        geometry,
                        HostRevision {
                            incarnation,
                            content_version,
                        },
                    ))
                })();
                let Some((language_id, text, geometry, revision)) = prepared else {
                    drop(edit_guard);
                    self.documents
                        .remove_edit_lock_if_unshared(&host_url, &edit_lock);
                    return None;
                };
                revision_then = Some(revision.content_version);
                Some(CompletionResolveDocument {
                    host_uri: host_url,
                    language_id,
                    text,
                    geometry,
                    revision,
                    edit_guard,
                })
            };
            let resolved = pool
                .dispatch_completion_resolve(params, &settings, upstream_id, document)
                .await;
            // Edits after enqueue are allowed to proceed, but their older
            // reply must not surface coordinates for the superseded text.
            if let (Some(envelope), Some(unresolved)) = (envelope.as_ref(), unresolved) {
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
                let revision_now = self
                    .documents
                    .get(&host_url)
                    .map(|document| document.content_version());
                if revision_now != revision_then {
                    log::debug!(
                        target: "kakehashi::bridge",
                        "completionItem/resolve: {} was edited while resolving; returning item unresolved",
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
