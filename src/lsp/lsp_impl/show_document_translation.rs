//! Virtual→host translation for inbound `window/showDocument` requests.
//!
//! A bridged downstream server only knows the *virtual* document it was handed
//! (one per isolated region or combined group), so a `window/showDocument` carries a
//! virtual URI and a `selection` in virtual coordinates. Before the bridge
//! forwards the request to the editor, [`ShowDocumentTranslator`] rewrites the
//! URI back to the host document and the selection back to host coordinates —
//! mirroring how goto/location responses are remapped (see
//! [`transform_goto_response_to_host`]) — so the editor opens the real file at
//! the right spot.
//!
//! The offset is rebuilt from the live parse exactly as the goto path does
//! (`region_id → node byte range → resolve injection → RegionOffset`) via the
//! shared [`resolve_region_offset`](super::region_offset::resolve_region_offset),
//! so a showDocument-to-a-region can't disagree with goto on the same region.
//!
//! Once the URI resolves to a currently-open virtual document, the editor always
//! gets the **host** document URI. The selection is translated when the region
//! offset can be rebuilt; when it can't — region invalidated by edits
//! (`lookup_node` returns `None`), a `region_id` that no longer matches the live
//! parse at that byte (edit race), or a missing document/snapshot/language/query
//! — the selection is **dropped** (a stale virtual-coordinate selection would
//! point at the wrong place) but the host document still opens.
//!
//! Forwarding the params **unchanged** (the pre-translation behavior) only
//! happens when the URI isn't a resolvable virtual document at all: not a
//! virtual URI / `external: true` / a `region_id` that isn't a valid id / no
//! host mapping for the URI / an unmappable host URI.
//!
//! [`transform_goto_response_to_host`]: crate::lsp::bridge::protocol::transform_goto_response_to_host

use std::sync::Arc;

use tower_lsp_server::ls_types::ShowDocumentParams;
use url::Url;

use crate::document::DocumentStore;
use crate::language::LanguageCoordinator;
use crate::lsp::bridge::{BridgeCoordinator, RegionOffset, translate_virtual_range_to_host};

use super::region_offset::resolve_region_offset;

/// Translates `window/showDocument` params whose URI is a virtual document back
/// to the host document + host coordinates. Holds shared (cheaply cloneable)
/// handles to the document store, language coordinator, and bridge so it can run
/// off the forwarding loop without the `Kakehashi` receiver.
pub(super) struct ShowDocumentTranslator {
    documents: Arc<DocumentStore>,
    language: Arc<LanguageCoordinator>,
    bridge: Arc<BridgeCoordinator>,
    tree_worker_shadow: Option<Arc<crate::lsp::lsp_impl::tree_worker_shadow::TreeWorkerShadow>>,
}

impl ShowDocumentTranslator {
    pub(super) fn new(
        documents: Arc<DocumentStore>,
        language: Arc<LanguageCoordinator>,
        bridge: Arc<BridgeCoordinator>,
        tree_worker_shadow: Option<Arc<crate::lsp::lsp_impl::tree_worker_shadow::TreeWorkerShadow>>,
    ) -> Self {
        Self {
            documents,
            language,
            bridge,
            tree_worker_shadow,
        }
    }

    /// Translate a virtual-document `showDocument` to the host document (and host
    /// coordinates when the offset resolves); forward `params` unchanged only when
    /// the URI isn't a resolvable virtual document. See the module docs for the
    /// exact behavior.
    pub(super) async fn translate(&self, mut params: ShowDocumentParams) -> ShowDocumentParams {
        // `external: true` is a browser/OS resource, never a virtual document.
        if params.external == Some(true) {
            return params;
        }
        // `resolve_virtual_uri` returns `None` for any non-virtual URI (it parses
        // the region_id first), so a separate `is_virtual_uri` check would just
        // parse the URL twice.
        let Some((host_url, region_id)) =
            self.bridge.resolve_virtual_uri(params.uri.as_str()).await
        else {
            return params; // not a currently-open virtual document
        };
        let Ok(host_uri) = super::url_to_uri(&host_url) else {
            return params;
        };

        // Only a selection needs the (live-parse) region offset, so skip
        // resolving it when there's nothing to translate.
        if params.selection.is_some() {
            match self.region_offset(&host_url, &region_id).await {
                Some(offset) => return Self::apply_host_translation(params, host_uri, &offset),
                // Offset unavailable (region invalidated by edits, or otherwise
                // unresolvable): drop the selection we can't translate — a
                // virtual-coordinate selection on the host URI would point at the
                // wrong place — but still open the host document below.
                None => params.selection = None,
            }
        }
        params.uri = host_uri;
        params
    }

