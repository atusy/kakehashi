//! Rebuild an injection region's current host offset from the live parse.
//!
//! Shared by translators of inbound (downstream → editor) payloads that must
//! map virtual-document coordinates back to the host document —
//! `window/showDocument` ([`ShowDocumentTranslator`]) and `workspace/applyEdit`
//! ([`ApplyEditTranslator`]) — and by the `*/resolve` staleness gates
//! (completion, code action, code lens, document link, inlay hint), which
//! compare the rebuilt offset against the snapshot their envelope carries.
//! The offset is rebuilt exactly as the goto request path does (`region_id →
//! node byte range → resolve injection → RegionOffset`), so neither inbound
//! translation nor a resolve gate can disagree with goto on the same region.
//! The region's content-precise host end and contiguity are returned
//! alongside so an edit translator can reject a range that escapes the
//! region or targets a combined document with masked host gaps. The
//! lifetime (incarnation) gate every resolve shares, and the revision gate
//! the code action resolve adds, live here too.
//!
//! [`ShowDocumentTranslator`]: super::show_document_translation::ShowDocumentTranslator
//! [`ApplyEditTranslator`]: super::apply_edit_translation::ApplyEditTranslator

use std::sync::Arc;

use tower_lsp_server::ls_types::Position;
use url::Url;

use super::Kakehashi;
use crate::document::DocumentStore;
use crate::language::injection::ResolvedInjection;
use crate::language::{InjectionResolver, LanguageCoordinator};
use crate::lsp::bridge::{BridgeCoordinator, EnvelopeOffset, RegionOffset, region_host_end};

/// How long a resolve gate waits for the post-edit reparse before it reads the
/// region as stale. Matches the request handlers' stale-snapshot deadline.
const RESOLVE_PARSE_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

impl Kakehashi {
    /// Whether the host document is still at the text revision a resolve
    /// envelope was minted at. A same-shape edit inside a region keeps its
    /// geometry, so the region gates admit it, yet the downstream's lazily
    /// materialized fields were computed against the previous text; the
    /// revision stamp is what refuses that. Envelopes without a stamp (a
    /// mangled echo) fail closed.
    pub(super) fn document_revision_is_current(
        &self,
        host_url: &Url,
        expected: Option<u64>,
    ) -> bool {
        expected.is_some_and(|expected| {
            self.documents
                .get(host_url)
                .is_some_and(|document| document.content_version() == expected)
        })
    }

    /// Whether the host document is still the lifetime a resolve envelope
    /// was minted under. A close and reopen restarts the revision count, so
    /// the revision alone cannot tell a reopened document from the one the
    /// item was computed on. Envelopes without a stamp fail closed.
    pub(super) fn host_incarnation_is_current(
        &self,
        host_url: &Url,
        expected: Option<u64>,
    ) -> bool {
        expected.is_some_and(|expected| {
            self.bridge.pool_arc().current_host_incarnation(host_url) == Some(expected)
        })
    }

    /// Whether the injection region a resolve envelope names still resolves
    /// to the offset snapshot the envelope carries.
    ///
    /// Re-resolves the region from the live parse and compares the WHOLE
    /// effective offset (start line plus every per-line column offset), so a
    /// region that moved, was invalidated, or had an interior blockquote
    /// prefix edited is reported stale — translating the resolved payload with
    /// the snapshot offset would bind it to wrong host coordinates then. The
    /// live offset is the same `#offset!`/`#trim!`-adjusted geometry the
    /// envelope was minted from, so regions under those directives (markdown
    /// frontmatter, blockquote prose) compare equal while unchanged.
    ///
    /// The code-lens and document-link gates ask exactly this question and
    /// share this one answer, so they cannot drift. Completion, code action
    /// and inlay hint need the region end and contiguity as well, so they
    /// call [`resolve_region_offset`] directly and compare the same offset.
    ///
    /// The envelope's host incarnation is checked first, then the document
    /// is awaited to its current parse: `didChange` clears
    /// the tree and reparses off-ingress, and a resolve issued in that window
    /// would otherwise find no snapshot and report the region stale.
    ///
    /// Contiguity is deliberately NOT required here. A non-contiguous
    /// combined region masks its host gaps with whitespace, so its line
    /// geometry — and therefore range translation — stays valid; only
    /// edit-carrying methods are refused on such regions
    /// (`method_requires_contiguous_injection`), and code lenses and document
    /// links are minted for them on purpose. Refusing to resolve what was
    /// deliberately produced would leave those items permanently unresolved.
    /// The parse wait a resolve gate pays before rebuilding its region: a hard
    /// bound, unlike the request handlers' wait, whose first-parse backstop
    /// (15s) is right for a request that needs an answer and wrong for a
    /// gate that fails soft. A reparse still running when it expires reads as
    /// a stale region; a close+reopen that lands between the lifetime check
    /// and this wait would otherwise park the resolve on the reopened
    /// lifetime's first parse.
    pub(super) async fn wait_for_resolve_parse(&self, host_url: &Url) {
        let _ = tokio::time::timeout(
            RESOLVE_PARSE_WAIT,
            self.wait_for_current_snapshot(host_url, RESOLVE_PARSE_WAIT),
        )
        .await;
    }

