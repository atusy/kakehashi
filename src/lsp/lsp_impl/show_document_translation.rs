//! Virtual→host translation for inbound `window/showDocument` requests.
//!
//! A bridged downstream server only knows the *virtual* document it was handed
//! (one per injected region), so a `window/showDocument` it issues carries a
//! virtual URI and a `selection` in virtual coordinates. Before the bridge
//! forwards the request to the editor, [`ShowDocumentTranslator`] rewrites the
//! URI back to the host document and the selection back to host coordinates —
//! mirroring how goto/location responses are remapped (see
//! [`transform_goto_response_to_host`]) — so the editor opens the real file at
//! the right spot.
//!
//! The offset is rebuilt from the live parse exactly as the goto path does
//! (`region_id → node byte range → resolve injection → RegionOffset`), so a
//! showDocument-to-a-region can't disagree with goto on the same region. Any
//! miss — not a virtual URI, `external: true`, no host mapping, or a region
//! invalidated by edits (`lookup_node` returns `None`) — falls back to
//! forwarding the params unchanged, which is the pre-translation behavior.
//!
//! [`transform_goto_response_to_host`]: crate::lsp::bridge
//! (response translation lives in the bridge protocol layer)

use std::sync::Arc;

use tower_lsp_server::ls_types::ShowDocumentParams;
use url::Url;

use crate::document::DocumentStore;
use crate::language::{InjectionResolver, LanguageCoordinator};
use crate::lsp::bridge::{
    BridgeCoordinator, RegionOffset, VirtualDocumentUri, translate_virtual_range_to_host,
};

/// Translates `window/showDocument` params whose URI is a virtual document back
/// to the host document + host coordinates. Holds shared (cheaply cloneable)
/// handles to the document store, language coordinator, and bridge so it can run
/// off the forwarding loop without the `Kakehashi` receiver.
pub(crate) struct ShowDocumentTranslator {
    documents: Arc<DocumentStore>,
    language: Arc<LanguageCoordinator>,
    bridge: Arc<BridgeCoordinator>,
}

impl ShowDocumentTranslator {
    pub(crate) fn new(
        documents: Arc<DocumentStore>,
        language: Arc<LanguageCoordinator>,
        bridge: Arc<BridgeCoordinator>,
    ) -> Self {
        Self {
            documents,
            language,
            bridge,
        }
    }

    /// Translate a virtual-document `showDocument` to host coordinates, or return
    /// `params` unchanged on any miss (see module docs for the fallback cases).
    pub(crate) async fn translate(&self, mut params: ShowDocumentParams) -> ShowDocumentParams {
        // `external: true` is a browser/OS resource, never a virtual document.
        if params.external == Some(true) {
            return params;
        }
        if !VirtualDocumentUri::is_virtual_uri(params.uri.as_str()) {
            return params;
        }
        let Some((host_url, region_id)) =
            self.bridge.resolve_virtual_uri(params.uri.as_str()).await
        else {
            return params; // not a currently-open virtual document
        };
        let Some(offset) = self.region_offset(&host_url, &region_id) else {
            return params; // region invalidated by edits, or unresolvable
        };
        let Ok(host_uri) = super::url_to_uri(&host_url) else {
            return params;
        };

        if let Some(range) = params.selection.as_mut() {
            translate_virtual_range_to_host(range, &offset);
        }
        params.uri = host_uri;
        params
    }

    /// Rebuild the region's current host offset from the live parse, keyed by its
    /// `region_id` (a ULID). Mirrors the goto request path's offset construction.
    fn region_offset(&self, host_url: &Url, region_id: &str) -> Option<RegionOffset> {
        let ulid = ulid::Ulid::from_string(region_id).ok()?;
        let (start_byte, _end, _kind, _layer) =
            self.bridge.node_tracker().lookup_node(host_url, &ulid)?;
        // Snapshot is owned, so the document handle (a store lock) is released
        // before `detect_document_language` reaches back into the store.
        let snapshot = self.documents.get(host_url)?.snapshot()?;
        let language_name =
            super::detect_document_language(&self.language, &self.documents, host_url)?;
        let injection_query = self.language.injection_query(&language_name)?;
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &self.language,
            self.bridge.node_tracker(),
            host_url,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
            start_byte,
        )?;
        Some(RegionOffset::with_per_line_offsets(
            resolved.region.line_range.start,
            resolved.line_column_offsets.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tower_lsp_server::ls_types::{Position, Range, Uri};

    fn translator() -> ShowDocumentTranslator {
        ShowDocumentTranslator::new(
            Arc::new(DocumentStore::new()),
            Arc::new(LanguageCoordinator::new()),
            Arc::new(BridgeCoordinator::new()),
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
}