    /// Rewrite `params` to the host document: set `uri`, and translate the
    /// `selection` (if any) from virtual to host coordinates. Pure mutation,
    /// split out so it can be unit-tested without a live parse.
    fn apply_host_translation(
        mut params: ShowDocumentParams,
        host_uri: tower_lsp_server::ls_types::Uri,
        offset: &RegionOffset,
    ) -> ShowDocumentParams {
        if let Some(range) = params.selection.as_mut() {
            translate_virtual_range_to_host(range, offset);
        }
        params.uri = host_uri;
        params
    }

    /// Rebuild the region's current host offset from the live parse via the
    /// shared [`resolve_region_offset`]. On `None` the caller opens the host
    /// document without a selection rather than risk a wrong one. showDocument
    /// only needs the offset (it translates a single selection position, not an
    /// edit), so the region-end bound and contiguity marker are discarded.
    async fn region_offset(&self, host_url: &Url, region_id: &str) -> Option<RegionOffset> {
        resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            self.tree_worker_shadow.as_ref(),
            host_url,
            region_id,
        )
        .await
        .map(|(offset, _region_end, _contiguous)| offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::VirtualDocumentUri;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{Position, Range, Uri};

    fn translator() -> ShowDocumentTranslator {
        ShowDocumentTranslator::new(
            Arc::new(DocumentStore::new()),
            Arc::new(LanguageCoordinator::new()),
            Arc::new(BridgeCoordinator::new()),
            None,
        )
    }

    fn params(uri: &str) -> ShowDocumentParams {
        ShowDocumentParams {
            uri: Uri::from_str(uri).unwrap(),
            external: None,
            take_focus: None,
            selection: Some(Range::new(Position::new(1, 2), Position::new(3, 4))),
        }
    }

    #[tokio::test]
    async fn passes_through_non_virtual_uri_unchanged() {
        let original = params("file:///project/main.rs");
        let out = translator().translate(original.clone()).await;
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn passes_through_external_resource_unchanged() {
        let mut original = params("https://example.com/");
        original.external = Some(true);
        let out = translator().translate(original.clone()).await;
        assert_eq!(out, original);
    }

    #[tokio::test]
    async fn passes_through_unknown_virtual_uri_unchanged() {
        // A well-formed virtual URI that no open document maps to: the tracker
        // miss must fall back to verbatim forwarding (no host translation).
        let host = Uri::from_str("file:///project/doc.md").unwrap();
        let virtual_uri =
            VirtualDocumentUri::new(&host, "lua", "01ARZ3NDEKTSV4RRFFQ69G5FAV").to_uri_string();
        assert!(VirtualDocumentUri::is_virtual_uri(&virtual_uri));

        let original = params(&virtual_uri);
        let out = translator().translate(original.clone()).await;
        assert_eq!(out, original);
    }

    #[test]
    fn apply_host_translation_rewrites_uri_and_translates_selection() {
        let host = Uri::from_str("file:///project/doc.md").unwrap();
        let virtual_uri =
            VirtualDocumentUri::new(&host, "lua", "01ARZ3NDEKTSV4RRFFQ69G5FAV").to_uri_string();
        let mut original = params(&virtual_uri);
        // Selection on virtual line 0 so both the line and column offsets apply.
        original.selection = Some(Range::new(Position::new(0, 2), Position::new(0, 6)));

        // Region starts at host line 10, column 5.
        let out = ShowDocumentTranslator::apply_host_translation(
            original,
            host.clone(),
            &RegionOffset::new(10, 5),
        );

        assert_eq!(out.uri, host);
        assert_eq!(
            out.selection,
            Some(Range::new(Position::new(10, 7), Position::new(10, 11))),
            "line 0 selection shifts by the region's (line, column) offset"
        );
    }

    #[test]
    fn apply_host_translation_without_selection_only_rewrites_uri() {
        let host = Uri::from_str("file:///project/doc.md").unwrap();
        let mut original = params("file:///ignored.lua");
        original.selection = None;

        let out = ShowDocumentTranslator::apply_host_translation(
            original,
            host.clone(),
            &RegionOffset::new(3, 0),
        );

        assert_eq!(out.uri, host);
        assert_eq!(out.selection, None);
    }
}