    pub(super) async fn region_offset_is_fresh(
        &self,
        host_uri: &str,
        region_id: &str,
        offset: &EnvelopeOffset,
        incarnation: Option<u64>,
        injection_language: &str,
    ) -> bool {
        let Ok(host_url) = Url::parse(host_uri) else {
            return false;
        };
        // Lifetime first: an item from a closed and reopened document would
        // otherwise park on the reopened lifetime's first parse only to be
        // refused by the region rebuild.
        if !self.host_incarnation_is_current(&host_url, incarnation) {
            return false;
        }
        self.wait_for_resolve_parse(&host_url).await;
        resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            &host_url,
            region_id,
        )
        // The language too: an edit can re-route a region (a shebang change
        // under an `unknown` injection) while keeping its id and geometry,
        // and an item produced for the old language must not resolve on the
        // server the region now routes to. An envelope with no language
        // (cleared by the client) names no region.
        .is_some_and(|(live_offset, _, _, live_language)| {
            live_offset == RegionOffset::from(offset) && live_language == injection_language
        })
    }
}

/// Rebuild the region's current host offset from the live parse, keyed by its
/// `region_id` (a ULID in production). Returns `None` when the offset can't be
/// rebuilt: region invalidated by edits (`lookup_node` misses), a `region_id`
/// whose tracked geometry/layer no longer exists in the live parse, or a
/// missing document/snapshot/language/query. The third tuple field reports
/// whether the virtual content maps to one contiguous host span; the fourth
/// is the region's live injection language, so a resolve gate can refuse an
/// envelope whose echoed language no longer names this region.
pub(super) fn resolve_region_offset(
    documents: &DocumentStore,
    language: &Arc<LanguageCoordinator>,
    bridge: &BridgeCoordinator,
    host_url: &Url,
    region_id: &str,
) -> Option<(RegionOffset, Position, bool, String)> {
    resolve_region(documents, language, bridge, host_url, region_id).map(resolved_region_geometry)
}

/// Resolve geometry and content from the same current snapshot.
pub(super) fn resolve_region(
    documents: &DocumentStore,
    language: &Arc<LanguageCoordinator>,
    bridge: &BridgeCoordinator,
    host_url: &Url,
    region_id: &str,
) -> Option<ResolvedInjection> {
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
    Some(resolved)
}

/// Same rebuild under the name the call hierarchy gate was written against
/// before the language joined the [`resolve_region_offset`] tuple.
pub(super) fn resolve_region_offset_and_language(
    documents: &DocumentStore,
    language: &Arc<LanguageCoordinator>,
    bridge: &BridgeCoordinator,
    host_url: &Url,
    region_id: &str,
) -> Option<(RegionOffset, Position, bool, String)> {
    resolve_region_offset(documents, language, bridge, host_url, region_id)
}

