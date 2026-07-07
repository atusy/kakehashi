//! Rebuild an injection region's current host offset from the live parse.
//!
//! Shared by translators of inbound (downstream → editor) payloads that must
//! map virtual-document coordinates back to the host document — today
//! `window/showDocument` ([`ShowDocumentTranslator`]). The offset is rebuilt
//! exactly as the goto request path does (`region_id → node byte range →
//! resolve injection → RegionOffset`), so inbound translation can't disagree
//! with goto on the same region.
//!
//! [`ShowDocumentTranslator`]: super::show_document_translation::ShowDocumentTranslator

use std::sync::Arc;

use tower_lsp_server::ls_types::Position;
use url::Url;

use crate::document::DocumentStore;
use crate::language::{InjectionResolver, LanguageCoordinator};
use crate::lsp::bridge::{BridgeCoordinator, RegionOffset};
use crate::text::PositionMapper;

/// Rebuild the region's current host offset from the live parse, keyed by its
/// `region_id` (a ULID in production). Returns `None` when the offset can't be
/// rebuilt: region invalidated by edits (`lookup_node` misses), a `region_id`
/// that no longer matches the live parse at that byte (edit race), or a missing
/// document/snapshot/language/query.
pub(super) fn resolve_region_offset(
    documents: &DocumentStore,
    language: &Arc<LanguageCoordinator>,
    bridge: &BridgeCoordinator,
    host_url: &Url,
    region_id: &str,
) -> Option<(RegionOffset, Position)> {
    let ulid = ulid::Ulid::from_string(region_id).ok()?;
    let (start_byte, _end, _kind, _layer) = bridge.node_tracker().lookup_node(host_url, &ulid)?;
    // Snapshot is owned, so the document handle (a store lock) is released
    // before `detect_document_language` reaches back into the store.
    let snapshot = documents.get(host_url)?.snapshot()?;
    let language_name = super::detect_document_language(language, documents, host_url)?;
    let injection_query = language.injection_query(&language_name)?;
    let resolved = InjectionResolver::resolve_at_byte_offset(
        language,
        bridge.node_tracker(),
        host_url,
        snapshot.tree(),
        snapshot.text(),
        injection_query.as_ref(),
        start_byte,
    )?;
    // `region_id`/`start_byte` came from the tracker + node map, but
    // `resolved` came from a separately-fetched snapshot. If an edit landed
    // in between, `start_byte` could fall inside a *different* live region —
    // `resolve_at_byte_offset` returns whichever region contains the byte. A
    // mismatched `region_id` means we'd translate coordinates against the
    // wrong region, so return `None` (no offset); callers fall back to their
    // safe default rather than risk a wrong translation. (The goto path can't
    // hit this: there `resolved` and `region_id` come from one call.)
    if resolved.region.region_id != region_id {
        return None;
    }
    // The region's exclusive host-document end, content-precise (the injection
    // region's own byte range mapped through the live host text). Callers that
    // translate an inbound edit use it to reject a range that runs past the
    // region into unrelated host text.
    let region_end =
        PositionMapper::new(snapshot.text()).byte_to_position(resolved.region.byte_range.end)?;
    // `start` is `Copy`; move `line_column_offsets` out (no clone — `resolved`
    // is dropped after this).
    let start_line = resolved.region.line_range.start;
    Some((
        RegionOffset::with_per_line_offsets(start_line, resolved.line_column_offsets),
        region_end,
    ))
}
