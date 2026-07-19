//! Rebuild an injection region's current host offset from the live parse.
//!
//! Shared by translators of inbound (downstream → editor) payloads that must
//! map virtual-document coordinates back to the host document —
//! `window/showDocument` ([`ShowDocumentTranslator`]) and `workspace/applyEdit`
//! ([`ApplyEditTranslator`]). The offset is rebuilt exactly as the goto request
//! path does (`region_id → node byte range → resolve injection → RegionOffset`),
//! so inbound translation can't disagree with goto on the same region. The
//! region's content-precise host end and contiguity are returned alongside so
//! an edit translator can reject a range that escapes the region or targets a
//! combined document with masked host gaps.
//!
//! [`ShowDocumentTranslator`]: super::show_document_translation::ShowDocumentTranslator
//! [`ApplyEditTranslator`]: super::apply_edit_translation::ApplyEditTranslator

use std::sync::Arc;

use tower_lsp_server::ls_types::Position;
use url::Url;

use crate::document::DocumentStore;
use crate::language::injection::ResolvedInjection;
use crate::language::{InjectionResolver, LanguageCoordinator};
use crate::lsp::bridge::{BridgeCoordinator, RegionOffset, region_host_end};

/// Rebuild the region's current host offset from the live parse, keyed by its
/// `region_id` (a ULID in production). Returns `None` when the offset can't be
/// rebuilt: region invalidated by edits (`lookup_node` misses), a `region_id`
/// whose tracked geometry/layer no longer exists in the live parse, or a
/// missing document/snapshot/language/query. The third tuple field reports
/// whether the virtual content maps to one contiguous host span.
pub(super) async fn resolve_region_offset(
    documents: &DocumentStore,
    language: &Arc<LanguageCoordinator>,
    bridge: &BridgeCoordinator,
    tree_worker_shadow: Option<&Arc<crate::lsp::lsp_impl::tree_worker_shadow::TreeWorkerShadow>>,
    host_url: &Url,
    region_id: &str,
) -> Option<(RegionOffset, Position, bool)> {
    if let Some(tree_worker_shadow) = tree_worker_shadow
        && tree_worker_shadow.is_authoritative()
    {
        let (text, language_id, incarnation, content_version) =
            documents.get(host_url).map(|document| {
                (
                    document.text_arc(),
                    document.language_id().map(str::to_owned),
                    document.incarnation(),
                    document.content_version(),
                )
            })?;
        let language_name =
            language.detect_language(host_url.path(), &text, None, language_id.as_deref())?;
        if !language
            .ensure_language_loaded_async(&language_name)
            .await
            .success
        {
            return None;
        }
        let regions = tree_worker_shadow
            .worker_injection_regions(
                host_url,
                incarnation,
                content_version,
                language.configuration_generation(),
            )
            .await?;
        let current = documents.get(host_url)?;
        if current.incarnation() != incarnation || current.content_version() != content_version {
            return None;
        }
        drop(current);
        return resolved_region_by_id(&regions, region_id).map(resolved_region_geometry);
    }

    // Snapshot is owned, so the document handle (a store lock) is released
    // before `detect_document_language` reaches back into the store.
    let snapshot = documents.get(host_url)?.snapshot()?;
    let language_name = super::detect_document_language(language, documents, host_url)?;
    let injection_query = language.injection_query(&language_name)?;
    let resolved = InjectionResolver::resolve_by_region_id(
        language,
        bridge.node_tracker(),
        host_url,
        snapshot.tree(),
        snapshot.text(),
        injection_query.as_ref(),
        region_id,
        snapshot.incarnation(),
    )?;
    // Exact-ID resolution also validates the tracker incarnation and rejects
    // an edit race: if this snapshot no longer contains the tracked layer, no
    // candidate has the ID and callers fall back safely.
    Some(resolved_region_geometry(resolved))
}

fn resolved_region_by_id(
    regions: &[ResolvedInjection],
    region_id: &str,
) -> Option<ResolvedInjection> {
    regions
        .iter()
        .find(|resolved| resolved.region.region_id == region_id)
        .cloned()
}

/// Consume a resolved region into the geometry shared by freshness and edit
/// validation. The end is derived from the exact virtual content rather than
/// the raw content-node range, whose trailing named children may be excluded.
fn resolved_region_geometry(resolved: ResolvedInjection) -> (RegionOffset, Position, bool) {
    let start_line = resolved.region.line_range.start;
    let offset = RegionOffset::with_per_line_offsets(start_line, resolved.line_column_offsets);
    let region_end = region_host_end(&resolved.virtual_content, &offset);
    (offset, region_end, resolved.contiguous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::injection::{CacheableInjectionRegion, ResolvedInjection};

    #[test]
    fn geometry_uses_virtual_content_end_not_raw_node_end() {
        let resolved = ResolvedInjection {
            region: CacheableInjectionRegion {
                language: "rust".to_string(),
                byte_range: 10..99,
                line_range: 2..3,
                start_column: 3,
                region_id: "region".to_string(),
                content_hash: 0,
            },
            injection_language: "rust".to_string(),
            virtual_content: "x".to_string(),
            line_column_offsets: vec![3],
            contiguous: true,
        };

        let (_, region_end, _) = resolved_region_geometry(resolved);

        assert_eq!(region_end, Position::new(2, 4));
    }

    #[test]
    fn worker_region_lookup_requires_the_exact_region_id() {
        let make = |region_id: &str, line: u32| ResolvedInjection {
            region: CacheableInjectionRegion {
                language: "rust".to_string(),
                byte_range: 10..20,
                line_range: line..line + 1,
                start_column: 0,
                region_id: region_id.to_string(),
                content_hash: 0,
            },
            injection_language: "rust".to_string(),
            virtual_content: "x".to_string(),
            line_column_offsets: vec![0],
            contiguous: true,
        };
        let regions = vec![make("older", 2), make("current", 7)];

        let selected = resolved_region_by_id(&regions, "current").expect("exact region");

        assert_eq!(selected.region.line_range.start, 7);
        assert!(resolved_region_by_id(&regions, "missing").is_none());
    }
}