/// Consume a resolved region into the geometry shared by freshness and edit
/// validation. The end is derived from the exact virtual content rather than
/// the raw content-node range, whose trailing named children may be excluded.
pub(super) fn resolved_region_geometry(
    resolved: ResolvedInjection,
) -> (RegionOffset, Position, bool, String) {
    let start_line = resolved.region.line_range.start;
    let offset = RegionOffset::with_protected_blank_rows(
        start_line,
        resolved.line_column_offsets,
        &resolved.protected_blank_rows,
    );
    let region_end = region_host_end(&resolved.virtual_content, &offset);
    (
        offset,
        region_end,
        resolved.contiguous,
        resolved.injection_language,
    )
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
            protected_blank_rows: Vec::new(),
            contiguous: true,
        };

        let (_, region_end, _, _) = resolved_region_geometry(resolved);

        assert_eq!(region_end, Position::new(2, 4));
    }

    const FENCE_QUERY: &str = "((fenced_code_block (info_string (language) @injection.language) \
        (code_fence_content) @injection.content))";
    const COMBINED_FENCE_QUERY: &str = "((fenced_code_block (info_string (language) \
        @injection.language) (code_fence_content) @injection.content) \
        (#set! injection.combined))";
    const COMBINED_HOST: &str = "> ```py\n> a = 1\n> ```\n\n```py\nb = 2\n\nc = 3\n```\n";
    const INLINE_QUERY: &str =
        "((inline) @injection.content (#set! injection.language \"markdown_inline\"))";

    /// The regions injection discovery finds in a markdown `host`.
    fn discover(host: &str, query: &str) -> Vec<ResolvedInjection> {
        discover_in(tree_sitter_md::LANGUAGE.into(), host, query)
    }

    /// The regions injection discovery finds in a `host` of `language`.
    fn discover_in(
        language: tree_sitter::Language,
        host: &str,
        query: &str,
    ) -> Vec<ResolvedInjection> {
        let md = language;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&md).unwrap();
        let tree = parser.parse(host, None).unwrap();
        let query = tree_sitter::Query::new(&md, query).unwrap();
        crate::language::InjectionResolver::resolve_all(
            &LanguageCoordinator::new(),
            &crate::language::NodeTracker::new(),
            &Url::parse("file:///test.md").unwrap(),
            &tree,
            host,
            &query,
            0,
        )
    }

    /// Whether the edit checks the bridge applies take a host `edit` on the
    /// region discovered from `host`, through the production geometry.
    fn accepts(resolved: ResolvedInjection, edit: &tower_lsp_server::ls_types::TextEdit) -> bool {
        let host_uri: tower_lsp_server::ls_types::Uri = "file:///test.md".parse().unwrap();
        let workspace_edit = tower_lsp_server::ls_types::WorkspaceEdit {
            changes: Some(
                [(host_uri.clone(), vec![edit.clone()])]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        };
        let (offset, region_end, _, _) = resolved_region_geometry(resolved);
        crate::lsp::bridge::workspace_edit_within_region(
            &workspace_edit,
            &host_uri,
            &offset,
            region_end,
        ) && crate::lsp::bridge::workspace_edit_preserves_line_prefixes(
            &workspace_edit,
            &host_uri,
            &offset,
            region_end,
        )
    }

    fn edit(
        start: (u32, u32),
        end: (u32, u32),
        new_text: &str,
    ) -> tower_lsp_server::ls_types::TextEdit {
        tower_lsp_server::ls_types::TextEdit {
            range: tower_lsp_server::ls_types::Range::new(
                Position::new(start.0, start.1),
                Position::new(end.0, end.1),
            ),
            new_text: new_text.to_string(),
        }
    }

    /// Edits on rows recording no prefix that are not blank rows of a
    /// prefixed block are judged as on any content row. Each accepted edit
    /// is applied to the host, and discovery reads back the edited text.
    #[rstest::rstest]
    // A blockquote paragraph's lazy continuation row "b".
    #[case::lazy_continuation_in_a_blockquote(
        "> a\nb\n> c\n", INLINE_QUERY, 0, ((1, 0), (1, 1), "x"), "a\nx\nc"
    )]
    // A list item paragraph's lazy continuation row "b".
    #[case::lazy_continuation_in_a_list_item(
        "- a\nb\n  c\n", INLINE_QUERY, 0, ((1, 0), (1, 1), "x"), "a\nx\nc"
    )]
    // A combined region: a blockquoted fence, then a plain fence whose rows
    // record no prefix. Its code row "b = 2" ...
    #[case::plain_fence_row_after_a_blockquoted_fence(
        COMBINED_HOST,
        COMBINED_FENCE_QUERY,
        0,
        ((5, 0), (5, 1), "z"),
        "a = 1\n\n\n\nz = 2\n\nc = 3\n"
    )]
    // ... and its own blank row.
    #[case::plain_fence_blank_row_after_a_blockquoted_fence(
        COMBINED_HOST,
        COMBINED_FENCE_QUERY,
        0,
        ((6, 0), (6, 0), "x"),
        "a = 1\n\n\n\nb = 2\nx\nc = 3\n"
    )]
    fn an_edit_on_an_unprefixed_content_row_is_accepted(
        #[case] host: &str,
        #[case] query: &str,
        #[case] region: usize,
        #[case] change: ((u32, u32), (u32, u32), &str),
        #[case] expected_virtual: &str,
    ) {
        let resolved = discover(host, query).swap_remove(region);
        let change = edit(change.0, change.1, change.2);
        assert!(accepts(resolved, &change), "{change:?} must be accepted");
        let edited = crate::text::edit::apply_text_edits(host, &[change]);
        assert_eq!(
            discover(&edited, query)[region].virtual_content,
            expected_virtual
        );
    }

    /// Discovery's list-item fence: its blank rows stay protected through
    /// the production geometry, and a blank line added there still applies.
    #[test]
    fn a_list_item_fence_keeps_its_blank_rows_protected() {
        let host = "- item\n\n  ```py\n  a = 1\n\n\n  ```\n";
        let resolved = || discover(host, FENCE_QUERY).swap_remove(0);
        assert!(!accepts(resolved(), &edit((4, 0), (4, 0), "b = 2")));
        assert!(!accepts(resolved(), &edit((5, 0), (5, 0), "b = 2\n")));
        assert!(accepts(resolved(), &edit((4, 0), (4, 0), "\n")));
    }

    /// A string whose content has an escape sequence records per-line
    /// offsets, the first being where the string opens mid-line. That is no
    /// container prefix: the blank rows inside the string are content rows
    /// of the host language, and text written there is judged as anywhere.
    #[test]
    fn a_blank_row_inside_a_string_opening_mid_line_is_not_protected() {
        let host = "x = \"\"\"a\n\nb\\tc\n\nd\"\"\"\n";
        let query = "((string (string_content) @injection.content) \
            (#set! injection.language \"sql\"))";
        let discover = |host: &str| discover_in(tree_sitter_python::LANGUAGE.into(), host, query);
        let resolved = discover(host).swap_remove(0);
        assert_eq!(resolved.line_column_offsets, vec![7, 0, 3, 0, 0]);

        let change = edit((1, 0), (1, 0), "z");
        assert!(accepts(resolved, &change), "{change:?} must be accepted");
        let edited = crate::text::edit::apply_text_edits(host, &[change]);
        assert_eq!(discover(&edited)[0].virtual_content, "a\nz\nbc\n\nd");
    }

    /// Discovery marks only blank rows of markdown container blocks.
    #[rstest::rstest]
    #[case::list_fence_trailing_blank_rows("- item\n\n  ```py\n  a = 1\n\n\n  ```\n", FENCE_QUERY, vec![1, 2])]
    #[case::list_fence_leading_blank_rows("- item\n\n  ```py\n\n\n  a = 1\n  ```\n", FENCE_QUERY, vec![0, 1])]
    #[case::ordered_list_fence("1. item\n\n   ```py\n   a\n\n   b\n   ```\n", FENCE_QUERY, vec![1])]
    #[case::blockquote_fence("> ```py\n> a\n>\n> b\n> ```\n", FENCE_QUERY, vec![])]
    // A blank row keeping less whitespace than the item's indentation.
    #[case::list_fence_short_whitespace_row("- item\n\n  ```py\n  a\n \n  b\n  ```\n", FENCE_QUERY, vec![1])]
    #[case::list_fence_short_whitespace_row_crlf("- item\r\n\r\n  ```py\r\n  a\r\n \r\n  b\r\n  ```\r\n", FENCE_QUERY, vec![1])]
    #[case::list_fence_tab_row("- item\n\n  ```py\n  a\n\t\n  b\n  ```\n", FENCE_QUERY, vec![1])]
    // A blank row keeping the item's whole indentation is a content row.
    #[case::list_fence_full_whitespace_row("- item\n\n  ```py\n  a\n  \n  b\n  ```\n", FENCE_QUERY, vec![])]
    #[case::lazy_continuation("> a\nb\n> c\n", INLINE_QUERY, vec![])]
    fn discovery_marks_the_blank_rows_of_container_blocks(
        #[case] host: &str,
        #[case] query: &str,
        #[case] expected: Vec<u32>,
    ) {
        assert_eq!(discover(host, query)[0].protected_blank_rows, expected);
    }

    /// A blank row keeping one space of a two-space item records offset 1:
    /// text written there lands at host column 1, outside the item.
    #[test]
    fn a_short_whitespace_blank_row_of_a_list_item_fence_is_protected() {
        let host = "- item\n\n  ```py\n  a\n \n  b\n  ```\n";
        let resolved = || discover(host, FENCE_QUERY).swap_remove(0);
        assert_eq!(resolved().line_column_offsets, vec![2, 1, 2, 0]);
        assert!(!accepts(resolved(), &edit((4, 1), (4, 1), "x")));
        // Deleting the row's content still applies.
        assert!(accepts(resolved(), &edit((4, 1), (4, 1), "")));
    }
}
